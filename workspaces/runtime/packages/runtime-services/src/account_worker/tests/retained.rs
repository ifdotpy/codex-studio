use super::*;
use crate::account_session_runtime::{
    AccountSessionError, AccountSessionPort, ProviderRequest, ProviderResponse, RequestCertainty,
    SessionReceipt,
};
use crate::account_worker::retained::{
    PortError, RequestActor, RequestResult, RetainedSessionPort, SessionRequest,
};
use crate::{
    recovery::{RecoveryAction, RecoveryOutcome},
    recovery_history::ConnectionScope,
    recovery_policy::RecoverySnapshot,
    recovery_store::CompletionWriter,
};
use std::sync::{Arc, Mutex};

struct ResetBrokerSession {
    port: RetainedSessionPort,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    lose_response: Arc<tokio::sync::Notify>,
}

impl AccountSessionPort for ResetBrokerSession {
    fn ensure_session<'a>(
        &'a self,
        account: &'a str,
    ) -> crate::account_session_runtime::AccountSessionFuture<'a, ConnectionScope> {
        Box::pin(async move {
            (self.port.scope().account == account)
                .then(|| self.port.scope().clone())
                .ok_or(AccountSessionError::Unknown)
        })
    }

    fn read_limits<'a>(
        &'a self,
        account: &'a str,
    ) -> crate::account_session_runtime::AccountSessionFuture<'a, Value> {
        Box::pin(async move {
            if account != "default" {
                return Err(AccountSessionError::Unknown);
            }
            Ok(json!({
                "accountKey":"default",
                "data":{"accountId":"account-A","rateLimitResetCredits":{"credits":[{
                    "id":"credit-A","status":"available","resetType":"codexRateLimits"
                }]}}
            }))
        })
    }

    fn provider_request(
        &self,
        request: ProviderRequest,
    ) -> crate::account_session_runtime::AccountSessionFuture<'_, SessionReceipt> {
        Box::pin(async move {
            self.requests
                .lock()
                .unwrap()
                .push((request.operation.clone(), request.params.clone()));
            let call = SessionRequest {
                scope: self.port.scope().clone(),
                expected_scope: request.expected_scope,
                operation: request.operation,
                actor: None,
                transfer: None,
                deleted_actor_session: None,
                method: request.method,
                params: request.params,
                deadline: request.deadline,
            };
            let lose_response = self.lose_response.clone();
            let receipt = tokio::select! {
                result = self.port.request(call) => result,
                _ = lose_response.notified() => Err(PortError::Unknown),
            }
            .map_err(|error| match error {
                PortError::Unknown => AccountSessionError::Unknown,
                _ => AccountSessionError::Refused("Retained reset request was refused"),
            })?;
            let (certainty, result) = match receipt.result {
                RequestResult::Reply(value) => (
                    RequestCertainty::Written,
                    Some(ProviderResponse::Reply(value)),
                ),
                RequestResult::Rejected(value) => (
                    RequestCertainty::Written,
                    Some(ProviderResponse::Rejected(value)),
                ),
                RequestResult::NotSent => (RequestCertainty::NotSent, None),
                RequestResult::Unknown => return Err(AccountSessionError::Unknown),
            };
            Ok(SessionReceipt {
                scope: receipt.scope,
                operation: receipt.operation,
                ticket: Some(receipt.ticket),
                certainty,
                result,
                not_sent_reason: None,
                replayed: receipt.replayed,
            })
        })
    }
}

