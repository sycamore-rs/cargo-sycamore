//! Compile a Cargo application to WASM and generate matching wasm-bindgen web bindings.

use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fs,
    io::BufReader,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail, ensure};
use cargo_metadata::{Message, Metadata, MetadataCommand, Package, Target};
use serde::Deserialize;

use crate::{
    options::Options,
    tools::{BuiltinTool, ToolCache},
};

const WASM_TARGET: &str = "wasm32-unknown-unknown";

pub fn build(options: &Options) -> Result<()> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    build_with(options, &cargo, |version| {
        eprintln!("Ensuring wasm-bindgen-cli {version} is available...");
        let mut cache = ToolCache::system()?;
        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            cache = cache.with_github_token(token);
        }
        cache.ensure(&BuiltinTool::WasmBindgenCli, version)
    })
}

fn build_with(
    options: &Options,
    cargo: &OsStr,
    ensure_bindgen: impl FnOnce(&str) -> Result<PathBuf>,
) -> Result<()> {
    let mut metadata_command = MetadataCommand::new();
    metadata_command.cargo_path(cargo).no_deps();
    if let Some(manifest) = &options.manifest_path {
        metadata_command.manifest_path(manifest);
    }
    let metadata = metadata_command
        .exec()
        .context("reading Cargo project metadata")?;
    let package = select_package(&metadata, options.package.as_deref())?;
    let target = select_target(package, options.bin.as_deref())?;

    let mut command = Command::new(cargo);
    command
        .arg("build")
        .arg("--manifest-path")
        .arg(&package.manifest_path)
        .arg("--package")
        .arg(package.name.as_str())
        .arg("--target")
        .arg(WASM_TARGET)
        .arg("--message-format=json-render-diagnostics");
    if target.is_bin() {
        command.arg("--bin").arg(&target.name);
    } else {
        command.arg("--lib");
    }
    if options.release {
        command.arg("--release");
    }
    if !options.features.is_empty() {
        command.arg("--features").arg(options.features.join(","));
    }
    if !options.default_features {
        command.arg("--no-default-features");
    }
    eprintln!("Building {} for {WASM_TARGET}...", package.name);
    let mut child = command
        .stdout(Stdio::piped())
        .spawn()
        .context("starting cargo build")?;
    let stdout = child
        .stdout
        .take()
        .context("capturing cargo build output")?;
    let mut wasm_files = BTreeSet::new();
    let mut parse_error = None;
    for message in Message::parse_stream(BufReader::new(stdout)) {
        match message {
            Ok(Message::CompilerArtifact(artifact))
                if artifact.package_id == package.id
                    && artifact.target.name == target.name
                    && artifact.target.kind == target.kind =>
            {
                for filename in artifact.filenames {
                    if filename.extension() == Some("wasm") {
                        wasm_files.insert(filename.into_std_path_buf());
                    }
                }
            }
            Ok(Message::TextLine(line)) => eprintln!("{line}"),
            Err(error) => parse_error = Some(error),
            _ => {}
        }
    }
    let status = child.wait().context("waiting for cargo build")?;
    ensure!(
        status.success(),
        "cargo build failed ({status}); see the errors above (if the target is missing, run `rustup target add {WASM_TARGET}`)"
    );
    if let Some(error) = parse_error {
        return Err(error).context("reading cargo build messages");
    }
    ensure!(
        wasm_files.len() == 1,
        "expected one WASM artifact for '{}', found {}",
        target.name,
        wasm_files.len()
    );
    let wasm = wasm_files
        .into_iter()
        .next()
        .context("missing WASM artifact")?;

    // Read after building: Cargo may create or update the workspace lockfile.
    let lockfile = metadata.workspace_root.join("Cargo.lock");
    let version = wasm_bindgen_version(lockfile.as_std_path())?;
    let bindgen = ensure_bindgen(&version)?;
    let out_dir = options.out_dir.clone().unwrap_or_else(|| {
        package
            .manifest_path
            .parent()
            .expect("manifest has a parent")
            .join("dist")
            .into_std_path_buf()
    });
    fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let output_name = wasm
        .file_stem()
        .and_then(|name| name.to_str())
        .context("WASM artifact has no UTF-8 filename")?;
    let status = Command::new(&bindgen)
        .arg(&wasm)
        .arg("--target")
        .arg("web")
        .arg("--no-typescript")
        .arg("--out-dir")
        .arg(&out_dir)
        .arg("--out-name")
        .arg(output_name)
        .status()
        .with_context(|| format!("running {}", bindgen.display()))?;
    ensure!(status.success(), "wasm-bindgen {version} failed ({status})");
    let project_root = package
        .manifest_path
        .parent()
        .context("manifest has no parent")?;
    crate::html::generate(project_root.as_std_path(), &out_dir, output_name)?;
    eprintln!("Built {} → {}", package.name, out_dir.display());
    Ok(())
}

