use std::process::ExitCode;

use clap::Parser;

use kanso::cli::{self, Cli};

const EXIT_OK: u8 = 0;
const EXIT_USAGE: u8 = 1;

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            // PLAN.md §6 uses exit code 1 for usage errors; clap defaults to 2.
            let code = if error.use_stderr() {
                EXIT_USAGE
            } else {
                EXIT_OK
            };
            let _ = error.print();
            return ExitCode::from(code);
        }
    };

    match cli::run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            error.print();
            ExitCode::from(error.exit_code())
        }
    }
}
