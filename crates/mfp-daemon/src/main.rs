//! The daemon entry point. Everything it does lives in the library beside it.

use std::process::ExitCode;

fn main() -> anyhow::Result<ExitCode> {
    mfp_daemon::ipc::server::run()
}
