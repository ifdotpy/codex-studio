//! A provider request is durably accepted with its journal batch; its record,
//! not its journal cursor, owns the answer. These witnesses cover the record's
//! transitions, the crash points between commit, acknowledgement and answer,
//! and a held answer that must not stop the intake of provider events.
use super::*;
use crate::account_worker::callback_record;
use crate::tool_requests::{self, CallbackTask};
use studio_runtime_providers::{DecodedJournalPayload, decode_journal_value};
use studio_runtime_schema::persistence::{OutboxKind, OutboxStatus, ToolRequestStatus};
use studio_runtime_store::outbox::Outbox;

const GENERATION: u64 = 1;
const SCOPE: &str = "default";
/// Provider events after one held answer; more than the session's unsettled
/// bound, so the old cursor-held design would stop reading in the middle.
const EVENTS_BEHIND_HELD_ANSWER: u64 = crate::provider_session_task::MAX_UNSETTLED as u64 + 6;

fn request(id: &str) -> Value {
    json!({"id":id,"method":"unknown/providerMethod","params":{}})
}

fn usage() -> Value {
    json!({"method":"thread/tokenUsage/updated","params":{}})
}

fn entry(generation: u64, cursor: u64, frame: &Value) -> JournalEntry {
    let DecodedJournalPayload::Frame(frame) =
        decode_journal_value(cursor, generation, frame).payload
    else {
        panic!("fixture frame decodes");
    };
    JournalEntry {
        received_at_ms: cursor,
        cursor: EventCursor(cursor),
        generation: Generation(generation),
        payload: JournalPayload::Frame(frame),
    }
}

/// Commit entries as one journal batch, without a worker.
async fn journal(state: &State, entries: Vec<JournalEntry>) {
    store::journal_batch(
        Arc::clone(&state.bridge.store),
        "default",
        SCOPE,
        &NotificationStreamBuffer::default(),
        entries,
        &Value::Null,
        None,
        None,
        Vec::new(),
        None,
        false,
    )
    .await
    .expect("journal batch commits");
}

/// Read the exact current callback obligation, including completed identity fences.
fn callback_task(
    state: &State,
    generation: u64,
    cursor: u64,
) -> CallbackTask<callback_record::CallbackObligation> {
    let db = state.bridge.store.test_reader().expect("reader");
    tool_requests::get_callback(&db, &callback_record::operation(SCOPE, generation, cursor))
        .expect("callback task")
        .expect("callback identity fence")
}

/// Prepare the canonical response outbox and the exact handoff metadata used by recovery.
async fn saved_response(state: &State, operation: &str, response: Value) -> Value {
    let operation = operation.to_owned();
    state.bridge.store.transaction_with(move|tx| {
        let task:CallbackTask<callback_record::CallbackObligation>=tool_requests::get_callback(tx,&operation).map_err(|error|WorkerError::PreparationRefused(error.to_string()))?.expect("accepted callback");
        let raw:String=tx.query_row("SELECT record FROM runtime_tool_requests WHERE id=?1",[&task.payload.root_request_id],|row|row.get(0))?;
        let owner:CallbackTask<crate::provider_callbacks::CallbackOwner>=serde_json::from_str(&raw).expect("logical callback owner");
        let mut canonical=response.clone();canonical["id"]=task.payload.accepted.request_id.clone();
        let DecodedJournalPayload::Frame(frame)=decode_journal_value(task.payload.accepted.cursor,task.payload.accepted.generation,&canonical).payload else { panic!("response frame") };
        let identity=crate::provider_callbacks::save_frame_response(tx,&owner.payload.identity.context,&task.payload.accepted.frame()?,&frame).map_err(|error|WorkerError::PreparationRefused(error.to_string()))?;
        assert_eq!(crate::provider_callbacks::begin_frame(tx,&identity.context.stream_id,identity.context.generation,&identity.operation_id(),&frame).map_err(|error|WorkerError::PreparationRefused(error.to_string()))?,crate::provider_callbacks::FrameAdmission::Send);
        let key=crate::provider_callbacks::callback_origin_key(&identity.context,&identity.rpc_id);
        let encoded=format!("{}\n",response);
        let digest=studio_runtime_store::sha256_hex(encoded.as_bytes());
        if response["id"]!=canonical["id"] {
            let mut row=Outbox::get::<crate::provider_callbacks::CallbackResponse>(tx,&key).map_err(StoreError::from)?.expect("response outbox");
            row.payload.identity.rpc_id=serde_json::from_value(response["id"].clone()).expect("corrupt RPC id fixture");
            row.payload.frame_fingerprint=Some(digest.clone());
            Outbox::put(tx,&row).map_err(StoreError::from)?;
        }
        Ok::<_,WorkerError>(json!({"handle":"account:default","generation":task.payload.accepted.generation,"pid":1,"response_key":key,"sha256":digest,"authority":null}))
    }).await.expect("canonical saved response")
}

fn store_submitted(
    tx: &studio_runtime_store::Transaction<'_>,
    operation: &str,
    saved: Value,
) -> Result<(), WorkerError> {
    let mut task: CallbackTask<callback_record::CallbackObligation> =
        tool_requests::get_callback(tx, operation)
            .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?
            .expect("accepted callback");
    task.status = ToolRequestStatus::Running;
    task.payload.submitted = Some(serde_json::from_value(saved).expect("typed response proof"));
    tool_requests::put_callback(tx, &task)
        .map_err(|error| WorkerError::PreparationRefused(error.to_string()))
}

