//! Durable human callback round trips against Python-written records.
#![allow(clippy::unwrap_used)]
use crate::callback_fixtures as fixture;
use fixture::*;
use serde_json::json;
use std::sync::Arc;
use studio_runtime_schema::persistence::{OutboxStatus, ToolRequestStatus};
use studio_runtime_services::{callback_runtime::*, provider_callbacks::*};
use studio_runtime_store::{
    RuntimeStore,
    outbox::Outbox,
    request_results::{RequestResults, ResultEnvelope},
};

pub(crate) fn callback_owner(
    store: &RuntimeStore,
    context: &CallbackContext,
    rpc: &studio_runtime_providers::RequestId,
) -> studio_runtime_services::tool_requests::CallbackTask<CallbackOwner> {
    studio_runtime_services::tool_requests::get_callback(
        &store.test_reader().unwrap(),
        &callback_origin_key(context, rpc),
    )
    .unwrap()
    .unwrap()
}

pub(crate) fn callback_body(
    store: &RuntimeStore,
    owner: &studio_runtime_services::tool_requests::CallbackTask<CallbackOwner>,
) -> Option<CallbackResponse> {
    let admission = owner.payload.admission.as_ref().unwrap();
    RequestResults::replay(
        &store.test_reader().unwrap(),
        admission.id(),
        admission.signature(),
        chrono::Utc::now().timestamp_millis(),
    )
    .unwrap()
    .map(|row| {
        serde_json::from_str::<ResultEnvelope<CallbackResponse>>(&row.result)
            .unwrap()
            .reply
    })
}

#[tokio::test]
async fn complete_python_answer_and_delete_corpus() {
    let (_root, store, context) = fixture().await;
    let port = Arc::new(Port::default());
    let runtime = CallbackRuntime::new(store.clone(), port.clone());
    for case in template()["fullAnswers"].as_array().unwrap() {
        seed_case(&store, case).await;
        let before_rows = logical_snapshot(&store);
        let id = case["before"]["id"].as_str().unwrap();
        assert_eq!(
            runtime.answer(id, &case["data"], 20.0).await.unwrap(),
            case["response"]
        );
        assert_eq!(
            runtime.answer(id, &case["data"], 21.0).await.unwrap(),
            case["replay"]
        );
        assert_outcome(&stored(&store, id), &case["after"]);
        assert_eq!(
            actor(&store, &context),
            normalized_actor(&case["afterActor"])
        );
        assert_writes(&before_rows, &store, &case["writes"]);
        assert_eq!(
            frame_value(&port.calls.lock().unwrap().last().unwrap().frame()),
            case["frame"]
        );
    }
    assert_eq!(port.calls.lock().unwrap().len(), 30);
    for case in template()["deletes"].as_array().unwrap() {
        seed_case(&store, case).await;
        let before_rows = logical_snapshot(&store);
        let id = case["before"]["id"].as_str().unwrap();
        let before = port.calls.lock().unwrap().len();
        let result = runtime.delete_question(id, 20.0).await;
        if let Some(error) = case["expected"].get("error") {
            assert_eq!(result.unwrap_err().to_string(), error.as_str().unwrap());
        } else {
            assert_eq!(result.unwrap(), case["expected"]["result"]);
        }
        assert_outcome(&stored(&store, id), &case["after"]);
        assert_eq!(
            actor(&store, &context),
            normalized_actor(&case["afterActor"])
        );
        assert_writes(&before_rows, &store, &case["writes"]);
        let frames: Vec<_> = port.calls.lock().unwrap()[before..]
            .iter()
            .map(|r| frame_value(&r.frame()))
            .collect();
        assert_eq!(json!(frames), case["frames"]);
    }
}

