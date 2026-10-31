use serde_json::json;
use std::collections::VecDeque;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use studio_runtime_providers::{Method, RequestId, RequestTicket};
use studio_runtime_services::account_session_runtime::SessionOutcome as ReceiptOutcome;
use studio_runtime_services::account_session_runtime::{
    AccountSessionFuture, AccountSessionPort, ConnectionScope, ProviderRequest, ProviderResponse,
    RequestCertainty, SessionReceipt,
};
use studio_runtime_services::account_worker::retained::RequestActor;
use studio_runtime_services::callback_runtime::{
    CallbackReplyFuture, CallbackReplyPort, ReplyOutcome, ReplyReceipt,
};
use studio_runtime_services::voice;
use studio_runtime_services::voice::account_session::AccountSessionVoicePort;
use studio_runtime_services::voice::policy::{
    self, SessionSignal, VoiceSession, VoiceSessionRow, VoiceSessionState,
};
use studio_runtime_services::voice::provider::VoiceCallbackActor;
use studio_runtime_services::voice::provider::notification_requires_stop;
use studio_runtime_services::voice::runtime::{
    VoiceConnectionScope as VoiceProviderContext, VoiceFuture as ProviderFuture,
    VoicePrepareFailure, VoiceReply, VoiceRequestPort as VoiceProviderPort, VoiceRequestReceipt,
    VoiceRpcId, VoiceWriteCertainty,
};
use studio_runtime_store::RuntimeStore;

fn connecting_session() -> VoiceSession {
    VoiceSession::from_row(VoiceSessionRow {
        id: "voice-session".into(),
        agent: "agent".into(),
        created: 1.0,
        ended: None,
        offer: Some("offer".into()),
        answer: None,
        native_thread: Some("thread".into()),
        account_key: Some("account".into()),
        connection_id: Some("connection".into()),
        state: Some("connecting".into()),
        error: None,
    })
    .expect("valid scripted voice session")
}

#[tokio::test]
async fn provider_error_callback_stops_and_keeps_failure_terminal() {
    let session = connecting_session();
    let transition = policy::transition_session(
        &session,
        SessionSignal::ProviderFailed(Some("provider failed".into())),
        2.0,
    )
    .expect("provider error transition");
    assert!(transition.stop_provider);
    assert_eq!(transition.session.state(), Some(VoiceSessionState::Failed));
    assert_eq!(transition.session.error(), Some("provider failed"));
}

#[tokio::test]
async fn session_machine_distinguishes_admitted_stop_rejection_and_disconnect() {
    let session = connecting_session();
    let answered = policy::transition_session(
        &session,
        SessionSignal::AnswerReceived("answer".into()),
        1.5,
    )
    .expect("provider answer transition");
    assert_eq!(answered.session.state(), Some(VoiceSessionState::Ready));

    let failed = policy::transition_session(
        &session,
        SessionSignal::ProviderFailed(Some("provider failed".into())),
        1.75,
    )
    .expect("provider failure transition");
    let closed_after_failure = policy::transition_session(
        &failed.session,
        SessionSignal::PeerClosed(Some("transport closed".into())),
        1.9,
    )
    .expect("close after provider failure");
    assert_eq!(
        closed_after_failure.session.state(),
        Some(VoiceSessionState::Failed)
    );
    let stop_after_failure = policy::transition_session(
        &failed.session,
        SessionSignal::StopRequested {
            start_dispatched: true,
        },
        1.95,
    )
    .expect("stop after provider failure");
    assert!(stop_after_failure.stop_provider);
    assert_eq!(
        stop_after_failure.session.state(),
        Some(VoiceSessionState::Failed)
    );

    let admitted = policy::transition_session(
        &session,
        SessionSignal::StopRequested {
            start_dispatched: true,
        },
        2.0,
    )
    .expect("admitted stop transition");
    assert!(admitted.stop_provider);
    assert_eq!(admitted.session.state(), Some(VoiceSessionState::Stopping));

    let not_admitted = policy::transition_session(
        &session,
        SessionSignal::StopRequested {
            start_dispatched: false,
        },
        2.0,
    )
    .expect("not admitted stop transition");
    assert!(!not_admitted.stop_provider);
    assert_eq!(not_admitted.session.state(), Some(VoiceSessionState::Ended));
    assert_eq!(not_admitted.session.ended(), Some(2.0));
    let late_answer = policy::transition_session(
        &not_admitted.session,
        SessionSignal::AnswerReceived("late answer".into()),
        2.5,
    )
    .expect("late provider answer transition");
    assert_eq!(late_answer.session.state(), Some(VoiceSessionState::Ended));
    assert_eq!(late_answer.session.ended(), Some(2.0));

    let rejected = policy::transition_session(
        &admitted.session,
        SessionSignal::StopRejected("write unknown".into()),
        3.0,
    )
    .expect("rejected stop transition");
    assert!(!rejected.stop_provider);
    assert_eq!(rejected.session.state(), Some(VoiceSessionState::Unknown));
    assert_eq!(rejected.session.error(), Some("write unknown"));

    let disconnected =
        policy::transition_session(&session, SessionSignal::ProviderDisconnected, 4.0)
            .expect("disconnect transition");
    assert!(!disconnected.stop_provider);
    assert_eq!(disconnected.session.state(), Some(VoiceSessionState::Lost));
    assert_eq!(disconnected.session.ended(), Some(4.0));
}

#[tokio::test]
async fn realtime_error_notifications_require_stop_rpc() {
    assert!(notification_requires_stop(&json!({
        "method": "thread/realtime/error",
        "params": {"threadId": "thread"}
    })));
    assert!(!notification_requires_stop(&json!({
        "method": "thread/realtime/closed",
        "params": {"threadId": "thread"}
    })));
}

async fn seed_callback_session(store: &RuntimeStore, id: &str, thread: &str, connection: &str) {
    let id = id.to_owned();
    let thread = thread.to_owned();
    let connection = connection.to_owned();
    store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO voice_sessions(id,agent,created,native_thread,account_key,connection_id,state) \
                 VALUES(?1,'agent',1,?2,'account',?3,'connecting')",
                rusqlite::params![id, thread, connection],
            )?;
            Ok(())
        })
        .await
        .expect("seed callback session");
}

#[tokio::test]
async fn callback_actor_prefers_canonical_transcript_over_fallback_delta() {
    let (_root, store) = seeded_store().await;
    seed_callback_session(
        &store,
        "canonical-session",
        "canonical-thread",
        "voice-connection",
    )
    .await;
    let actor = VoiceCallbackActor::default();
    let started = json!({
        "method":"thread/realtime/item/started",
        "params":{"threadId":"canonical-thread","item":{"type":"transcriptSegment","realtimeSessionId":"canonical-session"}}
    });
    assert!(
        actor
            .handle_notification(&store, "account", "voice-connection", &started, 2.0)
            .await
            .expect("canonical starts")
    );
    let delta = json!({
        "method":"thread/realtime/transcript/delta",
        "params":{"threadId":"canonical-thread","realtimeSessionId":"canonical-session","role":"assistant","delta":"fallback"}
    });
    actor
        .handle_notification(&store, "account", "voice-connection", &delta, 3.0)
        .await
        .expect("delta handled");
    let completed = json!({
        "method":"thread/realtime/item/completed",
        "params":{"threadId":"canonical-thread","item":{"type":"transcriptSegment","id":"item-1","realtimeSessionId":"canonical-session","role":"assistant","text":"canonical transcript"}}
    });
    actor
        .handle_notification(&store, "account", "voice-connection", &completed, 4.0)
        .await
        .expect("canonical completes");
    let closed = json!({
        "method":"thread/realtime/closed",
        "params":{"threadId":"canonical-thread","realtimeSessionId":"canonical-session"}
    });
    actor
        .handle_notification(&store, "account", "voice-connection", &closed, 5.0)
        .await
        .expect("close cleans actor state");

    let connection = store.test_reader().expect("read transcript records");
    let rows = connection
        .prepare("SELECT text FROM voice_records WHERE session='canonical-session' ORDER BY seq")
        .expect("voice record query")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("voice records")
        .collect::<Result<Vec<_>, _>>()
        .expect("records decoded");
    assert_eq!(rows, ["canonical transcript"]);
}

#[tokio::test]
async fn callback_actor_flushes_fallback_tail_when_provider_closes() {
    let (_root, store) = seeded_store().await;
    seed_callback_session(
        &store,
        "fallback-session",
        "fallback-thread",
        "voice-connection",
    )
    .await;
    let actor = VoiceCallbackActor::default();
    let delta = json!({
        "method":"thread/realtime/transcript/delta",
        "params":{"threadId":"fallback-thread","realtimeSessionId":"fallback-session","role":"user","delta":"partial words"}
    });
    actor
        .handle_notification(&store, "account", "voice-connection", &delta, 2.0)
        .await
        .expect("delta buffered");
    let closed = json!({
        "method":"thread/realtime/closed",
        "params":{"threadId":"fallback-thread","realtimeSessionId":"fallback-session","reason":"network"}
    });
    actor
        .handle_notification(&store, "account", "voice-connection", &closed, 3.0)
        .await
        .expect("close flushes fallback");

    let connection = store.test_reader().expect("read flushed transcript");
    let (text, payload): (String, String) = connection
        .query_row(
            "SELECT text,payload FROM voice_records WHERE session='fallback-session'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("flushed transcript row");
    assert_eq!(text, "partial words");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&payload).expect("payload")["partial"],
        true
    );
}

#[tokio::test]
async fn callback_actor_discards_fallback_tail_when_transcript_is_finalized_empty() {
    let (_root, store) = seeded_store().await;
    seed_callback_session(
        &store,
        "discard-session",
        "discard-thread",
        "voice-connection",
    )
    .await;
    let actor = VoiceCallbackActor::default();
    let delta = json!({
        "method":"thread/realtime/transcript/delta",
        "params":{"threadId":"discard-thread","realtimeSessionId":"discard-session","role":"user","delta":"discard me"}
    });
    actor
        .handle_notification(&store, "account", "voice-connection", &delta, 2.0)
        .await
        .expect("delta buffered");
    let done = json!({
        "method":"thread/realtime/transcript/done",
        "params":{"threadId":"discard-thread","realtimeSessionId":"discard-session","role":"user","text":""}
    });
    actor
        .handle_notification(&store, "account", "voice-connection", &done, 3.0)
        .await
        .expect("empty done clears tail");
    let closed = json!({
        "method":"thread/realtime/closed",
        "params":{"threadId":"discard-thread","realtimeSessionId":"discard-session"}
    });
    actor
        .handle_notification(&store, "account", "voice-connection", &closed, 4.0)
        .await
        .expect("close completes");
    let connection = store.test_reader().expect("read transcript records");
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM voice_records WHERE session='discard-session'",
            [],
            |row| row.get(0),
        )
        .expect("record count");
    assert_eq!(count, 0);
}

