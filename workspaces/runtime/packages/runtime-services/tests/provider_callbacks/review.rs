use super::*;

async fn put_record(store: &RuntimeStore, table: &str, record: &Value) {
    try_put_record(store, table, record).await.unwrap();
}

async fn try_put_record(
    store: &RuntimeStore,
    table: &str,
    record: &Value,
) -> Result<(), studio_runtime_store::StoreError> {
    let table = table.to_owned();
    let id = record["id"].as_str().unwrap().to_owned();
    let encoded = record.to_string();
    if table == "runtime_requests" {
        // Cards are typed approvals; the fixture is a legacy record.
        let record = record.clone();
        return store
            .transaction_with::<_, studio_runtime_store::StoreError>(move |tx| {
                studio_runtime_services::provider_callbacks::legacy_view::insert(tx, &record)
            })
            .await;
    }
    store
        .transaction_with::<_, studio_runtime_store::StoreError>(move |tx| {
            tx.execute(
                &format!("INSERT INTO {table}(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record"),
                rusqlite::params![id, encoded],
            )?;
            Ok(())
        })
        .await
}
fn record(store: &RuntimeStore, table: &str, id: &str) -> Value {
    if table == "runtime_requests" {
        return studio_runtime_services::provider_callbacks::legacy_view::stored(
            &store.test_reader().unwrap(),
            id,
        )
        .unwrap();
    }
    let raw: String = store
        .test_reader()
        .unwrap()
        .query_row(
            &format!("SELECT record FROM {table} WHERE id=?1"),
            [id],
            |r| r.get(0),
        )
        .unwrap();
    let mut record = serde_json::from_str(&raw).unwrap();
    studio_runtime_store::agent_volatile::merge_if_agent(
        &store.test_reader().unwrap(),
        table,
        id,
        &mut record,
    )
    .unwrap();
    record
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

#[tokio::test]
async fn work_commands_match_python_arguments_bounds_and_provenance() {
    let (template_dir, template_store, context) = fixture().await;
    template_store.flush_wal_for_shutdown().await.unwrap();
    let databases = ["canvas.sqlite3", "analytics.sqlite3"]
        .map(|name| std::fs::read(template_dir.path().join(name)).unwrap());
    drop(template_store);
    for (index, case) in template().value["reviewCases"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let _dir = tempfile::tempdir().unwrap();
        for (name, bytes) in ["canvas.sqlite3", "analytics.sqlite3"]
            .into_iter()
            .zip(&databases)
        {
            std::fs::write(_dir.path().join(name), bytes).unwrap();
        }
        let store = std::sync::Arc::new(RuntimeStore::open(Some(_dir.path())).await.unwrap());
        put_record(&store, "runtime_work", &case["before"]).await;
        let frame = call(
            &context,
            &format!("review-work-{index}"),
            "orchestration_task",
            case["args"].clone(),
        );
        let result = reply(
            dispatch(std::sync::Arc::clone(&store), &context, &frame, 20.0)
                .await
                .unwrap(),
        );
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
        } else if case["args"]["action"] == "create" {
            let mut actual = tool_value(&result);
            let mut expected = case["expected"]["value"].clone();
            let actual_root = actual.as_object_mut().unwrap().remove("requestId").unwrap();
            let legacy_root = expected
                .as_object_mut()
                .unwrap()
                .remove("requestId")
                .unwrap();
            assert_eq!(legacy_root, format!("thread-0:review-work-{index}"));
            assert_eq!(
                uuid::Uuid::parse_str(actual_root.as_str().unwrap())
                    .unwrap()
                    .get_version_num(),
                7,
                "created work uses an issued UUIDv7 root"
            );
            let id = actual["id"].as_str().unwrap().to_owned();
            let mut stored = record(&store, "runtime_work", &id);
            let mut expected_stored = case["created"].clone();
            // Normalize only newly allocated work identity, preserving every
            // occurrence and every non-generated field in the full result.
            for value in [&mut actual, &mut expected] {
                assert!(!value["id"].as_str().unwrap().is_empty());
                assert_ne!(value["id"], case["before"]["id"]);
                assert_eq!(value["detail"]["task_id"], value["id"]);
                value["id"] = json!("<created-work>");
                value["detail"]["task_id"] = json!("<created-work>");
            }
            assert_eq!(actual, expected);
            for value in [&mut stored, &mut expected_stored] {
                value["id"] = json!("<created-work>");
                value.as_object_mut().unwrap().remove("created");
                value.as_object_mut().unwrap().remove("updated");
            }
            // Rust owns review gates. Python has no gate field, and this fixture
            // has no configured gate. Compare every other stored field.
            assert_eq!(
                stored.as_object_mut().unwrap().remove("gate"),
                Some(Value::Null)
            );
            assert!(expected_stored.get("gate").is_none());
            assert_eq!(stored, expected_stored);
        } else {
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
                let events:i64=store.test_reader().unwrap().query_row("SELECT count(*) FROM runtime_events WHERE kind='work_decision' AND agent=?1",[&context.agent],|r|r.get(0)).unwrap();
                assert_eq!(events, 0);
                assert!(case["events"].as_array().unwrap().is_empty());
            }
        }
    }
}

