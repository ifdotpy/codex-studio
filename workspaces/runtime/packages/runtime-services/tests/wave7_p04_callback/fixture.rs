#![allow(clippy::unwrap_used, dead_code)]
#[path = "../support/prepared_python.rs"]
mod prepared_python;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex, OnceLock},
};
use studio_runtime_providers::{Frame, Method, RequestFrame, RequestId};
use studio_runtime_services::{callback_runtime::*, provider_callbacks::*};
use studio_runtime_store::RuntimeStore;
pub fn template() -> &'static Value {
    static VALUE: OnceLock<Value> = OnceLock::new();
    VALUE.get_or_init(|| {
        let root = tempfile::tempdir().unwrap().keep();
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let script =
            source.join("../../../packages/runtime-services/tests/wave7_p04_callback/oracle.py");
        let output = Command::new(prepared_python::prepared_python(&source))
            .arg(script)
            .arg(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    })
}
pub fn fixture() -> (tempfile::TempDir, Arc<RuntimeStore>, CallbackContext) {
    let data = template();
    let root = tempfile::tempdir().unwrap();
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(
            PathBuf::from(data["template"].as_str().unwrap()).join(name),
            root.path().join(name),
        )
        .unwrap();
    }
    // Public projections bind the same generated fixture server identity.
    std::fs::create_dir(root.path().join("multi-server")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            root.path().join("multi-server"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    std::fs::copy(
        PathBuf::from(data["template"].as_str().unwrap()).join("multi-server/identity.json"),
        root.path().join("multi-server/identity.json"),
    )
    .unwrap();
    let store = Arc::new(RuntimeStore::open(Some(root.path())).unwrap());
    let context = CallbackContext {
        agent: data["agents"][0]["id"].as_str().unwrap().into(),
        account: "default".into(),
        connection: "connection".into(),
        generation: 7,
        epoch: 3,
        thread: "thread-0".into(),
        turn: "turn-0".into(),
    };
    (root, store, context)
}
pub fn card(store: &RuntimeStore, value: &Value) {
    store
        .transaction_with::<_, studio_runtime_store::StoreError>(|tx| {
            studio_runtime_store::QuestionsWriteRepository::put_request(
                tx,
                value["id"].as_str().unwrap(),
                &value.to_string(),
            )
        })
        .unwrap();
}
pub fn stored(store: &RuntimeStore, id: &str) -> Value {
    let raw: String = store
        .read()
        .unwrap()
        .query_row(
            "SELECT record FROM runtime_requests WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    serde_json::from_str(&raw).unwrap()
}
#[derive(Default)]
pub struct Port {
    pub calls: Mutex<Vec<Reply>>,
    pub next: Mutex<Option<ReplyOutcome>>,
    pub foreign: Mutex<bool>,
}
impl CallbackReplyPort for Port {
    fn deliver(
        &self,
        reply: &Reply,
        deadline: std::time::Duration,
    ) -> Result<ReplyReceipt, CallbackError> {
        assert_eq!(deadline, REPLY_DEADLINE);
        self.calls.lock().unwrap().push(reply.clone());
        let mut identity = reply.identity.clone();
        if *self.foreign.lock().unwrap() {
            identity.attempt = "foreign-attempt".into();
        }
        Ok(ReplyReceipt {
            identity,
            outcome: self
                .next
                .lock()
                .unwrap()
                .take()
                .unwrap_or(ReplyOutcome::Written),
        })
    }
}
pub fn request(context: &CallbackContext) -> Frame {
    Frame::Request(RequestFrame {
        id: RequestId::Number(42),
        method: Method::ItemToolRequestUserInput,
        params: Some(
            json!({"threadId":context.thread,"turnId":context.turn,"questions":[{"id":"secret","question":"Value?","isSecret":true}]}),
        ),
        fields: Default::default(),
    })
}
pub fn pending(store: &RuntimeStore, context: &CallbackContext) -> (String, Frame) {
    let frame = request(context);
    let decision = dispatch(store, context, &frame, 10.0).unwrap();
    bind_journal(store, &decision, context, &RequestId::Number(42), 1).unwrap();
    let CallbackDecision::Pending(id) = decision else {
        panic!("pending")
    };
    (id, frame)
}
pub fn checkpoint(store: &RuntimeStore, context: &CallbackContext, cursor: u64) -> usize {
    store.transaction_with::<_,CallbackError>(|tx| {
        tx.execute("INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'ack',?2) ON CONFLICT(id) DO UPDATE SET result=excluded.result",rusqlite::params![format!("account-worker:cursor:{}:{}:ack",context.account,context.generation),cursor.to_string()])?;
        retire_checkpointed(tx,&context.account,context.generation)
    }).unwrap()
}

pub fn actor(store: &RuntimeStore, context: &CallbackContext) -> Value {
    let raw: String = store
        .read()
        .unwrap()
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [&context.agent],
            |r| r.get(0),
        )
        .unwrap();
    serde_json::from_str(&raw).unwrap()
}
pub fn put_actor(store: &RuntimeStore, value: &Value) {
    store
        .transaction_with::<_, studio_runtime_store::StoreError>(|tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=?2 WHERE id=?1",
                rusqlite::params![value["id"].as_str().unwrap(), value.to_string()],
            )?;
            Ok(())
        })
        .unwrap();
}
pub fn ack(store: &RuntimeStore, context: &CallbackContext, cursor: u64) {
    store.transaction_with::<_,CallbackError>(|tx| {
        tx.execute("INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'ack',?2) ON CONFLICT(id) DO UPDATE SET result=excluded.result",rusqlite::params![format!("account-worker:cursor:{}:{}:ack",context.account,context.generation),cursor.to_string()])?; Ok(())
    }).unwrap();
}