#[tokio::test]
async fn callback_actor_refuses_deltas_over_the_record_text_bound() {
    let (_root, store) = seeded_store().await;
    seed_callback_session(
        &store,
        "bounded-session",
        "bounded-thread",
        "voice-connection",
    )
    .await;
    let actor = VoiceCallbackActor::default();
    let delta = json!({
        "method":"thread/realtime/transcript/delta",
        "params":{"threadId":"bounded-thread","realtimeSessionId":"bounded-session","role":"assistant","delta":"x".repeat(100_001)}
    });
    assert!(
        actor
            .handle_notification(&store, "account", "voice-connection", &delta, 2.0)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn callback_actor_flushes_fallback_tail_and_loses_session_on_disconnect() {
    let (_root, store) = seeded_store().await;
    seed_callback_session(
        &store,
        "disconnect-session",
        "disconnect-thread",
        "disconnect-connection",
    )
    .await;
    let actor = VoiceCallbackActor::default();
    let delta = json!({
        "method":"thread/realtime/transcript/delta",
        "params":{"threadId":"disconnect-thread","realtimeSessionId":"disconnect-session","role":"assistant","delta":"last words"}
    });
    actor
        .handle_notification(&store, "account", "disconnect-connection", &delta, 2.0)
        .await
        .expect("delta buffered");
    assert_eq!(
        actor
            .handle_disconnect(&store, "account", "disconnect-connection", 3.0)
            .await
            .expect("disconnect closes actor"),
        1
    );
    let connection = store.test_reader().expect("read disconnected state");
    let (state, text): (String, String) = connection
        .query_row(
            "SELECT s.state,r.text FROM voice_sessions s JOIN voice_records r ON r.session=s.id WHERE s.id='disconnect-session'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("lost session and flushed record");
    assert_eq!(state, "lost");
    assert_eq!(text, "last words");
}

#[allow(clippy::disallowed_methods)]
async fn seeded_store() -> (tempfile::TempDir, RuntimeStore) {
    let root = fixture_directory();
    let state = root.path().join("state");
    fs::create_dir(&state).expect("disposable state root");
    crate::media_auth_fixtures::write_state(
        &state,
        include_str!("voice_provider/initial-state.json"),
    );
    let store = RuntimeStore::open(Some(&state))
        .await
        .expect("open voice fixture store");
    store
        .transaction(|connection| {
            studio_runtime_store::receipt_schema::create(connection)?;
            for agent in ["agent", "other-agent"] {
                let id = studio_runtime_store::AgentId::from(agent);
                let record = studio_runtime_store::AgentRecord {
                    in_flight: None,
        account_key: "account".into(),
        thread_id: Some("thread".into()),
        turn_id: None,
        auto_wake: None,
        usage_resume_enabled: None,
        execution_mode: None,
        turn_epoch: None,
        title_job: None,
                    start_outcome_hold: None,
                    fired_events: Default::default(),
                    native_review_thread: None,
                    vm_launch: None,
                    credential_copy: None,
                    vm_auth_bootstrap: None,
                    native_release: None,
                    id: id.clone(), root_id: id, epoch: 0,
                    status: studio_runtime_store::AgentStatus::Idle,
                    is_lead: Some(true),
                    extra: [
                        ("provider".into(), serde_json::to_value(studio_runtime_schema::ProviderKind::Codex).expect("provider")),
                    ].into(),
                    ..studio_runtime_store::AgentRecord::test_defaults()
                };
                studio_runtime_store::AgentRepository::put(connection, &record)?;
            }
            for (session, agent) in [
                ("session:one", "agent"),
                ("session", "agent"),
                ("owned-by-agent", "agent"),
            ] {
                connection.execute(
                    "INSERT INTO voice_sessions(id,agent,created,state) VALUES(?1,?2,1,NULL)",
                    rusqlite::params![session, agent],
                )?;
            }
            connection.execute(
                "INSERT INTO voice_speech_requests(id,session,record_id,state,error) VALUES('owned-by-agent:private-record','owned-by-agent','private-record','unknown','foreign detail')",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("seed agent and session rows");
    studio_runtime_services::account_worker::attach_provider_stream_for_test(
        &store,
        "account",
        None,
        1,
        "receipt-matrix-connection",
    )
    .await
    .expect("attach fixture provider");
    (root, store)
}

fn fixture_directory() -> tempfile::TempDir {
    tempfile::tempdir().expect("voice fixture directory")
}

async fn attach_delete_scope(store: &RuntimeStore) {
    studio_runtime_services::account_worker::attach_provider_stream_for_test(
        store, "account", None, 7, "7",
    )
    .await
    .expect("attach voice provider");
}

fn save_scope(tx: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
    let agent = studio_runtime_store::AgentRepository::get(
        tx,
        &studio_runtime_store::AgentId::from("agent"),
    )
    .expect("read fixture agent")
    .expect("fixture agent exists");
    let scope = studio_runtime_store::account_state::current_provider_scope(tx, &agent)
        .expect("fixture provider is attached");
    let encoded = serde_json::to_string(&scope).expect("encode provider scope");
    tx.execute(
        "UPDATE voice_sessions SET provider_scope=?1 WHERE connection_id=?2",
        rusqlite::params![encoded, scope.connection],
    )?;
    Ok(())
}

#[tokio::test]
async fn delete_voice_stop_intent_is_staged_before_voice_rows_are_removed() {
    let (_root, store) = seeded_store().await;
    attach_delete_scope(&store).await;
    let first = store
        .transaction_with_error(|transaction| -> Result<_, voice::provider::VoiceProviderError> {
            transaction.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.deletedAt',1) WHERE id='agent'",
                [],
            )?;
            transaction.execute(
                "INSERT INTO voice_sessions(id,agent,created,native_thread,account_key,connection_id,state) \
                 VALUES('delete-voice-session','agent',1,'thread','account','7','ready')",
                [],
            )?;
            save_scope(transaction)?;
            let intent = voice::provider::stage_delete_sessions(transaction, "agent")?;
            transaction.execute("DELETE FROM voice_sessions WHERE agent='agent'", [])?;
            Ok(intent)
        })
        .await
        .expect("deletion transaction stages voice stop before deleting its rows");
    assert_eq!(first.len(), 1);
    let first = &first[0];
    assert_eq!(first.session.agent_id, "agent");
    assert_eq!(first.session.epoch, 0);
    assert_eq!(first.session.session_id, "delete-voice-session");
    assert_eq!(first.session.generation, 7);
    let before = studio_runtime_store::outbox::Outbox::get::<studio_runtime_store::VoiceEffect>(
        &store.test_reader().expect("read deletion receipt"),
        &first.id,
    )
    .expect("read staged receipt")
    .expect("receipt exists");

    let replay = store
        .transaction_with_error(
            |transaction| -> Result<_, voice::provider::VoiceProviderError> {
                voice::provider::stage_delete_sessions(transaction, "agent")
            },
        )
        .await
        .expect("retry reuses the committed delete intent after voice rows are gone");
    assert!(
        replay.is_empty(),
        "deleted session cannot stage a second stop"
    );
    let connection = store.test_reader().expect("read staged deletion receipt");
    let after = studio_runtime_store::outbox::Outbox::get::<studio_runtime_store::VoiceEffect>(
        &connection,
        &first.id,
    )
    .expect("read receipt")
    .expect("durable voice delete intent");
    assert_eq!(after, before, "replay preserves the committed receipt");
    let result = after.payload;
    assert_eq!(result.agent, "agent");
    assert_eq!(result.epoch, 0);
    assert_eq!(result.session_id, "delete-voice-session");
    assert!(
        matches!(result.authority, studio_runtime_store::VoiceEffectAuthority::Admitted { scope, .. } if scope.connection == "7")
    );
}

#[tokio::test]
async fn eight_not_sent_delete_stops_leave_the_intent_durable_and_pending() {
    let (_root, store) = seeded_store().await;
    attach_delete_scope(&store).await;
    let intents = store
        .transaction_with_error(move |transaction| -> Result<_, voice::provider::VoiceProviderError> {
            transaction.execute(
                "INSERT INTO voice_sessions(id,agent,created,native_thread,account_key,connection_id,state) \
                 VALUES('delete-retry-session','agent',1,'thread','account','7','ready')",
                [],
            )?;
            save_scope(transaction)?;
            let intents = voice::provider::stage_delete_sessions(transaction, "agent")?;
            transaction.execute("DELETE FROM voice_sessions WHERE agent='agent'", [])?;
            Ok(intents)
        })
        .await
        .expect("stage voice stop before removing its session row");
    let store = Arc::new(store);
    let provider = Arc::new(ReceiptProvider {
        certainty: VoiceWriteCertainty::NotSent,
        calls: Mutex::new(Vec::new()),
    });
    assert_eq!(intents.len(), 1);
    let intent_id = intents[0].id.clone();

    for _ in 0..8 {
        let intent = voice::provider::pending_delete_intents(&store)
            .await
            .expect("load pending durable stop")
            .into_iter()
            .find(|intent| intent.id == intent_id)
            .expect("NotSent keeps the delete intent pending");
        assert_eq!(intent.session.session_id, "delete-retry-session");
        voice::provider::end_deleted_sessions_tracked(
            Arc::clone(&store),
            Arc::clone(&provider) as Arc<dyn VoiceProviderPort>,
            intent.id,
            intent.session,
            10.0,
        )
        .await
        .expect("persist each proven-unsent result for retry");
    }

    let still_pending = voice::provider::pending_delete_intents(&store)
        .await
        .expect("reload durable retry")
        .into_iter()
        .find(|intent| intent.id == intent_id)
        .expect("retry cap leaves the intent for a later startup/lifecycle pass");
    assert_eq!(still_pending.session.session_id, "delete-retry-session");
    let connection = store.test_reader().expect("read persisted retry state");
    let result = studio_runtime_store::outbox::Outbox::get::<studio_runtime_store::VoiceEffect>(
        &connection,
        &intent_id,
    )
    .expect("read receipt")
    .expect("delete intent remains in the receipt table");
    assert!(matches!(
        result.payload.authority,
        studio_runtime_store::VoiceEffectAuthority::Admitted {
            outcome: studio_runtime_schema::persistence::VoiceStopOutcome::NotSent,
            ..
        }
    ));
    let calls = provider.calls.lock().expect("provider call log");
    assert_eq!(calls.len(), 8);
    assert!(calls.iter().all(|(operation, method)| {
        operation == "voice:delete-stop:delete-retry-session"
            && *method == Method::ThreadRealtimeStop
    }));
}

#[tokio::test]
async fn proven_not_sent_delete_stop_retries_same_identity_and_completes_when_written() {
    let (_root, store) = seeded_store().await;
    attach_delete_scope(&store).await;
    let intents = store
        .transaction_with_error(|transaction| -> Result<_, voice::provider::VoiceProviderError> {
            transaction.execute(
                "INSERT INTO voice_sessions(id,agent,created,native_thread,account_key,connection_id,state) \
                 VALUES('delete-retry-written-session','agent',1,'thread','account','7','ready')",
                [],
            )?;
            save_scope(transaction)?;
            let intents = voice::provider::stage_delete_sessions(transaction, "agent")?;
            transaction.execute("DELETE FROM voice_sessions WHERE agent='agent'", [])?;
            Ok(intents)
        })
        .await
        .expect("stage voice stop before removing its session row");
    let store = Arc::new(store);
    assert_eq!(intents.len(), 1);
    let intent_id = intents[0].id.clone();
    let first = Arc::new(ReceiptProvider {
        certainty: VoiceWriteCertainty::NotSent,
        calls: Mutex::new(Vec::new()),
    });
    let pending = voice::provider::pending_delete_intents(&store)
        .await
        .expect("load initial delete intent")
        .into_iter()
        .find(|intent| intent.id == intent_id)
        .expect("delete intent exists");
    voice::provider::end_deleted_sessions_tracked(
        Arc::clone(&store),
        Arc::clone(&first) as Arc<dyn VoiceProviderPort>,
        pending.id,
        pending.session,
        10.0,
    )
    .await
    .expect("persist proven NotSent result");

    let retry = voice::provider::pending_delete_intents(&store)
        .await
        .expect("reload safe retry")
        .into_iter()
        .find(|intent| intent.id == intent_id)
        .expect("NotSent leaves the deletion intent pending");
    assert_eq!(retry.session.session_id, "delete-retry-written-session");
    let second = Arc::new(ReceiptProvider {
        certainty: VoiceWriteCertainty::Written,
        calls: Mutex::new(Vec::new()),
    });
    voice::provider::end_deleted_sessions_tracked(
        Arc::clone(&store),
        Arc::clone(&second) as Arc<dyn VoiceProviderPort>,
        retry.id,
        retry.session,
        11.0,
    )
    .await
    .expect("same operation succeeds on safe retry");

    let first_call = first.calls.lock().expect("first call log")[0].clone();
    let second_call = second.calls.lock().expect("retry call log")[0].clone();
    assert_eq!(first_call, second_call);
    assert_eq!(
        first_call.0,
        "voice:delete-stop:delete-retry-written-session"
    );
    let completed = voice::provider::pending_delete_intents(&store)
        .await
        .expect("reload completed deletion");
    assert!(
        completed.iter().all(|intent| intent.id != intent_id),
        "a written stop settles the intent and removes its receipt"
    );
    let connection = store.test_reader().expect("read completed receipt");
    let remaining: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM runtime_outbox WHERE id=?1",
            [&intent_id],
            |row| row.get(0),
        )
        .expect("count settled receipt");
    assert_eq!(remaining, 0);
}

#[tokio::test]
async fn empty_delete_intents_write_no_outbox() {
    let (_root, store) = seeded_store().await;
    let empty = store
        .transaction_with_error(|tx| voice::provider::stage_delete_sessions(tx, "agent"))
        .await
        .expect("empty intent");
    assert!(empty.is_empty());
    let reader = store.test_reader().expect("reader");
    assert!(
        studio_runtime_store::outbox::Outbox::for_owner::<studio_runtime_store::VoiceEffect>(
            &reader,
            studio_runtime_schema::persistence::OwnerKind::Agent,
            "agent",
            studio_runtime_schema::persistence::OutboxKind::VoiceDelete,
        )
        .expect("owner obligations")
        .is_empty()
    );
}

#[tokio::test]
async fn delete_holds_an_unknown_stop_and_sends_only_for_the_active_session() {
    use studio_runtime_schema::persistence::{
        OutboxKind, OutboxStatus, OwnerKind, VoiceStopOutcome,
    };
    use studio_runtime_store::outbox::Outbox;
    let (_root, store) = seeded_store().await;
    attach_delete_scope(&store).await;
    store.transaction(|tx| {
        tx.execute("INSERT INTO voice_sessions(id,agent,created,native_thread,account_key,connection_id,state) VALUES('unknown-session','agent',1,'thread','account','7','ready'),('active-session','agent',2,'thread','account','7','ready')", [])?;
        save_scope(tx)?;
        Ok(())
    }).await.expect("voice sessions");
    let store = Arc::new(store);
    let unknown = Arc::new(ReceiptProvider {
        certainty: VoiceWriteCertainty::Unknown,
        calls: Mutex::new(vec![]),
    });
    assert!(matches!(
        voice::provider::end(
            store.clone(),
            unknown,
            "agent".into(),
            "unknown-session".into(),
            1.0
        )
        .await,
        Err(voice::provider::VoiceProviderError::Unknown)
    ));
    let intents = store
        .transaction_with_error(|tx| -> Result<_, voice::provider::VoiceProviderError> {
            let intents = voice::provider::stage_delete_sessions(tx, "agent")?;
            tx.execute("DELETE FROM voice_sessions WHERE agent='agent'", [])?;
            tx.execute("DELETE FROM runtime_agents WHERE id='agent'", [])?;
            Ok(intents)
        })
        .await
        .expect("captured deletion");
    assert_eq!(intents.len(), 1);
    let intent = &intents[0];
    assert_eq!(intent.session.session_id, "active-session");
    let provider = Arc::new(ReceiptProvider {
        certainty: VoiceWriteCertainty::Written,
        calls: Mutex::new(vec![]),
    });
    for now in [10.0, 11.0] {
        voice::provider::end_deleted_sessions_tracked(
            store.clone(),
            provider.clone(),
            intent.id.clone(),
            intent.session.clone(),
            now,
        )
        .await
        .expect("active stop and stale retry");
    }
    assert_eq!(
        provider.calls.lock().expect("calls").as_slice(),
        &[(
            "voice:delete-stop:active-session".into(),
            Method::ThreadRealtimeStop
        )]
    );
    assert!(
        voice::provider::pending_delete_intents(&store)
            .await
            .expect("pending index")
            .is_empty()
    );
    let reader = store.test_reader().expect("reader");
    let held = Outbox::for_owner::<studio_runtime_store::VoiceEffect>(
        &reader,
        OwnerKind::Agent,
        "agent",
        OutboxKind::VoiceStop,
    )
    .expect("held rows");
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].status, OutboxStatus::Unknown);
    assert_eq!(held[0].payload.session_id, "unknown-session");
    assert!(matches!(
        held[0].payload.authority,
        studio_runtime_store::VoiceEffectAuthority::Admitted {
            outcome: VoiceStopOutcome::Unknown,
            ..
        }
    ));
}

#[tokio::test]
async fn rejected_ordinary_stop_can_be_stopped_by_deletion() {
    let (_root, store) = seeded_store().await;
    attach_delete_scope(&store).await;
    store.transaction(|tx| {
        tx.execute("INSERT INTO voice_sessions(id,agent,created,native_thread,account_key,connection_id,state) VALUES('rejected-stop','agent',1,'thread','account','7','ready')",[])?;
        save_scope(tx)?;
        Ok(())
    }).await.expect("session");
    let provider = Arc::new(ReceiptProvider {
        certainty: VoiceWriteCertainty::Rejected,
        calls: Mutex::new(vec![]),
    });
    let store = Arc::new(store);
    assert!(matches!(
        voice::provider::end(
            store.clone(),
            provider,
            "agent".into(),
            "rejected-stop".into(),
            10.0
        )
        .await,
        Err(voice::provider::VoiceProviderError::Rejected)
    ));
    let intent = store
        .transaction_with_error(|tx| voice::provider::stage_delete_sessions(tx, "agent"))
        .await
        .expect("delete after rejection");
    assert_eq!(intent.len(), 1);
    assert_eq!(intent[0].session.session_id, "rejected-stop");
}

fn table_rows(connection: &rusqlite::Connection, table: &str, order: &str) -> serde_json::Value {
    let mut statement = connection
        .prepare(&format!("SELECT * FROM {table} ORDER BY {order}"))
        .expect("table snapshot query");
    let columns = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let rows = statement
        .query_map([], |row| {
            let mut value = serde_json::Map::new();
            for (index, name) in columns.iter().enumerate() {
                let field = match row.get_ref(index)? {
                    rusqlite::types::ValueRef::Null => serde_json::Value::Null,
                    rusqlite::types::ValueRef::Integer(value) => json!(value),
                    rusqlite::types::ValueRef::Real(value) => json!(value),
                    rusqlite::types::ValueRef::Text(value) => {
                        json!(String::from_utf8_lossy(value).into_owned())
                    }
                    rusqlite::types::ValueRef::Blob(value) => json!(value),
                };
                value.insert(name.clone(), field);
            }
            Ok(serde_json::Value::Object(value))
        })
        .expect("table snapshot rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("table snapshot values");
    json!(rows)
}

struct CapturedRequest {
    account: String,
    operation: String,
    actor: Option<RequestActor>,
    expected_scope: Option<ConnectionScope>,
    deleted_actor_session: Option<String>,
    method: Method,
    params: serde_json::Value,
}

struct MockAccountSession {
    scope: ConnectionScope,
    outcomes: Mutex<VecDeque<(RequestCertainty, Option<ProviderResponse>)>>,
    calls: Mutex<Vec<CapturedRequest>>,
}

impl AccountSessionPort for MockAccountSession {
    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        (self.scope.account == account).then(|| self.scope.clone())
    }

    fn provider_request<'a>(
        &'a self,
        request: ProviderRequest,
    ) -> AccountSessionFuture<'a, SessionReceipt> {
        Box::pin(async move {
            self.calls
                .lock()
                .expect("calls lock")
                .push(CapturedRequest {
                    account: request.account.clone(),
                    operation: request.operation.clone(),
                    actor: request.actor.clone(),
                    expected_scope: request.expected_scope.clone(),
                    deleted_actor_session: request.deleted_actor_session.clone(),
                    method: request.method,
                    params: request.params,
                });
            let (certainty, result) = self
                .outcomes
                .lock()
                .expect("outcomes lock")
                .pop_front()
                .expect("scripted session outcome");
            Ok(SessionReceipt {
                outcome: {
                    let ticket = RequestTicket {
                        generation: self.scope.generation,
                        id: RequestId::Number(1),
                        method: request.method,
                    };
                    match (certainty, result) {
                        (RequestCertainty::Written, Some(response)) => {
                            ReceiptOutcome::Written { ticket, response }
                        }
                        (RequestCertainty::Written, None) | (RequestCertainty::Unknown, _) => {
                            ReceiptOutcome::Unknown {
                                ticket: RequestTicket {
                                    generation: self.scope.generation,
                                    id: RequestId::Number(1),
                                    method: request.method,
                                },
                            }
                        }
                        (RequestCertainty::NotSent, _) => ReceiptOutcome::NoLiveAccountWorker,
                    }
                },
                scope: self.scope.clone(),
                operation: request.operation,
                replayed: false,
            })
        })
    }
}

