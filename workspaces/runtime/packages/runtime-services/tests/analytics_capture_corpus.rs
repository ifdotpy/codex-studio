use crate::analytics_fixtures as common;
use common::{actual, tables};
use rusqlite::Connection;
use serde_json::{Value, json};
use studio_runtime_services::analytics_capture::{
    CaptureSnapshot, CaptureSource, PreparedCapture, decide,
};

// These transaction tests need only analytics; durable cross-database capture is
// exercised by analytics_history_checkpoint and notification_dispatch_corpus.
fn fixture() -> (Connection, Value) {
    let db = Connection::open_in_memory().expect("analytics database");
    let schema: Value = serde_json::from_str(include_str!(
        "../../runtime-store/src/runtime-state-schema.json"
    ))
    .expect("Rust-owned schema");
    let mut objects = schema["analytics.sqlite3"]["sqlite_master"]
        .as_array()
        .expect("analytics catalog")
        .iter()
        .collect::<Vec<_>>();
    objects.sort_by_key(|object| object["create_order"].as_i64().expect("creation order"));
    for object in objects {
        db.execute_batch(object["sql"].as_str().expect("DDL"))
            .expect("analytics schema");
    }
    studio_runtime_store::cost_aggregate::install(&db).expect("cost aggregate");
    (db, common::agent())
}

fn started_turn() -> (Connection, Value) {
    let (mut db, agent) = fixture();
    capture(&mut db, &agent, "turn/started", json!({}), 10.);
    (db, agent)
}

fn capture(db: &mut Connection, agent: &Value, method: &str, params: Value, at: f64) {
    let prepared = PreparedCapture::event(agent, method, &params, at, 200., CaptureSource::Live)
        .expect("request");
    let tx = db.transaction().expect("transaction");
    let decision = decide(CaptureSnapshot::load(&tx, prepared.into()).expect("snapshot"))
        .unwrap_or_else(|error| panic!("{method}: {error}"));
    decision.apply(&tx).expect("apply");
    tx.commit().expect("commit");
}

#[test]
fn uncertain_turn_keeps_its_outcome_and_reason_in_analytics() {
    let (mut db, agent) = started_turn();
    let error = json!({"message":"The native transcript cannot confirm the outcome."});
    capture(
        &mut db,
        &agent,
        "turn/completed",
        json!({"turn":{"id":"turn","status":"uncertain","error":error}}),
        20.,
    );
    let raw: String = db
        .query_row("SELECT record FROM analytics_turns LIMIT 1", [], |row| {
            row.get(0)
        })
        .expect("turn");
    let turn: Value = serde_json::from_str(&raw).expect("turn record");
    assert_eq!(turn["status"], "uncertain");
    assert_eq!(turn["error"], error);
}

#[test]
fn malformed_stored_rows_refuse_without_writes() {
    let (mut db, agent) = started_turn();
    for invalid in [
        json!({}),
        json!({"id":"a:thread:turn","agentId":"a","at":0,"startedAt":"bad","finishedAt":null,"firstOutputAt":null,"status":"running"}),
    ] {
        db.execute("UPDATE analytics_turns SET record=?", [invalid.to_string()])
            .expect("corrupt fixture");
        let before = actual(&db, &tables());
        let input = PreparedCapture::event(
            &agent,
            "turn/completed",
            &json!({}),
            20.,
            200.,
            CaptureSource::Live,
        )
        .expect("input");
        let tx = db.transaction().expect("tx");
        assert!(CaptureSnapshot::load(&tx, input.into()).is_err());
        drop(tx);
        assert_eq!(before, actual(&db, &tables()));
    }
}

#[test]
fn capture_rows_bind_request_identity_without_writes() {
    let (mut db, agent) = started_turn();
    let raw: String = db
        .query_row("SELECT record FROM analytics_turns LIMIT 1", [], |r| {
            r.get(0)
        })
        .expect("stored row");
    let row: Value = serde_json::from_str(&raw).expect("record");
    for field in ["id", "agentId", "threadId", "turnId", "rootId"] {
        let mut foreign = row.clone();
        foreign[field] = json!("foreign");
        db.execute("UPDATE analytics_turns SET record=?", [foreign.to_string()])
            .expect("foreign identity");
        let before = actual(&db, &tables());
        let prepared = PreparedCapture::event(
            &agent,
            "turn/completed",
            &json!({}),
            20.,
            200.,
            CaptureSource::Live,
        )
        .expect("request");
        let tx = db.transaction().expect("transaction");
        assert!(
            CaptureSnapshot::load(&tx, prepared.into()).is_err(),
            "{field}"
        );
        drop(tx);
        assert_eq!(actual(&db, &tables()), before);
    }
}