async fn submit_response(state: &State, operation: &str, response: Value) {
    let saved = saved_response(state, operation, response).await;
    let operation = operation.to_owned();
    state
        .bridge
        .store
        .transaction_with(move |tx| store_submitted(tx, &operation, saved))
        .await
        .expect("committed handoff proof");
}

async fn checkpoint(state: &State, generation: u64) -> u64 {
    state
        .bridge
        .store
        .transaction_with(move |tx| store::checkpoint(tx, SCOPE, generation))
        .await
        .expect("checkpoint")
}

async fn unsent(state: &State) -> Vec<callback_record::Accepted> {
    callback_record::unsent(&state.bridge.store, "default", GENERATION, 100)
        .await
        .expect("unsent records")
}

/// A cancelled session retains the in-flight answer's final outcome outside
/// its event lane. The final receipt must determine shutdown's result.
#[tokio::test]
async fn shutdown_reads_the_final_callback_outcome_after_the_event_lane_closes() {
    for outcome in [WriteOutcome::Written, WriteOutcome::Unknown] {
        let state = State::new().await;
        let mut peer = Peer::new();
        let mut worker = peer.worker(state.bridge.clone()).await;
        worker_barrier(&worker).await;
        let cursor = peer.frame(request("shutdown-final-outcome")).await;
        let answer = peer.next_write().await;
        *state.bridge.faults.pause_operation.lock().unwrap() = Some("journal".into());
        peer.frame(usage()).await;
        tokio::time::timeout(SHUTDOWN_GRACE, state.bridge.faults.paused.notified())
            .await
            .expect("worker journals a provider event while the callback receipt is held");

        // Both stops and the receipt become ready in this poll. The session
        // must retain the receipt in SessionExit instead of its event lane.
        worker.session_control.cancel();
        answer
            .receipt
            .send(match outcome {
                WriteOutcome::Written => CommandReceipt::Written {
                    remote_id: None,
                    duplicate: false,
                },
                WriteOutcome::Unknown => CommandReceipt::Unknown,
                WriteOutcome::NotSent => unreachable!("only in-flight outcomes are tested"),
            })
            .ok()
            .expect("adapter still owns the receipt");
        worker.cancel();
        let mut stopped = peer.stop.subscribe();
        tokio::time::timeout(SHUTDOWN_GRACE, Shutdown::cancelled(&mut stopped))
            .await
            .expect("session cancels its adapter during the held store operation");
        state.bridge.faults.release.notify_one();

        let result = worker.completion().await;
        match outcome {
            WriteOutcome::Written => {
                assert!(
                    result.is_ok(),
                    "written callback stops normally: {result:?}"
                );
                assert_eq!(
                    callback_task(&state, GENERATION, cursor).status,
                    ToolRequestStatus::Completed
                );
            }
            WriteOutcome::Unknown => {
                assert!(
                    matches!(result, Err(WorkerError::CallbackUnknown)),
                    "the final receipt must determine the error: {result:?}"
                );
                assert_eq!(
                    callback_task(&state, GENERATION, cursor).status,
                    ToolRequestStatus::Unknown
                );
            }
            WriteOutcome::NotSent => unreachable!("only in-flight outcomes are tested"),
        }
    }
}

#[tokio::test]
async fn record_follows_the_answer_outcome_and_survives_journal_compaction() {
    let state = State::new().await;
    journal(&state, vec![entry(GENERATION, 1, &request("one"))]).await;
    assert_eq!(
        checkpoint(&state, GENERATION).await,
        1,
        "the cursor settles with its batch"
    );
    let records = unsent(&state).await;
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(
        (
            record.cursor,
            record.generation,
            record.journal_scope.as_str()
        ),
        (1, GENERATION, SCOPE)
    );
    assert_eq!(record.request_id, json!("one"));
    assert_eq!(record.method, "unknown/providerMethod");
    assert!(record.frame().is_ok());
    let store = &state.bridge.store;
    let operation = record.operation();
    let current = callback_task(&state, GENERATION, 1);
    assert_eq!(current.origin_key, operation);
    assert_eq!(current.status, ToolRequestStatus::Queued);
    assert!(current.payload.submitted.is_none());
    assert_eq!(
        crate::provider_callbacks::pending_callback_count(
            &state.bridge.store.test_reader().unwrap(),
            "default"
        )
        .unwrap(),
        1
    );

    submit_response(
        &state,
        &operation,
        json!({"id":"one","result":{"success":true}}),
    )
    .await;
    assert_eq!(
        callback_task(&state, GENERATION, 1).status,
        ToolRequestStatus::Running
    );
    let recovery = unsent(&state).await;
    assert_eq!(recovery.len(), 1);
    assert!(
        recovery[0].submitted.is_some(),
        "recovery carries the exact response rather than dispatching the business request"
    );
    assert!(
        callback_task(&state, GENERATION, 1)
            .payload
            .submitted
            .is_some()
    );

    callback_record::written(store, &operation, WriteOutcome::NotSent)
        .await
        .unwrap();
    assert_eq!(
        callback_task(&state, GENERATION, 1).status,
        ToolRequestStatus::Queued
    );
    assert_eq!(
        unsent(&state).await.len(),
        1,
        "a send that never left may be sent again"
    );

    submit_response(
        &state,
        &operation,
        json!({"id":"one","result":{"success":true}}),
    )
    .await;
    callback_record::written(store, &operation, WriteOutcome::Unknown)
        .await
        .unwrap();
    assert_eq!(
        callback_task(&state, GENERATION, 1).status,
        ToolRequestStatus::Unknown
    );
    assert!(unsent(&state).await.is_empty());
    assert!(
        callback_task(&state, GENERATION, 1)
            .payload
            .submitted
            .is_some(),
        "unknown retains response provenance"
    );

    // Later batches compact the journal over the uncertain record's cursor.
    journal(
        &state,
        (2..40)
            .map(|cursor| entry(GENERATION, cursor, &usage()))
            .collect(),
    )
    .await;
    assert_eq!(checkpoint(&state, GENERATION).await, 39);
    assert_eq!(
        callback_task(&state, GENERATION, 1).status,
        ToolRequestStatus::Unknown,
        "compaction implies nothing about callbacks"
    );
    // The same entry journaled again changes nothing.
    journal(&state, vec![entry(GENERATION, 1, &request("one"))]).await;
    assert_eq!(
        callback_task(&state, GENERATION, 1).status,
        ToolRequestStatus::Unknown
    );

    journal(&state, vec![entry(GENERATION, 40, &request("two"))]).await;
    let second = callback_record::operation(SCOPE, GENERATION, 40);
    submit_response(
        &state,
        &second,
        json!({"id":"two","result":{"success":true}}),
    )
    .await;
    callback_record::written(store, &second, WriteOutcome::Written)
        .await
        .unwrap();
    assert_eq!(
        callback_task(&state, GENERATION, 40).status,
        ToolRequestStatus::Completed,
        "a written answer retires its record"
    );
    assert!(
        unsent(&state)
            .await
            .iter()
            .all(|request| request.cursor != 40),
        "a completed callback never enters dispatch"
    );
}

