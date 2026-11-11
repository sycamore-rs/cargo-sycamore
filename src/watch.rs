//! Debounced filesystem watching with serialized builds and generated-file exclusions.

use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use cargo_metadata::MetadataCommand;
use notify::{Event, EventKind, RecursiveMode, Watcher, event::ModifyKind};

use crate::{
    build,
    options::{BuildArgs, Options},
};

const DEBOUNCE: Duration = Duration::from_millis(250);
type WatchEvent = notify::Result<Event>;

// Deliberately narrow editor-artifact patterns. Don't ignore all dotfiles or
// generic *.tmp / *.bak files, which may be legitimate application inputs.
const EDITOR_ARTIFACT_NAMES: &[&str] = &["4913"]; // Vim directory write check.
const EDITOR_ARTIFACT_PREFIXES: &[&str] = &[
    ".#",              // Emacs lock files: .#main.rs
    ".goutputstream-", // GIO/GNOME editor atomic-save files.
];
const EDITOR_ARTIFACT_SUFFIXES: &[&str] = &[
    "~",            // Vim/Emacs backups, numbered backups, Vim undo files.
    "___jb_tmp___", // JetBrains safe-write temporary files.
    "___jb_old___", // JetBrains safe-write originals.
    "___jb_bak___", // JetBrains backups.
    ".kate-swp",    // Kate swap files.
];

fn is_editor_artifact(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    EDITOR_ARTIFACT_NAMES.contains(&name)
        || EDITOR_ARTIFACT_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
        || EDITOR_ARTIFACT_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
        // Emacs autosaves: #main.rs#.
        || (name.len() > 2 && name.starts_with('#') && name.ends_with('#'))
        // Vim swap files: .main.rs.swp, .swo, ... .swa.
        || (name.starts_with('.') && name.rsplit('.').next()
            .is_some_and(|ext| ext.len() == 3 && ext.starts_with("sw")
                && matches!(ext.as_bytes()[2], b'a'..=b'p')))
}

/// Build immediately, then rebuild on workspace/config changes until interrupted.
/// Build/config errors are reported without stopping the watcher. Watcher errors
/// are fatal, since otherwise we could silently miss future changes.
pub(crate) fn validate(options: &Options) -> Result<()> {
    WatchPlan::new(options).map(|_| ())
}

pub fn watch(args: &BuildArgs) -> Result<()> {
    watch_with(args, Options::resolve(args)?, |_| Ok(()))
}

#[derive(Debug)]
pub(crate) enum BuildEvent<'a> {
    Started,
    Succeeded(&'a Path),
    Failed,
}

