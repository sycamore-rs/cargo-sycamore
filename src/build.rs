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
use clap::Args;
use serde::Deserialize;

use crate::tools::{BuiltinTool, ToolCache};

const WASM_TARGET: &str = "wasm32-unknown-unknown";

#[derive(Debug, Default, Args)]
pub struct BuildOptions {
    /// Path to the application's Cargo.toml.
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,
    /// Workspace package to build (required if the selection is ambiguous).
    #[arg(short, long, value_name = "PACKAGE")]
    pub package: Option<String>,
    /// Binary target to build instead of the package's cdylib or sole binary.
    #[arg(long, value_name = "NAME")]
    pub bin: Option<String>,
    /// Build with Cargo's release profile.
    #[arg(long)]
    pub release: bool,
    /// Output directory for JS/WASM bindings (defaults to <package>/dist).
    #[arg(long, value_name = "PATH")]
    pub out_dir: Option<PathBuf>,
}

pub fn build(options: &BuildOptions) -> Result<()> {
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
    options: &BuildOptions,
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
    let status = Command::new(&bindgen)
        .arg(&wasm)
        .arg("--target")
        .arg("web")
        .arg("--out-dir")
        .arg(&out_dir)
        .status()
        .with_context(|| format!("running {}", bindgen.display()))?;
    ensure!(status.success(), "wasm-bindgen {version} failed ({status})");
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
mod tests;
