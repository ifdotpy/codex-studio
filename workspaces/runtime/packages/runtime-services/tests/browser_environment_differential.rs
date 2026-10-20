//! Parity of the per-thread sign-in environment of host Codex threads: Rust
//! `browser_environment::thread_environment` and its owner, Python
//! `codex_browser_host.thread_environment` (docs/vm-browser-bridge.md, section 6.4).

use serde_json::{Map, Value};
use std::process::Command;
use studio_runtime_services::browser_environment::{
    BROWSER_HOST_FOLDER, BROWSER_LAUNCHER, thread_environment,
};

#[path = "support/prepared_python.rs"]
mod prepared_python;

fn python_environment(state: &std::path::Path, session: bool) -> Map<String, Value> {
    let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
    let python = prepared_python::prepared_python(&source);
    let program = concat!(
        "import json, sys\n",
        "from pathlib import Path\n",
        "sys.path.insert(0, sys.argv[1])\n",
        "import codex_browser_host as host\n",
        "state = Path(sys.argv[2])\n",
        "if sys.argv[3] == '1':\n",
        "    host.session_environment(state)\n",
        "print(json.dumps(host.thread_environment(state, 'agent-1')))\n",
    );
    let environment = prepared_python::IsolatedTestEnvironment::new();
    let mut command = Command::new(python);
    command
        .arg("-I")
        .arg("-c")
        .arg(program)
        .arg(&source)
        .arg(state)
        .arg(if session { "1" } else { "0" });
    environment.apply(&mut command);
    let output = prepared_python::output_with_timeout(
        &mut command,
        &source.join("codex_browser_host.py"),
        std::time::Duration::from_secs(60),
    )
    .expect("run the Python sign-in environment");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Python prints one JSON object")
}

#[test]
fn rust_and_python_give_host_codex_threads_the_same_sign_in_environment() {
    let state = tempfile::tempdir().expect("state root");
    // No launcher yet: neither side sets anything.
    assert!(python_environment(state.path(), false).is_empty());
    assert!(thread_environment(state.path(), "agent-1").is_empty());
    // Python's host bridge writes the launcher; both sides then name the same paths.
    let python = python_environment(state.path(), true);
    assert!(
        state
            .path()
            .join(BROWSER_HOST_FOLDER)
            .join(BROWSER_LAUNCHER)
            .is_file()
    );
    assert_eq!(python.len(), 3);
    assert_eq!(thread_environment(state.path(), "agent-1"), python);
}
