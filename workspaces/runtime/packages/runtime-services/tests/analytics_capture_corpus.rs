#[path = "support/prepared_python.rs"]
mod prepared_python;
use rusqlite::{Connection, params_from_iter, types::Value as SqlValue};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use studio_runtime_services::analytics_capture::{
    CaptureSnapshot, CaptureSource, PreparedCapture, decide,
};
use studio_runtime_services::analytics_rollout::{RolloutAction, RolloutContext};

fn oracle() -> &'static Value {
    static DATA: OnceLock<Value> = OnceLock::new();
    DATA.get_or_init(|| {
        let python = prepared_python::prepared_python(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
        );
        let out = Command::new(python)
            .arg(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/analytics_capture/oracle.py"),
            )
            .output()
            .expect("Python oracle");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("oracle JSON")
    })
}
fn sql(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::String(s) => SqlValue::Text(s.clone()),
        Value::Number(n) => n
            .as_i64()
            .map(SqlValue::Integer)
            .unwrap_or_else(|| SqlValue::Real(n.as_f64().expect("number"))),
        _ => panic!("SQL scalar"),
    }
}
fn seed(db: &Connection, rows: &Value) {
    for (table, rows) in rows.as_object().expect("tables") {
        db.execute(&format!("DELETE FROM {table}"), [])
            .expect("clear");
        for row in rows.as_array().expect("rows") {
            let row = row.as_object().expect("row");
            let fields = row.keys().cloned().collect::<Vec<_>>().join(",");
            let marks = vec!["?"; row.len()].join(",");
            db.execute(
                &format!("INSERT INTO {table}({fields}) VALUES({marks})"),
                params_from_iter(row.values().map(sql)),
            )
            .expect("seed Python-written row");
        }
    }
}
#[path = "analytics_capture/common.rs"]
mod common;
use common::{actual, compare};
#[test]
fn real_python_capture_rows_match_complete_rust_outputs() {
    let data = oracle();
    let mut failures = Vec::new();
    for case in data["capture"].as_array().expect("corpus") {
        let mut db = Connection::open_in_memory().expect("db");
        db.execute_batch(data["template"].as_str().expect("Python schema"))
            .expect("schema");
        seed(&db, &case["before"]);
        let step = &case["step"];
        let source = if step["source"] == "rollout" {
            CaptureSource::Rollout
        } else {
            CaptureSource::Live
        };
        let q = if step["model"] == true {
            PreparedCapture::model_payload(
                &case["agent"],
                &step["params"],
                step["at"].as_f64().expect("time"),
                200.,
            )
        } else {
            PreparedCapture::event(
                &case["agent"],
                step["method"].as_str().expect("method"),
                &step["params"],
                step["at"].as_f64().expect("time"),
                200.,
                source,
            )
        }
        .expect("checked input");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let snapshot = CaptureSnapshot::load(&db, q.into()).expect("Python rows validate");
            let plan = decide(snapshot).expect("pure decision");
            let tx = db.transaction().expect("tx");
            plan.apply(&tx).expect("apply");
            tx.commit().expect("commit");
            let name = case["name"].as_str().expect("name");
            let fingerprint_path = format!("{name}/analytics_usage/0/record/fingerprint");
            let canonical = case["canonicalClaudeFingerprint"].as_str().map(|_| {
                (
                    fingerprint_path.as_str(),
                    &case["canonicalClaudeFingerprint"],
                )
            });
            common::compare_with_fingerprint(
                &actual(&db, &case["after"]),
                &case["after"],
                name,
                canonical,
            );
        }));
        if result.is_err() {
            failures.push(case["name"].clone());
        }
    }
    assert!(failures.is_empty(), "failed cases {failures:?}");
}
#[test]
fn real_python_rollout_actions_and_context_match() {
    for case in oracle()["rollout"].as_array().expect("corpus") {
        let mut context = RolloutContext::from_record(case["context"].clone())
            .expect("checked persisted context");
        let actions = context
            .actions(
                &case["record"],
                case["identity"].as_str().expect("identity"),
                50.,
            )
            .expect("actions");
        let actual: Vec<Value> = actions
            .into_iter()
            .map(|a| match a {
                RolloutAction::Event { method, params, at } => json!(["event", method, params, at]),
                RolloutAction::Payload { params, at } => json!(["payload", null, params, at]),
                RolloutAction::WrongThread => json!([
                    "coverage",
                    "wrongThreadRecord",
                    {},
                    case["record"]["timestamp"]
                ]),
            })
            .collect();
        compare(
            &json!(actual),
            &case["actions"],
            case["name"].as_str().expect("name"),
        );
        compare(context.record(), &case["after"], "context");
    }
}
#[test]
fn malformed_stored_rows_refuse_without_writes() {
    let data = oracle();
    let case = &data["capture"][1];
    let mut db = Connection::open_in_memory().expect("db");
    db.execute_batch(data["template"].as_str().expect("schema"))
        .expect("schema");
    seed(&db, &case["before"]);
    for invalid in [
        json!({}),
        json!({"id":"a:thread:turn","agentId":"a","at":0,"startedAt":"bad","finishedAt":null,"firstOutputAt":null,"status":"running"}),
    ] {
        db.execute("UPDATE analytics_turns SET record=?", [invalid.to_string()])
            .expect("corrupt fixture");
        let before = actual(&db, &case["before"]);
        let input = PreparedCapture::event(
            &case["agent"],
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
        assert_eq!(before, actual(&db, &case["before"]));
    }
}

#[test]
fn capture_rows_bind_request_identity_without_writes() {
    let data = oracle();
    let case = &data["capture"][1];
    let mut db = Connection::open_in_memory().expect("db");
    db.execute_batch(data["template"].as_str().expect("schema"))
        .expect("schema");
    seed(&db, &case["before"]);
    let raw: String = db
        .query_row("SELECT record FROM analytics_turns LIMIT 1", [], |r| {
            r.get(0)
        })
        .expect("Python row");
    let row: Value = serde_json::from_str(&raw).expect("record");
    for field in ["id", "agentId", "threadId", "turnId", "rootId"] {
        let mut foreign = row.clone();
        foreign[field] = json!("foreign");
        db.execute("UPDATE analytics_turns SET record=?", [foreign.to_string()])
            .expect("foreign identity");
        let before = actual(&db, &case["before"]);
        let prepared = PreparedCapture::event(
            &case["agent"],
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
        assert_eq!(actual(&db, &case["before"]), before);
    }
}

#[test]
fn claude_python_and_canonical_rust_rows_replay_without_usage_writes() {
    use studio_runtime_store::analytics_capture::CaptureTable;
    let data = oracle();
    let case = data["capture"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["name"] == "claude-response/0")
        .expect("Claude case");
    for python_written in [true, false] {
        let mut db = Connection::open_in_memory().expect("db");
        db.execute_batch(data["template"].as_str().expect("schema"))
            .expect("schema");
        seed(
            &db,
            if python_written {
                &case["after"]
            } else {
                &case["before"]
            },
        );
        let prepare = |at| {
            PreparedCapture::event(
                &case["agent"],
                "thread/tokenUsage/updated",
                &case["step"]["params"],
                at,
                200.,
                CaptureSource::Live,
            )
            .expect("request")
        };
        if !python_written {
            let plan = decide(CaptureSnapshot::load(&db, prepare(10.).into()).expect("snapshot"))
                .expect("decision");
            let tx = db.transaction().expect("tx");
            plan.apply(&tx).expect("apply");
            tx.commit().expect("commit");
        }
        let before: String = db
            .query_row("SELECT record FROM analytics_usage", [], |r| r.get(0))
            .expect("saved row");
        let value: Value = serde_json::from_str(&before).expect("row");
        if !python_written {
            assert_eq!(value["fingerprint"], case["canonicalClaudeFingerprint"]);
        }
        // Fail on any INSERT/UPDATE attempt, even one restoring equal content.
        for table in ["analytics_usage", "analytics_meta", "analytics_usage_roots"] {
            db.execute_batch(&format!("CREATE TRIGGER reject_{table} BEFORE INSERT ON {table} BEGIN SELECT RAISE(ABORT,'duplicate usage write'); END;")).expect("write guard");
        }
        let plan = decide(CaptureSnapshot::load(&db, prepare(20.).into()).expect("snapshot"))
            .expect("replay");
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
}

#[test]
fn a_command_item_from_an_earlier_turn_still_captures_later_events() {
    // Live cutover 4: a terminalInteraction of a command started one turn
    // earlier failed with "/snapshot/row/turnId", stopped the account's
    // publication queue at its head, and filled it to the evidence bound.
    // Python keys items by agent, thread and item id only.
    let data = oracle();
    let mut db = Connection::open_in_memory().expect("db");
    db.execute_batch(data["template"].as_str().expect("schema"))
        .expect("schema");
    let mut agent = data["capture"][1]["agent"].clone();
    let thread = agent["threadId"].as_str().expect("thread").to_owned();
    let mut run = |turn: &str, method: &str, params: Value, at: f64| {
        agent["turnId"] = json!(turn);
        let mut params = params;
        params["threadId"] = json!(thread);
        params["turnId"] = json!(turn);
        let prepared =
            PreparedCapture::event(&agent, method, &params, at, at, CaptureSource::Live)
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
    let raw: String = db
        .query_row(
            "SELECT record FROM analytics_items WHERE id LIKE '%:exec-1'",
            [],
            |row| row.get(0),
        )
        .expect("item row");
    let item: Value = serde_json::from_str(&raw).expect("item record");
    assert_eq!(item["turnId"], "turn-a", "the item keeps its starting turn");
    assert_eq!(item["stream"]["deltas"], 1, "the later delta is counted");
}