#[derive(Default)]
struct NoopCallbackReplyPort(AtomicUsize);

impl CallbackReplyPort for NoopCallbackReplyPort {
    fn has_live_session(
        &self,
        _: &studio_runtime_services::provider_callbacks::CallbackContext,
    ) -> bool {
        false
    }

    fn deliver<'a>(
        &'a self,
        reply: &'a studio_runtime_services::provider_callbacks::Reply,
        _deadline: Duration,
    ) -> CallbackReplyFuture<'a> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ReplyReceipt {
                identity: reply.identity.clone(),
                outcome: ReplyOutcome::Written,
            })
        })
    }
}

#[derive(Default)]
struct LiveCallbackReplyPort(AtomicUsize);

impl CallbackReplyPort for LiveCallbackReplyPort {
    fn has_live_session(
        &self,
        _: &studio_runtime_services::provider_callbacks::CallbackContext,
    ) -> bool {
        true
    }

    fn deliver<'a>(
        &'a self,
        reply: &'a studio_runtime_services::provider_callbacks::Reply,
        _deadline: Duration,
    ) -> CallbackReplyFuture<'a> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ReplyReceipt {
                identity: reply.identity.clone(),
                outcome: ReplyOutcome::Written,
            })
        })
    }
}

#[tokio::test]
async fn account_session_voice_adapter_preserves_scope_identity_and_unknown_once() {
    let (_root, store) = seeded_store().await;
    let scope = ConnectionScope {
        account: "account".into(),
        connection: "connection-7".into(),
        generation: 7,
    };
    let session = Arc::new(MockAccountSession {
        scope: scope.clone(),
        outcomes: Mutex::new(VecDeque::from([
            (
                RequestCertainty::Written,
                Some(ProviderResponse::Reply(
                    json!({"account":{"type":"chatgpt"}}),
                )),
            ),
            (
                RequestCertainty::Written,
                Some(ProviderResponse::Reply(json!({"answer":"v=0"}))),
            ),
            (RequestCertainty::Unknown, None),
            (
                RequestCertainty::Written,
                Some(ProviderResponse::Rejected(json!({"message":"no"}))),
            ),
            (
                RequestCertainty::Written,
                Some(ProviderResponse::Reply(json!({}))),
            ),
        ])),
        calls: Mutex::new(Vec::new()),
    });
    let store = Arc::new(store);
    let callbacks = Arc::new(
        studio_runtime_services::callback_runtime::CallbackRuntime::new(
            Arc::clone(&store),
            Arc::new(NoopCallbackReplyPort::default()),
        ),
    );
    let adapter = AccountSessionVoicePort::new(Arc::clone(&store), session.clone(), callbacks);
    let context = adapter
        .prepare("agent", "voice-session", "account", "thread", 0)
        .await
        .expect("account/read prepares the live session");
    assert_eq!(context.connection_id, scope.connection);
    assert_eq!(context.generation, scope.generation);

    let deadline = Instant::now() + Duration::from_secs(30);
    let started = adapter
        .request(
            context.clone(),
            studio_runtime_services::voice_runtime::VoiceOperation {
                id: "voice:start:voice-session".into(),
                root_request_id: studio_runtime_schema::persistence::RequestIdentity::issue_root(),
                admitted_request: None,
            },
            Method::ThreadRealtimeStart,
            json!({"threadId":"thread","realtimeSessionId":"voice-session"}),
            deadline,
        )
        .await;
    assert_eq!(started.certainty, VoiceWriteCertainty::Written);
    let unknown = adapter
        .request(
            context.clone(),
            studio_runtime_services::voice_runtime::VoiceOperation {
                id: "voice:speech:voice-session:record".into(),
                root_request_id: studio_runtime_schema::persistence::RequestIdentity::issue_root(),
                admitted_request: None,
            },
            Method::ThreadRealtimeAppendSpeech,
            json!({"threadId":"thread","audio":"encoded"}),
            deadline,
        )
        .await;
    assert_eq!(unknown.certainty, VoiceWriteCertainty::Unknown);
    let rejected = adapter
        .request(
            context,
            studio_runtime_services::voice_runtime::VoiceOperation {
                id: "voice:end:voice-session".into(),
                root_request_id: studio_runtime_schema::persistence::RequestIdentity::issue_root(),
                admitted_request: None,
            },
            Method::ThreadRealtimeStop,
            json!({"threadId":"thread"}),
            deadline,
        )
        .await;
    assert_eq!(rejected.certainty, VoiceWriteCertainty::Rejected);
    let deleted_stop = adapter
        .request(
            VoiceProviderContext {
                agent_id: "deleted-agent".into(),
                epoch: 3,
                account_key: "account".into(),
                thread_id: "thread".into(),
                connection_id: scope.connection.clone(),
                generation: scope.generation,
            },
            studio_runtime_services::voice_runtime::VoiceOperation {
                id: "voice:delete-stop:deleted-session".into(),
                root_request_id: studio_runtime_schema::persistence::RequestIdentity::issue_root(),
                admitted_request: None,
            },
            Method::ThreadRealtimeStop,
            json!({"threadId":"thread"}),
            deadline,
        )
        .await;
    assert_eq!(deleted_stop.certainty, VoiceWriteCertainty::Written);

    let calls = session.calls.lock().expect("calls lock");
    assert_eq!(calls.len(), 5, "Unknown is returned without a retry");
    assert_eq!(calls[0].operation, "voice:prepare:voice-session");
    assert_eq!(calls[0].method, Method::AccountRead);
    assert_eq!(calls[0].params, json!({"refreshToken":false}));
    assert!(calls[0].actor.is_none());
    assert!(calls[0].expected_scope.is_none());
    assert_eq!(calls[1].account, "account");
    assert_eq!(calls[1].operation, "voice:start:voice-session");
    assert_eq!(
        calls[1].actor,
        Some(RequestActor {
            agent: "agent".into(),
            epoch: 0
        })
    );
    assert_eq!(calls[1].expected_scope, Some(scope.clone()));
    assert_eq!(calls[1].method, Method::ThreadRealtimeStart);
    assert_eq!(calls[2].expected_scope, Some(scope));
    assert_eq!(calls[2].method, Method::ThreadRealtimeAppendSpeech);
    assert_eq!(calls[3].method, Method::ThreadRealtimeStop);
    assert_eq!(calls[3].deleted_actor_session, None);
    assert_eq!(calls[4].operation, "voice:delete-stop:deleted-session");
    assert_eq!(calls[4].actor.as_ref().map(|actor| actor.epoch), Some(3));
    assert_eq!(
        calls[4].deleted_actor_session.as_deref(),
        Some("deleted-session")
    );
}

