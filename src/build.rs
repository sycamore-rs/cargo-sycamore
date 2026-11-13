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
use cargo_metadata::{CrateType, Message, Metadata, MetadataCommand, Package, Target};
use serde::Deserialize;

use crate::{
    options::{Options, TailwindOptions},
    tools::{BuiltinTool, ToolCache},
};

const WASM_TARGET: &str = "wasm32-unknown-unknown";

/// Public results from a completed application build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildOutput {
    /// Build root containing sycamore-assets.json and the public/ directory.
    pub directory: PathBuf,
    pub server: Option<PathBuf>,
}

/// Build the client and, when configured, its native server executable.
pub fn build_application(options: &Options) -> Result<BuildOutput> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    build_application_with(options, &cargo, |tool, version| {
        let name = match tool {
            BuiltinTool::WasmBindgenCli => "wasm-bindgen-cli",
            BuiltinTool::WasmOpt => "wasm-opt",
            BuiltinTool::TailwindCss => "tailwindcss",
        };
        crate::logging::debug(format_args!("Ensuring {name} {version} is available..."));
        let mut cache = ToolCache::system()?;
        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            cache = cache.with_github_token(token);
        }
        cache.ensure(&tool, version)
    })
}

/// Build the application and return its build output directory.
pub fn build(options: &Options) -> Result<PathBuf> {
    Ok(build_application(options)?.directory)
}

fn build_application_with(
    options: &Options,
    cargo: &OsStr,
    mut ensure_tool: impl FnMut(BuiltinTool, &str) -> Result<PathBuf>,
) -> Result<BuildOutput> {
    // Compile the backend first: a failed native build must not publish a new
    // client generation over the currently served output.
    let server = options
        .server
        .as_ref()
        .map(|config| build_server(options, config, cargo))
        .transpose()?;
    let directory = build_client_with(
        options,
        cargo,
        &mut ensure_tool,
        options.mode == crate::options::BuildMode::Spa,
        server.as_deref(),
    )?;
    Ok(BuildOutput { directory, server })
}

#[cfg(test)]
fn build_with(
    options: &Options,
    cargo: &OsStr,
    mut ensure_tool: impl FnMut(BuiltinTool, &str) -> Result<PathBuf>,
) -> Result<PathBuf> {
    build_client_with(options, cargo, &mut ensure_tool, true, None)
}

