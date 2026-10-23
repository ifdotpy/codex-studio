#![cfg(unix)]
#![allow(clippy::disallowed_methods, clippy::unwrap_used)]

//! The Linux VM start entry points of the runtime binary.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

/// The helper runs in a new session, so a backend restart keeps the VM, and it
/// gets exactly `serve <vm-state>`.
#[test]
fn the_helper_starts_in_its_own_session_with_its_arguments() {
    let root = tempfile::tempdir().unwrap();
    let helper = root.path().join("studio-linux-vm");
    std::fs::write(
        &helper,
        "#!/bin/sh\nprintf '%s %s %s\\n' \"$$\" \"$(ps -o pgid= -p $$ | tr -d ' ')\" \"$*\" > \"$2/helper.out\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let state = root.path().join("linux-vm");
    std::fs::create_dir(&state).unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_studio-runtime-server"))
        .arg("linux-vm-helper")
        .arg(&helper)
        .arg("serve")
        .arg(&state)
        .status()
        .unwrap();
    assert!(status.success());
    let output = std::fs::read_to_string(state.join("helper.out")).unwrap();
    let fields: Vec<&str> = output.trim().splitn(3, ' ').collect();
    assert_eq!(
        fields[0], fields[1],
        "the helper leads its own process group: {output}"
    );
    assert_eq!(fields[2], format!("serve {}", state.display()));
    // Other arguments are refused before anything runs.
    let refused = Command::new(env!("CARGO_BIN_EXE_studio-runtime-server"))
        .args(["linux-vm-helper", "/bin/sh", "-c", "true"])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(2));
}

/// A state folder without a VM helper reports a stopped VM.
#[test]
fn the_status_command_reports_a_stopped_vm() {
    let root = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_studio-runtime-server"))
        .args(["linux-vm", "status", "--state"])
        .arg(root.path())
        .env_remove("CODEX_LINUX_VM_STATE_DIR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["state"], "stopped");
}
