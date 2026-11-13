mod cli;

use std::process::ExitCode;

use cargo_sycamore::logging::{self, Verbosity};
use cli::{Cli, Command};

fn main() -> ExitCode {
    let cli = Cli::parse_args();
    logging::set_verbosity(if cli.quiet {
        Verbosity::Quiet
    } else if cli.verbose {
        Verbosity::Verbose
    } else {
        Verbosity::Normal
    });

    match cli.command {
        Command::Build(args) => match cargo_sycamore::options::Options::resolve(&args)
            .and_then(|options| cargo_sycamore::build::build(&options))
        {
            Ok(_) => ExitCode::SUCCESS,
            Err(error) => {
                cargo_sycamore::logging::error(format_args!("{error:#}"));
                ExitCode::FAILURE
            }
        },
        Command::Watch(args) => match cargo_sycamore::watch::watch(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                cargo_sycamore::logging::error(format_args!("{error:#}"));
                ExitCode::FAILURE
            }
        },
        Command::Serve(args) => match cargo_sycamore::serve::serve(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                cargo_sycamore::logging::error(format_args!("{error:#}"));
                ExitCode::FAILURE
            }
        },
        Command::New { path } => {
            cargo_sycamore::logging::error(format_args!(
                "creating a Sycamore application at '{}' is not yet implemented",
                path.display()
            ));
            ExitCode::FAILURE
        }
    }
}
