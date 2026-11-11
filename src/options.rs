//! Separate CLI/file inputs from the resolved options consumed by commands.

use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::Args;
use serde::Deserialize;

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
    #[arg(long, value_name = "NAME")]
    pub bin: Option<String>,
    /// Build with Cargo's release profile (CLI only).
    #[arg(long)]
    pub release: bool,
    /// Output directory for JS/WASM bindings (also configurable in Sycamore.toml).
    #[arg(long, value_name = "PATH")]
    pub out_dir: Option<PathBuf>,
    /// Explicit config file; otherwise search for Sycamore.toml in parent directories.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
}

/// File input only. CLI-only settings intentionally cannot be deserialized.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub build: BuildConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct BuildConfig {
    pub manifest_path: Option<PathBuf>,
    pub package: Option<String>,
    pub bin: Option<String>,
    pub out_dir: Option<PathBuf>,
    /// Config-only Cargo feature selection.
    pub features: Vec<String>,
    /// Config-only setting; defaults to true when omitted.
    pub default_features: Option<bool>,
}

/// Final, merged user options. This is neither a clap parser nor a file schema:
/// commands do not need to know which source supplied a value.
#[derive(Debug)]
pub struct Options {
    /// Configuration file used during resolution, if any (for watch mode).
    pub config_path: Option<PathBuf>,
    pub manifest_path: Option<PathBuf>,
    pub package: Option<String>,
    pub bin: Option<String>,
    pub out_dir: Option<PathBuf>,
    pub release: bool,
    pub features: Vec<String>,
    pub default_features: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            config_path: None,
            manifest_path: None,
            package: None,
            bin: None,
            out_dir: None,
            release: false,
            features: Vec::new(),
            default_features: true,
        }
    }
}

impl Options {
    /// Resolve flags over the nearest configuration, then apply defaults.
    /// CLI paths are relative to cwd; config paths are relative to the config file.
    pub fn resolve(args: &BuildArgs) -> Result<Self> {
        Self::resolve_in(
            args,
            &std::env::current_dir().context("reading current directory")?,
        )
    }

    fn resolve_in(args: &BuildArgs, cwd: &Path) -> Result<Self> {
        let config_path = match &args.config {
            Some(path) => Some(absolute(cwd, path)),
            None => {
                let start = args.manifest_path.as_ref().map(|path| absolute(cwd, path));
                discover(start.as_deref().and_then(Path::parent).unwrap_or(cwd))?
            }
        };
        let (config, base) = if let Some(path) = &config_path {
            let contents = fs::read_to_string(path)
                .with_context(|| format!("reading configuration {}", path.display()))?;
            let config: Config = toml::from_str(&contents)
                .with_context(|| format!("parsing configuration {}", path.display()))?;
            (
                config,
                path.parent()
                    .context("config file has no parent directory")?,
            )
        } else {
            (Config::default(), cwd)
        };
        let file = config.build;
        Ok(Self {
            manifest_path: args
                .manifest_path
                .as_ref()
                .map(|p| absolute(cwd, p))
                .or_else(|| file.manifest_path.as_ref().map(|p| absolute(base, p))),
            package: args.package.clone().or(file.package),
            bin: args.bin.clone().or(file.bin),
            out_dir: args
                .out_dir
                .as_ref()
                .map(|p| absolute(cwd, p))
                .or_else(|| file.out_dir.as_ref().map(|p| absolute(base, p))),
            release: args.release,
            features: file.features,
            default_features: file.default_features.unwrap_or(true),
            config_path,
        })
    }
}

fn absolute(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    }
}

fn discover(start: &Path) -> Result<Option<PathBuf>> {
    for directory in start.ancestors() {
        let path = directory.join("Sycamore.toml");
        match fs::metadata(&path) {
            Ok(_) => return Ok(Some(path)),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("checking configuration {}", path.display()));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    package = "configured-package"
    bin = "configured-bin"
    out-dir = "output"
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
            "[build]\npackage = 'root'\n",
        )
        .unwrap();
        fs::write(
            member.join("Sycamore.toml"),
            "[build]\npackage = 'member'\n",
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
    fn explicit_config_overrides_discovery_and_missing_explicit_file_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("Sycamore.toml"),
            "[build]\npackage = 'discovered'\n",
        )
        .unwrap();
        fs::write(
            temp.path().join("custom.toml"),
            "[build]\npackage = 'explicit'\n",
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
            "[build]\ndefault-features = 'not a boolean'",
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