#[tokio::test]
async fn replay_keeps_pending_callback_and_completed_response_owners() {
    let state = State::new().await;
    journal(
        &state,
        vec![
            entry(GENERATION, 1, &request("held")),
            entry(GENERATION, 2, &request("written")),
        ],
    )
    .await;
    let operation = callback_record::operation(SCOPE, GENERATION, 2);
    let saved = saved_response(
        &state,
        &operation,
        json!({"id":"written","result":{"success":true}}),
    )
    .await;
    state
        .bridge
        .store
        .transaction_with(move |tx| store_submitted(tx, &operation, saved))
        .await
        .unwrap();
    callback_record::written(
        &state.bridge.store,
        &callback_record::operation(SCOPE, GENERATION, 2),
        WriteOutcome::Written,
    )
    .await
    .unwrap();
    journal(
        &state,
        vec![
            entry(GENERATION, 1, &request("held")),
            entry(GENERATION, 2, &request("written")),
        ],
    )
    .await;
    assert_eq!(checkpoint(&state, GENERATION).await, 2);
    assert_eq!(
        callback_task(&state, GENERATION, 1).status,
        ToolRequestStatus::Queued,
        "the unanswered request is owed"
    );
    let written = callback_task(&state, GENERATION, 2);
    assert_eq!(
        written.status,
        ToolRequestStatus::Completed,
        "the written answer is not owed again"
    );
    assert!(
        written.payload.accepted.frame.is_empty(),
        "the completed owner is slim"
    );
}

/// Crash after the batch committed and acknowledged, before any answer.
#[tokio::test]
async fn an_accepted_request_is_answered_once_after_restart() {
    let state = State::new().await;
    journal(&state, vec![entry(GENERATION, 1, &request("callback"))]).await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let answer = peer.next_write().await;
    assert_eq!(answer.value["id"], "callback");
    assert_eq!(
        answer.value["error"]["data"]["kind"],
        "unsupported_callback"
    );
    answer
        .receipt
        .send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        })
        .ok()
        .unwrap();
    callbacks_retired(&state).await;
    worker_barrier(&worker).await;
    assert!(
        peer.writes.try_recv().is_err(),
        "the answer is written once"
    );
    worker.shutdown().await.unwrap();
}

/// Crash after the batch committed but before the session acknowledged it: the
/// provider sends the request again.
#[tokio::test]
async fn a_request_redelivered_after_commit_is_answered_once() {
    let state = State::new().await;
    journal(&state, vec![entry(GENERATION, 1, &request("callback"))]).await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    peer.frame(request("callback")).await;
    let answer = peer.next_write().await;
    assert_eq!(answer.value["id"], "callback");
    answer
        .receipt
        .send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        })
        .ok()
        .unwrap();
    callbacks_retired(&state).await;
    worker_barrier(&worker).await;
    assert!(
        peer.writes.try_recv().is_err(),
        "the answer is written once"
    );
    worker.shutdown().await.unwrap();
    // A third delivery after the answer was written finds no record.
    let mut replay = Peer::new();
    let mut replay_worker = replay.worker(state.bridge.clone()).await;
    replay.frame(request("callback")).await;
    worker_barrier(&replay_worker).await;
    assert!(
        replay.writes.try_recv().is_err(),
        "a retired record is not answered again"
    );
    replay_worker.shutdown().await.unwrap();
}

