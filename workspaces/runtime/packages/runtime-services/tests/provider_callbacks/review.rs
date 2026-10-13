use super::*;

fn put_record(store: &RuntimeStore, table: &str, record: &Value) {
    store.transaction_with::<_, studio_runtime_store::StoreError>(|tx| {
        tx.execute(&format!("INSERT INTO {table}(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record"), rusqlite::params![record["id"].as_str().unwrap(), record.to_string()])?;
        Ok(())
    }).unwrap();
}
fn record(store: &RuntimeStore, table: &str, id: &str) -> Value {
    let raw: String = store
        .read()
        .unwrap()
        .query_row(
            &format!("SELECT record FROM {table} WHERE id=?1"),
            [id],
            |r| r.get(0),
        )
        .unwrap();
    serde_json::from_str(&raw).unwrap()
}
pub(super) fn canonical_response(value: &Value) -> Value {
    let mut result = value.clone();
    for item in result["contentItems"].as_array_mut().unwrap() {
        if let Some(value) = item["text"]
            .as_str()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
        {
            item["text"] = value;
        }
    }
    result
}

#[test]
fn work_commands_match_python_arguments_bounds_and_provenance() {
    for (index, case) in template().value["reviewCases"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let (_dir, store, context) = fixture();
        put_record(&store, "runtime_work", &case["before"]);
        let frame = call(
            &context,
            &format!("review-work-{index}"),
            "orchestration_task",
            case["args"].clone(),
        );
        let result = reply(dispatch(&store, &context, &frame, 20.0).unwrap());
        assert_eq!(
            body(&result)["success"],
            case["expected"]["success"],
            "{}: {result:?}",
            case["args"]
        );
        if case["expected"]["success"] == false {
            assert_eq!(
                record(
                    &store,
                    "runtime_work",
                    case["before"]["id"].as_str().unwrap()
                ),
                case["before"]
            );
        } else if case["args"]["action"] != "create" {
            let mut actual = record(
                &store,
                "runtime_work",
                case["before"]["id"].as_str().unwrap(),
            );
            let mut expected = case["after"].clone();
            // Clock is supplied by the shared writer, rather than the callback clock.
            actual.as_object_mut().unwrap().remove("updated");
            expected.as_object_mut().unwrap().remove("updated");
            for value in [&mut actual, &mut expected] {
                for decision in value["decisions"].as_array_mut().unwrap() {
                    decision.as_object_mut().unwrap().remove("created");
                }
            }
            assert_eq!(actual, expected, "{}", case["args"]);
            if case["args"]["action"] == "reject" {
                assert_eq!(actual["decisions"][0]["by"], context.agent);
                let events:i64=store.read().unwrap().query_row("SELECT count(*) FROM runtime_events WHERE kind='work_decision' AND agent=?1",[&context.agent],|r|r.get(0)).unwrap();
                assert_eq!(events, 0);
                assert!(case["events"].as_array().unwrap().is_empty());
            }
        }
    }
}

#[test]
fn malformed_work_never_poison_store_writer() {
    for (field, value) in [
        ("results", json!(["bad"])),
        ("decisions", json!([5])),
        ("status", Value::Null),
    ] {
        let (_dir, store, context) = fixture();
        let mut work = template().value["baseWork"].clone();
        work[field] = value;
        put_record(&store, "runtime_work", &work);
        let frame = call(
            &context,
            "bad-work",
            "orchestration_task",
            json!({"action":"history","task_id":work["id"]}),
        );
        let error = dispatch(&store, &context, &frame, 20.0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("runtime_work"), "{error}");
        assert!(error.contains(work["id"].as_str().unwrap()), "{error}");
        store
            .transaction_with::<_, studio_runtime_store::StoreError>(|tx| {
                tx.execute("DELETE FROM runtime_work", [])?;
                Ok(())
            })
            .unwrap();
        let good = call(
            &context,
            "good-after-bad",
            "orchestration_task",
            json!({"action":"create","title":"Writer still usable"}),
        );
        assert_eq!(
            body(&reply(dispatch(&store, &context, &good, 21.0).unwrap()))["success"],
            true
        );
    }
}

#[test]
fn native_preview_matches_python_admission() {
    let (_dir, store, context) = fixture();
    let case = &template().value["preview"];
    let frame = request(
        Method::ItemCommandExecutionRequestApproval,
        RequestId::String("preview-rpc".into()),
        case["request"]["params"].clone(),
    );
    let CallbackDecision::Pending(id) = dispatch(&store, &context, &frame, 20.0).unwrap() else {
        panic!("pending")
    };
    assert_eq!(
        record(&store, "runtime_requests", &id)["preview"],
        case["expected"]
    );
}