#[tokio::test]
async fn account_session_voice_approval_uses_the_shared_callback_lane() {
    let (_root, store) = seeded_store().await;
    let callback_request_id = "voice-approval-request";
    let connection_id = "connection-7";
    let callback_request_id_for_transaction = callback_request_id;
    let connection_id_for_transaction = connection_id;
    let scope = VoiceProviderContext {
        agent_id: "agent".into(),
        epoch: 0,
        account_key: "account".into(),
        thread_id: "thread".into(),
        connection_id: connection_id.into(),
        generation: 7,
    };
    store
        .transaction(move |tx| {
            let raw: String = tx.query_row(
                "SELECT record FROM runtime_agents WHERE id='agent'",
                [],
                |row| row.get(0),
            )?;
            let mut actor: serde_json::Value = serde_json::from_str(&raw)?;
            actor["status"] = json!("approval");
            actor["autoWake"] = json!(true);
            actor["turnId"] = json!("turn");
            actor["turnEpoch"] = json!(0);
            actor["startAttempt"] = json!({
                "connectionId":connection_id_for_transaction,
                "connectionGeneration":7
            });
            tx.execute(
                "UPDATE runtime_agents SET record=?1 WHERE id='agent'",
                [actor.to_string()],
            )?;
            studio_runtime_services::provider_callbacks::legacy_view::insert(
                tx,
                &json!({
                    "id":callback_request_id_for_transaction,
                    "rpcId":99,
                    "method":"item/commandExecution/requestApproval",
                    "params":{"threadId":"thread","turnId":"turn"},
                    "agent":"agent",
                    "accountKey":"account",
                    "connectionId":connection_id_for_transaction,
                    "epoch":0,
                    "supervisor":{"generation":7},
                    "status":"pending",
                    "createdAt":1.0
                }),
            )?;
            Ok(())
        })
        .await
        .expect("seed callback request and live actor");
    let card = studio_runtime_services::provider_callbacks::legacy_view::stored(
        &store.test_reader().expect("fixture reader"),
        callback_request_id,
    )
    .expect("callback card");
    crate::callback_fixtures::seed_admission(&store, &card).await;

    let store = Arc::new(store);
    let reply_port = Arc::new(LiveCallbackReplyPort::default());
    let callbacks = Arc::new(
        studio_runtime_services::callback_runtime::CallbackRuntime::new(
            Arc::clone(&store),
            reply_port.clone(),
        ),
    );
    let unused_session = Arc::new(MockAccountSession {
        scope: ConnectionScope {
            account: "account".into(),
            connection: connection_id.into(),
            generation: 7,
        },
        outcomes: Mutex::new(VecDeque::new()),
        calls: Mutex::new(Vec::new()),
    });
    let adapter =
        AccountSessionVoicePort::new(Arc::clone(&store), unused_session.clone(), callbacks);
    let receipt = adapter
        .reply(
            scope,
            format!("voice:approve:{callback_request_id}"),
            VoiceReply {
                rpc_id: VoiceRpcId::Number(99),
                result: json!({"decision":"accept"}),
            },
            Instant::now() + Duration::from_secs(10),
        )
        .await;
    assert_eq!(receipt.certainty, VoiceWriteCertainty::Written);
    assert_eq!(reply_port.0.load(Ordering::SeqCst), 1);
    assert!(unused_session.calls.lock().expect("calls lock").is_empty());
}

#[tokio::test]
async fn providerless_receipt_replay_checks_agent_session_and_record_tuple() {
    let (_root, store) = seeded_store().await;
    store
        .transaction(|connection| {
            connection.execute(
                "INSERT INTO voice_speech_requests(id,session,record_id,state,error) VALUES(?1,?2,?3,'unknown','private')",
                rusqlite::params!["session:one:record", "session:one", "record"],
            )?;
            Ok(())
        })
        .await
        .expect("seed existing Python-format receipt");

    let replay = voice::provider::read_speech_receipt(&store, "agent", "session:one", "record")
        .await
        .expect("owned exact receipt is safe to replay");
    assert_eq!(replay.expect("receipt exists")["record_id"], "record");
    assert!(
        voice::provider::read_speech_receipt(
            &store,
            "other-agent",
            "owned-by-agent",
            "private-record",
        )
        .await
        .expect("foreign session is refused without disclosure")
        .is_none()
    );
    assert!(
        voice::provider::read_speech_receipt(&store, "agent", "session", "one:record")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn python_written_speech_receipt_replays_without_provider_request() {
    let (_root, store) = seeded_store().await;
    store.transaction(|tx| {
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('cutover-record','agent','session:one','orchestrator','spoken before cutover','item','', '{}',1)", [])?;
        tx.execute("INSERT INTO voice_speech_requests(id,session,record_id,state,error) VALUES('session:one:cutover-record','session:one','cutover-record','submitted',NULL)", [])?;
        Ok(())
    }).await.expect("seed Python-written speech receipt");
    let provider = Arc::new(ReceiptProvider {
        certainty: VoiceWriteCertainty::Written,
        calls: Mutex::new(Vec::new()),
    });
    let replay = voice::provider::speech(
        Arc::new(store),
        provider.clone(),
        "agent".into(),
        "session:one".into(),
        "cutover-record".into(),
        None,
    )
    .await
    .expect("Python receipt replays");
    assert_eq!(replay["id"], "session:one:cutover-record");
    assert_eq!(replay["state"], "submitted");
    assert!(provider.calls.lock().expect("calls").is_empty());
}

#[tokio::test]
async fn changed_speech_text_after_written_receipt_refuses_without_writes() {
    let (_root, store) = seeded_store().await;
    let store = Arc::new(store);
    store.transaction(|tx| {
        tx.execute("UPDATE voice_sessions SET native_thread='thread',account_key='account',connection_id='receipt-matrix-connection',state='ready' WHERE id='session'", [])?;
        save_scope(tx)?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('written-record','agent','','orchestrator','original text','item','', '{}',1)", [])?;
        Ok(())
    }).await.expect("seed ready session and speech row");
    let provider = Arc::new(ReceiptProvider {
        certainty: VoiceWriteCertainty::Written,
        calls: Mutex::new(Vec::new()),
    });
    voice::provider::speech(
        store.clone(),
        provider.clone(),
        "agent".into(),
        "session".into(),
        "written-record".into(),
        None,
    )
    .await
    .expect("first speech is written");
    let before = voice::provider::read_speech_receipt(&store, "agent", "session", "written-record")
        .await
        .expect("receipt read")
        .expect("receipt exists");
    store
        .transaction(move |tx| {
            tx.execute(
                "UPDATE voice_records SET text='changed text' WHERE id='written-record'",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("change content under same identity");
    let changed = voice::provider::speech(
        store.clone(),
        provider.clone(),
        "agent".into(),
        "session".into(),
        "written-record".into(),
        None,
    )
    .await
    .expect_err("written receipt cannot authorize changed text");
    assert!(matches!(
        changed,
        voice::provider::VoiceProviderError::Policy(policy::VoiceError::SessionIdentityConflict)
    ));
    let after = voice::provider::read_speech_receipt(&store, "agent", "session", "written-record")
        .await
        .expect("receipt read")
        .expect("receipt exists");
    assert_eq!(before, after);
    assert_eq!(provider.calls.lock().expect("calls").len(), 1);
}

#[tokio::test]
#[allow(clippy::disallowed_methods)]
async fn real_python_start_speech_end_rows_match_rust_with_rpc_boundary_stub() {
    let (_root, store) = seeded_store().await;
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("voice_provider/lifecycle-expected.json"))
            .expect("frozen voice lifecycle corpus");

    store.transaction(|tx| {
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('oracle-speech','agent','oracle-session','orchestrator','hello','item','', '{}',100)", [])?;
        Ok(())
    }).await.expect("seed Rust speech row");
    let store = Arc::new(store);
    let provider = Arc::new(SignaledReceiptProvider {
        certainty: VoiceWriteCertainty::Written,
        requested: tokio::sync::Notify::new(),
        calls: Mutex::new(Vec::new()),
    });
    let requested = provider.requested.notified();
    voice::provider::start(
        store.clone(),
        provider.clone(),
        "agent".into(),
        "oracle-session".into(),
        "v=0\r\noffer".into(),
        100.0,
    )
    .await
    .expect("Rust start");
    tokio::time::timeout(std::time::Duration::from_secs(10), requested)
        .await
        .expect("provider request dispatched");
    voice::provider::wait_for_start_setup("agent", "oracle-session").await;
    voice::provider::handle_notification(&store, "account", "receipt-matrix-connection", &json!({"method":"thread/realtime/sdp","params":{"threadId":"thread","realtimeSessionId":"oracle-session","sdp":"answer"}}), 100.0).await.expect("Rust SDP callback");
    voice::provider::speech(
        store.clone(),
        provider.clone(),
        "agent".into(),
        "oracle-session".into(),
        "oracle-speech".into(),
        None,
    )
    .await
    .expect("Rust speech");
    voice::provider::end(
        store.clone(),
        provider.clone(),
        "agent".into(),
        "oracle-session".into(),
        100.0,
    )
    .await
    .expect("Rust end");

    let connection = store.test_reader().expect("Rust lifecycle rows");
    let encoded_scope: String = connection
        .query_row(
            "SELECT provider_scope FROM voice_sessions WHERE id='oracle-session'",
            [],
            |row| row.get(0),
        )
        .expect("captured voice scope");
    let scope: studio_runtime_schema::persistence::ProviderScope =
        serde_json::from_str(&encoded_scope).expect("typed voice scope");
    assert_eq!(
        scope,
        studio_runtime_schema::persistence::ProviderScope {
            generation: 1,
            connection: "receipt-matrix-connection".into(),
        }
    );
    for (table, order) in [
        ("voice_sessions", "id"),
        ("voice_records", "seq"),
        ("runtime_events", "created,id"),
        ("sync_entities", "collection,id"),
        ("sync_documents", "scope,id,seq"),
    ] {
        let mut rust_rows = table_rows(&connection, table, order);
        let mut python_rows = expected["tables"][table].clone();
        if table == "voice_sessions" {
            rust_rows
                .as_array_mut()
                .expect("Rust voice-session array")
                .retain(|row| row["id"] == "oracle-session");
            for row in rust_rows.as_array_mut().expect("Rust voice-session array") {
                row.as_object_mut()
                    .expect("voice-session row")
                    .remove("provider_scope");
            }
            python_rows
                .as_array_mut()
                .expect("Python voice-session array")
                .retain(|row| row["id"] == "oracle-session");
            // The durable stop owner settles a written stop before returning.
            python_rows[0]["state"] = json!("ended");
            python_rows[0]["ended"] = json!(100.0);
        } else if table == "voice_records" {
            rust_rows
                .as_array_mut()
                .expect("Rust voice-record array")
                .retain(|row| row["id"] == "oracle-speech");
            python_rows
                .as_array_mut()
                .expect("Python voice-record array")
                .retain(|row| row["id"] == "oracle-speech");
        }
        assert_eq!(rust_rows, python_rows, "Python/Rust {table}");
    }
    let mut receipts = table_rows(&connection, "voice_speech_requests", "id");
    receipts
        .as_array_mut()
        .expect("Rust speech-receipt array")
        .retain(|row| row["id"] == "oracle-session:oracle-speech");
    let mut python_receipts = expected["tables"]["voice_speech_requests"].clone();
    python_receipts
        .as_array_mut()
        .expect("Python speech-receipt array")
        .retain(|row| row["id"] == "oracle-session:oracle-speech");
    assert_eq!(receipts[0]["id"], "oracle-session:oracle-speech");
    assert!(
        receipts[0]["error"]
            .as_str()
            .is_some_and(|value| value.starts_with("voice-content-sha256:"))
    );
    receipts[0]["error"] = serde_json::Value::Null;
    assert_eq!(
        receipts, python_receipts,
        "only expected difference: Rust's registered transcript digest marker in error"
    );
    assert_eq!(provider.calls.lock().expect("Rust provider calls").len(), 3);
    assert_eq!(
        expected["provider_calls"]
            .as_array()
            .expect("Python RPC boundary calls")
            .len(),
        3
    );
    let rust_agent: serde_json::Value = connection
        .query_row(
            "SELECT record FROM runtime_agents WHERE id='agent'",
            [],
            |row| row.get::<_, String>(0),
        )
        .map(|raw| serde_json::from_str(&raw).expect("Rust agent row JSON"))
        .expect("Rust voice actor row");
    assert_eq!(rust_agent["autoWake"], expected["agent"]["autoWake"]);
    assert_eq!(rust_agent["turnEpoch"], expected["agent"]["turnEpoch"]);
    assert_eq!(rust_agent["autoWake"], true);
    assert_eq!(rust_agent["turnEpoch"], rust_agent["epoch"]);
}

#[tokio::test]
async fn start_preflight_matches_python_context_repair_denial() {
    for (case, patch) in [(
        "context repair",
        "UPDATE runtime_agents SET record=json_set(record,'$.contextRepair',json('{\"id\":\"repair\",\"phase\":\"unknown\"}')) WHERE id='agent'",
    )] {
        let (_root, store) = seeded_store().await;
        store
            .transaction(|tx| {
                tx.execute(patch, [])?;
                Ok(())
            })
            .await
            .expect("prepare denied Python-format actor");
        let provider = Arc::new(ReceiptProvider {
            certainty: VoiceWriteCertainty::Written,
            calls: Mutex::new(Vec::new()),
        });
        let result = voice::provider::start(
            Arc::new(store),
            provider.clone(),
            "agent".into(),
            format!("denied-{case}"),
            "v=0\\r\\noffer".into(),
            100.0,
        )
        .await;
        assert!(result.is_err(), "{case} must refuse voice start");
        assert!(
            provider.calls.lock().expect("provider calls").is_empty(),
            "{case} must refuse before realtime start dispatch"
        );
    }
}

#[tokio::test]
async fn audio_validation_precedes_scoped_expiry_pruning() {
    let (_root, store) = seeded_store().await;
    let store = Arc::new(store);
    let directory = store.root().join("voice-audio");
    fs::create_dir_all(&directory).expect("voice audio directory");
    let other_session_expired = directory.join("s-another-session-c-expired");
    fs::write(&other_session_expired, b"other session data").expect("expired chunk");
    let first = voice::save_audio(
        Arc::clone(&store),
        "agent",
        "session",
        "expire-me",
        "AQID",
        "audio/webm",
    )
    .await
    .expect("first synthetic upload");
    let record_id = first["id"].as_str().expect("record id");
    let payload: String = store
        .test_reader()
        .expect("audio record database")
        .query_row(
            "SELECT payload FROM voice_records WHERE id=?1",
            [record_id],
            |row| row.get(0),
        )
        .expect("audio payload");
    let own_expired = directory.join(
        serde_json::from_str::<serde_json::Value>(&payload).expect("audio payload JSON")["file"]
            .as_str()
            .expect("audio file name"),
    );
    assert!(
        own_expired.exists(),
        "audio path {} missing; directory entries: {:?}",
        own_expired.display(),
        fs::read_dir(&directory)
            .expect("audio dir")
            .map(|e| e.expect("entry").file_name())
            .collect::<Vec<_>>()
    );
    let old = SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60);
    for path in [&other_session_expired, &own_expired] {
        fs::File::options()
            .write(true)
            .open(path)
            .expect("open expired chunk")
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .expect("mark chunk expired");
    }

    assert!(
        voice::save_audio(
            Arc::clone(&store),
            "agent",
            "session",
            "bad",
            "%%%",
            "text/plain"
        )
        .await
        .is_err()
    );
    assert!(
        other_session_expired.exists(),
        "invalid upload must not prune"
    );
    assert!(
        own_expired.exists(),
        "invalid upload must preserve own expired data"
    );
    voice::save_audio(
        Arc::clone(&store),
        "agent",
        "session",
        "valid",
        "AQID",
        "audio/webm",
    )
    .await
    .expect("valid synthetic upload");
    assert!(
        !own_expired.exists(),
        "valid upload prunes this session's expiry"
    );
    assert!(other_session_expired.exists(), "pruning is session scoped");
}

struct BlockingProvider {
    start_entered: tokio::sync::Notify,
    release_start: tokio::sync::Notify,
    stop_entered: tokio::sync::Notify,
    methods: Mutex<Vec<Method>>,
}

struct ReceiptProvider {
    certainty: VoiceWriteCertainty,
    calls: Mutex<Vec<(String, Method)>>,
}

struct SignaledReceiptProvider {
    certainty: VoiceWriteCertainty,
    requested: tokio::sync::Notify,
    calls: Mutex<Vec<(String, Method)>>,
}

struct PrepareNotSentProvider {
    prepare_entered: tokio::sync::Notify,
    release_prepare: tokio::sync::Notify,
    prepare_calls: AtomicUsize,
    request_calls: AtomicUsize,
}

struct DelayedPrepareProvider {
    prepare_entered: tokio::sync::Notify,
    release_prepare: tokio::sync::Notify,
    requested: tokio::sync::Notify,
    prepare_calls: AtomicUsize,
    request_calls: AtomicUsize,
}

impl VoiceProviderPort for DelayedPrepareProvider {
    fn prepare<'a>(
        &'a self,
        agent_id: &'a str,
        _session_id: &'a str,
        account_key: &'a str,
        thread_id: &'a str,
        epoch: i64,
    ) -> ProviderFuture<'a, Result<VoiceProviderContext, VoicePrepareFailure>> {
        Box::pin(async move {
            self.prepare_calls.fetch_add(1, Ordering::SeqCst);
            self.prepare_entered.notify_one();
            self.release_prepare.notified().await;
            Ok(VoiceProviderContext {
                agent_id: agent_id.to_owned(),
                epoch,
                account_key: account_key.to_owned(),
                thread_id: thread_id.to_owned(),
                connection_id: "delayed-prepare-connection".into(),
                generation: 1,
            })
        })
    }

    fn request<'a>(
        &'a self,
        _scope: VoiceProviderContext,
        _operation_id: studio_runtime_services::voice_runtime::VoiceOperation,
        _method: Method,
        _params: serde_json::Value,
        _deadline: Instant,
    ) -> ProviderFuture<'a, VoiceRequestReceipt> {
        Box::pin(async move {
            self.request_calls.fetch_add(1, Ordering::SeqCst);
            self.requested.notify_one();
            VoiceRequestReceipt {
                certainty: VoiceWriteCertainty::Written,
            }
        })
    }

    fn reply<'a>(
        &'a self,
        _scope: VoiceProviderContext,
        _operation_id: String,
        _reply: VoiceReply,
        _deadline: Instant,
    ) -> ProviderFuture<'a, VoiceRequestReceipt> {
        Box::pin(async {
            VoiceRequestReceipt {
                certainty: VoiceWriteCertainty::NotSent,
            }
        })
    }
}