fn request(state: &State, port: &RetainedSessionPort, operation: &str) -> SessionRequest {
    SessionRequest {
        scope: port.scope().clone(),
        expected_scope: None,
        operation: operation.into(),
        actor: Some(RequestActor {
            agent: state.agent.clone(),
            epoch: state.epoch,
        }),
        transfer: None,
        deleted_actor_session: None,
        method: Method::ThreadRead,
        params: json!({"threadId":"fake-thread-0","includeTurns":false}),
        deadline: (tokio::time::Instant::now() + std::time::Duration::from_secs(60)).into_std(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_service_recovers_lost_response_through_retained_worker_once() {
    let broker_state = tempfile::tempdir().unwrap();
    let broker_root = broker_state.path().to_path_buf();
    let broker_store = studio_runtime_store::RuntimeStore::open(Some(&broker_root))
        .await
        .unwrap();
    let bridge = test_bridge(Arc::new(broker_store)).await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(bridge).await;
    let port = worker.retained_session();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let lose_response = Arc::new(tokio::sync::Notify::new());
    let session = Arc::new(ResetBrokerSession {
        port: port.clone(),
        requests: requests.clone(),
        lose_response: lose_response.clone(),
    });
    let request_id = "11000000-0000-4000-8000-000000000020";
    let body = json!({
        "account_key":"default",
        "account_id":"account-A",
        "credit_id":"credit-A",
        "request_id":request_id
    });

    // Keep the reset record store separate from the worker fixture's runtime
    // lock, then reopen it below to model the service process restarting.
    let reset_state = tempfile::tempdir().unwrap();
    let reset_root = reset_state.path().to_path_buf();
    let reset_store = Arc::new(
        studio_runtime_store::RuntimeStore::open(Some(&reset_root))
            .await
            .unwrap(),
    );
    let first_session = session.clone();
    let first_body = body.clone();
    let first_store = reset_store.clone();
    let mut first = tokio::spawn(async move {
        crate::limit_reset::consume_reset(&first_store, first_session.as_ref(), &first_body).await
    });
    let first_wire = tokio::select! {
        wire = peer.writes.recv() => wire.expect("retained worker keeps provider transport open"),
        result = worker.completion() => panic!("retained worker exited before reset dispatch: {result:?}"),
        result = &mut first => panic!("reset service ended before retained dispatch: {result:?}"),
    };
    assert_eq!(
        first_wire.value["method"],
        "account/rateLimitResetCredit/consume"
    );
    let charge_key = first_wire.value["params"]["idempotencyKey"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut charges = std::collections::HashSet::new();
    assert!(charges.insert(charge_key.clone()));
    // The fake provider has charged the key and confirms the write, but its
    // response is lost before the reset service can observe it.
    assert!(
        first_wire
            .receipt
            .send(CommandReceipt::Written {
                remote_id: None,
                duplicate: false,
            })
            .is_ok()
    );
    lose_response.notify_one();
    let uncertain = first.await.unwrap().unwrap();
    assert_eq!(uncertain["outcome"], "uncertain");
    drop(reset_store);

    // Opening the durable service store again models recovery after a process restart.
    let reset_store = Arc::new(
        studio_runtime_store::RuntimeStore::open(Some(&reset_root))
            .await
            .unwrap(),
    );
    let retry_session = session.clone();
    let retry_body = body.clone();
    let retry = tokio::spawn(async move {
        crate::limit_reset::consume_reset(&reset_store, retry_session.as_ref(), &retry_body)
            .await
            .unwrap()
    });
    let retry_wire = peer.next_write().await;
    assert_eq!(
        retry_wire.value["method"],
        "account/rateLimitResetCredit/consume"
    );
    assert_eq!(
        retry_wire.value["params"]["idempotencyKey"], charge_key,
        "the provider charge identity survives restart"
    );
    let new_charge = charges.insert(
        retry_wire.value["params"]["idempotencyKey"]
            .as_str()
            .unwrap()
            .to_owned(),
    );
    assert!(
        !new_charge,
        "the fake provider sees a replay of the same charge key"
    );
    assert!(
        retry_wire
            .receipt
            .send(CommandReceipt::Written {
                remote_id: None,
                duplicate: false,
            })
            .is_ok()
    );
    peer.reply(&retry_wire.value, json!({"outcome":"alreadyRedeemed"}))
        .await;
    let recovered = retry.await.unwrap();
    assert_eq!(recovered["outcome"], "alreadyRedeemed");
    assert_eq!(charges.len(), 1, "exactly one fake provider charge");

    let recorded = requests.lock().unwrap().clone();
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[0].1, recorded[1].1);
    assert_eq!(recorded[0].0, format!("limit-reset:{charge_key}:1"));
    assert_eq!(recorded[1].0, format!("limit-reset:{charge_key}:2"));

    // The first broker operation remains Unknown. Recovery used a fresh transport
    // identity, and replaying or rebinding either identity cannot send another write.
    let old = SessionRequest {
        scope: port.scope().clone(),
        expected_scope: None,
        operation: recorded[0].0.clone(),
        actor: None,
        transfer: None,
        deleted_actor_session: None,
        method: Method::AccountRateLimitResetCreditConsume,
        params: recorded[0].1.clone(),
        deadline: (tokio::time::Instant::now() + std::time::Duration::from_secs(60)).into_std(),
    };
    assert_eq!(
        port.request(old.clone()).await.unwrap().result,
        RequestResult::Unknown
    );
    let mut changed_key = old.clone();
    changed_key.params["idempotencyKey"] = json!("11000000-0000-4000-8000-000000000021");
    assert_eq!(
        port.request(changed_key).await.unwrap_err(),
        PortError::Invalid
    );
    let mut changed_body = SessionRequest {
        operation: recorded[1].0.clone(),
        params: recorded[1].1.clone(),
        ..old
    };
    changed_body.params["creditId"] = json!("credit-B");
    assert_eq!(
        port.request(changed_body).await.unwrap_err(),
        PortError::Identity
    );
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

async fn recovered_publications(state: &State) -> Vec<(String, String)> {
    state
        .bridge
        .run(|store| async move {
            let db = store.read()?;
            let mut statement = db.prepare(
                "SELECT json_extract(result,'$.effect.Recovered.identity'), json_extract(result,'$.effect.Recovered.observation.method') FROM runtime_operation_receipts WHERE id LIKE 'account-worker:notification-publication:default:%:pending' AND json_extract(result,'$.effect.Recovered.identity') IS NOT NULL ORDER BY id",
            )?;
            let rows = statement
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .expect("read recovered publication receipts")
}

#[derive(Default)]
struct RecoveredPublicationRecorder(Mutex<Vec<(String, String)>>);

impl crate::notification_runtime_effects::NotificationPostCommitSink
    for RecoveredPublicationRecorder
{
    fn publish<'a>(
        &'a self,
        effect: &'a crate::notification_dispatch::NotificationPostCommit,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<(), crate::notification_dispatch::NotificationDispatchError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            match effect {
                crate::notification_dispatch::NotificationPostCommit::Recovered {
                    identity,
                    observation,
                } => {
                    self.0
                        .lock()
                        .expect("recorded recovered publications")
                        .push((identity.clone(), observation.method.clone()));
                    Ok(())
                }
                _ => Err(
                    crate::notification_dispatch::NotificationDispatchError::InvalidNotification,
                ),
            }
        })
    }
}

#[tokio::test]
async fn retained_reply_replays_without_resending_and_rejects_changed_payload() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let call = request(&state, &port, "history-1");
    let sender = port.clone();
    let first = call.clone();
    let task = tokio::spawn(async move { sender.request(first).await });
    let wire = peer.write("thread/read").await;
    peer.reply(&wire, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    let receipt = task.await.unwrap().unwrap();
    assert_eq!(
        receipt.result,
        RequestResult::Reply(json!({"thread":{"id":"fake-thread-0"}}))
    );
    assert_eq!(
        port.request(call.clone()).await.unwrap().result,
        receipt.result
    );
    let mut altered = call;
    altered.params["limit"] = json!(2);
    assert_eq!(
        port.request(altered).await.unwrap_err(),
        PortError::Identity
    );
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_retained_operation_conflict_does_not_kill_worker() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let mut first = request(&state, &port, "import:fixture:read");
    first.actor = None;
    let mut conflicting = first.clone();
    conflicting.params["threadId"] = json!("other-source-thread");

    // Make both valid callers compete for the same durable operation identity.
    let start = Arc::new(tokio::sync::Barrier::new(3));
    let first_start = Arc::clone(&start);
    let first_port = port.clone();
    let first_task = tokio::spawn(async move {
        first_start.wait().await;
        first_port.request(first).await
    });
    let conflicting_start = Arc::clone(&start);
    let conflicting_port = port.clone();
    let conflicting_task = tokio::spawn(async move {
        conflicting_start.wait().await;
        conflicting_port.request(conflicting).await
    });
    start.wait().await;

    let write = peer.write("thread/read").await;
    let source_thread = write["params"]["threadId"].as_str().unwrap();
    peer.reply(&write, json!({"thread":{"id":source_thread}}))
        .await;
    let outcomes = [first_task.await.unwrap(), conflicting_task.await.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Ok(receipt) if matches!(receipt.result, RequestResult::Reply(_))))
            .count(),
        1,
        "one source identity must be accepted"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Err(PortError::Identity)))
            .count(),
        1,
        "the conflicting source identity must be refused"
    );
    assert!(
        peer.writes.try_recv().is_err(),
        "the refused call is never sent"
    );

    // The identity conflict belongs to one caller; the account owner remains
    // available to service another retained request on the same session.
    let health_port = port.clone();
    let health_request = request(&state, &port, "post-conflict-health-check");
    let health = tokio::spawn(async move { health_port.request(health_request).await });
    let health_write = peer.write("thread/read").await;
    peer.reply(&health_write, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    assert!(matches!(
        health.await.unwrap().unwrap().result,
        RequestResult::Reply(_)
    ));
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_exact_retained_requests_keep_uncertainty_and_replay_once() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let call = request(&state, &port, "import:fixture:exact-read");
    let start = Arc::new(tokio::sync::Barrier::new(3));
    let first_start = Arc::clone(&start);
    let first_port = port.clone();
    let first_call = call.clone();
    let first_task = tokio::spawn(async move {
        first_start.wait().await;
        first_port.request(first_call).await
    });
    let retry_start = Arc::clone(&start);
    let retry_port = port.clone();
    let retry_call = call.clone();
    let retry_task = tokio::spawn(async move {
        retry_start.wait().await;
        retry_port.request(retry_call).await
    });
    start.wait().await;

    let write = peer.write("thread/read").await;
    peer.reply(&write, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    let outcomes = [first_task.await.unwrap(), retry_task.await.unwrap()];
    assert!(
        outcomes.iter().any(
            |outcome| matches!(outcome, Ok(receipt) if matches!(receipt.result, RequestResult::Reply(_)))
        ),
        "the submitted operation must eventually return its provider result"
    );
    assert!(
        outcomes.iter().all(|outcome| match outcome {
            Ok(receipt) => matches!(
                receipt.result,
                RequestResult::Reply(_) | RequestResult::Unknown
            ),
            Err(PortError::Unknown) => true,
            _ => false,
        }),
        "an exact concurrent retry may remain unknown, but must not be refused as a changed identity"
    );
    assert!(
        peer.writes.try_recv().is_err(),
        "concurrent exact retries issue only one provider write"
    );

    let replay = port.request(call).await.unwrap();
    assert_eq!(
        replay.result,
        RequestResult::Reply(json!({"thread":{"id":"fake-thread-0"}}))
    );
    assert!(replay.replayed);
    assert!(
        peer.writes.try_recv().is_err(),
        "replay never resends the write"
    );
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn retained_checkpointed_not_sent_retries_with_the_same_ticket_once() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let call = request(&state, &port, "voice:delete-stop:session-1");

    let sender = port.clone();
    let first_call = call.clone();
    let first = tokio::spawn(async move { sender.request(first_call).await });
    let first_wire = peer.next_write().await;
    assert!(first_wire.receipt.send(CommandReceipt::NotSent).is_ok());
    assert_eq!(first.await.unwrap().unwrap().result, RequestResult::NotSent);

    let sender = port.clone();
    let retry_call = call.clone();
    let retry = tokio::spawn(async move { sender.request(retry_call).await });
    let retry_wire = peer.next_write().await;
    assert_eq!(retry_wire.value["id"], first_wire.value["id"]);
    assert!(
        retry_wire
            .receipt
            .send(CommandReceipt::Written {
                remote_id: None,
                duplicate: false,
            })
            .is_ok()
    );
    peer.reply(&retry_wire.value, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    assert_eq!(
        retry.await.unwrap().unwrap().result,
        RequestResult::Reply(json!({"thread":{"id":"fake-thread-0"}}))
    );
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn retained_retry_lost_response_replays_unknown_without_another_send() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let call = request(&state, &port, "retry-lost-response");

    let sender = port.clone();
    let first_call = call.clone();
    let first = tokio::spawn(async move { sender.request(first_call).await });
    let first_wire = peer.next_write().await;
    assert!(first_wire.receipt.send(CommandReceipt::NotSent).is_ok());
    assert_eq!(first.await.unwrap().unwrap().result, RequestResult::NotSent);

    let sender = port.clone();
    let retry_call = call.clone();
    let retry = tokio::spawn(async move { sender.request(retry_call).await });
    let retry_wire = peer.next_write().await;
    assert!(
        retry_wire
            .receipt
            .send(CommandReceipt::Written {
                remote_id: None,
                duplicate: false,
            })
            .is_ok()
    );
    retry.abort();
    let _ignored = retry.await;
    assert_eq!(
        port.request(call).await.unwrap().result,
        RequestResult::Unknown
    );
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn retained_uncheckpointed_not_sent_is_unknown_and_not_retried() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let call = request(&state, &port, "uncheckpointed-not-sent");
    let sender = port.clone();
    let first_call = call.clone();
    let first = tokio::spawn(async move { sender.request(first_call).await });
    let first_wire = peer.next_write().await;
    assert!(first_wire.receipt.send(CommandReceipt::NotSent).is_ok());
    assert_eq!(first.await.unwrap().unwrap().result, RequestResult::NotSent);

    state.bridge.run(|store| async move {
        store.transaction(move |tx| {
            tx.execute(
                "UPDATE runtime_operation_receipts SET result=json_set(result,'$.cursor',1) WHERE id LIKE 'account-worker:retained:default:%:result' AND json_extract(result,'$.result')='NotSent'",
                [],
            )?;
            Ok(())
        }).await?;
        Ok(())
    }).await.unwrap();
    assert_eq!(
        port.request(call).await.unwrap().result,
        RequestResult::Unknown
    );
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn retained_retry_intent_crash_before_transport_replays_unknown_without_sending() {
    use crate::account_worker::retained::Call;

    let state = State::new().await;
    let mut first_peer = Peer::new();
    let mut first_worker = first_peer.worker(state.bridge.clone()).await;
    let first_port = first_worker.retained_session();
    let call = request(&state, &first_port, "crash-before-retry-transport");
    let sender = first_port.clone();
    let first_call = call.clone();
    let first = tokio::spawn(async move { sender.request(first_call).await });
    let first_wire = first_peer.next_write().await;
    assert!(first_wire.receipt.send(CommandReceipt::NotSent).is_ok());
    assert_eq!(first.await.unwrap().unwrap().result, RequestResult::NotSent);
    first_worker.shutdown().await.unwrap();

    let key = format!(
        "retained:default:{}",
        studio_runtime_store::sha256_hex(call.operation.as_bytes())
    );
    let (stored_call, retry_request) = state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let prior = crate::account_worker::store::receipt(tx, &key, "submitted")?
                        .ok_or_else(|| crate::account_worker::WorkerError::Capacity)?;
                    let call: Call = serde_json::from_value(prior["call"].clone())
                        .map_err(|_| crate::account_worker::WorkerError::Capacity)?;
                    let request = crate::account_worker::store::decode(&prior["request"])?;
                    Ok((call, request))
                })
                .await
        })
        .await
        .unwrap();

    // This is the retry's atomic clear-and-submit commit. Simulate a process
    // crash immediately after commit and before the transport receives it.
    crate::account_worker::retained::test_submitted(
        &state.bridge.store,
        &stored_call,
        &retry_request,
        Some(&retry_request.ticket),
    )
    .await
    .unwrap()
    .unwrap();

    let mut second_peer = Peer::identity("default", 2, None);
    let mut second_worker = second_peer.worker(state.bridge.clone()).await;
    let second_port = second_worker.retained_session();
    assert_eq!(
        second_port
            .request(request(&state, &second_port, &call.operation))
            .await
            .unwrap()
            .result,
        RequestResult::Unknown
    );
    assert!(second_peer.writes.try_recv().is_err());
    second_worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn retained_written_receipt_replays_after_reconnect_without_sending() {
    let state = State::new().await;
    let mut first_peer = Peer::identity("default", 1, None);
    let mut first_worker = first_peer.worker(state.bridge.clone()).await;
    let first_port = first_worker.retained_session();
    let original = request(&state, &first_port, "branch:fixture:fork");
    let sender = first_port.clone();
    let pending = original.clone();
    let task = tokio::spawn(async move { sender.request(pending).await });
    let wire = first_peer.write("thread/read").await;
    first_peer
        .reply(&wire, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    let original_receipt = task.await.unwrap().unwrap();
    assert_eq!(
        original_receipt.result,
        RequestResult::Reply(json!({"thread":{"id":"fake-thread-0"}}))
    );
    first_worker.shutdown().await.unwrap();

    let mut second_peer = Peer::identity("default", 2, None);
    let mut second_worker = second_peer.worker(state.bridge.clone()).await;
    let second_port = second_worker.retained_session();
    // Replay the original scope to prove the replacement worker consults the
    // durable operation instead of fabricating a connection identity.
    let replay = original.clone();
    let receipt = second_port.request(replay.clone()).await.unwrap();
    assert_eq!(
        receipt.result,
        RequestResult::Reply(json!({"thread":{"id":"fake-thread-0"}}))
    );
    assert_eq!(
        receipt.scope.generation, 1,
        "replay returns the saved scope"
    );
    assert_eq!(receipt.scope, original_receipt.scope);
    assert_eq!(receipt.ticket, original_receipt.ticket);
    assert!(
        second_peer.writes.try_recv().is_err(),
        "saved receipt is never sent again"
    );

    let mut changed_method = replay.clone();
    changed_method.method = Method::ThreadTurnsList;
    changed_method.params = json!({
        "threadId":"fake-thread-0","limit":10,"sortDirection":"desc","itemsView":"full"
    });
    assert_eq!(
        second_port.request(changed_method).await.unwrap_err(),
        PortError::Identity
    );

    let mut changed_params = replay.clone();
    changed_params.params["threadId"] = json!("other-thread");
    assert_eq!(
        second_port.request(changed_params).await.unwrap_err(),
        PortError::Identity
    );

    assert!(
        second_peer.writes.try_recv().is_err(),
        "mismatches never reach provider"
    );
    second_worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn reconnect_churn_keeps_checkpoints_bounded_by_retained_generations() {
    let state = State::new().await;
    for generation in 1..=8 {
        let mut peer = Peer::identity("default", generation, None);
        let mut worker = peer.worker(state.bridge.clone()).await;
        let port = worker.retained_session();
        let operation = format!("retained-generation-{generation}");
        let pending = request(&state, &port, &operation);
        let sender = port.clone();
        let task = tokio::spawn(async move { sender.request(pending).await });
        let wire = peer.write("thread/read").await;
        peer.reply(&wire, json!({"thread":{"id":"fake-thread-0"}}))
            .await;
        let receipt = task.await.unwrap().unwrap();
        assert_eq!(receipt.scope.generation, generation);
        worker.shutdown().await.unwrap();

        let (retained_generations, checkpoints) = state.bridge.run(|store| async move {
            let db = store.read()?;
            let retained_generations = db.query_row(
                "SELECT COUNT(DISTINCT json_extract(result,'$.call.scope.generation')) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:retained:default:%:submitted'",
                [],
                |row| row.get::<_, i64>(0),
            )?;
            let checkpoints = db.query_row(
                "SELECT COUNT(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:cursor:default:%:ack'",
                [],
                |row| row.get::<_, i64>(0),
            )?;
            Ok((retained_generations, checkpoints))
        }).await.unwrap();
        assert_eq!(retained_generations, generation as i64);
        assert!(
            checkpoints <= retained_generations,
            "{checkpoints} cursor checkpoints exceeded {retained_generations} retained generations"
        );
    }
}

#[tokio::test]
async fn retained_unknown_receipt_stays_unknown_after_reconnect_without_sending() {
    let state = State::new().await;
    let mut first_peer = Peer::identity("default", 1, None);
    let mut first_worker = first_peer.worker(state.bridge.clone()).await;
    let first_port = first_worker.retained_session();
    let original = request(&state, &first_port, "branch:unknown:fork");
    let sender = first_port.clone();
    let pending = original.clone();
    let task = tokio::spawn(async move { sender.request(pending).await });
    let _wire = first_peer.write("thread/read").await;
    first_worker.shutdown().await.unwrap();
    let _request_result = task.await;

    let mut second_peer = Peer::identity("default", 2, None);
    let mut second_worker = second_peer.worker(state.bridge.clone()).await;
    let second_port = second_worker.retained_session();
    let replay = original.clone();
    assert_eq!(
        second_port.request(replay).await.unwrap().result,
        RequestResult::Unknown,
    );
    assert!(
        second_peer.writes.try_recv().is_err(),
        "Unknown is held after reconnect"
    );
    second_worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn crash_before_checkpoint_replays_unknown_without_resending_durable_reply() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let call = request(&state, &port, "crash-before-checkpoint");
    let sender = port.clone();
    let task_call = call.clone();
    let task = tokio::spawn(async move { sender.request(task_call).await });
    let _original_wire = peer.write("thread/read").await;

    // Model a crash after the provider result's journal transaction committed,
    // but before the generation checkpoint could include that cursor.
    let operation = call.operation.clone();
    let key = format!(
        "retained:default:{}",
        studio_runtime_store::sha256_hex(operation.as_bytes())
    );
    let saved_result = serde_json::to_value(RequestResult::Reply(
        json!({"thread":{"id":"fake-thread-0"}}),
    ))
    .unwrap();
    state
        .bridge
        .run(move |store| async move {
            store.transaction_with::<_, StoreError>(move |tx| {
                tx.execute(
                    "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'result',?2)",
                    rusqlite::params![
                        format!("account-worker:{key}:result"),
                        json!({"result":saved_result,"cursor":2}).to_string()
                    ],
                )?;
                tx.execute(
                    "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'settled',?2)",
                    rusqlite::params![
                        format!("account-worker:{}:settled", store::journal_operation("default", 1, 2)),
                        json!({"cursor":2,"digest":"fixture","ready":true,"committed":true}).to_string()
                    ],
                )?;
                Ok(())
            }).await?;
            Ok(())
        })
        .await
        .unwrap();
    worker.shutdown().await.unwrap();
    task.abort();
    let _ignored = task.await;

    let mut replacement_peer = Peer::new();
    let mut replacement = replacement_peer.worker(state.bridge.clone()).await;
    let replacement_port = replacement.retained_session();
    let replayed = replacement_port
        .request(call)
        .await
        .expect("saved retained request is replayed");
    assert_eq!(replayed.result, RequestResult::Unknown);
    assert!(
        replacement_peer.writes.try_recv().is_err(),
        "a beyond-checkpoint result must not cause a second provider send"
    );
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn retained_request_rejects_a_stale_expected_connection_before_dispatch() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let mut call = request(&state, &port, "expected-scope-mismatch");
    let mut expected = port.scope().clone();
    expected.connection = "stale-connection".into();
    call.expected_scope = Some(expected);
    assert_eq!(
        port.request(call).await.unwrap_err(),
        PortError::ScopeMismatch
    );
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn actorless_account_read_only_allows_non_refreshing_voice_shape() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let mut call = request(&state, &port, "voice-account-read");
    call.actor = None;
    call.expected_scope = Some(port.scope().clone());
    call.method = Method::AccountRead;
    call.params = json!({"refreshToken":false});

    let sender = port.clone();
    let task = tokio::spawn(async move { sender.request(call.clone()).await });
    let wire = peer.write("account/read").await;
    peer.reply(&wire, json!({"account":{"id":"fixture"}})).await;
    assert_eq!(
        task.await.unwrap().unwrap().result,
        RequestResult::Reply(json!({"account":{"id":"fixture"}}))
    );

    let mut refresh = request(&state, &port, "voice-account-refresh");
    refresh.actor = None;
    refresh.expected_scope = Some(port.scope().clone());
    refresh.method = Method::AccountRead;
    refresh.params = json!({"refreshToken":true});
    assert_eq!(port.request(refresh).await.unwrap_err(), PortError::Invalid);
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

async fn restart_history_case(outcome: &str) {
    let state = State::new().await;
    let agent_id = state.agent.clone();
    let epoch = state.epoch;
    let fail_terminal_settlement = outcome == "terminal_failure";
    state
        .bridge
        .run(move |store| async move {
            store.transaction_with(move |tx| {
                let mut agent = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                    .expect("seed agent");
                let event = "annotation:input";
                let attempt = json!({
                    "id":"restart-attempt","epoch":epoch,"accountKey":"default",
                    "threadId":"fake-thread-0","events":[event],"submitted":true,
                    "executionOutcome":"unknown","created":1.0,"connectionGeneration":1
                });
                agent.status = studio_runtime_store::AgentStatus::Queued;
                agent.extra.insert("threadId".into(), json!("fake-thread-0"));
                agent.extra.insert("autoWake".into(), json!(true));
                agent.extra.insert("inFlight".into(), json!(false));
                agent.extra.insert("error".into(), json!("Context repair waits for a confirmed input receipt: annotation:input"));
                agent.extra.insert("restartRecovery".into(), json!({"stage":"held","startAttempt":attempt}));
                agent.extra.insert("contextRepairWait".into(), json!({"error":"Context repair waits for a confirmed input receipt: annotation:input","scope":"native","events":[event],"nextCheckAt":0,"source":{"id":agent_id,"epoch":epoch,"accountKey":"default","threadId":"fake-thread-0","attemptId":null}}));
                agent.extra.remove("startAttempt");
                agent.extra.remove("disconnectRecovery");
                AgentRepository::put(tx, &agent)?;
                tx.execute("UPDATE runtime_events SET status='uncertain',error='Server restarted before delivery acknowledgement' WHERE id=?1", [event])?;
                if fail_terminal_settlement {
                    tx.execute(
                        &format!("CREATE TRIGGER fail_restart_transcript BEFORE INSERT ON runtime_items WHEN NEW.agent='{}' BEGIN SELECT RAISE(ABORT,'restart settlement failpoint'); END", agent_id),
                        [],
                    )?;
                }
                Ok::<(), WorkerError>(())
            }).await?;
            Ok(())
        })
        .await
        .expect("seed restart wait");

    let mut peer = Peer::new();
    let publication_attempted = Arc::new(tokio::sync::Notify::new());
    let sink = Arc::new(RejectingPublicationSink {
        attempted: publication_attempted.clone(),
    });
    let mut worker = if outcome == "terminal" {
        peer.open_worker_with_resources_and_sink(
            state.bridge.clone(),
            test_resources().unwrap(),
            sink,
        )
        .await
    } else {
        peer.worker(state.bridge.clone()).await
    };
    let history = peer.write("thread/turns/list").await;
    match outcome {
        "delivered" | "terminal" | "terminal_failure" => {
            let (status, items) = if outcome == "delivered" {
                ("inProgress", json!([]))
            } else {
                (
                    "completed",
                    json!([{"id":"answer","type":"agentMessage","text":"recovered answer"}]),
                )
            };
            peer.reply(
                &history,
                json!({"data":[{"id":"old-turn","threadId":"fake-thread-0","status":status,
                    "items":items,"clientUserMessageId":"annotation:input"}]}),
            )
            .await;
            worker_barrier(&worker).await;
            if outcome == "terminal_failure" {
                assert_eq!(
                    state.record().await["restartRecovery"]["stage"],
                    "held",
                    "failed settlement rolls back to held receipt"
                );
                assert_eq!(state.event_status("input").await, "uncertain");
                let agent = state.agent.clone();
                let item_id = format!("{agent}:answer");
                let transcript = state
                    .bridge
                    .run(move |store| async move {
                        Ok(studio_runtime_store::TranscriptRepository::item(
                            &*store.read()?,
                            &agent,
                            &item_id,
                        )
                        .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?)
                    })
                    .await
                    .unwrap();
                assert!(
                    transcript.is_none(),
                    "failed settlement rolls back transcript writes"
                );
            } else {
                if outcome == "terminal" {
                    assert_eq!(state.record().await["lastCompletedTurn"], "old-turn");
                    assert_eq!(state.event_status("input").await, "delivered");
                    publication_attempted.notified().await;
                    let agent = state.agent.clone();
                    let item_id = format!("{agent}:answer");
                    let item = state
                        .bridge
                        .run(move |store| async move {
                            Ok(studio_runtime_store::TranscriptRepository::item(
                                &*store.read()?,
                                &agent,
                                &item_id,
                            )
                            .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?)
                        })
                        .await
                        .unwrap()
                        .expect("recovered transcript item");
                    assert_eq!(item.record["text"], "recovered answer");
                    assert_eq!(item.record["turnStatus"], "completed");
                    let publications = recovered_publications(&state).await;
                    assert_eq!(publications.len(), 2);
                    assert_eq!(publications[0].1, "item/completed");
                    assert_eq!(publications[1].1, "turn/completed");
                } else {
                    assert_eq!(state.record().await["turnId"], "old-turn");
                }
            }
            assert!(
                peer.writes.try_recv().is_err(),
                "delivered input is never submitted again"
            );
        }
        "not_applied" => {
            peer.reply(
                &history,
                json!({"data":[{
                    "id":"rejected","threadId":"fake-thread-0","status":"failed",
                    "items":[],"clientUserMessageId":"annotation:input","startOutcome":"not_applied"
                }]}),
            )
            .await;
            let thread = peer.write("thread/read").await;
            peer.reply(
                &thread,
                json!({"thread":{"id":"fake-thread-0","status":{"type":"idle"}}}),
            )
            .await;
            let resume = peer.write("thread/resume").await;
            peer.reply(&resume, json!({"thread":{"id":"fake-thread-0"}}))
                .await;
            let start = peer.write("turn/start").await;
            peer.accepted(&start).await;
            assert!(peer.writes.try_recv().is_err());
        }
        "unavailable" => {
            let cursor = peer.frame(json!({"id":history["id"],"error":{"code":-32000,"message":"history unavailable"}})).await;
            peer.acknowledged(cursor).await;
            worker_barrier(&worker).await;
            assert_eq!(state.record().await["restartRecovery"]["stage"], "held");
            assert_eq!(state.event_status("input").await, "uncertain");
            assert!(
                peer.writes.try_recv().is_err(),
                "unknown input is never resubmitted"
            );
        }
        _ => unreachable!(),
    }
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn startup_first_pass_reconciles_exact_history_without_replaying_unknown_input() {
    for outcome in [
        "delivered",
        "terminal",
        "terminal_failure",
        "not_applied",
        "unavailable",
    ] {
        restart_history_case(outcome).await;
    }
}

/// A turn that ran when the backend and its providers were replaced, as in the
/// 18 October 2026 cutover (agents a5fa7930 and be102fa0). Restart recovery
/// left it interrupted with a pending marker. The first worker of the new
/// backend reads native history and continues it, as Python's connection
/// recovery does, without submitting the old input again.
async fn seed_restart_turn(state: &State) {
    let agent_id = state.agent.clone();
    let epoch = state.epoch;
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let mut agent = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                        .expect("seed agent");
                    let receipt = json!({
                        "epoch":epoch,"accountKey":"default","threadId":"fake-thread-0",
                        "turnId":"old-turn","autoWake":true,"at":1.0,"startAttempt":null
                    });
                    let mut disconnect = receipt.clone();
                    disconnect["source"] = json!("restart");
                    let mut marker = receipt;
                    marker["stage"] = json!("pending");
                    agent.status = studio_runtime_store::AgentStatus::Interrupted;
                    agent.extra.insert("threadId".into(), json!("fake-thread-0"));
                    agent.extra.insert("turnId".into(), json!("old-turn"));
                    agent.extra.insert("turnEpoch".into(), json!(epoch));
                    agent.extra.insert("autoWake".into(), json!(false));
                    agent.extra.insert("inFlight".into(), json!(false));
                    agent.extra.insert(
                        "error".into(),
                        json!(studio_runtime_store::BACKEND_RESTARTED_TURN_ERROR),
                    );
                    agent.extra.insert("disconnectRecovery".into(), disconnect);
                    agent.extra.insert("restartRecovery".into(), marker);
                    agent.extra.remove("startAttempt");
                    AgentRepository::put(tx, &agent)?;
                    // The earlier input reached the old turn.
                    tx.execute(
                        "UPDATE runtime_events SET status='delivered',turn_id='old-turn' WHERE id='annotation:input'",
                        [],
                    )?;
                    Ok::<(), WorkerError>(())
                })
                .await?;
            Ok(())
        })
        .await
        .expect("seed restart turn");
}

