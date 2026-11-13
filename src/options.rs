//! Separate CLI/file inputs from the resolved options consumed by commands.

use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use clap::Args;
use serde::Deserialize;

/// Pinned Binaryen release used by default for reproducible optimization.
pub const DEFAULT_WASM_OPT_VERSION: &str = "133";
/// Pinned standalone Tailwind CSS CLI release.
pub const DEFAULT_TAILWINDCSS_VERSION: &str = "4.3.5";

/// CLI input only. Optional values retain whether the user supplied a flag.
#[derive(Debug, Default, Args)]
pub struct BuildArgs {
    /// Path to the application's Cargo.toml (also configurable in Sycamore.toml).
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,
    /// Workspace package to build (also configurable in Sycamore.toml).
    #[arg(short, long, value_name = "PACKAGE")]
    pub package: Option<String>,
    /// Binary target to build (also configurable in Sycamore.toml).
    #[arg(long, value_name = "NAME", conflicts_with = "example")]
    pub bin: Option<String>,
    /// Cargo example to build and run in the browser.
    #[arg(long, value_name = "NAME", conflicts_with = "bin")]
    pub example: Option<String>,
    /// Build with Cargo's release profile (CLI only).
    #[arg(long)]
    pub release: bool,
    /// Optimize release WASM (default: true; also [tools.wasm-opt].enable; debug builds skip it).
    #[arg(long, num_args = 0..=1, default_missing_value = "true", require_equals = true, action = clap::ArgAction::Set)]
    pub wasm_opt: Option<bool>,
    /// Build output root; browser files go under public/ (also configurable in Sycamore.toml).
    #[arg(long, value_name = "PATH")]
    pub out_dir: Option<PathBuf>,
    /// Explicit config file; otherwise discover Sycamore.toml or Cargo package metadata.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
}

/// Serve-specific flags, alongside all standard build/watch flags.
#[derive(Debug, Default, Args)]
pub struct ServeArgs {
    #[command(flatten)]
    pub build: BuildArgs,
    /// Local HTTP port (default: 8080; also configurable as [serve].port).
    #[arg(long, value_name = "PORT")]
    pub port: Option<u16>,
    /// Automatically reload browsers after successful builds (default: true).
    #[arg(long, num_args = 0..=1, default_missing_value = "true", require_equals = true, action = clap::ArgAction::Set)]
    pub autoreload: Option<bool>,
    /// Serve index.html for missing pages (default: false; also configurable as [serve].spa).
    #[arg(long, num_args = 0..=1, default_missing_value = "true", require_equals = true, action = clap::ArgAction::Set)]
    pub spa: Option<bool>,
}

