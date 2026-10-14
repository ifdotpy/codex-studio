#![allow(clippy::unwrap_used)]

#[path = "../../tests/support/prepared_python.rs"]
mod prepared_python;

use crate::managed_host_tools::{
    Actor, ChildCreationPort, ComplaintPort, ContextPort, Error, InterruptPort, OperationResult,
    PostCommitPort, ReviewPort, SearchPort, SendPort, StatusPort, WakeSignal, WatchMonitorPort,
    WorkAssignmentPort,
};
use crate::provider_callbacks::{
    CallbackContext, CallbackDecision, CallbackError, dispatch_with_managed_tools,
};
use rusqlite::{Transaction, params};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::Command,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use studio_runtime_providers::{Frame, Method, RequestFrame, RequestId};
use studio_runtime_store::{RuntimeStore, StoreError};

struct Template {
    _scratch: tempfile::TempDir,
    value: Value,
}

fn template() -> &'static Template {
    static TEMPLATE: OnceLock<Template> = OnceLock::new();
    TEMPLATE.get_or_init(|| {
        let scratch = tempfile::tempdir().unwrap();
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let source = manifest.join("../../apps/server/src");
        let oracle = manifest.join("tests/provider_callbacks/oracle.py");
        let output = Command::new(prepared_python::prepared_python(&source))
            .arg(oracle)
            .arg(scratch.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "oracle: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Template {
            _scratch: scratch,
            value: serde_json::from_slice(&output.stdout).unwrap(),
        }
    })
}

fn fixture() -> (tempfile::TempDir, RuntimeStore, CallbackContext) {
    let template = template();
    let directory = tempfile::tempdir().unwrap();
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(
            PathBuf::from(template.value["template"].as_str().unwrap()).join(name),
            directory.path().join(name),
        )
        .unwrap();
    }
    let store = RuntimeStore::open(Some(directory.path())).unwrap();
    store
        .transaction_with::<_, StoreError>(|tx| {
            tx.execute_batch(
                "CREATE TABLE p05_test_intents (kind TEXT NOT NULL, id TEXT NOT NULL, payload TEXT NOT NULL, started INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(kind,id)); CREATE TABLE p05_test_starts (child_id TEXT PRIMARY KEY);",
            )?;
            Ok(())
        })
        .unwrap();
    let actor = &template.value["agents"][0];
    let context = CallbackContext {
        agent: actor["id"].as_str().unwrap().into(),
        account: "default".into(),
        connection: "connection".into(),
        generation: 7,
        epoch: 3,
        thread: "thread-0".into(),
        turn: "turn-0".into(),
    };
    (directory, store, context)
}

#[derive(Default)]
struct PortBundle {
    staged: AtomicUsize,
    released: AtomicUsize,
    skip_release: AtomicBool,
    fail_reply_save: AtomicBool,
}

impl PortBundle {
    fn stage(
        &self,
        tx: &Transaction<'_>,
        kind: &str,
        request_id: &str,
        value: Value,
    ) -> Result<OperationResult, Error> {
        tx.execute(
            "INSERT OR IGNORE INTO p05_test_intents(kind,id,payload) VALUES(?1,?2,?3)",
            params![kind, request_id, value.to_string()],
        )?;
        self.staged.fetch_add(1, Ordering::SeqCst);
        Ok(OperationResult {
            value,
            signals: vec![WakeSignal::new(kind)],
        })
    }
}

impl ChildCreationPort for PortBundle {
    type Prepared = String;

    fn prepare_work_item(
        &self,
        tx: &Transaction<'_>,
        _: &Actor,
        request_id: &str,
        spec: &Value,
        _: f64,
    ) -> Result<Self::Prepared, Error> {
        let id = format!("child-{request_id}");
        tx.execute(
            "INSERT OR IGNORE INTO p05_test_intents(kind,id,payload) VALUES('spawn',?1,?2)",
            params![id, spec.to_string()],
        )?;
        self.staged.fetch_add(1, Ordering::SeqCst);
        Ok(id)
    }

