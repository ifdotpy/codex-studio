//! Review regressions: lifecycle races and actual queue/receipt restart boundaries.
#![allow(clippy::unwrap_used)]
#[path = "wave7_p04_callback/fixture.rs"]
mod fixture;
use fixture::*;
use serde_json::json;
use std::sync::{Arc, mpsc};
use studio_runtime_providers::RequestId;
use studio_runtime_services::{callback_runtime::*, provider_callbacks::*};
use studio_runtime_store::RuntimeStore;

#[test]
fn resolved_before_admission_or_before_deletion_survives_restart() {
    for before_admission in [true, false] {
        let (root, mut store, context) = fixture();
        let frame = request(&context);
        let id = if before_admission {
            None
        } else {
            Some(pending(&store, &context).0)
        };
        resolve(&store, &context, &RequestId::Number(42), 20.0).unwrap();
        if let Some(id) = &id {
            prepare_delete(&store, id, 21.0).unwrap();
        }
        drop(store);
        store = Arc::new(RuntimeStore::open(Some(root.path())).unwrap());
        let decision = dispatch(&store, &context, &frame, 22.0).unwrap();
        assert!(matches!(decision, CallbackDecision::Settled(_)));
        bind_journal(&store, &decision, &context, &RequestId::Number(42), 1).unwrap();
        assert!(prepared_replies(&store, &context).unwrap().is_empty());
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

#[test]
fn python_admission_retires_written_payload_without_a_rust_cursor() {
    let (root, mut store, context) = fixture();
    let legacy = &template()["legacyCard"];
    card(&store, legacy);
    let data = json!({"decision":"accept"});
    let reply = prepare_user_answer(&store, legacy["id"].as_str().unwrap(), &data, 20.0, false)
        .unwrap()
        .unwrap();
    delivery(&store, &reply.identity, DeliveryOutcome::Submitted).unwrap();
    delivery(&store, &reply.identity, DeliveryOutcome::Written).unwrap();
    drop(store);
    store = Arc::new(RuntimeStore::open(Some(root.path())).unwrap());
    let runtime = CallbackRuntime::new(store.clone(), Arc::new(Port::default()));
    let raw:String=store.read().unwrap().query_row("SELECT result FROM runtime_operation_receipts WHERE json_extract(result,'$.legacyAdmission')=1",[],|r|r.get(0)).unwrap();
    let row: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(row["delivery"], "retired");
    assert!(row.get("body").is_none() && row.get("unanswered").is_none());
    assert!(row.get("journalCursor").is_none());
    assert_eq!(
        runtime
            .answer(legacy["id"].as_str().unwrap(), &data, 21.0)
            .unwrap()["replayed"],
        true
    );
    assert_eq!(checkpoint(&store, &context, 0), 0);
}

#[test]
fn queued_reply_is_checked_under_the_same_gate_as_resolution_and_stop() {
    for action in ["resolve", "stop", "delete", "complete"] {
        let (_root, store, context) = fixture();
        let (id, _) = pending(&store, &context);
        let reply = prepare_user_answer(&store, &id, &json!({"answers":{}}), 20.0, false)
            .unwrap()
            .unwrap();
        delivery(&store, &reply.identity, DeliveryOutcome::Submitted).unwrap();
        let (mailbox, receiver) = CallbackMailbox::new(Arc::new(|| {}));
        let runtime = Arc::new(CallbackRuntime::new(store.clone(), mailbox.clone()));
        let queued = reply.clone();
        let waiting = std::thread::spawn(move || mailbox.deliver(&queued, REPLY_DEADLINE));
        let queued = receiver.recv().unwrap();
        if action == "resolve" {
            runtime
                .resolve(&context, &RequestId::Number(42), 21.0)
                .unwrap();
        } else {
            runtime
                .with_lifecycle(|| {
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
                    put_actor(&store, &value);
                    Ok(())
                })
                .unwrap();
        }
        let mut sent = false;
        let result = runtime.submit_committed(&queued.reply, || {
            sent = true;
            Ok(())
        });
        assert!(!sent, "{action}");
        assert!(!matches!(result, Ok(Some(()))));
        drop(queued.receipt);
        assert!(waiting.join().unwrap().is_err());
    }
}

#[test]
fn send_acceptance_and_resolution_are_serialized_without_holding_a_store_transaction() {
    let (_root, store, context) = fixture();
    let (id, _) = pending(&store, &context);
    let reply = prepare_user_answer(&store, &id, &json!({"answers":{}}), 20.0, false)
        .unwrap()
        .unwrap();
    delivery(&store, &reply.identity, DeliveryOutcome::Submitted).unwrap();
    let runtime = Arc::new(CallbackRuntime::new(
        store.clone(),
        Arc::new(Port::default()),
    ));
    let (start_tx, start_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let other = runtime.clone();
    let other_context = context.clone();
    let resolver = std::thread::spawn(move || {
        start_rx.recv().unwrap();
        other
            .resolve(&other_context, &RequestId::Number(42), 21.0)
            .unwrap();
        done_tx.send(()).unwrap();
    });
    assert_eq!(
        runtime
            .submit_committed(&reply, || {
                // Taking a store transaction here proves the check released its transaction.
                store.transaction_with::<_, CallbackError>(|_| Ok(()))?;
                start_tx.send(()).unwrap();
                assert!(done_rx.try_recv().is_err());
                Ok(())
            })
            .unwrap(),
        Some(())
    );
    resolver.join().unwrap();
    done_rx.recv().unwrap();
    assert_eq!(stored(&store, &id)["status"], "resolved");
}

#[test]
fn late_written_settles_card_without_reviving_completed_failed_or_replaced_turn() {
    for status in ["completed", "failed", "stopped", "deleted", "replaced"] {
        let (_root, store, context) = fixture();
        let (id, _) = pending(&store, &context);
        let reply = prepare_user_answer(&store, &id, &json!({"answers":{}}), 20.0, false)
            .unwrap()
            .unwrap();
        delivery(&store, &reply.identity, DeliveryOutcome::Submitted).unwrap();
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
        put_actor(&store, &value);
        delivery(&store, &reply.identity, DeliveryOutcome::Written).unwrap();
        assert_eq!(actor(&store, &context), value, "{status}");
        assert_eq!(stored(&store, &id)["status"], "answered");
    }
}

#[test]
fn every_foreign_receipt_dimension_is_held_unknown() {
    struct Foreign(usize);
    impl CallbackReplyPort for Foreign {
        fn deliver(
            &self,
            reply: &Reply,
            _: std::time::Duration,
        ) -> Result<ReplyReceipt, CallbackError> {
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
        }
    }
    for dimension in 0..9 {
        let (_root, store, context) = fixture();
        let (id, _) = pending(&store, &context);
        let runtime = CallbackRuntime::new(store.clone(), Arc::new(Foreign(dimension)));
        assert!(matches!(
            runtime.answer(&id, &json!({"answers":{}}), 20.0),
            Err(CallbackError::Unknown)
        ));
        assert_eq!(stored(&store, &id)["status"], "uncertain");
        assert!(prepared_replies(&store, &context).unwrap().is_empty());
    }
}

#[test]
fn answer_and_cancellation_restart_at_each_real_store_queue_and_transport_boundary() {
    // 0 before admission, 1 admission, 2 bind, 3 answer/delete intent, 4 Submitted,
    // 5 actual mailbox acceptance, 6 native submission, 7 typed receipt received,
    // 8 Written persisted, 9 ACK persisted, 10 payload retirement persisted.
    for cancel in [false, true] {
        for boundary in 0..=10 {
            let (root, mut store, context) = fixture();
            let frame = request(&context);
            let (native_tx, native_rx) = mpsc::channel();
            let mut decision = None;
            let mut id = None;
            if boundary >= 1 {
                let accepted = dispatch(&store, &context, &frame, 10.0).unwrap();
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
                .unwrap();
            }
            if boundary >= 3 {
                let reply = if cancel {
                    prepare_delete(&store, id.as_ref().unwrap(), 20.0)
                } else {
                    prepare_user_answer(
                        &store,
                        id.as_ref().unwrap(),
                        &json!({"answers":{}}),
                        20.0,
                        false,
                    )
                }
                .unwrap()
                .unwrap();
                if boundary >= 4 {
                    delivery(&store, &reply.identity, DeliveryOutcome::Submitted).unwrap();
                }
                if boundary >= 5 {
                    let (mailbox, receiver) = CallbackMailbox::new(Arc::new(|| {}));
                    let runtime = CallbackRuntime::new(store.clone(), mailbox.clone());
                    let for_port = reply.clone();
                    let waiting =
                        std::thread::spawn(move || mailbox.deliver(&for_port, REPLY_DEADLINE));
                    let queued = receiver.recv().unwrap();
                    if boundary >= 6 {
                        runtime
                            .submit_committed(&queued.reply, || {
                                native_tx.send(queued.reply.frame()).unwrap();
                                Ok(())
                            })
                            .unwrap()
                            .unwrap();
                    }
                    if boundary >= 7 {
                        queued
                            .receipt
                            .send(ReplyReceipt {
                                identity: reply.identity.clone(),
                                outcome: ReplyOutcome::Written,
                            })
                            .unwrap();
                    }
                    drop(queued);
                    let receipt = waiting.join().unwrap();
                    if boundary >= 7 {
                        assert_eq!(receipt.unwrap().outcome, ReplyOutcome::Written);
                    } else {
                        assert!(receipt.is_err());
                    }
                }
                if boundary >= 8 {
                    delivery(&store, &reply.identity, DeliveryOutcome::Written).unwrap();
                }
                if boundary >= 9 {
                    ack(&store, &context, 1);
                }
                if boundary >= 10 {
                    assert_eq!(retire(&store, &context), 1);
                }
            }
            // Every branch stops here before the next boundary; no hidden bind/ACK.
            drop(store);
            store = Arc::new(RuntimeStore::open(Some(root.path())).unwrap());
            let port = Arc::new(Port::default());
            let runtime = CallbackRuntime::new(store.clone(), port.clone());
            if boundary < 2 {
                let accepted = runtime.dispatch(&context, &frame, 11.0).unwrap();
                let CallbackDecision::Pending(card_id) = &accepted else {
                    panic!("pending")
                };
                id = Some(card_id.clone());
                bind_journal(&store, &accepted, &context, &RequestId::Number(42), 1).unwrap();
            }
            let id = id.unwrap();
            if boundary < 3 {
                if cancel {
                    runtime.delete_question(&id, 20.0).unwrap();
                } else {
                    runtime.answer(&id, &json!({"answers":{}}), 20.0).unwrap();
                }
            } else if boundary == 3 {
                let prepared = prepared_replies(&store, &context).unwrap();
                assert_eq!(prepared.len(), 1);
                runtime.deliver(&prepared[0]).unwrap();
            } else if boundary < 8 {
                assert!(prepared_replies(&store, &context).unwrap().is_empty());
                assert!(runtime.answer(&id, &json!({"answers":{}}), 22.0).is_err());
                runtime
                    .recover_connection(&context.account, &context.connection, 22.0)
                    .unwrap();
                assert_eq!(stored(&store, &id)["status"], "uncertain");
            } else {
                if cancel {
                    runtime.delete_question(&id, 22.0).unwrap();
                } else {
                    assert_eq!(
                        runtime.answer(&id, &json!({"answers":{}}), 22.0).unwrap()["replayed"],
                        true
                    );
                }
                if boundary == 9 {
                    assert_eq!(retire(&store, &context), 1);
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

#[test]
fn python_failures_null_decisions_and_race_winners_are_preserved() {
    for case in template()["failures"].as_array().unwrap() {
        let (_root, store, context) = fixture();
        seed_case(&store, case);
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
            runtime.delete_question(id, 20.0)
        } else {
            runtime.answer(id, &json!({"answers":{}}), 20.0)
        };
        assert!(result.is_err());
        assert_eq!(stored(&store, id), case["after"]);
        assert_eq!(actor(&store, &context), case["afterActor"]);
        assert_writes(&before, &store, &case["writes"]);
    }
    for case in template()["invalid"].as_array().unwrap() {
        let (_root, store, context) = fixture();
        seed_case(&store, case);
        let before = logical_snapshot(&store);
        let port = Arc::new(Port::default());
        let runtime = CallbackRuntime::new(store.clone(), port.clone());
        let id = case["before"]["id"].as_str().unwrap();
        let result = runtime.answer(id, &case["data"], 20.0);
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
        let (_root, store, context) = fixture();
        card(&store, &case["before"]);
        let id = case["before"]["id"].as_str().unwrap();
        let reply = prepare_user_answer(&store, id, &json!({"answers":{}}), 20.0, false)
            .unwrap()
            .unwrap();
        delivery(&store, &reply.identity, DeliveryOutcome::Submitted).unwrap();
        if case["outcome"] == "resolved" {
            resolve(&store, &context, &reply.identity.rpc_id, 20.0).unwrap();
            assert!(delivery(&store, &reply.identity, DeliveryOutcome::Written).is_err());
            assert_eq!(stored(&store, id), case["winner"]["after"]);
        } else {
            put_actor(&store, &case["winner"]["afterActor"]);
            delivery(&store, &reply.identity, DeliveryOutcome::Written).unwrap();
            assert_eq!(actor(&store, &context), case["winner"]["afterActor"]);
            assert_eq!(stored(&store, id), case["pythonAfter"]["after"]);
            assert_eq!(case["pythonAfter"]["afterActor"]["status"], "running");
        }
    }
}