async fn reply_restart_turn_history(peer: &mut Peer, status: &str) {
    for method in ["thread/read", "thread/turns/list", "thread/read"] {
        let request = peer.write(method).await;
        let result = if method == "thread/read" {
            json!({"thread":{"id":"fake-thread-0","status":{"type":"notLoaded"}}})
        } else {
            json!({"data":[{"id":"old-turn","threadId":"fake-thread-0","status":status,
                "items":[{"id":"answer","type":"agentMessage","text":"before restart"}]}]})
        };
        peer.reply(&request, result).await;
    }
}

#[tokio::test]
async fn startup_first_pass_continues_an_interrupted_restart_turn_from_native_history() {
    let state = State::new().await;
    seed_restart_turn(&state).await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    reply_restart_turn_history(&mut peer, "interrupted").await;
    // The continuation wakes the worker: it resumes the thread and submits only
    // the restart follow-up. The old input stays delivered.
    let resume = peer.write("thread/resume").await;
    peer.reply(&resume, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    let start = peer.write("turn/start").await;
    let input = start["params"]["input"].to_string();
    assert!(
        input.contains("Studio restarted. The previous native turn is confirmed interrupted."),
        "{input}"
    );
    assert!(!input.contains("annotation:input"), "{input}");
    peer.accepted(&start).await;
    worker_barrier(&worker).await;
    let record = state.record().await;
    assert_eq!(record["restartRecovery"]["stage"], "continued");
    assert_eq!(
        record["restartRecovery"]["eventId"],
        format!("restart:{}:{}:old-turn", state.agent, state.epoch)
    );
    assert_eq!(record["lastCompletedTurn"], "old-turn");
    assert_eq!(record["connectionRecovery"]["continued"], true);
    assert_eq!(state.event_status("input").await, "delivered");
    worker.shutdown().await.unwrap();
}

/// Idle agents of the same account whose ids sort before the test agent. They
/// are not eligible for recovery, so a scan page of them hands off nothing.
async fn seed_idle_agents_before(state: &State, count: usize) {
    let agent_id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let template = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                        .expect("seed agent")
                        .value();
                    for n in 0..count {
                        let id = format!("0000-idle-{n:03}");
                        assert!(id < agent_id, "{id} must sort before {agent_id}");
                        let mut idle = template.clone();
                        idle["id"] = json!(id);
                        idle["rootId"] = json!(id);
                        idle["status"] = json!("completed");
                        idle["inFlight"] = json!(false);
                        idle["turnId"] = Value::Null;
                        let object = idle.as_object_mut().expect("agent object");
                        for key in ["startAttempt", "restartRecovery", "disconnectRecovery"] {
                            object.remove(key);
                        }
                        AgentRepository::put(
                            tx,
                            &studio_runtime_store::AgentRecord::from_value(idle)
                                .expect("idle agent"),
                        )?;
                    }
                    Ok::<(), WorkerError>(())
                })
                .await?;
            Ok(())
        })
        .await
        .expect("seed idle agents");
}

