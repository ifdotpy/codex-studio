use serde_json::{Value, json};
use studio_runtime_services::{
    account_worker::commit_prepared_turn,
    claude_input_recovery::{
        TransportSnapshot,
        runtime::{SubmittedBinding, bind_submitted, freeze_start, submission::commit_start},
    },
    notification_dispatch::{PreparedSubmission, SubmittedInput},
    scheduler::TurnIntent,
};

#[derive(serde::Deserialize)]
struct Case {
    retry: bool,
    fault: Option<String>,
    before: Value,
    after: Value,
}
fn corpus() -> Vec<Case> {
    #[derive(serde::Deserialize)]
    struct Captured {
        case: Case,
        database: serde::de::IgnoredAny,
    }
    let cases: Vec<Captured> =
        crate::turn_fixtures::load(include_bytes!("wave7_p08_submission.json"));
    cases
        .into_iter()
        .map(|c| {
            let _ = c.database;
            c.case
        })
        .collect()
}

#[derive(serde::Deserialize)]
struct Seed {
    runtime_agents: Vec<RecordRow>,
    runtime_execution_runs: Vec<RunRow>,
    runtime_execution_attempts: Vec<AttemptRow>,
    runtime_execution_inputs: Vec<InputRow>,
    runtime_events: Vec<EventRow>,
    runtime_event_meta: Vec<RecordRow>,
    runtime_items: Vec<ItemRow>,
}
#[derive(serde::Deserialize)]
struct RecordRow {
    id: String,
    record: Value,
}
#[derive(serde::Deserialize)]
struct RunRow {
    id: String,
    agent: String,
    account: String,
    epoch: i64,
    thread: Option<String>,
    turn: Option<String>,
    created: f64,
    record: Value,
}
#[derive(serde::Deserialize)]
struct AttemptRow {
    id: String,
    run: String,
    record: Value,
}
#[derive(serde::Deserialize)]
struct InputRow {
    run: String,
    event: String,
}
#[derive(serde::Deserialize)]
struct EventRow {
    id: String,
    agent: String,
    epoch: i64,
    kind: String,
    text: String,
    created: f64,
    status: String,
    turn_id: Option<String>,
    error: Option<String>,
}
#[derive(serde::Deserialize)]
struct ItemRow {
    id: String,
    agent: String,
    record: Value,
    created: f64,
}

async fn seeded(path: &std::path::Path, state: &Value) -> studio_runtime_store::RuntimeStore {
    use studio_runtime_schema::persistence::RequestIdentity;
    use studio_runtime_store::request_results::{Admission, RequestResults};
    std::fs::create_dir_all(path).expect("fixture directory");
    let store = studio_runtime_store::RuntimeStore::open(Some(path))
        .await
        .expect("fresh current store");
    let seed: Seed = serde_json::from_value(state.clone()).expect("typed captured rows");
    store.transaction(move |tx| {
        for row in seed.runtime_agents {
            let agent = studio_runtime_store::AgentRecord::from_value(row.record)?;
            assert_eq!(agent.id.0, row.id);
            studio_runtime_store::AgentRepository::put(tx, &agent)?;
        }
        for row in seed.runtime_execution_runs {
            tx.execute("INSERT INTO runtime_execution_runs(id,agent,account,epoch,thread,turn,created,record) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)", rusqlite::params![row.id,row.agent,row.account,row.epoch,row.thread,row.turn,row.created,row.record.to_string()])?;
        }
        for row in seed.runtime_execution_attempts {
            let root = RequestIdentity::issue_root();
            let identity = RequestIdentity::new("execution-attempt", &["agent"], &root, &[&row.id]).expect("identity");
            let Admission::Fresh(admission) = RequestResults::admit(tx, &identity, "fixture", identity.issued_ms()).map_err(studio_runtime_store::StoreError::from)? else { panic!("new identity"); };
            tx.execute("INSERT INTO runtime_execution_attempts(id,run,record) VALUES(?1,?2,json_set(?3,'$.admission',json(?4),'$.rootRequestId',?5))",rusqlite::params![row.id,row.run,row.record.to_string(),serde_json::to_string(&admission)?,root])?;
        }
        for row in seed.runtime_execution_inputs { tx.execute("INSERT INTO runtime_execution_inputs(run,event) VALUES(?1,?2)",rusqlite::params![row.run,row.event])?; }
        for row in seed.runtime_events { tx.execute("INSERT INTO runtime_events(id,agent,epoch,kind,text,created,status,turn_id,error) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",rusqlite::params![row.id,row.agent,row.epoch,row.kind,row.text,row.created,row.status,row.turn_id,row.error])?; }
        for row in seed.runtime_event_meta { tx.execute("INSERT INTO runtime_event_meta(id,record) VALUES(?1,?2)",rusqlite::params![row.id,row.record.to_string()])?; }
        for row in seed.runtime_items { tx.execute("INSERT INTO runtime_items(id,agent,record,created) VALUES(?1,?2,?3,?4)",rusqlite::params![row.id,row.agent,row.record.to_string(),row.created])?; }
        Ok::<_, studio_runtime_store::StoreError>(())
    }).await.expect("current captured rows");
    store
}

