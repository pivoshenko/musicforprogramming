//! The `mfp` entry point: no subcommand draws the interface; one acts, prints a line, exits.

use std::process::ExitCode;

fn main() -> ExitCode {
    mfp_tui::cli::run()
}
