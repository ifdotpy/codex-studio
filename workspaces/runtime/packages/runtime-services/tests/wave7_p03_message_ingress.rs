//! Message ingress at its transaction boundary, using frozen historical state.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::{Value, json};
use studio_runtime_services::coordination::{self, PostManagedMessage};
use studio_runtime_services::message_admission::PreparedAdmission;
use studio_runtime_store::RuntimeStore;

/// Marker in the historical SQL and JSON, rebound to this fixture's real workspace.
const WORKSPACE_MARKER: &str = "__INGRESS_WORKSPACE__";

fn request_id() -> String {
    studio_runtime_schema::persistence::RequestIdentity::issue_root()
}

fn normalize_id(value: &mut Value, id: &str) {
    match value {
        Value::String(text) if text == id => *text = "message".into(),
        Value::Array(rows) => rows.iter_mut().for_each(|row| normalize_id(row, id)),
        Value::Object(fields) => {
            if let Some(value) = fields.remove(id) {
                fields.insert("message".into(), value);
            }
            for value in fields.values_mut() {
                normalize_id(value, id);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Expected {
    results: Value,
    state: BTreeMap<String, Value>,
    operation_receipts: Value,
    receipts: Value,
}

struct Fixture {
    root: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("isolated root");
        std::fs::create_dir(root.path().join("workspace")).expect("workspace");
        Self { root }
    }
    fn sql(&self, source: &str) -> String {
        source.replace(
            WORKSPACE_MARKER,
            &self
                .root
                .path()
                .join("workspace")
                .to_string_lossy()
                .replace('\'', "''"),
        )
    }
    fn seed(&self, name: &str) -> PathBuf {
        let root = self.root.path().join(name);
        crate::recovery_fixtures::state(
            &root,
            &self.sql(include_str!("wave7_p03_message_ingress/canvas.sqlite3.sql")),
            include_str!("wave7_p03_message_ingress/analytics.sqlite3.sql"),
        );
        let db = rusqlite::Connection::open(root.join("canvas.sqlite3")).expect("seed reader");
        let hashes: Vec<(String, String, String)> = db
            .prepare("SELECT collection,id,payload FROM sync_entities WHERE payload IS NOT NULL")
            .expect("seed hashes")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("seed payloads")
            .collect::<Result<_, _>>()
            .expect("seed rows");
        for (collection, id, payload) in hashes {
            db.execute(
                "UPDATE sync_entities SET hash=?1 WHERE collection=?2 AND id=?3",
                rusqlite::params![
                    studio_runtime_store::sha256_hex(payload.as_bytes()),
                    collection,
                    id
                ],
            )
            .expect("rebind seed hash");
        }
        drop(db);
        let identity_root = root.join("multi-server");
        std::fs::create_dir(&identity_root).expect("identity directory");
        let identity = identity_root.join("identity.json");
        std::fs::write(
            &identity,
            include_str!("wave7_p03_message_ingress/identity.json"),
        )
        .expect("frozen server identity");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&identity_root, std::fs::Permissions::from_mode(0o700))
                .expect("private identity directory");
            std::fs::set_permissions(&identity, std::fs::Permissions::from_mode(0o600))
                .expect("private identity file");
        }
        root
    }
    fn expected(&self, name: &str) -> Expected {
        let workspace =
            serde_json::to_string(&self.root.path().join("workspace")).expect("workspace JSON");
        let source = include_str!("wave7_p03_message_ingress/expected.json")
            .replace(WORKSPACE_MARKER, &workspace[1..workspace.len() - 1]);
        let mut cases: BTreeMap<String, Expected> =
            serde_json::from_str(&source).expect("frozen ingress expectations");
        cases.remove(name).expect("named ingress expectation")
    }
    async fn configure(&self, store: &RuntimeStore, case: &str) {
        let source = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/wave7_p03_message_ingress")
                .join(format!("{case}.sql")),
        )
        .expect("frozen live case configuration");
        let sql = self.sql(&source).replace(
            "INSERT INTO \"runtime_agents\" VALUES",
            "INSERT INTO \"runtime_agents\"(id,record) VALUES",
        );
        store.transaction(move |tx| {
            tx.execute_batch(&sql)?;
            tx.execute("UPDATE sync_entities SET hash=sync_entity_hash(payload) WHERE payload IS NOT NULL", [])?;
            studio_runtime_store::migrations::migrate_2026_10_20(tx)
        }).await.expect("configure ingress case");
    }
}