/// Config-file/manifest input only. CLI-only settings cannot be deserialized.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub build: BuildConfig,
    pub serve: ServeConfig,
    pub tools: ToolsConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct ToolsConfig {
    pub tailwindcss: TailwindConfig,
    pub wasm_opt: WasmOptConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WasmOptConfig {
    /// Defaults to true; only release builds run the optimizer.
    pub enable: Option<bool>,
    /// Defaults to the pinned Binaryen release.
    pub version: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TailwindConfig {
    /// Explicit opt-in; other settings alone do not enable the compiler.
    pub enable: bool,
    pub input: Option<PathBuf>,
    pub output: Option<PathBuf>,
    pub version: Option<String>,
}

/// Resolved config-only Tailwind settings. None on Options means disabled.
#[derive(Debug)]
pub struct TailwindOptions {
    pub input: PathBuf,
    /// Relative to the public/ directory inside the build output; defaults to index.css.
    pub output: Option<PathBuf>,
    pub version: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct ServeConfig {
    pub port: Option<u16>,
    pub autoreload: Option<bool>,
    /// Fallback to index.html for missing pages.
    pub spa: Option<bool>,
    pub serve_frontend: Option<bool>,
    pub backend_port: Option<u16>,
    pub forward_routes: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BuildMode {
    #[default]
    Spa,
    Ssr,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct ClientBuildConfig {
    pub package: Option<String>,
    pub bin: Option<String>,
    pub example: Option<String>,
    pub features: Vec<String>,
    pub default_features: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct ServerBuildConfig {
    pub package: Option<String>,
    pub bin: Option<String>,
    pub features: Vec<String>,
    pub default_features: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct BuildConfig {
    pub mode: BuildMode,
    pub client: ClientBuildConfig,
    pub server: Option<ServerBuildConfig>,
    pub manifest_path: Option<PathBuf>,
    pub out_dir: Option<PathBuf>,
    /// Config-only static files and directories to copy into the output.
    pub assets: Vec<crate::assets::AssetConfig>,
}

impl TailwindOptions {
    pub(crate) fn output_path(&self, out_dir: &Path) -> Result<PathBuf> {
        let path = self.output.as_deref().unwrap_or(Path::new("index.css"));
        ensure!(
            path.components()
                .any(|part| matches!(part, std::path::Component::Normal(_)))
                && path.components().all(|part| matches!(
                    part,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )),
            "tools.tailwindcss.output must be a relative path without '..' components"
        );
        Ok(out_dir.join(path))
    }
}

/// Resolved server target settings.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub package: Option<String>,
    pub bin: Option<String>,
    pub features: Vec<String>,
    pub default_features: bool,
}

/// Final, merged user options. This is neither a clap parser nor a file schema:
/// commands do not need to know which source supplied a value.
#[derive(Debug)]
pub struct Options {
    /// Selected configuration source (Sycamore.toml or Cargo.toml), for watch mode.
    pub config_path: Option<PathBuf>,
    pub manifest_path: Option<PathBuf>,
    pub package: Option<String>,
    pub bin: Option<String>,
    pub example: Option<String>,
    pub out_dir: Option<PathBuf>,
    pub release: bool,
    pub assets: Vec<crate::assets::Asset>,
    pub features: Vec<String>,
    pub default_features: bool,
    pub wasm_opt: bool,
    pub wasm_opt_version: String,
    pub tailwindcss: Option<TailwindOptions>,
    pub port: u16,
    pub autoreload: bool,
    pub spa: bool,
    pub mode: BuildMode,
    pub server: Option<ServerOptions>,
    pub serve_frontend: bool,
    pub backend_port: Option<u16>,
    pub forward_routes: Vec<String>,
    pub spa_explicit: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            config_path: None,
            manifest_path: None,
            package: None,
            bin: None,
            example: None,
            out_dir: None,
            release: false,
            assets: Vec::new(),
            features: Vec::new(),
            default_features: true,
            wasm_opt: true,
            wasm_opt_version: DEFAULT_WASM_OPT_VERSION.into(),
            tailwindcss: None,
            port: 8080,
            autoreload: true,
            spa: false,
            mode: BuildMode::Spa,
            server: None,
            serve_frontend: true,
            backend_port: None,
            forward_routes: Vec::new(),
            spa_explicit: false,
        }
    }
}

impl Options {
    /// Validate options that only affect serving, not compilation.
    pub fn validate_serve(&self) -> Result<()> {
        ensure!(
            self.mode != BuildMode::Ssr || !self.serve_frontend,
            "SSR mode cannot serve the frontend"
        );
        if !self.serve_frontend || !self.forward_routes.is_empty() {
            ensure!(
                self.server.is_some(),
                "backend serving requires [build.server]"
            );
        }
        if self.server.is_some() {
            let backend_port = self
                .backend_port
                .context("configured [build.server] requires [serve].backend-port")?;
            ensure!(backend_port != 0, "[serve].backend-port must be nonzero");
            ensure!(
                backend_port != self.port,
                "[serve].backend-port must differ from [serve].port"
            );
        }
        if !self.serve_frontend {
            ensure!(
                !self.spa_explicit,
                "[serve].spa cannot be specified in backend-only mode"
            );
            ensure!(
                self.forward_routes.is_empty(),
                "[serve].forward-routes cannot be used in backend-only mode"
            );
        }
        for route in &self.forward_routes {
            validate_route_prefix(route)?;
        }
        Ok(())
    }

    /// Resolve flags over the nearest configuration, then apply defaults.
    /// An explicit --package starts discovery at that workspace package's root.
    /// CLI paths are relative to cwd; config paths are relative to the config file.
    pub fn resolve(args: &BuildArgs) -> Result<Self> {
        Self::resolve_in(
            args,
            &std::env::current_dir().context("reading current directory")?,
        )
    }

    pub fn resolve_serve(args: &ServeArgs) -> Result<Self> {
        let mut options = Self::resolve(&args.build)?;
        if let Some(port) = args.port {
            options.port = port;
        }
        if let Some(autoreload) = args.autoreload {
            options.autoreload = autoreload;
        }
        if let Some(spa) = args.spa {
            options.spa = spa;
            options.spa_explicit = true;
        }
        options.validate_serve()?;
        Ok(options)
    }

    fn resolve_in(args: &BuildArgs, cwd: &Path) -> Result<Self> {
        let selected = match &args.config {
            Some(path) => {
                let path = absolute(cwd, path);
                let config = read_config(&path)?;
                warn_duplicate_config(&path);
                Some((path, config))
            }
            None => {
                let start = if let Some(package) = &args.package {
                    package_directory(args, cwd, package)?
                } else {
                    args.manifest_path
                        .as_ref()
                        .map(|path| absolute(cwd, path))
                        .and_then(|path| path.parent().map(Path::to_path_buf))
                        .unwrap_or_else(|| cwd.to_owned())
                };
                discover(&start)?
            }
        };
        let (config_path, config) = match selected {
            Some((path, config)) => (Some(path), config),
            None => (None, Config::default()),
        };
        let base = config_path.as_deref().and_then(Path::parent).unwrap_or(cwd);
        let wasm_opt = config.tools.wasm_opt;
        let tailwind = config.tools.tailwindcss;
        if !tailwind.enable
            && (tailwind.input.is_some() || tailwind.output.is_some() || tailwind.version.is_some())
        {
            eprintln!(
                "warning: tools.tailwindcss options are set but enable = false; Tailwind CSS is disabled and these options are ignored. Set tools.tailwindcss.enable = true to enable it."
            );
        }
        let tailwindcss = if tailwind.enable {
            Some(TailwindOptions {
                input: absolute(
                    base,
                    tailwind.input.as_deref().unwrap_or(Path::new("index.css")),
                ),
                output: tailwind.output,
                version: tailwind
                    .version
                    .unwrap_or_else(|| DEFAULT_TAILWINDCSS_VERSION.into()),
            })
        } else {
            None
        };
        if let Some(tailwind) = &tailwindcss {
            tailwind.output_path(Path::new("."))?;
        }
        let file = config.build;
        ensure!(
            args.bin.is_none() || args.example.is_none(),
            "--bin and --example are mutually exclusive"
        );
        let client = file.client;
        // A CLI target selector overrides the configured target selector as a
        // group, so --example can replace a configured bin (and vice versa).
        let (bin, example) = if args.bin.is_some() || args.example.is_some() {
            (args.bin.clone(), args.example.clone())
        } else {
            (client.bin, client.example)
        };
        ensure!(
            bin.is_none() || example.is_none(),
            "configure either [build.client].bin or [build.client].example, not both"
        );
        let server = file.server.map(|server| ServerOptions {
            package: server.package,
            bin: server.bin,
            features: server.features,
            default_features: server.default_features.unwrap_or(true),
        });
        ensure!(
            file.mode != BuildMode::Ssr || server.is_some(),
            "[build].mode = 'ssr' requires [build.server]"
        );
        let mode = file.mode;
        let spa_explicit = config.serve.spa.is_some();
        Ok(Self {
            manifest_path: args
                .manifest_path
                .as_ref()
                .map(|p| absolute(cwd, p))
                .or_else(|| file.manifest_path.as_ref().map(|p| absolute(base, p))),
            package: args.package.clone().or(client.package),
            bin,
            example,
            out_dir: args
                .out_dir
                .as_ref()
                .map(|p| absolute(cwd, p))
                .or_else(|| file.out_dir.as_ref().map(|p| absolute(base, p))),
            release: args.release,
            assets: file
                .assets
                .into_iter()
                .map(|asset| asset.resolve(base))
                .collect::<Result<_>>()?,
            wasm_opt: args.wasm_opt.or(wasm_opt.enable).unwrap_or(true),
            wasm_opt_version: wasm_opt
                .version
                .unwrap_or_else(|| DEFAULT_WASM_OPT_VERSION.into()),
            features: client.features,
            default_features: client.default_features.unwrap_or(true),
            tailwindcss,
            config_path,
            port: config.serve.port.unwrap_or(8080),
            autoreload: config.serve.autoreload.unwrap_or(true),
            spa: config.serve.spa.unwrap_or(false),
            mode,
            server,
            serve_frontend: config
                .serve
                .serve_frontend
                .unwrap_or(mode == BuildMode::Spa),
            backend_port: config.serve.backend_port,
            forward_routes: config.serve.forward_routes,
            spa_explicit,
        })
    }
}

fn validate_route_prefix(route: &str) -> Result<()> {
    ensure!(
        route.starts_with('/'),
        "forward route prefix '{route}' must be an absolute path"
    );
    ensure!(
        !route.contains('?') && !route.contains('#'),
        "forward route prefix '{route}' cannot contain a query or fragment"
    );
    ensure!(
        !route
            .split('/')
            .any(|segment| segment == "." || segment == ".."),
        "forward route prefix '{route}' cannot contain traversal components"
    );
    ensure!(
        !route.split('/').any(|segment| segment == "__sycamore"),
        "forward route prefix '{route}' uses the reserved __sycamore path"
    );
    Ok(())
}

fn absolute(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    }
}

/// Discovery must use CLI inputs only: the configuration has not been selected
/// yet. In particular, a configured package must not recursively select a file.
fn package_directory(args: &BuildArgs, cwd: &Path, name: &str) -> Result<PathBuf> {
    let mut command = cargo_metadata::MetadataCommand::new();
    command.no_deps().current_dir(cwd);
    if let Some(cargo) = std::env::var_os("CARGO") {
        command.cargo_path(cargo);
    }
    if let Some(manifest) = &args.manifest_path {
        command.manifest_path(absolute(cwd, manifest));
    }
    let metadata = command
        .exec()
        .context("reading Cargo metadata for package configuration discovery")?;
    let package = metadata
        .workspace_packages()
        .into_iter()
        .find(|package| package.name == name)
        .with_context(|| format!("workspace has no package named '{name}'"))?;
    Ok(package
        .manifest_path
        .parent()
        .context("package manifest has no parent directory")?
        .to_path_buf()
        .into_std_path_buf())
}

fn read_config(path: &Path) -> Result<Config> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("reading configuration {}", path.display()))?;
    toml::from_str(&contents).with_context(|| format!("parsing configuration {}", path.display()))
}

fn manifest_metadata(path: &Path) -> Result<Option<toml::Value>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("reading manifest {}", path.display()));
        }
    };
    let manifest: toml::Value = toml::from_str(&contents)
        .with_context(|| format!("parsing manifest {}", path.display()))?;
    Ok(manifest
        .get("package")
        .and_then(|package| package.get("metadata"))
        .and_then(|metadata| metadata.get("sycamore"))
        .cloned())
}