/// Crash after the answer was submitted, before its write outcome was known.
#[tokio::test]
async fn a_callback_with_missing_response_provenance_stays_held_after_restart() {
    let state = State::new().await;
    journal(&state, vec![entry(GENERATION, 1, &request("callback"))]).await;
    let operation = callback_record::operation(SCOPE, GENERATION, 1);
    state
        .bridge
        .store
        .transaction_with(move |tx| {
            let mut task: CallbackTask<callback_record::CallbackObligation> =
                tool_requests::get_callback(tx, &operation)
                    .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?
                    .unwrap();
            task.status = ToolRequestStatus::Unknown;
            tool_requests::put_callback(tx, &task)
                .map_err(|error| WorkerError::PreparationRefused(error.to_string()))
        })
        .await
        .unwrap();
    assert!(
        callback_reply_accounts(&state.bridge.store.test_reader().unwrap(), "", 10)
            .unwrap()
            .is_empty(),
        "missing response provenance is not recovery authority"
    );
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    peer.frame(request("callback")).await;
    worker_barrier(&worker).await;
    assert!(
        peer.writes.try_recv().is_err(),
        "an uncertain answer is not resent"
    );
    assert_eq!(
        callback_task(&state, GENERATION, 1).status,
        ToolRequestStatus::Unknown,
        "the uncertainty is kept"
    );
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn submitted_callback_recovery_sends_exact_saved_reply_without_dispatching_the_tool() {
    let state = State::new().await;
    journal(&state, vec![entry(GENERATION, 1, &request("saved-spawn"))]).await;
    let operation = callback_record::operation(SCOPE, GENERATION, 1);
    let response =
        json!({"id":"saved-spawn","result":{"existingChild":"already-created","success":true}});
    let saved = saved_response(&state, &operation, response.clone()).await;
    state
        .bridge
        .store
        .transaction_with(move |tx| store_submitted(tx, &operation, saved))
        .await
        .unwrap();
    let reader = state.bridge.store.test_reader().unwrap();
    assert_eq!(
        callback_reply_accounts(&reader, "", 1).unwrap(),
        ["default"]
    );
    assert!(
        callback_reply_accounts(&reader, "default", 1)
            .unwrap()
            .is_empty()
    );
    assert!(callback_reply_accounts(&reader, "", 0).unwrap().is_empty());
    drop(reader);
    let mut peer = Peer::supervised();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let wire = tokio::time::timeout(std::time::Duration::from_secs(5), peer.next_write())
        .await
        .expect("the retained worker recovers the saved callback reply");
    assert_eq!(
        wire.value, response,
        "never rebuild or redispatch the business operation"
    );
    wire.receipt
        .send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        })
        .unwrap_or_else(|_| panic!("the receipt has an owner"));
    callbacks_retired(&state).await;
    worker_barrier(&worker).await;
    assert_eq!(
        callback_task(&state, GENERATION, 1).status,
        ToolRequestStatus::Completed
    );
    assert!(
        peer.writes.try_recv().is_err(),
        "one callback response only"
    );
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn saved_callback_recovery_refuses_changed_identity_digest_and_direct_transport() {
    for case in [
        "handle",
        "generation",
        "pid",
        "digest",
        "rpc-id",
        "direct",
        "unknown",
        "recoverable-pid",
    ] {
        let state = State::new().await;
        journal(&state, vec![entry(GENERATION, 1, &request("saved-answer"))]).await;
        let operation = callback_record::operation(SCOPE, GENERATION, 1);
        let id = if case == "rpc-id" {
            "another-request"
        } else {
            "saved-answer"
        };
        let mut saved = saved_response(
            &state,
            &operation,
            json!({"id":id,"result":{"success":true}}),
        )
        .await;
        match case {
            "handle" => saved["handle"] = json!("account:other"),
            "generation" => saved["generation"] = json!(GENERATION + 1),
            "pid" | "recoverable-pid" => saved["pid"] = json!(2),
            "digest" => saved["sha256"] = json!("incorrect"),
            _ => {}
        }
        state
            .bridge
            .store
            .transaction_with(move |tx| {
                store_submitted(tx, &operation, saved)?;
                if case == "unknown" {
                    let mut task: CallbackTask<callback_record::CallbackObligation> =
                        tool_requests::get_callback(tx, &operation)
                            .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?
                            .unwrap();
                    task.status = ToolRequestStatus::Unknown;
                    tool_requests::put_callback(tx, &task)
                        .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
                }
                if case == "recoverable-pid" {
                    let mut task: CallbackTask<callback_record::CallbackObligation> =
                        tool_requests::get_callback(tx, &operation)
                            .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?
                            .unwrap();
                    task.payload.recovery_authorized = true;
                    tool_requests::put_callback(tx, &task)
                        .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
                }
                Ok::<_, WorkerError>(())
            })
            .await
            .unwrap();
        let mut peer = if case == "direct" {
            Peer::new()
        } else {
            Peer::supervised()
        };
        let mut worker = peer.worker(state.bridge.clone()).await;
        worker_barrier(&worker).await;
        assert!(
            peer.writes.try_recv().is_err(),
            "{case} must not authorize a write"
        );
        assert_eq!(
            callback_task(&state, GENERATION, 1).status,
            ToolRequestStatus::Unknown,
            "{case} stays uncertain"
        );
        worker.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn duplicate_callback_receipt_keeps_uncertainty_and_never_repeats_the_answer() {
    let state = State::new().await;
    let mut peer = Peer::supervised();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let cursor = peer.frame(request("duplicate-answer")).await;
    let answer = peer.next_write().await;
    answer
        .receipt
        .send(CommandReceipt::Written {
            remote_id: None,
            duplicate: true,
        })
        .unwrap_or_else(|_| panic!("the receipt has an owner"));
    let result = tokio::time::timeout(SHUTDOWN_GRACE, worker.completion())
        .await
        .unwrap();
    assert!(
        matches!(result, Err(WorkerError::CallbackUnknown)),
        "accepted does not prove stdin written: {result:?}"
    );
    assert_eq!(
        callback_task(&state, GENERATION, cursor).status,
        ToolRequestStatus::Unknown
    );
    assert!(peer.writes.try_recv().is_err());
}

#[tokio::test]
async fn missing_callback_receipt_recovers_exact_saved_answer_once_after_same_generation_reattach()
{
    let state = State::new().await;
    let mut peer = Peer::supervised();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let cursor = peer.frame(request("lost-receipt")).await;
    let answer = peer.next_write().await;
    worker_barrier(&worker).await;
    tokio::time::pause();
    tokio::time::advance(
        callback_record::WRITE_RECEIPT_TIMEOUT + std::time::Duration::from_secs(1),
    )
    .await;
    // The adapter still owns the response, so its absence cannot prove NotSent.
    let result = worker.completion().await;
    tokio::time::resume();
    assert!(
        matches!(result, Err(WorkerError::CallbackUnknown)),
        "missing receipt stays uncertain: {result:?}"
    );
    assert_eq!(
        callback_task(&state, GENERATION, cursor).status,
        ToolRequestStatus::Running
    );
    assert!(peer.writes.try_recv().is_err());
    let response = answer.value.clone();
    drop(answer);
    let mut retained = Peer::supervised();
    let mut resumed = retained.worker(state.bridge.clone()).await;
    let reply = tokio::time::timeout(std::time::Duration::from_secs(5), retained.next_write())
        .await
        .expect("the first lost handoff recovers on the retained connection");
    assert_eq!(reply.value, response);
    reply
        .receipt
        .send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        })
        .unwrap_or_else(|_| panic!("the resumed receipt has an owner"));
    callbacks_retired(&state).await;
    worker_barrier(&resumed).await;
    assert_eq!(
        callback_task(&state, GENERATION, cursor).status,
        ToolRequestStatus::Completed
    );
    assert!(retained.writes.try_recv().is_err());
    resumed.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_second_lost_callback_receipt_stays_unknown_without_another_replay() {
    let state = State::new().await;
    journal(&state, vec![entry(GENERATION, 1, &request("second-loss"))]).await;
    let operation = callback_record::operation(SCOPE, GENERATION, 1);
    let saved = saved_response(
        &state,
        &operation,
        json!({"id":"second-loss","result":{"success":true}}),
    )
    .await;
    state
        .bridge
        .store
        .transaction_with(move |tx| store_submitted(tx, &operation, saved))
        .await
        .unwrap();
    let mut peer = Peer::supervised();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let answer = peer.next_write().await;
    worker_barrier(&worker).await;
    tokio::time::pause();
    tokio::time::advance(
        callback_record::WRITE_RECEIPT_TIMEOUT + std::time::Duration::from_secs(1),
    )
    .await;
    let result = worker.completion().await;
    tokio::time::resume();
    assert!(matches!(result, Err(WorkerError::CallbackUnknown)));
    assert_eq!(
        callback_task(&state, GENERATION, 1).status,
        ToolRequestStatus::Unknown
    );
    assert!(
        callback_reply_accounts(&state.bridge.store.test_reader().unwrap(), "", 10)
            .unwrap()
            .is_empty(),
        "an exhausted retry never starts another account attachment"
    );
    drop(answer);
    let mut retained = Peer::supervised();
    let mut resumed = retained.worker(state.bridge.clone()).await;
    worker_barrier(&resumed).await;
    assert!(
        retained.writes.try_recv().is_err(),
        "the retry budget is durable"
    );
    assert_eq!(
        callback_task(&state, GENERATION, 1).status,
        ToolRequestStatus::Unknown
    );
    resumed.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_direct_callback_receipt_timeout_never_authorizes_replay() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let cursor = peer.frame(request("direct-loss")).await;
    let answer = peer.next_write().await;
    worker_barrier(&worker).await;
    tokio::time::pause();
    tokio::time::advance(
        callback_record::WRITE_RECEIPT_TIMEOUT + std::time::Duration::from_secs(1),
    )
    .await;
    let result = worker.completion().await;
    tokio::time::resume();
    assert!(matches!(result, Err(WorkerError::CallbackUnknown)));
    assert_eq!(
        callback_task(&state, GENERATION, cursor).status,
        ToolRequestStatus::Unknown
    );
    drop(answer);
    assert!(unsent(&state).await.is_empty());
}

#[tokio::test]
async fn a_late_not_sent_receipt_does_not_reopen_explicit_callback_uncertainty() {
    let state = State::new().await;
    journal(
        &state,
        vec![entry(GENERATION, 1, &request("late-not-sent"))],
    )
    .await;
    let operation = callback_record::operation(SCOPE, GENERATION, 1);
    submit_response(
        &state,
        &operation,
        json!({"id":"late-not-sent","result":{"success":true}}),
    )
    .await;
    callback_record::written(&state.bridge.store, &operation, WriteOutcome::Unknown)
        .await
        .unwrap();
    callback_record::written(&state.bridge.store, &operation, WriteOutcome::NotSent)
        .await
        .unwrap();
    assert_eq!(
        callback_task(&state, GENERATION, 1).status,
        ToolRequestStatus::Unknown
    );
    assert!(unsent(&state).await.is_empty());
    // Missing response provenance cannot reopen a held current task.
    state
        .bridge
        .store
        .transaction_with(move |tx| {
            let mut task: CallbackTask<callback_record::CallbackObligation> =
                tool_requests::get_callback(tx, &operation)
                    .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?
                    .unwrap();
            task.payload.submitted = None;
            tool_requests::put_callback(tx, &task)
                .map_err(|error| WorkerError::PreparationRefused(error.to_string()))
        })
        .await
        .unwrap();
    assert!(
        unsent(&state).await.is_empty(),
        "Unknown alone never permits another handler dispatch"
    );
}

#[tokio::test]
async fn managed_saved_callback_cannot_resume_a_stopped_changed_or_deleted_actor() {
    for case in [
        "stopped",
        "epoch",
        "account",
        "deleted",
        "provider",
        "root",
        "lead",
        "role",
        "parent",
        "execution",
        "move",
        "move-before",
        "transfer",
        "lazy",
    ] {
        for captured in [false, true] {
            let state = State::new().await;
            let actor = state.agent.clone();
            let epoch = state.epoch;
            state.bridge.store.transaction(move |tx| {
                tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.status','running','$.autoWake',json('true'),'$.inFlight',json('true'),'$.provider','claude','$.rootId',?1,'$.threadId','managed-thread','$.turnId','managed-turn','$.parentId',null,'$.role','implementer','$.isLead',json('true'),'$.executionMode',null,'$.executionMove',json('false'),'$.startAttempt',json(?2)) WHERE id=?1",
                    rusqlite::params![actor,json!({"connectionGeneration":GENERATION,"connectionId":"original-connection"}).to_string()])?;
                Ok(())
            }).await.unwrap();
            let call = json!({"id":"managed-reply","method":"item/tool/call","params":{
                "threadId":"managed-thread","turnId":"managed-turn","callId":"saved-spawn","tool":"orchestration_spawn","arguments":{}}});
            journal(&state, vec![entry(GENERATION, 1, &call)]).await;
            let operation = callback_record::operation(SCOPE, GENERATION, 1);
            let mut saved = saved_response(&state,&operation,json!({"id":"managed-reply","result":{"existingChild":"already-created","success":true}})).await;
            if captured {
                saved["authority"] = json!({"context":{"agent":state.agent,"account":"default","connection":"original-connection",
                    "stream_id":"default","generation":GENERATION,"epoch":epoch,"thread":"managed-thread","turn":"managed-turn"},"provider":"claude","root":state.agent,
                    "parent":null,"role":"implementer","isLead":true,"executionMode":null,"moving":false});
                if case == "move-before" {
                    saved["authority"]["moving"] = json!(true);
                }
            }
            let actor = state.agent.clone();
            state
                .bridge
                .store
                .transaction_with(move |tx| {
                    store_submitted(tx, &operation, saved)?;
                    let mutation = match case {
                        "stopped" => {
                            "record=json_set(record,'$.status','paused','$.autoWake',json('false'))"
                                .to_owned()
                        }
                        "epoch" => format!("record=json_set(record,'$.epoch',{})", epoch + 1),
                        "account" => "record=json_set(record,'$.accountKey','other')".to_owned(),
                        "deleted" => "record=json_set(record,'$.deletedAt',1)".to_owned(),
                        "provider" => "record=json_set(record,'$.provider','codex')".to_owned(),
                        "root" => "record=json_set(record,'$.rootId','another-root')".to_owned(),
                        "lead" => "record=json_set(record,'$.isLead',json('false'))".to_owned(),
                        "role" => "record=json_set(record,'$.role','reviewer')".to_owned(),
                        "parent" => {
                            "record=json_set(record,'$.parentId','another-parent')".to_owned()
                        }
                        "execution" => "record=json_set(record,'$.executionMode','vm')".to_owned(),
                        "move" | "move-before" => {
                            "record=json_set(record,'$.executionMove',json('true'))".to_owned()
                        }
                        "transfer" => "record=json_set(record,'$.accountTransferId','pending-transfer')".to_owned(),
                        "lazy" => "record=json_set(record,'$.lazyAccountTransfer',json('{\"id\":\"pending-transfer\"}'))".to_owned(),
                        _ => unreachable!(),
                    };
                    tx.execute(
                        &format!("UPDATE runtime_agents SET {mutation} WHERE id=?1"),
                        [actor],
                    )?;
                    Ok::<_, WorkerError>(())
                })
                .await
                .unwrap();
            let mut peer = Peer::supervised();
            let mut worker = peer.worker(state.bridge.clone()).await;
            worker_barrier(&worker).await;
            assert!(
                peer.writes.try_recv().is_err(),
                "{case}, captured={captured}: no reply or new turn"
            );
            assert_eq!(
                callback_task(&state, GENERATION, 1).status,
                ToolRequestStatus::Unknown,
                "{case}, captured={captured}"
            );
            worker.shutdown().await.unwrap();
        }
    }
}