async fn send(
    store: &RuntimeStore,
    step: &Value,
    request_id: &str,
) -> Result<coordination::PostMessageResult, coordination::CoordinationError> {
    let room = step.get("room").and_then(Value::as_str).unwrap_or("agent");
    let prepared = PreparedAdmission::read(&store.test_reader().expect("read"), room)
        .expect("SQL admission inputs")
        .prepare()
        .await?;
    if let Some(operation) = step
        .get("startOperation")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        // A separate writer wins after filesystem preparation, before admission.
        let room_for_write = room.to_owned();
        store
            .transaction(move |tx| {
                let mut agent =
                    studio_runtime_store::AgentRepository::get(tx, &room_for_write.into())?
                        .expect("agent");
                agent
                    .extra
                    .insert("workspaceOperation".into(), json!(operation));
                studio_runtime_store::AgentRepository::put(tx, &agent)?;
                studio_runtime_services::agents_create::publish_agent(
                    tx,
                    &serde_json::to_value(agent)?,
                )?;
                Ok(())
            })
            .await?;
    }
    let step = step.clone();
    let request_id = request_id.to_owned();
    store
        .transaction_with_error(move |tx| post(tx, &step, &prepared, &request_id))
        .await
}

fn post(
    tx: &studio_runtime_store::Transaction<'_>,
    step: &Value,
    prepared: &PreparedAdmission,
    request_id: &str,
) -> Result<coordination::PostMessageResult, coordination::CoordinationError> {
    let assets = step
        .get("assets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    coordination::post_message_with_admission(
        tx,
        PostManagedMessage {
            room: step.get("room").and_then(Value::as_str).unwrap_or("agent"),
            id: request_id,
            text: step
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or(" hello "),
            delivery: studio_runtime_schema::delivery::DeliveryMoment::for_event(
                "user",
                "",
                Some(step),
            )
            .expect("fixture delivery"),
            assets: &assets,
        },
        prepared,
    )
}