fn build_client_with(
    options: &Options,
    cargo: &OsStr,
    mut ensure_tool: impl FnMut(BuiltinTool, &str) -> Result<PathBuf>,
    generate_shell: bool,
    server: Option<&Path>,
) -> Result<PathBuf> {
    let mut metadata_command = MetadataCommand::new();
    metadata_command.cargo_path(cargo).no_deps();
    if let Some(manifest) = &options.manifest_path {
        metadata_command.manifest_path(manifest);
    }
    let metadata = metadata_command
        .exec()
        .context("reading Cargo project metadata")?;
    let package = select_package(&metadata, options.package.as_deref())?;
    let target = select_target(package, options.bin.as_deref(), options.example.as_deref())?;

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
    if target.is_example() {
        command.arg("--example").arg(&target.name);
    } else if target.is_bin() {
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
    crate::logging::status(
        "Building",
        format_args!("{} for {WASM_TARGET}...", package.name),
    );
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
            Ok(Message::TextLine(line)) => crate::logging::raw(format_args!("{line}")),
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
    let bindgen = ensure_tool(BuiltinTool::WasmBindgenCli, &version)?;
    // Obtain all tools before bindgen can overwrite an existing served build.
    let optimizer = if options.release && options.wasm_opt {
        Some(
            ensure_tool(BuiltinTool::WasmOpt, &options.wasm_opt_version)
                .context("ensuring wasm-opt is available (disable with --wasm-opt=false)")?,
        )
    } else {
        None
    };
    let tailwind = if let Some(config) = &options.tailwindcss {
        ensure!(
            config.input.is_file(),
            "Tailwind CSS input {} is not a file",
            config.input.display()
        );
        Some(
            ensure_tool(BuiltinTool::TailwindCss, &config.version)
                .context("ensuring Tailwind CSS is available")?,
        )
    } else {
        None
    };
    let directory = package_output_directory(options, package);
    let staging = prepare_staging(&directory, options, &metadata, &wasm, server)?;
    let out_dir = staging.join("public");
    fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    // Generate artifacts after assets so static inputs cannot replace the
    // freshly generated bindings, optimized WASM, or stylesheet.
    crate::assets::copy(&options.assets, &out_dir)?;
    let output_name = wasm
        .file_stem()
        .and_then(|name| name.to_str())
        .context("WASM artifact has no UTF-8 filename")?;
    crate::logging::status(
        "Generating",
        format_args!("web bindings for {}", package.name),
    );
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
    if let Some(optimizer) = optimizer {
        optimize_wasm(&out_dir.join(format!("{output_name}_bg.wasm")), &optimizer)?;
    }
    let project_root = package
        .manifest_path
        .parent()
        .context("manifest has no parent")?;
    let stylesheet = if let (Some(config), Some(tailwind)) = (&options.tailwindcss, tailwind) {
        Some(compile_tailwind(
            config,
            &tailwind,
            project_root.as_std_path(),
            &out_dir,
            options.release,
        )?)
    } else {
        None
    };
    crate::html::generate_bootstrap_and_manifest(
        &staging,
        &out_dir,
        output_name,
        stylesheet.as_deref(),
        &options.public_url,
    )?;
    if generate_shell {
        crate::html::generate_with_stylesheet(
            project_root.as_std_path(),
            &out_dir,
            output_name,
            stylesheet.as_deref(),
            &options.public_url,
        )?;
    }
    crate::logging::debug(format_args!(
        "Publishing staged build to {}",
        directory.display()
    ));
    publish_staging(&directory, &staging)?;
    crate::logging::status(
        "Finished",
        format_args!("{} → {}", package.name, directory.display()),
    );
    Ok(directory)
}

/// Prepare a disposable generation without touching the published output.
fn prepare_staging(
    directory: &Path,
    options: &Options,
    metadata: &Metadata,
    wasm: &Path,
    server: Option<&Path>,
) -> Result<PathBuf> {
    fs::create_dir_all(directory)
        .with_context(|| format!("creating build output directory {}", directory.display()))?;
    let output = fs::canonicalize(directory)?;
    // Successful publication recursively deletes the old output. Refuse paths
    // that would consume the project, its inputs, or Cargo's build artifacts.
    let protect = |path: &Path| -> Result<()> {
        if let Ok(path) = fs::canonicalize(path) {
            ensure!(
                !path.starts_with(&output),
                "build output directory {} contains protected input or build artifacts: {}",
                directory.display(),
                path.display()
            );
        }
        Ok(())
    };
    protect(metadata.workspace_root.as_std_path())?;
    protect(metadata.target_directory.as_std_path())?;
    for package in metadata.workspace_packages() {
        let project = package
            .manifest_path
            .parent()
            .context("manifest has no parent")?;
        protect(project.as_std_path())?;
        protect(project.join("index.html").as_std_path())?;
        let project = fs::canonicalize(project)?;
        for target in &package.targets {
            protect(target.src_path.as_std_path())?;
            if let Some(source) = target.src_path.parent()
                && let Ok(source) = fs::canonicalize(source)
                && source != project
            {
                ensure!(
                    !output.starts_with(&source),
                    "build output directory {} is inside source directory {}",
                    directory.display(),
                    source.display()
                );
            }
        }
    }
    protect(wasm)?;
    if let Some(server) = server {
        protect(server)?;
    }
    if let Some(config) = &options.config_path {
        protect(config)?;
    }
    if let Some(tailwind) = &options.tailwindcss {
        protect(&tailwind.input)?;
    }
    for asset in &options.assets {
        protect(&asset.from)?;
    }
    let staging = directory.join(".staging");
    remove_output_entry(&staging)?;
    fs::create_dir(&staging)
        .with_context(|| format!("creating staging directory {}", staging.display()))?;
    Ok(staging)
}

/// Delete-and-copy publication deliberately happens only after generation has
/// succeeded. This is not an atomic filesystem swap; keep staging if copying fails.
fn publish_staging(directory: &Path, staging: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_name() != ".staging" {
            remove_output_entry(&entry.path())?;
        }
    }
    crate::assets::copy(
        &[crate::assets::Asset {
            from: staging.to_owned(),
            to: Some(".".into()),
        }],
        directory,
    )
    .context("publishing staged build output")?;
    if let Err(error) = remove_output_entry(staging) {
        crate::logging::warn(format_args!(
            "published output but could not remove staging: {error:#}"
        ));
    }
    Ok(())
}

/// Do not follow output symlinks when clearing previous generations.
fn remove_output_entry(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .with_context(|| format!("removing {}", path.display()))
}

fn build_server(
    options: &Options,
    config: &crate::options::ServerOptions,
    cargo: &OsStr,
) -> Result<PathBuf> {
    let mut metadata_command = MetadataCommand::new();
    metadata_command.cargo_path(cargo).no_deps();
    if let Some(manifest) = &options.manifest_path {
        metadata_command.manifest_path(manifest);
    }
    let metadata = metadata_command
        .exec()
        .context("reading Cargo project metadata for server build")?;
    // Server package/bin selection is independent from the client target, while
    // an omitted server package follows the same package selection convention.
    let package = select_package(
        &metadata,
        config.package.as_deref().or(options.package.as_deref()),
    )?;
    let target = select_server_target(package, config.bin.as_deref())?;
    let host = rust_host()?;
    let mut command = Command::new(cargo);
    command
        .arg("build")
        .arg("--manifest-path")
        .arg(&package.manifest_path)
        .arg("--package")
        .arg(package.name.as_str())
        .arg("--target")
        .arg(&host)
        .arg("--bin")
        .arg(&target.name)
        .arg("--message-format=json-render-diagnostics");
    if options.release {
        command.arg("--release");
    }
    if !config.features.is_empty() {
        command.arg("--features").arg(config.features.join(","));
    }
    if !config.default_features {
        command.arg("--no-default-features");
    }
    crate::logging::status(
        "Building",
        format_args!("server {} for {host}...", package.name),
    );
    let mut child = command
        .stdout(Stdio::piped())
        .spawn()
        .context("starting native server cargo build")?;
    let stdout = child
        .stdout
        .take()
        .context("capturing native cargo build output")?;
    let mut executables = BTreeSet::new();
    let mut parse_error = None;
    for message in Message::parse_stream(BufReader::new(stdout)) {
        match message {
            Ok(Message::CompilerArtifact(artifact))
                if artifact.package_id == package.id
                    && artifact.target.name == target.name
                    && artifact.target.is_bin() =>
            {
                if let Some(executable) = artifact.executable {
                    executables.insert(executable.into_std_path_buf());
                }
            }
            Ok(Message::TextLine(line)) => crate::logging::raw(format_args!("{line}")),
            Err(error) => parse_error = Some(error),
            _ => {}
        }
    }
    let status = child.wait().context("waiting for native cargo build")?;
    ensure!(status.success(), "native cargo build failed ({status})");
    if let Some(error) = parse_error {
        return Err(error).context("reading native cargo build messages");
    }
    ensure!(
        executables.len() == 1,
        "expected one executable for server binary '{}', found {}",
        target.name,
        executables.len()
    );
    let executable = executables
        .into_iter()
        .next()
        .context("missing server executable")?;
    ensure!(
        executable.is_file(),
        "Cargo reported server executable {} but it does not exist",
        executable.display()
    );
    crate::logging::status(
        "Finished",
        format_args!("server {} → {}", package.name, executable.display()),
    );
    Ok(executable)
}