fn state(db: &rusqlite::Connection) -> Value {
    let mut result = serde_json::Map::new();
    for table in [
        "runtime_agents",
        "runtime_execution_runs",
        "runtime_execution_attempts",
        "runtime_execution_inputs",
        "runtime_events",
        "runtime_event_meta",
        "runtime_items",
        "runtime_item_fulltext",
    ] {
        let columns = match table {
            "runtime_agents" => "id,record",
            "runtime_execution_runs" => "id,agent,account,epoch,thread,turn,created,record",
            "runtime_execution_attempts" => "id,run,record",
            "runtime_execution_inputs" => "run,event",
            "runtime_events" => "id,agent,epoch,kind,text,created,status,turn_id,error",
            "runtime_event_meta" => "id,record",
            "runtime_items" => "id,agent,record,created",
            "runtime_item_fulltext" => "*",
            _ => unreachable!(),
        };
        let mut stmt = db
            .prepare(&format!("SELECT {columns} FROM {table} ORDER BY 1,2"))
            .expect("table");
        let names = stmt
            .column_names()
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        let rows = stmt
            .query_map([], |row| {
                let mut value = serde_json::Map::new();
                for (i, name) in names.iter().enumerate() {
                    use rusqlite::types::ValueRef;
                    let v = match row.get_ref(i)? {
                        ValueRef::Null => Value::Null,
                        ValueRef::Integer(v) => json!(v),
                        ValueRef::Real(v) => json!(v),
                        ValueRef::Text(v) => {
                            let s = std::str::from_utf8(v).expect("UTF8");
                            if name == "record" {
                                serde_json::from_str(s).expect("JSON")
                            } else {
                                json!(s)
                            }
                        }
                        // Owned stored-text columns hold stored format 1.
                        ValueRef::Blob(bytes) => json!(
                            studio_runtime_store::stored_text::decode_blob(bytes)
                                .expect("blob is stored text")
                        ),
                    };
                    value.insert(name.clone(), v);
                }
                Ok(Value::Object(value))
            })
            .expect("rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("rows");
        let mut rows = rows;
        if table == "runtime_agents" {
            studio_runtime_store::agent_volatile::merge_dumped_rows(db, &mut rows)
                .expect("volatile agent fields");
        }
        if table == "runtime_execution_attempts" || table == "runtime_event_meta" {
            for row in &mut rows {
                if let Some(record) = row.get_mut("record").and_then(Value::as_object_mut) {
                    record.remove("admission");
                    record.remove("rootRequestId");
                    record.remove("providerSubmission");
                }
            }
        }
        result.insert(table.into(), json!(rows));
    }
    Value::Object(result)
}
fn agent(state: &Value) -> &Value {
    &state["runtime_agents"][0]["record"]
}
fn transport() -> TransportSnapshot {
    serde_json::from_str(r#"{"accountKey":"account","connectionId":"connection","supervisorIdentity":null,"closed":false}"#).expect("transport")
}
fn binding(generation: u64) -> SubmittedBinding {
    SubmittedBinding {
        transport: transport(),
        generation,
    }
}
fn intent(state: &Value) -> TurnIntent {
    let a = agent(state);
    TurnIntent {
        agent_id: "agent".into(),
        epoch: 2,
        attempt_id: a["startAttempt"]["id"].as_str().expect("id").into(),
        account_key: "account".into(),
        provider: studio_runtime_providers::ProviderKind::Claude,
        thread_id: Some("thread".into()),
        cwd: Some("/fixture".into()),
        model: Some(a["model"].as_str().expect("model").into()),
        radio_authorized: false,
        events: vec![],
    }
}
fn submission(state: &Value, at: f64) -> PreparedSubmission {
    PreparedSubmission {
        first_event_id: "one".into(),
        submitted_at: at,
        created_at: at,
        text: "compiled original".into(),
        assets: json!([]),
        inputs: state["runtime_events"]
            .as_array()
            .expect("events")
            .iter()
            .map(|e| SubmittedInput {
                id: e["id"].as_str().expect("id").into(),
                kind: e["kind"].as_str().expect("kind").into(),
                text: e["text"].as_str().expect("text").into(),
                at: e["created"].as_f64().expect("time"),
                assets: json!([]),
            })
            .collect(),
        radio_authorized: false,
    }
}
fn params(state: &Value) -> Value {
    json!({"threadId":"thread","model":agent(state)["model"],"cwd":"/fixture","serviceTier":"default",
        "clientUserMessageId":"one","input":[{"type":"text","text":"compiled original"}],"dynamicTools":[{"name":"refreshed"}]})
}
fn compare(db: &rusqlite::Connection, expected: &Value, label: &str) {
    let mut actual = state(db);
    // Current execution ownership also publishes a typed active pointer. Check it
    // before comparing the unchanged Python record fields.
    let mut expected = expected.clone();
    for state in [&mut actual, &mut expected] {
        for row in state
            .get_mut("runtime_agents")
            .and_then(Value::as_array_mut)
            .expect("agents")
        {
            if let Some(record) = row.get_mut("record").and_then(Value::as_object_mut) {
                for key in ["threadId", "turnId"] {
                    if record.get(key).is_some_and(Value::is_null) {
                        record.remove(key);
                    }
                }
            }
            if let Some(record) = row.get_mut("record").and_then(Value::as_object_mut)
                && record.contains_key("activeExecution")
            {
                let agent =
                    studio_runtime_store::AgentRecord::from_value(Value::Object(record.clone()))
                        .expect("agent record");
                let pointer = agent
                    .active_execution()
                    .expect("active execution")
                    .expect("pointer");
                record.remove("activeExecution");
                assert_eq!(pointer.run_id.as_deref(), Some("run:attempt"));
                assert_eq!(pointer.attempt_id.as_deref(), Some("attempt"));
            }
        }
    }
    for (table, rows) in expected.as_object().expect("state") {
        if table != "runtime_operation_receipts" {
            assert_eq!(&actual[table], rows, "{label}: {table}");
        }
    }
}

fn assert_current_receipts(db: &rusqlite::Connection) {
    use studio_runtime_schema::persistence::ExecutionReceiptKind;
    use studio_runtime_services::notification_dispatch::DispatchOutcome;
    use studio_runtime_store::request_results::{AdmittedRequest, RequestResults};
    #[derive(serde::Deserialize)]
    struct Input {
        agent: String,
        epoch: i64,
        ticket: String,
        account: String,
        connection: String,
        generation: u64,
        thread: String,
    }
    #[derive(serde::Deserialize, PartialEq, Debug)]
    #[serde(rename_all = "camelCase")]
    struct Owner {
        attempt_id: Option<String>,
        first_event_id: String,
        admission: AdmittedRequest,
        publication_outcome: DispatchOutcome,
    }
    let receipt = studio_runtime_store::ExecutionHistoryRepository::receipt::<Input>(
        db,
        "attempt",
        ExecutionReceiptKind::Submission,
    )
    .expect("read submission receipt")
    .expect("submission receipt exists");
    let input = receipt.input;
    assert_eq!(
        (
            input.agent.as_str(),
            input.epoch,
            input.ticket.as_str(),
            input.account.as_str(),
            input.connection.as_str(),
            input.generation,
            input.thread.as_str()
        ),
        (
            "agent",
            2,
            "request:attempt",
            "account",
            "connection",
            7,
            "thread"
        )
    );
    assert_eq!(receipt.result, Value::Bool(true));
    let attempt = studio_runtime_store::ExecutionHistoryRepository::attempt(db, "attempt")
        .expect("attempt read")
        .expect("attempt exists");
    let owner: Owner = serde_json::from_str(
        attempt
            .provider_submission
            .expect("publication owner")
            .get(),
    )
    .expect("typed publication owner");
    assert_eq!(owner.attempt_id.as_deref(), Some("attempt"));
    assert_eq!(owner.first_event_id, "one");
    assert_eq!(owner.publication_outcome, DispatchOutcome::Written);
    assert!(RequestResults::admitted_result(db, &owner.admission)
        .expect("retained result read")
        .is_none());
    for event in ["one", "two"] {
        let raw: String = db.query_row("SELECT json_extract(record,'$.providerSubmission') FROM runtime_event_meta WHERE id=?1",[event],|r| r.get(0)).expect("event owner");
        let event_owner: Owner = serde_json::from_str(&raw).expect("typed event owner");
        assert_eq!(event_owner, owner);
    }
}

fn receipt_snapshot(db: &rusqlite::Connection) -> Vec<(String, String)> {
    let mut result = Vec::new();
    for (table, sql) in [
        (
            "attempt",
            "SELECT id,record FROM runtime_execution_attempts ORDER BY id",
        ),
        (
            "event",
            "SELECT id,record FROM runtime_event_meta ORDER BY id",
        ),
        (
            "receipt",
            "SELECT id,json_array(signature,result,created) FROM request_results ORDER BY id",
        ),
    ] {
        let mut statement = db.prepare(sql).expect("receipt snapshot query");
        let rows = statement
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .expect("receipt snapshot rows");
        for row in rows {
            let (id, value) = row.expect("receipt snapshot row");
            result.push((format!("{table}/{id}"), value));
        }
    }
    result
}

async fn submit(
    store: &studio_runtime_store::RuntimeStore,
    before: Value,
    submitted_at: f64,
    rollback: bool,
) -> Result<(), studio_runtime_services::account_worker::WorkerError> {
    store
        .transaction_with(move |tx| {
            let i = intent(&before);
            commit_start(
                tx,
                &i,
                &format!("request:{}", i.attempt_id),
                binding(7),
                &params(&before),
                &submission(&before, submitted_at),
            )?;
            if rollback {
                return Err(
                    studio_runtime_store::StoreError::RecordContract("before commit").into(),
                );
            }
            Ok(())
        })
        .await
}

#[tokio::test]
async fn real_submission_transaction_matches_python_complete_rows_and_crash_boundaries() {
    let root = tempfile::tempdir().expect("scratch");
    for (n, case) in corpus().into_iter().enumerate() {
        let path = root.path().join(n.to_string());
        let store = seeded(&path, &case.before).await;
        compare(
            &store.test_reader().expect("reader"),
            &case.before,
            "Python prestate",
        );
        let retry = case.retry;
        let before = case.before.clone();
        if retry {
            assert!(
                submit(&store, before.clone(), 170.0, false).await.is_err(),
                "legacy retry must stay held"
            );
            compare(
                &store.test_reader().expect("reader"),
                &before,
                "legacy retry held without writes",
            );
            continue;
        }
        if case.fault.as_deref() == Some("bind") {
            store.transaction(|tx| { tx.execute_batch("CREATE TRIGGER cut BEFORE UPDATE ON runtime_agents WHEN json_extract(NEW.record,'$.startAttempt.connectionId')='connection' AND json_extract(NEW.record,'$.startAttempt.submitted')=1 BEGIN SELECT RAISE(ABORT,'bind cut'); END")?; Ok(()) }).await.expect("fault");
        }
        let result = submit(
            &store,
            before,
            150.0,
            case.fault.as_deref() == Some("before_commit"),
        )
        .await;
        let failed = matches!(
            case.fault.as_deref(),
            Some("configuration" | "bind" | "before_commit")
        );
        assert_eq!(result.is_err(), failed, "case {:?}: {result:?}", case.fault);
        if case.fault.as_deref() == Some("bind") {
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER cut")?;
                    Ok(())
                })
                .await
                .expect("clear fault");
        }
        drop(store);
        let store = studio_runtime_store::RuntimeStore::open(Some(&path))
            .await
            .expect("restart");
        if !failed {
            assert_current_receipts(&store.test_reader().expect("reader"));
        }
        compare(
            &store.test_reader().expect("reader"),
            &case.after,
            &format!("fault={:?}", case.fault),
        );
    }
}