#[tokio::test]
async fn managed_callback_first_handoff_captures_the_current_actor_authority() {
    let state = State::new().await;
    let mut peer = Peer::supervised();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    worker_barrier(&worker).await;
    let actor = state.record().await;
    let cursor = peer
        .frame(
            json!({"id":"capture-authority","method":"item/tool/call","params":{
        "threadId":actor["threadId"],"turnId":actor["turnId"],"callId":"capture-authority",
        "tool":"unsupported-fixture-tool","arguments":{}}}),
        )
        .await;
    let answer = tokio::time::timeout(std::time::Duration::from_secs(5), peer.next_write())
        .await
        .expect("an active actor can receive its first callback answer");
    let operation = callback_record::operation(SCOPE, GENERATION, cursor);
    let submitted = state
        .bridge
        .store
        .transaction_with(move |tx| {
            let task: CallbackTask<callback_record::CallbackObligation> =
                tool_requests::get_callback(tx, &operation)
                    .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?
                    .unwrap();
            Ok::<_, WorkerError>(
                task.payload
                    .submitted
                    .map(|submitted| serde_json::to_value(submitted).unwrap()),
            )
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(submitted["authority"]["context"]["agent"], actor["id"]);
    assert_eq!(submitted["authority"]["context"]["epoch"], actor["epoch"]);
    assert_eq!(submitted["authority"]["context"]["turn"], actor["turnId"]);
    answer
        .receipt
        .send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        })
        .unwrap_or_else(|_| panic!("the first reply receipt has an owner"));
    callbacks_retired(&state).await;
    worker_barrier(&worker).await;
    assert_eq!(
        callback_task(&state, GENERATION, cursor).status,
        ToolRequestStatus::Completed
    );
    worker.shutdown().await.unwrap();
}

