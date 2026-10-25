#![allow(clippy::unwrap_used, dead_code)]
#[path = "../recovery/frozen.rs"]
pub(crate) mod frozen;

use serde_json::{Value, json};
use std::sync::{Arc, Mutex, OnceLock};
use studio_runtime_providers::{Frame, Method, RequestFrame, RequestId};
use studio_runtime_services::{callback_runtime::*, provider_callbacks::*};
use studio_runtime_store::RuntimeStore;
pub fn template() -> &'static Value {
    static VALUE: OnceLock<Value> = OnceLock::new();
    VALUE.get_or_init(|| {
        serde_json::from_str(include_str!("expected.json")).expect("frozen callback expectations")
    })
}

pub async fn fixture() -> (tempfile::TempDir, Arc<RuntimeStore>, CallbackContext) {
    let data = template();
    let root = tempfile::tempdir().unwrap();
    frozen::state(
        root.path(),
        include_str!("canvas.sqlite3.sql"),
        include_str!("analytics.sqlite3.sql"),
    );
    // Public projections bind the same frozen fixture server identity.
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
    let identity = root.path().join("multi-server/identity.json");
    std::fs::write(&identity, include_str!("identity.json")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&identity, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
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
pub async fn card(store: &RuntimeStore, value: &Value) {
    let id = value["id"].as_str().unwrap().to_owned();
    let record = value.to_string();
    store
        .transaction_with::<_, studio_runtime_store::StoreError>(move |tx| {
            studio_runtime_store::QuestionsWriteRepository::put_request(tx, &id, &record)
        })
        .await
        .unwrap();
}
pub fn stored(store: &RuntimeStore, id: &str) -> Value {
    let raw: String = store
        .test_reader()
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
    fn has_live_session(&self, _: &CallbackContext) -> bool {
        true
    }

    fn deliver<'a>(
        &'a self,
        reply: &'a Reply,
        deadline: std::time::Duration,
    ) -> CallbackReplyFuture<'a> {
        Box::pin(async move {
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
pub async fn pending(store: &Arc<RuntimeStore>, context: &CallbackContext) -> (String, Frame) {
    let frame = request(context);
    let decision = dispatch(Arc::clone(store), context, &frame, 10.0)
        .await
        .unwrap();
    bind_journal(store, &decision, context, &RequestId::Number(42), 1)
        .await
        .unwrap();
    let CallbackDecision::Pending(id) = decision else {
        panic!("pending")
    };
    (id, frame)
}
pub async fn checkpoint(store: &RuntimeStore, context: &CallbackContext, cursor: u64) -> usize {
    let account = context.account.clone();
    let generation = context.generation;
    store.transaction_with::<_, CallbackError>(move |tx| {
        tx.execute(
            "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'ack',?2) ON CONFLICT(id) DO UPDATE SET result=excluded.result",
            rusqlite::params![format!("account-worker:cursor:{account}:{generation}:ack"),cursor.to_string()],
        )?;
        retire_checkpointed(tx, &account, generation)
    })
    .await
    .unwrap()
}

pub fn actor(store: &RuntimeStore, context: &CallbackContext) -> Value {
    studio_runtime_store::agent_volatile::merged_record(
        &store.test_reader().unwrap(),
        &context.agent,
    )
    .unwrap()
    .unwrap()
}
pub async fn put_actor(store: &RuntimeStore, value: &Value) {
    let id = value["id"].as_str().unwrap().to_owned();
    let record = value.to_string();
    store
        .transaction_with::<_, studio_runtime_store::StoreError>(move |tx| {
            let record: Value = serde_json::from_str(&record).expect("actor JSON");
            studio_runtime_store::agent_volatile::update(tx, &id, &record)?;
            Ok(())
        })
        .await
        .unwrap();
}
pub async fn ack(store: &RuntimeStore, context: &CallbackContext, cursor: u64) {
    let account = context.account.clone();
    let generation = context.generation;
    store.transaction_with::<_, CallbackError>(move |tx| {
        tx.execute(
            "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'ack',?2) ON CONFLICT(id) DO UPDATE SET result=excluded.result",
            rusqlite::params![format!("account-worker:cursor:{account}:{generation}:ack"),cursor.to_string()],
        )?;
        Ok(())
    })
    .await
    .unwrap();
}

pub fn logical_snapshot(store: &RuntimeStore) -> Value {
    snapshot_rows(store, true)
}
pub fn physical_snapshot(store: &RuntimeStore) -> Value {
    snapshot_rows(store, false)
}
fn snapshot_rows(store: &RuntimeStore, logical: bool) -> Value {
    use rusqlite::types::ValueRef;
    let connection = store.test_reader().unwrap();
    super::agent_view_fixtures::assert_agent_view_projections(&connection);
    let mut names=connection.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").unwrap();
    let tables = names
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let mut result = serde_json::Map::new();
    for name in tables {
        // Logical rows show each agent with its volatile fields (format
        // 2026-10-20), as a Python state stores it.
        if logical
            && (name == "runtime_operation_receipts"
                || name == studio_runtime_store::agent_volatile::AGENT_VOLATILE_TABLE
                || (name.starts_with("sync_") && name != "sync_entities"))
        {
            continue;
        }
        let sql = if logical && name == "sync_entities" {
            "SELECT collection,id,payload,deleted FROM sync_entities WHERE collection NOT IN ('agentSummary','agentDetail')".to_owned()
        } else {
            studio_runtime_store::agent_columns::select_declared_sql(&connection, &name).unwrap()
        };
        let mut statement = connection.prepare(&sql).unwrap();
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
        let mut rows = rows;
        if logical && name == "runtime_agents" {
            for row in &mut rows {
                let id = row[0].as_str().unwrap().to_owned();
                studio_runtime_store::agent_volatile::merge(&connection, &id, &mut row[1]).unwrap();
            }
        }
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

pub async fn seed_admission(store: &RuntimeStore, before: &Value) {
    // Current Rust cards always carry a durable admission receipt. Keep the
    // frozen failure corpus focused on answer delivery rather than Python cutover.
    let context = CallbackContext {
        agent: before["agent"].as_str().unwrap().into(),
        account: before["accountKey"].as_str().unwrap().into(),
        connection: before["connectionId"].as_str().unwrap().into(),
        generation: before["supervisor"]["generation"].as_u64().unwrap(),
        epoch: before["epoch"].as_u64().unwrap(),
        thread: before["params"]["threadId"].as_str().unwrap().into(),
        turn: before["params"]["turnId"]
            .as_str()
            .unwrap_or_default()
            .into(),
    };
    let identity = ReplyIdentity {
        context,
        rpc_id: serde_json::from_value(before["rpcId"].clone()).unwrap(),
        attempt: String::new(),
    };
    let admission_key = format!("{}card", identity.operation_id());
    let admission = json!({"identity":identity,"card":before["id"],"journalCursor":1});
    store.transaction(move |tx| {
        tx.execute("INSERT OR REPLACE INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'provider-callback',?2)", rusqlite::params![admission_key, admission.to_string()])?;
        Ok(())
    }).await.unwrap();
}

/// Import the exact Python-observed before rows for each logical write. The
/// template contains all corpus identities; each case restores its own baseline.
pub async fn seed_case(store: &RuntimeStore, case: &Value) {
    put_actor(store, &case["actor"]).await;
    card(store, &case["before"]).await;
    seed_admission(store, &case["before"]).await;
    let case = case.clone();
    store
        .transaction_with::<_, studio_runtime_store::StoreError>(move |tx| {
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
                            if row[0] == "agent" {
                                // This corpus explicitly installs a historical view. Seed
                                // both current projections from that same view, too.
                                for collection in ["agentSummary", "agentDetail"] {
                                    let value = studio_runtime_services::sync_policy::project_persisted_record(
                                        collection, &row[2]["value"],
                                    ).expect("frozen agent view has id");
                                    studio_runtime_store::SyncDocumentsRepository::put_entity(
                                        tx, collection, row[1].as_str().unwrap(), value, row[3] == 1,
                                    )?;
                                }
                            }
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
        .await
        .unwrap();
}
pub async fn retire(store: &RuntimeStore, context: &CallbackContext) -> usize {
    let account = context.account.clone();
    let generation = context.generation;
    store
        .transaction_with(move |tx| retire_checkpointed(tx, &account, generation))
        .await
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
