use std::{ffi::OsString, path::PathBuf};

use clap::{Parser, Subcommand};

/// Command-line tools for building Sycamore applications.
#[derive(Debug, Parser)]
#[command(name = "cargo sycamore", bin_name = "cargo sycamore", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Build a WASM application and generate web bindings.
    #[command(visible_alias = "b")]
    Build(cargo_sycamore::build::BuildOptions),
    /// Create a new Sycamore application (not yet implemented).
    New {
        /// Directory for the new application.
        path: PathBuf,
    },
}

impl Cli {
    pub fn parse_args() -> Self {
        Self::parse_from(normalize_args(std::env::args_os()))
    }
}

// Cargo invokes external subcommands as `cargo-sycamore sycamore ...`.
// Remove only that first argument, preserving paths and other OS strings.
fn normalize_args(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut args: Vec<_> = args.into_iter().collect();
    if args.get(1).is_some_and(|arg| arg == "sycamore") {
        args.remove(1);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(normalize_args(args.iter().map(OsString::from)))
    }

    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn accepts_direct_and_cargo_invocation() {
        for args in [
            vec!["cargo-sycamore", "new", "my-app"],
            vec!["cargo-sycamore", "sycamore", "new", "my-app"],
        ] {
            let cli = parse(&args).unwrap();
            assert!(
                matches!(cli.command, Command::New { path } if path == std::path::Path::new("my-app"))
            );
        }
    }

    #[test]
    fn parses_build_options() {
        let cli = parse(&[
            "cargo-sycamore",
            "sycamore",
            "build",
            "--release",
            "--package",
            "app",
            "--bin",
            "client",
            "--manifest-path",
            "app/Cargo.toml",
            "--out-dir",
            "public",
        ])
        .unwrap();
        let Command::Build(options) = cli.command else {
            panic!("expected build");
        };
        assert!(options.release);
        assert_eq!(options.package.as_deref(), Some("app"));
        assert_eq!(options.bin.as_deref(), Some("client"));
        assert_eq!(options.out_dir, Some(PathBuf::from("public")));
        assert_eq!(options.manifest_path, Some(PathBuf::from("app/Cargo.toml")));
        assert!(matches!(
            parse(&["cargo-sycamore", "build"]).unwrap().command,
            Command::Build(_)
        ));
    }

    #[test]
    fn accepts_build_shorthand() {
        for args in [
            vec!["cargo-sycamore", "b", "--release"],
            vec!["cargo-sycamore", "sycamore", "b", "--release"],
        ] {
            assert!(matches!(
                parse(&args).unwrap().command,
                Command::Build(options) if options.release
            ));
        }
    }

    #[test]
    fn preserves_a_project_named_sycamore() {
        let cli = parse(&["cargo-sycamore", "sycamore", "new", "sycamore"]).unwrap();
        assert!(
            matches!(cli.command, Command::New { path } if path == std::path::Path::new("sycamore"))
        );
    }

    #[test]
    fn exposes_help_and_version() {
        for (flag, kind) in [
            ("--help", ErrorKind::DisplayHelp),
            ("--version", ErrorKind::DisplayVersion),
        ] {
            assert_eq!(parse(&["cargo-sycamore", flag]).unwrap_err().kind(), kind);
            assert_eq!(
                parse(&["cargo-sycamore", "sycamore", flag])
                    .unwrap_err()
                    .kind(),
                kind
            );
        }
    }

    #[test]
    fn rejects_missing_arguments_and_unknown_commands() {
        assert_eq!(
            parse(&["cargo-sycamore"]).unwrap_err().kind(),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
        assert_eq!(
            parse(&["cargo-sycamore", "new"]).unwrap_err().kind(),
            ErrorKind::MissingRequiredArgument
        );
        assert_eq!(
            parse(&["cargo-sycamore", "unknown"]).unwrap_err().kind(),
            ErrorKind::InvalidSubcommand
        );
    }
}
