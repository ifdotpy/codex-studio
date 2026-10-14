//! Test-only gates suspend the real callers after preparation, before their writer.
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::json;
use studio_runtime_store::{AgentRepository, RuntimeStore};

struct Gate {
    prepared: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
}
static GATE: Mutex<Option<Gate>> = Mutex::new(None);
const DEADLINE: Duration = Duration::from_secs(30);

pub fn arm() -> (mpsc::Receiver<()>, mpsc::SyncSender<()>) {
    let (prepared, ready) = mpsc::sync_channel(0);
    let (release, proceed) = mpsc::sync_channel(0);
    let old = GATE.lock().expect("gate").replace(Gate {
        prepared,
        release: proceed,
    });
    assert!(old.is_none(), "one gated request per caller test");
    (ready, release)
}

pub fn after_preparation(agent: &str) {
    if agent != "late-operation-recipient" {
        return;
    }
    let gate = GATE.lock().expect("gate").take();
    if let Some(gate) = gate {
        gate.prepared.send(()).expect("preparation observed");
        gate.release
            .recv_timeout(DEADLINE)
            .expect("operation committed before admission");
    }
}

pub fn start_late_operation(
    store: &RuntimeStore,
    ready: mpsc::Receiver<()>,
    release: mpsc::SyncSender<()>,
) {
    ready
        .recv_timeout(DEADLINE)
        .expect("caller completed preparation");
    store
        .transaction(|tx| {
            let mut other = AgentRepository::get(tx, &"other".into())?.expect("idle other");
            assert!(
                other
                    .extra
                    .get("workspaceOperation")
                    .is_none_or(serde_json::Value::is_null)
            );
            other
                .extra
                .insert("workspaceOperation".into(), json!("restore"));
            AgentRepository::put(tx, &other)
        })
        .expect("unrelated operation commits while request is gated");
    release.send(()).expect("release caller");
}

pub struct Fixture {
    pub store: Arc<RuntimeStore>,
    child: Child,
    root: PathBuf,
}
impl Fixture {
    pub fn new(label: &str, prepared_python: fn(&Path) -> PathBuf) -> Self {
        let root =
            std::env::temp_dir().join(format!("cs-w7-p03-gate-{label}-{}", std::process::id()));
        std::fs::create_dir(&root).expect("unique tmpfs fixture");
        std::fs::create_dir(root.join("oracle")).expect("oracle scratch");
        let services = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("packages")
            .join("runtime-services");
        let mut child = Command::new(prepared_python(&services.join("../../apps/server/src")))
            .arg("-B")
            .arg(services.join("tests/wave7_p03_message_ingress/oracle.py"))
            .arg(root.join("oracle"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("isolated Python fixture");
        let state = root.join("state");
        writeln!(
            child.stdin.as_mut().expect("stdin"),
            "{}",
            json!({"rust":state,"case":{"steps":[]}})
        )
        .expect("seed request");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .flush()
            .expect("flush seed");
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("stdout"))
            .read_line(&mut line)
            .expect("seed response");
        let response: serde_json::Value = serde_json::from_str(&line).expect("seed JSON");
        assert!(response.get("oracleError").is_none(), "{response}");
        let store = Arc::new(RuntimeStore::open(Some(&state)).expect("store"));
        store
            .transaction(|tx| {
                let mut agent = AgentRepository::get(tx, &"agent".into())?.expect("Python agent");
                agent.id = "late-operation-recipient".into();
                agent.root_id = agent.id.clone();
                AgentRepository::put(tx, &agent)?;
                agent.id = "other".into();
                agent.root_id = agent.id.clone();
                agent.extra.insert("cwd".into(), json!(root));
                AgentRepository::put(tx, &agent)
            })
            .expect("recipient and idle unrelated workspace");
        Self { store, child, root }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        writeln!(
            self.child.stdin.as_mut().expect("stdin"),
            "{{\"close\":true}}"
        )
        .expect("close oracle");
        assert!(self.child.wait().expect("reap oracle").success());
        std::fs::remove_dir_all(&self.root).expect("remove own fixture");
    }
}
