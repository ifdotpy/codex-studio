//! Complete Python-written rows through the transaction adapter, without providers.
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};
use studio_runtime_services::{
    budget::{CaptureSource, HistoryFileObservation, HistoryObservation},
    budget_service,
};
struct Oracle {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Oracle {
    fn new() -> Self {
        let mut child = Command::new(
            std::env::var_os("CODEX_AGENTS_PYTHON").expect("prepared isolated Python"),
        )
        .arg("-B")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/budget/oracle.py"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("oracle");
        Self {
            input: child.stdin.take().expect("stdin"),
            output: BufReader::new(child.stdout.take().expect("stdout")),
            child,
        }
    }
    fn call(&mut self, case: &Value) -> Value {
        writeln!(self.input, "{case}").expect("case");
        self.input.flush().expect("flush");
        let mut s = String::new();
        self.output.read_line(&mut s).expect("result");
        serde_json::from_str(&s).expect("oracle result")
    }
}
impl Drop for Oracle {
    fn drop(&mut self) {
        let _ignored = writeln!(self.input, "{{\"close\":true}}");
        let _ignored = self.input.flush();
        assert!(self.child.wait().expect("reap").success());
    }
}
fn db(records: &Value, case: &Value) -> Connection {
    let connection = Connection::open_in_memory().expect("SQLite");
    connection.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);CREATE TABLE runtime_budget(id TEXT PRIMARY KEY,record TEXT NOT NULL);CREATE TABLE runtime_budget_usage(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,tokens INTEGER,at REAL);CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,created REAL,epoch INTEGER,turn_id TEXT,error TEXT);CREATE TABLE runtime_native_action_receipts(id TEXT PRIMARY KEY,signature TEXT,receipt TEXT,outcome TEXT);").expect("schema");
    for (name, table) in [("agents", "runtime_agents"), ("states", "runtime_budget")] {
        for (id, row) in records[name].as_object().expect("records") {
            connection
                .execute(
                    &format!("INSERT INTO {table} VALUES (?1,?2)"),
                    params![id, row.to_string()],
                )
                .expect("Python row");
        }
    }
    for row in records["usage"].as_array().expect("usage") {
        connection
            .execute(
                "INSERT INTO runtime_budget_usage VALUES (?1,?2,?3,?4,?5)",
                params![
                    row["id"].as_str(),
                    row["agent"].as_str(),
                    row["kind"].as_str(),
                    row["tokens"].as_i64(),
                    row["at"].as_f64()
                ],
            )
            .expect("usage");
    }
    for row in records["events"].as_array().expect("events") {
        connection
            .execute(
                "INSERT INTO runtime_events VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    row["id"].as_str(),
                    row["agent"].as_str(),
                    row["kind"].as_str(),
                    row["text"].as_str(),
                    row["status"].as_str(),
                    row["created"].as_f64(),
                    row["epoch"].as_i64(),
                    row["turn_id"].as_str(),
                    row["error"].as_str()
                ],
            )
            .expect("event");
    }
    if let Some(receipts) = case["receipts"].as_object() {
        for (id, row) in receipts {
            connection
                .execute(
                    "INSERT INTO runtime_native_action_receipts VALUES (?1,'fixture',?2,?3)",
                    params![id, row["receipt"].to_string(), row["outcome"].to_string()],
                )
                .expect("receipt");
        }
    }
    connection
}
fn stored(connection: &Connection) -> Value {
    let mut state = json!({"usage":[],"events":[]});
    for (name, table, column) in [
        ("agents", "runtime_agents", "record"),
        ("states", "runtime_budget", "record"),
        ("receipts", "runtime_native_action_receipts", "outcome"),
    ] {
        let mut result = serde_json::Map::new();
        let mut query = connection
            .prepare(&format!("SELECT id,{column} FROM {table} ORDER BY id"))
            .expect("query");
        let rows = query
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .expect("rows");
        for row in rows {
            let (id, raw) = row.expect("row");
            result.insert(id, serde_json::from_str(&raw).expect("record"));
        }
        state[name] = json!(result);
    }
    let mut query = connection
        .prepare("SELECT id FROM runtime_budget_usage ORDER BY id")
        .expect("query");
    state["usage"] = json!(
        query
            .query_map([], |r| r.get::<_, String>(0))
            .expect("rows")
            .map(|id| studio_runtime_store::budget::BudgetRepository::usage(
                connection,
                &id.expect("id")
            )
            .expect("usage")
            .expect("row"))
            .collect::<Vec<_>>()
    );
    let mut query = connection
        .prepare("SELECT id FROM runtime_events ORDER BY id")
        .expect("query");
    let ids = query
        .query_map([], |r| r.get::<_, String>(0))
        .expect("rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("ids");
    state["events"] = json!(
        studio_runtime_store::budget::BudgetRepository::events(connection, &ids).expect("events")
    );
    state
}
fn run(case: &Value, records: &Value) -> Value {
    let mut connection = db(records, case);
    let mut denial = None;
    let mut outputs = Vec::new();
    for step in case["steps"].as_array().expect("steps") {
        let tx = connection.transaction().expect("transaction");
        let id = step["agent"].as_str().unwrap_or("a");
        let now = step["now"].as_f64().unwrap_or(100.);
        let result = match step["op"].as_str().expect("op") {
            "capture" => {
                let mut agent = step.get("historical_agent").cloned().unwrap_or_else(|| {
                    studio_runtime_store::budget::BudgetRepository::agent(&tx, id).expect("agent")
                });
                json!(
                    budget_service::capture(
                        &tx,
                        &mut agent,
                        &step["params"],
                        step["at"].as_f64().unwrap_or(now),
                        if step["source"] == "history" {
                            CaptureSource::History
                        } else {
                            CaptureSource::Live
                        },
                        now
                    )
                    .expect("capture")
                )
            }
            "update" => {
                let mut agent =
                    studio_runtime_store::budget::BudgetRepository::agent(&tx, id).expect("agent");
                if let Some(fields) = step["set"].as_object() {
                    for (key, value) in fields {
                        agent[key] = value.clone();
                    }
                }
                if let Some(fields) = step["paths"].as_object() {
                    for (path, value) in fields {
                        *agent.pointer_mut(path).expect("existing fixture path") = value.clone();
                    }
                }
                if let Some(fields) = step["remove"].as_array() {
                    for key in fields {
                        agent
                            .as_object_mut()
                            .expect("agent")
                            .remove(key.as_str().expect("key"));
                    }
                }
                studio_runtime_store::budget::BudgetRepository::save_agent(&tx, &agent)
                    .expect("update");
                Value::Null
            }
            "admission" | "status" => {
                let mut histories = BTreeMap::new();
                // The Python template has an analytics table; a missing row is
                // incomplete, while a missing table/observation is unavailable.
                for id in records["agents"].as_object().expect("agents").keys() {
                    let agent = studio_runtime_store::budget::BudgetRepository::agent(&tx, id)
                        .expect("agent");
                    let empty = json!({});
                    histories.insert(
                        id.clone(),
                        HistoryObservation::from_records(
                            &agent,
                            if step["history_table"] == false {
                                None
                            } else {
                                Some(&empty)
                            },
                            None,
                        )
                        .expect("missing history"),
                    );
                }
                if let Some(values) = step["histories"].as_object() {
                    for (id, value) in values {
                        let agent = studio_runtime_store::budget::BudgetRepository::agent(&tx, id)
                            .expect("agent");
                        let file: Option<HistoryFileObservation> = value
                            .get("file")
                            .filter(|v| !v.is_null())
                            .map(|v| serde_json::from_value(v.clone()).expect("file observation"));
                        histories.insert(
                            id.clone(),
                            HistoryObservation::from_records(
                                &agent,
                                Some(
                                    value
                                        .get("history")
                                        .filter(|v| !v.is_null())
                                        .unwrap_or(&json!({})),
                                ),
                                file.as_ref(),
                            )
                            .expect("history"),
                        );
                    }
                }
                if step["op"] == "status" {
                    json!(
                        budget_service::status(
                            &tx,
                            id,
                            &histories,
                            step["check_coverage"].as_bool().unwrap_or(true),
                            now
                        )
                        .expect("status")
                    )
                } else {
                    let decision =
                        budget_service::admission(&tx, id, &histories, now).expect("admission");
                    denial = decision.denial.clone();
                    json!(decision)
                }
            }
            "defer" => {
                let explicit = step
                    .get("denial")
                    .map(|v| serde_json::from_value(v.clone()).expect("denial"));
                json!(
                    budget_service::defer_wait(
                        &tx,
                        id,
                        step["attempt_id"].as_str().unwrap_or("attempt"),
                        explicit.as_ref().or(denial.as_ref()).expect("denial"),
                        step["unknown"].as_bool().unwrap_or(false),
                        false,
                        now
                    )
                    .expect("defer")
                )
            }
            "claim" => json!(budget_service::claim_wait(&tx, id, false, now).expect("claim")),
            _ => panic!("unknown operation"),
        };
        tx.commit().expect("commit");
        outputs.push(json!({"result":result,"stored":stored(&connection)}));
    }
    json!(outputs)
}
#[test]
fn budget_python_corpus() {
    let cases: Value = serde_json::from_str(include_str!("budget/cases.json")).expect("cases");
    let mut oracle = Oracle::new();
    for case in cases.as_array().expect("cases") {
        let expected = oracle.call(case);
        compare(
            &run(case, &expected["records"]),
            &expected["outputs"],
            case["name"].as_str().expect("name"),
        );
    }
}

fn compare(actual: &Value, expected: &Value, path: &str) {
    match (actual, expected) {
        (Value::Object(a), Value::Object(b)) => {
            assert_eq!(
                a.keys().collect::<Vec<_>>(),
                b.keys().collect::<Vec<_>>(),
                "{path}: keys"
            );
            for (key, value) in a {
                compare(value, &b[key], &format!("{path}/{key}"));
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len(), "{path}: length");
            for (index, (a, b)) in a.iter().zip(b).enumerate() {
                compare(a, b, &format!("{path}/{index}"));
            }
        }
        (Value::Number(a), Value::Number(b))
            if ["/created", "/cutoff", "/noticeAt", "/at", "/finishedAt"]
                .iter()
                .any(|key| path.ends_with(key)) =>
        {
            // Timestamp schemas accept integer and floating JSON numbers. The
            // one-way cutover does not preserve Python's stored number spelling.
            assert_eq!(a.as_f64(), b.as_f64(), "{path}");
        }
        _ => assert_eq!(actual, expected, "{path}"),
    }
}

fn fixture() -> (Value, Value) {
    let cases: Value = serde_json::from_str(include_str!("budget/cases.json")).expect("cases");
    let case = cases[0].clone();
    let mut oracle = Oracle::new();
    let result = oracle.call(&case);
    (case, result["records"].clone())
}
fn sync_fixture(db: &Connection) {
    db.execute_batch("CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,PRIMARY KEY(collection,id));CREATE TABLE sync_documents(seq INTEGER);CREATE TABLE sync_versions(seq INTEGER);INSERT INTO sync_entities VALUES('agent','a',1,'initial','{\"collection\":\"agent\",\"id\":\"a\",\"value\":{\"id\":\"a\",\"name\":\"Keep presentation\",\"source\":\"native\",\"kind\":\"agent\",\"canSend\":true}}',0);").expect("sync fixture");
}
fn all_stored(db: &Connection) -> Value {
    let sync: Option<String> = db
        .query_row("SELECT payload FROM sync_entities WHERE id='a'", [], |r| {
            r.get(0)
        })
        .expect("sync");
    json!({"stored":stored(db),"sync":sync.map(|v|serde_json::from_str::<Value>(&v).expect("sync")),"sequence":db.query_row("SELECT seq FROM sync_entities WHERE id='a'",[],|r|r.get::<_,i64>(0)).expect("seq")})
}
#[test]
fn capture_failpoints_restart_and_lost_response() {
    let (case, records) = fixture();
    let params = &case["steps"][0]["params"];
    let temporary = tempfile::tempdir().expect("isolated directory");
    for table in [
        "runtime_budget_usage",
        "runtime_budget",
        "runtime_agents",
        "sync_entities",
    ] {
        for point in ["BEFORE", "AFTER"] {
            let mut connection = db(&records, &case);
            sync_fixture(&connection);
            let before = all_stored(&connection);
            let operation = if matches!(table, "runtime_agents" | "sync_entities") {
                "UPDATE"
            } else {
                "INSERT"
            };
            connection.execute_batch(&format!("CREATE TRIGGER fail_point {point} {operation} ON {table} BEGIN SELECT RAISE(ABORT,'injected'); END")).expect("failpoint");
            let tx = connection.transaction().expect("transaction");
            let mut agent = records["agents"]["a"].clone();
            assert!(
                budget_service::capture(&tx, &mut agent, params, 101., CaptureSource::Live, 100.)
                    .is_err(),
                "{table}/{point}"
            );
            assert_eq!(
                agent, records["agents"]["a"],
                "caller copy must remain unchanged"
            );
            tx.commit()
                .expect("caller can commit other work after savepoint rollback");
            assert_eq!(all_stored(&connection), before, "{table}/{point}");
            connection
                .execute_batch("DROP TRIGGER fail_point")
                .expect("remove fault");
            let path = temporary.path().join(format!("{table}-{point}.sqlite3"));
            if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some() {
                connection
                    .execute("VACUUM INTO ?1", [path.to_str().expect("path")])
                    .expect("durable restart state");
                drop(connection);
                connection = Connection::open(&path).expect("restart");
            }
            for _ in 0..2 {
                let tx = connection.transaction().expect("transaction");
                let mut agent =
                    studio_runtime_store::budget::BudgetRepository::agent(&tx, "a").expect("agent");
                assert_eq!(
                    budget_service::capture(
                        &tx,
                        &mut agent,
                        params,
                        101.,
                        CaptureSource::Live,
                        100.
                    )
                    .expect("retry"),
                    20
                );
                tx.commit().expect("commit; acknowledgement may be lost");
                if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some() {
                    drop(connection);
                    connection =
                        Connection::open(&path).expect("restart after lost acknowledgement");
                }
            }
            assert_eq!(
                stored(&connection)["usage"]
                    .as_array()
                    .expect("usage")
                    .len(),
                1
            );
            assert_eq!(
                all_stored(&connection)["sync"]["value"]["name"],
                "Keep presentation"
            );
            assert_eq!(all_stored(&connection)["sync"]["value"]["tokensUsed"], 20);
        }
    }
}
#[test]
fn capture_rejects_stale_durable_identity() {
    let (case, records) = fixture();
    for source in [CaptureSource::Live, CaptureSource::History] {
        for initialized in [false, true] {
            for (field, value) in [
                ("rootId", json!("other-root")),
                ("accountKey", json!("other-account")),
                ("threadId", json!("other-thread")),
                ("turnId", json!("other-turn")),
                ("epoch", json!(1)),
            ] {
                let mut connection = db(&records, &case);
                sync_fixture(&connection);
                let mut supplied = records["agents"]["a"].clone();
                if initialized {
                    let tx = connection.transaction().expect("seed transaction");
                    budget_service::capture(
                        &tx,
                        &mut supplied,
                        &case["steps"][0]["params"],
                        101.,
                        source,
                        100.,
                    )
                    .expect("initial capture");
                    tx.commit().expect("seed commit");
                }
                let supplied_before = supplied.clone();
                let tx = connection.transaction().expect("transaction");
                // Change the durable identity inside the same writer transaction.
                // A pre-transaction check of the supplied copy cannot detect this.
                let mut durable = supplied.clone();
                durable[field] = value;
                studio_runtime_store::budget::BudgetRepository::save_agent(&tx, &durable)
                    .expect("durable identity changed");
                let before = all_stored(&tx);
                let result = budget_service::capture(
                    &tx,
                    &mut supplied,
                    &case["steps"][0]["params"],
                    101.,
                    source,
                    100.,
                );
                assert!(
                    matches!(result, Err(studio_runtime_services::budget::BudgetError::Invalid(ref location))
                    if location == &format!("runtime_agents/{field}/identity")),
                    "{field}: {result:?}"
                );
                assert_eq!(supplied, supplied_before, "refusal must not patch caller");
                tx.commit().expect("commit surrounding identity change");
                assert_eq!(all_stored(&connection), before, "{field}: no budget writes");
            }
        }
    }
}

#[test]
fn capture_partial_copy_uses_durable_identity() {
    let (mut case, _) = fixture();
    case["agents"][0]["turnId"] = json!("durable-turn");
    case["agents"][0]["created"] = json!(50);
    case["steps"] = json!([{"op":"capture", "at":101, "params":{
        "tokenUsage":{"total":{"totalTokens":20},"last":{"totalTokens":20}}
    }}]);
    let mut oracle = Oracle::new();
    for source in [CaptureSource::Live, CaptureSource::History] {
        case["steps"][0]["source"] = json!(source);
        let expected = oracle.call(&case);
        let mut connection = db(&expected["records"], &case);
        let mut supplied = json!({"id":"a", "created":999, "notificationField":"preserved"});
        for _ in 0..2 {
            let tx = connection.transaction().expect("transaction");
            assert_eq!(
                budget_service::capture(
                    &tx,
                    &mut supplied,
                    &case["steps"][0]["params"],
                    101.,
                    source,
                    100.
                )
                .expect("partial agent capture"),
                20
            );
            tx.commit().expect("commit");
            compare(
                &stored(&connection),
                &expected["outputs"][0]["stored"],
                "durable identity",
            );
        }
        assert_eq!(supplied["created"], 999);
        assert_eq!(supplied["notificationField"], "preserved");
        assert!(supplied.get("accountKey").is_none());
        assert_eq!(supplied["tokensUsed"], 20);
    }
}

#[test]
fn malformed_budget_inputs_refuse_without_writes() {
    let (case, records) = fixture();
    for field in [
        "tokenBudget",
        "tokensUsed",
        "accountKey",
        "epoch",
        "autoWake",
    ] {
        for value in [json!({}), json!([]), json!(-1), json!("bad")] {
            let mut records = records.clone();
            records["agents"]["a"][field] = value;
            let mut connection = db(&records, &case);
            let before = stored(&connection);
            let tx = connection.transaction().expect("transaction");
            let mut agent = records["agents"]["a"].clone();
            let result = budget_service::capture(
                &tx,
                &mut agent,
                &case["steps"][0]["params"],
                101.,
                CaptureSource::Live,
                100.,
            );
            // Strings are valid account identities; every other seeded value is malformed.
            if field == "accountKey" && agent[field].is_string() {
                assert!(result.is_ok());
                continue;
            }
            assert!(result.is_err(), "{field}: {}", agent[field]);
            tx.commit().expect("commit refusal");
            assert_eq!(stored(&connection), before);
        }
    }
    for value in [
        json!(true),
        json!(-1),
        json!(1.5),
        json!("10"),
        json!(9_007_199_254_740_992_u64),
    ] {
        let mut connection = db(&records, &case);
        let before = stored(&connection);
        let tx = connection.transaction().expect("transaction");
        let mut agent = records["agents"]["a"].clone();
        let mut params = case["steps"][0]["params"].clone();
        params["requestUsage"]["totalTokens"] = value;
        assert!(
            budget_service::capture(&tx, &mut agent, &params, 101., CaptureSource::Live, 100.)
                .is_err()
        );
        tx.commit().expect("commit refusal");
        assert_eq!(stored(&connection), before);
    }
}

#[test]
fn wait_failpoints_preserve_batch_and_exact_resume() {
    let cases: Value = serde_json::from_str(include_str!("budget/cases.json")).expect("cases");
    let case = cases
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["name"] == "wait-resume")
        .expect("wait case");
    let mut oracle = Oracle::new();
    let expected = oracle.call(case);
    let initial = &expected["outputs"][0]["stored"];
    let denial = serde_json::from_value(expected["outputs"][0]["result"]["denial"].clone())
        .expect("real Python denial");
    let temporary = tempfile::tempdir().expect("isolated directory");
    for phase in ["defer", "claim"] {
        for table in ["runtime_events", "runtime_agents", "sync_entities"] {
            for point in ["BEFORE", "AFTER"] {
                let mut connection = db(initial, case);
                sync_fixture(&connection);
                if phase == "claim" {
                    let tx = connection.transaction().expect("transaction");
                    assert!(
                        budget_service::defer_wait(
                            &tx, "a", "attempt", &denial, false, false, 100.
                        )
                        .expect("defer")
                    );
                    tx.commit().expect("commit");
                }
                let before = all_stored(&connection);
                connection.execute_batch(&format!("CREATE TRIGGER fail_point {point} UPDATE ON {table} BEGIN SELECT RAISE(ABORT,'injected'); END")).expect("failpoint");
                let tx = connection.transaction().expect("transaction");
                let failed = if phase == "defer" {
                    budget_service::defer_wait(&tx, "a", "attempt", &denial, false, false, 100.)
                        .is_err()
                } else {
                    budget_service::claim_wait(&tx, "a", false, 101.).is_err()
                };
                assert!(failed, "{phase}/{table}/{point}");
                tx.commit().expect("commit rolled-back scope");
                assert_eq!(all_stored(&connection), before, "{phase}/{table}/{point}");
                connection
                    .execute_batch("DROP TRIGGER fail_point")
                    .expect("remove fault");
                let path = temporary
                    .path()
                    .join(format!("{phase}-{table}-{point}.sqlite3"));
                if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some() {
                    connection
                        .execute("VACUUM INTO ?1", [path.to_str().expect("path")])
                        .expect("persist");
                    drop(connection);
                    connection = Connection::open(path).expect("restart");
                }
                let tx = connection.transaction().expect("transaction");
                if phase == "defer" {
                    assert!(
                        budget_service::defer_wait(
                            &tx, "a", "attempt", &denial, false, false, 100.
                        )
                        .expect("retry defer")
                    );
                }
                let job = budget_service::claim_wait(&tx, "a", false, 101.)
                    .expect("claim")
                    .expect("job");
                assert_eq!(job["attempt"]["id"], "attempt");
                assert_eq!(job["rows"][0]["text"], "Exact input");
                assert!(
                    budget_service::claim_wait(&tx, "a", false, 102.)
                        .expect("repeat claim")
                        .is_none()
                );
                tx.commit().expect("commit");
            }
        }
    }
}