#[tokio::test]
async fn python_send_and_receipt_corpus() {
    let fixture = Fixture::new();
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("wave7_p03_message_ingress/cases.json")).expect("cases");
    for mut case in cases {
        // This corpus starts after HTTP request validation. Exercise its
        // largest valid attachment list; the shared request-schema and public
        // send contracts cover rejection above the same owner budget.
        for step in case["steps"].as_array_mut().expect("steps") {
            if step["assetsAtTurnLimit"] == true {
                step.as_object_mut()
                    .expect("step")
                    .remove("assetsAtTurnLimit");
                step["assets"] = json!(
                    (0..studio_runtime_schema::shared_values::MAX_TURN_ATTACHMENTS)
                        .map(|index| format!("asset-{index}"))
                        .collect::<Vec<_>>()
                );
            }
        }
        let name = case["name"].as_str().expect("case name");
        let root = fixture.seed(name);
        let expected = fixture.expected(name);
        let store = RuntimeStore::open(Some(&root))
            .await
            .unwrap_or_else(|error| panic!("{name}: frozen state opens: {error}"));
        let request_id = request_id();
        fixture.configure(&store, name).await;
        let mut results = Vec::new();
        for step in case["steps"].as_array().expect("steps") {
            if let Some(status) = step.get("status") {
                let status = status.as_str().map(str::to_owned);
                let error = step.get("error").and_then(Value::as_str).map(str::to_owned);
                let id = request_id.clone();
                store
                    .transaction(move |tx| {
                        tx.execute(
                            "UPDATE runtime_events SET status=?1,error=?2 WHERE id=?3",
                            rusqlite::params![status, error, id],
                        )?;
                        Ok(())
                    })
                    .await
                    .expect("advance event");
            } else {
                results.push(match send(&store, step, &request_id).await {
                    Ok(posted) => posted.receipt,
                    Err(coordination::CoordinationError::RequestResult(
                        studio_runtime_store::request_results::RequestResultError::Conflict {
                            ..
                        },
                    )) => json!({"error":"This message id has different content"}),
                    Err(error) => json!({"error":error.to_string()}),
                });
            }
        }
        let mut results = json!(results);
        normalize_id(&mut results, &request_id);
        let mut expected_results = case.get("rustResults").unwrap_or(&expected.results).clone();
        if let Some(replies) = expected_results.as_array_mut()
            && let Some(first) = replies.first().cloned()
        {
            for reply in replies.iter_mut().skip(1) {
                if reply.get("id") == Some(&json!("message")) && reply.get("status").is_some() {
                    *reply = first.clone();
                }
            }
        }
        assert_eq!(
            results, expected_results,
            "{name}: complete send results (existing account-name redaction is explicit)"
        );
        let receipt_id = request_id.clone();
        let mut receipts = store
            .transaction_with_error(move |tx| {
                studio_runtime_services::coordination_reads::delivery_receipts(
                    tx,
                    "agent",
                    &[receipt_id.clone(), "missing".into(), receipt_id],
                )
            })
            .await
            .unwrap_or_else(|error| json!({"error":error.to_string()}));
        normalize_id(&mut receipts, &request_id);
        assert_eq!(receipts, expected.receipts, "{name}: complete receipts");
        let actual = logical_state(&store);
        let key = studio_runtime_schema::persistence::RequestIdentity::new(
            "message",
            &["agent", "user"],
            &request_id,
            &[],
        )
        .expect("message identity");
        let saved: Option<(String, String)> = store
            .test_reader()
            .expect("reader")
            .query_row(
                "SELECT signature,result FROM request_results WHERE id=?1",
                [key.id()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .expect("saved result");
        let old = expected
            .operation_receipts
            .as_array()
            .expect("legacy receipts");
        assert_eq!(
            usize::from(saved.is_some()),
            old.len(),
            "{name}: saved receipt count"
        );
        if let Some((signature, raw)) = saved {
            assert_eq!(
                signature,
                studio_runtime_services::event_write::python_json(&old[0]["signature"]),
                "{name}: exact signature"
            );
            let mut reply: Value =
                serde_json::from_str::<Value>(&raw).expect("saved envelope")["reply"].clone();
            normalize_id(&mut reply, &request_id);
            assert_eq!(reply, old[0]["result"], "{name}: saved reply");
        }
        for (table, rows) in &actual {
            let (mut rows, mut python) = (rows.clone(), expected.state[table].clone());
            normalize_id(&mut rows, &request_id);
            if table == "messages" {
                for row in python.as_array_mut().expect("messages") {
                    for status in row["deliveries"]
                        .as_object_mut()
                        .expect("deliveries")
                        .values_mut()
                    {
                        if status.is_string() {
                            *status = json!({"status":status,"error":null});
                        }
                    }
                }
            }
            crate::removed_budget_fields::strip(&mut rows);
            crate::removed_budget_fields::strip(&mut python);
            assert_json(&rows, &python, &format!("{name}/{table}"));
        }
    }
}

/// Match the historical oracle's logical projection, retaining its hash checks
/// and omitting only independently generated scheduling timestamps.
fn logical_state(store: &RuntimeStore) -> BTreeMap<String, Value> {
    let db = store.test_reader().expect("logical state reader");
    super::agent_view_fixtures::assert_agent_view_projections(&db);
    let json_rows = |sql: &str| -> Vec<Value> {
        db.prepare(sql)
            .expect("logical state query")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("logical state rows")
            .map(|row| serde_json::from_str(&row.expect("JSON row")).expect("logical JSON"))
            .collect()
    };
    let pairs = |sql: &str| -> BTreeMap<String, Value> {
        db.prepare(sql)
            .expect("logical keyed query")
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("logical keyed rows")
            .map(|row| {
                let (id, raw) = row.expect("keyed row");
                (id, serde_json::from_str(&raw).expect("keyed JSON"))
            })
            .collect()
    };
    let mut agents = pairs("SELECT id,record FROM runtime_agents ORDER BY id");
    for (id, record) in &mut agents {
        studio_runtime_store::agent_volatile::merge(&db, id, record).expect("merged agent");
    }
    let mut hashes = db
        .prepare("SELECT hash,payload FROM sync_entities WHERE payload IS NOT NULL")
        .expect("hash query");
    for row in hashes
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("hash rows")
    {
        let (hash, payload) = row.expect("hash row");
        assert_eq!(hash, studio_runtime_store::sha256_hex(payload.as_bytes()));
    }
    let invalid_capacity_timestamps: i64 = db.query_row(
        "SELECT count(*) FROM runtime_capacity_retries WHERE json_extract(record,'$.status')='finished' AND COALESCE(json_extract(record,'$.updatedAt')>1,0)=0", [], |row| row.get(0)
    ).expect("capacity timestamps");
    assert_eq!(
        invalid_capacity_timestamps, 0,
        "capacity reset refreshes updatedAt"
    );
    let invalid_generated_timestamps: i64 = db.query_row(
        "SELECT
            (SELECT count(*) FROM runtime_events WHERE typeof(created) NOT IN ('integer','real')) +
            (SELECT count(*) FROM runtime_event_meta WHERE COALESCE(json_type(record,'$.acceptedAt'),'null') NOT IN ('integer','real')) +
            (SELECT count(*) FROM messages WHERE typeof(at) NOT IN ('integer','real')) +
            (SELECT count(*) FROM sync_entities WHERE collection='event' AND payload IS NOT NULL AND COALESCE(json_type(payload,'$.value.created'),'null') NOT IN ('integer','real'))",
        [], |row| row.get(0),
    ).expect("generated timestamp types");
    assert_eq!(
        invalid_generated_timestamps, 0,
        "normalized timestamps remain numeric"
    );
    let mut state = BTreeMap::from([
        ("agents".into(), json!(agents)),
        (
            "metadata".into(),
            json!(pairs(
                "SELECT id,json_remove(record,'$.acceptedAt','$.timing') FROM runtime_event_meta ORDER BY id"
            )),
        ),
    ]);
    for (name, sql) in [
        (
            "events",
            "SELECT json_object('id',id,'agent',agent,'kind',kind,'text',text,'status',status,'epoch',epoch,'turn_id',turn_id,'error',error) FROM runtime_events ORDER BY id",
        ),
        (
            "messages",
            "SELECT json_object('id',id,'room',room,'author',author,'text',text,'at','generated timestamp','deliveries',json(deliveries)) FROM messages ORDER BY rowid",
        ),
        (
            "sync_entities",
            "SELECT json_object('collection',collection,'id',id,'seq',seq,'hash',CASE WHEN payload IS NULL THEN hash ELSE 'verified payload hash' END,'payload',json(CASE WHEN collection='event' AND payload IS NOT NULL THEN json_set(payload,'$.value.created','generated timestamp') ELSE payload END),'deleted',deleted) FROM sync_entities WHERE collection NOT IN ('agentSummary','agentDetail') ORDER BY rowid",
        ),
        (
            "sync_documents",
            "SELECT json_object('seq',seq,'scope',scope,'id',id,'payload',json(payload),'deleted',deleted) FROM sync_documents ORDER BY rowid",
        ),
        (
            "sync_versions",
            "SELECT json_object('seq',seq,'scope',scope,'hash',hash,'deleted',deleted,'updated',updated) FROM sync_versions ORDER BY rowid",
        ),
        (
            "runtime_capacity_retries",
            "SELECT json_object('id',id,'agent',agent,'record',json(CASE WHEN json_extract(record,'$.status')='finished' THEN json_set(record,'$.updatedAt','generated timestamp') ELSE record END)) FROM runtime_capacity_retries ORDER BY rowid",
        ),
    ] {
        state.insert(name.into(), json!(json_rows(sql)));
    }
    state
}

fn assert_json(actual: &Value, expected: &Value, path: &str) {
    match (actual, expected) {
        (Value::Object(a), Value::Object(b)) => {
            assert_eq!(
                a.keys().collect::<Vec<_>>(),
                b.keys().collect::<Vec<_>>(),
                "{path}: keys"
            );
            for (key, value) in a {
                assert_json(value, &b[key], &format!("{path}/{key}"));
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len(), "{path}: rows");
            for (i, (a, b)) in a.iter().zip(b).enumerate() {
                assert_json(a, b, &format!("{path}/{i}"));
            }
        }
        _ if path.ends_with("/seq") => {
            // New view publications share the sequence owner with legacy rows.
            // Exact counters are covered by rollback/replay and resume tests.
            assert!(
                actual.as_i64().is_some_and(|n| n > 0),
                "{path}: actual revision"
            );
            assert!(
                expected.as_i64().is_some_and(|n| n > 0),
                "{path}: expected revision"
            );
        }
        _ => assert_eq!(actual, expected, "{path}"),
    }
}

fn snapshot(store: &RuntimeStore) -> Vec<(String, Vec<String>)> {
    let connection = store.test_reader().expect("snapshot connection");
    let mut tables = connection
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .expect("tables");
    tables
        .query_map([], |row| row.get::<_, String>(0))
        .expect("names")
        .map(|name| {
            let name = name.expect("name");
            let mut query = connection
                .prepare(
                    &studio_runtime_store::agent_columns::select_declared_sql(&connection, &name)
                        .expect("columns"),
                )
                .expect("table");
            let count = query.column_count();
            let mut rows: Vec<String> = query
                .query_map([], |row| {
                    (0..count)
                        .map(|i| row.get::<_, rusqlite::types::Value>(i))
                        .collect::<Result<Vec<_>, _>>()
                })
                .expect("rows")
                .map(|row| format!("{:?}", row.expect("row")))
                .collect();
            rows.sort();
            (name, rows)
        })
        .collect()
}

const WRITE_BOUNDARIES: &[(&str, &str)] = &[
    ("runtime_agents", "UPDATE"),
    ("runtime_event_meta", "INSERT"),
    ("runtime_events", "INSERT"),
    ("runtime_event_meta", "UPDATE"),
    ("request_results", "INSERT"),
    ("messages", "INSERT"),
    ("sync_entities", "INSERT"),
    ("sync_entities", "UPDATE"),
    ("runtime_capacity_retries", "UPDATE"),
];

async fn arm_statement(
    store: &RuntimeStore,
    table: &str,
    operation: &str,
    point: &str,
    target: usize,
) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let count = Arc::new(AtomicUsize::new(0));
    let counter = count.clone();
    let table = table.to_owned();
    let operation = operation.to_owned();
    let point = point.to_owned();
    store.transaction(move |tx| {
        tx.create_scalar_function("p03_statement", 0, rusqlite::functions::FunctionFlags::SQLITE_UTF8, move |_| {
            let occurrence = counter.fetch_add(1, Ordering::SeqCst) + 1;
            if occurrence == target { std::process::exit(73); }
            Ok(0_i64)
        })?;
        tx.execute_batch(&format!("CREATE TEMP TRIGGER p03_statement {point} {operation} ON {table} BEGIN SELECT p03_statement(); END"))?;
        Ok(())
    }).await.expect("arm occurrence-specific boundary");
    count
}