#[test]
fn claude_rows_replay_without_usage_writes() {
    use studio_runtime_store::analytics_capture::CaptureTable;
    let usage = json!({"tokenUsage":{"total":{"inputTokens":10,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":10,"reasoningOutputTokens":0,"totalTokens":20},"last":{"inputTokens":10,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":10,"reasoningOutputTokens":0,"totalTokens":20},"modelContextWindow":100},"usageSource":"claudeResponse","requestUsage":{"inputTokens":10,"outputTokens":4,"cachedInputTokens":2,"cacheWriteInputTokens":1},"responseId":"r1"});
    let (mut db, mut agent) = fixture();
    agent["provider"] = json!("claude");
    let prepare = |at| {
        PreparedCapture::event(
            &agent,
            "thread/tokenUsage/updated",
            &usage,
            at,
            200.,
            CaptureSource::Live,
        )
        .expect("request")
    };
    let plan = decide(CaptureSnapshot::load(&db, prepare(10.).into()).expect("snapshot"))
        .expect("decision");
    let tx = db.transaction().expect("tx");
    plan.apply(&tx).expect("apply");
    tx.commit().expect("commit");
    let before: String = db
        .query_row("SELECT record FROM analytics_usage", [], |r| r.get(0))
        .expect("saved row");
    // Fail on any INSERT/UPDATE attempt, even one restoring equal content.
    for table in ["analytics_usage", "analytics_meta", "analytics_usage_roots"] {
        db.execute_batch(&format!("CREATE TRIGGER reject_{table} BEFORE INSERT ON {table} BEGIN SELECT RAISE(ABORT,'duplicate usage write'); END;")).expect("write guard");
    }
    let plan =
        decide(CaptureSnapshot::load(&db, prepare(20.).into()).expect("snapshot")).expect("replay");
    assert!(
        !plan
            .rows
            .iter()
            .any(|(table, _)| *table == CaptureTable::Usage)
    );
    assert!(plan.generation_roots.is_empty());
    let tx = db.transaction().expect("tx");
    plan.apply(&tx).expect("no usage writes");
    tx.commit().expect("commit");
    assert_eq!(
        before,
        db.query_row("SELECT record FROM analytics_usage", [], |r| r
            .get::<_, String>(0))
            .expect("preserved bytes")
    );
}

#[test]
fn a_command_item_from_an_earlier_turn_still_captures_later_events() {
    // Live cutover 4: a terminalInteraction of a command started one turn
    // earlier failed with "/snapshot/row/turnId", stopped the account's
    // publication queue at its head, and filled it to the evidence bound.
    // Items are keyed by agent, thread and item id only.
    let (mut db, mut agent) = fixture();
    let thread = agent["threadId"].as_str().expect("thread").to_owned();
    let mut run = |turn: &str, method: &str, params: Value, at: f64| {
        agent["turnId"] = json!(turn);
        let mut params = params;
        params["threadId"] = json!(thread);
        params["turnId"] = json!(turn);
        let prepared = PreparedCapture::event(&agent, method, &params, at, at, CaptureSource::Live)
            .expect("request");
        let tx = db.transaction().expect("transaction");
        let decision = decide(CaptureSnapshot::load(&tx, prepared.into()).expect("snapshot"))
            .unwrap_or_else(|error| panic!("{method} in {turn}: {error}"));
        decision.apply(&tx).expect("apply");
        tx.commit().expect("commit");
    };
    run(
        "turn-a",
        "item/started",
        json!({"item":{"id":"exec-1","type":"commandExecution","command":"sleep 600","processId":"74036"}}),
        10.0,
    );
    run(
        "turn-b",
        "item/commandExecution/terminalInteraction",
        json!({"itemId":"exec-1","processId":"74036","stdin":""}),
        20.0,
    );
    run(
        "turn-b",
        "item/commandExecution/outputDelta",
        json!({"itemId":"exec-1","delta":"line one\n"}),
        21.0,
    );
    let raw = db
        .query_row(
            "SELECT record FROM analytics_items WHERE id LIKE '%:exec-1'",
            [],
            |row| row.get::<_, studio_runtime_store::stored_text::StoredText>(0),
        )
        .expect("item row")
        .into_string();
    let item: Value = serde_json::from_str(&raw).expect("item record");
    assert_eq!(item["turnId"], "turn-a", "the item keeps its starting turn");
    assert_eq!(item["stream"]["deltas"], 1, "the later delta is counted");
}