#[test]
fn budget_full_matrix() {
    if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_none() {
        return;
    }
    let cases: Value = serde_json::from_str(include_str!("budget/cases.json")).expect("cases");
    let mut oracle = Oracle::new();
    let mut count = 0;
    for floor in [0, 1, 10, 100] {
        for amount in [0, 1, 10, 100] {
            for at in [0., 99., 100., 101.] {
                for history in [false, true] {
                    for estimated in [false, true] {
                        for response_first in [false, true] {
                            let mut case = cases[0].clone();
                            case["agents"][0]["tokensUsed"] = json!(floor);
                            case["agents"][0]["tokenBudget"] = json!(100);
                            let response = json!({"op":"capture","at":at,"source":if history{"history"}else{"live"},"params":{"responseId":"r","rawTokenUsageRecord":{"response_id":"r"},"requestUsage":{"totalTokens":amount},"_analyticsTimestampSource":if estimated{"fileModifiedEstimate"}else{"record"}}});
                            let notice = json!({"op":"capture","at":at,"params":{"tokenUsage":{"total":{"totalTokens":floor+amount},"last":{"totalTokens":amount}}}});
                            case["steps"] = if response_first {
                                json!([response,notice,response,{"op":"admission"}])
                            } else {
                                json!([notice,response,response,{"op":"admission"}])
                            };
                            let expected = oracle.call(&case);
                            compare(
                                &run(&case, &expected["records"]),
                                &expected["outputs"],
                                &format!(
                                    "matrix floor={floor} amount={amount} at={at} history={history} estimated={estimated} response_first={response_first}"
                                ),
                            );
                            count += 1;
                        }
                    }
                }
            }
        }
    }
    eprintln!("budget full accounting matrix: {count} cases");
}