    fn recheck_worker(
        &self,
        _: &Transaction<'_>,
        _: &Actor,
        _: &Self::Prepared,
    ) -> Result<(), Error> {
        Ok(())
    }

    fn persisted_record(
        &self,
        _: &Transaction<'_>,
        child_id: &Self::Prepared,
    ) -> Result<Value, Error> {
        Ok(
            json!({"id":child_id,"name":"Worker","status":"starting","provider":"codex","cwd":"/isolated","worktree":true}),
        )
    }

    fn wake_signal(&self, _: &Self::Prepared) -> WakeSignal {
        WakeSignal::new("child-start")
    }
}

impl WorkAssignmentPort for PortBundle {
    fn assign_in_transaction(
        &self,
        tx: &Transaction<'_>,
        task_id: &str,
        child_id: &str,
        _: &studio_runtime_store::AgentRecord,
        _: f64,
    ) -> Result<(), Error> {
        tx.execute(
            "INSERT OR IGNORE INTO p05_test_intents(kind,id,payload) VALUES('assignment',?1,?2)",
            params![task_id, child_id],
        )?;
        Ok(())
    }
}

impl SendPort for PortBundle {
    fn send(
        &self,
        tx: &Transaction<'_>,
        _: &Actor,
        args: &Value,
        request_id: &str,
        _: f64,
    ) -> Result<OperationResult, Error> {
        self.stage(tx, "send", request_id, args.clone())
    }
}

impl InterruptPort for PortBundle {
    fn interrupt_in_transaction(
        &self,
        tx: &Transaction<'_>,
        _: &Actor,
        target_id: &str,
        reason: &str,
        request_id: &str,
        _: f64,
    ) -> Result<OperationResult, Error> {
        self.stage(
            tx,
            "interrupt",
            request_id,
            json!({"targetId":target_id,"reason":reason}),
        )
    }
}

impl StatusPort for PortBundle {
    fn status(
        &self,
        tx: &Transaction<'_>,
        _: &Actor,
        args: &Value,
    ) -> Result<OperationResult, Error> {
        let result = self.stage(tx, "status", "status", args.clone())?;
        if self.fail_reply_save.load(Ordering::SeqCst) {
            tx.execute_batch("DROP TABLE runtime_tool_results")?;
        }
        Ok(result)
    }
}

impl SearchPort for PortBundle {
    fn search(
        &self,
        tx: &Transaction<'_>,
        _: &Actor,
        args: &Value,
    ) -> Result<OperationResult, Error> {
        self.stage(tx, "search", "search", args.clone())
    }
}

impl ContextPort for PortBundle {
    fn context(
        &self,
        tx: &Transaction<'_>,
        _: &Actor,
        args: &Value,
    ) -> Result<OperationResult, Error> {
        self.stage(tx, "context", "context", args.clone())
    }
}

impl ComplaintPort for PortBundle {
    fn complaint(
        &self,
        tx: &Transaction<'_>,
        _: &Actor,
        args: &Value,
        request_id: &str,
        _: f64,
    ) -> Result<OperationResult, Error> {
        self.stage(tx, "complaint", request_id, args.clone())
    }
}

impl WatchMonitorPort for PortBundle {
    fn watch(
        &self,
        tx: &Transaction<'_>,
        _: &Actor,
        args: &Value,
        request_id: &str,
        _: f64,
    ) -> Result<OperationResult, Error> {
        self.stage(tx, "watch", request_id, args.clone())
    }

    fn monitor(
        &self,
        tx: &Transaction<'_>,
        _: &Actor,
        args: &Value,
        request_id: &str,
        _: f64,
    ) -> Result<OperationResult, Error> {
        self.stage(tx, "monitor", request_id, args.clone())
    }
}