fn select_package<'a>(metadata: &'a Metadata, name: Option<&str>) -> Result<&'a Package> {
    if let Some(name) = name {
        return metadata
            .workspace_packages()
            .into_iter()
            .find(|p| p.name == name)
            .with_context(|| format!("workspace has no package named '{name}'"));
    }
    let defaults: Vec<_> = metadata
        .workspace_packages()
        .into_iter()
        .filter(|package| metadata.workspace_default_members.contains(&package.id))
        .collect();
    ensure!(
        defaults.len() == 1,
        "workspace package selection is ambiguous; use --package <NAME>"
    );
    Ok(defaults[0])
}

fn select_target<'a>(package: &'a Package, bin: Option<&str>) -> Result<&'a Target> {
    if let Some(name) = bin {
        return package
            .targets
            .iter()
            .find(|t| t.is_bin() && t.name == name)
            .with_context(|| format!("package '{}' has no binary named '{name}'", package.name));
    }
    let libraries: Vec<_> = package.targets.iter().filter(|t| t.is_cdylib()).collect();
    if libraries.len() == 1 {
        return Ok(libraries[0]);
    }
    let binaries: Vec<_> = package.targets.iter().filter(|t| t.is_bin()).collect();
    ensure!(
        binaries.len() == 1,
        "package '{}' needs a cdylib library or one binary target; use --bin <NAME> to select a binary",
        package.name
    );
    Ok(binaries[0])
}

#[derive(Deserialize)]
struct Lockfile {
    #[serde(default)]
    package: Vec<LockedPackage>,
}

#[derive(Deserialize)]
struct LockedPackage {
    name: String,
    version: String,
    source: Option<String>,
}

/// Read the resolved wasm-bindgen crate version, never the manifest's semver range.
/// Multiple versions are rejected rather than risking a CLI/schema mismatch.
pub fn wasm_bindgen_version(lockfile: &Path) -> Result<String> {
    let contents =
        fs::read_to_string(lockfile).with_context(|| format!("reading {}", lockfile.display()))?;
    parse_wasm_bindgen_version(&contents)
        .with_context(|| format!("detecting wasm-bindgen version in {}", lockfile.display()))
}