fn crash_child(
    root: &Path,
    table: &str,
    operation: &str,
    point: &str,
    occurrence: usize,
    request_id: &str,
) {
    let status = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "wave7_p03_message_ingress::ingress_crash_child",
            "--ignored",
            "--nocapture",
        ])
        .env("P03_CRASH_ROOT", root)
        .env("P03_CRASH_TABLE", table)
        .env("P03_CRASH_OPERATION", operation)
        .env("P03_CRASH_POINT", point)
        .env("P03_CRASH_OCCURRENCE", occurrence.to_string())
        .env("P03_REQUEST_ID", request_id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .expect("crash child")
        .wait()
        .expect("child exit");
    assert_eq!(
        status.code(),
        Some(73),
        "{table}/{operation}/{point}/{occurrence}: boundary reached"
    );
}

#[tokio::test]
#[ignore = "subprocess entrypoint"]
async fn ingress_crash_child() {
    let Some(root) = std::env::var_os("P03_CRASH_ROOT") else {
        return;
    };
    let store = RuntimeStore::open(Some(Path::new(&root)))
        .await
        .expect("child store");
    let table = std::env::var("P03_CRASH_TABLE").expect("table");
    let request_id = std::env::var("P03_REQUEST_ID").expect("request identity");
    let point = std::env::var("P03_CRASH_POINT").expect("point");
    if table != "handoff" {
        arm_statement(
            &store,
            &table,
            &std::env::var("P03_CRASH_OPERATION").expect("operation"),
            &point,
            std::env::var("P03_CRASH_OCCURRENCE")
                .expect("occurrence")
                .parse()
                .expect("number"),
        )
        .await;
        send(&store, &json!({}), &request_id)
            .await
            .expect("send before crash");
        panic!("statement boundary was not reached");
    }
    let prepared = PreparedAdmission::read(&store.test_reader().expect("read"), "agent")
        .expect("SQL admission inputs")
        .prepare()
        .await
        .expect("prepare");
    let before_commit = point == "before_commit";
    let post_id = request_id.clone();
    let posted = store
        .transaction_with_error(move |tx| {
            let posted = post(tx, &json!({}), &prepared, &post_id)?;
            if before_commit {
                std::process::exit(73);
            }
            Ok::<_, coordination::CoordinationError>(posted)
        })
        .await
        .expect("commit");
    if point == "committed_before_wake" {
        std::process::exit(73);
    }
    // The host wake carries no provider input. The durable event is the only
    // source for a restarted consumer, even if this in-memory signal is lost.
    let wakes = std::cell::Cell::new(0);
    assert!(posted.event_inserted);
    posted.notify_committed(|| wakes.set(wakes.get() + 1));
    let replay = send(&store, &json!({}), &request_id)
        .await
        .expect("committed replay before handoff");
    replay.notify_committed(|| wakes.set(wakes.get() + 1));
    assert_eq!(
        wakes.get(),
        1,
        "each wake callback must be counted, without coalescing"
    );
    std::process::exit(73);
}