// Reuse the caller's startup snapshot: serve already resolved it (and emitted
// any config warnings). Only subsequent file changes should resolve it again.
pub(crate) fn watch_with(
    args: &BuildArgs,
    options: Options,
    mut on_event: impl FnMut(BuildEvent<'_>) -> Result<()>,
) -> Result<()> {
    let mut plan = WatchPlan::new(&options)?;
    let (sender, receiver) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = sender.send(event);
    })
    .context("creating filesystem watcher")?;
    set_watches(&mut watcher, &[], &plan.roots)?;
    eprintln!("Watching {} (Ctrl-C to stop)", plan.workspace.display());
    // Install watches before building so edits during a build aren't lost.
    on_event(BuildEvent::Started)?;
    report_build(&options, &mut on_event)?;
    loop {
        let changed = wait_for_change(&receiver, &plan, DEBOUNCE)?;
        if changed.is_empty() {
            eprintln!("Filesystem rescan requested; checking for changes...");
        } else {
            let paths = changed
                .iter()
                .map(|path| {
                    path.strip_prefix(&plan.workspace)
                        .unwrap_or(path)
                        .display()
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join(", ");
            eprintln!("Changed: {paths}");
        }
        on_event(BuildEvent::Started)?;
        // Reload file configuration on every rebuild, including changes to the
        // manifest/output paths. Keep the existing watches if resolution fails.
        let resolved = Options::resolve(args).and_then(|options| {
            let next = WatchPlan::new(&options)?;
            Ok((options, next))
        });
        match resolved {
            Ok((options, next)) => {
                set_watches(&mut watcher, &plan.roots, &next.roots)?;
                plan = next;
                eprintln!("Change detected; rebuilding...");
                report_build(&options, &mut on_event)?;
            }
            Err(error) => {
                eprintln!("error: {error:#}\nWaiting for changes...");
                on_event(BuildEvent::Failed)?;
            }
        }
    }
}

fn report_build(
    options: &Options,
    on_event: &mut impl FnMut(BuildEvent<'_>) -> Result<()>,
) -> Result<()> {
    match build::build(options) {
        Ok(directory) => on_event(BuildEvent::Succeeded(&directory)),
        Err(error) => {
            eprintln!("error: {error:#}\nWaiting for changes...");
            on_event(BuildEvent::Failed)
        }
    }
}

#[derive(Debug)]
struct WatchPlan {
    workspace: PathBuf,
    config: Option<PathBuf>,
    tailwind_input: Option<PathBuf>,
    ignored: Vec<PathBuf>,
    roots: Vec<(PathBuf, RecursiveMode)>,
}

impl WatchPlan {
    fn new(options: &Options) -> Result<Self> {
        let mut command = MetadataCommand::new();
        command.no_deps();
        if let Some(cargo) = std::env::var_os("CARGO") {
            command.cargo_path(cargo);
        }
        if let Some(manifest) = &options.manifest_path {
            command.manifest_path(manifest);
        }
        let metadata = command
            .exec()
            .context("reading Cargo workspace for watching")?;
        let workspace = normalize(metadata.workspace_root.as_std_path());
        let mut ignored = BTreeSet::new();
        ignored.insert(normalize(metadata.target_directory.as_std_path()));
        // Exclude generated output for all members, not just the selected one.
        for package in metadata.workspace_packages() {
            if let Some(parent) = package.manifest_path.parent() {
                ignored.insert(normalize(parent.join("dist").as_std_path()));
            }
        }
        if let Some(output) = &options.out_dir {
            ignored.insert(normalize(&std::path::absolute(output)?));
        }
        if let Some(output) = options
            .tailwindcss
            .as_ref()
            .and_then(|config| config.output.as_ref())
        {
            ignored.insert(normalize(&std::path::absolute(output)?));
        }
        for output in &ignored {
            ensure!(
                !workspace.starts_with(output),
                "watch output/target directory '{}' contains the workspace; use a separate output directory",
                output.display()
            );
        }
        let config = options.config_path.as_ref().map(|p| normalize(p));
        let mut roots = vec![(workspace.clone(), RecursiveMode::Recursive)];
        if let Some(parent) = config.as_deref().and_then(Path::parent)
            && !parent.starts_with(&workspace)
        {
            // Watching the parent rather than the file handles atomic-save
            // editors which replace the inode on each save.
            roots.push((parent.to_owned(), RecursiveMode::NonRecursive));
        }
        let tailwind_input = options
            .tailwindcss
            .as_ref()
            .map(|config| normalize(&config.input));
        if let Some(parent) = tailwind_input.as_deref().and_then(Path::parent)
            && !roots.iter().any(|(root, mode)| {
                parent == root
                    || (matches!(mode, RecursiveMode::Recursive) && parent.starts_with(root))
            })
        {
            // Watch the parent to survive atomic saves. If it doesn't exist yet,
            // watch its nearest existing ancestor so a missing input can be fixed.
            if let Some(existing) = parent.ancestors().find(|path| path.is_dir()) {
                let mode = if existing == parent {
                    RecursiveMode::NonRecursive
                } else {
                    RecursiveMode::Recursive
                };
                if let Some((_, existing_mode)) =
                    roots.iter_mut().find(|(root, _)| root == existing)
                {
                    if matches!(mode, RecursiveMode::Recursive) {
                        *existing_mode = mode;
                    }
                } else {
                    roots.push((existing.to_owned(), mode));
                }
            }
        }
        Ok(Self {
            workspace,
            config,
            tailwind_input,
            ignored: ignored.into_iter().collect(),
            roots,
        })
    }

    fn relevant(&self, event: &Event) -> bool {
        // Backends may report queue overflow without paths; rebuild conservatively.
        if event.need_rescan() {
            return true;
        }
        if !matches!(
            event.kind,
            EventKind::Create(_)
                | EventKind::Remove(_)
                | EventKind::Modify(ModifyKind::Any | ModifyKind::Data(_) | ModifyKind::Name(_))
                | EventKind::Any
        ) {
            return false;
        }
        event
            .paths
            .iter()
            .any(|path| self.relevant_path(&normalize(path)))
    }

    fn relevant_path(&self, path: &Path) -> bool {
        self.config.as_deref() == Some(path)
            || ((path.starts_with(&self.workspace) || self.tailwind_input.as_deref() == Some(path))
                && !is_editor_artifact(path)
                && !path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with(".cargo-sycamore-tailwind-") && name.ends_with(".css")
                })
                && !path.components().any(|part| part.as_os_str() == ".git")
                && !self.ignored.iter().any(|ignored| path.starts_with(ignored)))
    }

    fn collect_paths(&self, event: &Event, paths: &mut BTreeSet<PathBuf>) {
        paths.extend(
            event
                .paths
                .iter()
                .map(|path| normalize(path))
                .filter(|path| self.relevant_path(path)),
        );
    }
}