pub fn logical_snapshot(store: &RuntimeStore) -> Value {
    snapshot_rows(store, true)
}
pub fn physical_snapshot(store: &RuntimeStore) -> Value {
    snapshot_rows(store, false)
}
fn snapshot_rows(store: &RuntimeStore, logical: bool) -> Value {
    use rusqlite::types::ValueRef;
    let connection = store.read().unwrap();
    let mut names=connection.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").unwrap();
    let tables = names
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let mut result = serde_json::Map::new();
    for name in tables {
        if logical
            && (name == "runtime_operation_receipts"
                || (name.starts_with("sync_") && name != "sync_entities"))
        {
            continue;
        }
        let columns = if logical && name == "sync_entities" {
            "collection,id,payload,deleted"
        } else {
            "*"
        };
        let mut statement = connection
            .prepare(&format!(
                "SELECT {columns} FROM \"{}\"",
                name.replace('"', "\"\"")
            ))
            .unwrap();
        let count = statement.column_count();
        let rows=statement.query_map([],|row| {
            let mut values=Vec::new();
            for i in 0..count {
                values.push(match row.get_ref(i)? {
                    ValueRef::Null=>Value::Null,ValueRef::Integer(v)=>json!(v),ValueRef::Real(v)=>json!(v),
                    ValueRef::Text(v)=>{let s=std::str::from_utf8(v).unwrap();serde_json::from_str(s).unwrap_or(json!(s))},
                    ValueRef::Blob(v)=>json!({"blobHex":v.iter().map(|b|format!("{b:02x}")).collect::<String>()}),
                });
            }
            Ok(json!(values))
        }).unwrap().collect::<Result<Vec<_>,_>>().unwrap();
        result.insert(name, json!(rows));
    }
    Value::Object(result)
}
pub fn logical_changes(before: &Value, after: &Value) -> Value {
    use std::collections::BTreeMap;
    let mut result = serde_json::Map::new();
    for (table, rows) in before.as_object().unwrap() {
        if table == "runtime_agent_record_generation" {
            let delta = after[table][0][1].as_i64().unwrap() - rows[0][1].as_i64().unwrap();
            if delta != 0 {
                result.insert(table.clone(), json!({"increment":delta}));
            }
            continue;
        }
        let old: BTreeMap<_, _> = rows
            .as_array()
            .unwrap()
            .iter()
            .map(|v| (v.to_string(), v))
            .collect();
        let new: BTreeMap<_, _> = after[table]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| (v.to_string(), v))
            .collect();
        let removed: Vec<_> = old
            .iter()
            .filter(|(key, _)| !new.contains_key(*key))
            .map(|(_, v)| *v)
            .collect();
        let added: Vec<_> = new
            .iter()
            .filter(|(key, _)| !old.contains_key(*key))
            .map(|(_, v)| *v)
            .collect();
        if !removed.is_empty() || !added.is_empty() {
            result.insert(table.clone(), json!({"removed":removed,"added":added}));
        }
    }
    Value::Object(result)
}
pub fn assert_writes(before: &Value, store: &RuntimeStore, expected: &Value) {
    let mut actual = logical_changes(before, &logical_snapshot(store));
    let mut expected = expected.clone();
    for root in [&mut actual, &mut expected] {
        for (_, change) in root.as_object_mut().unwrap() {
            if change.get("increment").is_some() {
                continue;
            }
            for key in ["removed", "added"] {
                change[key]
                    .as_array_mut()
                    .unwrap()
                    .sort_by_key(Value::to_string);
            }
        }
    }
    if actual != expected {
        panic!("{}", first_difference(&actual, &expected, "writes"));
    }
}