#[tokio::test]
async fn each_write_occurrence_and_commit_wake_crash_recovers_exactly_once() {
    let fixture = Fixture::new();
    use std::sync::atomic::Ordering;
    let mut reached = 0;
    for &(table, operation) in WRITE_BOUNDARIES {
        if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_none()
            && !["runtime_events", "request_results"].contains(&table)
        {
            continue;
        }
        for point in ["BEFORE", "AFTER"] {
            let root = fixture.seed(&format!("count-{table}-{operation}-{point}"));
            let store = RuntimeStore::open(Some(&root)).await.expect("count store");
            let request_id = request_id();
            fixture.configure(&store, "capacity").await;
            let count = arm_statement(&store, table, operation, point, usize::MAX).await;
            let prepared = PreparedAdmission::read(&store.test_reader().expect("read"), "agent")
                .expect("SQL admission inputs")
                .prepare()
                .await
                .expect("prepare");
            let before = snapshot(&store);
            let prepared = prepared.clone();
            let post_id = request_id.clone();
            let result = store
                .transaction_with_error(move |tx| {
                    post(tx, &json!({}), &prepared, &post_id)?;
                    Err::<(), _>(coordination::CoordinationError::InvalidRequest(
                        "census rollback",
                    ))
                })
                .await;
            assert!(matches!(
                result,
                Err(coordination::CoordinationError::InvalidRequest(
                    "census rollback"
                ))
            ));
            assert_eq!(snapshot(&store), before);
            let occurrences = count.load(Ordering::SeqCst);
            assert!(occurrences > 0, "{table}/{operation}: real write");
            drop(store);
            for occurrence in 1..=occurrences {
                crash_child(&root, table, operation, point, occurrence, &request_id);
                let recovered = RuntimeStore::open(Some(&root))
                    .await
                    .expect("recover abrupt exit");
                assert_eq!(
                    snapshot(&recovered),
                    before,
                    "{table}/{operation}/{point}/{occurrence}: atomic crash recovery"
                );
                drop(recovered);
                reached += 1;
            }
            let store = RuntimeStore::open(Some(&root)).await.expect("retry store");
            assert!(
                send(&store, &json!({}), &request_id)
                    .await
                    .expect("retry")
                    .event_inserted
            );
            let committed = snapshot(&store);
            drop(store);
            let store = RuntimeStore::open(Some(&root))
                .await
                .expect("lost-response restart");
            assert!(
                !send(&store, &json!({}), &request_id)
                    .await
                    .expect("replay")
                    .event_inserted
            );
            assert_eq!(snapshot(&store), committed);
        }
    }
    for point in ["before_commit", "committed_before_wake", "after_wake"] {
        let root = fixture.seed(point);
        let store = RuntimeStore::open(Some(&root))
            .await
            .expect("before handoff");
        let request_id = request_id();
        let before = snapshot(&store);
        drop(store);
        crash_child(&root, "handoff", "", point, 0, &request_id);
        let store = RuntimeStore::open(Some(&root))
            .await
            .expect("handoff restart");
        if point == "before_commit" {
            assert_eq!(snapshot(&store), before);
        }
        let response = send(&store, &json!({}), &request_id)
            .await
            .expect("handoff retry");
        assert_eq!(response.event_inserted, point == "before_commit");
        let committed = snapshot(&store);
        assert!(
            !send(&store, &json!({}), &request_id)
                .await
                .expect("replay")
                .event_inserted
        );
        assert_eq!(snapshot(&store), committed);
        let count: i64 = store
            .test_reader()
            .expect("read")
            .query_row(
                "SELECT count(*) FROM runtime_events WHERE id=?1",
                [&request_id],
                |row| row.get(0),
            )
            .expect("event count");
        assert_eq!(count, 1);
    }
    eprintln!(
        "P03 covered {reached} occurrence-specific abrupt statement exits and 4 commit/wake exits"
    );
}