/// The provider process that sent the request is gone; nobody waits for it.
#[tokio::test]
async fn a_request_of_an_ended_generation_is_retired_without_an_answer() {
    const ENDED: u64 = 5;
    let state = State::new().await;
    journal(&state, vec![entry(ENDED, 1, &request("callback"))]).await;
    assert_eq!(
        callback_task(&state, ENDED, 1).status,
        ToolRequestStatus::Queued
    );
    let mut peer = Peer::identity("default", ENDED + 1, None);
    let mut worker = peer.worker(state.bridge.clone()).await;
    worker_barrier(&worker).await;
    assert!(peer.writes.try_recv().is_err());
    assert_eq!(
        callback_task(&state, ENDED, 1).status,
        ToolRequestStatus::Completed
    );
    worker.shutdown().await.unwrap();
}

/// Send frames, reading ACKs and receipting every provider write meanwhile, so
/// the fake provider never waits on the worker. Written answers' request ids
/// go to `answered`.
async fn flood(
    peer: &mut Peer,
    acked: &mut u64,
    answered: &mut Vec<String>,
    frames: impl IntoIterator<Item = Value>,
) -> u64 {
    let mut last = peer.cursor;
    for frame in frames {
        peer.cursor += 1;
        last = peer.cursor;
        let entry = journal_frame(&peer.clock, peer.generation, last, &frame);
        let slot = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                tokio::select! {
                    slot = peer.entries.reserve() => break slot.expect("session reads events"),
                    ack = peer.acks.recv() => *acked = (*acked).max(ack.expect("ack lane").0),
                    wire = peer.writes.recv() => {
                        let wire = wire.expect("provider writes");
                        answered.push(wire.value["id"].as_str().unwrap_or_default().to_owned());
                        wire.receipt
                            .send(CommandReceipt::Written { remote_id: None, duplicate: false })
                            .ok()
                            .unwrap();
                    }
                }
            }
        })
        .await
        .expect("the session keeps reading provider events");
        slot.send(entry);
    }
    last
}

