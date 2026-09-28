use std::process::ExitCode;

fn main() -> anyhow::Result<ExitCode> {
    mfp_daemon::ipc::server::run()
}