#[tokio::test]
async fn written_retires_transport_body_and_preserves_exact_http_replay() {
    let (_root, store, context) = fixture().await;
    let (id, frame) = pending(&store, &context).await;
    let data = json!({"answers":{"secret":{"answers":["secret-payload"]}}});
    let port = Arc::new(Port::default());
    let runtime = CallbackRuntime::new(store.clone(), port.clone());
    runtime.answer(&id, &data, 20.0).await.unwrap();
    let owner = callback_owner(
        &store,
        &context,
        &studio_runtime_providers::RequestId::Number(42),
    );
    assert_eq!(owner.status, ToolRequestStatus::Completed);
    assert!(owner.payload.frame_fingerprint.is_some());
    assert!(
        !serde_json::to_string(&owner)
            .unwrap()
            .contains("secret-payload")
    );
    assert!(
        Outbox::get::<CallbackResponse>(&store.test_reader().unwrap(), &owner.origin_key)
            .unwrap()
            .is_none()
    );
    let saved = callback_body(&store, &owner).unwrap();
    assert_eq!(saved.body, CallbackBody::Result(data.clone()));
    let reply = port.calls.lock().unwrap()[0].clone();
    store
        .transaction_with::<_, CallbackError>(move |tx| {
            assert_eq!(
                begin_frame(
                    tx,
                    &reply.identity.context.stream_id,
                    reply.identity.context.generation,
                    &reply.identity.operation_id(),
                    &reply.frame()
                )?,
                FrameAdmission::Written
            );
            Ok(())
        })
        .await
        .unwrap();
    assert!(matches!(
        dispatch(Arc::clone(&store), &context, &frame, 21.0)
            .await
            .unwrap(),
        CallbackDecision::Settled(_)
    ));
    assert_eq!(
        runtime.answer(&id, &data, 21.0).await.unwrap()["replayed"],
        true
    );
    // Documented limit: the stored answer to a secret question is the redaction mark,
    // so a different secret value cannot be told from the first. It replays.
    let other = runtime
        .answer(
            &id,
            &json!({"answers":{"secret":{"answers":["other-payload"]}}}),
            21.0,
        )
        .await
        .unwrap();
    assert_eq!(other["replayed"], true);
    assert_eq!(port.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn not_sent_creates_new_attempt_and_foreign_receipt_is_unknown() {
    let (_root, store, context) = fixture().await;
    let (id, _) = pending(&store, &context).await;
    let port = Arc::new(Port::default());
    *port.next.lock().unwrap() = Some(ReplyOutcome::NotSent);
    let runtime = CallbackRuntime::new(store.clone(), port.clone());
    let data = json!({"answers":{}});
    assert!(runtime.answer(&id, &data, 20.0).await.is_err());
    assert_eq!(stored(&store, &id)["status"], "pending");
    let old = port.calls.lock().unwrap()[0].identity.clone();
    *port.foreign.lock().unwrap() = true;
    assert!(matches!(
        runtime.answer(&id, &data, 21.0).await,
        Err(CallbackError::Unknown)
    ));
    let new = port.calls.lock().unwrap()[1].identity.clone();
    assert_ne!(old.attempt, new.attempt);
    assert!(matches!(
        delivery(&store, &old, DeliveryOutcome::Written).await,
        Err(CallbackError::Stale)
    ));
    assert_eq!(stored(&store, &id)["status"], "uncertain");
    let owner = callback_owner(
        &store,
        &context,
        &studio_runtime_providers::RequestId::Number(42),
    );
    assert_eq!(owner.status, ToolRequestStatus::Unknown);
    let row = Outbox::get::<CallbackResponse>(&store.test_reader().unwrap(), &owner.origin_key)
        .unwrap()
        .unwrap();
    assert_eq!(row.status, OutboxStatus::Unknown);
    assert_eq!(row.next_at, None);
    assert!(prepared_replies(&store, &context).await.unwrap().is_empty());
}

#[tokio::test]
async fn python_resolution_rows_match_after_completed_or_cleared_turn() {
    let (_root, store, context) = fixture().await;
    for case in template()["resolved"].as_array().unwrap() {
        seed_case(&store, case).await;
        let before = logical_snapshot(&store);
        resolve(
            &store,
            &context,
            &studio_runtime_providers::RequestId::String("resolve-rpc".into()),
            20.0,
        )
        .await
        .unwrap();
        // The frozen Python record resolves every state. A typed card resolves only a
        // pending one; an answer in delivery is unknown, and a record without an
        // answer cannot be answering.
        let after = stored(&store, "resolve-card");
        if case["before"]["status"] == "pending" {
            assert_outcome(&after, &case["expected"]);
        } else {
            assert!(
                matches!(
                    after["status"].as_str(),
                    Some("resolved" | "expired" | "uncertain")
                ),
                "{after}"
            );
        }
        assert_eq!(
            actor(&store, &context),
            normalized_actor(&case["expectedActor"])
        );
        assert_writes(&before, &store, &case["writes"]);
    }
}

fn frame_value(frame: &studio_runtime_providers::Frame) -> serde_json::Value {
    serde_json::from_slice(&studio_runtime_providers::encode_line(frame).unwrap()).unwrap()
}

#[tokio::test]
async fn mailbox_timeout_is_unknown_and_closed_queue_proves_not_sent() {
    let (_root, store, context) = fixture().await;
    let (id, _) = pending(&store, &context).await;
    let reply = prepare_user_answer(&store, &id, &json!({"answers":{}}), 20.0, false)
        .await
        .unwrap()
        .unwrap();
    let (mailbox, receiver) = CallbackMailbox::new(Arc::new(|| {}));
    assert!(matches!(
        mailbox.deliver(&reply, std::time::Duration::ZERO).await,
        Err(CallbackError::Unknown)
    ));
    assert_eq!(receiver.try_recv().unwrap().reply, reply);
    drop(receiver);
    assert_eq!(
        mailbox
            .deliver(&reply, REPLY_DEADLINE)
            .await
            .unwrap()
            .outcome,
        ReplyOutcome::NotSent
    );
}

#[tokio::test]
async fn committed_reply_rechecks_card_eligibility_before_provider_handoff() {
    let (_root, store, context) = fixture().await;
    let (id, _) = pending(&store, &context).await;
    let reply = prepare_user_answer(&store, &id, &json!({"answers":{}}), 20.0, false)
        .await
        .unwrap()
        .unwrap();
    let id_for_update = id.clone();
    store
        .transaction_with::<_, studio_runtime_store::StoreError>(move |tx| {
            tx.execute(
                "UPDATE approvals SET state='answered',command=NULL,answered_by='{\"kind\":\"owner\"}',at=20 WHERE id=?1",
                [&id_for_update],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(stored(&store, &id)["status"], "answered");

    let port = Arc::new(Port::default());
    let runtime = CallbackRuntime::new(store.clone(), port.clone());
    assert!(!runtime.deliver(&reply).await.unwrap());
    assert!(port.calls.lock().unwrap().is_empty());
    assert_eq!(stored(&store, &id)["status"], "answered");
}

#[tokio::test]
async fn tool_replay_rebuilds_the_same_reply_without_reexecuting_effect() {
    use studio_runtime_providers::{Frame, Method, RequestFrame, RequestId};
    let (_root, store, context) = fixture().await;
    let frame = Frame::Request(RequestFrame {
        id: RequestId::Number(25),
        method: Method::ItemToolCall,
        params: Some(
            json!({"threadId":context.thread,"turnId":context.turn,"callId":"rotate","tool":"orchestration_title","arguments":{"title":"Once"}}),
        ),
        fields: Default::default(),
    });
    let CallbackDecision::Answer(first) = dispatch(Arc::clone(&store), &context, &frame, 10.0)
        .await
        .unwrap()
    else {
        panic!("answer")
    };
    let CallbackDecision::Answer(second) = dispatch(Arc::clone(&store), &context, &frame, 11.0)
        .await
        .unwrap()
    else {
        panic!("answer")
    };
    assert_eq!(
        first, second,
        "a replay returns the identical committed reply"
    );
    let owner = callback_owner(&store, &context, &RequestId::Number(25));
    let row = Outbox::get::<CallbackResponse>(&store.test_reader().unwrap(), &owner.origin_key)
        .unwrap()
        .unwrap();
    assert_eq!(row.status, OutboxStatus::Pending);
    assert_eq!(row.payload.body, CallbackBody::from(&first.body));
    let tool = owner.payload.tool_request.as_ref().unwrap();
    assert!(
        studio_runtime_services::tool_requests::saved_result(&store.test_reader().unwrap(), tool)
            .unwrap()
            .is_some()
    );
    let count: i64 = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM runtime_tool_requests WHERE id=?1",
            [tool],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    let runtime = CallbackRuntime::new(store.clone(), Arc::new(Port::default()));
    assert!(runtime.deliver(&first).await.unwrap());
    let owner = callback_owner(&store, &context, &RequestId::Number(25));
    assert_eq!(owner.status, ToolRequestStatus::Completed);
    assert!(
        Outbox::get::<CallbackResponse>(&store.test_reader().unwrap(), &owner.origin_key)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        callback_body(&store, &owner).unwrap().identity,
        first.identity
    );
}

#[tokio::test]
async fn failed_store_calls_roll_back_card_outbox_and_publication_before_handoff() {
    for (table, event, phase) in [
        ("approvals", "INSERT", "admit"),
        ("runtime_agents", "UPDATE", "admit"),
        ("runtime_tool_requests", "INSERT", "admit"),
        ("sync_entities", "INSERT", "admit"),
        ("approvals", "UPDATE", "answer"),
        ("runtime_outbox", "INSERT", "answer"),
        ("runtime_outbox", "UPDATE", "answer"),
    ] {
        let (_root, store, context) = fixture().await;
        let id = if phase == "answer" {
            Some(pending(&store, &context).await.0)
        } else {
            None
        };
        let trigger_table = table.to_owned();
        let trigger_event = event.to_owned();
        store.transaction_with::<_,studio_runtime_store::StoreError>(move |tx| {
            tx.execute_batch(&format!("CREATE TRIGGER callback_failpoint BEFORE {trigger_event} ON {trigger_table} BEGIN SELECT RAISE(ABORT,'callback failpoint'); END"))?; Ok(())
        }).await.unwrap();
        let before = snapshot(&store);
        let port = Arc::new(Port::default());
        let runtime = CallbackRuntime::new(store.clone(), port.clone());
        let result = if let Some(id) = id {
            runtime
                .answer(&id, &json!({"answers":{}}), 20.0)
                .await
                .map(|_| ())
        } else {
            use studio_runtime_providers::{Frame, Method, RequestFrame, RequestId};
            let frame = Frame::Request(RequestFrame {
                id: RequestId::Number(42),
                method: Method::ItemToolRequestUserInput,
                params: Some(
                    json!({"threadId":context.thread,"turnId":context.turn,"questions":[]}),
                ),
                fields: Default::default(),
            });
            dispatch(Arc::clone(&store), &context, &frame, 10.0)
                .await
                .map(|_| ())
        };
        assert!(result.is_err(), "{phase} {table} {event}");
        assert_eq!(snapshot(&store), before, "{phase} {table} {event}");
        assert!(port.calls.lock().unwrap().is_empty());
    }
}
fn snapshot(store: &RuntimeStore) -> Vec<(String, Vec<Vec<rusqlite::types::Value>>)> {
    let connection = store.test_reader().unwrap();
    let mut tables=connection.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").unwrap();
    tables
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|name| {
            let name = name.unwrap();
            let mut statement = connection
                .prepare(&format!("SELECT * FROM \"{}\"", name.replace('"', "\"\"")))
                .unwrap();
            let count = statement.column_count();
            let mut rows = statement
                .query_map([], |row| {
                    (0..count)
                        .map(|i| row.get(i))
                        .collect::<Result<Vec<_>, _>>()
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            rows.sort_by_key(|row| format!("{row:?}"));
            (name, rows)
        })
        .collect()
}

#[tokio::test]
async fn resolved_pending_or_prepared_callbacks_ack_without_resurrecting_approval() {
    for answered in [false, true] {
        let (_root, store, context) = fixture().await;
        let (id, frame) = pending(&store, &context).await;
        if answered {
            prepare_user_answer(
                &store,
                &id,
                &json!({"answers":{"secret":{"answers":["resolved-secret"]}}}),
                20.0,
                false,
            )
            .await
            .unwrap();
        }
        resolve(
            &store,
            &context,
            &studio_runtime_providers::RequestId::Number(42),
            21.0,
        )
        .await
        .unwrap();
        let decision = dispatch(Arc::clone(&store), &context, &frame, 22.0)
            .await
            .unwrap();
        assert!(matches!(decision, CallbackDecision::Settled(_)));
        let owner = callback_owner(
            &store,
            &context,
            &studio_runtime_providers::RequestId::Number(42),
        );
        assert_eq!(owner.status, ToolRequestStatus::Completed);
        assert!(
            !serde_json::to_string(&owner)
                .unwrap()
                .contains("resolved-secret")
        );
        assert!(
            Outbox::get::<CallbackResponse>(&store.test_reader().unwrap(), &owner.origin_key)
                .unwrap()
                .is_none()
        );
        assert!(
            callback_body(&store, &owner).is_none(),
            "resolution without delivery invents no response"
        );
        // A pending card closes. An answer in delivery is not known to have
        // arrived, so it is unknown, never resolved.
        assert_eq!(
            stored(&store, &id)["status"],
            if answered { "uncertain" } else { "resolved" }
        );
        assert!(prepared_replies(&store, &context).await.unwrap().is_empty());
    }
}