impl ReviewPort for PortBundle {
    fn review(
        &self,
        tx: &Transaction<'_>,
        _: &Actor,
        args: &Value,
        request_id: &str,
        _: f64,
    ) -> Result<OperationResult, Error> {
        self.stage(tx, "review", request_id, args.clone())
    }
}

impl PostCommitPort for PortBundle {
    fn release(&self, signals: &[WakeSignal]) -> Result<(), Error> {
        if !self.skip_release.load(Ordering::SeqCst) {
            self.released.fetch_add(signals.len(), Ordering::SeqCst);
        }
        Ok(())
    }
}

fn call(context: &CallbackContext, rpc: &str, tool: &str, args: Value) -> Frame {
    Frame::Request(RequestFrame {
        id: RequestId::String(format!("rpc-{rpc}")),
        method: Method::ItemToolCall,
        params: Some(json!({
            "threadId":context.thread,
            "turnId":context.turn,
            "callId":rpc,
            "tool":tool,
            "arguments":args
        })),
        fields: Default::default(),
    })
}

fn intent_count(store: &RuntimeStore, kind: &str) -> i64 {
    store
        .read()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM p05_test_intents WHERE kind=?1",
            [kind],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn every_managed_tool_runs_through_provider_callback_dispatch() {
    let (_root, store, context) = fixture();
    let ports = PortBundle::default();
    let worker = template().value["agents"][1]["id"].as_str().unwrap();
    let cases = [
        (
            "orchestration_spawn",
            json!({"agents":[{"name":"Worker","prompt":"Task"}]}),
        ),
        (
            "orchestration_send",
            json!({"agent_id":"worker","text":"continue"}),
        ),
        ("orchestration_interrupt", json!({"agent_id":worker})),
        ("orchestration_status", json!({})),
        ("orchestration_search", json!({"query":"needle"})),
        ("orchestration_context", json!({"topic":"plan"})),
        (
            "orchestration_complaint",
            json!({"action":"submit","text":"blocked"}),
        ),
        ("orchestration_watch", json!({"action":"list"})),
        ("orchestration_monitor", json!({"command":"true"})),
        ("orchestration_review", json!({"target":{"kind":"diff"}})),
    ];
    for (index, (tool, args)) in cases.into_iter().enumerate() {
        let rpc = format!("managed-{index}");
        let frame = call(&context, &rpc, tool, args);
        let decision = dispatch_with_managed_tools(&store, &context, &frame, 20.0, &ports).unwrap();
        assert!(matches!(decision, CallbackDecision::Answer(_)), "{tool}");
    }
    assert_eq!(ports.staged.load(Ordering::SeqCst), 10);
    assert_eq!(ports.released.load(Ordering::SeqCst), 10);
}

#[test]
fn failed_final_transaction_rolls_back_intent_and_does_not_release() {
    let (_root, store, context) = fixture();
    let ports = PortBundle::default();
    ports.fail_reply_save.store(true, Ordering::SeqCst);
    let frame = call(
        &context,
        "rollback-status",
        "orchestration_status",
        json!({}),
    );
    assert!(dispatch_with_managed_tools(&store, &context, &frame, 20.0, &ports).is_err());
    assert_eq!(ports.released.load(Ordering::SeqCst), 0);
    assert_eq!(intent_count(&store, "status"), 0);
}

#[test]
fn spawn_intent_is_reconciled_after_restart_when_no_signal_was_released() {
    let (root, store, context) = fixture();
    let ports = PortBundle::default();
    ports.skip_release.store(true, Ordering::SeqCst);
    let frame = call(
        &context,
        "crash-spawn",
        "orchestration_spawn",
        json!({"agents":[{"name":"Worker","prompt":"Task"}]}),
    );
    dispatch_with_managed_tools(&store, &context, &frame, 20.0, &ports).unwrap();
    assert_eq!(ports.released.load(Ordering::SeqCst), 0);
    assert_eq!(intent_count(&store, "spawn"), 1);
    drop(store);

    let restarted = RuntimeStore::open(Some(root.path())).unwrap();
    for _ in 0..2 {
        restarted
            .transaction_with::<_, StoreError>(|tx| {
                tx.execute(
                    "INSERT OR IGNORE INTO p05_test_starts(child_id) SELECT id FROM p05_test_intents WHERE kind='spawn' AND started=0",
                    [],
                )?;
                tx.execute(
                    "UPDATE p05_test_intents SET started=1 WHERE kind='spawn' AND started=0",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
    }
    let starts: i64 = restarted
        .read()
        .unwrap()
        .query_row("SELECT count(*) FROM p05_test_starts", [], |row| row.get(0))
        .unwrap();
    assert_eq!(starts, 1);
}

#[test]
fn same_request_replays_saved_result_without_a_second_effect() {
    let (_root, store, context) = fixture();
    let ports = PortBundle::default();
    let frame = call(
        &context,
        "exact-replay",
        "orchestration_send",
        json!({"agent_id":"worker","text":"continue"}),
    );
    let first = dispatch_with_managed_tools(&store, &context, &frame, 20.0, &ports).unwrap();
    let replay = dispatch_with_managed_tools(&store, &context, &frame, 21.0, &ports).unwrap();
    let (CallbackDecision::Answer(first), CallbackDecision::Answer(replay)) = (first, replay)
    else {
        panic!("expected saved answers")
    };
    assert_eq!(first.body, replay.body);
    assert_eq!(ports.staged.load(Ordering::SeqCst), 1);
    assert_eq!(ports.released.load(Ordering::SeqCst), 1);
}

#[test]
fn remote_origin_cannot_use_managed_ports() {
    let (_root, store, context) = fixture();
    let ports = PortBundle::default();
    store
        .transaction_with::<_, StoreError>(|tx| {
            let raw: String = tx.query_row(
                "SELECT record FROM runtime_agents WHERE id=?1",
                [&context.agent],
                |row| row.get(0),
            )?;
            let mut actor: Value = serde_json::from_str(&raw).unwrap();
            actor["remoteOrigin"] = json!(true);
            tx.execute(
                "UPDATE runtime_agents SET record=?2 WHERE id=?1",
                params![context.agent, actor.to_string()],
            )?;
            Ok(())
        })
        .unwrap();
    let frame = call(
        &context,
        "cross-team",
        "orchestration_search",
        json!({"query":"private"}),
    );
    let answer = dispatch_with_managed_tools(&store, &context, &frame, 20.0, &ports).unwrap();
    assert!(matches!(answer, CallbackDecision::Answer(_)));
    assert_eq!(ports.staged.load(Ordering::SeqCst), 0);
    assert_eq!(ports.released.load(Ordering::SeqCst), 0);
    assert_eq!(intent_count(&store, "search"), 0);
}

#[test]
fn queued_managed_tool_request_can_be_cancelled_before_dispatch() {
    let (_root, store, context) = fixture();
    let queued = template().value["reservations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|reservation| &reservation["record"])
        .find(|record| record["tool"] == "orchestration_spawn")
        .unwrap()
        .clone();
    let request_id = queued["id"].as_str().unwrap().to_owned();
    store
        .transaction_with(|tx| {
            let stored =
                studio_runtime_store::system_reads::get(tx, "runtime_tool_requests", &request_id)?;
            assert_eq!(stored.as_ref(), Some(&queued));
            let result = crate::tool_requests::execute(
                tx,
                &context.agent,
                crate::tool_requests::Action::Cancel(&request_id),
                21.0,
            )?;
            Ok::<_, CallbackError>(result)
        })
        .unwrap();
    let connection = store.read().unwrap();
    let cancelled =
        studio_runtime_store::system_reads::get(&connection, "runtime_tool_requests", &request_id)
            .unwrap()
            .unwrap();
    assert_eq!(cancelled["stage"], "cancelled");
    assert_eq!(cancelled["cancelRequested"], true);
    assert_eq!(intent_count(&store, "spawn"), 0);
}