fn select_server_target<'a>(package: &'a Package, bin: Option<&str>) -> Result<&'a Target> {
    if let Some(bin) = bin {
        let target = package
            .targets
            .iter()
            .find(|target| target.name == bin)
            .with_context(|| format!("package '{}' has no target named '{bin}'", package.name))?;
        ensure!(
            target.is_bin(),
            "server target '{bin}' in package '{}' is not a binary target",
            package.name
        );
        return Ok(target);
    }
    let bins: Vec<_> = package
        .targets
        .iter()
        .filter(|target| target.is_bin())
        .collect();
    ensure!(
        bins.len() == 1,
        "package '{}' needs exactly one binary target for the server build; configure server.bin",
        package.name
    );
    Ok(bins[0])
}

fn rust_host() -> Result<String> {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = Command::new(&rustc).arg("-vV").output().with_context(|| {
        format!(
            "running {} -vV to detect the native host target",
            Path::new(&rustc).display()
        )
    })?;
    ensure!(
        output.status.success(),
        "rustc -vV failed ({})",
        output.status
    );
    let version = String::from_utf8(output.stdout).context("rustc -vV output is not UTF-8")?;
    version
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_owned)
        .context("rustc -vV did not report a host target")
}

/// Optimize the bindgen-generated module, not Cargo's input artifact. Publish
/// only a successful, nonempty output so a failed optimizer cannot truncate it.
fn optimize_wasm(wasm: &Path, optimizer: &Path) -> Result<()> {
    let metadata = fs::metadata(wasm)
        .with_context(|| format!("reading wasm-bindgen output {}", wasm.display()))?;
    let output = tempfile::Builder::new()
        .prefix(".cargo-sycamore-wasm-opt-")
        .suffix(".wasm")
        .tempfile_in(wasm.parent().context("WASM output has no parent")?)
        .context("creating temporary wasm-opt output")?;
    crate::logging::status(
        "Optimizing",
        format_args!("{} with wasm-opt (-Oz)...", wasm.display()),
    );
    let status = Command::new(optimizer)
        .arg(wasm)
        .arg("-Oz")
        .arg("--all-features")
        // Compact import encoding is not yet supported by browser runtimes.
        // Keep this after --all-features so the explicit exclusion wins.
        .arg("--disable-compact-imports")
        .arg("-o")
        .arg(output.path())
        .status()
        .with_context(|| format!("running wasm-opt {}", optimizer.display()))?;
    ensure!(
        status.success(),
        "wasm-opt failed ({status}); disable with --wasm-opt=false"
    );
    ensure!(
        fs::metadata(output.path())?.len() > 0,
        "wasm-opt produced empty output"
    );
    fs::set_permissions(output.path(), metadata.permissions())?;
    output
        .persist(wasm)
        .with_context(|| format!("publishing optimized {}", wasm.display()))?;
    Ok(())
}

/// Run once per build, publishing CSS only after the compiler succeeds. The
/// outer workspace watcher owns rebuilding; do not start a second --watch loop.
fn compile_tailwind(
    config: &TailwindOptions,
    executable: &Path,
    project: &Path,
    out_dir: &Path,
    release: bool,
) -> Result<PathBuf> {
    let output = config.output_path(out_dir)?;
    let input = fs::canonicalize(&config.input).context("resolving Tailwind CSS input")?;
    ensure!(
        fs::canonicalize(&output).ok().as_ref() != Some(&input),
        "Tailwind CSS output must not overwrite its input"
    );
    let parent = output
        .parent()
        .context("Tailwind CSS output has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating CSS output directory {}", parent.display()))?;
    let temporary = tempfile::Builder::new()
        .prefix(".cargo-sycamore-tailwind-")
        .suffix(".css")
        .tempfile_in(parent)
        .context("creating temporary Tailwind CSS output")?;
    let mut command = Command::new(executable);
    command
        .current_dir(project)
        .arg("--input")
        .arg(&input)
        .arg("--output")
        .arg(temporary.path());
    if release {
        command.arg("--minify");
    }
    crate::logging::status(
        "Compiling",
        format_args!(
            "Tailwind CSS {} → {}...",
            config.input.display(),
            output.display()
        ),
    );
    let status = command
        .status()
        .with_context(|| format!("running Tailwind CSS {}", executable.display()))?;
    ensure!(
        status.success(),
        "Tailwind CSS {} failed ({status})",
        config.version
    );
    // Keep published CSS readable like other generated static assets.
    let permissions = fs::metadata(&output)
        .or_else(|_| fs::metadata(&input))?
        .permissions();
    fs::set_permissions(temporary.path(), permissions)?;
    temporary
        .persist(&output)
        .with_context(|| format!("publishing Tailwind CSS {}", output.display()))?;
    Ok(output)
}