#[tokio::test]
async fn event_projection_failure_returns_error_and_rolls_back_message_admission() {
    let fixture = Fixture::new();
    let root = fixture.seed("projection-failure");
    let store = RuntimeStore::open(Some(&root)).await.expect("store");
    let request_id = request_id();
    store
        .transaction(move |tx| {
            tx.execute_batch(
                "CREATE TRIGGER fail_event_projection BEFORE INSERT ON sync_entities
                 WHEN NEW.collection='event'
                 BEGIN SELECT RAISE(ABORT,'injected event projection failure'); END;",
            )?;
            Ok(())
        })
        .await
        .expect("arm projection failure");
    let before = snapshot(&store);
    let prepared = PreparedAdmission::read(&store.test_reader().expect("read"), "agent")
        .expect("SQL admission inputs")
        .prepare()
        .await
        .expect("prepare admission");
    let prepared = prepared.clone();
    let result = store
        .transaction_with_error(move |tx| {
            post(
                tx,
                &json!({"id":"projection-failure-message","room":"agent","text":"rollback"}),
                &prepared,
                &request_id,
            )
        })
        .await;
    assert!(matches!(
        result,
        Err(coordination::CoordinationError::Store(_))
    ));
    assert_eq!(snapshot(&store), before);
}

#[tokio::test]
async fn injected_policy_denials_and_changed_workspace_snapshot_write_nothing() {
    let fixture = Fixture::new();
    use studio_runtime_services::coordination::CoordinationError;
    use studio_runtime_services::message_admission::MessageAdmission;
    use studio_runtime_store::{AgentRecord, Transaction};
    struct Policy {
        workspace_denied: bool,
    }
    impl MessageAdmission for Policy {
        fn workspace(
            &self,
            _tx: &Transaction<'_>,
            _agent: &AgentRecord,
        ) -> Result<Vec<Value>, CoordinationError> {
            if self.workspace_denied {
                return Err(CoordinationError::InvalidRequest("Workspace busy"));
            }
            Ok(Vec::new())
        }
    }
    let root = fixture.seed("gates");
    let store = RuntimeStore::open(Some(&root)).await.expect("store");
    let request_id = request_id();
    for policy in [Policy {
        workspace_denied: true,
    }] {
        let before = snapshot(&store);
        let request_id = request_id.clone();
        let result = store
            .transaction_with_error(move |tx| {
                coordination::post_message_with_admission(
                    tx,
                    PostManagedMessage {
                        room: "agent",
                        id: &request_id,
                        text: "input",
                        delivery: studio_runtime_schema::delivery::DeliveryMoment::CurrentTurn,
                        assets: &[],
                    },
                    &policy,
                )
            })
            .await;
        assert!(matches!(result, Err(CoordinationError::InvalidRequest(_))));
        assert_eq!(snapshot(&store), before);
    }
    let prepared = PreparedAdmission::read(&store.test_reader().expect("read"), "agent")
        .expect("SQL admission inputs")
        .prepare()
        .await
        .expect("prepare");
    store.transaction(|tx| {
        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.cwd','/unprepared/path','$.workspaceOperation','capture') WHERE id='agent'",[])?;
        Ok(())
    }).await.expect("change workspace");
    let before = snapshot(&store);
    let prepared = prepared.clone();
    let result = store
        .transaction_with_error(move |tx| {
            coordination::post_message_with_admission(
                tx,
                PostManagedMessage {
                    room: "agent",
                    id: &request_id,
                    text: "input",
                    delivery: studio_runtime_schema::delivery::DeliveryMoment::CurrentTurn,
                    assets: &[],
                },
                &prepared,
            )
        })
        .await;
    assert!(
        matches!(result, Err(CoordinationError::AdmissionDenied(_))),
        "a changed cwd missing from the prepared snapshot must fail closed: {result:?}"
    );
    assert_eq!(snapshot(&store), before);
}