/// Rehearsal 9 (agent 3a35babb): the account had 415 agents and the restart
/// turn sorted after the first scan page of 32. The first pass stopped at that
/// page, so the turn was never read, its marker stayed pending, and a later
/// restart interrupted the agent again after the user's next turn.
#[tokio::test]
async fn startup_first_pass_reaches_a_restart_turn_after_the_first_scan_page() {
    let state = State::new().await;
    seed_idle_agents_before(&state, crate::recovery_runtime::MAX_SCAN_JOBS + 8).await;
    seed_restart_turn(&state).await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    reply_restart_turn_history(&mut peer, "interrupted").await;
    let resume = peer.write("thread/resume").await;
    peer.reply(&resume, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    let start = peer.write("turn/start").await;
    let input = start["params"]["input"].to_string();
    assert!(
        input.contains("Studio restarted. The previous native turn is confirmed interrupted."),
        "{input}"
    );
    peer.accepted(&start).await;
    worker_barrier(&worker).await;
    let record = state.record().await;
    assert_eq!(record["restartRecovery"]["stage"], "continued");
    assert_eq!(record["lastCompletedTurn"], "old-turn");
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn startup_first_pass_finishes_a_completed_restart_turn_without_new_input() {
    let state = State::new().await;
    seed_restart_turn(&state).await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    reply_restart_turn_history(&mut peer, "completed").await;
    worker_barrier(&worker).await;
    let record = state.record().await;
    assert_eq!(record["status"], "completed");
    assert_eq!(record["restartRecovery"]["stage"], "finished");
    assert_eq!(record["lastCompletedTurn"], "old-turn");
    assert_eq!(record["lastAnswer"], "before restart");
    assert_eq!(record["autoWake"], true);
    assert!(record["error"].is_null(), "{record}");
    assert!(
        peer.writes.try_recv().is_err(),
        "a completed restart turn submits nothing"
    );
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn restart_terminal_commit_before_wake_converges_on_next_worker_start() {
    let state = State::new().await;
    let agent_id = state.agent.clone();
    let epoch = state.epoch;
    state
        .bridge
        .run(move |store| async move {
            store.transaction_with(move |tx| {
                let mut agent = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                    .expect("seed agent");
                let attempt = json!({
                    "id":"restart-attempt","epoch":epoch,"accountKey":"default",
                    "threadId":"fake-thread-0","events":["annotation:input"],"submitted":true,
                    "executionOutcome":"unknown","created":1.0,"connectionGeneration":1
                });
                agent.status = studio_runtime_store::AgentStatus::Queued;
                agent.extra.insert("threadId".into(), json!("fake-thread-0"));
                agent.extra.insert("autoWake".into(), json!(true));
                agent.extra.insert("inFlight".into(), json!(false));
                agent.extra.insert("error".into(), json!("held"));
                agent.extra.insert("restartRecovery".into(), json!({"stage":"held","startAttempt":attempt}));
                agent.extra.insert("contextRepairWait".into(), json!({
                    "error":"held","scope":"native","events":["annotation:input"],"nextCheckAt":0,
                    "source":{"id":agent_id,"epoch":epoch,"accountKey":"default","threadId":"fake-thread-0","attemptId":null}
                }));
                agent.extra.remove("startAttempt");
                AgentRepository::put(tx, &agent)?;
                tx.execute("UPDATE runtime_events SET status='uncertain',error='Server restarted before delivery acknowledgement' WHERE id='annotation:input'", [])?;
                Ok::<(), WorkerError>(())
            }).await?;
            Ok(())
        })
        .await
        .unwrap();

    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            let raw: String = store.read()?.query_row(
                "SELECT record FROM runtime_agents WHERE id=?1",
                [&agent],
                |row| row.get(0),
            )?;
            let snapshot = RecoverySnapshot::from_record(serde_json::from_str(&raw).unwrap())
                .map_err(|error| StoreError::SchemaMismatch(error.location))
                .map_err(WorkerError::from)?;
            let scope = ConnectionScope {
                account: "default".into(),
                connection: "restart-test".into(),
                generation: 1,
            };
            let turn = json!({
                "id":"old-turn","threadId":"fake-thread-0","status":"completed",
                "items":[{"id":"answer","type":"agentMessage","text":"recovered answer"}],
                "clientUserMessageId":"annotation:input"
            });
            let action = RecoveryAction::AcceptStart { turn };
            let mut completion = super::super::restart_recovery::RestartInputCompletion::default();
            let outcome = store.transaction_with(move |tx| {
                tx.execute(
                    "UPDATE runtime_agents SET record=json_set(record,'$.contextRepairWait.nextCheckAt',1) WHERE id=?1",
                    [&agent],
                )?;
                let stale_wait = completion
                    .complete(tx, &snapshot, &scope, &action)
                    .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
                assert!(matches!(stale_wait.outcome, RecoveryOutcome::Superseded));
                tx.execute(
                    "UPDATE runtime_agents SET record=json_set(record,'$.contextRepairWait.nextCheckAt',0) WHERE id=?1",
                    [&agent],
                )?;
                tx.execute(
                    "UPDATE runtime_events SET status='pending' WHERE id='annotation:input'",
                    [],
                )?;
                let stale_event = completion
                    .complete(tx, &snapshot, &scope, &action)
                    .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
                assert!(matches!(stale_event.outcome, RecoveryOutcome::Superseded));
                tx.execute(
                    "UPDATE runtime_events SET status='uncertain' WHERE id='annotation:input'",
                    [],
                )?;
                completion
                    .complete(tx, &snapshot, &scope, &action)
                    .map_err(|error| StoreError::SchemaMismatch(error.to_string()))
            }).await?;
            assert!(matches!(outcome.outcome, RecoveryOutcome::Applied(_)));
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(state.record().await["lastCompletedTurn"], "old-turn");
    assert_eq!(state.event_status("input").await, "delivered");
    let agent = state.agent.clone();
    let item_id = format!("{agent}:answer");
    let item = state
        .bridge
        .run(move |store| async move {
            Ok(
                studio_runtime_store::TranscriptRepository::item(&*store.read()?, &agent, &item_id)
                    .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?,
            )
        })
        .await
        .unwrap()
        .expect("terminal item is committed with the settlement");
    assert_eq!(item.record["turnStatus"], "completed");
    let mut peer = Peer::new();
    let publications = Arc::new(RecoveredPublicationRecorder::default());
    let mut worker = peer
        .open_worker_with_resources_and_sink(
            state.bridge.clone(),
            test_resources().unwrap(),
            publications.clone(),
        )
        .await;
    let mut ready = peer.ready.clone();
    while !*ready.borrow_and_update() {
        ready.changed().await.unwrap();
    }
    worker_barrier(&worker).await;
    let recovered = publications
        .0
        .lock()
        .expect("recorded recovered publications")
        .clone();
    assert_eq!(
        recovered,
        [
            (
                "restart:restart-attempt:old-turn:item:answer".into(),
                "item/completed".into(),
            ),
            (
                "restart:restart-attempt:old-turn:turn".into(),
                "turn/completed".into(),
            ),
        ]
    );
    worker_barrier(&worker).await;
    assert_eq!(
        publications
            .0
            .lock()
            .expect("recorded recovered publications")
            .len(),
        2,
        "restart recovery publishes each committed notification once"
    );
    assert!(
        peer.writes.try_recv().is_err(),
        "committed recovery does not resubmit input on restart"
    );
    worker.shutdown().await.unwrap();
}
#[tokio::test]
async fn retained_lost_waiter_preserves_unknown_until_exact_late_reply() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let call = request(&state, &port, "lost-control");
    let sender = port.clone();
    let first = call.clone();
    let task = tokio::spawn(async move { sender.request(first).await });
    let wire = peer.write("thread/read").await;
    task.abort();
    let _ignored = task.await;
    assert_eq!(
        port.request(call.clone()).await.unwrap().result,
        RequestResult::Unknown
    );
    peer.reply(&wire, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    port.barrier(port.scope(), call.deadline).await.unwrap();
    assert!(matches!(
        port.request(call).await.unwrap().result,
        RequestResult::Reply(_)
    ));
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}
#[tokio::test]
async fn retained_stale_scope_settles_unknown_and_epoch_expiry_refuses_before_write() {
    let state = State::new().await;
    let peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let mut call = request(&state, &port, "scope");
    call.scope.generation += 1;
    assert_eq!(port.request(call).await.unwrap_err(), PortError::Unknown);
    let mut call = request(&state, &port, "epoch");
    call.actor.as_mut().expect("actor-bound request").epoch += 1;
    assert_eq!(port.request(call).await.unwrap_err(), PortError::Identity);
    let mut call = request(&state, &port, "expired");
    call.deadline = tokio::time::Instant::now().into_std();
    assert_eq!(port.request(call).await.unwrap_err(), PortError::NotSent);
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn stale_retained_request_after_restart_settles_unknown_without_resend() {
    let state = State::new().await;
    let old_peer = Peer::new();
    let mut old_worker = old_peer.worker(state.bridge.clone()).await;
    let old_port = old_worker.retained_session();
    let stale_request = request(&state, &old_port, "restart-without-submission");
    old_worker.shutdown().await.unwrap();

    let mut replacement_peer = Peer::new();
    let mut replacement_worker = replacement_peer.worker(state.bridge.clone()).await;
    let replacement_port = replacement_worker.retained_session();
    assert_ne!(stale_request.scope, *replacement_port.scope());

    assert_eq!(
        replacement_port.request(stale_request).await.unwrap_err(),
        PortError::Unknown,
        "a request bound to a vanished connection is settled as uncertain"
    );
    assert!(
        replacement_peer.writes.try_recv().is_err(),
        "the replacement connection must not resend a stale request"
    );
    replacement_worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn retained_authorization_refuses_deleted_agent() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    tx.execute(
                    "UPDATE runtime_agents SET record=json_set(record,'$.deletedAt',1) WHERE id=?1",
                    [agent],
                )?;
                    Ok(())
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(
        port.request(request(&state, &port, "deleted-agent"))
            .await
            .unwrap_err(),
        PortError::Identity
    );
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

fn monitor_request(
    state: &State,
    port: &RetainedSessionPort,
    operation: String,
    method: Method,
    params: Value,
) -> SessionRequest {
    SessionRequest {
        scope: port.scope().clone(),
        expected_scope: Some(port.scope().clone()),
        operation,
        actor: Some(RequestActor {
            agent: state.agent.clone(),
            epoch: state.epoch,
        }),
        transfer: None,
        deleted_actor_session: None,
        method,
        params,
        deadline: (tokio::time::Instant::now() + std::time::Duration::from_secs(60)).into_std(),
    }
}

/// Wait for the provider write of `method`; a refusal before that write fails
/// the test at once instead of at the write deadline.
async fn monitor_write(
    peer: &mut Peer,
    task: &mut tokio::task::JoinHandle<
        Result<crate::account_worker::retained::SessionReceipt, PortError>,
    >,
    method: &str,
) -> Value {
    tokio::select! {
        wire = peer.write(method) => wire,
        result = task => panic!("{method} ended before its provider write: {result:?}"),
    }
}

/// Live defect 2026-10-19: an `orchestration_monitor` command of an agent on a
/// non-default account stayed `starting` forever. The tool records no monitor
/// `accountKey` (Python never did either), and this check read the missing key
/// as `default`, so it refused the start before any provider write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_monitor_on_a_non_default_account_reaches_its_provider() {
    let state = State::new().await;
    let agent = state.agent.clone();
    let epoch = state.epoch;
    let cwd = state.record().await["cwd"].as_str().unwrap().to_owned();
    let seed_cwd = cwd.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let mut row =
                        AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                    row.extra.insert("accountKey".into(), json!("profile-a"));
                    AgentRepository::put(tx, &row)?;
                    let monitor = |id: &str, extra: Value| {
                        let mut record = json!({"id":id,"agent":agent,"epoch":epoch,
                            "cwd":seed_cwd,"status":"starting","command":"true"});
                        record
                            .as_object_mut()
                            .unwrap()
                            .extend(extra.as_object().unwrap().clone());
                        record
                    };
                    for record in [
                        monitor("tool-monitor", json!({})),
                        monitor("foreign-rule", json!({"accountKey":"other-account"})),
                        monitor(
                            "foreign-command",
                            json!({"status":"running","operation":{"accountKey":"other-account",
                                "agent":agent,"epoch":epoch,"connectionId":"old"}}),
                        ),
                    ] {
                        tx.execute(
                            "INSERT INTO runtime_monitors(id,record) VALUES(?1,?2)",
                            (record["id"].as_str().unwrap(), record.to_string()),
                        )?;
                    }
                    Ok(())
                })
                .await
        })
        .await
        .unwrap();
    let mut peer = Peer::identity("profile-a", 1, None);
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();

    let config = monitor_request(
        &state,
        &port,
        format!("monitor-config:tool-monitor:{epoch}"),
        Method::ConfigRead,
        json!({"cwd":cwd,"includeLayers":false}),
    );
    let sender = port.clone();
    let mut task = tokio::spawn(async move { sender.request(config).await });
    let wire = monitor_write(&mut peer, &mut task, "config/read").await;
    peer.reply(&wire, json!({"config":{}})).await;
    assert_eq!(
        task.await.unwrap().unwrap().result,
        RequestResult::Reply(json!({"config":{}}))
    );

    let exec = |id: &str| {
        monitor_request(
            &state,
            &port,
            format!("monitor:{id}"),
            Method::CommandExec,
            json!({"command":["/bin/sh","-c","true"],"cwd":cwd,"processId":id}),
        )
    };
    let sender = port.clone();
    let call = exec("tool-monitor");
    let mut task = tokio::spawn(async move { sender.request(call).await });
    let wire = monitor_write(&mut peer, &mut task, "command/exec").await;
    assert_eq!(wire["params"]["processId"], "tool-monitor");
    peer.reply(&wire, json!({"exitCode":0,"stdout":"","stderr":""}))
        .await;
    assert!(task.await.unwrap().is_ok());

    // A monitor bound to another account stays refused on this connection.
    for refused in [
        monitor_request(
            &state,
            &port,
            format!("monitor-config:foreign-rule:{epoch}"),
            Method::ConfigRead,
            json!({"cwd":cwd,"includeLayers":false}),
        ),
        exec("foreign-command"),
    ] {
        assert_eq!(
            port.request(refused).await.unwrap_err(),
            PortError::Identity
        );
    }
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleted_voice_stop_requires_the_exact_committed_session() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let actor = state.agent.clone();
    let epoch = state.epoch;
    let scope = port.scope().clone();
    let receipt_id = format!("agent-delete-voice:{actor}:{epoch}");
    let session = json!({
        "sessionId":"voice-session-7",
        "agentId":actor,
        "epoch":epoch,
        "accountKey":scope.account,
        "threadId":"fake-thread-0",
        "connectionId":scope.connection,
        "generation":scope.generation
    });
    let actor_for_store = actor.clone();
    state.bridge.run(move |store| async move {
        store.transaction_with(move |tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.deletedAt',1) WHERE id=?1",
                [&actor_for_store],
            )?;
            tx.execute(
                "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'voice-delete',?2)",
                rusqlite::params![receipt_id, json!({"agent":actor_for_store,"epoch":epoch,"voiceEndSessions":[session]}).to_string()],
            )?;
            Ok::<(), studio_runtime_store::StoreError>(())
        }).await?;
        Ok(())
    }).await.unwrap();

    let mut call = request(&state, &port, "voice:delete-stop:voice-session-7");
    call.method = Method::ThreadRealtimeStop;
    call.params = json!({"threadId":"fake-thread-0"});
    call.expected_scope = Some(scope.clone());
    call.deleted_actor_session = Some("voice-session-7".into());
    let sender = port.clone();
    let task = tokio::spawn(async move { sender.request(call).await });
    let wire = peer.write("thread/realtime/stop").await;
    peer.reply(&wire, json!({})).await;
    assert_eq!(
        task.await.unwrap().unwrap().result,
        RequestResult::Reply(json!({}))
    );
    assert!(peer.writes.try_recv().is_err());

    let mut mismatched = request(&state, &port, "voice:delete-stop:unlisted-session");
    mismatched.method = Method::ThreadRealtimeStop;
    mismatched.params = json!({"threadId":"fake-thread-0"});
    mismatched.expected_scope = Some(scope);
    mismatched.deleted_actor_session = Some("unlisted-session".into());
    assert_eq!(
        port.request(mismatched).await.unwrap_err(),
        PortError::Identity
    );
    assert!(peer.writes.try_recv().is_err());

    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn repeated_retained_history_reads_observe_fresh_provider_state() {
    use crate::{
        account_worker::retained::consumers::HistoryPort,
        recovery_history::{HistoryMethod, HistoryReceipt},
        recovery_runtime::RetainedHistory,
    };

    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let retained = worker.retained_session();
    let scope = retained.scope().clone();
    let first_history = HistoryPort::new(
        retained.clone(),
        RequestActor {
            agent: state.agent.clone(),
            epoch: state.epoch,
        },
        "history-read-test".into(),
    );
    let first_deadline =
        (tokio::time::Instant::now() + std::time::Duration::from_secs(60)).into_std();
    let params = json!({"threadId":"fake-thread-0","includeTurns":false});

    let first_scope = scope.clone();
    let first_params = params.clone();
    let first = tokio::spawn(async move {
        let mut history = first_history;
        let receipt = history
            .request(
                &first_scope,
                HistoryMethod::Thread,
                first_params,
                first_deadline,
            )
            .await;
        (history, receipt)
    });
    let first_wire = peer.write("thread/read").await;
    let first_cursor = peer
        .frame(json!({
            "id":first_wire["id"],
            "result":{"thread":{"id":"fake-thread-0","status":{"type":"active"},"revision":1}}
        }))
        .await;
    peer.acknowledged_or_worker_exits(first_cursor, &mut worker)
        .await;
    let (history, first_result): (HistoryPort, Result<HistoryReceipt, _>) = first.await.unwrap();
    let first_result = first_result.unwrap();

    let second_scope = scope.clone();
    let second_params = params.clone();
    let second_deadline =
        (tokio::time::Instant::now() + std::time::Duration::from_secs(60)).into_std();
    let second = tokio::spawn(async move {
        let mut history = history;
        let receipt = history
            .request(
                &second_scope,
                HistoryMethod::Thread,
                second_params,
                second_deadline,
            )
            .await;
        (history, receipt)
    });
    let mut second = second;
    let second_wire = tokio::select! {
        wire = peer.write("thread/read") => wire,
        _result = &mut second => panic!("repeated history read reused an earlier receipt"),
    };
    assert_eq!(second_wire["method"], "thread/read");
    assert_ne!(first_wire["id"], second_wire["id"]);
    let second_cursor = peer
        .frame(json!({
            "id":second_wire["id"],
            "result":{"thread":{"id":"fake-thread-0","status":{"type":"idle"},"revision":2}}
        }))
        .await;
    peer.acknowledged_or_worker_exits(second_cursor, &mut worker)
        .await;
    let (_history, second_result) = second.await.unwrap();
    let second_result = second_result.unwrap();
    assert_eq!(first_result.body["thread"]["revision"], 1);
    assert_eq!(second_result.body["thread"]["revision"], 2);
    assert_eq!(first_result.body["thread"]["status"]["type"], "active");
    assert_eq!(second_result.body["thread"]["status"]["type"], "idle");
    worker.shutdown().await.unwrap();
}
#[tokio::test]
async fn retained_barrier_waits_for_committed_journal_without_owning_bridge() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    *state.bridge.faults.pause_operation.lock().unwrap() = Some("journal".into());
    let cursor = peer
        .frame(json!({"method":"test/barrier","params":{}}))
        .await;
    state.bridge.faults.paused.notified().await;
    let deadline = (tokio::time::Instant::now() + std::time::Duration::from_secs(60)).into_std();
    let waiter = tokio::spawn(async move { port.barrier(port.scope(), deadline).await });
    // The same permit remains available while the journal/barrier is paused.
    state
        .bridge
        .run(|s| async move {
            let _: i64 = s.read()?.query_row("SELECT 1", [], |r| r.get(0))?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(!waiter.is_finished());
    *state.bridge.faults.pause_operation.lock().unwrap() = None;
    state.bridge.faults.release.notify_waiters();
    waiter.await.unwrap().unwrap();
    peer.acknowledged(cursor).await;
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn connection_recovery_history_receipt_survives_worker_restart_without_resend() {
    use crate::{
        account_worker::retained::consumers::HistoryPort,
        recovery_history::{HistoryMethod, HistoryReceipt},
        recovery_runtime::RetainedHistory,
    };

    let state = State::new().await;
    let before = state.record().await;
    let recovery_id = crate::recovery_policy::connection_recovery_id(&before);
    crate::account_worker::connection_recovery::begin_request(
        &state.bridge.store,
        &state.agent,
        &before,
        &recovery_id,
    )
    .await
    .expect("persist request intent before history read");
    let params = json!({"threadId":"fake-thread-0","includeTurns":false});
    let expected = json!({"thread":{"id":"fake-thread-0","revision":7}});

    let mut first_peer = Peer::identity("default", 1, None);
    let mut first_worker = first_peer.worker(state.bridge.clone()).await;
    let first_session = first_worker.retained_session();
    let first_scope = first_session.scope().clone();
    let first_history = HistoryPort::new(
        first_session,
        RequestActor {
            agent: state.agent.clone(),
            epoch: state.epoch,
        },
        recovery_id.clone(),
    );
    let deadline = (tokio::time::Instant::now() + std::time::Duration::from_secs(60)).into_std();
    let first_params = params.clone();
    let first = tokio::spawn(async move {
        let mut history = first_history;
        history
            .request(&first_scope, HistoryMethod::Thread, first_params, deadline)
            .await
    });
    let request = first_peer.write("thread/read").await;
    first_peer.reply(&request, expected.clone()).await;
    let first_receipt: HistoryReceipt = first.await.unwrap().unwrap();
    assert_eq!(first_receipt.body, expected);

    // This models a process crash after the retained provider receipt is durable,
    // but before RecoveryRuntime can commit the final connectionRecovery result.
    first_worker.shutdown().await.unwrap();

    let mut second_peer = Peer::identity("default", 2, None);
    let mut second_worker = second_peer.worker(state.bridge.clone()).await;
    let second_session = second_worker.retained_session();
    let second_scope = second_session.scope().clone();
    let second_history = HistoryPort::new(
        second_session,
        RequestActor {
            agent: state.agent.clone(),
            epoch: state.epoch,
        },
        recovery_id.clone(),
    );
    let deadline = (tokio::time::Instant::now() + std::time::Duration::from_secs(60)).into_std();
    let mut second_history = second_history;
    let replay: HistoryReceipt = second_history
        .request(&second_scope, HistoryMethod::Thread, params, deadline)
        .await
        .unwrap();
    assert_eq!(replay.body, first_receipt.body);
    assert_eq!(replay.scope.generation, 1);
    assert!(
        second_peer.writes.try_recv().is_err(),
        "saved history receipt must not be resent"
    );
    let agent = state.record().await;
    assert_eq!(agent["connectionRecovery"]["requestId"], recovery_id);
    assert_eq!(agent["connectionRecovery"]["requestState"], "pending");
    second_worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn connection_recovery_repeated_request_returns_saved_status_without_history_read() {
    let state = State::new().await;
    let before = state.record().await;
    let recovery_id = crate::recovery_policy::connection_recovery_id(&before);
    let agent_id = state.agent.clone();
    let saved = json!({"status":"superseded"});
    crate::account_worker::connection_recovery::begin_request(
        &state.bridge.store,
        &agent_id,
        &before,
        &recovery_id,
    )
    .await
    .expect("persist recovery intent");
    crate::account_worker::connection_recovery::finish_request(
        &state.bridge.store,
        &agent_id,
        &recovery_id,
        &saved,
    )
    .await
    .expect("persist recovery result");

    let (commands, _receiver) = tokio::sync::mpsc::channel(1);
    let session = RetainedSessionPort {
        scope: ConnectionScope {
            account: "default".into(),
            connection: "saved-result-test".into(),
            generation: 1,
        },
        commands,
        stop: studio_runtime_process::provider_seam::Shutdown::default(),
    };
    let result = crate::account_worker::connection_recovery::run(
        crate::account_worker::connection_recovery::Command {
            bridge: state.bridge.clone(),
            account: "default".into(),
            scope: session.scope().clone(),
            session,
            agent_id,
        },
    )
    .await
    .unwrap();
    assert_eq!(result.receipt, saved);
}

#[tokio::test]
async fn recovery_reconcile_wait_does_not_hold_store_bridge_permit() {
    use crate::{
        recovery::{RecoveryError, RecoveryOutcome},
        recovery_absence::RecoveryObservation,
        recovery_history::ConnectionScope,
        recovery_policy::RecoverySnapshot,
        recovery_runtime::{RecoveryJob, RecoveryRuntime, RetainedRecoveryReader},
        recovery_store::CompletionWriter,
        task_recovery_store::TaskCompletionWriter,
    };

    struct Completion;
    impl CompletionWriter for Completion {
        fn start_held(
            &mut self,
            _: &studio_runtime_store::Transaction<'_>,
            _: &Value,
            _: &str,
            _: &str,
        ) -> Result<(), RecoveryError> {
            Ok(())
        }
        fn complete(
            &mut self,
            _: &studio_runtime_store::Transaction<'_>,
            _: &RecoverySnapshot,
            _: &ConnectionScope,
            _: &crate::recovery::RecoveryAction,
        ) -> Result<crate::recovery::RecoveryEffectResult, RecoveryError> {
            Ok(crate::recovery::RecoveryEffectResult::plain(
                RecoveryOutcome::Unconfirmed,
            ))
        }
    }
    struct TaskCompletion;
    impl TaskCompletionWriter for TaskCompletion {
        fn resolve_receipt<'a>(
            &'a self,
            receipt: Arc<Value>,
        ) -> futures_util::future::BoxFuture<'a, Result<Value, RecoveryError>> {
            Box::pin(async move { Ok(receipt.as_ref().clone()) })
        }
        fn complete<'a>(
            &'a self,
            _: &'a crate::task_recovery::TaskRecoverySnapshot,
            _: &'a ConnectionScope,
            _: Value,
        ) -> futures_util::future::BoxFuture<'a, Result<bool, RecoveryError>> {
            Box::pin(async { Ok(false) })
        }
    }

    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let mut record = state.record().await;
    record["inFlight"] = json!(true);
    record["status"] = json!("running");
    if let Some(agent) = record.as_object_mut() {
        agent.remove("startAttempt");
        agent.remove("turnId");
    }
    let snapshot = RecoverySnapshot::from_record(record).unwrap();
    let session = worker.retained_session();
    let scope = session.scope().clone();
    let observation = RecoveryObservation {
        scope: scope.clone(),
        at: 100.0,
        supervisor: None,
        quiet: false,
        journal_drained: false,
        account_reserved: true,
        tools_refreshing: false,
        preparation_pending: false,
        completed_turn: None,
    };
    let observation_scope = scope.clone();
    let observation_for_reader = observation.clone();
    let actor = RequestActor {
        agent: state.agent.clone(),
        epoch: state.epoch,
    };
    let store = state.bridge.store.clone();
    let history_port = crate::account_worker::retained::consumers::HistoryPort::new(
        session,
        actor,
        "recovery-runtime-test".into(),
    );
    let reconcile = tokio::spawn(async move {
        let observation: Arc<crate::recovery_runtime::ObservationSource> =
            Arc::new(move |current: &ConnectionScope| {
                (current == &observation_scope).then(|| observation_for_reader.clone())
            });
        let completion: Arc<std::sync::Mutex<Box<dyn CompletionWriter>>> =
            Arc::new(std::sync::Mutex::new(Box::new(Completion)));
        let task_completion: Arc<dyn TaskCompletionWriter> = Arc::new(TaskCompletion);
        let mut history = history_port;
        let mut reader = RetainedRecoveryReader::new(
            scope.clone(),
            "fake-thread-0".into(),
            &mut history,
            observation.as_ref(),
        )
        .unwrap();
        let mut runtime = RecoveryRuntime {
            store,
            observation: Arc::clone(&observation),
            completion,
            task_completion,
        };
        runtime
            .reconcile(&RecoveryJob::Turn(snapshot), scope, &mut reader)
            .await
    });
    let request = peer.next_write().await;
    assert_eq!(request.value["method"], "thread/read");
    assert!(
        request
            .receipt
            .send(CommandReceipt::Written {
                remote_id: None,
                duplicate: false,
            })
            .is_ok()
    );
    state
        .bridge
        .run(|store| async move {
            let _: i64 = store.read()?.query_row("SELECT 1", [], |row| row.get(0))?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(!reconcile.is_finished());
    peer.reply(
        &request.value,
        json!({"thread":{"id":"fake-thread-0","status":{"type":"active"}}}),
    )
    .await;
    assert!(matches!(
        reconcile.await.unwrap().unwrap().outcome,
        RecoveryOutcome::Unconfirmed
    ));
    worker.shutdown().await.unwrap();
}
#[tokio::test]
async fn retained_reply_failure_cannot_publish_a_positive_receipt() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let call = request(&state, &port, "failed-journal");
    let sender = port.clone();
    let first = call.clone();
    let waiter = tokio::spawn(async move { sender.request(first).await });
    let wire = peer.write("thread/read").await;
    state.bridge.run(|s| async move { s.transaction_with(move |tx| { tx.execute_batch("CREATE TRIGGER fail_journal BEFORE INSERT ON runtime_operation_receipts WHEN NEW.id LIKE 'account-worker:journal:%' BEGIN SELECT RAISE(ABORT,'journal failed'); END;")?; Ok(()) }).await }).await.unwrap();
    peer.frame(json!({"id":wire["id"],"result":{"thread":{"id":"fake-thread-0"}}}))
        .await;
    assert_eq!(waiter.await.unwrap().unwrap_err(), PortError::Unknown);
    assert!(worker.completion().await.is_err());
    state
        .bridge
        .run(|s| async move {
            s.transaction_with(move |tx| {
                tx.execute_batch("DROP TRIGGER fail_journal")?;
                Ok(())
            })
            .await
        })
        .await
        .unwrap();
    // Same generation replay has the same operation receipt but no committed cursor.
    let peer = Peer::new();
    let mut replacement = peer.worker(state.bridge.clone()).await;
    let replacement_port = replacement.retained_session();
    assert_eq!(
        replacement_port.request(call).await.unwrap().result,
        RequestResult::Unknown
    );
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn retained_committed_callback_keeps_exact_response_and_sends_once() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let frame = Frame::Response {
        id: RequestId::String("native-callback".into()),
        body: ResponseBody::Result(json!({"answer":"approved"})),
        fields: Default::default(),
    };
    let deadline = (tokio::time::Instant::now() + std::time::Duration::from_secs(60)).into_std();
    let sender = port.clone();
    let first = frame.clone();
    let task = tokio::spawn(async move {
        sender
            .submit_committed_frame(
                sender.scope(),
                "canonical-operation".into(),
                first,
                deadline,
            )
            .await
    });
    let wire = peer.next_write().await;
    assert_eq!(wire.value["id"], "native-callback");
    assert_eq!(wire.value["result"], json!({"answer":"approved"}));
    assert!(
        wire.receipt
            .send(CommandReceipt::Written {
                remote_id: Some(42),
                duplicate: false,
            })
            .is_ok()
    );
    assert_eq!(task.await.unwrap().unwrap(), WriteOutcome::Written);
    assert_eq!(
        port.submit_committed_frame(port.scope(), "canonical-operation".into(), frame, deadline)
            .await
            .unwrap(),
        WriteOutcome::Written
    );
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn retained_deadline_during_intent_never_writes_after_expiry() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    *state.bridge.faults.pause_operation.lock().unwrap() = Some("retained_submitted".into());
    let mut call = request(&state, &port, "expires-in-queue");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    call.deadline = deadline.into_std();
    let sender = port.clone();
    let first = call.clone();
    let waiter = tokio::spawn(async move { sender.request(first).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        state.bridge.faults.paused.notified(),
    )
    .await
    .expect("retained request reaches its intent pause before expiry");
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    assert!(
        !remaining.is_zero(),
        "intent pause is reached before deadline"
    );
    tokio::time::pause();
    tokio::time::advance(remaining).await;
    assert_eq!(waiter.await.unwrap().unwrap_err(), PortError::Unknown);
    tokio::time::resume();
    *state.bridge.faults.pause_operation.lock().unwrap() = None;
    state.bridge.faults.release.notify_waiters();
    worker_barrier(&worker).await;
    call.deadline = (tokio::time::Instant::now() + std::time::Duration::from_secs(60)).into_std();
    let sender = port.clone();
    let retry_call = call.clone();
    let retry = tokio::spawn(async move { sender.request(retry_call).await });
    let wire = peer.next_write().await;
    assert!(
        wire.receipt
            .send(CommandReceipt::Written {
                remote_id: None,
                duplicate: false,
            })
            .is_ok()
    );
    peer.reply(&wire.value, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    assert!(matches!(
        retry.await.unwrap().unwrap().result,
        RequestResult::Reply(_)
    ));
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn retained_response_before_write_receipt_stays_positive() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let call = request(&state, &port, "reply-before-write");
    let sender = port.clone();
    let first = call.clone();
    let waiter = tokio::spawn(async move { sender.request(first).await });
    let wire = peer.next_write().await;
    let cursor = peer
        .frame(json!({"id":wire.value["id"],"result":{"thread":{"id":"fake-thread-0"}}}))
        .await;
    assert!(
        wire.receipt
            .send(CommandReceipt::Written {
                remote_id: None,
                duplicate: false
            })
            .is_ok()
    );
    peer.acknowledged(cursor).await;
    assert!(matches!(
        waiter.await.unwrap().unwrap().result,
        RequestResult::Reply(_)
    ));
    assert!(matches!(
        port.request(call).await.unwrap().result,
        RequestResult::Reply(_)
    ));
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn claude_adapter_uses_retained_scoped_control_and_invalidates_on_stop() {
    use crate::claude_controls::service::{ControlScope, ControlSession};
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let session = std::sync::Arc::new(worker.claude_session().unwrap());
    let scope = ControlScope {
        agent: state.agent.clone(),
        epoch: state.epoch.try_into().unwrap(),
        request_id: Some("stop-task-request".into()),
    };
    let copy = session.clone();
    let call_scope = scope.clone();
    let task = tokio::spawn(async move {
        copy.call_scoped(
            call_scope,
            "claude/stopTask",
            json!({"threadId":"fake-thread-0","taskId":"task"}),
        )
        .await
    });
    let request = peer.write("claude/stopTask").await;
    peer.reply(&request, json!({"status":"stopped"})).await;
    assert_eq!(task.await.unwrap().unwrap(), json!({"status":"stopped"}));
    assert_eq!(
        session
            .call_scoped(
                scope.clone(),
                "claude/stopTask",
                json!({"threadId":"fake-thread-0","taskId":"task"})
            )
            .await
            .unwrap(),
        json!({"status":"stopped"})
    );
    worker.shutdown().await.unwrap();
    assert!(
        session
            .call_scoped(scope, "claude/state", json!({"threadId":"fake-thread-0"}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn claude_adapter_distinguishes_not_sent_from_unknown() {
    use crate::claude_controls::service::ControlSession;
    use crate::claude_controls::{ControlError, service::ControlScope};
    use crate::claude_runtime::RetainedClaudeSession;

    for (request_id, receipt, expected) in [
        ("not-sent", CommandReceipt::NotSent, "not-sent"),
        ("unknown", CommandReceipt::Unknown, "unknown"),
    ] {
        let state = State::new().await;
        let mut peer = Peer::new();
        peer.wire = studio_runtime_providers::ProviderKind::Claude.wire();
        let mut worker = peer.worker(state.bridge.clone()).await;
        let session = RetainedClaudeSession::new(std::sync::Arc::new(
            crate::account_worker::retained::consumers::ClaudePort::new(worker.retained_session())
                .unwrap(),
        ));
        let scope = ControlScope {
            agent: state.agent.clone(),
            epoch: state.epoch.try_into().unwrap(),
            request_id: Some(request_id.into()),
        };
        let task = tokio::spawn(async move {
            session
                .call_scoped(
                    scope,
                    "claude/stopTask",
                    json!({"threadId":"fake-thread-0","taskId":"task"}),
                )
                .await
        });
        let wire = peer.next_write().await;
        assert_eq!(wire.value["method"], "claude/stopTask");
        let _ignored = wire.receipt.send(receipt);
        match expected {
            "not-sent" => assert!(matches!(task.await.unwrap(), Err(ControlError::NotSent))),
            _ => assert!(matches!(task.await.unwrap(), Err(ControlError::Unknown))),
        }
        worker.shutdown().await.unwrap();
    }
}
