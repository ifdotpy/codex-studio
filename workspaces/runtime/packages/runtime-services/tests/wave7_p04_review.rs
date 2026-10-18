//! Review regressions: lifecycle races and actual queue/receipt restart boundaries.
#![allow(clippy::unwrap_used)]
#[path = "wave7_p04_callback/fixture.rs"]
mod fixture;
use fixture::*;
use serde_json::json;
use std::sync::{Arc, mpsc};
use studio_runtime_providers::{Frame, RequestId};
use studio_runtime_services::{callback_runtime::*, provider_callbacks::*};
use studio_runtime_store::RuntimeStore;

#[tokio::test]
async fn complaint_event_failure_returns_error_and_rolls_back_complaint() {
    let (_root, store, context) = fixture().await;
    store
        .transaction(|tx| {
            tx.execute_batch(
                "CREATE TRIGGER fail_complaint_event BEFORE INSERT ON runtime_events
                 WHEN NEW.id LIKE 'complaint:%'
                 BEGIN SELECT RAISE(ABORT,'injected complaint event failure'); END;",
            )?;
            Ok(())
        })
        .await
        .expect("arm complaint event failure");
    let count = |table: &str| {
        store
            .read()
            .expect("read counts")
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("row count")
    };
    let before = (count("runtime_complaints"), count("runtime_events"));
    let agent = context.agent.clone();
    let result = store
        .transaction_with_error(move |tx| {
            studio_runtime_services::questions_write::submit_complaint(
                tx,
                &agent,
                "rollback-event-failure",
                "Injected event failure",
                |_| true,
            )
        })
        .await;
    assert!(
        result.is_err(),
        "failed enqueue must reach the service caller"
    );
    assert_eq!(
        (count("runtime_complaints"), count("runtime_events")),
        before
    );
}