#[tokio::test]
async fn voice_delivery_prepares_workspace_admission() {
    let fixture = Fixture::new();
    use studio_runtime_services::voice;
    for operation in ["unrelated", "checkpoint", "capture"] {
        let root = fixture.seed(operation);
        let store = std::sync::Arc::new(RuntimeStore::open(Some(&root)).await.expect("store"));
        let request_id = request_id();
        let record = studio_runtime_store::VoiceRecord {
            sequence: 0,
            id: "spoken".into(),
            agent: "agent".into(),
            session: "session".into(),
            kind: "user".into(),
            text: "spoken input".into(),
            item_id: String::new(),
            previous_item_id: String::new(),
            payload: "{}".into(),
            created: 1.0,
        };
        let record_id = record.id.clone();
        store
            .transaction(move |tx| {
                studio_runtime_store::VoiceRepository::insert_record(tx, &record)
            })
            .await
            .expect("transcript");
        store
            .transaction(move |tx| {
                let mut other = studio_runtime_store::AgentRepository::get(tx, &"agent".into())?
                    .expect("agent");
                other.id = "other".into();
                other.extra.insert(
                    "workspaceOperation".into(),
                    json!(if operation == "unrelated" {
                        "restore"
                    } else {
                        operation
                    }),
                );
                if operation == "unrelated" {
                    other.extra.insert("cwd".into(), json!(root));
                }
                studio_runtime_store::AgentRepository::put(tx, &other)
            })
            .await
            .expect("active operation");
        let receipt = voice::submit(
            std::sync::Arc::clone(&store),
            "agent",
            &request_id,
            &[record_id],
            None,
        )
        .await
        .expect("accepted voice delivery");
        assert_eq!(receipt["status"], "queued", "{operation}");
        assert_eq!(
            receipt.get("waitingFor").is_some(),
            operation != "unrelated"
        );
    }
}