impl VoiceProviderPort for PrepareNotSentProvider {
    fn prepare<'a>(
        &'a self,
        _agent_id: &'a str,
        _session_id: &'a str,
        _account_key: &'a str,
        _thread_id: &'a str,
        _epoch: i64,
    ) -> ProviderFuture<'a, Result<VoiceProviderContext, VoicePrepareFailure>> {
        Box::pin(async move {
            self.prepare_calls.fetch_add(1, Ordering::SeqCst);
            self.prepare_entered.notify_one();
            self.release_prepare.notified().await;
            Err(VoicePrepareFailure::NotSent)
        })
    }

    fn request<'a>(
        &'a self,
        _scope: VoiceProviderContext,
        _operation_id: studio_runtime_services::voice_runtime::VoiceOperation,
        _method: Method,
        _params: serde_json::Value,
        _deadline: Instant,
    ) -> ProviderFuture<'a, VoiceRequestReceipt> {
        Box::pin(async move {
            self.request_calls.fetch_add(1, Ordering::SeqCst);
            VoiceRequestReceipt {
                certainty: VoiceWriteCertainty::NotSent,
            }
        })
    }

    fn reply<'a>(
        &'a self,
        _scope: VoiceProviderContext,
        _operation_id: String,
        _reply: VoiceReply,
        _deadline: Instant,
    ) -> ProviderFuture<'a, VoiceRequestReceipt> {
        Box::pin(async move {
            self.request_calls.fetch_add(1, Ordering::SeqCst);
            VoiceRequestReceipt {
                certainty: VoiceWriteCertainty::NotSent,
            }
        })
    }
}

impl VoiceProviderPort for ReceiptProvider {
    fn prepare<'a>(
        &'a self,
        agent_id: &'a str,
        _session_id: &'a str,
        account_key: &'a str,
        thread_id: &'a str,
        epoch: i64,
    ) -> ProviderFuture<'a, Result<VoiceProviderContext, VoicePrepareFailure>> {
        Box::pin(async move {
            Ok(VoiceProviderContext {
                agent_id: agent_id.to_owned(),
                epoch,
                account_key: account_key.to_owned(),
                thread_id: thread_id.to_owned(),
                connection_id: "receipt-matrix-connection".into(),
                generation: 1,
            })
        })
    }

    fn request<'a>(
        &'a self,
        _scope: VoiceProviderContext,
        operation_id: studio_runtime_services::voice_runtime::VoiceOperation,
        method: Method,
        _params: serde_json::Value,
        deadline: std::time::Instant,
    ) -> ProviderFuture<'a, VoiceRequestReceipt> {
        let operation_id = operation_id.id;
        Box::pin(async move {
            assert!(deadline > std::time::Instant::now());
            self.calls
                .lock()
                .expect("provider call log")
                .push((operation_id, method));
            VoiceRequestReceipt {
                certainty: self.certainty,
            }
        })
    }

    fn reply<'a>(
        &'a self,
        _scope: VoiceProviderContext,
        operation_id: String,
        _reply: VoiceReply,
        _deadline: std::time::Instant,
    ) -> ProviderFuture<'a, VoiceRequestReceipt> {
        Box::pin(async move {
            self.calls
                .lock()
                .expect("provider call log")
                .push((operation_id, Method::ItemCommandExecutionRequestApproval));
            VoiceRequestReceipt {
                certainty: self.certainty,
            }
        })
    }
}

impl VoiceProviderPort for SignaledReceiptProvider {
    fn prepare<'a>(
        &'a self,
        agent_id: &'a str,
        _session_id: &'a str,
        account_key: &'a str,
        thread_id: &'a str,
        epoch: i64,
    ) -> ProviderFuture<'a, Result<VoiceProviderContext, VoicePrepareFailure>> {
        Box::pin(async move {
            Ok(VoiceProviderContext {
                agent_id: agent_id.to_owned(),
                epoch,
                account_key: account_key.to_owned(),
                thread_id: thread_id.to_owned(),
                connection_id: "receipt-matrix-connection".into(),
                generation: 1,
            })
        })
    }

    fn request<'a>(
        &'a self,
        _scope: VoiceProviderContext,
        operation_id: studio_runtime_services::voice_runtime::VoiceOperation,
        method: Method,
        _params: serde_json::Value,
        _deadline: Instant,
    ) -> ProviderFuture<'a, VoiceRequestReceipt> {
        let operation_id = operation_id.id;
        Box::pin(async move {
            self.calls
                .lock()
                .expect("provider call log")
                .push((operation_id, method));
            self.requested.notify_one();
            VoiceRequestReceipt {
                certainty: self.certainty,
            }
        })
    }

    fn reply<'a>(
        &'a self,
        _scope: VoiceProviderContext,
        operation_id: String,
        _reply: VoiceReply,
        _deadline: Instant,
    ) -> ProviderFuture<'a, VoiceRequestReceipt> {
        Box::pin(async move {
            self.calls
                .lock()
                .expect("provider call log")
                .push((operation_id, Method::ItemCommandExecutionRequestApproval));
            VoiceRequestReceipt {
                certainty: self.certainty,
            }
        })
    }
}