#[tokio::test]
async fn malformed_work_never_poison_store_writer() {
    for (field, value) in [
        ("results", Some(json!(["bad"]))),
        ("decisions", Some(json!([5]))),
        ("status", Some(Value::Null)),
        ("status", None),
        ("status", Some(json!("unknown"))),
        ("dependencies", Some(json!({"0":"prerequisite"}))),
        ("dependencies", Some(json!([7]))),
    ] {
        let (_dir, store, context) = fixture().await;
        let mut work = template().value["baseWork"].clone();
        if let Some(value) = value {
            work[field] = value;
        } else {
            work.as_object_mut()
                .expect("work fixture object")
                .remove(field);
        }
        let inserted = try_put_record(&store, "runtime_work", &work).await;
        if field == "status" && work.get("status").and_then(Value::as_str) == Some("unknown") {
            assert!(
                inserted.is_err(),
                "invalid status is refused by the stored schema"
            );
        } else {
            inserted.unwrap();
            let frame = call(
                &context,
                "bad-work",
                "orchestration_task",
                json!({"action":"history","task_id":work["id"]}),
            );
            let error = dispatch(std::sync::Arc::clone(&store), &context, &frame, 20.0)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("runtime_work"), "{error}");
            assert!(error.contains(work["id"].as_str().unwrap()), "{error}");
        }
        store
            .transaction_with::<_, studio_runtime_store::StoreError>(|tx| {
                tx.execute("DELETE FROM runtime_work", [])?;
                Ok(())
            })
            .await
            .unwrap();
        let good = call(
            &context,
            "good-after-bad",
            "orchestration_task",
            json!({"action":"create","title":"Writer still usable"}),
        );
        assert_eq!(
            body(&reply(
                dispatch(std::sync::Arc::clone(&store), &context, &good, 21.0)
                    .await
                    .unwrap()
            ))["success"],
            true
        );
    }
}

#[tokio::test]
async fn native_preview_matches_python_admission() {
    let (_dir, store, context) = fixture().await;
    let case = &template().value["preview"];
    let frame = request(
        Method::ItemCommandExecutionRequestApproval,
        RequestId::String("preview-rpc".into()),
        case["request"]["params"].clone(),
    );
    let CallbackDecision::Pending(id) =
        dispatch(std::sync::Arc::clone(&store), &context, &frame, 20.0)
            .await
            .unwrap()
    else {
        panic!("pending")
    };
    assert_eq!(
        record(&store, "runtime_requests", &id)["preview"],
        case["expected"]
    );
}

