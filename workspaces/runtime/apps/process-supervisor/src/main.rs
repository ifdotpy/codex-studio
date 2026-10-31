//! Explicit standalone entry point; service installation remains an operator action.
#[cfg(test)]
extern crate studio_test_deps;

/// Only a coverage run (`layr retest` sets `LLVM_PROFILE_FILE`) installs this. Tests end the
/// supervisor with SIGTERM first, and a normal exit writes the coverage profile; SIGKILL
/// leaves none. Without the variable SIGTERM keeps its default action.
#[cfg(unix)]
fn exit_on_sigterm() {
    use tokio::signal::unix::{SignalKind, signal};
    std::thread::spawn(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("signal runtime");
        runtime.block_on(async {
            signal(SignalKind::terminate())
                .expect("SIGTERM handler")
                .recv()
                .await;
        });
        std::process::exit(0);
    });
}

fn main() -> std::process::ExitCode {
    #[cfg(unix)]
    if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
        exit_on_sigterm();
    }
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
    // The supervisor owns the native provider processes; they inherit this limit.
    #[cfg(unix)]
    studio_runtime_process::open_files::raise_open_file_limit();
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