fn manifest_config(path: &Path) -> Result<Option<Config>> {
    manifest_metadata(path)?
        .map(|value| {
            value.try_into().with_context(|| {
                format!("parsing [package.metadata.sycamore] in {}", path.display())
            })
        })
        .transpose()
}

fn warn_duplicate_config(path: &Path) {
    let Some(directory) = path.parent() else {
        return;
    };
    let manifest = directory.join("Cargo.toml");
    // This source is ignored, so even malformed metadata must not invalidate
    // the selected standalone file. Read only enough to detect its presence.
    if matches!(manifest_metadata(&manifest), Ok(Some(_))) {
        eprintln!(
            "warning: found both {} and [package.metadata.sycamore] in {}; using {} and ignoring Cargo metadata (configuration sources are not merged)",
            path.display(),
            manifest.display(),
            path.display()
        );
    }
}

fn discover(start: &Path) -> Result<Option<(PathBuf, Config)>> {
    for directory in start.ancestors() {
        let path = directory.join("Sycamore.toml");
        match fs::metadata(&path) {
            Ok(_) => {
                let config = read_config(&path)?;
                warn_duplicate_config(&path);
                return Ok(Some((path, config)));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("checking configuration {}", path.display()));
            }
        }
        let manifest = directory.join("Cargo.toml");
        if let Some(config) = manifest_config(&manifest)? {
            return Ok(Some((manifest, config)));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_port_defaults_config_and_cli_precedence() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Sycamore.toml");
        fs::write(&path, "").unwrap();
        let mut args = ServeArgs {
            build: BuildArgs {
                config: Some(path.clone()),
                ..Default::default()
            },
            port: None,
            autoreload: None,
            spa: None,
        };
        assert_eq!(Options::resolve_serve(&args).unwrap().port, 8080);
        fs::write(&path, "[serve]\nport = 9000\n").unwrap();
        assert_eq!(Options::resolve_serve(&args).unwrap().port, 9000);
        args.port = Some(9090);
        assert_eq!(Options::resolve_serve(&args).unwrap().port, 9090);
        for config in [
            "[serve]\nport = 65536",
            "[serve]\nport = -1",
            "[serve]\nport = 'bad'",
            "[serve]\nunknown = true",
        ] {
            assert!(toml::from_str::<Config>(config).is_err());
        }
    }

    #[test]
    fn autoreload_defaults_and_cli_overrides_both_config_values() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Sycamore.toml");
        fs::write(&path, "").unwrap();
        let mut args = ServeArgs {
            build: BuildArgs {
                config: Some(path.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(Options::resolve_serve(&args).unwrap().autoreload);
        fs::write(&path, "[serve]\nautoreload = false\n").unwrap();
        assert!(!Options::resolve_serve(&args).unwrap().autoreload);
        args.autoreload = Some(true);
        assert!(Options::resolve_serve(&args).unwrap().autoreload);
        fs::write(&path, "[serve]\nautoreload = true\n").unwrap();
        args.autoreload = Some(false);
        assert!(!Options::resolve_serve(&args).unwrap().autoreload);
        assert!(toml::from_str::<Config>("[serve]\nautoreload = 'false'").is_err());
    }

    #[test]
    fn example_selection_merges_with_cli_target_precedence() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Sycamore.toml");
        let mut args = BuildArgs {
            config: Some(path.clone()),
            ..Default::default()
        };
        fs::write(&path, "[build.client]\nexample = 'counter'\n").unwrap();
        let options = Options::resolve(&args).unwrap();
        assert_eq!(options.example.as_deref(), Some("counter"));
        assert!(options.bin.is_none());
        args.example = Some("other".into());
        assert_eq!(
            Options::resolve(&args).unwrap().example.as_deref(),
            Some("other")
        );
        args.example = None;
        args.bin = Some("client".into());
        let options = Options::resolve(&args).unwrap();
        assert_eq!(options.bin.as_deref(), Some("client"));
        assert!(options.example.is_none());
        fs::write(&path, "[build.client]\nbin = 'client'\n").unwrap();
        args.bin = None;
        args.example = Some("counter".into());
        let options = Options::resolve(&args).unwrap();
        assert_eq!(options.example.as_deref(), Some("counter"));
        assert!(options.bin.is_none());
        args.bin = Some("client".into());
        assert!(Options::resolve(&args).is_err());
        args.bin = None;
        args.example = None;
        fs::write(
            &path,
            "[build.client]\nbin = 'client'\nexample = 'counter'\n",
        )
        .unwrap();
        assert!(Options::resolve(&args).is_err());
    }

    #[test]
    fn spa_is_disabled_by_default_and_configurable_under_serve() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Sycamore.toml");
        fs::write(&path, "").unwrap();
        let args = ServeArgs {
            build: BuildArgs {
                config: Some(path.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!Options::resolve_serve(&args).unwrap().spa);
        fs::write(&path, "[serve]\nspa = true\n").unwrap();
        assert!(Options::resolve_serve(&args).unwrap().spa);
        fs::write(&path, "[serve]\nspa = false\n").unwrap();
        assert!(!Options::resolve_serve(&args).unwrap().spa);
        let mut args = args;
        args.spa = Some(true);
        assert!(Options::resolve_serve(&args).unwrap().spa);
        fs::write(&path, "[serve]\nspa = true\n").unwrap();
        args.spa = Some(false);
        assert!(!Options::resolve_serve(&args).unwrap().spa);
        assert!(toml::from_str::<Config>("[serve]\nspa = 'true'").is_err());
    }

    #[test]
    fn tailwind_is_opt_in_and_resolves_defaults_and_config_relative_paths() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("config");
        fs::create_dir(&base).unwrap();
        let path = base.join("Sycamore.toml");
        let args = BuildArgs {
            config: Some(path.clone()),
            ..Default::default()
        };
        for source in [
            "",
            "[tools]",
            "[tools.tailwindcss]",
            "[tools.tailwindcss]\ninput = 'styles.css'",
            "[tools.tailwindcss]\noutput = 'dist/styles.css'",
            "[tools.tailwindcss]\nversion = '4.0.0'",
            "[tools.tailwindcss]\nenable = false\ninput = 'styles.css'\noutput = 'dist/styles.css'\nversion = '4.0.0'",
        ] {
            fs::write(&path, source).unwrap();
            assert!(
                Options::resolve_in(&args, temp.path())
                    .unwrap()
                    .tailwindcss
                    .is_none()
            );
        }
        for setting in [
            "",
            "input = 'styles.css'",
            "output = 'public/app.css'",
            "version = '4.0.0'",
        ] {
            fs::write(
                &path,
                format!("[tools.tailwindcss]\nenable = true\n{setting}\n"),
            )
            .unwrap();
            let tailwind = Options::resolve_in(&args, temp.path())
                .unwrap()
                .tailwindcss
                .unwrap();
            assert_eq!(
                tailwind.input,
                base.join(if setting.starts_with("input") {
                    "styles.css"
                } else {
                    "index.css"
                })
            );
            assert_eq!(
                tailwind.output,
                setting
                    .starts_with("output")
                    .then(|| PathBuf::from("public/app.css"))
            );
            assert_eq!(
                tailwind.version,
                if setting.starts_with("version") {
                    "4.0.0"
                } else {
                    DEFAULT_TAILWINDCSS_VERSION
                }
            );
        }
        for invalid in [
            "[tools.tailwindcss]\nenable = 'true'",
            "[tools.tailwindcss]\nenable = 1",
            "[tools.tailwindcss]\ninput = 123",
            "[tools.tailwindcss]\nversion = 4",
            "[tools.tailwindcss]\nminfy = true",
            "[tools.unknown]\ninput = 'file'",
        ] {
            assert!(toml::from_str::<Config>(invalid).is_err());
        }
        let absolute = temp.path().join("absolute.css");
        let config = format!(
            "[tools.tailwindcss]\nenable = true\ninput = {}\noutput = 'site.css'\n",
            toml::Value::String(absolute.to_string_lossy().into_owned())
        );
        fs::write(&path, config).unwrap();
        let tailwind = Options::resolve_in(&args, temp.path())
            .unwrap()
            .tailwindcss
            .unwrap();
        assert_eq!(tailwind.input, absolute);
        assert_eq!(tailwind.output, Some(PathBuf::from("site.css")));
        for output in [absolute, PathBuf::from("../outside.css")] {
            let config = format!(
                "[tools.tailwindcss]\nenable = true\noutput = {}\n",
                toml::Value::String(output.to_string_lossy().into_owned())
            );
            fs::write(&path, config).unwrap();
            assert!(Options::resolve_in(&args, temp.path()).is_err());
        }
    }

    #[test]
    fn tailwind_configuration_is_also_supported_in_cargo_metadata() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("Cargo.toml"), "[package]\nname = 'app'\nversion = '0.1.0'\n[package.metadata.sycamore.tools.tailwindcss]\nenable = true\ninput = 'styles.css'\noutput = 'site.css'\nversion = '4.0.0'\n").unwrap();
        let options = Options::resolve_in(&BuildArgs::default(), temp.path()).unwrap();
        let tailwind = options.tailwindcss.unwrap();
        assert_eq!(tailwind.input, temp.path().join("styles.css"));
        assert_eq!(tailwind.output, Some(PathBuf::from("site.css")));
        assert_eq!(tailwind.version, "4.0.0");
    }

    #[test]
    fn wasm_opt_defaults_and_cli_overrides_config() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Sycamore.toml");
        fs::write(&path, "").unwrap();
        let mut args = BuildArgs {
            config: Some(path.clone()),
            ..Default::default()
        };
        let options = Options::resolve_in(&args, temp.path()).unwrap();
        assert!(options.wasm_opt);
        assert_eq!(options.wasm_opt_version, DEFAULT_WASM_OPT_VERSION);
        assert!(!options.release);
        for source in ["[tools.wasm-opt]", "[tools.wasm-opt]\nversion = '123'"] {
            fs::write(&path, source).unwrap();
            let options = Options::resolve_in(&args, temp.path()).unwrap();
            assert!(options.wasm_opt);
            assert_eq!(
                options.wasm_opt_version,
                if source.contains("version") {
                    "123"
                } else {
                    DEFAULT_WASM_OPT_VERSION
                }
            );
        }
        for configured in [false, true] {
            fs::write(
                &path,
                format!("[tools.wasm-opt]\nenable = {configured}\nversion = '123'\n"),
            )
            .unwrap();
            args.wasm_opt = None;
            let options = Options::resolve_in(&args, temp.path()).unwrap();
            assert_eq!(options.wasm_opt, configured);
            assert_eq!(options.wasm_opt_version, "123");
            args.wasm_opt = Some(!configured);
            assert_eq!(
                Options::resolve_in(&args, temp.path()).unwrap().wasm_opt,
                !configured
            );
        }
        for invalid in [
            "[tools.wasm-opt]\nenable = 'false'",
            "[tools.wasm-opt]\nversion = 133",
            "[tools.wasm-opt]\nenabled = true",
            "[build]\nwasm-opt = false",
            "[build]\nwasm-opt-version = '133'",
        ] {
            assert!(toml::from_str::<Config>(invalid).is_err());
        }
    }

    #[test]
    fn defaults_work_without_a_config_file() {
        let temp = tempfile::tempdir().unwrap();
        let options = Options::resolve_in(&BuildArgs::default(), temp.path()).unwrap();
        assert!(options.default_features);
        assert!(options.features.is_empty());
        assert!(!options.release);
        assert!(options.out_dir.is_none());
        assert!(options.manifest_path.is_none());
    }

    #[test]
    fn merges_cli_over_config_and_resolves_paths_by_source() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("config");
        fs::create_dir(&config_dir).unwrap();
        fs::write(
            config_dir.join("Sycamore.toml"),
            r#"
    [build]
    manifest-path = "app/Cargo.toml"
    out-dir = "output"
    assets = ["images", { from = "public", to = "." }, { from = "logo.svg", to = "img/brand.svg" }]
    [build.client]
    package = "configured-package"
    bin = "configured-bin"
    features = ["csr", "extra"]
    default-features = false
    "#,
        )
        .unwrap();
        let args = BuildArgs {
            config: Some(PathBuf::from("config/Sycamore.toml")),
            package: Some("cli-package".into()),
            release: true,
            ..Default::default()
        };
        let options = Options::resolve_in(&args, temp.path()).unwrap();
        assert_eq!(
            options.manifest_path,
            Some(config_dir.join("app/Cargo.toml"))
        );
        assert_eq!(options.out_dir, Some(config_dir.join("output")));
        assert_eq!(options.package.as_deref(), Some("cli-package"));
        assert_eq!(options.bin.as_deref(), Some("configured-bin"));
        assert_eq!(options.features, ["csr", "extra"]);
        assert_eq!(options.assets[0].from, config_dir.join("images"));
        assert_eq!(options.assets[1].from, config_dir.join("public"));
        assert_eq!(options.assets[1].to.as_deref(), Some(Path::new(".")));
        assert_eq!(
            options.assets[2].to.as_deref(),
            Some(Path::new("img/brand.svg"))
        );
        assert!(!options.default_features);
        assert!(options.release);

        let args = BuildArgs {
            manifest_path: Some(PathBuf::from("cli/Cargo.toml")),
            out_dir: Some(PathBuf::from("cli-output")),
            bin: Some("cli-bin".into()),
            ..args
        };
        let options = Options::resolve_in(&args, temp.path()).unwrap();
        assert_eq!(
            options.manifest_path,
            Some(temp.path().join("cli/Cargo.toml"))
        );
        assert_eq!(options.out_dir, Some(temp.path().join("cli-output")));
        assert_eq!(options.bin.as_deref(), Some("cli-bin"));
    }

    #[test]
    fn discovers_nearest_config_from_cwd_or_manifest_directory() {
        let temp = tempfile::tempdir().unwrap();
        let member = temp.path().join("member");
        fs::create_dir_all(member.join("src")).unwrap();
        fs::write(
            temp.path().join("Sycamore.toml"),
            "[build.client]\npackage = 'root'\n",
        )
        .unwrap();
        fs::write(
            member.join("Sycamore.toml"),
            "[build.client]\npackage = 'member'\n",
        )
        .unwrap();
        let options = Options::resolve_in(&BuildArgs::default(), &member.join("src")).unwrap();
        assert_eq!(options.package.as_deref(), Some("member"));
        let args = BuildArgs {
            manifest_path: Some(member.join("Cargo.toml")),
            ..Default::default()
        };
        let options = Options::resolve_in(&args, temp.path()).unwrap();
        assert_eq!(options.package.as_deref(), Some("member"));
        fs::remove_file(member.join("Sycamore.toml")).unwrap();
        let options = Options::resolve_in(&args, temp.path()).unwrap();
        assert_eq!(options.package.as_deref(), Some("root"));
    }

    #[test]
    fn reads_manifest_metadata_and_resolves_paths_from_the_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let cwd = root.join("src");
        fs::create_dir(&cwd).unwrap();
        let manifest = root.join("Cargo.toml");
        fs::write(
            &manifest,
            r#"
[package]
name = "app"
version = "0.1.0"
[package.metadata.unrelated]
anything = true
[package.metadata.sycamore.build]
out-dir = "web-dist"
[package.metadata.sycamore.build.client]
bin = "app"
features = ["csr"]
default-features = false
[package.metadata.sycamore.tools.wasm-opt]
enable = false
version = "133"
[package.metadata.sycamore.serve]
port = 9001
autoreload = false
spa = true
"#,
        )
        .unwrap();
        let options = Options::resolve_in(&BuildArgs::default(), &cwd).unwrap();
        assert_eq!(options.config_path, Some(manifest.clone()));
        assert_eq!(options.out_dir, Some(root.join("web-dist")));
        assert_eq!(options.bin.as_deref(), Some("app"));
        assert_eq!(options.features, ["csr"]);
        assert!(!options.default_features);
        assert!(!options.wasm_opt);
        assert_eq!(options.wasm_opt_version, "133");
        assert_eq!(options.port, 9001);
        assert!(!options.autoreload);
        assert!(options.spa);
        let args = BuildArgs {
            manifest_path: Some(manifest),
            out_dir: Some("cli-dist".into()),
            wasm_opt: Some(true),
            example: Some("counter".into()),
            ..Default::default()
        };
        let options = Options::resolve_in(&args, &cwd).unwrap();
        assert_eq!(options.out_dir, Some(cwd.join("cli-dist")));
        assert!(options.wasm_opt);
        assert!(options.bin.is_none());
        assert_eq!(options.example.as_deref(), Some("counter"));
    }

    #[test]
    fn nearest_package_metadata_wins_over_ancestor_config_but_not_local_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        package_workspace(root);
        fs::write(
            root.join("Sycamore.toml"),
            "[build.client]\nfeatures = ['root']\n[serve]\nspa = true\n",
        )
        .unwrap();
        let member = root.join("apps/frontend");
        let manifest = member.join("Cargo.toml");
        let original = fs::read_to_string(&manifest).unwrap();
        fs::write(
            &manifest,
            format!("{original}\n[package.metadata.sycamore.build]\nout-dir = 'metadata-dist'\n"),
        )
        .unwrap();
        let args = BuildArgs {
            package: Some("frontend".into()),
            ..Default::default()
        };
        let options = Options::resolve_in(&args, root).unwrap();
        assert_eq!(options.config_path, Some(manifest.clone()));
        assert_eq!(options.out_dir, Some(member.join("metadata-dist")));
        assert!(options.features.is_empty(), "sources are not merged");
        assert!(!options.spa);
        let file = member.join("Sycamore.toml");
        fs::write(&file, "[serve]\nport = 9002\n").unwrap();
        let options = Options::resolve_in(&args, root).unwrap();
        assert_eq!(options.config_path, Some(file.clone()));
        assert!(
            options.out_dir.is_none(),
            "local file replaces metadata completely"
        );
        assert_eq!(options.port, 9002);
        fs::remove_file(file).unwrap();
        fs::write(manifest, original).unwrap();
        assert_eq!(
            Options::resolve_in(&args, root).unwrap().config_path,
            Some(root.join("Sycamore.toml"))
        );
    }

    #[test]
    fn malformed_metadata_is_an_error_unless_discovery_is_overridden() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let manifest = root.join("Cargo.toml");
        for metadata in [
            "unexpected = true",
            "build = false",
            "[package.metadata.sycamore.tools.wasm-opt]\nenable = 'false'",
            "[package.metadata.sycamore.tools.wasm-opt]\nversion = 133",
        ] {
            fs::write(&manifest, format!("[package]\nname = 'app'\nversion = '0.1.0'\n[package.metadata.sycamore]\n{metadata}\n")).unwrap();
            let error = Options::resolve_in(&BuildArgs::default(), root).unwrap_err();
            let error = format!("{error:#}");
            assert!(error.contains("package.metadata.sycamore"), "{error}");
            assert!(error.contains("Cargo.toml"), "{error}");
        }
        // The same-directory file wins, even if ignored metadata is invalid.
        fs::write(root.join("Sycamore.toml"), "[serve]\nspa = true\n").unwrap();
        assert!(
            Options::resolve_in(&BuildArgs::default(), root)
                .unwrap()
                .spa
        );
        fs::write(&manifest, "invalid [manifest").unwrap();
        let args = BuildArgs {
            config: Some("Sycamore.toml".into()),
            ..Default::default()
        };
        assert!(Options::resolve_in(&args, root).unwrap().spa);
    }

    #[test]
    fn build_modes_and_targets_resolve() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Sycamore.toml");
        let args = BuildArgs {
            config: Some(path.clone()),
            ..Default::default()
        };
        fs::write(&path, "").unwrap();
        let options = Options::resolve_in(&args, temp.path()).unwrap();
        assert_eq!(options.mode, BuildMode::Spa);
        assert!(options.serve_frontend);
        assert!(options.server.is_none());

        fs::write(
            &path,
            r#"
[build]
mode = "ssr"
[build.client]
package = "web"
example = "demo"
features = ["hydrate"]
default-features = false
[build.server]
package = "backend"
bin = "http"
features = ["server"]
default-features = false
"#,
        )
        .unwrap();
        let options = Options::resolve_in(&args, temp.path()).unwrap();
        assert_eq!(options.mode, BuildMode::Ssr);
        assert_eq!(options.package.as_deref(), Some("web"));
        assert_eq!(options.example.as_deref(), Some("demo"));
        assert_eq!(options.features, ["hydrate"]);
        assert!(!options.default_features);
        assert!(!options.serve_frontend);
        let server = options.server.unwrap();
        assert_eq!(server.package.as_deref(), Some("backend"));
        assert_eq!(server.bin.as_deref(), Some("http"));
        assert_eq!(server.features, ["server"]);
        assert!(!server.default_features);

        let args = BuildArgs {
            example: Some("cli-demo".into()),
            ..args
        };
        let options = Options::resolve_in(&args, temp.path()).unwrap();
        assert_eq!(options.example.as_deref(), Some("cli-demo"));
        assert!(options.bin.is_none());

        for source in ["[build]\nmode = 'ssr'", "[build]\nmode = 'unknown'"] {
            fs::write(&path, source).unwrap();
            assert!(
                Options::resolve_in(
                    &BuildArgs {
                        config: Some(path.clone()),
                        ..Default::default()
                    },
                    temp.path()
                )
                .is_err(),
                "{source}"
            );
        }
        fs::write(
            &path,
            "[build.client]\nfeatures = ['client']\ndefault-features = false\n",
        )
        .unwrap();
        let options = Options::resolve_in(&args, temp.path()).unwrap();
        assert_eq!(options.features, ["client"]);
        assert!(!options.default_features);
    }

    #[test]
    fn serve_validation_is_separate_from_build_resolution() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Sycamore.toml");
        let args = ServeArgs {
            build: BuildArgs {
                config: Some(path.clone()),
                ..Default::default()
            },
            ..Default::default()
        };

        fs::write(&path, "[build]\nmode = 'ssr'\n[build.server]\npackage = 'backend'\n[serve]\nbackend-port = 9000\n").unwrap();
        let build = Options::resolve_in(&args.build, temp.path()).unwrap();
        assert_eq!(build.mode, BuildMode::Ssr);
        assert!(!build.serve_frontend);
        assert!(!Options::resolve_serve(&args).unwrap().serve_frontend);
        fs::write(&path, "[build]\nmode = 'ssr'\n[build.server]\npackage = 'backend'\n[serve]\nbackend-port = 9000\nserve-frontend = true\n").unwrap();
        assert!(
            Options::resolve_serve(&args).is_err(),
            "SSR cannot serve the frontend"
        );

        fs::write(&path, "[build]\nmode = 'spa'\n[build.server]\npackage = 'backend'\n[serve]\nserve-frontend = false\nbackend-port = 9000\n").unwrap();
        assert!(!Options::resolve_serve(&args).unwrap().serve_frontend);
        for serve in [
            "serve-frontend = false",
            "serve-frontend = false\nbackend-port = 0",
            "serve-frontend = false\nbackend-port = 8080",
            "serve-frontend = false\nbackend-port = 9000\nspa = false",
            "serve-frontend = false\nbackend-port = 9000\nforward-routes = ['/api']",
        ] {
            fs::write(
                &path,
                format!("[build.server]\npackage = 'backend'\n[serve]\n{serve}\n"),
            )
            .unwrap();
            assert!(Options::resolve_serve(&args).is_err(), "{serve}");
        }
        for route in [
            "api",
            "/api?x=1",
            "/api#fragment",
            "/api/../admin",
            "/__sycamore",
            "/x/__sycamore/y",
        ] {
            fs::write(&path, format!("[build.server]\npackage = 'backend'\n[serve]\nbackend-port = 9000\nforward-routes = ['{route}']\n")).unwrap();
            assert!(Options::resolve_serve(&args).is_err(), "{route}");
        }
        fs::write(&path, "[build.server]\npackage = 'backend'\n[serve]\nbackend-port = 9000\nforward-routes = ['/api', '/v1']\n").unwrap();
        let options = Options::resolve_serve(&args).unwrap();
        assert_eq!(options.forward_routes, ["/api", "/v1"]);
        assert!(options.serve_frontend);
        assert!(!options.spa);
        assert!(!options.spa_explicit);
    }

    #[test]
    fn configured_server_requires_a_distinct_explicit_serve_port_only_for_serve() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Sycamore.toml");
        let build_args = BuildArgs {
            config: Some(path.clone()),
            ..Default::default()
        };
        let serve_args = ServeArgs {
            build: BuildArgs {
                config: Some(path.clone()),
                ..Default::default()
            },
            ..Default::default()
        };

        fs::write(&path, "[build.server]\npackage = 'backend'\n").unwrap();
        assert!(Options::resolve_in(&build_args, temp.path()).is_ok());
        assert!(Options::resolve_serve(&serve_args).is_err());

        for backend_port in [0, 8080] {
            fs::write(
                &path,
                format!(
                    "[build.server]\npackage = 'backend'\n[serve]\nbackend-port = {backend_port}\n"
                ),
            )
            .unwrap();
            assert!(Options::resolve_in(&build_args, temp.path()).is_ok());
            assert!(Options::resolve_serve(&serve_args).is_err());
        }

        fs::write(
            &path,
            "[build.server]\npackage = 'backend'\n[serve]\nbackend-port = 9000\n",
        )
        .unwrap();
        assert!(Options::resolve_in(&build_args, temp.path()).is_ok());
        let serving = Options::resolve_serve(&serve_args).unwrap();
        assert!(serving.serve_frontend);
        assert!(serving.forward_routes.is_empty());
        assert_eq!(serving.backend_port, Some(9000));
    }

    #[test]
    fn build_and_serve_settings_deserialize_from_cargo_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("Cargo.toml");
        fs::write(
            &manifest,
            r#"
[package]
name = "app"
version = "0.1.0"
[package.metadata.sycamore.build]
mode = "ssr"
[package.metadata.sycamore.build.server]
package = "backend"
features = ["http"]
[package.metadata.sycamore.serve]
backend-port = 9000
"#,
        )
        .unwrap();
        let options = Options::resolve_in(&BuildArgs::default(), temp.path()).unwrap();
        assert_eq!(options.mode, BuildMode::Ssr);
        assert_eq!(options.server.unwrap().features, ["http"]);
        assert_eq!(options.backend_port, Some(9000));
        assert!(!options.serve_frontend);
    }

    fn package_workspace(root: &Path) {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = ['apps/frontend', 'apps/other']\nresolver = '3'\n",
        )
        .unwrap();
        for name in ["frontend", "other"] {
            let package = root.join("apps").join(name);
            fs::create_dir_all(package.join("src")).unwrap();
            fs::write(
                package.join("Cargo.toml"),
                format!("[package]\nname = '{name}'\nversion = '0.1.0'\nedition = '2024'\n"),
            )
            .unwrap();
            fs::write(package.join("src/lib.rs"), "").unwrap();
        }
    }

    #[test]
    fn explicit_package_prefers_its_config_without_merging_ancestors() {
        let temp = tempfile::tempdir().unwrap();
        package_workspace(temp.path());
        let member = temp.path().join("apps/frontend");
        fs::write(temp.path().join("Sycamore.toml"), "[build.client]\npackage = 'other'\nfeatures = ['root']\ndefault-features = false\n[serve]\nspa = true\nport = 9090\n").unwrap();
        fs::write(
            member.join("Sycamore.toml"),
            "[build]\nout-dir = 'local-dist'\n",
        )
        .unwrap();
        let args = BuildArgs {
            package: Some("frontend".into()),
            ..Default::default()
        };
        for cwd in [temp.path().to_owned(), temp.path().join("apps/other/src")] {
            let options = Options::resolve_in(&args, &cwd).unwrap();
            assert_eq!(options.config_path, Some(member.join("Sycamore.toml")));
            assert_eq!(options.out_dir, Some(member.join("local-dist")));
            assert_eq!(options.package.as_deref(), Some("frontend"));
            assert!(options.features.is_empty());
            assert!(options.default_features);
            assert!(!options.spa);
            assert_eq!(options.port, 8080);
        }
        let args = BuildArgs {
            out_dir: Some("cli-dist".into()),
            ..args
        };
        assert_eq!(
            Options::resolve_in(&args, temp.path()).unwrap().out_dir,
            Some(temp.path().join("cli-dist"))
        );
        fs::remove_file(member.join("Sycamore.toml")).unwrap();
        let options = Options::resolve_in(&args, temp.path()).unwrap();
        assert_eq!(options.config_path, Some(temp.path().join("Sycamore.toml")));
        assert_eq!(options.package.as_deref(), Some("frontend"));
        assert_eq!(options.features, ["root"]);
        assert!(options.spa);
    }

    #[test]
    fn package_discovery_uses_cli_manifest_from_outside_the_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let cwd = temp.path().join("outside");
        fs::create_dir(&cwd).unwrap();
        package_workspace(&root);
        let member = root.join("apps/frontend");
        fs::write(member.join("Sycamore.toml"), "[serve]\nspa = true\n").unwrap();
        let args = BuildArgs {
            manifest_path: Some("../workspace/Cargo.toml".into()),
            package: Some("frontend".into()),
            ..Default::default()
        };
        let options = Options::resolve_in(&args, &cwd).unwrap();
        assert_eq!(options.config_path, Some(member.join("Sycamore.toml")));
        assert!(options.spa);
        assert_eq!(
            options.manifest_path,
            Some(cwd.join("../workspace/Cargo.toml"))
        );
    }

    #[test]
    fn package_discovery_reports_errors_but_explicit_config_bypasses_metadata() {
        let temp = tempfile::tempdir().unwrap();
        package_workspace(temp.path());
        let member = temp.path().join("apps/frontend");
        fs::write(temp.path().join("Sycamore.toml"), "[serve]\nspa = true\n").unwrap();
        fs::write(member.join("Sycamore.toml"), "invalid [toml").unwrap();
        let mut args = BuildArgs {
            package: Some("frontend".into()),
            ..Default::default()
        };
        assert!(
            format!("{:#}", Options::resolve_in(&args, temp.path()).unwrap_err())
                .contains("parsing configuration")
        );
        args.package = Some("missing".into());
        assert!(
            format!("{:#}", Options::resolve_in(&args, temp.path()).unwrap_err())
                .contains("workspace has no package named 'missing'")
        );
        args.manifest_path = Some("missing/Cargo.toml".into());
        assert!(Options::resolve_in(&args, temp.path()).is_err());
        args.config = Some("Sycamore.toml".into());
        let options = Options::resolve_in(&args, temp.path()).unwrap();
        assert!(options.spa);
        assert_eq!(options.package.as_deref(), Some("missing"));
    }

    #[test]
    fn explicit_config_overrides_discovery_and_missing_explicit_file_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("Sycamore.toml"),
            "[build.client]\npackage = 'discovered'\n",
        )
        .unwrap();
        fs::write(
            temp.path().join("custom.toml"),
            "[build.client]\npackage = 'explicit'\n",
        )
        .unwrap();
        let args = BuildArgs {
            config: Some("custom.toml".into()),
            ..Default::default()
        };
        assert_eq!(
            Options::resolve_in(&args, temp.path())
                .unwrap()
                .package
                .as_deref(),
            Some("explicit")
        );
        let args = BuildArgs {
            config: Some("missing.toml".into()),
            ..Default::default()
        };
        assert!(Options::resolve_in(&args, temp.path()).is_err());
    }

    #[test]
    fn rejects_cli_only_keys_typos_and_invalid_config() {
        for contents in [
            "[build]\nrelease = true",
            "[build]\nconfig = 'other.toml'",
            "[build]\nout_dir = 'typo'",
            "[build.client]\ndefault-features = 'not a boolean'",
            "[unknown]\nvalue = true",
            "broken [toml",
        ] {
            assert!(toml::from_str::<Config>(contents).is_err(), "{contents}");
        }
    }

    #[test]
    fn reports_invalid_discovered_config_instead_of_ignoring_it() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("Sycamore.toml"), "[build]\nrelease = true").unwrap();
        let error = Options::resolve_in(&BuildArgs::default(), temp.path()).unwrap_err();
        assert!(format!("{error:#}").contains("Sycamore.toml"));
        assert!(format!("{error:#}").contains("release"));
    }

    #[test]
    fn absolute_config_paths_remain_absolute() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("absolute-output");
        // Serialize the path as a TOML string without platform-specific escaping assumptions.
        let mut config = toml::Table::new();
        let mut build = toml::Table::new();
        build.insert(
            "out-dir".into(),
            toml::Value::String(output.to_string_lossy().into_owned()),
        );
        config.insert("build".into(), toml::Value::Table(build));
        fs::write(
            temp.path().join("Sycamore.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();
        let options = Options::resolve_in(&BuildArgs::default(), temp.path()).unwrap();
        assert_eq!(options.out_dir, Some(output));
    }
}
