mod cli;

use std::process::ExitCode;

use cli::{Cli, Command};

fn main() -> ExitCode {
    let cli = Cli::parse_args();

    match cli.command {
        Command::Build(args) => match cargo_sycamore::options::Options::resolve(&args)
            .and_then(|options| cargo_sycamore::build::build(&options))
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("error: {error:#}");
                ExitCode::FAILURE
            }
        },
        Command::Watch(args) => match cargo_sycamore::watch::watch(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("error: {error:#}");
                ExitCode::FAILURE
            }
        },
        Command::New { path } => {
            eprintln!(
                "error: creating a Sycamore application at '{}' is not yet implemented",
                path.display()
            );
            ExitCode::FAILURE
        }
    }
}