#[test]
fn resolved_answering_unknown_and_blocked_match_python() {
    for case in template().value["resolved"].as_array().unwrap() {
        let (_dir, store, context) = fixture();
        store
            .transaction_with::<_, studio_runtime_store::StoreError>(|tx| {
                tx.execute("DELETE FROM runtime_requests", [])?;
                Ok(())
            })
            .unwrap();
        put_record(&store, "runtime_requests", &case["card"]);
        let mut actor = template().value["agents"][0].clone();
        actor["status"] = json!("approval");
        actor["inFlight"] = json!(true);
        put_record(&store, "runtime_agents", &actor);
        assert_eq!(
            resolve(
                &store,
                &context,
                &RequestId::String("resolve-rpc".into()),
                20.0
            )
            .unwrap(),
            1
        );
        assert_eq!(
            record(&store, "runtime_requests", "resolve-card"),
            case["expected"]
        );
        assert_eq!(
            record(&store, "runtime_agents", &context.agent)["status"],
            case["agentStatus"]
        );
    }
    for submitted in [false, true] {
        let (_dir, store, context) = fixture();
        let frame = request(
            Method::ItemCommandExecutionRequestApproval,
            RequestId::String("cancel-after-answer".into()),
            json!({"threadId":context.thread}),
        );
        let CallbackDecision::Pending(id) = dispatch(&store, &context, &frame, 10.0).unwrap()
        else {
            panic!("pending")
        };
        let prepared = answer(
            &store,
            &context,
            &id,
            AnswerAuthority::User,
            &json!({"decision":"accept"}),
            11.0,
        )
        .unwrap();
        if submitted {
            delivery(&store, &prepared.identity, DeliveryOutcome::Submitted).unwrap();
        }
        resolve(&store, &context, &prepared.identity.rpc_id, 12.0).unwrap();
        assert!(delivery(&store, &prepared.identity, DeliveryOutcome::Submitted).is_err());
        assert!(delivery(&store, &prepared.identity, DeliveryOutcome::Written).is_err());
        assert_eq!(
            record(&store, "runtime_requests", &id)["status"],
            "resolved"
        );
    }
}

#[test]
fn stale_answer_attempt_cannot_submit_or_settle_replacement() {
    let (_dir, store, context) = fixture();
    let frame = request(
        Method::ItemCommandExecutionRequestApproval,
        RequestId::Number(500),
        json!({"threadId":context.thread}),
    );
    let CallbackDecision::Pending(id) = dispatch(&store, &context, &frame, 10.0).unwrap() else {
        panic!("pending")
    };
    let old = answer(
        &store,
        &context,
        &id,
        AnswerAuthority::User,
        &json!({"decision":"accept"}),
        11.0,
    )
    .unwrap();
    delivery(&store, &old.identity, DeliveryOutcome::Submitted).unwrap();
    delivery(&store, &old.identity, DeliveryOutcome::NotSent).unwrap();
    let new = answer(
        &store,
        &context,
        &id,
        AnswerAuthority::User,
        &json!({"decision":"decline"}),
        12.0,
    )
    .unwrap();
    assert_ne!(old.identity, new.identity);
    assert!(delivery(&store, &old.identity, DeliveryOutcome::Submitted).is_err());
    delivery(&store, &new.identity, DeliveryOutcome::Submitted).unwrap();
    for outcome in [
        DeliveryOutcome::Written,
        DeliveryOutcome::Unknown,
        DeliveryOutcome::NotSent,
    ] {
        assert!(delivery(&store, &old.identity, outcome).is_err());
    }
    delivery(&store, &new.identity, DeliveryOutcome::Written).unwrap();
    assert_eq!(
        record(&store, "runtime_requests", &id)["decision"],
        "decline"
    );
}