impl VoiceProviderPort for BlockingProvider {
    fn prepare<'a>(
        &'a self,
        agent_id: &'a str,
        _session_id: &'a str,
        account_key: &'a str,
        thread_id: &'a str,
        epoch: i64,
    ) -> ProviderFuture<'a, Result<VoiceProviderContext, VoicePrepareFailure>> {
        Box::pin(async move {
            Ok(VoiceProviderContext {
                agent_id: agent_id.to_owned(),
                epoch,
                account_key: account_key.to_owned(),
                thread_id: thread_id.to_owned(),
                connection_id: "voice-corpus-connection".into(),
                generation: 1,
            })
        })
    }

    fn request<'a>(
        &'a self,
        scope: VoiceProviderContext,
        operation_id: studio_runtime_services::voice_runtime::VoiceOperation,
        method: Method,
        params: serde_json::Value,
        deadline: std::time::Instant,
    ) -> ProviderFuture<'a, VoiceRequestReceipt> {
        let operation_id = operation_id.id;
        Box::pin(async move {
            self.methods.lock().expect("provider call log").push(method);
            match method {
                Method::ThreadRealtimeStart => {
                    self.start_entered.notify_one();
                    self.release_start.notified().await;
                }
                Method::ThreadRealtimeStop => self.stop_entered.notify_one(),
                Method::Initialize
                | Method::Initialized
                | Method::Ack
                | Method::AccountLoginCancel
                | Method::AccountLoginStart
                | Method::AccountRateLimitResetCreditConsume
                | Method::ThreadLoadedList
                | Method::ConfigRead
                | Method::SkillsExtraRootsSet
                | Method::ModelList
                | Method::AccountRead
                | Method::AccountRateLimitsRead
                | Method::ThreadStart
                | Method::ThreadRealtimeAppendSpeech
                | Method::ThreadRealtimeItemStarted
                | Method::ThreadRealtimeTranscriptDelta
                | Method::ThreadRealtimeTranscriptDone
                | Method::ThreadRealtimeSdp
                | Method::ThreadRealtimeError
                | Method::ThreadRealtimeClosed
                | Method::ThreadResume
                | Method::ThreadRead
                | Method::ThreadTurnsList
                | Method::TurnStart
                | Method::TurnSteer
                | Method::TurnInterrupt
                | Method::ThreadList
                | Method::ThreadArchive
                | Method::ThreadFork
                | Method::ThreadNameSet
                | Method::ThreadMetadataUpdate
                | Method::ThreadSettingsUpdate
                | Method::ThreadCompactStart
                | Method::ThreadRollback
                | Method::ThreadItemsList
                | Method::ThreadGoalSet
                | Method::ThreadGoalUpdated
                | Method::ThreadUnarchive
                | Method::ThreadUnsubscribe
                | Method::ProcessSpawn
                | Method::ProcessWriteStdin
                | Method::ProcessKill
                | Method::ProcessResizePty
                | Method::CommandExec
                | Method::CommandExecWrite
                | Method::CommandExecResize
                | Method::CommandExecTerminate
                | Method::ThreadBackgroundTerminalsList
                | Method::ThreadBackgroundTerminalsTerminate
                | Method::ThreadBackgroundTerminalsClean
                | Method::ThreadCommandExecutionBackground
                | Method::ThreadQueueList
                | Method::ClaudeState
                | Method::ClaudeRepairReport
                | Method::ClaudeFileBackups
                | Method::ClaudeSettings
                | Method::ClaudeActivity
                | Method::ClaudeStopTask
                | Method::ClaudeShellPolicy
                | Method::ClaudeDiagnostics
                | Method::ClaudeMoveVersions
                | Method::McpServerReconnect
                | Method::McpServerStatusList
                | Method::SkillsList
                | Method::OperationStatus
                | Method::ReviewStart
                | Method::BrowserAnswer
                | Method::BrowserCallback
                | Method::BrowserClose
                | Method::BrowserWait
                | Method::ClaudeCommands
                | Method::ClaudeMoveExport
                | Method::ClaudeMoveIdentity
                | Method::ClaudeMovePreflight
                | Method::ClaudeMoveProof
                | Method::Detach
                | Method::EventRead
                | Method::Exec
                | Method::Generate
                | Method::Health
                | Method::Info
                | Method::LayrHealth
                | Method::Next
                | Method::Open
                | Method::ProjectEnsure
                | Method::ProjectExec
                | Method::ProjectImport
                | Method::ProviderList
                | Method::Replay
                | Method::ResponseStatus
                | Method::RetireCommand
                | Method::SecretsList
                | Method::SecretsRemove
                | Method::SecretsSet
                | Method::ShareConfigure
                | Method::ShareName
                | Method::ShareStatus
                | Method::Sign
                | Method::Status
                | Method::SyncMacApply
                | Method::SyncMacHandoff
                | Method::SyncMacHashes
                | Method::SyncMacHead
                | Method::SyncMacRead
                | Method::SyncMacWorktree
                | Method::VaultConfigure
                | Method::VaultPush
                | Method::VaultStatus
                | Method::UploadBegin
                | Method::UploadChunk
                | Method::UploadCommit
                | Method::Verify
                | Method::Write
                | Method::CurrentTimeRead
                | Method::ItemToolCall
                | Method::ItemCommandExecutionRequestApproval
                | Method::ItemFileChangeRequestApproval
                | Method::ItemPermissionsRequestApproval
                | Method::McpServerElicitationRequest
                | Method::ItemToolRequestUserInput
                | Method::ItemStarted
                | Method::ItemCompleted
                | Method::ItemAgentMessageDelta
                | Method::ItemReasoningTextDelta
                | Method::ItemCommandExecutionOutputDelta
                | Method::CommandExecOutputDelta
                | Method::TurnStarted
                | Method::TurnCompleted
                | Method::TurnPlanUpdated
                | Method::TurnDiffUpdated
                | Method::ThreadClosed
                | Method::ThreadStatusChanged
                | Method::ThreadTokenUsageUpdated
                | Method::ThreadShellPolicyUpdated
                | Method::ThreadIdleCompactionUpdated
                | Method::AccountLoginCompleted
                | Method::AccountUpdated
                | Method::AccountRateLimitsUpdated
                | Method::ModelSafetyBufferingUpdated
                | Method::ProviderGenerationStarted
                | Method::ProtocolError => {}
            }
            let _ = (scope, operation_id, params, deadline);
            VoiceRequestReceipt {
                certainty: VoiceWriteCertainty::Written,
            }
        })
    }

    fn reply<'a>(
        &'a self,
        _scope: VoiceProviderContext,
        _operation_id: String,
        _reply: VoiceReply,
        _deadline: std::time::Instant,
    ) -> ProviderFuture<'a, VoiceRequestReceipt> {
        Box::pin(async {
            VoiceRequestReceipt {
                certainty: VoiceWriteCertainty::Written,
            }
        })
    }
}

#[tokio::test]
async fn unknown_stop_receipt_is_held_without_resending() {
    let (_root, store) = seeded_store().await;
    store
        .transaction(|connection| {
            connection.execute(
                "UPDATE voice_sessions SET native_thread='thread',account_key='account',connection_id='voice-corpus-connection',state='unknown' WHERE id='session'",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("persist uncertain stop state");
    let provider = Arc::new(BlockingProvider {
        start_entered: tokio::sync::Notify::new(),
        release_start: tokio::sync::Notify::new(),
        stop_entered: tokio::sync::Notify::new(),
        methods: Mutex::new(Vec::new()),
    });
    let result = voice::provider::end(
        Arc::new(store),
        provider.clone(),
        "agent".into(),
        "session".into(),
        2.0,
    )
    .await
    .expect("unknown stop is a stable read");
    assert_eq!(result["state"], "unknown");
    assert!(
        provider
            .methods
            .lock()
            .expect("provider call log")
            .is_empty()
    );
}

#[tokio::test]
async fn provider_actions_do_not_dispatch_when_intent_write_fails() {
    let provider = Arc::new(ReceiptProvider {
        certainty: VoiceWriteCertainty::Written,
        calls: Mutex::new(Vec::new()),
    });

    let (_root, store) = seeded_store().await;
    store.transaction(|tx| {
        tx.execute_batch("CREATE TRIGGER fail_start_intent BEFORE INSERT ON voice_sessions WHEN NEW.id='matrix-start-fail' BEGIN SELECT RAISE(ABORT,'injected'); END;")?;
        Ok(())
    }).await.expect("install start intent fail point");
    assert!(
        voice::provider::start(
            Arc::new(store),
            provider.clone(),
            "other-agent".into(),
            "matrix-start-fail".into(),
            "v=0\r\n".into(),
            1.0,
        )
        .await
        .is_err()
    );
    assert!(provider.calls.lock().expect("provider calls").is_empty());

    let (_root, store) = seeded_store().await;
    store.transaction(|tx| {
        tx.execute("UPDATE voice_sessions SET native_thread='thread',account_key='account',connection_id='receipt-matrix-connection',state='ready' WHERE id='session'", [])?;
        save_scope(tx)?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('matrix-speech-fail','agent','','orchestrator','spoken','item','', '{}',1)", [])?;
        tx.execute_batch("CREATE TRIGGER fail_speech_intent BEFORE INSERT ON voice_speech_requests BEGIN SELECT RAISE(ABORT,'injected'); END;")?;
        Ok(())
    }).await.expect("install speech intent fail point");
    assert!(
        voice::provider::speech(
            Arc::new(store),
            provider.clone(),
            "agent".into(),
            "session".into(),
            "matrix-speech-fail".into(),
            None,
        )
        .await
        .is_err()
    );
    assert!(provider.calls.lock().expect("provider calls").is_empty());

    let (_root, store) = seeded_store().await;
    store.transaction(|tx| {
        let actor: String = tx.query_row("SELECT record FROM runtime_agents WHERE id='agent'", [], |row| row.get(0))?;
        let mut actor: serde_json::Value = serde_json::from_str(&actor).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        actor["autoWake"] = json!(true);
        tx.execute("UPDATE runtime_agents SET record=?1 WHERE id='agent'", [actor.to_string()])?;
        tx.execute("UPDATE voice_sessions SET native_thread='thread',account_key='account',connection_id='receipt-matrix-connection',state='ready' WHERE id='session'", [])?;
        tx.execute("INSERT INTO voice_sessions(id,agent,created,account_key,connection_id,state) VALUES('matrix-approve-fail','agent',1,'account','receipt-matrix-connection','ready')", [])?;
        save_scope(tx)?;
        let request = json!({"id":"matrix-approve-fail-request","rpcId":"matrix-rpc-fail","agent":"agent","epoch":0,"status":"pending","method":"item/commandExecution/requestApproval","accountKey":"account","connectionId":"receipt-matrix-connection","params":{"command":"safe","threadId":"thread"}});
        let approval = studio_runtime_store::approvals::from_legacy_record("matrix-approve-fail-request", &request).expect("typed approval");
        studio_runtime_store::approvals::put(tx, &approval).expect("approval row");
        let request = serde_json::to_value(&approval).expect("approval json");
        let fingerprint = studio_runtime_store::sha256_hex(studio_runtime_store::python_json(&request).as_bytes());
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('matrix-approve-fail-speech','agent','','orchestrator','approval','','',?1,1)", [json!({"request_id":"matrix-approve-fail-request","fingerprint":fingerprint}).to_string()])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('matrix-approve-fail-playback','agent','matrix-approve-fail','playback','played','','',?1,2)", [json!({"record_id":"matrix-approve-fail-speech"}).to_string()])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('matrix-approve-fail-transcript','agent','matrix-approve-fail','user','разрешаю','','','{}',3)", [])?;
        tx.execute_batch("CREATE TRIGGER fail_approval_intent BEFORE INSERT ON voice_approvals BEGIN SELECT RAISE(ABORT,'injected'); END;")?;
        Ok(())
    }).await.expect("install approval intent fail point");
    assert!(
        voice::provider::approve(
            Arc::new(store),
            provider.clone(),
            "agent".into(),
            "matrix-approve-fail".into(),
            "matrix-approve-fail-speech".into(),
            "matrix-approve-fail-transcript".into(),
            1.0,
        )
        .await
        .is_err()
    );
    assert!(provider.calls.lock().expect("provider calls").is_empty());

    let (_root, store) = seeded_store().await;
    store.transaction(|tx| {
        tx.execute("UPDATE voice_sessions SET native_thread='thread',account_key='account',connection_id='receipt-matrix-connection',state='ready' WHERE id='session'", [])?;
        save_scope(tx)?;
        tx.execute_batch("CREATE TRIGGER fail_end_intent BEFORE UPDATE ON voice_sessions WHEN OLD.id='session' BEGIN SELECT RAISE(ABORT,'injected'); END;")?;
        Ok(())
    }).await.expect("install end intent fail point");
    assert!(
        voice::provider::end(
            Arc::new(store),
            provider.clone(),
            "agent".into(),
            "session".into(),
            1.0,
        )
        .await
        .is_err()
    );
    assert!(provider.calls.lock().expect("provider calls").is_empty());
}

#[tokio::test]
async fn approval_from_another_connection_generation_is_refused_without_writes() {
    let (_root, store) = seeded_store().await;
    store.transaction(|tx| {
        let actor: String = tx.query_row("SELECT record FROM runtime_agents WHERE id='agent'", [], |row| row.get(0))?;
        let mut actor: serde_json::Value = serde_json::from_str(&actor)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        actor["autoWake"] = json!(true);
        tx.execute("UPDATE runtime_agents SET record=?1 WHERE id='agent'", [actor.to_string()])?;
        tx.execute("INSERT INTO voice_sessions(id,agent,created,account_key,connection_id,state) VALUES('connection-bound-approval','agent',1,'account','receipt-matrix-connection','ready')", [])?;
        save_scope(tx)?;
        let request = json!({
            "id":"request-from-prior-generation", "rpcId":"prior-rpc", "agent":"agent", "epoch":0,
            "status":"pending", "method":"item/commandExecution/requestApproval",
            "accountKey":"account", "connectionId":"another-connection-generation",
            "params":{"command":"safe", "threadId":"thread"}
        });
        let approval = studio_runtime_store::approvals::from_legacy_record("request-from-prior-generation", &request).expect("typed approval");
        studio_runtime_store::approvals::put(tx, &approval).expect("approval row");
        let request = serde_json::to_value(&approval).expect("approval json");
        let fingerprint = studio_runtime_store::sha256_hex(studio_runtime_store::python_json(&request).as_bytes());
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('connection-bound-speech','agent','','orchestrator','approval','','',?1,1)", [json!({"request_id":"request-from-prior-generation","fingerprint":fingerprint}).to_string()])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('connection-bound-playback','agent','connection-bound-approval','playback','played','','',?1,2)", [json!({"record_id":"connection-bound-speech"}).to_string()])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('connection-bound-transcript','agent','connection-bound-approval','user','разрешаю','','','{}',3)", [])?;
        Ok(())
    }).await.expect("seed an approval tied to another connection generation");
    let provider = Arc::new(ReceiptProvider {
        certainty: VoiceWriteCertainty::Written,
        calls: Mutex::new(Vec::new()),
    });
    let store = Arc::new(store);
    let error = voice::provider::approve(
        store.clone(),
        provider.clone(),
        "agent".into(),
        "connection-bound-approval".into(),
        "connection-bound-speech".into(),
        "connection-bound-transcript".into(),
        1.0,
    )
    .await
    .expect_err("a stale-connection RPC cannot be answered on this voice connection");
    assert!(matches!(
        error,
        voice::provider::VoiceProviderError::ContextChanged
    ));
    assert!(provider.calls.lock().expect("provider calls").is_empty());
    let connection = store.test_reader().expect("read refusal state");
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM voice_approvals", [], |row| row
                .get::<_, i64>(0))
            .expect("approval count"),
        0
    );
    let request = studio_runtime_store::approvals::get(
        &connection,
        &studio_runtime_schema::approvals::ApprovalId("request-from-prior-generation".into()),
    )
    .expect("request row")
    .expect("request");
    assert_eq!(
        request.state,
        studio_runtime_schema::approvals::ApprovalState::Pending
    );
}