#[tokio::test]
async fn initial_binding_refuses_wrong_or_missing_prepared_generation_and_foreign_transport() {
    let root = tempfile::tempdir().expect("scratch");
    let cases = corpus();
    let fresh = &cases[0];
    let store = seeded(root.path(), &fresh.before).await;
    let before = state(&store.test_reader().expect("reader"));
    for fault in [
        "generation",
        "saved_generation",
        "missing_generation",
        "foreign_connection",
        "foreign_supervisor",
    ] {
        let snapshot = before.clone();
        let result: Result<(), studio_runtime_services::account_worker::WorkerError> = store.transaction_with(move |tx| {
            freeze_start(tx,"agent","attempt",transport(),&params(&snapshot),"compiled original")?;
            assert!(commit_prepared_turn(tx,&intent(&snapshot),"request:attempt",7,"test-connection",&submission(&snapshot,150.0))?);
            match fault {
                "saved_generation" => { tx.execute("UPDATE runtime_execution_attempts SET record=json_set(record,'$.connectionGeneration',8)",[])?; }
                "missing_generation" => { tx.execute("UPDATE runtime_agents SET record=json_remove(record,'$.startAttempt.connectionGeneration')",[])?; }
                "foreign_connection" => { tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.startAttempt.connectionId','foreign')",[])?; tx.execute("UPDATE runtime_execution_attempts SET record=json_set(record,'$.connectionId','foreign')",[])?; }
                "foreign_supervisor" => { tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.startAttempt.supervisorIdentity',json('{\"handle\":\"foreign\"}'))",[])?; tx.execute("UPDATE runtime_execution_attempts SET record=json_set(record,'$.supervisorIdentity',json('{\"handle\":\"foreign\"}'))",[])?; }
                _ => {}
            }
            let unbound = state(tx);
            let receipt_before = receipt_snapshot(tx);
            assert!(bind_submitted(tx,"agent","attempt",binding(if fault == "generation" { 8 } else { 7 })).is_err(),"{fault}");
            compare(tx,&unbound,"binding refusal has no writes");
            assert_eq!(receipt_snapshot(tx),receipt_before,"binding refusal retains exact receipts");
            Err(studio_runtime_store::StoreError::RecordContract("outer rollback").into())
        }).await;
        assert!(
            matches!(
                result,
                Err(studio_runtime_services::account_worker::WorkerError::Store(
                    studio_runtime_store::StoreError::RecordContract("outer rollback")
                ))
            ),
            "fault must reach binding refusal: {result:?}"
        );
        compare(
            &store.test_reader().expect("reader"),
            &before,
            "outer rollback restores complete prestate",
        );
    }
}

#[tokio::test]
async fn submission_write_failpoints_restore_every_row_before_a_safe_retry() {
    let root = tempfile::tempdir().expect("scratch");
    let cases = corpus();
    let fresh = &cases[0];
    let stages = [
        (
            "freeze_attempt",
            "UPDATE",
            "runtime_execution_attempts",
            "1",
        ),
        (
            "freeze_agent",
            "UPDATE",
            "runtime_agents",
            "json_extract(NEW.record,'$.startAttempt.submitted')=0",
        ),
        (
            "submitted",
            "UPDATE",
            "runtime_agents",
            "json_extract(NEW.record,'$.startAttempt.submitted')=1",
        ),
        ("event", "UPDATE", "runtime_events", "1"),
        ("transcript", "INSERT", "runtime_items", "1"),
        ("metadata", "UPDATE", "runtime_event_meta", "1"),
        ("receipt", "INSERT", "request_results", "1"),
        (
            "bind_attempt",
            "UPDATE",
            "runtime_execution_attempts",
            "json_extract(NEW.record,'$.submitted')=1 AND json_extract(NEW.record,'$.connectionId')='connection'",
        ),
        (
            "bind_agent",
            "UPDATE",
            "runtime_agents",
            "json_extract(NEW.record,'$.startAttempt.submitted')=1 AND json_extract(NEW.record,'$.startAttempt.connectionId')='connection'",
        ),
    ];
    for (stage, action, table, condition) in stages {
        for when in ["BEFORE", "AFTER"] {
            let path = root.path().join(format!("{stage}-{when}"));
            let store = seeded(&path, &fresh.before).await;
            let before = state(&store.test_reader().expect("reader"));
            let receipt_before = receipt_snapshot(&store.test_reader().expect("reader"));
            store.transaction(move |tx| { tx.execute_batch(&format!("CREATE TRIGGER cut {when} {action} ON {table} WHEN {condition} BEGIN SELECT RAISE(ABORT,'cut'); END"))?; Ok(()) }).await.expect("fault");
            assert!(
                submit(&store, before.clone(), 150.0, false).await.is_err(),
                "{stage} {when}"
            );
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER cut")?;
                    Ok(())
                })
                .await
                .expect("clear fault");
            drop(store);
            let store = studio_runtime_store::RuntimeStore::open(Some(&path))
                .await
                .expect("restart");
            compare(
                &store.test_reader().expect("reader"),
                &before,
                &format!("{stage} {when} rolled back every row"),
            );
            assert_eq!(
                receipt_snapshot(&store.test_reader().expect("reader")),
                receipt_before,
                "{stage} {when} rolled back receipt owners and results"
            );
            submit(&store, before, 150.0, false)
                .await
                .expect("safe retry before native submission");
            assert_current_receipts(&store.test_reader().expect("reader"));
            compare(
                &store.test_reader().expect("reader"),
                &fresh.after,
                &format!("{stage} {when} recovered"),
            );
        }
    }
    let path = root.path().join("stale");
    let store = seeded(&path, &fresh.before).await;
    let before = state(&store.test_reader().expect("reader"));
    let snapshot = before.clone();
    let result: Result<(), studio_runtime_services::account_worker::WorkerError> = store
        .transaction_with(move |tx| {
            let mut stale = intent(&snapshot);
            stale.epoch += 1;
            commit_start(
                tx,
                &stale,
                "request:attempt",
                binding(7),
                &params(&snapshot),
                &submission(&snapshot, 150.0),
            )?;
            Ok(())
        })
        .await;
    assert!(result.is_err());
    compare(
        &store.test_reader().expect("reader"),
        &before,
        "ownership loss after freeze rolls back",
    );
}