async fn acknowledged_through(peer: &mut Peer, acked: &mut u64, cursor: u64) {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while *acked < cursor {
            *acked = (*acked).max(peer.acks.recv().await.expect("ack lane").0);
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the journal is acknowledged through {cursor}; acked {acked}"));
}

/// A request whose answer is not produced yet must not hold later events,
/// other agents' work or a new thread.
#[tokio::test]
async fn an_unanswered_request_does_not_stop_events_or_new_work() {
    let state = State::new().await;
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut record =
                        AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                    record.extra.remove("threadId");
                    AgentRepository::put(tx, &record)
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let mut acked = 0;
    let mut answered = Vec::new();
    // The handler of this tool call does not answer until the gate opens.
    let gate = Arc::clone(&state.bridge.faults.callback_gate)
        .write_owned()
        .await;
    let tool_call = json!({"id":"held","method":"item/tool/call","params":{}});
    let first = flood(&mut peer, &mut acked, &mut answered, [tool_call]).await;
    let last = flood(
        &mut peer,
        &mut acked,
        &mut answered,
        (0..EVENTS_BEHIND_HELD_ANSWER).map(|_| usage()),
    )
    .await;
    assert!(last > first + crate::provider_session_task::MAX_UNSETTLED as u64);
    acknowledged_through(&mut peer, &mut acked, last).await;
    assert!(answered.is_empty(), "the held request has no answer yet");
    assert_eq!(
        callback_task(&state, GENERATION, first).status,
        ToolRequestStatus::Queued
    );

    // Another turn and a new thread complete behind the unanswered request.
    state.wake(&worker).await;
    let thread = peer.write("thread/start").await;
    peer.reply(&thread, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    let turn = peer.write("turn/start").await;
    peer.accepted(&turn).await;
    peer.completed().await;

    drop(gate);
    let answer = peer.next_write().await;
    assert_eq!(answer.value["id"], "held");
    answer
        .receipt
        .send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        })
        .ok()
        .unwrap();
    callbacks_retired(&state).await;
    worker.shutdown().await.unwrap();
}

/// A burst larger than either callback limit is accepted and answered once.
/// Immediate receipts exercise throughput, without holding either lane full.
#[tokio::test]
async fn a_burst_beyond_the_callback_limits_is_answered_once_each() {
    let burst = MAX_RUNNING_CALLBACKS.max(MAX_QUEUED_CALLBACK_ANSWERS) + 1;
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let mut acked = 0;
    let mut answered = Vec::new();
    let mut last = 0;
    for index in 0..burst {
        last = flood(
            &mut peer,
            &mut acked,
            &mut answered,
            [request(&format!("callback-{index}"))],
        )
        .await;
    }
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while acked < last || answered.len() < burst {
            tokio::select! {
                ack = peer.acks.recv() => acked = acked.max(ack.expect("ack lane").0),
                wire = peer.writes.recv() => {
                    let wire = wire.expect("provider writes");
                    answered.push(wire.value["id"].as_str().unwrap().to_owned());
                    wire.receipt
                        .send(CommandReceipt::Written { remote_id: None, duplicate: false })
                        .ok()
                        .unwrap();
                }
            }
        }
    })
    .await
    .expect("every request is acknowledged and answered");
    let mut expected = (0..burst)
        .map(|index| format!("callback-{index}"))
        .collect::<Vec<_>>();
    expected.sort();
    answered.sort();
    assert_eq!(answered, expected, "every request is answered exactly once");
    callbacks_retired(&state).await;
    worker_barrier(&worker).await;
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

