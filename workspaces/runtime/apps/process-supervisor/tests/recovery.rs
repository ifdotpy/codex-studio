#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]
#[path = "../../../packages/runtime-process/tests/support/mod.rs"]
mod support;
use std::{collections::BTreeMap, path::Path, process::Command};
use studio_runtime_process::{
    AdminCloseHandleRequest, HealthRequest, SupervisorClient, SupervisorHandle,
};
use support::*;

#[test]
fn unknown_ownership_survives_restart_until_exact_operator_close() {
    let state = DisposableState::new();
    let python = python_executable();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../server/src");
    let created = Command::new(&python).args(["-B", "-c", "import sys; sys.path.insert(0,sys.argv[1]); from codex_process_supervisor import Journal; j=Journal(sys.argv[2]);\nwith j.db() as db: db.execute(\"INSERT INTO handles(id,signature,pid,created) VALUES ('unknown','saved',0,1)\")\nj.close()"])
        .arg(source).arg(&state.0).envs(child_environment(&state.0)).status().unwrap();
    assert!(created.success());
    for restart in 0..2 {
        let supervisor = SupervisorProcess::start(&state, &python);
        let mut client = connect_client(&state);
        let health = client.health(HealthRequest::default()).unwrap();
        assert!(health.recovery.blocked.is_some());
        assert_eq!(health.handles[0].id, "unknown");
        assert!(
            client
                .open(open_request(
                    "unknown",
                    vec![
                        python.clone(),
                        "-c".into(),
                        "raise AssertionError('must not spawn')".into()
                    ],
                    &state.0
                ))
                .is_err()
        );
        if restart == 1 {
            let request = |signature: &str| AdminCloseHandleRequest {
                handle: SupervisorHandle("unknown".into()),
                expected_pid: 0,
                expected_start_time: String::new(),
                expected_signature: signature.into(),
                extra: BTreeMap::new(),
            };
            assert!(client.admin_close_handle(request("saved")).is_err());
            let mut operator =
                SupervisorClient::connect_operator(safe_socket_path(&state), state.0.clone())
                    .unwrap();
            assert!(operator.admin_close_handle(request("wrong")).is_err());
            assert!(
                operator
                    .admin_close_handle(request("saved"))
                    .unwrap()
                    .closed
            );
            let cleared = client.health(HealthRequest::default()).unwrap();
            assert!(cleared.recovery.blocked.is_none());
            assert!(cleared.handles.is_empty());
        }
        drop(client);
        drop(supervisor);
        std::fs::remove_file(state.socket()).unwrap();
    }
    let _supervisor = SupervisorProcess::start(&state, &python);
    let mut client = connect_client(&state);
    let health = client.health(HealthRequest::default()).unwrap();
    assert!(health.recovery.blocked.is_none());
    assert!(health.handles.is_empty());
}

struct Probe(std::process::Child);
impl Drop for Probe {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
        }
        self.0.wait().unwrap();
    }
}

#[test]
fn kernel_collision_refuses_signal_and_legacy_upgrade_commits_before_signal() {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    for mode in ["collision", "legacy", "upgrade-fails"] {
        let state = DisposableState::new();
        let python = python_executable();
        let mut probe = Probe(
            Command::new(&python)
                .args(["-c", "import sys; sys.stdin.read()"])
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .env_clear()
                .envs(child_environment(&state.0))
                .spawn()
                .unwrap(),
        );
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../server/src");
        let output = Command::new(&python).args(["-B", "-c", r#"
import sys, os, json
from pathlib import Path
sys.path.insert(0,sys.argv[1])
from codex_process_supervisor import Journal, process_start_time
root, pid, mode = sys.argv[2], int(sys.argv[3]), sys.argv[4]
ticks = int(Path(f'/proc/{pid}/stat').read_text().rsplit(')',1)[1].split()[19])
boot = Path('/proc/sys/kernel/random/boot_id').read_text().strip()
actual = f'linux:{boot}:{ticks}'
if mode == 'collision':
    hz = os.sysconf('SC_CLK_TCK')
    stale = ticks + 1 if ticks % hz < hz - 1 else ticks - 1
    assert stale != ticks and stale // hz == ticks // hz
    saved = f'linux:{boot}:{stale}'
else:
    saved = process_start_time(pid)
j = Journal(root)
with j.db() as db:
    db.execute("INSERT INTO handles(id,signature,pid,created,generation) VALUES ('fixture','saved',?,1,1)", (pid,))
    db.execute("INSERT INTO child_identities(handle,pid,pgid,start_time) VALUES ('fixture',?,?,?)", (pid,pid,saved))
    if mode == 'upgrade-fails':
        db.execute("CREATE TRIGGER block_upgrade BEFORE UPDATE OF start_time ON child_identities BEGIN SELECT RAISE(ABORT,'injected'); END")
j.close()
print(json.dumps({'actual':actual,'saved':saved}))
"#]).arg(&source).arg(&state.0).arg(probe.0.id().to_string()).arg(mode)
            .envs(child_environment(&state.0)).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let identity: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let _supervisor = SupervisorProcess::start(&state, &python);
        let mut client = connect_client(&state);
        let health = client.health(HealthRequest::default()).unwrap();
        if mode == "legacy" {
            assert!(health.recovery.blocked.is_none());
            assert!(health.handles.is_empty());
            wait_until("verified adopted child termination", || {
                probe.0.try_wait().unwrap().is_some()
            });
        } else {
            assert!(
                probe.0.try_wait().unwrap().is_none(),
                "{mode}: must not signal probe"
            );
            assert_eq!(health.recovery.blocked.is_some(), mode == "upgrade-fails");
        }
        let check = Command::new(&python).args(["-B", "-c", r#"
import sys, sqlite3, json
with sqlite3.connect(sys.argv[1]+'/supervisor.sqlite3') as db:
    start = db.execute("SELECT start_time FROM child_identities WHERE handle='fixture'").fetchone()[0]
    outcome = db.execute("SELECT outcome,start_time FROM recovery_events WHERE handle='fixture' ORDER BY id DESC LIMIT 1").fetchone()
print(json.dumps({'start':start,'outcome':outcome}))
"#]).arg(&state.0).envs(child_environment(&state.0)).output().unwrap();
        assert!(check.status.success());
        let stored: serde_json::Value = serde_json::from_slice(&check.stdout).unwrap();
        if mode == "legacy" {
            assert_eq!(stored["start"], identity["actual"]);
            assert_eq!(stored["outcome"][1], identity["actual"]);
            assert!(matches!(
                stored["outcome"][0].as_str(),
                Some("terminated" | "killed")
            ));
        } else {
            assert_eq!(stored["start"], identity["saved"]);
            assert_eq!(
                stored["outcome"][0],
                if mode == "collision" {
                    "identity-mismatch"
                } else {
                    "termination-failed"
                }
            );
        }
    }
}