#[test]
fn malformed_stored_ledgers_and_late_team_read_roll_back() {
    let (case, records) = fixture();
    let mut oracle = Oracle::new();
    let written = oracle.call(&case);
    let state = &written["outputs"][0]["stored"]["states"]["a"];
    for field in [
        "cutoff",
        "floor",
        "spent",
        "before",
        "after",
        "historicalNotices",
        "noticeSpent",
        "noticeAt",
        "created",
        "ambiguousNotices",
        "faults",
    ] {
        for value in [Value::Null, json!(false), json!({}), json!("bad")] {
            let mut records = records.clone();
            records["states"]["a"] = state.clone();
            records["states"]["a"][field] = value;
            let mut connection = db(&records, &case);
            let before = stored(&connection);
            let tx = connection.transaction().expect("transaction");
            let mut agent = records["agents"]["a"].clone();
            assert!(
                budget_service::capture(
                    &tx,
                    &mut agent,
                    &case["steps"][0]["params"],
                    101.,
                    CaptureSource::Live,
                    100.
                )
                .is_err(),
                "{field}"
            );
            tx.commit().expect("commit refused scope");
            assert_eq!(stored(&connection), before);
        }
    }
    // The root initialization writes before the malformed worker is read. The
    // scope must undo those earlier writes even if the outer transaction commits.
    let mut records = records;
    records["agents"]["a"]["tokenBudget"] = json!(100);
    records["agents"]["worker"] = records["agents"]["a"].clone();
    records["agents"]["worker"]["id"] = json!("worker");
    records["states"]["worker"] = json!({"spent":"bad"});
    let mut connection = db(&records, &case);
    let before = stored(&connection);
    let tx = connection.transaction().expect("transaction");
    assert!(budget_service::admission(&tx, "a", &BTreeMap::new(), 100.).is_err());
    tx.commit().expect("commit refused scope");
    assert_eq!(stored(&connection), before);
}
#[test]
fn usage_state_check_refuses_bad_scalar_rows_without_writing() {
    let (case, records) = fixture();
    let connection = db(&records, &case);
    connection
        .execute(
            "INSERT INTO runtime_budget_usage VALUES ('bad','a','response',-1,1)",
            [],
        )
        .expect("bad existing row");
    let changes = connection.total_changes();
    let report =
        studio_runtime_store::budget::check_usage_readonly(&connection).expect("read-only check");
    assert_eq!(report.row_count, 1);
    assert_eq!(report.rejected_count, 1);
    assert_eq!(report.failures[0].pointer, "/tokens");
    assert_eq!(connection.total_changes(), changes);
}

