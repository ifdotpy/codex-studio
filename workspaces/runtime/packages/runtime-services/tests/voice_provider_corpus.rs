use serde_json::json;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use studio_runtime_providers::Method;
use studio_runtime_services::voice;
use studio_runtime_services::voice::policy::{
    self, SessionSignal, VoiceSession, VoiceSessionRow, VoiceSessionState,
};
use studio_runtime_services::voice::provider::notification_requires_stop;
use studio_runtime_services::voice::provider::{
    ProviderFuture, VoiceCallbackActor, VoiceProviderContext, VoiceProviderFailure,
    VoiceProviderPort, VoiceRpcCall,
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

#[test]
fn provider_error_callback_stops_and_keeps_failure_terminal() {
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

#[test]
fn session_machine_distinguishes_admitted_stop_rejection_and_disconnect() {
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

#[test]
fn realtime_error_notifications_require_stop_rpc() {
    assert!(notification_requires_stop(&json!({
        "method": "thread/realtime/error",
        "params": {"threadId": "thread"}
    })));
    assert!(!notification_requires_stop(&json!({
        "method": "thread/realtime/closed",
        "params": {"threadId": "thread"}
    })));
}

fn seed_callback_session(store: &RuntimeStore, id: &str, thread: &str, connection: &str) {
    store
        .transaction(|tx| {
            tx.execute(
                "INSERT INTO voice_sessions(id,agent,created,native_thread,account_key,connection_id,state) \
                 VALUES(?1,'agent',1,?2,'account',?3,'connecting')",
                rusqlite::params![id, thread, connection],
            )?;
            Ok(())
        })
        .expect("seed callback session");
}

#[test]
fn callback_actor_prefers_canonical_transcript_over_fallback_delta() {
    let (_root, store) = seeded_store();
    seed_callback_session(
        &store,
        "canonical-session",
        "canonical-thread",
        "voice-connection",
    );
    let actor = VoiceCallbackActor::default();
    let started = json!({
        "method":"thread/realtime/item/started",
        "params":{"threadId":"canonical-thread","item":{"type":"transcriptSegment","realtimeSessionId":"canonical-session"}}
    });
    assert!(
        actor
            .handle_notification(&store, "account", "voice-connection", &started, 2.0)
            .expect("canonical starts")
    );
    let delta = json!({
        "method":"thread/realtime/transcript/delta",
        "params":{"threadId":"canonical-thread","realtimeSessionId":"canonical-session","role":"assistant","delta":"fallback"}
    });
    actor
        .handle_notification(&store, "account", "voice-connection", &delta, 3.0)
        .expect("delta handled");
    let completed = json!({
        "method":"thread/realtime/item/completed",
        "params":{"threadId":"canonical-thread","item":{"type":"transcriptSegment","id":"item-1","realtimeSessionId":"canonical-session","role":"assistant","text":"canonical transcript"}}
    });
    actor
        .handle_notification(&store, "account", "voice-connection", &completed, 4.0)
        .expect("canonical completes");
    let closed = json!({
        "method":"thread/realtime/closed",
        "params":{"threadId":"canonical-thread","realtimeSessionId":"canonical-session"}
    });
    actor
        .handle_notification(&store, "account", "voice-connection", &closed, 5.0)
        .expect("close cleans actor state");

    let connection = store.read().expect("read transcript records");
    let rows = connection
        .prepare("SELECT text FROM voice_records WHERE session='canonical-session' ORDER BY seq")
        .expect("voice record query")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("voice records")
        .collect::<Result<Vec<_>, _>>()
        .expect("records decoded");
    assert_eq!(rows, ["canonical transcript"]);
}

#[test]
fn callback_actor_flushes_fallback_tail_when_provider_closes() {
    let (_root, store) = seeded_store();
    seed_callback_session(
        &store,
        "fallback-session",
        "fallback-thread",
        "voice-connection",
    );
    let actor = VoiceCallbackActor::default();
    let delta = json!({
        "method":"thread/realtime/transcript/delta",
        "params":{"threadId":"fallback-thread","realtimeSessionId":"fallback-session","role":"user","delta":"partial words"}
    });
    actor
        .handle_notification(&store, "account", "voice-connection", &delta, 2.0)
        .expect("delta buffered");
    let closed = json!({
        "method":"thread/realtime/closed",
        "params":{"threadId":"fallback-thread","realtimeSessionId":"fallback-session","reason":"network"}
    });
    actor
        .handle_notification(&store, "account", "voice-connection", &closed, 3.0)
        .expect("close flushes fallback");

    let connection = store.read().expect("read flushed transcript");
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

#[test]
fn callback_actor_discards_fallback_tail_when_transcript_is_finalized_empty() {
    let (_root, store) = seeded_store();
    seed_callback_session(
        &store,
        "discard-session",
        "discard-thread",
        "voice-connection",
    );
    let actor = VoiceCallbackActor::default();
    let delta = json!({
        "method":"thread/realtime/transcript/delta",
        "params":{"threadId":"discard-thread","realtimeSessionId":"discard-session","role":"user","delta":"discard me"}
    });
    actor
        .handle_notification(&store, "account", "voice-connection", &delta, 2.0)
        .expect("delta buffered");
    let done = json!({
        "method":"thread/realtime/transcript/done",
        "params":{"threadId":"discard-thread","realtimeSessionId":"discard-session","role":"user","text":""}
    });
    actor
        .handle_notification(&store, "account", "voice-connection", &done, 3.0)
        .expect("empty done clears tail");
    let closed = json!({
        "method":"thread/realtime/closed",
        "params":{"threadId":"discard-thread","realtimeSessionId":"discard-session"}
    });
    actor
        .handle_notification(&store, "account", "voice-connection", &closed, 4.0)
        .expect("close completes");
    let connection = store.read().expect("read transcript records");
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM voice_records WHERE session='discard-session'",
            [],
            |row| row.get(0),
        )
        .expect("record count");
    assert_eq!(count, 0);
}

#[test]
fn callback_actor_refuses_deltas_over_the_record_text_bound() {
    let (_root, store) = seeded_store();
    seed_callback_session(
        &store,
        "bounded-session",
        "bounded-thread",
        "voice-connection",
    );
    let actor = VoiceCallbackActor::default();
    let delta = json!({
        "method":"thread/realtime/transcript/delta",
        "params":{"threadId":"bounded-thread","realtimeSessionId":"bounded-session","role":"assistant","delta":"x".repeat(100_001)}
    });
    assert!(
        actor
            .handle_notification(&store, "account", "voice-connection", &delta, 2.0)
            .is_err()
    );
}

#[test]
fn callback_actor_flushes_fallback_tail_and_loses_session_on_disconnect() {
    let (_root, store) = seeded_store();
    seed_callback_session(
        &store,
        "disconnect-session",
        "disconnect-thread",
        "disconnect-connection",
    );
    let actor = VoiceCallbackActor::default();
    let delta = json!({
        "method":"thread/realtime/transcript/delta",
        "params":{"threadId":"disconnect-thread","realtimeSessionId":"disconnect-session","role":"assistant","delta":"last words"}
    });
    actor
        .handle_notification(&store, "account", "disconnect-connection", &delta, 2.0)
        .expect("delta buffered");
    assert_eq!(
        actor
            .handle_disconnect(&store, "account", "disconnect-connection", 3.0)
            .expect("disconnect closes actor"),
        1
    );
    let connection = store.read().expect("read disconnected state");
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

fn seeded_store() -> (tempfile::TempDir, RuntimeStore) {
    let root = if let Some(tmpfs) = std::env::var_os("STUDIO_RUNTIME_CORPUS_TMP") {
        tempfile::Builder::new()
            .prefix("voice-state-")
            .tempdir_in(tmpfs)
            .expect("temporary voice state root on configured tmpfs")
    } else {
        tempfile::tempdir().expect("temporary voice state root")
    };
    let python = std::env::var_os("CODEX_AGENTS_PYTHON").unwrap_or_else(|| "python3".into());
    let template = root.path().join("python-template");
    let state = root.path().join("state");
    let codex_home = root.path().join("codex-home");
    let status = Command::new(python)
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/voice_provider/prepare_state_template.py"),
        )
        .arg(&template)
        .env("CODEX_HOME", codex_home)
        .env("CODEX_NODE", "/usr/bin/node")
        .status()
        .expect("Python writes the disposable runtime fixture");
    assert!(status.success(), "Python fixture creation failed");
    fs::create_dir(&state).expect("disposable clone state root");
    for database in ["canvas.sqlite3", "analytics.sqlite3"] {
        fs::copy(template.join(database), state.join(database)).expect("clone Python template");
    }
    let store = RuntimeStore::open(Some(&state)).expect("open voice fixture store");
    store
        .transaction(|connection| {
            for agent in ["agent", "other-agent"] {
                let record = json!({
                    "id": agent,
                    "rootId": agent,
                    "epoch": 0,
                    "status": "idle",
                    "isLead": true,
                    "threadId": "thread",
                    "accountKey": "account"
                });
                connection.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                    rusqlite::params![agent, record.to_string()],
                )?;
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
        .expect("seed agent and session rows");
    (root, store)
}

#[test]
fn providerless_receipt_replay_checks_agent_session_and_record_tuple() {
    let (_root, store) = seeded_store();
    store
        .transaction(|connection| {
            connection.execute(
                "INSERT INTO voice_speech_requests(id,session,record_id,state,error) VALUES(?1,?2,?3,'unknown','private')",
                rusqlite::params!["session:one:record", "session:one", "record"],
            )?;
            Ok(())
        })
        .expect("seed existing Python-format receipt");

    let replay = voice::provider::read_speech_receipt(&store, "agent", "session:one", "record")
        .expect("owned exact receipt is safe to replay");
    assert_eq!(replay.expect("receipt exists")["record_id"], "record");
    assert!(
        voice::provider::read_speech_receipt(
            &store,
            "other-agent",
            "owned-by-agent",
            "private-record",
        )
        .expect("foreign session is refused without disclosure")
        .is_none()
    );
    assert!(
        voice::provider::read_speech_receipt(&store, "agent", "session", "one:record").is_err()
    );
}

#[test]
fn audio_validation_precedes_scoped_expiry_pruning() {
    let (_root, store) = seeded_store();
    let directory = store.root().join("voice-audio");
    fs::create_dir_all(&directory).expect("voice audio directory");
    let other_session_expired = directory.join("s-another-session-c-expired");
    fs::write(&other_session_expired, b"other session data").expect("expired chunk");
    let first = voice::save_audio(
        &store,
        "agent",
        "session",
        "expire-me",
        "AQID",
        "audio/webm",
    )
    .expect("first synthetic upload");
    let record_id = first["id"].as_str().expect("record id");
    let payload: String = store
        .read()
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

    assert!(voice::save_audio(&store, "agent", "session", "bad", "%%%", "text/plain").is_err());
    assert!(
        other_session_expired.exists(),
        "invalid upload must not prune"
    );
    assert!(
        own_expired.exists(),
        "invalid upload must preserve own expired data"
    );
    voice::save_audio(&store, "agent", "session", "valid", "AQID", "audio/webm")
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

impl VoiceProviderPort for BlockingProvider {
    fn prepare<'a>(
        &'a self,
        agent_id: &'a str,
        account_key: &'a str,
        thread_id: &'a str,
        epoch: i64,
    ) -> ProviderFuture<'a, Result<VoiceProviderContext, VoiceProviderFailure>> {
        Box::pin(async move {
            Ok(VoiceProviderContext {
                agent_id: agent_id.to_owned(),
                epoch,
                account_key: account_key.to_owned(),
                thread_id: thread_id.to_owned(),
                connection_id: "voice-corpus-connection".into(),
            })
        })
    }

    fn request<'a>(
        &'a self,
        call: VoiceRpcCall,
    ) -> ProviderFuture<'a, Result<(), VoiceProviderFailure>> {
        Box::pin(async move {
            self.methods
                .lock()
                .expect("provider call log")
                .push(call.method);
            match call.method {
                Method::ThreadRealtimeStart => {
                    self.start_entered.notify_one();
                    self.release_start.notified().await;
                }
                Method::ThreadRealtimeStop => self.stop_entered.notify_one(),
                _ => {}
            }
            Ok(())
        })
    }
}

#[tokio::test]
async fn start_admission_finishes_before_end_dispatches_stop() {
    let (_root, store) = seeded_store();
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
    start_signal.await;

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
    tokio::task::yield_now().await;
    assert!(
        !ending.is_finished(),
        "end must wait behind start admission"
    );
    assert!(
        provider
            .methods
            .lock()
            .expect("provider call log")
            .as_slice()
            == [Method::ThreadRealtimeStart]
    );

    provider.release_start.notify_one();
    starting
        .await
        .expect("start task join")
        .expect("start succeeds");
    stop_signal.await;
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
