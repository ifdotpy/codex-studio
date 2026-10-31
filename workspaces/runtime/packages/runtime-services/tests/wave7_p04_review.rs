//! Review regressions: lifecycle races and actual queue/receipt restart boundaries.
#![allow(clippy::unwrap_used)]
use crate::callback_fixtures as fixture;
use crate::wave7_p04_callback_runtime::{callback_body, callback_owner};
use fixture::*;
use futures_util::{FutureExt, poll};
use serde_json::json;
use std::sync::{Arc, mpsc};
use studio_runtime_providers::{Frame, RequestId};
use studio_runtime_schema::persistence::{OutboxStatus, ToolRequestStatus};
use studio_runtime_services::{callback_runtime::*, provider_callbacks::*};
use studio_runtime_store::{RuntimeStore, outbox::Outbox};

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
            .test_reader()
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
        assert!(prepared_replies(&store, &context).await.unwrap().is_empty());
        if let Some(id) = id {
            assert_eq!(stored(&store, &id)["status"], "deleted");
        } else {
            assert_eq!(
                store
                    .test_reader()
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
async fn python_admitted_resolution_keeps_its_signature_and_completed_origin_fence() {
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
    seed_admission(&store, &legacy).await;

    resolve(&store, &context, &RequestId::Number(42), 20.0)
        .await
        .unwrap();
    assert!(matches!(
        dispatch(Arc::clone(&store), &context, &frame, 21.0)
            .await
            .unwrap(),
        CallbackDecision::Settled(_)
    ));
    let owner = callback_owner(&store, &context, &RequestId::Number(42));
    assert_eq!(owner.status, ToolRequestStatus::Completed);
    assert!(
        Outbox::get::<CallbackResponse>(&store.test_reader().unwrap(), &owner.origin_key)
            .unwrap()
            .is_none()
    );
    assert!(callback_body(&store, &owner).is_none());
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
    let runtime = CallbackRuntime::new(store.clone(), Arc::new(Port::default()));
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let submit = runtime.submit_committed(&reply, || async {
        delivery(&store, &reply.identity, DeliveryOutcome::Submitted).await?;
        // The store transaction ends, but transport acceptance still holds the lifecycle gate.
        store
            .transaction_with::<_, CallbackError>(|_| Ok(()))
            .await?;
        entered_tx.send(()).unwrap();
        release_rx.await.unwrap();
        Ok(())
    });
    tokio::pin!(submit);
    tokio::select! {
        biased;
        _ = entered_rx => {},
        result = &mut submit => panic!("submission completed before release: {result:?}"),
    }
    assert!(
        runtime
            .with_lifecycle(|| async { Ok(()) })
            .now_or_never()
            .is_none()
    );
    let rpc = RequestId::Number(42);
    let resolve = runtime.resolve(&context, &rpc, 21.0);
    tokio::pin!(resolve);
    assert!(poll!(&mut resolve).is_pending());
    release_tx.send(()).unwrap();
    let (submitted, resolved) = tokio::join!(submit, resolve);
    assert_eq!(submitted.unwrap(), Some(()));
    assert_eq!(resolved.unwrap(), 1);
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
        let mut actual = actor(&store, &context);
        crate::normalize_agent_nulls(&mut actual);
        crate::normalize_agent_nulls(&mut value);
        assert_eq!(actual, value, "{status}");
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
#[ignore = "heavy tier: just test-quarantine (flaky under load, issue #616)"]
async fn answer_and_cancellation_restart_at_each_real_store_queue_and_transport_boundary() {
    // 0 before admission, 1 admission, 2 answer/delete intent, 3 Submitted,
    // 4 mailbox acceptance, 5 native submission, 6 typed receipt received,
    // 7 Written, exact result, origin fingerprint and body retirement commit atomically.
    for cancel in [false, true] {
        for boundary in 0..=7 {
            let (root, mut store, context) = fixture().await;
            let frame = request(&context);
            let (native_tx, native_rx) = mpsc::channel();
            let mut id = None;
            if boundary >= 1 {
                let accepted = dispatch(Arc::clone(&store), &context, &frame, 10.0)
                    .await
                    .unwrap();
                let CallbackDecision::Pending(card_id) = &accepted else {
                    panic!("pending")
                };
                id = Some(card_id.clone());
            }
            if boundary >= 2 {
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
                if (3..5).contains(&boundary) {
                    delivery(&store, &reply.identity, DeliveryOutcome::Submitted)
                        .await
                        .unwrap();
                }
                if boundary >= 4 {
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
                    if boundary >= 5 {
                        runtime
                            .submit_committed(&queued.reply, || async {
                                delivery(&store, &reply.identity, DeliveryOutcome::Submitted)
                                    .await?;
                                native_tx.send(queued.reply.frame()).unwrap();
                                Ok(())
                            })
                            .await
                            .unwrap()
                            .unwrap();
                    }
                    if boundary >= 6 {
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
                    if boundary >= 6 {
                        assert_eq!(receipt.unwrap().outcome, ReplyOutcome::Written);
                    } else {
                        assert!(receipt.is_err());
                    }
                }
                if boundary >= 7 {
                    delivery(&store, &reply.identity, DeliveryOutcome::Written)
                        .await
                        .unwrap();
                }
            }
            // Every branch stops before the next actual boundary; Written has no separate cleanup phase.
            drop(store);
            store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
            let port = Arc::new(Port::default());
            let runtime = CallbackRuntime::new(store.clone(), port.clone());
            if boundary < 1 {
                let accepted = runtime.dispatch(&context, &frame, 11.0).await.unwrap();
                let CallbackDecision::Pending(card_id) = &accepted else {
                    panic!("pending")
                };
                id = Some(card_id.clone());
            }
            let id = id.unwrap();
            if boundary < 2 {
                if cancel {
                    runtime.delete_question(&id, 20.0).await.unwrap();
                } else {
                    runtime
                        .answer(&id, &json!({"answers":{}}), 20.0)
                        .await
                        .unwrap();
                }
            } else if boundary == 2 {
                let prepared = prepared_replies(&store, &context).await.unwrap();
                assert_eq!(prepared.len(), 1);
                runtime.deliver(&prepared[0]).await.unwrap();
            } else if boundary < 7 {
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
            }
            let owner = callback_owner(&store, &context, &RequestId::Number(42));
            let response =
                Outbox::get::<CallbackResponse>(&store.test_reader().unwrap(), &owner.origin_key)
                    .unwrap();
            if (3..7).contains(&boundary) {
                assert_eq!(owner.status, ToolRequestStatus::Unknown);
                let response = response.unwrap();
                assert_eq!(response.status, OutboxStatus::Unknown);
                assert_eq!(response.next_at, None);
            } else {
                assert_eq!(owner.status, ToolRequestStatus::Completed);
                assert!(response.is_none());
                assert!(owner.payload.frame_fingerprint.is_some());
                assert!(callback_body(&store, &owner).is_some());
            }
            let native: Vec<_> = native_rx.try_iter().collect();
            assert_eq!(native.len(), usize::from(boundary >= 5));
            assert_eq!(port.calls.lock().unwrap().len(), usize::from(boundary < 3));
            assert!(
                native.len() + port.calls.lock().unwrap().len() <= 1,
                "{cancel} {boundary}"
            );
        }
    }
}

#[tokio::test]
async fn python_failures_null_decisions_and_race_winners_are_preserved() {
    fn expected_actor(case: &serde_json::Value) -> serde_json::Value {
        normalized_actor(&case["afterActor"])
    }
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
        assert_eq!(actor(&store, &context), expected_actor(case));
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
        assert_eq!(actor(&store, &context), expected_actor(case));
        assert_writes(&before, &store, &case["writes"]);
    }
    for case in template()["races"].as_array().unwrap() {
        let (_root, store, context) = fixture().await;
        card(&store, &case["before"]).await;
        seed_admission(&store, &case["before"]).await;
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
            assert_eq!(actor(&store, &context), expected_actor(&case["winner"]));
            assert_eq!(stored(&store, id), case["pythonAfter"]["after"]);
            assert_eq!(case["pythonAfter"]["afterActor"]["status"], "running");
        }
    }
}

#[tokio::test]
async fn deferring_a_question_publishes_the_request_entity() {
    let (_root, store, context) = fixture().await;
    let (id, _) = pending(&store, &context).await;
    let entity_payload = |store: Arc<RuntimeStore>, id: String| async move {
        store
            .read_async::<String, studio_runtime_store::StoreError>("test entity", move |tx| {
                Ok(tx.query_row(
                    "SELECT payload FROM sync_entities WHERE collection='request' AND id=?1",
                    [&id],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("request entity")
    };
    assert!(
        !entity_payload(Arc::clone(&store), id.clone())
            .await
            .contains("\"deferred\":true")
    );
    let request = id.clone();
    store
        .transaction_with_error(move |tx| {
            studio_runtime_services::questions_write::defer_question(tx, &request, true)
        })
        .await
        .expect("defer the question");
    assert!(
        entity_payload(Arc::clone(&store), id)
            .await
            .contains("\"deferred\":true")
    );
}

#[tokio::test]
async fn non_radio_async_answer_is_steered_and_acknowledged() {
    use studio_runtime_services::{
        coordination::CoordinationError,
        host_project_runtime::PreparedHost,
        message_admission::MessageAdmission,
        questions_write::{AsyncQuestionAnswerInput, answer_async_question},
    };
    use studio_runtime_store::{
        AgentId, AgentRecord, AgentRepository, SchedulerRepository, Transaction,
    };
    struct Allow;
    impl MessageAdmission for Allow {
        fn workspace(
            &self,
            _: &Transaction<'_>,
            _: &AgentRecord,
        ) -> Result<Vec<serde_json::Value>, CoordinationError> {
            Ok(vec![])
        }
    }
    let (_root, store, context) = fixture().await;
    let agent = context.agent.clone();
    let agent_for_setup = agent.clone();
    store
        .transaction(move |tx| {
            tx.execute(
                "DELETE FROM runtime_events WHERE agent=?1",
                [&agent_for_setup],
            )?;
            let mut record =
                AgentRepository::get(tx, &AgentId::from(agent_for_setup.as_str()))?.unwrap();
            record.status = studio_runtime_store::AgentStatus::Running;
            record.in_flight = Some(true);
            record.auto_wake = Some(true);
            record.thread_id = Some("thread-0".to_string());
            record.turn_id = Some("turn-0".to_string());
            record.is_lead = Some(true);
            record.start_attempt = Some(Box::new(studio_runtime_store::StartAttemptView {
                id: Some("start".into()),
                epoch: Some(3),
                submitted: Some(true),
                turn_id: Some("turn-0".into()),
                thread_id: Some("thread-0".into()),
                account_key: Some("default".into()),
                events: Some(vec!["initial".into()]),
                connection_generation: Some(7),
                ..Default::default()
            }));
            AgentRepository::put(tx, &record)?;
            Ok(())
        })
        .await
        .unwrap();
    card(&store, &json!({"id":"question","agent":agent,"epoch":3,"turnId":"turn-0","method":"agent/asyncQuestion","status":"pending","createdAt":1.0,"params":{"questions":[{"id":"q","question":"Choose"},{"id":"secret","question":"Token","isSecret":true},{"id":"legacy","question":"Optional"}]}})).await;
    let prepared = PreparedHost::load(&store).await.unwrap();
    let agent_for_write = agent.clone();
    store
        .transaction_with_error(move |tx| {
            let data = json!({"answers":{"q":{"answers":["yes"]},"secret":{"answers":["private"]},"legacy":{"answers":null}},"decision":null});
            let input = |data, now| AsyncQuestionAnswerInput {
                id: "question", data, admission: &Allow, prepared: &prepared, now,
            };
            let mut malformed = data.clone();
            malformed["answers"]["q"]["answers"] = json!(["yes", 7]);
            assert!(answer_async_question(tx, &input(&malformed, 29.0)).is_err());
            answer_async_question(
                tx,
                &AsyncQuestionAnswerInput {
                    id: "question",
                    data: &data,
                    admission: &Allow,
                    prepared: &prepared,
                    now: 30.0,
                    },
            )?;
            assert!(!answer_async_question(tx, &input(&data, 30.5))?.event_inserted);
            let mut changed = data.clone();
            changed.as_object_mut().unwrap().remove("decision");
            assert!(answer_async_question(tx, &input(&changed, 30.6)).is_err(), "null and omission keep distinct raw signatures");
            let record: String = tx.query_row("SELECT record FROM runtime_requests WHERE id='question'", [], |row| row.get(0)).map_err(studio_runtime_store::StoreError::from)?;
            let record: serde_json::Value = serde_json::from_str(&record)?;
            assert_eq!(record["answerHistory"][0]["answer"], json!(["yes"]));
            assert_eq!(record["answerHistory"][1]["answer"], json!("[redacted]"));
            assert_eq!(record["answerHistory"][2]["answer"], json!([]));
            let claim = SchedulerRepository::claim_steer(tx, &agent_for_write, 3, "steer", 31.0)?
                .expect("answer must be eligible for the running turn");
            assert_eq!(claim.events[0].text, "Choose\nyes\nToken\nprivate\nOptional");
            assert_eq!(
                claim
                    .events
                    .iter()
                    .map(|e| e.id.0.as_str())
                    .collect::<Vec<_>>(),
                ["question:answer"]
            );
            assert!(SchedulerRepository::steer_submitted(
                tx,
                &agent_for_write,
                "steer",
                "ticket",
                7
            )?);
            assert!(
                SchedulerRepository::acknowledge_steer(
                    tx,
                    &agent_for_write,
                    "question:answer",
                    "thread-0",
                    "turn-0"
                )?
                .is_some()
            );
            let (status, turn): (String, Option<String>) = tx
                .query_row(
                    "SELECT status,turn_id FROM runtime_events WHERE id='question:answer'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(studio_runtime_store::StoreError::from)?;
            assert_eq!(
                (status.as_str(), turn.as_deref()),
                ("delivered", Some("turn-0"))
            );
            Ok::<_, studio_runtime_services::questions_write::QuestionsWriteError>(())
        })
        .await
        .unwrap();
}