#[tokio::test]
async fn corrupt_replay_and_workspace_operation_errors_name_the_row() {
    let fixture = Fixture::new();
    let root = fixture.seed("malformed");
    let store = RuntimeStore::open(Some(&root)).await.expect("store");
    let request_id = request_id();
    send(&store, &json!({}), &request_id)
        .await
        .expect("accepted");
    let corrupt_id = request_id.clone();
    store
        .transaction(move |tx| {
            tx.execute(
                "UPDATE runtime_events SET created='not-a-number' WHERE id=?1",
                [&corrupt_id],
            )?;
            Ok(())
        })
        .await
        .expect("corrupt event");
    let before = snapshot(&store);
    assert!(
        send(&store, &json!({}), &request_id)
            .await
            .expect_err("invalid timestamp")
            .to_string()
            .contains(&format!("runtime_events/{request_id}"))
    );
    assert_eq!(snapshot(&store), before);
    let delete_id = request_id.clone();
    let result_id = studio_runtime_schema::persistence::RequestIdentity::new(
        "message",
        &["agent", "user"],
        &request_id,
        &[],
    )
    .expect("identity")
    .id()
    .to_owned();
    store.transaction(move |tx| {
        tx.execute("DELETE FROM runtime_events WHERE id=?1", [&delete_id])?;
        tx.execute("DELETE FROM request_results WHERE id=?1", [&result_id])?;
        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.workspaceOperation','capture','$.workspaceReservationId','operation') WHERE id='agent'", [])?;
        tx.execute("DROP INDEX IF EXISTS runtime_workspace_operation_phase", [])?;
        tx.execute("DROP INDEX IF EXISTS runtime_workspace_operation_agent_phase", [])?;
        tx.execute("INSERT INTO runtime_workspace_operations(id,record) VALUES('operation','{broken')", [])?;
        Ok(())
    }).await.expect("corrupt operation");
    let before = snapshot(&store);
    assert!(
        send(&store, &json!({}), &request_id)
            .await
            .expect_err("invalid operation JSON")
            .to_string()
            .contains("runtime_workspace_operations/operation")
    );
    assert_eq!(snapshot(&store), before);
}

#[tokio::test]
async fn late_operation_paths_use_the_prepared_canonical_relation() {
    let fixture = Fixture::new();
    use studio_runtime_store::AgentRepository;
    for scenario in ["known_unrelated", "known_alias", "unknown_alias"] {
        let root = fixture.seed(scenario);
        let store = RuntimeStore::open(Some(&root)).await.expect("store");
        let request_id = request_id();
        let mut other = AgentRepository::get(&store.test_reader().expect("read"), &"agent".into())
            .expect("get")
            .expect("agent");
        other.id = "other".into();
        let path = if scenario == "known_unrelated" {
            root.clone()
        } else {
            let alias = root.join("workspace-alias");
            std::os::unix::fs::symlink(other.extra["cwd"].as_str().expect("cwd"), &alias)
                .expect("workspace alias");
            alias
        };
        other.extra.insert("cwd".into(), json!(path));
        if scenario != "unknown_alias" {
            store
                .transaction({
                    let other = other.clone();
                    move |tx| AgentRepository::put(tx, &other)
                })
                .await
                .expect("idle other");
        }
        let prepared = PreparedAdmission::read(&store.test_reader().expect("read"), "agent")
            .expect("SQL admission inputs")
            .prepare()
            .await
            .expect("prepare");
        other
            .extra
            .insert("workspaceOperation".into(), json!("capture"));
        store
            .transaction({
                let other = other.clone();
                move |tx| AgentRepository::put(tx, &other)
            })
            .await
            .expect("late operation");
        let before = snapshot(&store);
        let prepared = prepared.clone();
        let result = store
            .transaction_with_error(move |tx| post(tx, &json!({}), &prepared, &request_id))
            .await;
        if scenario == "unknown_alias" {
            // A new cwd may alias this workspace: raw string inequality cannot
            // establish independence. Python resolves it under its runtime lock;
            // this adapter requires a fresh host snapshot outside the writer.
            assert!(
                matches!(
                    result,
                    Err(coordination::CoordinationError::AdmissionDenied(_))
                ),
                "a late, previously unseen workspace alias must fail closed: {result:?}"
            );
            assert_eq!(snapshot(&store), before);
        } else {
            let receipt = result.expect("known relation permits admission").receipt;
            assert_eq!(receipt["status"], "queued");
            if scenario == "known_alias" {
                assert_eq!(
                    receipt["waitingFor"],
                    json!([{"agentId":"other","operation":"capture"}])
                );
            } else {
                assert!(receipt.get("waitingFor").is_none());
            }
        }
    }
}