#[tokio::test]
async fn resolved_answering_and_unknown_cards_keep_what_nobody_inferred() {
    for case in template().value["resolved"].as_array().unwrap() {
        let mut card = case["card"].clone();
        let status = card["status"].as_str().unwrap().to_owned();
        // Nothing writes the blocked status any more.
        if status == "blocked" {
            continue;
        }
        if status != "pending" {
            card["decision"] = json!("accept");
        }
        let (_dir, store, context) = fixture().await;
        store
            .transaction_with::<_, studio_runtime_store::StoreError>(|tx| {
                tx.execute("DELETE FROM approvals", [])?;
                Ok(())
            })
            .await
            .unwrap();
        put_record(&store, "runtime_requests", &card).await;
        put_record(&store, "runtime_agents", &case["actor"]).await;
        // A pending card closes. An answer in delivery is unknown, and an
        // unknown answer stays: the provider's resolution never replaces it.
        let (changed, expected) = match status.as_str() {
            "pending" => (1, "resolved"),
            "answering" => (1, "uncertain"),
            _ => (0, "uncertain"),
        };
        assert_eq!(
            resolve(
                &store,
                &context,
                &RequestId::String("resolve-rpc".into()),
                20.0
            )
            .await
            .unwrap(),
            changed
        );
        assert_eq!(
            record(&store, "runtime_requests", "resolve-card")["status"],
            expected
        );
        assert_eq!(
            record(&store, "runtime_agents", &context.agent),
            case["expectedActor"]
        );
    }
    for submitted in [false, true] {
        let (_dir, store, context) = fixture().await;
        let frame = request(
            Method::ItemCommandExecutionRequestApproval,
            RequestId::String("cancel-after-answer".into()),
            json!({"threadId":context.thread}),
        );
        let CallbackDecision::Pending(id) =
            dispatch(std::sync::Arc::clone(&store), &context, &frame, 10.0)
                .await
                .unwrap()
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
        .await
        .unwrap();
        if submitted {
            delivery(&store, &prepared.identity, DeliveryOutcome::Submitted)
                .await
                .unwrap();
        }
        let mut completed = record(&store, "runtime_agents", &context.agent);
        completed["turnId"] = Value::Null;
        completed["inFlight"] = json!(false);
        completed["status"] = json!("completed");
        put_record(&store, "runtime_agents", &completed).await;
        resolve(&store, &context, &prepared.identity.rpc_id, 12.0)
            .await
            .unwrap();
        assert_eq!(record(&store, "runtime_agents", &context.agent), completed);
        assert!(
            !delivery(&store, &prepared.identity, DeliveryOutcome::Submitted)
                .await
                .unwrap(),
            "a resolved callback cannot submit again"
        );
        assert!(
            delivery(&store, &prepared.identity, DeliveryOutcome::Written)
                .await
                .is_err()
        );
        assert_eq!(
            record(&store, "runtime_requests", &id)["status"],
            "uncertain"
        );
    }
}