fn set_watches(
    watcher: &mut impl Watcher,
    old: &[(PathBuf, RecursiveMode)],
    new: &[(PathBuf, RecursiveMode)],
) -> Result<()> {
    if old == new {
        return Ok(());
    }
    // Re-register the complete set. Removing a nested watch after installing
    // a new parent can otherwise remove backend watches shared by both roots.
    // This happens before the next build, which reads edits made during the gap.
    for (path, _) in old {
        watcher
            .unwatch(path)
            .with_context(|| format!("stopping watch on {}", path.display()))?;
    }
    for (path, mode) in new {
        watcher
            .watch(path, *mode)
            .with_context(|| format!("watching {}", path.display()))?;
    }
    Ok(())
}

fn wait_for_change(
    receiver: &Receiver<WatchEvent>,
    plan: &WatchPlan,
    debounce: Duration,
) -> Result<BTreeSet<PathBuf>> {
    let mut paths = BTreeSet::new();
    loop {
        let event = receiver
            .recv()
            .context("filesystem watcher disconnected")?
            .context("filesystem watcher failed")?;
        if plan.relevant(&event) {
            plan.collect_paths(&event, &mut paths);
            break;
        }
    }
    let mut deadline = Instant::now() + debounce;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(paths);
        }
        match receiver.recv_timeout(remaining) {
            Ok(event) => {
                let event = event.context("filesystem watcher failed")?;
                if plan.relevant(&event) {
                    plan.collect_paths(&event, &mut paths);
                    deadline = Instant::now() + debounce;
                }
            }
            Err(RecvTimeoutError::Timeout) => return Ok(paths),
            Err(RecvTimeoutError::Disconnected) => anyhow::bail!("filesystem watcher disconnected"),
        }
    }
}

