//! Budget rows through the transaction adapter, without providers.
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use studio_runtime_services::{
    budget::{CaptureSource, HistoryFileObservation, HistoryObservation},
    budget_service,
};
fn db(records: &Value, case: &Value) -> Connection {
    let connection = Connection::open_in_memory().expect("SQLite");
    connection.execute_batch(&studio_runtime_store::agent_columns::fixture_sql("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);CREATE TABLE runtime_budget(id TEXT PRIMARY KEY,record TEXT NOT NULL);CREATE TABLE runtime_budget_usage(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,tokens INTEGER,at REAL);CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,created REAL,epoch INTEGER,turn_id TEXT,error TEXT);CREATE TABLE runtime_native_action_receipts(id TEXT PRIMARY KEY,signature TEXT,receipt TEXT,outcome TEXT);")).expect("schema");
    connection
        .execute_batch(studio_runtime_store::agent_volatile::agent_volatile_table_sql())
        .expect("volatile agent table creates");
    for (name, table) in [("agents", "runtime_agents"), ("states", "runtime_budget")] {
        for (id, row) in records[name].as_object().expect("records") {
            connection
                .execute(
                    &format!("INSERT INTO {table} VALUES (?1,?2)"),
                    params![id, row.to_string()],
                )
                .expect("agent or state row");
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
                    row["turnId"].as_str().or(row["turn_id"].as_str()),
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
            let mut record = serde_json::from_str(&raw).expect("record");
            studio_runtime_store::agent_volatile::merge_if_agent(
                connection,
                table,
                &id,
                &mut record,
            )
            .expect("volatile agent fields");
            result.insert(id, record);
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
    let mut outputs: Vec<Value> = Vec::new();
    let changed_replay_step = changed_response_replay_step(case);
    for (step_index, step) in case["steps"].as_array().expect("steps").iter().enumerate() {
        let tx = connection.transaction().expect("transaction");
        let id = step["agent"].as_str().unwrap_or("a");
        let now = step["now"].as_f64().unwrap_or(100.);
        let result = match step["op"].as_str().expect("op") {
            "capture" => {
                let mut agent = step.get("historical_agent").cloned().unwrap_or_else(|| {
                    studio_runtime_store::budget::BudgetRepository::agent(&tx, id).expect("agent")
                });
                match budget_service::capture(
                    &tx,
                    &mut agent,
                    &step["params"],
                    step["at"].as_f64().unwrap_or(now),
                    if step["source"] == "history" {
                        CaptureSource::History
                    } else {
                        CaptureSource::Live
                    },
                    now,
                ) {
                    Ok(tokens) => json!(tokens),
                    Err(studio_runtime_services::budget::BudgetError::Invalid(location))
                        if Some(step_index) == changed_replay_step
                            && location == "runtime_budget_usage/responseReplayConflict" =>
                    {
                        json!({"rustRefusal":"responseReplayConflict"})
                    }
                    Err(error) => panic!("{}: {error}", case["name"]),
                }
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
                // The template has an analytics table; a missing row is
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
        let output = json!({"result":result,"stored":stored(&connection)});
        if Some(step_index) == changed_replay_step {
            assert_eq!(
                output["result"],
                json!({"rustRefusal":"responseReplayConflict"}),
                "{}: Rust refuses changed-content replay",
                case["name"]
            );
            assert_eq!(
                output["stored"],
                outputs.last().expect("prior Rust output")["stored"],
                "Rust conflict refusal writes nothing"
            );
        }
        outputs.push(output);
    }
    json!(outputs)
}
fn every_budget_case_runs_to_completion() {
    let cases: Value = serde_json::from_str(include_str!("budget/cases.json")).expect("cases");
    for case in cases.as_array().expect("cases") {
        let outputs = run(case, &records(case));
        assert_eq!(
            outputs.as_array().expect("outputs").len(),
            case["steps"].as_array().expect("steps").len(),
            "{}",
            case["name"]
        );
    }
}

/// Initial rows of a case, including its seed captures, written by the Rust service.
fn records(case: &Value) -> Value {
    let agents: serde_json::Map<String, Value> = case["agents"]
        .as_array()
        .expect("agents")
        .iter()
        .map(|agent| {
            (
                agent["id"].as_str().expect("agent id").to_owned(),
                agent.clone(),
            )
        })
        .collect();
    let initial = json!({"agents":agents,"states":{},"usage":[],"events":case["events"].as_array().cloned().unwrap_or_default()});
    let mut connection = db(&initial, case);
    for seed in case["seed_captures"].as_array().into_iter().flatten() {
        let tx = connection.transaction().expect("seed transaction");
        let mut agent = studio_runtime_store::budget::BudgetRepository::agent(
            &tx,
            seed["agent"].as_str().unwrap_or("a"),
        )
        .expect("seed agent");
        // A refused seed (a conflicting replay) leaves no row; the case steps
        // then run against the rows the accepted seeds wrote.
        let captured = budget_service::capture(
            &tx,
            &mut agent,
            &seed["params"],
            seed["at"].as_f64().unwrap_or(101.),
            if seed["source"] == "history" {
                CaptureSource::History
            } else {
                CaptureSource::Live
            },
            100.,
        );
        if captured.is_ok() {
            tx.commit().expect("seed commit");
        }
    }
    stored(&connection)
}

fn changed_response_replay_step(case: &Value) -> Option<usize> {
    let mut responses = BTreeMap::new();
    case["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter(|(_, step)| step["op"] == "capture")
        .find_map(|(index, step)| {
            let response_id = step["params"]["responseId"].as_str()?;
            let tokens = step["params"]["requestUsage"]["totalTokens"].as_u64()?;
            responses
                .insert(response_id.to_owned(), tokens)
                .and_then(|previous| (previous != tokens).then_some(index))
        })
}

#[test]
fn exact_response_replay_is_noop_and_conflicting_replay_refuses_without_writes() {
    let (case, records) = fixture();
    let mut connection = db(&records, &case);
    sync_fixture(&connection);
    let params = &case["steps"][0]["params"];
    let tx = connection.transaction().expect("initial transaction");
    let mut agent = records["agents"]["a"].clone();
    assert_eq!(
        budget_service::capture(&tx, &mut agent, params, 101., CaptureSource::Live, 100.)
            .expect("initial response capture"),
        20
    );
    tx.commit().expect("initial commit");

    let before_replay = all_stored(&connection);
    let tx = connection.transaction().expect("replay transaction");
    let mut agent =
        studio_runtime_store::budget::BudgetRepository::agent(&tx, "a").expect("current agent");
    assert_eq!(
        budget_service::capture(&tx, &mut agent, params, 101., CaptureSource::Live, 100.)
            .expect("identical replay"),
        20
    );
    tx.commit().expect("replay commit");
    assert_eq!(
        all_stored(&connection),
        before_replay,
        "identical replay writes nothing"
    );

    let before_conflict = all_stored(&connection);
    let tx = connection.transaction().expect("conflict transaction");
    let mut agent =
        studio_runtime_store::budget::BudgetRepository::agent(&tx, "a").expect("current agent");
    let mut changed = params.clone();
    changed["requestUsage"]["totalTokens"] = json!(21);
    assert!(
        budget_service::capture(&tx, &mut agent, &changed, 101., CaptureSource::Live, 100.)
            .is_err()
    );
    tx.commit().expect("commit conflict refusal");
    assert_eq!(
        all_stored(&connection),
        before_conflict,
        "conflicting replay writes nothing"
    );
}

fn fixture() -> (Value, Value) {
    let cases: Value = serde_json::from_str(include_str!("budget/cases.json")).expect("cases");
    let case = cases[0].clone();
    let records = records(&case);
    (case, records)
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
fn capture_rejects_stale_durable_identity() {
    let (case, records) = fixture();
    for source in [CaptureSource::Live, CaptureSource::History] {
        for initialized in [false, true] {
            let mut fields = vec![
                ("rootId", json!("other-root")),
                ("accountKey", json!("other-account")),
                ("threadId", json!("other-thread")),
                ("epoch", json!(1)),
            ];
            if source == CaptureSource::Live {
                fields.push(("turnId", json!("other-turn")));
            }
            for (field, value) in fields {
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

fn history_capture_uses_event_turn_and_keeps_durable_turn() {
    let (case, mut records) = fixture();
    records["agents"]["a"]["turnId"] = json!("durable-turn");
    let mut connection = db(&records, &case);
    let mut supplied = records["agents"]["a"].clone();
    supplied["turnId"] = json!("newer-rollout-context-turn");
    let first_notice = json!({
        "turnId":"historical-event-turn-a",
        "tokenUsage":{"total":{"totalTokens":20},"last":{"totalTokens":20}}
    });

    for (turn_id, expected_receipts) in [
        ("historical-event-turn-a", 1),
        ("historical-event-turn-b", 2),
    ] {
        let params = if turn_id == "historical-event-turn-a" {
            first_notice.clone()
        } else {
            json!({
                "turnId":turn_id,
                "tokenUsage":{"total":{"totalTokens":20},"last":{"totalTokens":20}}
            })
        };
        let tx = connection.transaction().expect("transaction");
        budget_service::capture(
            &tx,
            &mut supplied,
            &params,
            101.,
            CaptureSource::History,
            100.,
        )
        .expect("validated historical event turn is accepted");
        tx.commit().expect("commit historical receipt");
        let state = stored(&connection);
        assert_eq!(
            state["usage"].as_array().expect("usage").len(),
            expected_receipts
        );
        assert!(
            state["usage"]
                .as_array()
                .expect("usage")
                .iter()
                .all(|row| row["kind"] == "notice")
        );
        assert_eq!(state["agents"]["a"]["turnId"], "durable-turn");
        assert_eq!(supplied["turnId"], "newer-rollout-context-turn");
    }
    let before_notice_replay = stored(&connection);
    let tx = connection
        .transaction()
        .expect("historical notice replay transaction");
    budget_service::capture(
        &tx,
        &mut supplied,
        &first_notice,
        101.,
        CaptureSource::History,
        100.,
    )
    .expect("historical notice replay is accepted");
    tx.commit().expect("commit notice replay");
    assert_eq!(stored(&connection), before_notice_replay);

    let exact_params = json!({
        "turnId":"historical-response-turn",
        "responseId":"response-1",
        "rawTokenUsageRecord":{"response_id":"response-1"},
        "tokenUsage":{"total":{"totalTokens":20},"last":{"totalTokens":20}}
    });
    let tx = connection.transaction().expect("response transaction");
    budget_service::capture(
        &tx,
        &mut supplied,
        &exact_params,
        101.,
        CaptureSource::History,
        100.,
    )
    .expect("historical response is accepted");
    tx.commit().expect("commit response receipt");
    let spent_after_first_response = stored(&connection)["agents"]["a"]["tokensUsed"].clone();
    let tx = connection
        .transaction()
        .expect("response replay transaction");
    budget_service::capture(
        &tx,
        &mut supplied,
        &exact_params,
        101.,
        CaptureSource::History,
        100.,
    )
    .expect("historical response replay is idempotent");
    tx.commit().expect("commit response replay");
    let final_state = stored(&connection);
    assert_eq!(final_state["usage"].as_array().expect("usage").len(), 3);
    assert_eq!(
        final_state["agents"]["a"]["tokensUsed"],
        spent_after_first_response
    );
    assert_eq!(final_state["agents"]["a"]["turnId"], "durable-turn");
    assert_eq!(supplied["turnId"], "newer-rollout-context-turn");
    let usages = final_state["usage"].as_array().expect("usage").clone();
    assert_ne!(usages[0]["id"], usages[1]["id"]);
}

fn capture_partial_copy_uses_durable_identity() {
    let (mut case, _) = fixture();
    case["agents"][0]["turnId"] = json!("durable-turn");
    case["agents"][0]["created"] = json!(50);
    case["steps"] = json!([{"op":"capture", "at":101, "params":{
        "tokenUsage":{"total":{"totalTokens":20},"last":{"totalTokens":20}}
    }}]);
    for source in [CaptureSource::Live, CaptureSource::History] {
        case["steps"][0]["source"] = json!(source);
        let initial = records(&case);
        let expected = run(&case, &initial);
        let mut connection = db(&initial, &case);
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
            assert_eq!(
                stored(&connection),
                expected[0]["stored"],
                "durable identity"
            );
        }
        assert_eq!(supplied["created"], 999);
        assert_eq!(supplied["notificationField"], "preserved");
        assert!(supplied.get("accountKey").is_none());
        assert_eq!(supplied["tokensUsed"], 20);
    }
}

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

fn wait_failpoints_preserve_batch_and_exact_resume() {
    let cases: Value = serde_json::from_str(include_str!("budget/cases.json")).expect("cases");
    let case = cases
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["name"] == "wait-resume")
        .expect("wait case");
    let expected = run(case, &records(case));
    let initial = &expected[0]["stored"];
    let denial =
        serde_json::from_value(expected[0]["result"]["denial"].clone()).expect("admission denial");
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

fn malformed_stored_ledgers_and_late_team_read_roll_back() {
    let (case, records) = fixture();
    let written = run(&case, &records);
    let state = &written[0]["stored"]["states"]["a"];
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

fn action_retirement_failpoints_keep_receipt_and_wait_atomic() {
    let cases: Value = serde_json::from_str(include_str!("budget/cases.json")).expect("cases");
    let case = cases
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["name"] == "action-stale")
        .expect("action case");
    let expected = run(case, &records(case));
    let initial = &expected[2]["stored"];
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
            assert_eq!(
                stored(&connection),
                expected[3]["stored"],
                "action retirement"
            );
        }
    }
}

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
        modified_seconds: 0,
        modified_nanos: 0,
        content_hash: String::new(),
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

#[test]
fn budget_corpus_suite() {
    let cases: &[(&str, fn())] = &[
        (
            "every_budget_case_runs_to_completion",
            every_budget_case_runs_to_completion,
        ),
        (
            "capture_failpoints_restart_and_lost_response",
            capture_failpoints_restart_and_lost_response,
        ),
        (
            "capture_rejects_stale_durable_identity",
            capture_rejects_stale_durable_identity,
        ),
        (
            "history_capture_uses_event_turn_and_keeps_durable_turn",
            history_capture_uses_event_turn_and_keeps_durable_turn,
        ),
        (
            "capture_partial_copy_uses_durable_identity",
            capture_partial_copy_uses_durable_identity,
        ),
        (
            "malformed_budget_inputs_refuse_without_writes",
            malformed_budget_inputs_refuse_without_writes,
        ),
        (
            "wait_failpoints_preserve_batch_and_exact_resume",
            wait_failpoints_preserve_batch_and_exact_resume,
        ),
        (
            "malformed_stored_ledgers_and_late_team_read_roll_back",
            malformed_stored_ledgers_and_late_team_read_roll_back,
        ),
        (
            "usage_state_check_refuses_bad_scalar_rows_without_writing",
            usage_state_check_refuses_bad_scalar_rows_without_writing,
        ),
        (
            "action_retirement_failpoints_keep_receipt_and_wait_atomic",
            action_retirement_failpoints_keep_receipt_and_wait_atomic,
        ),
        (
            "incomplete_collected_tail_cannot_authorize_admission",
            incomplete_collected_tail_cannot_authorize_admission,
        ),
    ];
    let mut failures = Vec::new();
    for (name, case) in cases {
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(case)) {
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("non-string panic");
            failures.push(format!("{name}: {message}"));
        }
    }
    assert!(
        failures.is_empty(),
        "budget corpus failures:\n{}",
        failures.join("\n")
    );
}