#[tokio::test]
async fn resolved_before_admission_or_before_deletion_survives_restart() {
    for before_admission in [true, false] {
        let (root, mut store, context) = fixture().await;
        let frame = request(&context);
        let id = if before_admission {
            None
        } else {
            Some(pending(&store, &context).await.0)
        };
        resolve(&store, &context, &RequestId::Number(42), 20.0)
            .await
            .unwrap();
        if let Some(id) = &id {
            prepare_delete(&store, id, 21.0).await.unwrap();
        }
        drop(store);
        store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
        let decision = dispatch(Arc::clone(&store), &context, &frame, 22.0)
            .await
            .unwrap();
        assert!(matches!(decision, CallbackDecision::Settled(_)));
        bind_journal(&store, &decision, &context, &RequestId::Number(42), 1)
            .await
            .unwrap();
        assert!(prepared_replies(&store, &context).await.unwrap().is_empty());
        if let Some(id) = id {
            assert_eq!(stored(&store, &id)["status"], "deleted");
        } else {
            assert_eq!(
                store
                    .read()
                    .unwrap()
                    .query_row("SELECT count(*) FROM runtime_requests", [], |r| r
                        .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
    }
}

#[tokio::test]
async fn python_admitted_resolution_keeps_its_signature_and_acked_proof_retires() {
    let (_root, store, context) = fixture().await;
    let frame = request(&context);
    let Frame::Request(request_frame) = &frame else {
        panic!("request")
    };
    let mut legacy = template()["legacyCard"].clone();
    legacy["id"] = json!("legacy-resolved-card");
    legacy["rpcId"] = json!(request_frame.id);
    legacy["method"] = json!(request_frame.method.as_str());
    legacy["params"] = request_frame.params.clone().unwrap();
    legacy["status"] = json!("pending");
    card(&store, &legacy).await;

    resolve(&store, &context, &RequestId::Number(42), 20.0)
        .await
        .unwrap();
    assert!(matches!(
        dispatch(Arc::clone(&store), &context, &frame, 21.0)
            .await
            .unwrap(),
        CallbackDecision::Settled(_)
    ));
    let decision = dispatch(Arc::clone(&store), &context, &frame, 22.0)
        .await
        .unwrap();
    bind_journal(&store, &decision, &context, &RequestId::Number(42), 1)
        .await
        .unwrap();
    assert_eq!(checkpoint(&store, &context, 0).await, 0);
    assert_eq!(checkpoint(&store, &context, 1).await, 1);
    assert!(matches!(
        dispatch(Arc::clone(&store), &context, &frame, 23.0)
            .await
            .unwrap(),
        CallbackDecision::Settled(_)
    ));

    let Frame::Request(mut changed) = frame else {
        panic!("request")
    };
    changed.params.as_mut().unwrap()["questions"][0]["question"] = json!("Changed?");
    assert!(matches!(
        dispatch(Arc::clone(&store), &context, &Frame::Request(changed), 24.0).await,
        Err(CallbackError::Conflict)
    ));
}

#[tokio::test]
async fn python_admission_retires_written_payload_without_a_rust_cursor() {
    let (root, mut store, context) = fixture().await;
    let legacy = &template()["legacyCard"];
    card(&store, legacy).await;
    let data = json!({"decision":"accept"});
    let reply = prepare_user_answer(&store, legacy["id"].as_str().unwrap(), &data, 20.0, false)
        .await
        .unwrap()
        .unwrap();
    delivery(&store, &reply.identity, DeliveryOutcome::Submitted)
        .await
        .unwrap();
    delivery(&store, &reply.identity, DeliveryOutcome::Written)
        .await
        .unwrap();
    drop(store);
    store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
    let runtime = CallbackRuntime::new(store.clone(), Arc::new(Port::default()));
    let raw:String=store.read().unwrap().query_row("SELECT result FROM runtime_operation_receipts WHERE json_extract(result,'$.legacyAdmission')=1",[],|r|r.get(0)).unwrap();
    let row: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(row["delivery"], "retired");
    assert!(row.get("body").is_none() && row.get("unanswered").is_none());
    assert!(row.get("journalCursor").is_none());
    assert_eq!(
        runtime
            .answer(legacy["id"].as_str().unwrap(), &data, 21.0)
            .await
            .unwrap()["replayed"],
        true
    );
    assert_eq!(checkpoint(&store, &context, 0).await, 0);
}

#[tokio::test]
async fn queued_reply_is_checked_under_the_same_gate_as_resolution_and_stop() {
    for action in ["resolve", "stop", "delete", "complete"] {
        let (_root, store, context) = fixture().await;
        let (id, _) = pending(&store, &context).await;
        let reply = prepare_user_answer(&store, &id, &json!({"answers":{}}), 20.0, false)
            .await
            .unwrap()
            .unwrap();
        delivery(&store, &reply.identity, DeliveryOutcome::Submitted)
            .await
            .unwrap();
        let (mailbox, receiver) = CallbackMailbox::new(Arc::new(|| {}));
        let runtime = Arc::new(CallbackRuntime::new(store.clone(), mailbox.clone()));
        let queued = reply.clone();
        let waiting = tokio::spawn(async move { mailbox.deliver(&queued, REPLY_DEADLINE).await });
        let queued = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/tests/wave7_p04_review",
            move || receiver.recv(),
        )
        .await
        .unwrap()
        .unwrap();
        if action == "resolve" {
            runtime
                .resolve(&context, &RequestId::Number(42), 21.0)
                .await
                .unwrap();
        } else {
            runtime
                .with_lifecycle(|| async {
                    let mut value = actor(&store, &context);
                    match action {
                        "stop" => value["autoWake"] = json!(false),
                        "delete" => value["deletedAt"] = json!(21.0),
                        _ => {
                            value["status"] = json!("completed");
                            value["turnId"] = serde_json::Value::Null;
                            value["inFlight"] = json!(false);
                        }
                    }
                    put_actor(&store, &value).await;
                    Ok(())
                })
                .await
                .unwrap();
        }
        let mut sent = false;
        let result = runtime
            .submit_committed(&queued.reply, || async {
                sent = true;
                Ok(())
            })
            .await;
        assert!(!sent, "{action}");
        assert!(!matches!(result, Ok(Some(()))));
        drop(queued.receipt);
        assert!(waiting.await.unwrap().is_err());
    }
}

#[tokio::test]
async fn send_acceptance_and_resolution_are_serialized_without_holding_a_store_transaction() {
    let (_root, store, context) = fixture().await;
    let (id, _) = pending(&store, &context).await;
    let reply = prepare_user_answer(&store, &id, &json!({"answers":{}}), 20.0, false)
        .await
        .unwrap()
        .unwrap();
    delivery(&store, &reply.identity, DeliveryOutcome::Submitted)
        .await
        .unwrap();
    let runtime = Arc::new(CallbackRuntime::new(
        store.clone(),
        Arc::new(Port::default()),
    ));
    assert_eq!(
        runtime
            .submit_committed(&reply, || async {
                // Taking a store transaction here proves the check released its transaction.
                store
                    .transaction_with::<_, CallbackError>(|_| Ok(()))
                    .await?;
                Ok(())
            })
            .await
            .unwrap(),
        Some(())
    );
    runtime
        .resolve(&context, &RequestId::Number(42), 21.0)
        .await
        .unwrap();
    assert_eq!(stored(&store, &id)["status"], "resolved");
}

#[tokio::test]
async fn late_written_settles_card_without_reviving_completed_failed_or_replaced_turn() {
    for status in ["completed", "failed", "stopped", "deleted", "replaced"] {
        let (_root, store, context) = fixture().await;
        let (id, _) = pending(&store, &context).await;
        let reply = prepare_user_answer(&store, &id, &json!({"answers":{}}), 20.0, false)
            .await
            .unwrap()
            .unwrap();
        delivery(&store, &reply.identity, DeliveryOutcome::Submitted)
            .await
            .unwrap();
        let mut value = actor(&store, &context);
        value["status"] = json!(if status == "replaced" {
            "running"
        } else {
            status
        });
        value["inFlight"] = json!(false);
        value["turnId"] = if status == "replaced" {
            json!("other-turn")
        } else {
            serde_json::Value::Null
        };
        if status == "deleted" {
            value["deletedAt"] = json!(21.0);
        }
        put_actor(&store, &value).await;
        delivery(&store, &reply.identity, DeliveryOutcome::Written)
            .await
            .unwrap();
        assert_eq!(actor(&store, &context), value, "{status}");
        assert_eq!(stored(&store, &id)["status"], "answered");
    }
}

#[tokio::test]
async fn every_foreign_receipt_dimension_is_held_unknown() {
    struct Foreign(usize);
    impl CallbackReplyPort for Foreign {
        fn has_live_session(&self, _: &CallbackContext) -> bool {
            true
        }

        fn deliver<'a>(
            &'a self,
            reply: &'a Reply,
            _: std::time::Duration,
        ) -> CallbackReplyFuture<'a> {
            Box::pin(async move {
                let mut identity = reply.identity.clone();
                match self.0 {
                    0 => identity.context.agent.push_str("other"),
                    1 => identity.context.epoch += 1,
                    2 => identity.context.account.push_str("other"),
                    3 => identity.context.connection.push_str("other"),
                    4 => identity.context.generation += 1,
                    5 => identity.attempt.push_str("other"),
                    6 => identity.rpc_id = RequestId::Number(43),
                    7 => identity.context.thread.push_str("other"),
                    _ => identity.context.turn.push_str("other"),
                }
                Ok(ReplyReceipt {
                    identity,
                    outcome: ReplyOutcome::Written,
                })
            })
        }
    }
    for dimension in 0..9 {
        let (_root, store, context) = fixture().await;
        let (id, _) = pending(&store, &context).await;
        let runtime = CallbackRuntime::new(store.clone(), Arc::new(Foreign(dimension)));
        assert!(matches!(
            runtime.answer(&id, &json!({"answers":{}}), 20.0).await,
            Err(CallbackError::Unknown)
        ));
        assert_eq!(stored(&store, &id)["status"], "uncertain");
        assert!(prepared_replies(&store, &context).await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn answer_and_cancellation_restart_at_each_real_store_queue_and_transport_boundary() {
    // 0 before admission, 1 admission, 2 bind, 3 answer/delete intent, 4 Submitted,
    // 5 actual mailbox acceptance, 6 native submission, 7 typed receipt received,
    // 8 Written persisted, 9 ACK persisted, 10 payload retirement persisted.
    for cancel in [false, true] {
        for boundary in 0..=10 {
            let (root, mut store, context) = fixture().await;
            let frame = request(&context);
            let (native_tx, native_rx) = mpsc::channel();
            let mut decision = None;
            let mut id = None;
            if boundary >= 1 {
                let accepted = dispatch(Arc::clone(&store), &context, &frame, 10.0)
                    .await
                    .unwrap();
                let CallbackDecision::Pending(card_id) = &accepted else {
                    panic!("pending")
                };
                id = Some(card_id.clone());
                decision = Some(accepted);
            }
            if boundary >= 2 {
                bind_journal(
                    &store,
                    decision.as_ref().unwrap(),
                    &context,
                    &RequestId::Number(42),
                    1,
                )
                .await
                .unwrap();
            }
            if boundary >= 3 {
                let reply = if cancel {
                    prepare_delete(&store, id.as_ref().unwrap(), 20.0).await
                } else {
                    prepare_user_answer(
                        &store,
                        id.as_ref().unwrap(),
                        &json!({"answers":{}}),
                        20.0,
                        false,
                    )
                    .await
                }
                .unwrap()
                .unwrap();
                if boundary >= 4 {
                    delivery(&store, &reply.identity, DeliveryOutcome::Submitted)
                        .await
                        .unwrap();
                }
                if boundary >= 5 {
                    let (mailbox, receiver) = CallbackMailbox::new(Arc::new(|| {}));
                    let runtime = CallbackRuntime::new(store.clone(), mailbox.clone());
                    let for_port = reply.clone();
                    let waiting =
                        tokio::spawn(
                            async move { mailbox.deliver(&for_port, REPLY_DEADLINE).await },
                        );
                    let queued = studio_runtime_process::blocking_pool::spawn_blocking(
                        "runtime-services/tests/wave7_p04_review",
                        move || receiver.recv(),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                    if boundary >= 6 {
                        runtime
                            .submit_committed(&queued.reply, || async {
                                native_tx.send(queued.reply.frame()).unwrap();
                                Ok(())
                            })
                            .await
                            .unwrap()
                            .unwrap();
                    }
                    if boundary >= 7 {
                        assert!(
                            queued
                                .receipt
                                .send(ReplyReceipt {
                                    identity: reply.identity.clone(),
                                    outcome: ReplyOutcome::Written,
                                })
                                .is_ok()
                        );
                    } else {
                        drop(queued.receipt);
                    }
                    let receipt = waiting.await.unwrap();
                    if boundary >= 7 {
                        assert_eq!(receipt.unwrap().outcome, ReplyOutcome::Written);
                    } else {
                        assert!(receipt.is_err());
                    }
                }
                if boundary >= 8 {
                    delivery(&store, &reply.identity, DeliveryOutcome::Written)
                        .await
                        .unwrap();
                }
                if boundary >= 9 {
                    ack(&store, &context, 1).await;
                }
                if boundary >= 10 {
                    assert_eq!(retire(&store, &context).await, 1);
                }
            }
            // Every branch stops here before the next boundary; no hidden bind/ACK.
            drop(store);
            store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
            let port = Arc::new(Port::default());
            let runtime = CallbackRuntime::new(store.clone(), port.clone());
            if boundary < 2 {
                let accepted = runtime.dispatch(&context, &frame, 11.0).await.unwrap();
                let CallbackDecision::Pending(card_id) = &accepted else {
                    panic!("pending")
                };
                id = Some(card_id.clone());
                bind_journal(&store, &accepted, &context, &RequestId::Number(42), 1)
                    .await
                    .unwrap();
            }
            let id = id.unwrap();
            if boundary < 3 {
                if cancel {
                    runtime.delete_question(&id, 20.0).await.unwrap();
                } else {
                    runtime
                        .answer(&id, &json!({"answers":{}}), 20.0)
                        .await
                        .unwrap();
                }
            } else if boundary == 3 {
                let prepared = prepared_replies(&store, &context).await.unwrap();
                assert_eq!(prepared.len(), 1);
                runtime.deliver(&prepared[0]).await.unwrap();
            } else if boundary < 8 {
                assert!(prepared_replies(&store, &context).await.unwrap().is_empty());
                assert!(
                    runtime
                        .answer(&id, &json!({"answers":{}}), 22.0)
                        .await
                        .is_err()
                );
                runtime
                    .recover_connection(&context.account, &context.connection, 22.0)
                    .await
                    .unwrap();
                assert_eq!(stored(&store, &id)["status"], "uncertain");
            } else {
                if cancel {
                    runtime.delete_question(&id, 22.0).await.unwrap();
                } else {
                    assert_eq!(
                        runtime
                            .answer(&id, &json!({"answers":{}}), 22.0)
                            .await
                            .unwrap()["replayed"],
                        true
                    );
                }
                if boundary == 9 {
                    assert_eq!(retire(&store, &context).await, 1);
                }
            }
            let native: Vec<_> = native_rx.try_iter().collect();
            assert_eq!(native.len(), usize::from(boundary >= 6));
            assert_eq!(port.calls.lock().unwrap().len(), usize::from(boundary < 4));
            assert!(
                native.len() + port.calls.lock().unwrap().len() <= 1,
                "{cancel} {boundary}"
            );
        }
    }
}

#[tokio::test]
async fn python_failures_null_decisions_and_race_winners_are_preserved() {
    for case in template()["failures"].as_array().unwrap() {
        let (_root, store, context) = fixture().await;
        seed_case(&store, case).await;
        let before = logical_snapshot(&store);
        let port = Arc::new(Port::default());
        *port.next.lock().unwrap() = Some(if case["outcome"] == "notSent" {
            ReplyOutcome::NotSent
        } else {
            ReplyOutcome::Unknown
        });
        let runtime = CallbackRuntime::new(store.clone(), port);
        let id = case["before"]["id"].as_str().unwrap();
        let result = if case["operation"] == "delete" {
            runtime.delete_question(id, 20.0).await
        } else {
            runtime.answer(id, &json!({"answers":{}}), 20.0).await
        };
        assert!(result.is_err());
        assert_eq!(stored(&store, id), case["after"]);
        assert_eq!(actor(&store, &context), case["afterActor"]);
        assert_writes(&before, &store, &case["writes"]);
    }
    for case in template()["invalid"].as_array().unwrap() {
        let (_root, store, context) = fixture().await;
        seed_case(&store, case).await;
        let before = logical_snapshot(&store);
        let port = Arc::new(Port::default());
        let runtime = CallbackRuntime::new(store.clone(), port.clone());
        let id = case["before"]["id"].as_str().unwrap();
        let result = runtime.answer(id, &case["data"], 20.0).await;
        if let Some(error) = case["expected"].get("error") {
            assert_eq!(result.unwrap_err().to_string(), error.as_str().unwrap());
            assert!(port.calls.lock().unwrap().is_empty());
        } else {
            assert_eq!(result.unwrap(), case["expected"]["result"]);
        }
        assert_eq!(stored(&store, id), case["after"]);
        assert_eq!(actor(&store, &context), case["afterActor"]);
        assert_writes(&before, &store, &case["writes"]);
    }
    for case in template()["races"].as_array().unwrap() {
        let (_root, store, context) = fixture().await;
        card(&store, &case["before"]).await;
        put_actor(&store, &case["beforeActor"]).await;
        let id = case["before"]["id"].as_str().unwrap();
        let reply = prepare_user_answer(&store, id, &json!({"answers":{}}), 20.0, false)
            .await
            .unwrap()
            .unwrap();
        delivery(&store, &reply.identity, DeliveryOutcome::Submitted)
            .await
            .unwrap();
        if case["outcome"] == "resolved" {
            let before_resolution = logical_snapshot(&store);
            resolve(&store, &context, &reply.identity.rpc_id, 20.0)
                .await
                .unwrap();
            assert_writes(
                &before_resolution,
                &store,
                &case["winner"]["resolutionWrites"],
            );
            assert!(
                delivery(&store, &reply.identity, DeliveryOutcome::Written)
                    .await
                    .is_err()
            );
            assert_eq!(stored(&store, id), case["winner"]["after"]);
        } else {
            put_actor(&store, &case["winner"]["afterActor"]).await;
            delivery(&store, &reply.identity, DeliveryOutcome::Written)
                .await
                .unwrap();
            assert_eq!(actor(&store, &context), case["winner"]["afterActor"]);
            assert_eq!(stored(&store, id), case["pythonAfter"]["after"]);
            assert_eq!(case["pythonAfter"]["afterActor"]["status"], "running");
        }
    }
}