/// Resolve the same output directory as build, without compiling anything.
pub(crate) fn output_directory(options: &Options) -> Result<PathBuf> {
    let mut command = MetadataCommand::new();
    command.no_deps();
    if let Some(cargo) = std::env::var_os("CARGO") {
        command.cargo_path(cargo);
    }
    if let Some(manifest) = &options.manifest_path {
        command.manifest_path(manifest);
    }
    let metadata = command.exec().context("reading Cargo project metadata")?;
    let package = select_package(&metadata, options.package.as_deref())?;
    Ok(package_output_directory(options, package))
}

fn package_output_directory(options: &Options, package: &Package) -> PathBuf {
    options.out_dir.clone().unwrap_or_else(|| {
        package
            .manifest_path
            .parent()
            .expect("manifest has a parent")
            .join("dist")
            .into_std_path_buf()
    })
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

fn select_target<'a>(
    package: &'a Package,
    bin: Option<&str>,
    example: Option<&str>,
) -> Result<&'a Target> {
    ensure!(
        bin.is_none() || example.is_none(),
        "--bin and --example are mutually exclusive"
    );
    if let Some(name) = example {
        let target = package
            .targets
            .iter()
            .find(|target| target.is_example() && target.name == name)
            .with_context(|| format!("package '{}' has no example named '{name}'", package.name))?;
        ensure!(
            target
                .crate_types
                .iter()
                .any(|kind| matches!(kind, CrateType::Bin | CrateType::CDyLib)),
            "example '{name}' must have a bin or cdylib crate type to produce WASM"
        );
        return Ok(target);
    }
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
        "package '{}' needs a cdylib library or one binary target; use --bin <NAME> or --example <NAME> to select a target",
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
        assert_eq!(select_target(package, None, None).unwrap().name, "test-app");
        assert!(select_package(&meta, Some("absent")).is_err());
        assert!(select_target(package, Some("absent"), None).is_err());
        fs::create_dir(temp.path().join("src/bin")).unwrap();
        fs::write(temp.path().join("src/bin/other.rs"), "fn main() {}\n").unwrap();
        let meta = metadata(temp.path());
        let package = select_package(&meta, None).unwrap();
        assert!(select_target(package, None, None).is_err());
        assert_eq!(
            select_target(package, Some("other"), None).unwrap().name,
            "other"
        );
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
            select_target(select_package(&meta, None).unwrap(), None, None)
                .unwrap()
                .is_cdylib()
        );
    }

    #[test]
    fn server_target_must_be_an_unambiguous_binary() {
        let temp = fixture();
        let root = temp.path();
        fs::write(root.join("src/lib.rs"), "pub fn app() {}\n").unwrap();
        fs::write(
            root.join("Cargo.toml"),
            r#"[package]
name = "test-app"
version = "0.1.0"
edition = "2024"
[workspace]
[lib]
name = "library"
"#,
        )
        .unwrap();
        let meta = metadata(root);
        let package = select_package(&meta, None).unwrap();
        assert!(select_server_target(package, Some("library")).is_err());
        assert_eq!(
            select_server_target(package, None).unwrap().name,
            "test-app"
        );

        fs::create_dir(root.join("src/bin")).unwrap();
        fs::write(root.join("src/bin/another.rs"), "fn main() {}\n").unwrap();
        let meta = metadata(root);
        let package = select_package(&meta, None).unwrap();
        assert!(select_server_target(package, None).is_err());
        assert_eq!(
            select_server_target(package, Some("another")).unwrap().name,
            "another"
        );
    }

    #[test]
    fn selects_named_examples_without_changing_default_targets() {
        let temp = fixture();
        let root = temp.path();
        fs::create_dir_all(root.join("examples/nested")).unwrap();
        fs::write(root.join("examples/counter.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("examples/nested/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("examples/custom.rs"), "pub fn app() {}\n").unwrap();
        fs::write(root.join("examples/library.rs"), "pub fn app() {}\n").unwrap();
        let manifest = root.join("Cargo.toml");
        let original = fs::read_to_string(&manifest).unwrap();
        fs::write(&manifest, format!("{original}\n[[example]]\nname = 'custom-name'\npath = 'examples/custom.rs'\ncrate-type = ['cdylib']\n\n[[example]]\nname = 'library'\ncrate-type = ['rlib']\n")).unwrap();
        let meta = metadata(root);
        let package = select_package(&meta, None).unwrap();
        for name in ["counter", "nested", "custom-name"] {
            assert!(
                select_target(package, None, Some(name))
                    .unwrap()
                    .is_example()
            );
        }
        assert!(select_target(package, None, None).unwrap().is_bin());
        assert!(
            select_target(package, None, Some("absent"))
                .unwrap_err()
                .to_string()
                .contains("no example named")
        );
        assert!(
            select_target(package, None, Some("library"))
                .unwrap_err()
                .to_string()
                .contains("bin or cdylib")
        );
        assert!(select_target(package, Some("test-app"), Some("counter")).is_err());
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
            mock_cargo_target(root, fail, None)
        }

        fn mock_application_cargo(root: &Path, native_fail: bool) -> PathBuf {
            let meta = metadata(root);
            let package = select_package(&meta, None).unwrap();
            let client = select_target(package, None, None).unwrap();
            let server = select_server_target(package, None).unwrap();
            let client_wasm = root.join("custom-target/test_app.wasm");
            let executable = root.join("custom-native/test-app");
            fs::create_dir_all(client_wasm.parent().unwrap()).unwrap();
            fs::write(&client_wasm, b"fake wasm").unwrap();
            let client_artifact = serde_json::json!({
                "reason":"compiler-artifact", "package_id":package.id,
                "manifest_path":package.manifest_path, "target":client,
                "profile":{"opt_level":"0", "debuginfo":0, "debug_assertions":true, "overflow_checks":true, "test":false},
                "features":[], "filenames":[client_wasm], "executable":null, "fresh":false
            });
            let server_artifact = serde_json::json!({
                "reason":"compiler-artifact", "package_id":package.id,
                "manifest_path":package.manifest_path, "target":server,
                "profile":{"opt_level":"0", "debuginfo":0, "debug_assertions":true, "overflow_checks":true, "test":false},
                "features":[], "filenames":[executable], "executable":executable, "fresh":false
            });
            let real_cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let cargo = root.join("mock-application-cargo");
            script(
                &cargo,
                &format!(
                    r#"if [ "$1" = metadata ]; then exec {} "$@"; fi
if [ "$1" = -vV ]; then printf 'host: x86_64-unknown-linux-gnu\n'; exit 0; fi
printf '%s\n' "$*" >> {}
if echo "$*" | grep -q wasm32-unknown-unknown; then printf wasm > {}; printf '%s\n' {}; else
{}
mkdir -p {}
printf server > {}
printf '%s\n' {}; fi"#,
                    quote(real_cargo),
                    quote(root.join("build-args")),
                    quote(&client_wasm),
                    quote(client_artifact.to_string()),
                    if native_fail { "exit 1" } else { "true" },
                    quote(executable.parent().unwrap()),
                    quote(&executable),
                    quote(server_artifact.to_string()),
                ),
            );
            cargo
        }

        fn mock_cargo_target(root: &Path, fail: bool, example: Option<&str>) -> PathBuf {
            let meta = metadata(root);
            let package = select_package(&meta, None).unwrap();
            let target = select_target(package, None, example).unwrap();
            let wasm = root
                .join("custom-target")
                .join(format!("{}.wasm", target.name.replace('-', "_")));
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
            fs::write(root.join("asset.txt"), "static asset").unwrap();
            let options = Options {
                manifest_path: Some(root.join("Cargo.toml")),
                release: true,
                wasm_opt: false,
                assets: vec![crate::assets::Asset {
                    from: root.join("asset.txt"),
                    to: Some("static/renamed.txt".into()),
                }],
                features: vec!["csr".into(), "extra".into()],
                default_features: false,
                ..Default::default()
            };
            build_with(&options, cargo.as_os_str(), |tool, version| {
                assert!(matches!(tool, BuiltinTool::WasmBindgenCli));
                assert_eq!(version, "0.2.100");
                assert!(root.join("cargo-args").exists());
                Ok(bindgen.clone())
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
            assert!(!root.join("dist/.staging").exists());
            assert_eq!(
                fs::read_to_string(root.join("dist/public/static/renamed.txt")).unwrap(),
                "static asset"
            );
            let html = fs::read_to_string(root.join("dist/public/index.html")).unwrap();
            assert!(html.contains("/test_app.js"));
            assert!(args.contains("--out-name\ntest_app\n"));
        }

        #[test]
        fn builds_ssr_client_and_native_server_with_independent_targets_and_features() {
            let temp = fixture();
            let root = temp.path();
            let cargo = mock_application_cargo(root, false);
            fs::write(root.join("Cargo.lock"), locked("0.2.100")).unwrap();
            let bindgen = root.join("wasm-bindgen");
            script(
                &bindgen,
                r#"while [ "$#" -gt 0 ]; do case "$1" in --out-dir) shift; out=$1;; --out-name) shift; name=$1;; esac; shift; done
printf js > "$out/$name.js"
printf wasm > "$out/${name}_bg.wasm""#,
            );
            let out = root.join("dist");
            fs::create_dir_all(out.join("public")).unwrap();
            fs::write(out.join("public/index.html"), "user-owned index").unwrap();
            let options = Options {
                manifest_path: Some(root.join("Cargo.toml")),
                mode: crate::options::BuildMode::Ssr,
                features: vec!["frontend".into()],
                default_features: true,
                server: Some(crate::options::ServerOptions {
                    package: None,
                    bin: None,
                    features: vec!["backend".into()],
                    default_features: false,
                }),
                wasm_opt: false,
                ..Default::default()
            };
            let output = build_application_with(&options, cargo.as_os_str(), |tool, _| {
                assert!(matches!(tool, BuiltinTool::WasmBindgenCli));
                Ok(bindgen.clone())
            })
            .unwrap();

            assert_eq!(output.directory, out);
            assert_eq!(
                output.server.as_deref(),
                Some(root.join("custom-native/test-app").as_path())
            );
            assert!(!output.directory.join("public/index.html").exists());
            assert!(!output.directory.join(".staging").exists());
            let manifest: serde_json::Value = serde_json::from_slice(
                &fs::read(output.directory.join("sycamore-assets.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(manifest["bootstrap"], "/test_app.bootstrap.js");
            assert_eq!(manifest["js"], "/test_app.js");
            assert_eq!(manifest["wasm"], "/test_app_bg.wasm");

            let invocations = fs::read_to_string(root.join("build-args")).unwrap();
            let calls: Vec<_> = invocations.lines().collect();
            assert_eq!(calls.len(), 2);
            assert!(calls[0].contains(&rust_host().unwrap()));
            assert!(calls[0].contains("backend"));
            assert!(calls[0].contains("--no-default-features"));
            assert!(!calls[0].contains("frontend"));
            assert!(calls[1].contains("wasm32-unknown-unknown"));
            assert!(calls[1].contains("frontend"));
            assert!(!calls[1].contains("backend"));
            assert!(!calls[1].contains("--no-default-features"));
        }

        #[test]
        fn native_build_failure_keeps_previous_client_output_untouched() {
            let temp = fixture();
            let root = temp.path();
            let cargo = mock_application_cargo(root, true);
            let out = root.join("dist");
            fs::create_dir_all(out.join("public")).unwrap();
            fs::write(out.join("public/index.html"), "previous HTML").unwrap();
            fs::write(out.join("public/previous.txt"), "previous asset").unwrap();
            let options = Options {
                manifest_path: Some(root.join("Cargo.toml")),
                mode: crate::options::BuildMode::Ssr,
                server: Some(crate::options::ServerOptions {
                    package: None,
                    bin: None,
                    features: Vec::new(),
                    default_features: true,
                }),
                ..Default::default()
            };
            let error = build_application_with(&options, cargo.as_os_str(), |_, _| {
                panic!("client tools must not be ensured after native build failure")
            })
            .unwrap_err();
            assert!(format!("{error:#}").contains("native cargo build failed"));
            assert_eq!(
                fs::read_to_string(out.join("public/index.html")).unwrap(),
                "previous HTML"
            );
            assert_eq!(
                fs::read_to_string(out.join("public/previous.txt")).unwrap(),
                "previous asset"
            );
            assert_eq!(fs::read_dir(out.join("public")).unwrap().count(), 2);
        }

        #[test]
        fn release_optimizes_bindgen_output_for_applications_and_examples() {
            for example in [false, true] {
                let temp = fixture();
                let root = temp.path();
                if example {
                    fs::create_dir(root.join("examples")).unwrap();
                    fs::write(root.join("examples/counter.rs"), "fn main() {}\n").unwrap();
                }
                let name = if example { "counter" } else { "test_app" };
                let cargo = mock_cargo_target(root, false, example.then_some("counter"));
                fs::write(root.join("Cargo.lock"), locked("0.2.100")).unwrap();
                let out = root.join("custom-output");
                let staged_module = out.join(".staging/public").join(format!("{name}_bg.wasm"));
                let module = out.join("public").join(format!("{name}_bg.wasm"));
                let bindgen = root.join("wasm-bindgen");
                script(
                    &bindgen,
                    &format!("printf 'bindgen wasm' > {}", quote(&staged_module)),
                );
                let optimizer = root.join("wasm-opt");
                script(
                    &optimizer,
                    &format!(
                        "printf '%s\\n' \"$@\" > {}\n[ \"$#\" -eq 6 ]\n[ \"$2\" = -Oz ]\n[ \"$3\" = --all-features ]\n[ \"$4\" = --disable-compact-imports ]\n[ \"$5\" = -o ]\ncp \"$1\" \"$6\"\nprintf optimized >> \"$6\"",
                        quote(root.join("opt-args"))
                    ),
                );
                let options = Options {
                    manifest_path: Some(root.join("Cargo.toml")),
                    release: true,
                    example: example.then(|| "counter".into()),
                    out_dir: Some(out.clone()),
                    wasm_opt_version: "test-version".into(),
                    ..Default::default()
                };
                let mut tools = Vec::new();
                let directory = build_with(&options, cargo.as_os_str(), |tool, version| {
                    tools.push(format!("{tool:?}:{version}"));
                    match tool {
                        BuiltinTool::WasmBindgenCli => Ok(bindgen.clone()),
                        BuiltinTool::WasmOpt => {
                            assert!(!module.exists(), "download tools before running bindgen");
                            Ok(optimizer.clone())
                        }
                        _ => panic!("unexpected tool"),
                    }
                })
                .unwrap();
                assert_eq!(directory, out);
                assert_eq!(tools, ["WasmBindgenCli:0.2.100", "WasmOpt:test-version"]);
                assert_eq!(fs::read(&module).unwrap(), b"bindgen wasmoptimized");
                assert_eq!(
                    fs::read(root.join(format!("custom-target/{name}.wasm"))).unwrap(),
                    b"fake wasm"
                );
                assert!(
                    fs::read_to_string(root.join("opt-args"))
                        .unwrap()
                        .starts_with(&format!(
                            "{}\n-Oz\n--all-features\n--disable-compact-imports\n-o\n",
                            staged_module.display()
                        ))
                );
                assert!(out.join("public/index.html").exists());
                assert!(!out.join(".staging").exists());
                assert_eq!(
                    fs::read_dir(out.join("public")).unwrap().count(),
                    3,
                    "temporary optimizer output must not remain"
                );
            }
        }

        #[test]
        fn optimizer_failure_does_not_publish_output_or_generate_html() {
            for failure in ["exit", "empty", "download", "missing-executable"] {
                let temp = fixture();
                let root = temp.path();
                let cargo = mock_cargo(root, false);
                fs::write(root.join("Cargo.lock"), locked("0.2.100")).unwrap();
                let module = root.join("dist/public/test_app_bg.wasm");
                let staged_module = root.join("dist/.staging/public/test_app_bg.wasm");
                fs::create_dir_all(root.join("dist/public")).unwrap();
                fs::write(&module, b"previous output").unwrap();
                fs::write(root.join("dist/public/index.html"), "previous HTML").unwrap();
                let bindgen = root.join("wasm-bindgen");
                script(
                    &bindgen,
                    &format!("printf original > {}", quote(&staged_module)),
                );
                let optimizer = root.join("wasm-opt");
                if failure == "exit" {
                    script(&optimizer, "printf partial > \"$6\"\nexit 1");
                }
                if failure == "empty" {
                    script(&optimizer, "exit 0");
                }
                let options = Options {
                    manifest_path: Some(root.join("Cargo.toml")),
                    release: true,
                    ..Default::default()
                };
                let error = build_with(&options, cargo.as_os_str(), |tool, _| match tool {
                    BuiltinTool::WasmBindgenCli => Ok(bindgen.clone()),
                    BuiltinTool::WasmOpt if failure == "download" => bail!("download failed"),
                    BuiltinTool::WasmOpt => Ok(optimizer.clone()),
                    _ => panic!("unexpected tool"),
                })
                .unwrap_err();
                assert!(format!("{error:#}").contains("wasm-opt"), "{error:#}");
                assert_eq!(fs::read(&module).unwrap(), b"previous output");
                assert_eq!(
                    fs::read_to_string(root.join("dist/public/index.html")).unwrap(),
                    "previous HTML"
                );
                if failure == "download" {
                    assert!(!staged_module.exists());
                } else {
                    assert_eq!(fs::read(&staged_module).unwrap(), b"original");
                }
            }
        }

        #[test]
        fn builds_tailwind_once_in_debug_and_release_with_expected_paths() {
            for release in [false, true] {
                let temp = fixture();
                let root = temp.path();
                let cargo = mock_cargo(root, false);
                fs::write(root.join("Cargo.lock"), locked("0.2.100")).unwrap();
                let input = root.join("input.css");
                fs::write(&input, "@import 'tailwindcss';").unwrap();
                let bindgen = root.join("wasm-bindgen");
                script(&bindgen, "exit 0");
                let tailwind = root.join("tailwindcss");
                script(
                    &tailwind,
                    &format!(
                        "printf '%s\\n' \"$@\" > {}\npwd > {}\n[ \"$1\" = --input ]\n[ \"$3\" = --output ]\nprintf 'body {{ color: red }}' > \"$4\"",
                        quote(root.join("tailwind-args")),
                        quote(root.join("tailwind-cwd"))
                    ),
                );
                let out = root.join("custom-dist");
                let explicit_output = release.then(|| PathBuf::from("assets/nested/styles.css"));
                let output = out
                    .join("public")
                    .join(explicit_output.as_deref().unwrap_or(Path::new("index.css")));
                let options = Options {
                    manifest_path: Some(root.join("Cargo.toml")),
                    out_dir: Some(out.clone()),
                    release,
                    wasm_opt: false,
                    tailwindcss: Some(TailwindOptions {
                        input: input.clone(),
                        output: explicit_output,
                        version: "test-version".into(),
                    }),
                    ..Default::default()
                };
                let mut tools = Vec::new();
                let result = build_with(&options, cargo.as_os_str(), |tool, version| {
                    tools.push(format!("{tool:?}:{version}"));
                    match tool {
                        BuiltinTool::WasmBindgenCli => Ok(bindgen.clone()),
                        BuiltinTool::TailwindCss => Ok(tailwind.clone()),
                        _ => panic!("unexpected tool"),
                    }
                })
                .unwrap();
                assert_eq!(result, out);
                assert_eq!(
                    tools,
                    ["WasmBindgenCli:0.2.100", "TailwindCss:test-version"]
                );
                assert_eq!(fs::read_to_string(&output).unwrap(), "body { color: red }");
                let args = fs::read_to_string(root.join("tailwind-args")).unwrap();
                assert!(args.starts_with(&format!(
                    "--input\n{}\n--output\n",
                    fs::canonicalize(input).unwrap().display()
                )));
                assert_eq!(args.contains("--minify\n"), release);
                assert!(!args.contains("--watch"));
                assert_eq!(
                    Path::new(
                        fs::read_to_string(root.join("tailwind-cwd"))
                            .unwrap()
                            .trim()
                    ),
                    fs::canonicalize(root).unwrap()
                );
                let html = fs::read_to_string(out.join("public/index.html")).unwrap();
                assert!(html.contains("data-cargo-sycamore-tailwind"));
                assert!(html.contains(if release {
                    "href=\"/assets/nested/styles.css\""
                } else {
                    "href=\"/index.css\""
                }));
                assert!(!out.join(".staging").exists());
            }
        }

        #[test]
        fn tailwind_failures_preserve_css_and_prevent_html_generation() {
            for failure in [
                "download",
                "exit",
                "missing-executable",
                "missing-input",
                "overwrite-input",
            ] {
                let temp = fixture();
                let root = temp.path();
                let cargo = mock_cargo(root, false);
                fs::write(root.join("Cargo.lock"), locked("0.2.100")).unwrap();
                fs::create_dir_all(root.join("dist/public")).unwrap();
                let input = if failure == "overwrite-input" {
                    root.join("dist/public/input.css")
                } else {
                    root.join("input.css")
                };
                if failure != "missing-input" {
                    fs::write(&input, "source CSS").unwrap();
                }
                let output = if failure == "overwrite-input" {
                    input.clone()
                } else {
                    root.join("dist/public/styles.css")
                };
                if failure != "overwrite-input" {
                    fs::write(&output, "previous CSS").unwrap();
                }
                fs::write(root.join("dist/public/index.html"), "previous HTML").unwrap();
                let bindgen = root.join("wasm-bindgen");
                script(&bindgen, "exit 0");
                let tailwind = root.join("tailwindcss");
                if failure != "missing-executable" {
                    script(&tailwind, "printf partial > \"$4\"\nexit 1");
                }
                let options = Options {
                    manifest_path: Some(root.join("Cargo.toml")),
                    tailwindcss: Some(TailwindOptions {
                        input,
                        output: Some(
                            output
                                .strip_prefix(root.join("dist/public"))
                                .unwrap()
                                .to_owned(),
                        ),
                        version: "test-version".into(),
                    }),
                    ..Default::default()
                };
                let error = build_with(&options, cargo.as_os_str(), |tool, _| match tool {
                    BuiltinTool::WasmBindgenCli => Ok(bindgen.clone()),
                    BuiltinTool::TailwindCss if failure == "download" => bail!("download failed"),
                    BuiltinTool::TailwindCss => Ok(tailwind.clone()),
                    _ => panic!("unexpected tool"),
                })
                .unwrap_err();
                if failure == "overwrite-input" {
                    let message = format!("{error:#}");
                    assert!(message.contains("build output directory"), "{message}");
                    assert!(message.contains("input.css"), "{message}");
                } else {
                    assert!(format!("{error:#}").contains("Tailwind CSS"), "{error:#}");
                }
                assert_eq!(
                    fs::read_to_string(&output).unwrap(),
                    if failure == "overwrite-input" {
                        "source CSS"
                    } else {
                        "previous CSS"
                    }
                );
                assert_eq!(
                    fs::read_to_string(root.join("dist/public/index.html")).unwrap(),
                    "previous HTML"
                );
            }
        }

        #[test]
        fn builds_example_and_generates_its_entrypoint() {
            let temp = fixture();
            let root = temp.path();
            fs::create_dir(root.join("examples")).unwrap();
            fs::write(root.join("examples/counter.rs"), "fn main() {}\n").unwrap();
            let cargo = mock_cargo_target(root, false, Some("counter"));
            fs::write(root.join("Cargo.lock"), locked("0.2.100")).unwrap();
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
                example: Some("counter".into()),
                ..Default::default()
            };
            build_with(&options, cargo.as_os_str(), |tool, _| {
                assert!(matches!(tool, BuiltinTool::WasmBindgenCli));
                Ok(bindgen.clone())
            })
            .unwrap();
            let args = fs::read_to_string(root.join("cargo-args")).unwrap();
            assert!(args.contains("--example\ncounter\n"));
            assert!(!args.contains("--bin\n"));
            assert!(!args.contains("--lib\n"));
            let args = fs::read_to_string(root.join("bindgen-args")).unwrap();
            assert!(args.contains("custom-target/counter.wasm\n"));
            assert!(args.contains("--out-name\ncounter\n"));
            assert!(
                fs::read_to_string(root.join("dist/public/index.html"))
                    .unwrap()
                    .contains("/counter.js")
            );
            assert!(!root.join("dist/.staging").exists());
        }

        #[test]
        fn failed_cargo_does_not_download_tools() {
            let temp = fixture();
            let cargo = mock_cargo(temp.path(), true);
            let options = Options {
                manifest_path: Some(temp.path().join("Cargo.toml")),
                ..Default::default()
            };
            let error = build_with(&options, cargo.as_os_str(), |_, _| {
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
            let error =
                build_with(&options, cargo.as_os_str(), |_, _| Ok(bindgen.clone())).unwrap_err();
            assert!(error.to_string().contains("wasm-bindgen 0.2.100 failed"));
            assert!(root.join("output").is_dir());
        }
    }
}