/// Import the exact Python-observed before rows for each logical write. The
/// template contains all corpus identities; each case restores its own baseline.
pub fn seed_case(store: &RuntimeStore, case: &Value) {
    put_actor(store, &case["actor"]);
    card(store, &case["before"]);
    store
        .transaction_with::<_, studio_runtime_store::StoreError>(|tx| {
            if let Some(writes) = case["writes"].as_object() {
                for (table, change) in writes {
                    if change.get("increment").is_some() {
                        continue;
                    }
                    for row in change["removed"].as_array().unwrap() {
                        if table == "sync_entities" {
                            studio_runtime_store::SyncDocumentsRepository::put_entity(
                                tx,
                                row[0].as_str().unwrap(),
                                row[1].as_str().unwrap(),
                                row[2]["value"].clone(),
                                row[3] == 1,
                            )?;
                        } else {
                            insert_row(tx, table, row)?;
                        }
                    }
                }
            }
            for (table, rows) in case["counters"].as_object().unwrap() {
                tx.execute(&format!("DELETE FROM {table}"), [])?;
                for row in rows.as_array().unwrap() {
                    insert_row(tx, table, row)?;
                }
            }
            Ok(())
        })
        .unwrap();
}
pub fn retire(store: &RuntimeStore, context: &CallbackContext) -> usize {
    store
        .transaction_with(|tx| retire_checkpointed(tx, &context.account, context.generation))
        .unwrap()
}

fn first_difference(actual: &Value, expected: &Value, path: &str) -> String {
    if let (Some(a), Some(e)) = (actual.as_object(), expected.as_object()) {
        for key in a.keys().chain(e.keys()) {
            if actual[key] != expected[key] {
                return first_difference(&actual[key], &expected[key], &format!("{path}/{key}"));
            }
        }
    }
    if let (Some(a), Some(e)) = (actual.as_array(), expected.as_array()) {
        if a.len() != e.len() {
            return format!("{path} lengths actual={} expected={}", a.len(), e.len());
        }
        for (i, (a, e)) in a.iter().zip(e).enumerate() {
            if a != e {
                return first_difference(a, e, &format!("{path}/{i}"));
            }
        }
    }
    format!("{path}: actual={actual}, expected={expected}")
}

fn insert_row(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    row: &Value,
) -> Result<(), studio_runtime_store::StoreError> {
    let values: Vec<rusqlite::types::Value> = row
        .as_array()
        .unwrap()
        .iter()
        .map(|v| match v {
            Value::Null => rusqlite::types::Value::Null,
            Value::Number(n) => {
                if let Some(v) = n.as_i64() {
                    rusqlite::types::Value::Integer(v)
                } else {
                    rusqlite::types::Value::Real(n.as_f64().unwrap())
                }
            }
            Value::String(s) => rusqlite::types::Value::Text(s.clone()),
            _ => rusqlite::types::Value::Text(v.to_string()),
        })
        .collect();
    let slots = vec!["?"; values.len()].join(",");
    tx.execute(
        &format!(
            "INSERT OR REPLACE INTO \"{}\" VALUES({slots})",
            table.replace('"', "\"\"")
        ),
        rusqlite::params_from_iter(values),
    )?;
    Ok(())
}
