//! `studio-runtime-server linux-vm <start|stop|status> [--state DIRECTORY]`: a
//! person's start or stop of a created Linux VM, and its helper state
//! (docs/linux-vm-workspaces.md, "VM start").

use std::path::PathBuf;
use std::process::ExitCode;

use super::Result;

pub(super) fn run(args: &[String]) -> Result<ExitCode> {
    #[cfg(not(unix))]
    {
        let _unsupported = args;
        Err("Linux VM workspaces require macOS on Apple silicon".into())
    }
    #[cfg(unix)]
    {
        let (command, state) = match args {
            [command] => (command.as_str(), None),
            [command, flag, state] if flag == "--state" => {
                (command.as_str(), Some(PathBuf::from(state)))
            }
            _ => {
                return Err(
                    "usage: studio-runtime-server linux-vm <start|stop|status> [--state DIRECTORY]"
                        .into(),
                );
            }
        };
        let state = match state {
            Some(state) => state,
            None => studio_runtime_store::resolve_state_dir()?.into_path_buf(),
        };
        let vm = studio_runtime_services::linux_vm_runtime::VmRuntime::production(
            studio_runtime_services::vault::linux_vm_state_dir(&state),
            crate::host_runtime::resolve_install_root()
                .map_err(|error| error.to_string())?
                .linux_vm_helper(),
            std::env::current_exe()?,
        );
        let result = match command {
            "status" => vm.status(),
            "start" => tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(vm.start()),
            "stop" => tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(vm.stop()),
            _ => {
                return Err(
                    "usage: studio-runtime-server linux-vm <start|stop|status> [--state DIRECTORY]"
                        .into(),
                );
            }
        };
        match result {
            Ok(status) => {
                println!("{}", serde_json::Value::Object(status));
                Ok(ExitCode::SUCCESS)
            }
            Err(error) => {
                println!("{}", serde_json::json!({"error": error.to_string()}));
                Ok(ExitCode::FAILURE)
            }
        }
    }
}