#[test]
fn action_retirement_failpoints_keep_receipt_and_wait_atomic() {
    let cases: Value = serde_json::from_str(include_str!("budget/cases.json")).expect("cases");
    let case = cases
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["name"] == "action-stale")
        .expect("action case");
    let expected = Oracle::new().call(case);
    let initial = &expected["outputs"][2]["stored"];
    let temporary = tempfile::tempdir().expect("isolated directory");
    for table in [
        "runtime_native_action_receipts",
        "runtime_agents",
        "sync_entities",
    ] {
        for point in ["BEFORE", "AFTER"] {
            let mut connection = db(initial, case);
            sync_fixture(&connection);
            let before = all_stored(&connection);
            connection.execute_batch(&format!("CREATE TRIGGER fail_point {point} UPDATE ON {table} BEGIN SELECT RAISE(ABORT,'injected'); END")).expect("failpoint");
            let tx = connection.transaction().expect("transaction");
            assert!(budget_service::claim_wait(&tx, "a", false, 100.).is_err());
            tx.commit().expect("commit refused scope");
            assert_eq!(all_stored(&connection), before, "{table}/{point}");
            connection
                .execute_batch("DROP TRIGGER fail_point")
                .expect("remove fault");
            if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some() {
                let path = temporary
                    .path()
                    .join(format!("action-{table}-{point}.sqlite3"));
                connection
                    .execute("VACUUM INTO ?1", [path.to_str().expect("path")])
                    .expect("persist");
                drop(connection);
                connection = Connection::open(path).expect("restart");
            }
            let tx = connection.transaction().expect("transaction");
            assert!(
                budget_service::claim_wait(&tx, "a", false, 100.)
                    .expect("retry")
                    .is_none()
            );
            tx.commit().expect("commit");
            compare(
                &stored(&connection),
                &expected["outputs"][3]["stored"],
                "action retirement",
            );
        }
    }
}

#[test]
fn incomplete_collected_tail_cannot_authorize_admission() {
    let (case, records) = fixture();
    let mut connection = db(&records, &case);
    let mut agent = records["agents"]["a"].clone();
    agent["tokenBudget"] = json!(100);
    agent["tokensUsed"] = json!(10);
    let tx = connection.transaction().expect("transaction");
    studio_runtime_store::budget::BudgetRepository::save_agent(&tx, &agent).expect("agent");
    let history = json!({"status":"current", "identity":[1,2], "offset":0, "context":{"requestUsageAvailable":true}});
    let file = HistoryFileObservation {
        identity: [1, 2],
        size: 100,
        final_size: 100,
        tail: "{}\n".into(),
    };
    let observation =
        HistoryObservation::from_records(&agent, Some(&history), Some(&file)).expect("observation");
    let decision =
        budget_service::admission(&tx, "a", &BTreeMap::from([("a".into(), observation)]), 100.)
            .expect("admission");
    assert_eq!(
        decision.status.incomplete[0].reason,
        "Native usage history has not caught up"
    );
    assert!(decision.denial.is_some());
}