#[tokio::test]
async fn voice_start_returns_connecting_before_provider_setup_and_replays_once() {
    let (_root, store) = seeded_store().await;
    studio_runtime_services::account_worker::attach_provider_stream_for_test(
        &store,
        "account",
        None,
        1,
        "delayed-prepare-connection",
    )
    .await
    .expect("attach delayed provider");
    let store = Arc::new(store);
    let provider = Arc::new(DelayedPrepareProvider {
        prepare_entered: tokio::sync::Notify::new(),
        release_prepare: tokio::sync::Notify::new(),
        requested: tokio::sync::Notify::new(),
        prepare_calls: AtomicUsize::new(0),
        request_calls: AtomicUsize::new(0),
    });
    let entered = provider.prepare_entered.notified();
    let accepted = voice::provider::start(
        store.clone(),
        provider.clone(),
        "other-agent".into(),
        "delayed-prepare-success".into(),
        "v=0\r\n".into(),
        10.0,
    )
    .await
    .expect("the durable connecting session is accepted before provider setup");
    assert_eq!(accepted["state"], "connecting");
    tokio::time::timeout(std::time::Duration::from_secs(10), entered)
        .await
        .expect("provider preparation entered");
    let connection = store.test_reader().expect("read connecting session");
    let (state, connection_id): (String, Option<String>) = connection
        .query_row(
            "SELECT state,connection_id FROM voice_sessions WHERE id='delayed-prepare-success'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("durable connecting session exists before setup completes");
    assert_eq!(state, "connecting");
    assert_eq!(connection_id, None);
    drop(connection);

    provider.release_prepare.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        provider.requested.notified(),
    )
    .await
    .expect("provider request dispatched");
    voice::provider::wait_for_start_setup("other-agent", "delayed-prepare-success").await;
    let replay = voice::provider::start(
        store,
        provider.clone(),
        "other-agent".into(),
        "delayed-prepare-success".into(),
        "v=0\r\n".into(),
        11.0,
    )
    .await
    .expect("retry returns the stored session");
    assert_eq!(replay["session_id"], "delayed-prepare-success");
    assert_eq!(provider.prepare_calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.request_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn voice_start_without_provider_commits_then_records_setup_failure() {
    let (_root, store) = seeded_store().await;
    store
        .transaction(|tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_remove(record,'$.threadId') WHERE id='other-agent'",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("seed an actor without a thread, as Python permits at admission");
    let store = Arc::new(store);
    let accepted = voice::provider::start_admitted(
        store.clone(),
        None,
        "other-agent".into(),
        "missing-provider-start".into(),
        "v=0\r\n".into(),
        10.0,
    )
    .await
    .expect("missing provider setup does not change the accepted HTTP response");
    assert_eq!(accepted["state"], "connecting");

    voice::provider::wait_for_start_setup("other-agent", "missing-provider-start").await;
    let connection = store.test_reader().expect("read setup result");
    let (state, error): (String, Option<String>) = connection
        .query_row(
            "SELECT state,error FROM voice_sessions WHERE id='missing-provider-start'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("accepted session remains durable");
    assert_eq!(state, "failed");
    assert!(error.is_some());
    drop(connection);

    let replay = voice::provider::start_admitted(
        store,
        None,
        "other-agent".into(),
        "missing-provider-start".into(),
        "v=0\r\n".into(),
        11.0,
    )
    .await
    .expect("replay returns the failed saved session");
    assert_eq!(replay["state"], "failed");
}

#[tokio::test]
async fn not_sent_voice_prepare_replays_the_persisted_intent_without_repreparing() {
    let (_root, store) = seeded_store().await;
    let store = Arc::new(store);
    let provider = Arc::new(PrepareNotSentProvider {
        prepare_entered: tokio::sync::Notify::new(),
        release_prepare: tokio::sync::Notify::new(),
        prepare_calls: AtomicUsize::new(0),
        request_calls: AtomicUsize::new(0),
    });

    let entered = provider.prepare_entered.notified();
    let first = voice::provider::start(
        store.clone(),
        provider.clone(),
        "other-agent".into(),
        "prepare-not-sent".into(),
        "v=0\r\n".into(),
        10.0,
    )
    .await
    .expect("provider setup does not delay HTTP acceptance");
    assert_eq!(first["state"], "connecting");
    tokio::time::timeout(std::time::Duration::from_secs(10), entered)
        .await
        .expect("provider preparation entered");
    let connection = store.test_reader().expect("read persisted start intent");
    let (state, ended, connection_id): (String, Option<f64>, Option<String>) = connection
        .query_row(
            "SELECT state,ended,connection_id FROM voice_sessions WHERE id='prepare-not-sent'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("failed prepare keeps its durable session");
    assert_eq!(state, "connecting");
    assert_eq!(ended, None);
    assert_eq!(connection_id, None);
    drop(connection);

    provider.release_prepare.notify_one();
    voice::provider::wait_for_start_setup("other-agent", "prepare-not-sent").await;
    let replay = voice::provider::start(
        store.clone(),
        provider.clone(),
        "other-agent".into(),
        "prepare-not-sent".into(),
        "v=0\r\n".into(),
        11.0,
    )
    .await
    .expect("the durable failed session is replayed");
    assert_eq!(replay["state"], "failed");
    let connection = store.test_reader().expect("read committed setup failure");
    let (state, ended): (String, Option<f64>) = connection
        .query_row(
            "SELECT state,ended FROM voice_sessions WHERE id='prepare-not-sent'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("failed voice session remains durable");
    assert_eq!(state, "failed");
    assert!(ended.is_some());
    assert_eq!(provider.prepare_calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.request_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unknown_provider_receipts_survive_restart_without_resending() {
    let (root, store) = seeded_store().await;
    let mut store = Arc::new(store);
    let provider = Arc::new(SignaledReceiptProvider {
        certainty: VoiceWriteCertainty::Unknown,
        requested: tokio::sync::Notify::new(),
        calls: Mutex::new(Vec::new()),
    });
    let requested = provider.requested.notified();

    let start = voice::provider::start(
        store.clone(),
        provider.clone(),
        "other-agent".into(),
        "matrix-start".into(),
        "v=0\r\n".into(),
        10.0,
    )
    .await;
    assert_eq!(
        start.expect("provider setup is asynchronous")["state"],
        "connecting"
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), requested)
        .await
        .expect("provider request dispatched");
    voice::provider::wait_for_start_setup("other-agent", "matrix-start").await;
    assert_eq!(provider.calls.lock().expect("provider calls").len(), 1);
    let replay_before_restart = voice::provider::start(
        store.clone(),
        provider.clone(),
        "other-agent".into(),
        "matrix-start".into(),
        "v=0\r\n".into(),
        10.5,
    )
    .await
    .expect("replay waits for the setup worker and returns its saved outcome");
    assert_eq!(replay_before_restart["state"], "unknown");
    drop(store);
    store = Arc::new(
        RuntimeStore::open(Some(&root.path().join("state")))
            .await
            .expect("restart after start"),
    );
    let replay = voice::provider::start(
        store.clone(),
        provider.clone(),
        "other-agent".into(),
        "matrix-start".into(),
        "v=0\r\n".into(),
        11.0,
    )
    .await
    .expect("start intent replays without dispatch");
    assert_eq!(replay["state"], "unknown");
    assert_eq!(provider.calls.lock().expect("calls").len(), 1);
    let changed_start = voice::provider::start(
        store.clone(),
        provider.clone(),
        "other-agent".into(),
        "matrix-start".into(),
        "v=0\r\nchanged\r\n".into(),
        11.5,
    )
    .await;
    assert!(
        matches!(
            changed_start,
            Err(voice::provider::VoiceProviderError::Policy(
                policy::VoiceError::SessionIdentityConflict
            ))
        ),
        "{changed_start:?}"
    );
    assert_eq!(provider.calls.lock().expect("calls").len(), 1);

    store.transaction(|tx| {
        let actor: String = tx.query_row("SELECT record FROM runtime_agents WHERE id='agent'", [], |row| row.get(0))?;
        let mut actor: serde_json::Value = serde_json::from_str(&actor)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        actor["autoWake"] = json!(true);
        tx.execute("UPDATE runtime_agents SET record=?1 WHERE id='agent'", [actor.to_string()])?;
        tx.execute("UPDATE voice_sessions SET native_thread='thread',account_key='account',connection_id='receipt-matrix-connection',state='ready' WHERE id='session'", [])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('matrix-speech','agent','','orchestrator','voice text','item','', '{}',1)", [])?;
        tx.execute("INSERT INTO voice_sessions(id,agent,created,account_key,connection_id,state) VALUES('matrix-approve','agent',1,'account','receipt-matrix-connection','ready')", [])?;
        let request = json!({"id":"matrix-request","rpcId":"matrix-rpc","agent":"agent","epoch":0,"status":"pending","method":"item/commandExecution/requestApproval","accountKey":"account","connectionId":"receipt-matrix-connection","params":{"command":"safe","threadId":"thread"}});
        let approval = studio_runtime_store::approvals::from_legacy_record("matrix-request", &request).expect("typed approval");
        studio_runtime_store::approvals::put(tx, &approval).expect("approval row");
        let request = serde_json::to_value(&approval).expect("approval json");
        let fingerprint = studio_runtime_store::sha256_hex(studio_runtime_store::python_json(&request).as_bytes());
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('matrix-approval-speech','agent','','orchestrator','approval','','',?1,1)", [json!({"request_id":"matrix-request","fingerprint":fingerprint}).to_string()])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('matrix-playback','agent','matrix-approve','playback','played','','',?1,2)", [json!({"record_id":"matrix-approval-speech"}).to_string()])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('matrix-transcript','agent','matrix-approve','user','разрешаю','','','{}',3)", [])?;
        tx.execute("INSERT INTO voice_sessions(id,agent,created,native_thread,account_key,connection_id,state) VALUES('matrix-end','agent',1,'thread','account','receipt-matrix-connection','ready')", [])?;
        save_scope(tx)?;
        Ok(())
    }).await.expect("seed speech, approval, and stop witness rows");

    assert!(matches!(
        voice::provider::speech(
            store.clone(),
            provider.clone(),
            "agent".into(),
            "session".into(),
            "matrix-speech".into(),
            None,
        )
        .await,
        Err(voice::provider::VoiceProviderError::Unknown)
    ));
    drop(store);
    store = Arc::new(
        RuntimeStore::open(Some(&root.path().join("state")))
            .await
            .expect("restart after speech"),
    );
    let receipt = voice::provider::read_speech_receipt(&store, "agent", "session", "matrix-speech")
        .await
        .expect("speech receipt read")
        .expect("saved speech receipt");
    assert_eq!(receipt["state"], "unknown");
    store
        .transaction(|tx| {
            tx.execute(
                "UPDATE voice_records SET text='changed speech' WHERE id='matrix-speech'",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("change speech content under its existing identity");
    let before_changed =
        voice::provider::read_speech_receipt(&store, "agent", "session", "matrix-speech")
            .await
            .expect("receipt before content conflict")
            .expect("receipt exists");
    let changed_speech = voice::provider::speech(
        store.clone(),
        provider.clone(),
        "agent".into(),
        "session".into(),
        "matrix-speech".into(),
        None,
    )
    .await
    .expect_err("changed speech content conflicts even after unknown outcome");
    assert!(matches!(
        changed_speech,
        voice::provider::VoiceProviderError::Policy(policy::VoiceError::SessionIdentityConflict)
    ));
    let after_changed =
        voice::provider::read_speech_receipt(&store, "agent", "session", "matrix-speech")
            .await
            .expect("receipt after content conflict")
            .expect("receipt exists");
    assert_eq!(before_changed, after_changed);
    assert_eq!(provider.calls.lock().expect("calls").len(), 2);

    let approval_result = voice::provider::approve(
        store.clone(),
        provider.clone(),
        "agent".into(),
        "matrix-approve".into(),
        "matrix-approval-speech".into(),
        "matrix-transcript".into(),
        12.0,
    )
    .await;
    assert!(
        matches!(
            approval_result,
            Err(voice::provider::VoiceProviderError::Unknown)
        ),
        "{approval_result:?}"
    );
    drop(store);
    store = Arc::new(
        RuntimeStore::open(Some(&root.path().join("state")))
            .await
            .expect("restart after approve"),
    );
    assert!(matches!(
        voice::provider::approve(
            store.clone(),
            provider.clone(),
            "agent".into(),
            "matrix-approve".into(),
            "matrix-approval-speech".into(),
            "matrix-transcript".into(),
            13.0
        )
        .await,
        Err(voice::provider::VoiceProviderError::Unknown)
    ));
    assert_eq!(provider.calls.lock().expect("calls").len(), 3);
    store
        .transaction(|tx| {
            tx.execute(
                "UPDATE voice_records SET text='not an approval' WHERE id='matrix-transcript'",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("change approval transcript under its existing identity");
    assert!(matches!(
        voice::provider::approve(
            store.clone(),
            provider.clone(),
            "agent".into(),
            "matrix-approve".into(),
            "matrix-approval-speech".into(),
            "matrix-transcript".into(),
            13.5,
        )
        .await,
        Err(voice::provider::VoiceProviderError::ContextChanged)
    ));
    assert_eq!(provider.calls.lock().expect("calls").len(), 3);

    assert!(matches!(
        voice::provider::end(
            store.clone(),
            provider.clone(),
            "agent".into(),
            "matrix-end".into(),
            14.0
        )
        .await,
        Err(voice::provider::VoiceProviderError::Unknown)
    ));
    drop(store);
    store = Arc::new(
        RuntimeStore::open(Some(&root.path().join("state")))
            .await
            .expect("restart after stop"),
    );
    let ended = voice::provider::end(
        store.clone(),
        provider.clone(),
        "agent".into(),
        "matrix-end".into(),
        15.0,
    )
    .await
    .expect("unknown stop remains held");
    assert_eq!(ended["state"], "unknown");
    assert_eq!(provider.calls.lock().expect("calls").len(), 4);
}

#[tokio::test]
async fn written_provider_effects_survive_restart_and_replay_once() {
    let (root, store) = seeded_store().await;
    let mut store = Arc::new(store);
    let provider = Arc::new(SignaledReceiptProvider {
        certainty: VoiceWriteCertainty::Written,
        requested: tokio::sync::Notify::new(),
        calls: Mutex::new(Vec::new()),
    });
    let requested = provider.requested.notified();

    let start_written = voice::provider::start(
        store.clone(),
        provider.clone(),
        "other-agent".into(),
        "written-start".into(),
        "v=0\r\n".into(),
        20.0,
    )
    .await
    .expect("provider setup is asynchronous");
    assert_eq!(start_written["state"], "connecting");
    tokio::time::timeout(std::time::Duration::from_secs(10), requested)
        .await
        .expect("provider request dispatched");
    voice::provider::wait_for_start_setup("other-agent", "written-start").await;
    assert_eq!(provider.calls.lock().expect("provider calls").len(), 1);
    let before_restart = voice::provider::start(
        store.clone(),
        provider.clone(),
        "other-agent".into(),
        "written-start".into(),
        "v=0\r\n".into(),
        20.5,
    )
    .await
    .expect("replay waits for the setup lease and returns the saved session");
    assert_eq!(before_restart["state"], "connecting");
    drop(store);
    store = Arc::new(
        RuntimeStore::open(Some(&root.path().join("state")))
            .await
            .expect("restart after written start"),
    );
    let start_replay = voice::provider::start(
        store.clone(),
        provider.clone(),
        "other-agent".into(),
        "written-start".into(),
        "v=0\r\n".into(),
        21.0,
    )
    .await
    .expect("start replay");
    assert_eq!(start_replay["state"], "connecting");
    assert_eq!(provider.calls.lock().expect("provider calls").len(), 1);

    store.transaction(|tx| {
        tx.execute("UPDATE voice_sessions SET native_thread='thread',account_key='account',connection_id='receipt-matrix-connection',state='ready' WHERE id='session'", [])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('written-fail-speech','agent','','orchestrator','spoken','item','', '{}',1)", [])?;
        tx.execute_batch("CREATE TRIGGER fail_speech_finalize BEFORE UPDATE OF state ON voice_speech_requests WHEN OLD.id='session:written-fail-speech' BEGIN SELECT RAISE(ABORT,'crash after provider Written'); END;")?;
        let actor: String = tx.query_row("SELECT record FROM runtime_agents WHERE id='agent'", [], |row| row.get(0))?;
        let mut actor: serde_json::Value = serde_json::from_str(&actor).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        actor["autoWake"] = json!(true);
        tx.execute("UPDATE runtime_agents SET record=?1 WHERE id='agent'", [actor.to_string()])?;
        tx.execute("INSERT INTO voice_sessions(id,agent,created,account_key,connection_id,state) VALUES('written-approval','agent',1,'account','receipt-matrix-connection','ready')", [])?;
        let request = json!({"id":"written-request","rpcId":"written-rpc","agent":"agent","epoch":0,"status":"pending","method":"item/commandExecution/requestApproval","accountKey":"account","connectionId":"receipt-matrix-connection","params":{"command":"safe","threadId":"thread"}});
        let approval = studio_runtime_store::approvals::from_legacy_record("written-request", &request).expect("typed approval");
        studio_runtime_store::approvals::put(tx, &approval).expect("approval row");
        let request = serde_json::to_value(&approval).expect("approval json");
        let fingerprint = studio_runtime_store::sha256_hex(studio_runtime_store::python_json(&request).as_bytes());
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('written-approval-speech','agent','','orchestrator','approval','','',?1,1)", [json!({"request_id":"written-request","fingerprint":fingerprint}).to_string()])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('written-approval-playback','agent','written-approval','playback','played','','',?1,2)", [json!({"record_id":"written-approval-speech"}).to_string()])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('written-approval-transcript','agent','written-approval','user','разрешаю','','','{}',3)", [])?;
        tx.execute_batch("CREATE TRIGGER fail_approval_finalize BEFORE UPDATE ON voice_approvals WHEN OLD.id='written-request' BEGIN SELECT RAISE(ABORT,'crash after provider Written'); END;")?;
        tx.execute("INSERT INTO voice_sessions(id,agent,created,native_thread,account_key,connection_id,state) VALUES('written-end','agent',1,'thread','account','receipt-matrix-connection','ready')", [])?;
        save_scope(tx)?;
        Ok(())
    }).await.expect("seed written-effect failpoints");

    assert!(
        voice::provider::speech(
            store.clone(),
            provider.clone(),
            "agent".into(),
            "session".into(),
            "written-fail-speech".into(),
            None
        )
        .await
        .is_err()
    );
    drop(store);
    store = Arc::new(
        RuntimeStore::open(Some(&root.path().join("state")))
            .await
            .expect("restart after speech finalization failure"),
    );
    assert_eq!(
        voice::provider::speech(
            store.clone(),
            provider.clone(),
            "agent".into(),
            "session".into(),
            "written-fail-speech".into(),
            None
        )
        .await
        .expect("speech replay")["state"],
        "pending"
    );

    let approval_error = voice::provider::approve(
        store.clone(),
        provider.clone(),
        "agent".into(),
        "written-approval".into(),
        "written-approval-speech".into(),
        "written-approval-transcript".into(),
        22.0,
    )
    .await
    .expect_err("finalization trigger aborts accepted receipt");
    assert_eq!(
        provider.calls.lock().expect("provider calls").len(),
        3,
        "approval provider reply must be written before finalization fails: {approval_error:?}"
    );
    drop(store);
    store = Arc::new(
        RuntimeStore::open(Some(&root.path().join("state")))
            .await
            .expect("restart after approval finalization failure"),
    );
    assert!(
        voice::provider::approve(
            store.clone(),
            provider.clone(),
            "agent".into(),
            "written-approval".into(),
            "written-approval-speech".into(),
            "written-approval-transcript".into(),
            23.0
        )
        .await
        .is_err()
    );

    let end_written = voice::provider::end(
        store.clone(),
        provider.clone(),
        "agent".into(),
        "written-end".into(),
        24.0,
    )
    .await
    .expect("provider reports Written before the process restart");
    assert_eq!(end_written["state"], "ended");
    assert_eq!(provider.calls.lock().expect("provider calls").len(), 4);
    drop(store);
    store = Arc::new(
        RuntimeStore::open(Some(&root.path().join("state")))
            .await
            .expect("restart after written end"),
    );
    let end_replay = voice::provider::end(
        store,
        provider.clone(),
        "agent".into(),
        "written-end".into(),
        25.0,
    )
    .await
    .expect("end replay");
    assert_eq!(end_replay["state"], "ended");
    assert_eq!(provider.calls.lock().expect("provider calls").len(), 4);
}

#[tokio::test]
async fn start_admission_finishes_before_end_dispatches_stop() {
    let (_root, store) = seeded_store().await;
    studio_runtime_services::account_worker::attach_provider_stream_for_test(
        &store,
        "account",
        None,
        1,
        "voice-corpus-connection",
    )
    .await
    .expect("attach blocking provider");
    let provider = Arc::new(BlockingProvider {
        start_entered: tokio::sync::Notify::new(),
        release_start: tokio::sync::Notify::new(),
        stop_entered: tokio::sync::Notify::new(),
        methods: Mutex::new(Vec::new()),
    });
    let start_signal = provider.start_entered.notified();
    let start_store = Arc::new(store);
    let start_provider: Arc<dyn VoiceProviderPort> = provider.clone();
    let starting = tokio::spawn(voice::provider::start(
        start_store.clone(),
        start_provider,
        "agent".into(),
        "dispatch-session".into(),
        "v=0\r\n".into(),
        1.0,
    ));
    tokio::time::timeout(std::time::Duration::from_secs(10), start_signal)
        .await
        .expect("start reaches provider");

    let stop_signal = provider.stop_entered.notified();
    let end_store = start_store.clone();
    let end_provider: Arc<dyn VoiceProviderPort> = provider.clone();
    let ending = tokio::spawn(voice::provider::end(
        end_store,
        end_provider,
        "agent".into(),
        "dispatch-session".into(),
        2.0,
    ));
    provider.release_start.notify_one();
    starting
        .await
        .expect("start task join")
        .expect("start succeeds");
    tokio::time::timeout(std::time::Duration::from_secs(10), stop_signal)
        .await
        .expect("stop reaches provider");
    ending.await.expect("end task join").expect("end succeeds");
    assert_eq!(
        provider
            .methods
            .lock()
            .expect("provider call log")
            .as_slice(),
        &[Method::ThreadRealtimeStart, Method::ThreadRealtimeStop]
    );
}