fn parse_wasm_bindgen_version(contents: &str) -> Result<String> {
    let lockfile: Lockfile = toml::from_str(contents).context("parsing Cargo.lock")?;
    let packages: Vec<_> = lockfile
        .package
        .into_iter()
        .filter(|p| p.name == "wasm-bindgen")
        .collect();
    ensure!(
        !packages.is_empty(),
        "Cargo.lock has no wasm-bindgen dependency"
    );
    for package in &packages {
        ensure!(
            package.source.as_deref().is_some_and(
                |source| source.starts_with("registry+") || source.starts_with("sparse+")
            ),
            "wasm-bindgen is a path/git dependency; a matching GitHub CLI release cannot be guaranteed"
        );
    }
    let versions: BTreeSet<_> = packages.into_iter().map(|p| p.version).collect();
    if versions.len() != 1 {
        bail!(
            "Cargo.lock contains multiple wasm-bindgen versions ({}); unify them before building",
            versions.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    Ok(versions.into_iter().next().expect("nonempty version set"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGISTRY: &str = "registry+https://github.com/rust-lang/crates.io-index";

    fn locked(version: &str) -> String {
        format!(
            "[[package]]\nname = \"wasm-bindgen\"\nversion = \"{version}\"\nsource = \"{REGISTRY}\"\n"
        )
    }

    #[test]
    fn reads_exact_resolved_version() {
        let contents = format!(
            "version = 4\n{}\n[[package]]\nname = \"wasm-bindgen-shared\"\nversion = \"0.2.99\"\n",
            locked("0.2.100")
        );
        assert_eq!(parse_wasm_bindgen_version(&contents).unwrap(), "0.2.100");
    }

    #[test]
    fn accepts_sparse_registry_sources() {
        let contents = locked("0.2.100").replace(REGISTRY, "sparse+https://example.com/index/");
        assert_eq!(parse_wasm_bindgen_version(&contents).unwrap(), "0.2.100");
    }

    #[test]
    fn rejects_missing_ambiguous_or_unreleased_bindgen() {
        for contents in [
            "version = 4".into(),
            "invalid toml".into(),
            format!("{}\n{}", locked("0.2.99"), locked("0.2.100")),
            "[[package]]\nname = \"wasm-bindgen\"\nversion = \"0.2.100\"".into(),
            locked("0.2.100").replace(REGISTRY, "git+https://github.com/example/wasm-bindgen"),
        ] {
            assert!(parse_wasm_bindgen_version(&contents).is_err(), "{contents}");
        }
    }

    fn fixture() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("src")).unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"test-app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[workspace]\n",
        )
        .unwrap();
        fs::write(temp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        temp
    }

    fn metadata(root: &Path) -> Metadata {
        MetadataCommand::new()
            .manifest_path(root.join("Cargo.toml"))
            .no_deps()
            .exec()
            .unwrap()
    }

    #[test]
    fn selects_binary_or_cdylib_and_validates_names() {
        let temp = fixture();
        let meta = metadata(temp.path());
        let package = select_package(&meta, None).unwrap();
        assert_eq!(select_target(package, None).unwrap().name, "test-app");
        assert!(select_package(&meta, Some("absent")).is_err());
        assert!(select_target(package, Some("absent")).is_err());
        fs::create_dir(temp.path().join("src/bin")).unwrap();
        fs::write(temp.path().join("src/bin/other.rs"), "fn main() {}\n").unwrap();
        let meta = metadata(temp.path());
        let package = select_package(&meta, None).unwrap();
        assert!(select_target(package, None).is_err());
        assert_eq!(select_target(package, Some("other")).unwrap().name, "other");
        let manifest = temp.path().join("Cargo.toml");
        let contents = fs::read_to_string(&manifest).unwrap();
        fs::write(
            &manifest,
            format!("{contents}\n[lib]\ncrate-type = [\"cdylib\"]\n"),
        )
        .unwrap();
        fs::write(temp.path().join("src/lib.rs"), "pub fn app() {}\n").unwrap();
        let meta = metadata(temp.path());
        assert!(
            select_target(select_package(&meta, None).unwrap(), None)
                .unwrap()
                .is_cdylib()
        );
    }

    #[test]
    fn respects_workspace_defaults_and_member_manifest() {
        let temp = fixture();
        let root = temp.path();
        let manifest = root.join("Cargo.toml");
        let contents = fs::read_to_string(&manifest).unwrap().replace(
            "[workspace]",
            "[workspace]\nmembers = [\"member\"]\ndefault-members = [\"member\"]",
        );
        fs::write(&manifest, &contents).unwrap();
        fs::create_dir_all(root.join("member/src")).unwrap();
        fs::write(
            root.join("member/Cargo.toml"),
            "[package]\nname = \"member\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(root.join("member/src/main.rs"), "fn main() {}\n").unwrap();
        for path in [root.to_path_buf(), root.join("member")] {
            let meta = metadata(&path);
            assert_eq!(select_package(&meta, None).unwrap().name.as_str(), "member");
        }
        // Cargo's metadata defaults reflect the invocation manifest even when the
        // workspace root is itself a package and has no explicit default-members.
        fs::write(
            &manifest,
            contents.replace("default-members = [\"member\"]\n", ""),
        )
        .unwrap();
        assert_eq!(
            select_package(&metadata(root), None).unwrap().name.as_str(),
            "test-app"
        );
        assert_eq!(
            select_package(&metadata(&root.join("member")), None)
                .unwrap()
                .name
                .as_str(),
            "member"
        );
    }

    #[test]
    fn virtual_workspace_requires_an_unambiguous_package() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        for name in ["a", "b"] {
            fs::create_dir_all(temp.path().join(name).join("src")).unwrap();
            fs::write(
                temp.path().join(name).join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
            )
            .unwrap();
            fs::write(temp.path().join(name).join("src/main.rs"), "fn main() {}\n").unwrap();
        }
        let meta = metadata(temp.path());
        assert!(select_package(&meta, None).is_err());
        assert_eq!(select_package(&meta, Some("b")).unwrap().name.as_str(), "b");
    }

    #[cfg(unix)]
    mod pipeline {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn quote(value: impl AsRef<OsStr>) -> String {
            format!(
                "'{}'",
                value.as_ref().to_string_lossy().replace('\'', "'\\''")
            )
        }

        fn script(path: &Path, contents: &str) {
            fs::write(path, format!("#!/bin/sh\nset -eu\n{contents}\n")).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn mock_cargo(root: &Path, fail: bool) -> PathBuf {
            let meta = metadata(root);
            let package = select_package(&meta, None).unwrap();
            let target = select_target(package, None).unwrap();
            let wasm = root.join("custom-target/test_app.wasm");
            fs::create_dir_all(wasm.parent().unwrap()).unwrap();
            fs::write(&wasm, b"fake wasm").unwrap();
            let artifact = serde_json::json!({
                "reason": "compiler-artifact", "package_id": package.id,
                "manifest_path": package.manifest_path, "target": target,
                "profile": {"opt_level":"0", "debuginfo":0, "debug_assertions":true, "overflow_checks":true, "test":false},
                "features":[], "filenames":[wasm], "executable":null, "fresh":false,
            });
            let real_cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let cargo = root.join("mock-cargo");
            script(
                &cargo,
                &format!(
                    "if [ \"$1\" = metadata ]; then exec {} \"$@\"; fi\nprintf '%s\\n' \"$@\" > {}\n{}\nprintf '%s\\n' {}",
                    quote(real_cargo),
                    quote(root.join("cargo-args")),
                    if fail { "exit 1" } else { "true" },
                    quote(artifact.to_string()),
                ),
            );
            cargo
        }

        #[test]
        fn builds_then_downloads_matching_cli_and_runs_web_bindings() {
            let temp = fixture();
            let root = temp.path();
            let cargo = mock_cargo(root, false);
            fs::write(
                root.join("Cargo.lock"),
                format!("version = 4\n{}", locked("0.2.100")),
            )
            .unwrap();
            let bindgen = root.join("wasm-bindgen");
            script(
                &bindgen,
                &format!(
                    "printf '%s\\n' \"$@\" > {}",
                    quote(root.join("bindgen-args"))
                ),
            );
            let options = Options {
                manifest_path: Some(root.join("Cargo.toml")),
                release: true,
                features: vec!["csr".into(), "extra".into()],
                default_features: false,
                ..Default::default()
            };
            build_with(&options, cargo.as_os_str(), |version| {
                assert_eq!(version, "0.2.100");
                assert!(root.join("cargo-args").exists());
                Ok(bindgen)
            })
            .unwrap();
            let args = fs::read_to_string(root.join("cargo-args")).unwrap();
            assert!(args.contains("--target\nwasm32-unknown-unknown\n"));
            assert!(args.contains("--release\n"));
            assert!(args.contains("--features\ncsr,extra\n"));
            assert!(args.contains("--no-default-features\n"));
            let args = fs::read_to_string(root.join("bindgen-args")).unwrap();
            assert!(args.contains("custom-target/test_app.wasm\n"));
            assert!(args.contains("--target\nweb\n--no-typescript\n--out-dir\n"));
            assert!(root.join("dist").is_dir());
            let html = fs::read_to_string(root.join("dist/index.html")).unwrap();
            assert!(html.contains("./test_app.js"));
            assert!(args.contains("--out-name\ntest_app\n"));
        }

        #[test]
        fn failed_cargo_does_not_download_tools() {
            let temp = fixture();
            let cargo = mock_cargo(temp.path(), true);
            let options = Options {
                manifest_path: Some(temp.path().join("Cargo.toml")),
                ..Default::default()
            };
            let error = build_with(&options, cargo.as_os_str(), |_| {
                panic!("must not download after failed build")
            })
            .unwrap_err();
            assert!(error.to_string().contains("cargo build failed"));
        }

        #[test]
        fn bindgen_failure_is_reported() {
            let temp = fixture();
            let root = temp.path();
            let cargo = mock_cargo(root, false);
            fs::write(root.join("Cargo.lock"), locked("0.2.100")).unwrap();
            let bindgen = root.join("wasm-bindgen");
            script(&bindgen, "exit 1");
            let options = Options {
                manifest_path: Some(root.join("Cargo.toml")),
                out_dir: Some(root.join("output")),
                ..Default::default()
            };
            let error = build_with(&options, cargo.as_os_str(), |_| Ok(bindgen)).unwrap_err();
            assert!(error.to_string().contains("wasm-bindgen 0.2.100 failed"));
            assert!(root.join("output").is_dir());
        }
    }
}
