//! Durable human callback round trips against Python-written records.
#![allow(clippy::unwrap_used)]
#[path = "wave7_p04_callback/fixture.rs"]
mod fixture;
use fixture::*;
use serde_json::json;
use std::sync::Arc;
use studio_runtime_services::{callback_runtime::*, provider_callbacks::*};
use studio_runtime_store::RuntimeStore;

#[test]
fn complete_python_answer_and_delete_corpus() {
    let (_root, store, context) = fixture();
    let port = Arc::new(Port::default());
    let runtime = CallbackRuntime::new(store.clone(), port.clone());
    for case in template()["fullAnswers"].as_array().unwrap() {
        seed_case(&store, case);
        let before_rows = logical_snapshot(&store);
        let id = case["before"]["id"].as_str().unwrap();
        assert_eq!(
            runtime.answer(id, &case["data"], 20.0).unwrap(),
            case["response"]
        );
        assert_eq!(
            runtime.answer(id, &case["data"], 21.0).unwrap(),
            case["replay"]
        );
        assert_eq!(stored(&store, id), case["after"]);
        assert_eq!(actor(&store, &context), case["afterActor"]);
        assert_writes(&before_rows, &store, &case["writes"]);
        assert_eq!(
            frame_value(&port.calls.lock().unwrap().last().unwrap().frame()),
            case["frame"]
        );
    }
    assert_eq!(port.calls.lock().unwrap().len(), 30);
    for case in template()["deletes"].as_array().unwrap() {
        seed_case(&store, case);
        let before_rows = logical_snapshot(&store);
        let id = case["before"]["id"].as_str().unwrap();
        let before = port.calls.lock().unwrap().len();
        let result = runtime.delete_question(id, 20.0);
        if let Some(error) = case["expected"].get("error") {
            assert_eq!(result.unwrap_err().to_string(), error.as_str().unwrap());
        } else {
            assert_eq!(result.unwrap(), case["expected"]["result"]);
        }
        assert_eq!(stored(&store, id), case["after"], "{id}");
        assert_eq!(actor(&store, &context), case["afterActor"]);
        assert_writes(&before_rows, &store, &case["writes"]);
        let frames: Vec<_> = port.calls.lock().unwrap()[before..]
            .iter()
            .map(|r| frame_value(&r.frame()))
            .collect();
        assert_eq!(json!(frames), case["frames"]);
    }
}

#[test]
fn retirement_requires_checkpoint_and_preserves_http_replay_and_unknown_proof() {
    let (_root, store, context) = fixture();
    let (id, frame) = pending(&store, &context);
    let data = json!({"answers":{"secret":{"answers":["secret-payload"]}}});
    let port = Arc::new(Port::default());
    let runtime = CallbackRuntime::new(store.clone(), port.clone());
    runtime.answer(&id, &data, 20.0).unwrap();
    assert_eq!(checkpoint(&store, &context, 0), 0);
    assert_eq!(checkpoint(&store, &context, 1), 1);
    assert_eq!(checkpoint(&store, &context, 1), 0);
    let raw:String=store.read().unwrap().query_row("SELECT group_concat(result) FROM runtime_operation_receipts WHERE signature='provider-callback'",[],|r|r.get(0)).unwrap();
    assert!(!raw.contains("secret-payload"));
    assert!(matches!(
        dispatch(&store, &context, &frame, 21.0).unwrap(),
        CallbackDecision::Settled(_)
    ));
    assert_eq!(runtime.answer(&id, &data, 21.0).unwrap()["replayed"], true);
    assert!(matches!(
        runtime.answer(&id, &json!({"answers":{}}), 21.0),
        Err(CallbackError::Conflict)
    ));
    assert_eq!(port.calls.lock().unwrap().len(), 1);
}

