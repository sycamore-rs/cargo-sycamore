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
    Build(cargo_sycamore::options::BuildArgs),
    /// Watch the workspace and rebuild when files change.
    #[command(visible_alias = "w")]
    Watch(cargo_sycamore::options::BuildArgs),
    /// Serve the app locally, watching and rebuilding on changes.
    #[command(visible_alias = "s")]
    Serve(cargo_sycamore::options::ServeArgs),
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
    fn config_only_options_are_not_cli_flags() {
        for flag in ["--features", "--default-features", "--no-default-features"] {
            assert_eq!(
                parse(&["cargo-sycamore", "build", flag])
                    .unwrap_err()
                    .kind(),
                ErrorKind::UnknownArgument
            );
        }
        let cli = parse(&["cargo-sycamore", "b", "--config", "custom.toml"]).unwrap();
        assert!(
            matches!(cli.command, Command::Build(args) if args.config == Some(PathBuf::from("custom.toml")))
        );
    }

    #[test]
    fn parses_serve_with_port_and_build_options() {
        for args in [
            vec!["cargo-sycamore", "serve", "--port", "9000", "--release"],
            vec![
                "cargo-sycamore",
                "sycamore",
                "serve",
                "--port",
                "9000",
                "--release",
            ],
        ] {
            assert!(matches!(parse(&args).unwrap().command,
                Command::Serve(options) if options.port == Some(9000) && options.build.release));
        }
        assert!(
            matches!(parse(&["cargo-sycamore", "serve"]).unwrap().command,
            Command::Serve(options) if options.port.is_none())
        );
        assert!(parse(&["cargo-sycamore", "serve", "--port", "65536"]).is_err());
        assert!(parse(&["cargo-sycamore", "build", "--port", "8080"]).is_err());
    }

    #[test]
    fn parses_examples_for_build_watch_and_serve() {
        for command in ["build", "b", "watch", "w", "serve", "s"] {
            let cli = parse(&[
                "cargo-sycamore",
                "sycamore",
                command,
                "--example",
                "counter",
                "--release",
            ])
            .unwrap();
            let args = match cli.command {
                Command::Build(args) | Command::Watch(args) => args,
                Command::Serve(args) => args.build,
                _ => panic!("unexpected command"),
            };
            assert_eq!(args.example.as_deref(), Some("counter"));
            assert!(args.release);
            assert!(
                parse(&[
                    "cargo-sycamore",
                    command,
                    "--bin",
                    "client",
                    "--example",
                    "counter"
                ])
                .is_err()
            );
            assert!(parse(&["cargo-sycamore", command, "--example"]).is_err());
        }
    }

    #[test]
    fn parses_autoreload_without_overriding_config_when_flag_is_absent() {
        for (flag, expected) in [
            ("--autoreload", true),
            ("--autoreload=true", true),
            ("--autoreload=false", false),
        ] {
            let cli = parse(&["cargo-sycamore", "sycamore", "s", flag, "--port", "9000"]).unwrap();
            assert!(matches!(cli.command, Command::Serve(args)
                if args.autoreload == Some(expected) && args.port == Some(9000)));
        }
        assert!(
            matches!(parse(&["cargo-sycamore", "serve"]).unwrap().command,
            Command::Serve(args) if args.autoreload.is_none())
        );
        assert!(parse(&["cargo-sycamore", "serve", "--autoreload=invalid"]).is_err());
        assert!(parse(&["cargo-sycamore", "build", "--autoreload"]).is_err());
    }

    #[test]
    fn parses_wasm_opt_for_build_watch_serve_and_aliases() {
        for command in ["build", "b", "watch", "w", "serve", "s"] {
            for (flags, expected) in [
                (vec![], None),
                (vec!["--wasm-opt"], Some(true)),
                (vec!["--wasm-opt=true"], Some(true)),
                (vec!["--wasm-opt=false"], Some(false)),
            ] {
                let mut argv = vec!["cargo-sycamore", command, "--release"];
                argv.extend(flags);
                let args = match parse(&argv).unwrap().command {
                    Command::Build(args) | Command::Watch(args) => args,
                    Command::Serve(args) => args.build,
                    _ => unreachable!(),
                };
                assert_eq!(args.wasm_opt, expected);
                assert!(args.release);
            }
        }
        assert!(parse(&["cargo-sycamore", "build", "--wasm-opt=invalid"]).is_err());
        assert!(parse(&["cargo-sycamore", "build", "--wasm-opt-version", "133"]).is_err());
    }

    #[test]
    fn parses_spa_flag_and_explicit_overrides() {
        for command in ["serve", "s"] {
            for (flag, expected) in [
                ("--spa", true),
                ("--spa=true", true),
                ("--spa=false", false),
            ] {
                assert!(
                    matches!(parse(&["cargo-sycamore", command, flag, "--port", "9000"]).unwrap().command,
                    Command::Serve(args) if args.spa == Some(expected) && args.port == Some(9000))
                );
            }
            assert!(
                matches!(parse(&["cargo-sycamore", command]).unwrap().command,
                Command::Serve(args) if args.spa.is_none())
            );
        }
        assert!(parse(&["cargo-sycamore", "serve", "--spa=invalid"]).is_err());
        assert!(parse(&["cargo-sycamore", "build", "--spa"]).is_err());
    }

    #[test]
    fn accepts_serve_shorthand() {
        for args in [
            vec!["cargo-sycamore", "s", "--port", "9000"],
            vec!["cargo-sycamore", "sycamore", "s", "--port", "9000"],
        ] {
            assert!(matches!(parse(&args).unwrap().command,
                Command::Serve(options) if options.port == Some(9000)));
        }
    }

    #[test]
    fn parses_watch_with_build_options() {
        for args in [
            vec![
                "cargo-sycamore",
                "watch",
                "--release",
                "--config",
                "custom.toml",
            ],
            vec![
                "cargo-sycamore",
                "sycamore",
                "watch",
                "--release",
                "--config",
                "custom.toml",
            ],
        ] {
            assert!(matches!(parse(&args).unwrap().command,
                Command::Watch(options) if options.release && options.config == Some(PathBuf::from("custom.toml"))));
        }
    }

    #[test]
    fn accepts_watch_shorthand() {
        for args in [
            vec!["cargo-sycamore", "w", "--release"],
            vec!["cargo-sycamore", "sycamore", "w", "--release"],
        ] {
            assert!(matches!(
                parse(&args).unwrap().command,
                Command::Watch(options) if options.release
            ));
        }
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