/// Accept a record directly, as an earlier worker run would have left it.
async fn leave_record(
    state: &State,
    generation: u64,
    cursor: u64,
    frame: &Value,
    actor: Option<(&str, i64)>,
) {
    let entry = entry(generation, cursor, frame);
    let scope = actor.map_or_else(
        || SCOPE.to_owned(),
        |(actor, _)| format!("default:vm:{actor}"),
    );
    let actor = actor.map(|(actor, epoch)| (actor.to_owned(), epoch));
    state
        .bridge
        .store
        .transaction_with(move |tx| {
            let actor = actor
                .as_ref()
                .map(|(actor, epoch)| (actor.as_str(), *epoch));
            callback_record::accept(tx, "default", &scope, actor, &entry)
        })
        .await
        .expect("leave record");
}

/// The same cursor in two provider generations names two records. The held
/// handler of the current one must survive the retirement of the old one.
#[tokio::test]
async fn the_same_cursor_in_two_generations_is_two_records() {
    let state = State::new().await;
    let tool_call = json!({"id":"current","method":"item/tool/call","params":{}});
    journal(&state, vec![entry(2, 1, &tool_call)]).await;
    let gate = Arc::clone(&state.bridge.faults.callback_gate)
        .write_owned()
        .await;
    let mut peer = Peer::identity("default", 2, None);
    let mut worker = peer.worker(state.bridge.clone()).await;
    worker_barrier(&worker).await;
    // An ended generation's record with the same cursor appears next to it.
    leave_record(&state, 1, 1, &request("old"), None).await;
    peer.frame(tool_call).await;
    let mut changes = state.commits();
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while callback_task(&state, 1, 1).status != ToolRequestStatus::Completed {
            changes.changed().await.expect("callback retirement commit");
        }
    })
    .await
    .expect("the scan retires the old record");
    drop(gate);
    let answer = peer.next_write().await;
    assert_eq!(answer.value["id"], "current");
    answer
        .receipt
        .send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        })
        .ok()
        .unwrap();
    callbacks_retired(&state).await;
    assert!(
        peer.writes.try_recv().is_err(),
        "each request is answered once"
    );
    worker.shutdown().await.unwrap();
}

/// An uncertain answer of an ended generation is retired, never sent.
#[tokio::test]
async fn a_submitted_record_of_an_ended_generation_is_retired_unsent() {
    let state = State::new().await;
    let mut peer = Peer::identity("default", 6, None);
    let mut worker = peer.worker(state.bridge.clone()).await;
    leave_record(&state, 5, 1, &request("old"), None).await;
    let operation = callback_record::operation(SCOPE, 5, 1);
    submit_response(
        &state,
        &operation,
        json!({"id":"old","result":{"success":true}}),
    )
    .await;
    peer.frame(request("current")).await;
    let answer = peer.next_write().await;
    assert_eq!(answer.value["id"], "current");
    answer
        .receipt
        .send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        })
        .ok()
        .unwrap();
    callbacks_retired(&state).await;
    assert_eq!(
        callback_task(&state, 5, 1).status,
        ToolRequestStatus::Completed
    );
    assert!(
        peer.writes.try_recv().is_err(),
        "the ended generation's answer is not sent"
    );
    worker.shutdown().await.unwrap();
}

/// The guest provider outlives a server restart and its actor reattaches, so
/// a record of a VM actor waits at startup; only a stale epoch retires it.
#[tokio::test]
async fn a_vm_actor_record_waits_for_reattach_and_a_stale_epoch_retires() {
    let state = State::new().await;
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut record =
                        AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                    record.extra.insert("executionMode".into(), json!("vm"));
                    AgentRepository::put(tx, &record)
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    let actor = state.agent.as_str();
    leave_record(
        &state,
        GENERATION,
        1,
        &request("live"),
        Some((actor, state.epoch)),
    )
    .await;
    leave_record(
        &state,
        GENERATION,
        2,
        &request("stale"),
        Some((actor, state.epoch + 1)),
    )
    .await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    // Any journaled request runs a scan.
    peer.frame(request("host")).await;
    let host = peer.next_write().await;
    assert_eq!(host.value["id"], "host");
    host.receipt
        .send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        })
        .ok()
        .unwrap();
    worker_barrier(&worker).await;
    let scope = format!("default:vm:{actor}");
    let task_of = |cursor| {
        let db = state.bridge.store.test_reader().unwrap();
        tool_requests::get_callback::<callback_record::CallbackObligation>(
            &db,
            &callback_record::operation(&scope, GENERATION, cursor),
        )
        .unwrap()
        .unwrap()
    };
    assert_eq!(
        task_of(1).status,
        ToolRequestStatus::Queued,
        "the current epoch waits for its actor"
    );
    assert_eq!(
        task_of(2).status,
        ToolRequestStatus::Completed,
        "the stale epoch is retired"
    );
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}
