//! Read-only Mac sync preview without opening the server database.

use std::{path::PathBuf, process::ExitCode};

use studio_runtime_services::{linux_vm_guest::UnixGuestRpc, mac_sync::round::ProjectSync, vault};

pub(super) fn run(args: &[String]) -> super::Result<ExitCode> {
    let [command, project, flag, state] = args else {
        return Err("usage: studio-runtime-server mac-sync plan PROJECT --state DIRECTORY".into());
    };
    if command != "plan" || flag != "--state" {
        return Err("usage: studio-runtime-server mac-sync plan PROJECT --state DIRECTORY".into());
    }
    let state = PathBuf::from(state);
    if !state.is_absolute() || !state.is_dir() {
        return Err("The runtime state directory must exist and be absolute".into());
    }
    let guest = UnixGuestRpc::for_runtime_state(&state);
    let project = ProjectSync::open(&guest, &vault::linux_vm_state_dir(&state), project)?;
    println!("{}", serde_json::to_string_pretty(&project.plan()?)?);
    Ok(ExitCode::SUCCESS)
}