#[test]
fn model_projection_matches_python_and_keeps_full_result() {
    for case in template().value["projections"].as_array().unwrap() {
        let actual = project_tool_result(
            &case["result"],
            case["key"].as_str().unwrap(),
            &case["outcome"],
        )
        .unwrap();
        assert_eq!(
            canonical_response(&actual),
            canonical_response(&case["expected"])
        );
    }
    let (_dir, store, context) = fixture();
    let mut work = template().value["baseWork"].clone();
    work["description"] = json!("x".repeat(32000));
    put_record(&store, "runtime_work", &work);
    let frame = call(
        &context,
        "large-work",
        "orchestration_task",
        json!({"action":"get","task_id":work["id"]}),
    );
    let result = reply(dispatch(&store, &context, &frame, 20.0).unwrap());
    assert!(body(&result).to_string().len() < 16000);
    let preview = tool_value(&result);
    assert_eq!(preview["truncated"], true);
    let raw: String = store
        .read()
        .unwrap()
        .query_row(
            "SELECT result FROM runtime_tool_results WHERE id=?1",
            [preview["outputRef"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    let full: Value = serde_json::from_str(&raw).unwrap();
    let value: Value =
        serde_json::from_str(full["contentItems"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(value["description"], work["description"]);
    let huge =
        json!({"success":true,"contentItems":[{"type":"image","data":"z".repeat(1_100_000)}]});
    assert!(
        project_tool_result(&huge, "saved-huge", &json!("applied"))
            .unwrap()
            .to_string()
            .len()
            < 1000
    );
}

#[test]
fn archived_private_member_blocks_saved_chat_like_python() {
    for case in template().value["chatVisibility"].as_array().unwrap() {
        let (_dir, store, context) = fixture();
        let mut worker = template().value["agents"][1].clone();
        worker["deletedAt"] = if case["archived"] == true {
            json!(15.0)
        } else {
            Value::Null
        };
        put_record(&store, "runtime_agents", &worker);
        put_record(
            &store,
            "runtime_rooms",
            &json!({"id":"private-review","kind":"private","members":[context.agent,worker["id"]],"updated":10.0}),
        );
        store.transaction_with::<_,studio_runtime_store::StoreError>(|tx| {
            tx.execute("INSERT INTO runtime_chat_messages(seq,id,room,sender,text,created,deliveries) VALUES(?2,'review-chat','private-review',?1,'Private text',10.0,'{}')",rusqlite::params![worker["id"].as_str().unwrap(),case["value"]["identity"]["seq"].as_i64().unwrap_or(1)])?; Ok(())
        }).unwrap();
        let frame = call(
            &context,
            "saved-chat",
            "orchestration_read",
            json!({"output_ref":"chat-message:review-chat"}),
        );
        let actual = reply(dispatch(&store, &context, &frame, 20.0).unwrap());
        assert_eq!(body(&actual)["success"], case["success"]);
        if case["success"] == true {
            assert_eq!(tool_value(&actual), case["value"]);
        }
    }
}

#[test]
fn resolve_retirement_rolls_back_with_card_and_agent() {
    for table in [
        "runtime_operation_receipts",
        "runtime_requests",
        "runtime_agents",
    ] {
        let (dir, store, context) = fixture();
        store
            .transaction_with::<_, studio_runtime_store::StoreError>(|tx| {
                tx.execute("DELETE FROM runtime_requests", [])?;
                Ok(())
            })
            .unwrap();
        let frame = request(
            Method::ItemCommandExecutionRequestApproval,
            RequestId::String("resolve-atomic".into()),
            json!({"threadId":context.thread}),
        );
        let CallbackDecision::Pending(id) = dispatch(&store, &context, &frame, 10.0).unwrap()
        else {
            panic!("pending")
        };
        let prepared = answer(
            &store,
            &context,
            &id,
            AnswerAuthority::User,
            &json!({"decision":"accept"}),
            11.0,
        )
        .unwrap();
        let mut actor = record(&store, "runtime_agents", &context.agent);
        actor["status"] = json!("approval");
        actor["inFlight"] = json!(true);
        put_record(&store, "runtime_agents", &actor);
        store.transaction_with::<_,studio_runtime_store::StoreError>(|tx| {
            tx.execute_batch(&format!("CREATE TRIGGER resolve_failpoint AFTER UPDATE ON {table} BEGIN SELECT RAISE(ABORT,'resolution failpoint'); END"))?; Ok(())
        }).unwrap();
        assert!(
            resolve(&store, &context, &prepared.identity.rpc_id, 12.0).is_err(),
            "{table}"
        );
        assert_eq!(
            record(&store, "runtime_requests", &id)["status"],
            "answering"
        );
        assert_eq!(
            record(&store, "runtime_agents", &context.agent)["status"],
            "approval"
        );
        store
            .transaction_with::<_, studio_runtime_store::StoreError>(|tx| {
                tx.execute_batch("DROP TRIGGER resolve_failpoint")?;
                Ok(())
            })
            .unwrap();
        drop(store);
        let store = RuntimeStore::open(Some(dir.path())).unwrap();
        // A rolled-back retirement leaves the exact durable attempt available.
        delivery(&store, &prepared.identity, DeliveryOutcome::Submitted).unwrap();
        resolve(&store, &context, &prepared.identity.rpc_id, 13.0).unwrap();
        assert!(delivery(&store, &prepared.identity, DeliveryOutcome::Written).is_err());
        assert_eq!(
            record(&store, "runtime_requests", &id)["status"],
            "resolved"
        );
    }
}
