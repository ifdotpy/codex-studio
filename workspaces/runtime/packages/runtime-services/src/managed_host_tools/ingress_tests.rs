#![allow(clippy::duplicate_mod, clippy::unwrap_used)]

#[path = "../../tests/support/prepared_python.rs"]
mod prepared_python;

use crate::managed_host_tools::{
    Actor, ChildCreationPort, ComplaintPort, ContextPort, Error, InterruptPort, OperationResult,
    PostCommitPort, ReviewPort, SearchPort, SendPort, StatusPort, WakeSignal, WatchMonitorPort,
};
use crate::provider_callbacks::{
    CallbackContext, CallbackDecision, CallbackError, dispatch_with_managed_tools,
};
use rusqlite::types::ValueRef;
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
use studio_runtime_providers::{Frame, Method, RequestFrame, RequestId, ResponseBody};
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

fn application_table_snapshot(store: &RuntimeStore) -> Vec<(String, Vec<String>)> {
    use rusqlite::types::ValueRef;
    let connection = store.read().unwrap();
    let mut tables = connection
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap();
    let names = tables
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let mut snapshot = Vec::new();
    for name in names {
        // Callback ingress necessarily persists its idempotency/result receipt.
        // The tested operation must leave every other row byte-for-byte stable.
        if matches!(
            name.as_str(),
            "runtime_tool_requests" | "runtime_tool_results" | "runtime_tool_request_aliases"
        ) {
            continue;
        }
        let quoted = name.replace('"', "\"\"");
        let statement = connection
            .prepare(&format!("SELECT * FROM \"{quoted}\""))
            .unwrap();
        let columns = statement.column_count();
        drop(statement);
        let ordering = (1..=columns)
            .map(|column| column.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let receipt_filter = if name == "runtime_operation_receipts" {
            " WHERE signature!='provider-callback'"
        } else {
            ""
        };
        let mut statement = connection
            .prepare(&format!(
                "SELECT * FROM \"{quoted}\"{receipt_filter} ORDER BY {ordering}"
            ))
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                let mut values = Vec::new();
                for column in 0..columns {
                    let value = row.get_ref(column)?;
                    values.push(match value {
                        ValueRef::Null => "null".to_owned(),
                        ValueRef::Integer(value) => format!("i:{value}"),
                        ValueRef::Real(value) => format!("r:{value:?}"),
                        ValueRef::Text(value) => format!("t:{}", String::from_utf8_lossy(value)),
                        ValueRef::Blob(value) => format!("b:{value:?}"),
                    });
                }
                Ok(values.join("\u{1f}"))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        snapshot.push((name, rows));
    }
    snapshot
}

fn managed_snapshot(store: &RuntimeStore, tables: &Value) -> Value {
    let connection = store.read().unwrap();
    let mut snapshot = serde_json::Map::new();
    for name in tables.as_object().unwrap().keys() {
        let quoted = name.replace('"', "\"\"");
        let column_names = connection
            .prepare(&format!("PRAGMA table_info(\"{quoted}\")"))
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let statement = connection
            .prepare(&format!("SELECT * FROM \"{quoted}\""))
            .unwrap();
        let columns = statement.column_count();
        drop(statement);
        let ordering = (1..=columns)
            .map(|column| column.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let mut statement = connection
            .prepare(&format!("SELECT * FROM \"{quoted}\" ORDER BY {ordering}"))
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                let mut values = Vec::new();
                for (column, column_name) in column_names.iter().enumerate().take(columns) {
                    let mut value = if name == "sync_entities" && column_name == "seq" {
                        json!(0)
                    } else {
                        match row.get_ref(column)? {
                        ValueRef::Null => Value::Null,
                        ValueRef::Integer(value) => json!(value),
                        ValueRef::Real(value) => json!(value),
                        ValueRef::Text(value) => {
                            let text = String::from_utf8_lossy(value);
                            serde_json::from_str(&text).unwrap_or_else(|_| json!(text.as_ref()))
                        }
                        ValueRef::Blob(value) => json!({
                            "blobHex":value.iter().map(|byte|format!("{byte:02x}")).collect::<String>()
                        }),
                        }
                    };
                    // Python's callback harness updates unrelated model
                    // context fields while resolving calls. The event timing
                    // and agent serverId are fixture-specific. Preserve every
                    // other row and projection field.
                    if name == "runtime_agents"
                        && column_name == "record"
                        && let Some(record) = value.as_object_mut()
                    {
                        record.remove("concurrency");
                        record.remove("deliveredMode");
                    }
                    if name == "runtime_event_meta"
                        && column_name == "record"
                        && let Some(record) = value.as_object_mut()
                    {
                        record.remove("timing");
                    }
                    if name == "sync_entities"
                        && column_name == "payload"
                        && let Some(payload) = value.as_object_mut()
                        && payload["collection"] == "agent"
                        && let Some(record) = payload["value"].as_object_mut()
                    {
                        record.remove("concurrency");
                        record.remove("deliveredMode");
                        record.remove("serverId");
                    }
                    values.push(value);
                }
                if name == "sync_entities" && values.first() == Some(&json!("agent")) {
                    // hash commits the full projection, including the
                    // fixture-specific agent serverId omitted above.
                    values[3] = json!("normalized-agent-projection-hash");
                }
                Ok(Value::Array(values))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        snapshot.insert(name.clone(), json!(rows));
    }
    Value::Object(snapshot)
}

fn dispatch_python_case(
    store: &RuntimeStore,
    oracle: &Value,
    expected: &Value,
    args: Option<Value>,
) -> Result<CallbackDecision, CallbackError> {
    let actor = oracle["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|actor| actor["id"] == expected["actor"])
        .unwrap();
    let context = CallbackContext {
        agent: actor["id"].as_str().unwrap().to_owned(),
        account: "default".into(),
        connection: "connection".into(),
        generation: 7,
        epoch: actor["epoch"].as_u64().unwrap(),
        thread: actor["threadId"].as_str().unwrap().to_owned(),
        turn: actor["turnId"].as_str().unwrap().to_owned(),
    };
    let frame = call(
        &context,
        expected["callId"].as_str().unwrap(),
        expected["tool"].as_str().unwrap(),
        args.unwrap_or_else(|| expected["arguments"].clone()),
    );
    let ports = crate::provider_callbacks::managed_tool_ports(
        store,
        &context,
        &frame,
        std::sync::Arc::new(tokio::sync::Notify::new()),
    )?;
    dispatch_with_managed_tools(store, &context, &frame, 20.0, &ports)
}

#[test]
fn named_refusals_leave_all_application_tables_unchanged() {
    let descendant = template().value["agents"][1]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let cases = [
        (
            "orchestration_interrupt",
            json!({"agent_id":descendant}),
            "account-worker/agent-interrupt",
        ),
        (
            "orchestration_status",
            json!({}),
            "model-directory/status-and-dispatch-capacity",
        ),
        (
            "orchestration_watch",
            json!({"action":"list"}),
            "rules/runtime-driver",
        ),
        (
            "orchestration_monitor",
            json!({"command":"true"}),
            "workspace-state/monitor-admission",
        ),
        (
            "orchestration_monitor_input",
            json!({"monitor_id":"missing-monitor","text":"input"}),
            "monitor-input-request-identity",
        ),
        (
            "orchestration_spawn",
            json!({"agents":[{"name":"Worker","prompt":"Task"}]}),
            "agent-create/worker-driver",
        ),
        (
            "orchestration_review",
            json!({"target":{"type":"uncommittedChanges"}}),
            "agent-create/worker-driver",
        ),
        (
            "orchestration_context",
            json!({"topic":"profiles"}),
            "model-context/requested-topic",
        ),
    ];
    for (index, (tool, args, dependency)) in cases.into_iter().enumerate() {
        let (_root, store, context) = fixture();
        let before = application_table_snapshot(&store);
        let rpc = format!("refusal-{index}");
        let frame = call(&context, &rpc, tool, args);
        let wake = std::sync::Arc::new(tokio::sync::Notify::new());
        let ports =
            crate::provider_callbacks::managed_tool_ports(&store, &context, &frame, wake).unwrap();
        let decision = dispatch_with_managed_tools(&store, &context, &frame, 20.0, &ports).unwrap();
        assert!(
            format!("{decision:?}").contains(dependency),
            "{tool} did not name {dependency}: {decision:?}"
        );
        let after = application_table_snapshot(&store);
        let changed = before
            .iter()
            .zip(&after)
            .filter_map(|((name, old), (_, new))| (old != new).then_some(name.as_str()))
            .collect::<Vec<_>>();
        assert!(changed.is_empty(), "{tool} changed tables: {changed:?}");
    }
}

#[test]
fn managed_tools_match_python_rows_events_and_sync_projection() {
    let (_root, store, _context) = fixture();
    let oracle = &template().value;
    let agents = oracle["agents"].as_array().unwrap();
    let wake = std::sync::Arc::new(tokio::sync::Notify::new());
    for expected in oracle["managedToolCases"].as_array().unwrap() {
        let actor = agents
            .iter()
            .find(|actor| actor["id"] == expected["actor"])
            .unwrap();
        let context = CallbackContext {
            agent: actor["id"].as_str().unwrap().to_owned(),
            account: "default".into(),
            connection: "connection".into(),
            generation: 7,
            epoch: actor["epoch"].as_u64().unwrap(),
            thread: actor["threadId"].as_str().unwrap().to_owned(),
            turn: actor["turnId"].as_str().unwrap().to_owned(),
        };
        if expected["remoteOrigin"] == true {
            store
                .transaction_with::<_, StoreError>(|tx| {
                    tx.execute(
                        "UPDATE runtime_agents SET record=json_set(record,'$.remoteOrigin','source-server') WHERE id=?1",
                        [context.agent.as_str()],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        let frame = call(
            &context,
            expected["callId"].as_str().unwrap(),
            expected["tool"].as_str().unwrap(),
            expected["arguments"].clone(),
        );
        let ports =
            crate::provider_callbacks::managed_tool_ports(&store, &context, &frame, wake.clone())
                .unwrap();
        let decision = dispatch_with_managed_tools(&store, &context, &frame, 20.0, &ports).unwrap();
        let CallbackDecision::Answer(reply) = decision else {
            panic!("managed callback must answer: {}", expected["tool"])
        };
        let ResponseBody::Result(actual) = reply.body else {
            panic!("managed callback result expected")
        };
        assert_eq!(
            actual["success"], expected["result"]["success"],
            "success for {}",
            expected["tool"]
        );
        let actual_text = actual["contentItems"][0]["text"].as_str().unwrap();
        let expected_text = expected["result"]["contentItems"][0]["text"]
            .as_str()
            .unwrap();
        if actual["success"] == true {
            assert_eq!(
                serde_json::from_str::<Value>(actual_text).unwrap(),
                serde_json::from_str::<Value>(expected_text).unwrap(),
                "tool value for {}",
                expected["tool"]
            );
        } else if expected["tool"] != "orchestration_send" || expected["result"]["success"] == true
        {
            assert_eq!(
                actual_text, expected_text,
                "tool refusal for {}",
                expected["tool"]
            );
        }
        let actual_rows = managed_snapshot(&store, &expected["rows"]);
        let differences = expected["rows"]
            .as_object()
            .unwrap()
            .iter()
            .filter_map(|(table, expected_rows)| {
                let actual_rows = actual_rows[table].as_array().unwrap();
                (actual_rows != expected_rows.as_array().unwrap()).then(|| {
                    let expected_rows = expected_rows.as_array().unwrap();
                    let mismatch = actual_rows
                        .iter()
                        .zip(expected_rows)
                        .find(|(actual, expected)| actual != expected);
                    let detail = mismatch.map(|(actual, expected)| {
                        if actual.as_array().is_some_and(|row| row.len() > 1)
                            && expected.as_array().is_some_and(|row| row.len() > 1)
                        {
                            let actual_row = actual.as_array().unwrap();
                            let expected_row = expected.as_array().unwrap();
                            if table == "sync_entities" {
                                return format!("actual={actual_row:?}; expected={expected_row:?}");
                            }
                            let fields = match (&actual_row[1], &expected_row[1]) {
                                (Value::Object(actual), Value::Object(expected)) => actual
                                    .keys()
                                    .chain(expected.keys())
                                    .collect::<std::collections::BTreeSet<_>>()
                                    .into_iter()
                                    .filter(|key| actual.get(*key) != expected.get(*key))
                                    .cloned()
                                    .collect::<Vec<_>>(),
                                _ => vec!["row-value".to_owned()],
                            };
                            format!("id={:?} fields={fields:?}", actual_row[0])
                        } else {
                            format!("actual={actual:?} expected={expected:?}")
                        }
                    });
                    format!("{table}: {detail:?}")
                })
            })
            .collect::<Vec<_>>();
        assert!(differences.is_empty(), "full rows differ: {differences:?}");
    }
}

#[test]
fn deleted_team_root_refuses_complaint_without_writes() {
    let (_root, store, _) = fixture();
    let oracle = &template().value;
    let expected = &oracle["managedRootDeletedRefusal"];
    assert_eq!(expected["result"]["success"], false);
    assert_eq!(expected["before"], expected["rows"]);
    let worker = oracle["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["id"] == expected["actor"])
        .unwrap();
    let root_id = worker["rootId"].as_str().unwrap();
    store
        .transaction_with::<_, StoreError>(|tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.deletedAt',20.0) WHERE id=?1",
                [root_id],
            )?;
            Ok(())
        })
        .unwrap();
    let before = application_table_snapshot(&store);
    let decision = dispatch_python_case(
        &store,
        oracle,
        expected,
        Some(expected["arguments"].clone()),
    )
    .unwrap();
    let CallbackDecision::Answer(reply) = decision else {
        panic!("deleted-root refusal must return a callback answer")
    };
    assert!(matches!(reply.body, ResponseBody::Result(ref value) if value["success"] == false));
    assert_eq!(before, application_table_snapshot(&store));
}

#[test]
fn managed_reads_preserve_corrupt_record_identity() {
    let (_root, store, context) = fixture();
    let monitor_id = template().value["managedToolCases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["tool"] == "orchestration_context")
        .unwrap()["arguments"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let search = call(
        &context,
        "corrupt-search",
        "orchestration_search",
        json!({"query":"fixture"}),
    );
    let search_ports = crate::provider_callbacks::managed_tool_ports(
        &store,
        &context,
        &search,
        std::sync::Arc::new(tokio::sync::Notify::new()),
    )
    .unwrap();
    // Prepare the search directory first; the corrupt row is deliberately
    // outside sync so the read reaches its stored-object validation path.
    store
        .transaction_with::<_, StoreError>(|tx| {
            tx.execute(
                "INSERT INTO runtime_work(id,record) VALUES('p05-corrupt-work',?1)",
                [json!({"id":"p05-corrupt-work","rootId":context.agent,"title":17,"status":"ready"}).to_string()],
            )?;
            Ok(())
        })
        .unwrap();
    let search_result = dispatch_with_managed_tools(&store, &context, &search, 20.0, &search_ports);
    assert!(
        matches!(
            &search_result,
            Err(CallbackError::Store(StoreError::InvalidStoredObject { table, id }))
                if *table == "runtime_work" && id == "p05-corrupt-work"
        ),
        "corrupt work result: {search_result:?}"
    );

    store
        .transaction_with::<_, StoreError>(|tx| {
            tx.execute("DELETE FROM runtime_work WHERE id='p05-corrupt-work'", [])?;
            tx.execute(
                "UPDATE runtime_monitors SET record=?1 WHERE id=?2",
                [
                    json!({"id":monitor_id,"agent":context.agent,"command":17}).to_string(),
                    monitor_id.clone(),
                ],
            )?;
            Ok(())
        })
        .unwrap();
    let context_call = call(
        &context,
        "corrupt-context",
        "orchestration_context",
        json!({"topic":"monitor","id":monitor_id}),
    );
    let context_ports = crate::provider_callbacks::managed_tool_ports(
        &store,
        &context,
        &context_call,
        std::sync::Arc::new(tokio::sync::Notify::new()),
    )
    .unwrap();
    let context_result =
        dispatch_with_managed_tools(&store, &context, &context_call, 20.0, &context_ports);
    assert!(
        matches!(
            &context_result,
        Err(CallbackError::Store(StoreError::InvalidStoredObject { table, id }))
            if *table == "runtime_monitors" && id == &monitor_id
        ),
        "corrupt monitor result: {context_result:?}"
    );
}

#[test]
fn managed_durable_writes_rollback_replay_once_and_reject_changed_content() {
    let oracle = &template().value;
    let cases = oracle["managedToolCases"].as_array().unwrap();
    let matrix = [
        (0, "runtime_events", "INSERT"),
        (0, "runtime_event_meta", "INSERT"),
        (1, "runtime_rooms", "INSERT"),
        (1, "runtime_chat_messages", "INSERT"),
        (5, "runtime_complaints", "INSERT"),
        (5, "runtime_events", "INSERT"),
        (6, "runtime_complaints", "UPDATE"),
        (7, "runtime_complaints", "UPDATE"),
        (7, "runtime_events", "UPDATE"),
        (7, "runtime_events", "INSERT"),
    ];
    for (case_index, table, operation) in matrix {
        for timing in ["BEFORE", "AFTER"] {
            let (_root, store, _) = fixture();
            for previous in &cases[..case_index] {
                assert!(matches!(
                    dispatch_python_case(&store, oracle, previous, None).unwrap(),
                    CallbackDecision::Answer(_)
                ));
            }
            let trigger = format!("p05_fail_{case_index}_{timing}_{operation}");
            store
                .transaction_with::<_, StoreError>(|tx| {
                    tx.execute_batch(&format!(
                        "CREATE TRIGGER {trigger} {timing} {operation} ON {table} BEGIN SELECT RAISE(ABORT,'injected {table} {timing} {operation}'); END"
                    ))?;
                    Ok(())
                })
                .unwrap();
            let before = application_table_snapshot(&store);
            let expected = &cases[case_index];
            let first_attempt = dispatch_python_case(&store, oracle, expected, None);
            let saved_failure = match first_attempt {
                Err(_) => None,
                Ok(CallbackDecision::Answer(reply)) => {
                    assert!(
                        matches!(&reply.body, ResponseBody::Result(result) if result["success"] == false),
                        "{} {timing} {operation} on {table} unexpectedly succeeded",
                        expected["tool"]
                    );
                    Some(reply.body)
                }
                Ok(other) => panic!("unexpected failure decision: {other:?}"),
            };
            store
                .transaction_with::<_, StoreError>(|tx| {
                    tx.execute_batch(&format!("DROP TRIGGER {trigger}"))?;
                    Ok(())
                })
                .unwrap();
            assert_eq!(
                before,
                application_table_snapshot(&store),
                "partial write: {table}"
            );
            let first = match dispatch_python_case(&store, oracle, expected, None).unwrap() {
                CallbackDecision::Answer(reply) => reply,
                _ => panic!("retry must answer"),
            };
            if let Some(saved) = saved_failure {
                assert_eq!(first.body, saved, "failed reply is durable for {table}");
                assert_eq!(
                    before,
                    application_table_snapshot(&store),
                    "failed operation wrote: {table}"
                );
            }
            let committed = application_table_snapshot(&store);
            let replay = dispatch_python_case(&store, oracle, expected, None).unwrap();
            let CallbackDecision::Answer(replay) = replay else {
                panic!("replay must return saved answer")
            };
            assert_eq!(first.body, replay.body, "saved answer for {table}");
            assert_eq!(
                committed,
                application_table_snapshot(&store),
                "duplicate effect: {table}"
            );
            let mut changed = expected["arguments"].clone();
            if let Some(text) = changed["text"].as_str() {
                changed["text"] = json!(format!("{text} changed"));
            } else if let Some(id) = changed["monitor_id"].as_str() {
                changed["monitor_id"] = json!(format!("{id}-changed"));
            } else {
                changed["action"] = json!("changed");
            }
            assert!(dispatch_python_case(&store, oracle, expected, Some(changed)).is_err());
            assert_eq!(
                committed,
                application_table_snapshot(&store),
                "changed identity wrote: {table}"
            );
        }
    }
}

#[test]
fn startup_reconciles_reserved_callback_once() {
    let (root, store, _) = fixture();
    let oracle = &template().value;
    let expected = &oracle["managedToolCases"][0];
    store
        .transaction_with::<_, StoreError>(|tx| {
            tx.execute("DELETE FROM runtime_requests", [])?;
            tx.execute("DELETE FROM runtime_operation_receipts", [])?;
            tx.execute_batch("CREATE TRIGGER p05_crash_after_reservation BEFORE INSERT ON runtime_tool_results BEGIN SELECT RAISE(ABORT,'crash after reservation'); END")?;
            Ok(())
        })
        .unwrap();
    assert!(dispatch_python_case(&store, oracle, expected, None).is_err());
    store
        .transaction_with::<_, StoreError>(|tx| {
            tx.execute_batch("DROP TRIGGER p05_crash_after_reservation")?;
            Ok(())
        })
        .unwrap();
    let queued: i64 = store
        .read()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM runtime_tool_requests WHERE json_extract(record,'$.tool')='orchestration_send' AND json_extract(record,'$.stage')='queued'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(queued, 1, "the admission transaction survives the crash");
    drop(store);

    // Reopen the real store and invoke the callback owner's startup recovery.
    let restarted = RuntimeStore::open(Some(root.path())).unwrap();
    assert_eq!(
        crate::provider_callbacks::recover_connection(&restarted, "default", "connection", 30.0)
            .unwrap(),
        1
    );
    assert_eq!(
        crate::provider_callbacks::recover_connection(&restarted, "default", "connection", 31.0)
            .unwrap(),
        0,
        "a second startup pass must not reconcile the same reservation twice"
    );
    let (stage, outcome): (String, String) = restarted
        .read()
        .unwrap()
        .query_row(
            "SELECT json_extract(record,'$.stage'),json_extract(record,'$.outcome') FROM runtime_tool_requests WHERE id LIKE '%:send:%'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (stage.as_str(), outcome.as_str()),
        ("cancelled", "not_applied")
    );
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
    // The plan topic has a local transactional projection; the other nine
    // operations stage through the private adapter bundle.
    assert_eq!(ports.staged.load(Ordering::SeqCst), 9);
    assert_eq!(ports.released.load(Ordering::SeqCst), 9);
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
    assert_eq!(ports.staged.load(Ordering::SeqCst), 1);
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
fn remote_origin_search_and_monitor_context_stay_local() {
    let (_root, store, context) = fixture();
    let monitor_id = template().value["managedToolCases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["tool"] == "orchestration_context")
        .unwrap()["arguments"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    store
        .transaction_with::<_, StoreError>(|tx| {
            tx.execute(
                "UPDATE runtime_monitors SET record=?1 WHERE id=?2",
                [json!({"id":monitor_id,"agent":context.agent,"command":"printf fixture","created":20.0,"status":"running"}).to_string(), monitor_id.clone()],
            )?;
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
    for (rpc, tool, args) in [
        (
            "remote-search",
            "orchestration_search",
            json!({"query":"fixture"}),
        ),
        (
            "remote-monitor-context",
            "orchestration_context",
            json!({"topic":"monitor","id":monitor_id}),
        ),
    ] {
        let frame = call(&context, rpc, tool, args);
        let ports = crate::provider_callbacks::managed_tool_ports(
            &store,
            &context,
            &frame,
            std::sync::Arc::new(tokio::sync::Notify::new()),
        )
        .unwrap();
        let CallbackDecision::Answer(reply) =
            dispatch_with_managed_tools(&store, &context, &frame, 20.0, &ports).unwrap()
        else {
            panic!("remote-origin local read must answer: {tool}")
        };
        assert!(
            matches!(reply.body, ResponseBody::Result(ref value) if value["success"] == true),
            "{tool}: {:?}",
            reply.body
        );
    }
}

#[test]
fn queued_managed_tool_request_can_be_cancelled_before_dispatch() {
    let (_root, store, context) = fixture();
    let mut queued = template().value["reservations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|reservation| &reservation["record"])
        .find(|record| record["tool"] == "orchestration_spawn")
        .unwrap()
        .clone();
    let request_id = "thread-0:spawn:p05-cancel";
    queued["id"] = json!(request_id);
    queued["request_id"] = json!("p05-cancel");
    store
        .transaction_with(|tx| {
            tx.execute(
                "INSERT INTO runtime_tool_requests(id,record) VALUES(?1,?2)",
                params![request_id, queued.to_string()],
            )?;
            let stored =
                studio_runtime_store::system_reads::get(tx, "runtime_tool_requests", request_id)?;
            assert_eq!(stored.as_ref(), Some(&queued));
            let result = crate::tool_requests::execute(
                tx,
                &context.agent,
                crate::tool_requests::Action::Cancel(request_id),
                21.0,
            )?;
            Ok::<_, CallbackError>(result)
        })
        .unwrap();
    let connection = store.read().unwrap();
    let cancelled =
        studio_runtime_store::system_reads::get(&connection, "runtime_tool_requests", request_id)
            .unwrap()
            .unwrap();
    assert_eq!(cancelled["stage"], "cancelled");
    assert_eq!(cancelled["cancelRequested"], true);
    assert_eq!(intent_count(&store, "spawn"), 0);
}
