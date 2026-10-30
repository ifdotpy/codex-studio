use super::claim_settlement::{request_row, request_with_admission};
use super::*;
use studio_runtime_schema::persistence::{REQUEST_RESULT_TTL_MS, RequestIdentityError};
use studio_runtime_store::ToolResultRecord;
use studio_runtime_store::request_results::{RequestResultError, RequestResults, ResultEnvelope};

fn assert_v7(value: &str) {
    assert_eq!(uuid::Uuid::parse_str(value).unwrap().get_version_num(), 7);
}

#[tokio::test]
async fn callback_studio_ids_are_v7_and_distinct_from_rpc_labels() {
    let (_root, store, context) = fixture().await;
    let ports = Arc::new(PortBundle::default());
    let rpc_label = "opaque:provider/rpc-label";
    let frame = call(
        &context,
        rpc_label,
        "orchestration_send",
        &json!({"agent_id":"worker","text":"continue"}),
    );
    let first = dispatch_with_managed_tools(store.clone(), &context, &frame, 20.0, ports.clone())
        .await
        .unwrap();
    let task = request_with_admission(&store, rpc_label);
    let task_id = task.fields["id"].as_str().unwrap();
    assert_v7(task_id);
    assert_eq!(task.fields["callId"], rpc_label);
    assert_eq!(task.fields["rpcId"], format!("rpc-{rpc_label}"));
    assert_ne!(task_id, rpc_label);
    assert!(
        task.fields.get("result").is_none(),
        "completed tool body belongs to request_results"
    );
    let token = &task.admitted;
    let saved = RequestResults::replay(
        &store.test_reader().unwrap(),
        token.id(),
        token.signature(),
        chrono::Utc::now().timestamp_millis(),
    )
    .unwrap()
    .unwrap();
    let reply: ResultEnvelope<ToolResultRecord> = serde_json::from_str(&saved.result).unwrap();
    assert!(reply.reply.success().unwrap());
    let callback_id: String = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT id FROM runtime_tool_requests WHERE kind='provider_callback'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_v7(&callback_id);
    assert_ne!(callback_id, task_id);
    let replay = dispatch_with_managed_tools(store.clone(), &context, &frame, 21.0, ports.clone())
        .await
        .unwrap();
    assert_eq!(replay, first);
    assert_eq!(request_row(&store, rpc_label)["id"], task_id);
    assert_eq!(ports.staged.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn seeded_token_settles_one_exact_typed_result_without_inline_copy() {
    let (_root, store, context) = fixture().await;
    let template = queued_tool_request(&context, "seeded-result");
    let original =
        json!({"success":true,"contentItems":[{"type":"inputText","text":"original response"}]});
    let saved_body = original.clone();
    let id = store
        .transaction_with::<_, StoreError>(move |tx| {
            let mut task = seed_tool_request(tx, template);
            let id = task["id"].as_str().unwrap().to_owned();
            task["stage"] = json!("completed");
            task["outcome"] = json!("applied");
            task["finished"] = json!(20.0);
            task["result"] = saved_body.clone();
            tx.execute(
                "UPDATE runtime_tool_requests SET record=?1 WHERE id=?2",
                params![task.to_string(), id],
            )?;
            crate::tool_requests::settle_result(tx, &id, &saved_body).unwrap();
            Ok(id)
        })
        .await
        .unwrap();
    let token = request_with_admission(&store, "seeded-result").admitted;
    let stored = RequestResults::replay(
        &store.test_reader().unwrap(),
        token.id(),
        token.signature(),
        chrono::Utc::now().timestamp_millis(),
    )
    .unwrap()
    .unwrap();
    let typed: ResultEnvelope<ToolResultRecord> = serde_json::from_str(&stored.result).unwrap();
    assert_eq!(typed.reply.into_value(), original);
    let actor = context.agent;
    let recovered = store
        .transaction_with(move |tx| {
            crate::tool_requests::execute(tx, &actor, crate::tool_requests::Action::Get(&id), 21.0)
        })
        .await
        .unwrap();
    assert_eq!(recovered["result"], original);
    assert!(request_row(&store, "seeded-result").get("result").is_none());
}

#[tokio::test]
async fn arbitrary_legacy_result_round_trips_through_settlement_and_replay() {
    let (_root, store, context) = fixture().await;
    for (call, original) in [
        ("legacy-scalar", json!(17)),
        (
            "legacy-malformed",
            json!({"success":"old","contentItems":[null],"extension":42}),
        ),
    ] {
        let template = queued_tool_request(&context, call);
        let expected = original.clone();
        let id = store
            .transaction_with::<_, StoreError>(move |tx| {
                let mut task = seed_tool_request(tx, template);
                let id = task["id"].as_str().unwrap().to_owned();
                task["stage"] = json!("completed");
                task["outcome"] = json!("applied");
                task["finished"] = json!(20.0);
                tx.execute(
                    "UPDATE runtime_tool_requests SET record=?1 WHERE id=?2",
                    params![task.to_string(), id],
                )?;
                crate::tool_requests::settle_result(tx, &id, &original).unwrap();
                Ok(id)
            })
            .await
            .unwrap();
        let token = request_with_admission(&store, call).admitted;
        let reader = store.test_reader().unwrap();
        let read_bytes = || {
            reader
                .query_row(
                    "SELECT result FROM request_results WHERE id=?1",
                    [token.id()],
                    |row| row.get::<_, String>(0),
                )
                .unwrap()
        };
        let before = read_bytes();
        assert_eq!(
            crate::tool_requests::saved_result(&reader, &id).unwrap(),
            Some(expected)
        );
        assert_eq!(read_bytes(), before);
    }
}

#[tokio::test]
async fn ambiguous_alias_neither_cancels_nor_dispatches_a_tool() {
    let (_root, store, context) = fixture().await;
    let template = queued_tool_request(&context, "shared-short-label");
    let actor = context.agent.clone();
    let ids = store
        .transaction_with::<_, StoreError>(move |tx| {
            let first = seed_tool_request(tx, template.clone());
            let second = seed_tool_request(tx, template);
            for action in [
                crate::tool_requests::Action::Get("shared-short-label"),
                crate::tool_requests::Action::Cancel("shared-short-label"),
            ] {
                let result = crate::tool_requests::execute(tx, &actor, action, 21.0).unwrap();
                assert_eq!(result["stage"], "ambiguous");
                assert_eq!(result["outcome"], "unknown");
            }
            Ok([
                first["id"].as_str().unwrap().to_owned(),
                second["id"].as_str().unwrap().to_owned(),
            ])
        })
        .await
        .unwrap();
    for id in &ids {
        let task = studio_runtime_store::system_reads::get(
            &store.test_reader().unwrap(),
            "runtime_tool_requests",
            id,
        )
        .unwrap()
        .unwrap();
        assert_eq!(task["stage"], "queued");
        assert_eq!(task["cancelRequested"], false);
    }
    let ports = Arc::new(PortBundle::default());
    let frame = call(
        &context,
        "ambiguous-alias-rpc",
        "orchestration_spawn",
        &json!({"request_id":"shared-short-label","agents":[]}),
    );
    assert!(matches!(
        dispatch_with_managed_tools(store.clone(), &context, &frame, 22.0, ports.clone()).await,
        Err(CallbackError::Unknown)
    ));
    assert_eq!(ports.staged.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn queued_cancel_is_not_applied_and_started_unknown_never_becomes_not_applied() {
    let (_root, store, context) = fixture().await;
    let queued = queued_tool_request(&context, "queued-cancel-proof");
    let running = queued_tool_request(&context, "running-unknown-proof");
    let actor = context.agent;
    store
        .transaction_with::<_, StoreError>(move |tx| {
            let queued = seed_tool_request(tx, queued);
            let mut running = seed_tool_request(tx, running);
            assert!(crate::tool_requests::admission::claim(&mut running, 20.0).unwrap());
            crate::tool_requests::admission::restart(&mut running).unwrap();
            tx.execute(
                "UPDATE runtime_tool_requests SET record=?1 WHERE id=?2",
                params![running.to_string(), running["id"].as_str().unwrap()],
            )?;
            let cancelled = crate::tool_requests::execute(
                tx,
                &actor,
                crate::tool_requests::Action::Cancel(queued["id"].as_str().unwrap()),
                21.0,
            )
            .unwrap();
            assert_eq!(cancelled["stage"], "cancelled");
            assert_eq!(cancelled["outcome"], "not_applied");
            let uncertain = crate::tool_requests::execute(
                tx,
                &actor,
                crate::tool_requests::Action::Cancel(running["id"].as_str().unwrap()),
                21.0,
            )
            .unwrap();
            assert_eq!(uncertain["outcome"], "unknown");
            assert!(
                crate::tool_requests::saved_result(tx, running["id"].as_str().unwrap())
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                tx.query_row("SELECT COUNT(*) FROM request_results", [], |row| row
                    .get::<_, i64>(0))?,
                1
            );
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(intent_count(&store, "spawn"), 0);
}

#[tokio::test]
async fn expired_tool_result_refuses_a_new_rpc_alias_without_dispatch() {
    let (_root, store, context) = fixture().await;
    let root = RequestIdentity::issue_root();
    let args = json!({"request_id":root,"agents":[{"name":"Worker","prompt":"Task"}]});
    let ports = Arc::new(PortBundle::default());
    let first = call(
        &context,
        "expiry-original-rpc",
        "orchestration_spawn",
        &args.clone(),
    );
    dispatch_with_managed_tools(store.clone(), &context, &first, 20.0, ports.clone())
        .await
        .unwrap();
    let task = request_with_admission(&store, "expiry-original-rpc");
    let id = task.fields["id"].as_str().unwrap().to_owned();
    let token = task.admitted;
    store
        .transaction_with::<_, StoreError>(move |tx| {
            tx.execute(
                "UPDATE request_results SET created=?1 WHERE id=?2",
                params![
                    chrono::Utc::now().timestamp_millis() - REQUEST_RESULT_TTL_MS - 1,
                    token.id()
                ],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(matches!(
        crate::tool_requests::saved_result(&store.test_reader().unwrap(), &id),
        Err(crate::tool_requests::ToolRequestError::RequestResult(
            RequestResultError::Identity(RequestIdentityError::Expired { .. })
        ))
    ));
    let retry = call(&context, "expiry-retry-rpc", "orchestration_spawn", &args);
    assert!(matches!(
        dispatch_with_managed_tools(store.clone(), &context, &retry, 21.0, ports.clone()).await,
        Err(CallbackError::Expired)
    ));
    assert_eq!(ports.staged.load(Ordering::SeqCst), 1);
    assert_eq!(intent_count(&store, "spawn"), 1);
    assert_eq!(
        store
            .test_reader()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM runtime_tool_requests WHERE kind='managed_tool'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}
