//! The environment that sends browser links of host Codex threads to Studio
//! (docs/vm-browser-bridge.md, section 6.4). Python `codex_browser_host.thread_environment`
//! owns the same rule; `tests/browser_environment_differential.rs` checks both.

use serde_json::{Map, Value};
use std::path::Path;

/// The host bridge folder in the server state directory (Python `codex_browser_host.FOLDER`).
pub const BROWSER_HOST_FOLDER: &str = "browser-host";
/// The launcher that a command runs with one URL (Python `codex_browser_host.LAUNCHER`).
pub const BROWSER_LAUNCHER: &str = "studio-browser";
/// The shim socket of the host bridge.
pub const BROWSER_OPEN_SOCKET: &str = "open.sock";

/// Per-thread Codex config overrides for a host Codex agent. Codex applies
/// `shell_environment_policy.set` to the thread's commands, so the app-server
/// launch does not change. PATH is not set: a Codex `set.PATH` replaces the
/// whole PATH of the command, also the entries that the person's shell startup
/// files add (measured with codex-cli 0.160.1, docs/vm-browser-bridge.md
/// section 6.4). Nothing is set before the launcher exists, so a server without
/// the host bridge keeps the tool's own browser behaviour.
pub fn thread_environment(state_root: &Path, agent_id: &str) -> Map<String, Value> {
    let folder = state_root.join(BROWSER_HOST_FOLDER);
    let launcher = folder.join(BROWSER_LAUNCHER);
    let mut overrides = Map::new();
    if !launcher.is_file() {
        return overrides;
    }
    overrides.insert(
        "shell_environment_policy.set.BROWSER".into(),
        Value::String(launcher.to_string_lossy().into_owned()),
    );
    overrides.insert(
        "shell_environment_policy.set.STUDIO_BROWSER_SOCKET".into(),
        Value::String(
            folder
                .join(BROWSER_OPEN_SOCKET)
                .to_string_lossy()
                .into_owned(),
        ),
    );
    overrides.insert(
        "shell_environment_policy.set.STUDIO_BROWSER_SESSION".into(),
        Value::String(agent_id.to_owned()),
    );
    overrides
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_names_appear_only_once_the_launcher_exists() {
        let root = tempfile::tempdir().expect("state root");
        assert!(thread_environment(root.path(), "agent-1").is_empty());
        let folder = root.path().join(BROWSER_HOST_FOLDER);
        std::fs::create_dir_all(&folder).expect("folder");
        std::fs::write(folder.join(BROWSER_LAUNCHER), "#!/bin/sh\n").expect("launcher");
        let overrides = thread_environment(root.path(), "agent-1");
        assert_eq!(
            overrides["shell_environment_policy.set.BROWSER"],
            folder.join(BROWSER_LAUNCHER).to_string_lossy().as_ref()
        );
        assert_eq!(
            overrides["shell_environment_policy.set.STUDIO_BROWSER_SOCKET"],
            folder.join(BROWSER_OPEN_SOCKET).to_string_lossy().as_ref()
        );
        assert_eq!(
            overrides["shell_environment_policy.set.STUDIO_BROWSER_SESSION"],
            "agent-1"
        );
        assert_eq!(overrides.len(), 3);
    }
}