#[test]
fn not_sent_creates_new_attempt_and_foreign_receipt_is_unknown() {
    let (_root, store, context) = fixture();
    let (id, _) = pending(&store, &context);
    let port = Arc::new(Port::default());
    *port.next.lock().unwrap() = Some(ReplyOutcome::NotSent);
    let runtime = CallbackRuntime::new(store.clone(), port.clone());
    let data = json!({"answers":{}});
    assert!(runtime.answer(&id, &data, 20.0).is_err());
    assert_eq!(stored(&store, &id)["status"], "pending");
    let old = port.calls.lock().unwrap()[0].identity.clone();
    *port.foreign.lock().unwrap() = true;
    assert!(matches!(
        runtime.answer(&id, &data, 21.0),
        Err(CallbackError::Unknown)
    ));
    let new = port.calls.lock().unwrap()[1].identity.clone();
    assert_ne!(old.attempt, new.attempt);
    assert!(matches!(
        delivery(&store, &old, DeliveryOutcome::Written),
        Err(CallbackError::Stale)
    ));
    assert_eq!(stored(&store, &id)["status"], "uncertain");
    assert_eq!(checkpoint(&store, &context, 1), 0);
    assert!(prepared_replies(&store, &context).unwrap().is_empty());
}

#[test]
fn python_resolution_rows_match_after_completed_or_cleared_turn() {
    let (_root, store, context) = fixture();
    for case in template()["resolved"].as_array().unwrap() {
        card(&store, &case["card"]);
        store
            .transaction_with::<_, studio_runtime_store::StoreError>(|tx| {
                tx.execute(
                    "UPDATE runtime_agents SET record=?1 WHERE id=?2",
                    rusqlite::params![case["actor"].to_string(), context.agent],
                )?;
                Ok(())
            })
            .unwrap();
        resolve(
            &store,
            &context,
            &studio_runtime_providers::RequestId::String("resolve-rpc".into()),
            20.0,
        )
        .unwrap();
        assert_eq!(stored(&store, "resolve-card"), case["expected"]);
        let raw: String = store
            .read()
            .unwrap()
            .query_row(
                "SELECT record FROM runtime_agents WHERE id=?1",
                [&context.agent],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&raw).unwrap(),
            case["expectedActor"]
        );
    }
}

fn frame_value(frame: &studio_runtime_providers::Frame) -> serde_json::Value {
    serde_json::from_slice(&studio_runtime_providers::encode_line(frame).unwrap()).unwrap()
}

#[test]
fn mailbox_timeout_is_unknown_and_closed_queue_proves_not_sent() {
    let (_root, store, context) = fixture();
    let (id, _) = pending(&store, &context);
    let reply = prepare_user_answer(&store, &id, &json!({"answers":{}}), 20.0, false)
        .unwrap()
        .unwrap();
    let (mailbox, receiver) = CallbackMailbox::new(Arc::new(|| {}));
    assert!(matches!(
        mailbox.deliver(&reply, std::time::Duration::ZERO),
        Err(CallbackError::Unknown)
    ));
    assert_eq!(receiver.try_recv().unwrap().reply, reply);
    drop(receiver);
    assert_eq!(
        mailbox.deliver(&reply, REPLY_DEADLINE).unwrap().outcome,
        ReplyOutcome::NotSent
    );
}

#[test]
fn binding_after_http_commit_still_retires_payload_and_preserves_attempt() {
    use studio_runtime_providers::{Frame, Method, RequestFrame, RequestId};
    let (_root, store, context) = fixture();
    let frame = Frame::Request(RequestFrame {
        id: RequestId::Number(12),
        method: Method::ItemToolRequestUserInput,
        params: Some(json!({"threadId":context.thread,"turnId":context.turn,"questions":[]})),
        fields: Default::default(),
    });
    let decision = dispatch(&store, &context, &frame, 10.0).unwrap();
    let CallbackDecision::Pending(id) = &decision else {
        panic!("pending")
    };
    let runtime = CallbackRuntime::new(store.clone(), Arc::new(Port::default()));
    runtime.answer(id, &json!({"answers":{}}), 20.0).unwrap();
    bind_journal(&store, &decision, &context, &RequestId::Number(12), 1).unwrap();
    assert_eq!(checkpoint(&store, &context, 1), 1);
    assert!(matches!(
        dispatch(&store, &context, &frame, 21.0).unwrap(),
        CallbackDecision::Settled(_)
    ));
}