#[tokio::test]
async fn stale_answer_attempt_cannot_submit_or_settle_replacement() {
    let (_dir, store, context) = fixture().await;
    let frame = request(
        Method::ItemCommandExecutionRequestApproval,
        RequestId::Number(500),
        json!({"threadId":context.thread}),
    );
    let CallbackDecision::Pending(id) =
        dispatch(std::sync::Arc::clone(&store), &context, &frame, 10.0)
            .await
            .unwrap()
    else {
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
    .await
    .unwrap();
    delivery(&store, &old.identity, DeliveryOutcome::Submitted)
        .await
        .unwrap();
    delivery(&store, &old.identity, DeliveryOutcome::NotSent)
        .await
        .unwrap();
    let new = answer(
        &store,
        &context,
        &id,
        AnswerAuthority::User,
        &json!({"decision":"decline"}),
        12.0,
    )
    .await
    .unwrap();
    assert_ne!(old.identity, new.identity);
    assert!(
        delivery(&store, &old.identity, DeliveryOutcome::Submitted)
            .await
            .is_err()
    );
    delivery(&store, &new.identity, DeliveryOutcome::Submitted)
        .await
        .unwrap();
    for outcome in [
        DeliveryOutcome::Written,
        DeliveryOutcome::Unknown,
        DeliveryOutcome::NotSent,
    ] {
        assert!(delivery(&store, &old.identity, outcome).await.is_err());
    }
    delivery(&store, &new.identity, DeliveryOutcome::Written)
        .await
        .unwrap();
    assert_eq!(
        record(&store, "runtime_requests", &id)["decision"],
        "decline"
    );
}

#[tokio::test]
async fn model_projection_matches_python_and_keeps_full_result() {
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
    let (_dir, store, context) = fixture().await;
    let mut work = template().value["baseWork"].clone();
    work["description"] = json!("x".repeat(32000));
    put_record(&store, "runtime_work", &work).await;
    let frame = call(
        &context,
        "large-work",
        "orchestration_task",
        json!({"action":"get","task_id":work["id"]}),
    );
    let result = reply(
        dispatch(std::sync::Arc::clone(&store), &context, &frame, 20.0)
            .await
            .unwrap(),
    );
    assert!(body(&result).to_string().len() < 16000);
    let preview = tool_value(&result);
    assert_eq!(preview["truncated"], true);
    let full = studio_runtime_services::tool_requests::saved_result(
        &store.test_reader().unwrap(),
        preview["outputRef"].as_str().unwrap(),
    )
    .unwrap()
    .expect("the current request result preserves the full reply");
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

#[tokio::test]
async fn archived_private_member_blocks_saved_chat_like_python() {
    for case in template().value["chatVisibility"].as_array().unwrap() {
        let (_dir, store, context) = fixture().await;
        let mut worker = template().value["agents"][1].clone();
        worker["deletedAt"] = if case["archived"] == true {
            json!(15.0)
        } else {
            Value::Null
        };
        put_record(&store, "runtime_agents", &worker).await;
        put_record(
            &store,
            "runtime_rooms",
            &json!({"id":"private-review","kind":"private","members":[context.agent,worker["id"]],"updated":10.0}),
        ).await;
        store.transaction_with::<_,studio_runtime_store::StoreError>(move |tx| {
            tx.execute("INSERT INTO runtime_chat_messages(seq,id,room,sender,text,created,deliveries) VALUES(?2,'review-chat','private-review',?1,'Private text',10.0,'{}')",rusqlite::params![worker["id"].as_str().unwrap(),case["value"]["identity"]["seq"].as_i64().unwrap_or(1)])?; Ok(())
        }).await.unwrap();
        let frame = call(
            &context,
            "saved-chat",
            "orchestration_read",
            json!({"output_ref":"chat-message:review-chat"}),
        );
        let actual = reply(
            dispatch(std::sync::Arc::clone(&store), &context, &frame, 20.0)
                .await
                .unwrap(),
        );
        assert_eq!(body(&actual)["success"], case["success"]);
        if case["success"] == true {
            assert_eq!(tool_value(&actual), case["value"]);
        }
    }
}

#[tokio::test]
async fn resolve_retirement_rolls_back_with_card_and_agent() {
    for (table, operation) in [
        ("runtime_tool_requests", "UPDATE"),
        ("runtime_outbox", "DELETE"),
        ("approvals", "UPDATE"),
        ("runtime_agents", "UPDATE"),
    ] {
        let (dir, store, context) = fixture().await;
        store
            .transaction_with::<_, studio_runtime_store::StoreError>(|tx| {
                tx.execute("DELETE FROM approvals", [])?;
                Ok(())
            })
            .await
            .unwrap();
        let frame = request(
            Method::ItemCommandExecutionRequestApproval,
            RequestId::String("resolve-atomic".into()),
            json!({"threadId":context.thread}),
        );
        let CallbackDecision::Pending(id) =
            dispatch(std::sync::Arc::clone(&store), &context, &frame, 10.0)
                .await
                .unwrap()
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
        .await
        .unwrap();
        let mut actor = record(&store, "runtime_agents", &context.agent);
        actor["status"] = json!("approval");
        actor["inFlight"] = json!(true);
        put_record(&store, "runtime_agents", &actor).await;
        store.transaction_with::<_,studio_runtime_store::StoreError>(move |tx| {
            tx.execute_batch(&format!("CREATE TRIGGER resolve_failpoint AFTER {operation} ON {table} BEGIN SELECT RAISE(ABORT,'resolution failpoint'); END"))?; Ok(())
        }).await.unwrap();
        assert!(
            resolve(&store, &context, &prepared.identity.rpc_id, 12.0)
                .await
                .is_err(),
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
            .await
            .unwrap();
        drop(store);
        let store = RuntimeStore::open(Some(dir.path())).await.unwrap();
        // A rolled-back retirement leaves the exact durable attempt available.
        delivery(&store, &prepared.identity, DeliveryOutcome::Submitted)
            .await
            .unwrap();
        resolve(&store, &context, &prepared.identity.rpc_id, 13.0)
            .await
            .unwrap();
        assert!(
            delivery(&store, &prepared.identity, DeliveryOutcome::Written)
                .await
                .is_err()
        );
        assert_eq!(
            record(&store, "runtime_requests", &id)["status"],
            "uncertain"
        );
    }
}

#[tokio::test]
async fn task_tool_cancel_retains_its_historical_reason_bound() {
    let (_directory, store, context) = fixture().await;
    let before = &template().value["reviewCases"][0]["before"];
    for (token_index, token) in ["x", "🙂"].into_iter().enumerate() {
        for (size, expected) in [(32000, true), (32001, false)] {
            put_record(&store, "runtime_work", before).await;
            let frame = call(
                &context,
                &format!("reason-bound-{token_index}-{size}"),
                "orchestration_task",
                json!({"action":"cancel","task_id":before["id"],"reason":token.repeat(size)}),
            );
            let result = reply(
                dispatch(std::sync::Arc::clone(&store), &context, &frame, 20.0)
                    .await
                    .unwrap(),
            );
            assert_eq!(body(&result)["success"], expected, "{token_index}/{size}");
            if !expected {
                assert_eq!(
                    record(&store, "runtime_work", before["id"].as_str().unwrap()),
                    *before
                );
            }
        }
    }
}