// Lexical normalization also works for removed files and directories that don't
// exist yet, unlike canonicalize(). All watch roots and resolved paths are absolute.
fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, CreateKind, DataChange, MetadataKind, RemoveKind, RenameMode};

    fn plan() -> WatchPlan {
        let root = std::env::temp_dir().join("watch-test-project");
        WatchPlan {
            workspace: root.clone(),
            config: Some(root.join("Sycamore.toml")),
            tailwind_input: None,
            ignored: vec![
                root.join("target"),
                root.join("dist"),
                root.join("custom-output"),
            ],
            roots: vec![(root, RecursiveMode::Recursive)],
        }
    }

    #[test]
    fn tailwind_inputs_trigger_rebuilds_but_generated_css_does_not() {
        let mut plan = plan();
        let input = plan
            .workspace
            .parent()
            .unwrap()
            .join("external-styles/input.css");
        let output = plan.workspace.join("assets/styles.css");
        plan.tailwind_input = Some(input.clone());
        plan.ignored.push(output.clone());
        assert!(plan.relevant_path(&input));
        assert!(!plan.relevant_path(&input.with_file_name("other.css")));
        assert!(plan.relevant_path(&plan.workspace.join("src/style.css")));
        assert!(!plan.relevant_path(&output));
        assert!(
            !plan.relevant_path(
                &plan
                    .workspace
                    .join("assets/.cargo-sycamore-tailwind-abc.css")
            )
        );
        let event = Event::new(EventKind::Modify(ModifyKind::Name(
            notify::event::RenameMode::Both,
        )))
        .add_path(
            plan.workspace
                .join("assets/.cargo-sycamore-tailwind-abc.css"),
        )
        .add_path(output);
        assert!(!plan.relevant(&event));
    }

    #[test]
    fn watcher_reuses_resolved_options_for_the_initial_build() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("src")).unwrap();
        std::fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = 'app'\nversion = '0.1.0'\n",
        )
        .unwrap();
        std::fs::write(temp.path().join("src/lib.rs"), "").unwrap();
        let options = Options {
            manifest_path: Some(temp.path().join("Cargo.toml")),
            ..Default::default()
        };
        // If startup resolves args again, this nonexistent config will fail
        // before our callback. Reusing the supplied snapshot reaches Started.
        let args = BuildArgs {
            config: Some(temp.path().join("missing.toml")),
            ..Default::default()
        };
        let mut started = false;
        let error = watch_with(&args, options, |event| {
            assert!(matches!(event, BuildEvent::Started));
            started = true;
            anyhow::bail!("stop before the initial build")
        })
        .unwrap_err();
        assert!(started);
        assert_eq!(error.to_string(), "stop before the initial build");
    }

    #[test]
    fn tailwind_plan_watches_external_input_and_ignores_custom_output() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let external = temp.path().join("styles");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::create_dir(&external).unwrap();
        std::fs::write(
            project.join("Cargo.toml"),
            "[package]\nname = 'app'\nversion = '0.1.0'\n",
        )
        .unwrap();
        std::fs::write(project.join("src/lib.rs"), "").unwrap();
        let options = Options {
            manifest_path: Some(project.join("Cargo.toml")),
            tailwindcss: Some(crate::options::TailwindOptions {
                input: external.join("input.css"),
                output: Some(project.join("assets/site.css")),
                version: "test-version".into(),
            }),
            ..Default::default()
        };
        let plan = WatchPlan::new(&options).unwrap();
        assert!(plan.roots.iter().any(|(path, _)| *path == external));
        assert!(plan.relevant_path(&external.join("input.css")));
        assert!(!plan.relevant_path(&project.join("assets/site.css")));
    }

    fn event(kind: EventKind, path: PathBuf) -> Event {
        Event::new(kind).add_path(path)
    }

    #[test]
    fn accepts_source_manifest_lockfile_and_config_changes() {
        let plan = plan();
        for name in [
            "src/main.rs",
            "Cargo.toml",
            "Cargo.lock",
            "Sycamore.toml",
            "assets/style.css",
        ] {
            for kind in [
                EventKind::Create(CreateKind::File),
                EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                EventKind::Remove(RemoveKind::File),
            ] {
                assert!(
                    plan.relevant(&event(kind, plan.workspace.join(name))),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn excludes_generated_files_git_and_read_events() {
        let plan = plan();
        for name in [
            "target/debug/app.wasm",
            "dist/app.js",
            "custom-output/app_bg.wasm",
            ".git/index",
            "member/.git/HEAD",
        ] {
            assert!(
                !plan.relevant(&event(
                    EventKind::Create(CreateKind::File),
                    plan.workspace.join(name)
                )),
                "{name}"
            );
        }
        for kind in [
            EventKind::Access(AccessKind::Any),
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
        ] {
            assert!(!plan.relevant(&event(kind, plan.workspace.join("src/main.rs"))));
        }
        assert!(plan.relevant(&event(
            EventKind::Create(CreateKind::File),
            plan.workspace.join("dist-source.rs")
        )));
    }

    #[test]
    fn handles_renames_and_external_configuration() {
        let mut plan = plan();
        let external = plan
            .workspace
            .parent()
            .unwrap()
            .join("external/Sycamore.toml");
        plan.config = Some(external.clone());
        assert!(plan.relevant(&event(
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            external.clone()
        )));
        assert!(!plan.relevant(&event(
            EventKind::Create(CreateKind::File),
            external.with_file_name("unrelated.txt")
        )));
        let renamed = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(plan.workspace.join("dist/temp"))
            .add_path(plan.workspace.join("src/new.rs"));
        assert!(plan.relevant(&renamed));
    }

    #[test]
    fn coalesces_event_bursts_and_retains_changes_queued_during_a_build() {
        let plan = plan();
        let (sender, receiver) = mpsc::channel();
        for _ in 0..5 {
            sender
                .send(Ok(event(
                    EventKind::Create(CreateKind::File),
                    plan.workspace.join("src/main.rs"),
                )))
                .unwrap();
        }
        wait_for_change(&receiver, &plan, Duration::from_millis(1)).unwrap();
        assert!(receiver.try_recv().is_err());
        // Simulate edits arriving while the synchronous build was running.
        sender
            .send(Ok(event(
                EventKind::Remove(RemoveKind::File),
                plan.workspace.join("src/old.rs"),
            )))
            .unwrap();
        wait_for_change(&receiver, &plan, Duration::from_millis(1)).unwrap();
    }

    #[test]
    fn reports_all_changed_paths_once_without_generated_paths() {
        let plan = plan();
        let (sender, receiver) = mpsc::channel();
        let first = plan.workspace.join("src/main.rs");
        let second = plan.workspace.join("Cargo.toml");
        for path in [&first, &first, &second, &plan.workspace.join("dist/app.js")] {
            sender
                .send(Ok(event(EventKind::Create(CreateKind::File), path.clone())))
                .unwrap();
        }
        let changed = wait_for_change(&receiver, &plan, Duration::from_millis(1)).unwrap();
        assert_eq!(changed, BTreeSet::from([first, second]));
    }

    #[test]
    fn filters_common_editor_artifacts_without_hiding_real_inputs() {
        let plan = plan();
        for name in [
            "4913",
            "main.rs~",
            "main.rs.~1~",
            ".main.rs.un~",
            ".main.rs.swp",
            ".main.rs.swo",
            ".main.rs.swn",
            ".main.rs.swa",
            ".swp",
            "#main.rs#",
            ".#main.rs",
            "main.rs___jb_tmp___",
            "main.rs___jb_old___",
            "main.rs___jb_bak___",
            ".main.rs.kate-swp",
            ".goutputstream-ABC123",
        ] {
            let path = plan.workspace.join("src").join(name);
            assert!(is_editor_artifact(&path), "{name}");
            assert!(
                !plan.relevant(&event(EventKind::Create(CreateKind::File), path)),
                "{name}"
            );
        }
        for name in [
            "main.rs",
            ".env",
            ".gitignore",
            "input.tmp",
            "fixture.bak",
            "4913.rs",
            "main.rs.swp",
            "#heading.md",
            "theme#",
        ] {
            assert!(!is_editor_artifact(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn atomic_editor_save_logs_only_the_destination_source_file() {
        let plan = plan();
        let source = plan.workspace.join("src/main.rs");
        let (sender, receiver) = mpsc::channel();
        for name in ["src/4913", "src/main.rs~", "src/.main.rs.swp"] {
            sender
                .send(Ok(event(
                    EventKind::Create(CreateKind::File),
                    plan.workspace.join(name),
                )))
                .unwrap();
        }
        sender
            .send(Ok(Event::new(EventKind::Modify(ModifyKind::Name(
                RenameMode::Both,
            )))
            .add_path(plan.workspace.join("src/main.rs___jb_tmp___"))
            .add_path(source.clone())))
            .unwrap();
        let changed = wait_for_change(&receiver, &plan, Duration::from_millis(1)).unwrap();
        assert_eq!(changed, BTreeSet::from([source]));
    }

    #[derive(Default)]
    struct RecordingWatcher(Vec<(bool, PathBuf)>);

    impl Watcher for RecordingWatcher {
        fn new<F: notify::EventHandler>(_: F, _: notify::Config) -> notify::Result<Self> {
            Ok(Self::default())
        }
        fn watch(&mut self, path: &Path, _: RecursiveMode) -> notify::Result<()> {
            self.0.push((true, path.to_owned()));
            Ok(())
        }
        fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
            self.0.push((false, path.to_owned()));
            Ok(())
        }
        fn kind() -> notify::WatcherKind {
            notify::WatcherKind::NullWatcher
        }
    }

    #[test]
    fn replaces_overlapping_watch_roots_without_removing_new_watches() {
        let plan = plan();
        let parent = plan.workspace;
        let child = parent.join("member");
        let old = vec![
            (child.clone(), RecursiveMode::Recursive),
            (parent.clone(), RecursiveMode::NonRecursive),
        ];
        let new = vec![(parent.clone(), RecursiveMode::Recursive)];
        let mut watcher = RecordingWatcher::default();
        set_watches(&mut watcher, &old, &new).unwrap();
        assert_eq!(
            watcher.0,
            vec![(false, child), (false, parent.clone()), (true, parent)]
        );
        watcher.0.clear();
        set_watches(&mut watcher, &new, &new).unwrap();
        assert!(watcher.0.is_empty());
    }

    #[test]
    fn propagates_watcher_errors_and_disconnects() {
        let plan = plan();
        let (sender, receiver) = mpsc::channel();
        sender
            .send(Err(notify::Error::generic("watch failed")))
            .unwrap();
        assert!(wait_for_change(&receiver, &plan, Duration::ZERO).is_err());
        drop(sender);
        assert!(wait_for_change(&receiver, &plan, Duration::ZERO).is_err());
    }
}
