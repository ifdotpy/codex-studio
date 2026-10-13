//! Explicit standalone entry point; service installation remains an operator action.
fn main() -> std::process::ExitCode {
    let mut args = std::env::args_os().skip(1);
    let mut state = None;
    let mut wait = false;
    while let Some(arg) = args.next() {
        if arg == "--state" {
            state = args.next().map(std::path::PathBuf::from);
        } else if arg == "--wait-for-lease" {
            wait = true;
        } else {
            eprintln!("usage: codex-process-supervisor --state DIRECTORY [--wait-for-lease]");
            return std::process::ExitCode::FAILURE;
        }
    }
    let Some(state) = state else {
        eprintln!("--state is required");
        return std::process::ExitCode::FAILURE;
    };
    #[cfg(unix)]
    match studio_runtime_process::supervisor_daemon::serve(&state, wait) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (state, wait);
        eprintln!("Not implemented: requires Windows supervisor transport");
        std::process::ExitCode::FAILURE
    }
}