#[test]
fn tools_rotate_attempt_after_proven_not_sent_without_reexecuting_effect() {
    use studio_runtime_providers::{Frame, Method, RequestFrame, RequestId};
    let (_root, store, context) = fixture();
    let frame = Frame::Request(RequestFrame {
        id: RequestId::Number(25),
        method: Method::ItemToolCall,
        params: Some(
            json!({"threadId":context.thread,"turnId":context.turn,"callId":"rotate","tool":"orchestration_title","arguments":{"title":"Once"}}),
        ),
        fields: Default::default(),
    });
    let CallbackDecision::Answer(first) = dispatch(&store, &context, &frame, 10.0).unwrap() else {
        panic!("answer")
    };
    delivery(&store, &first.identity, DeliveryOutcome::Submitted).unwrap();
    delivery(&store, &first.identity, DeliveryOutcome::NotSent).unwrap();
    let CallbackDecision::Answer(second) = dispatch(&store, &context, &frame, 11.0).unwrap() else {
        panic!("answer")
    };
    assert_eq!(first.body, second.body);
    assert_ne!(first.identity.attempt, second.identity.attempt);
    assert!(matches!(
        delivery(&store, &first.identity, DeliveryOutcome::Written),
        Err(CallbackError::Stale)
    ));
    let count: i64 = store
        .read()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM runtime_tool_results WHERE id='thread-0:rotate'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn failed_store_calls_roll_back_card_outbox_and_publication_before_handoff() {
    for (table, event, phase) in [
        ("runtime_requests", "INSERT", "admit"),
        ("runtime_agents", "UPDATE", "admit"),
        ("runtime_operation_receipts", "INSERT", "admit"),
        ("sync_entities", "INSERT", "admit"),
        ("runtime_requests", "INSERT", "answer"),
        ("runtime_operation_receipts", "INSERT", "answer"),
        ("runtime_operation_receipts", "UPDATE", "answer"),
    ] {
        let (_root, store, context) = fixture();
        let id = if phase == "answer" {
            Some(pending(&store, &context).0)
        } else {
            None
        };
        store.transaction_with::<_,studio_runtime_store::StoreError>(|tx| {
            tx.execute_batch(&format!("CREATE TRIGGER callback_failpoint BEFORE {event} ON {table} BEGIN SELECT RAISE(ABORT,'callback failpoint'); END"))?; Ok(())
        }).unwrap();
        let before = snapshot(&store);
        let port = Arc::new(Port::default());
        let runtime = CallbackRuntime::new(store.clone(), port.clone());
        let result = if let Some(id) = id {
            runtime
                .answer(&id, &json!({"answers":{}}), 20.0)
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
            dispatch(&store, &context, &frame, 10.0).map(|_| ())
        };
        assert!(result.is_err(), "{phase} {table} {event}");
        assert_eq!(snapshot(&store), before, "{phase} {table} {event}");
        assert!(port.calls.lock().unwrap().is_empty());
    }
}
fn snapshot(store: &RuntimeStore) -> Vec<(String, Vec<Vec<rusqlite::types::Value>>)> {
    let connection = store.read().unwrap();
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

#[test]
fn resolved_pending_or_prepared_callbacks_ack_without_resurrecting_approval() {
    for answered in [false, true] {
        let (_root, store, context) = fixture();
        let (id, frame) = pending(&store, &context);
        if answered {
            prepare_user_answer(
                &store,
                &id,
                &json!({"answers":{"secret":{"answers":["resolved-secret"]}}}),
                20.0,
                false,
            )
            .unwrap();
        }
        resolve(
            &store,
            &context,
            &studio_runtime_providers::RequestId::Number(42),
            21.0,
        )
        .unwrap();
        let decision = dispatch(&store, &context, &frame, 22.0).unwrap();
        assert!(matches!(decision, CallbackDecision::Settled(_)));
        bind_journal(
            &store,
            &decision,
            &context,
            &studio_runtime_providers::RequestId::Number(42),
            2,
        )
        .unwrap();
        let raw:String=store.read().unwrap().query_row("SELECT group_concat(result) FROM runtime_operation_receipts WHERE signature='provider-callback'",[],|r|r.get(0)).unwrap();
        assert!(!raw.contains("resolved-secret"));
        assert_eq!(stored(&store, &id)["status"], "resolved");
        assert!(prepared_replies(&store, &context).unwrap().is_empty());
    }
}
