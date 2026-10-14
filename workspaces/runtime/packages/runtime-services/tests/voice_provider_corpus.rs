#[path = "support/prepared_python.rs"]
mod python_test_support;
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
use studio_runtime_services::voice::provider::VoiceCallbackActor;
use studio_runtime_services::voice::provider::notification_requires_stop;
use studio_runtime_services::voice::runtime::{
    VoiceConnectionScope as VoiceProviderContext, VoiceFuture as ProviderFuture, VoiceReply,
    VoiceRequestPort as VoiceProviderPort, VoiceRequestReceipt, VoiceWriteCertainty,
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
    let python = python_test_support::prepared_python(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
    );
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

#[tokio::test]
async fn python_written_speech_receipt_replays_without_provider_request() {
    let (_root, store) = seeded_store();
    store.transaction(|tx| {
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('cutover-record','agent','session:one','orchestrator','spoken before cutover','item','', '{}',1)", [])?;
        tx.execute("INSERT INTO voice_speech_requests(id,session,record_id,state,error) VALUES('session:one:cutover-record','session:one','cutover-record','submitted',NULL)", [])?;
        Ok(())
    }).expect("seed Python-written speech receipt");
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
    let (_root, store) = seeded_store();
    let store = Arc::new(store);
    store.transaction(|tx| {
        tx.execute("UPDATE voice_sessions SET native_thread='thread',account_key='account',connection_id='receipt-matrix-connection',state='ready' WHERE id='session'", [])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('written-record','agent','','orchestrator','original text','item','', '{}',1)", [])?;
        Ok(())
    }).expect("seed ready session and speech row");
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
        .expect("receipt read")
        .expect("receipt exists");
    store
        .transaction(|tx| {
            tx.execute(
                "UPDATE voice_records SET text='changed text' WHERE id='written-record'",
                [],
            )?;
            Ok(())
        })
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
        .expect("receipt read")
        .expect("receipt exists");
    assert_eq!(before, after);
    assert_eq!(provider.calls.lock().expect("calls").len(), 1);
}

#[test]
fn python_approval_speech_rows_and_voice_views_match_rust() {
    let (root, store) = seeded_store();
    let request = json!({
        "id":"oracle-approval", "agent":"agent", "epoch":0,
        "status":"pending", "method":"item/commandExecution/requestApproval",
        "params":{"command":"printf safe", "cwd":"/tmp"}
    });
    store
        .transaction(|tx| {
            tx.execute(
                "INSERT INTO runtime_requests(id,record) VALUES(?1,?2)",
                rusqlite::params!["oracle-approval", request.to_string()],
            )?;
            Ok(())
        })
        .expect("seed Python-readable approval");

    let python = python_test_support::prepared_python(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
    );
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/voice_provider/python_oracle.py");
    let output = Command::new(python)
        .arg(script)
        .arg(root.path().join("state"))
        .arg("agent")
        .arg("oracle-approval")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("run guarded Python voice oracle");
    assert!(
        output.status.success(),
        "Python oracle: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: serde_json::Value = serde_json::from_slice(&output.stdout).expect("oracle JSON");

    let approvals = voice::approvals(&store, "agent").expect("Rust approvals");
    assert_eq!(approvals, expected["approvals"]);
    let records = voice::records(&store, "agent", 0).expect("Rust records view");
    assert_eq!(records, expected["records"]);
    let speech = voice::approval_speech(&store, "agent", "oracle-approval")
        .expect("Rust reads Python-written approval speech");
    assert_eq!(speech, expected["approval"]);

    let connection = store.read().expect("full voice record rows");
    let rows = connection.prepare(
        "SELECT seq,id,agent,session,kind,text,item_id,previous_item_id,payload,created \
         FROM voice_records WHERE agent='agent' ORDER BY seq",
    ).expect("voice rows query").query_map([], |row| {
        Ok(json!({"seq":row.get::<_,i64>(0)?,"id":row.get::<_,String>(1)?,"agent":row.get::<_,String>(2)?,
          "session":row.get::<_,String>(3)?,"kind":row.get::<_,String>(4)?,"text":row.get::<_,String>(5)?,
          "item_id":row.get::<_,String>(6)?,"previous_item_id":row.get::<_,String>(7)?,
          "payload":row.get::<_,String>(8)?,"created":row.get::<_,f64>(9)?}))
    }).expect("voice rows").collect::<Result<Vec<_>,_>>().expect("decode rows");
    assert_eq!(json!(rows), expected["rows"]);
    assert_eq!(
        table_rows(&connection, "runtime_events", "created,id"),
        expected["events"]
    );
    assert_eq!(
        table_rows(&connection, "sync_entities", "collection,id"),
        expected["sync_entities"]
    );
    assert_eq!(
        table_rows(&connection, "sync_documents", "scope,id,seq"),
        expected["sync_documents"]
    );
}

#[tokio::test]
async fn real_python_start_speech_end_rows_match_rust_with_rpc_boundary_stub() {
    let (root, store) = seeded_store();
    let python_root = root.path().join("python-lifecycle-state");
    fs::create_dir(&python_root).expect("Python lifecycle state root");
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        fs::copy(store.root().join(name), python_root.join(name))
            .expect("clone Python lifecycle fixture");
    }
    let python = python_test_support::prepared_python(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
    );
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/voice_provider/lifecycle_oracle.py");
    let output = Command::new(python)
        .arg(script)
        .arg(&python_root)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("run real Python lifecycle functions");
    assert!(
        output.status.success(),
        "Python lifecycle oracle: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("Python lifecycle JSON");

    store.transaction(|tx| {
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('oracle-speech','agent','oracle-session','orchestrator','hello','item','', '{}',100)", [])?;
        Ok(())
    }).expect("seed Rust speech row");
    let store = Arc::new(store);
    let provider = Arc::new(ReceiptProvider {
        certainty: VoiceWriteCertainty::Written,
        calls: Mutex::new(Vec::new()),
    });
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
    voice::provider::handle_notification(&store, "account", "receipt-matrix-connection", &json!({"method":"thread/realtime/sdp","params":{"threadId":"thread","realtimeSessionId":"oracle-session","sdp":"answer"}}), 100.0).expect("Rust SDP callback");
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

    let connection = store.read().expect("Rust lifecycle rows");
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
            python_rows
                .as_array_mut()
                .expect("Python voice-session array")
                .retain(|row| row["id"] == "oracle-session");
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

struct ReceiptProvider {
    certainty: VoiceWriteCertainty,
    calls: Mutex<Vec<(String, Method)>>,
}

impl VoiceProviderPort for ReceiptProvider {
    fn prepare<'a>(
        &'a self,
        agent_id: &'a str,
        account_key: &'a str,
        thread_id: &'a str,
        epoch: i64,
    ) -> ProviderFuture<'a, Result<VoiceProviderContext, VoiceWriteCertainty>> {
        Box::pin(async move {
            Ok(VoiceProviderContext {
                agent_id: agent_id.to_owned(),
                epoch,
                account_key: account_key.to_owned(),
                thread_id: thread_id.to_owned(),
                connection_id: "receipt-matrix-connection".into(),
            })
        })
    }

    fn request<'a>(
        &'a self,
        _scope: VoiceProviderContext,
        operation_id: String,
        method: Method,
        _params: serde_json::Value,
        deadline: std::time::Instant,
    ) -> ProviderFuture<'a, VoiceRequestReceipt> {
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

impl VoiceProviderPort for BlockingProvider {
    fn prepare<'a>(
        &'a self,
        agent_id: &'a str,
        account_key: &'a str,
        thread_id: &'a str,
        epoch: i64,
    ) -> ProviderFuture<'a, Result<VoiceProviderContext, VoiceWriteCertainty>> {
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
        scope: VoiceProviderContext,
        operation_id: String,
        method: Method,
        params: serde_json::Value,
        deadline: std::time::Instant,
    ) -> ProviderFuture<'a, VoiceRequestReceipt> {
        Box::pin(async move {
            self.methods.lock().expect("provider call log").push(method);
            match method {
                Method::ThreadRealtimeStart => {
                    self.start_entered.notify_one();
                    self.release_start.notified().await;
                }
                Method::ThreadRealtimeStop => self.stop_entered.notify_one(),
                _ => {}
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
    let (_root, store) = seeded_store();
    store
        .transaction(|connection| {
            connection.execute(
                "UPDATE voice_sessions SET native_thread='thread',account_key='account',connection_id='voice-corpus-connection',state='unknown' WHERE id='session'",
                [],
            )?;
            Ok(())
        })
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

    let (_root, store) = seeded_store();
    store.transaction(|tx| {
        tx.execute_batch("CREATE TRIGGER fail_start_intent BEFORE INSERT ON voice_sessions WHEN NEW.id='matrix-start-fail' BEGIN SELECT RAISE(ABORT,'injected'); END;")?;
        Ok(())
    }).expect("install start intent fail point");
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

    let (_root, store) = seeded_store();
    store.transaction(|tx| {
        tx.execute("UPDATE voice_sessions SET native_thread='thread',account_key='account',connection_id='receipt-matrix-connection',state='ready' WHERE id='session'", [])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('matrix-speech-fail','agent','','orchestrator','spoken','item','', '{}',1)", [])?;
        tx.execute_batch("CREATE TRIGGER fail_speech_intent BEFORE INSERT ON voice_speech_requests BEGIN SELECT RAISE(ABORT,'injected'); END;")?;
        Ok(())
    }).expect("install speech intent fail point");
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

    let (_root, store) = seeded_store();
    store.transaction(|tx| {
        let actor: String = tx.query_row("SELECT record FROM runtime_agents WHERE id='agent'", [], |row| row.get(0))?;
        let mut actor: serde_json::Value = serde_json::from_str(&actor).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        actor["autoWake"] = json!(true);
        tx.execute("UPDATE runtime_agents SET record=?1 WHERE id='agent'", [actor.to_string()])?;
        tx.execute("UPDATE voice_sessions SET native_thread='thread',account_key='account',connection_id='receipt-matrix-connection',state='ready' WHERE id='session'", [])?;
        tx.execute("INSERT INTO voice_sessions(id,agent,created,account_key,connection_id,state) VALUES('matrix-approve-fail','agent',1,'account','receipt-matrix-connection','ready')", [])?;
        let request = json!({"id":"matrix-approve-fail-request","rpcId":"matrix-rpc-fail","agent":"agent","epoch":0,"status":"pending","method":"item/commandExecution/requestApproval","accountKey":"account","connectionId":"receipt-matrix-connection","params":{"command":"safe","threadId":"thread"}});
        let fingerprint = studio_runtime_store::sha256_hex(studio_runtime_store::python_json(&request).as_bytes());
        tx.execute("INSERT INTO runtime_requests(id,record) VALUES('matrix-approve-fail-request',?1)", [request.to_string()])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('matrix-approve-fail-speech','agent','','orchestrator','approval','','',?1,1)", [json!({"request_id":"matrix-approve-fail-request","fingerprint":fingerprint}).to_string()])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('matrix-approve-fail-playback','agent','matrix-approve-fail','playback','played','','',?1,2)", [json!({"record_id":"matrix-approve-fail-speech"}).to_string()])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('matrix-approve-fail-transcript','agent','matrix-approve-fail','user','разрешаю','','','{}',3)", [])?;
        tx.execute_batch("CREATE TRIGGER fail_approval_intent BEFORE INSERT ON voice_approvals BEGIN SELECT RAISE(ABORT,'injected'); END;")?;
        Ok(())
    }).expect("install approval intent fail point");
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

    let (_root, store) = seeded_store();
    store.transaction(|tx| {
        tx.execute("UPDATE voice_sessions SET native_thread='thread',account_key='account',connection_id='receipt-matrix-connection',state='ready' WHERE id='session'", [])?;
        tx.execute_batch("CREATE TRIGGER fail_end_intent BEFORE UPDATE ON voice_sessions WHEN OLD.id='session' BEGIN SELECT RAISE(ABORT,'injected'); END;")?;
        Ok(())
    }).expect("install end intent fail point");
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
    let (_root, store) = seeded_store();
    store.transaction(|tx| {
        let actor: String = tx.query_row("SELECT record FROM runtime_agents WHERE id='agent'", [], |row| row.get(0))?;
        let mut actor: serde_json::Value = serde_json::from_str(&actor)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        actor["autoWake"] = json!(true);
        tx.execute("UPDATE runtime_agents SET record=?1 WHERE id='agent'", [actor.to_string()])?;
        tx.execute("INSERT INTO voice_sessions(id,agent,created,account_key,connection_id,state) VALUES('connection-bound-approval','agent',1,'account','receipt-matrix-connection','ready')", [])?;
        let request = json!({
            "id":"request-from-prior-generation", "rpcId":"prior-rpc", "agent":"agent", "epoch":0,
            "status":"pending", "method":"item/commandExecution/requestApproval",
            "accountKey":"account", "connectionId":"another-connection-generation",
            "params":{"command":"safe", "threadId":"thread"}
        });
        let fingerprint = studio_runtime_store::sha256_hex(studio_runtime_store::python_json(&request).as_bytes());
        tx.execute("INSERT INTO runtime_requests(id,record) VALUES('request-from-prior-generation',?1)", [request.to_string()])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('connection-bound-speech','agent','','orchestrator','approval','','',?1,1)", [json!({"request_id":"request-from-prior-generation","fingerprint":fingerprint}).to_string()])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('connection-bound-playback','agent','connection-bound-approval','playback','played','','',?1,2)", [json!({"record_id":"connection-bound-speech"}).to_string()])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('connection-bound-transcript','agent','connection-bound-approval','user','разрешаю','','','{}',3)", [])?;
        Ok(())
    }).expect("seed an approval tied to another connection generation");
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
    let connection = store.read().expect("read refusal state");
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM voice_approvals", [], |row| row
                .get::<_, i64>(0))
            .expect("approval count"),
        0
    );
    let request_record: String = connection
        .query_row(
            "SELECT record FROM runtime_requests WHERE id='request-from-prior-generation'",
            [],
            |row| row.get(0),
        )
        .expect("request record");
    let request: serde_json::Value =
        serde_json::from_str(&request_record).expect("decode request record");
    assert_eq!(request["status"], "pending");
}

#[tokio::test]
async fn unknown_provider_receipts_survive_restart_without_resending() {
    let (root, store) = seeded_store();
    let mut store = Arc::new(store);
    let provider = Arc::new(ReceiptProvider {
        certainty: VoiceWriteCertainty::Unknown,
        calls: Mutex::new(Vec::new()),
    });

    let start = voice::provider::start(
        store.clone(),
        provider.clone(),
        "other-agent".into(),
        "matrix-start".into(),
        "v=0\r\n".into(),
        10.0,
    )
    .await;
    assert!(matches!(
        start,
        Err(voice::provider::VoiceProviderError::Unknown)
    ));
    drop(store);
    store = Arc::new(
        RuntimeStore::open(Some(&root.path().join("state"))).expect("restart after start"),
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
        tx.execute("INSERT INTO runtime_requests(id,record) VALUES('matrix-request',?1)", [json!({"id":"matrix-request","rpcId":"matrix-rpc","agent":"agent","epoch":0,"status":"pending","method":"item/commandExecution/requestApproval","accountKey":"account","connectionId":"receipt-matrix-connection","params":{"command":"safe","threadId":"thread"}}).to_string()])?;
        let request = tx.query_row("SELECT record FROM runtime_requests WHERE id='matrix-request'", [], |row| row.get::<_,String>(0))?;
        let request: serde_json::Value = serde_json::from_str(&request).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        let fingerprint = studio_runtime_store::sha256_hex(studio_runtime_store::python_json(&request).as_bytes());
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('matrix-approval-speech','agent','','orchestrator','approval','','',?1,1)", [json!({"request_id":"matrix-request","fingerprint":fingerprint}).to_string()])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('matrix-playback','agent','matrix-approve','playback','played','','',?1,2)", [json!({"record_id":"matrix-approval-speech"}).to_string()])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('matrix-transcript','agent','matrix-approve','user','разрешаю','','','{}',3)", [])?;
        tx.execute("INSERT INTO voice_sessions(id,agent,created,native_thread,account_key,connection_id,state) VALUES('matrix-end','agent',1,'thread','account','receipt-matrix-connection','ready')", [])?;
        Ok(())
    }).expect("seed speech, approval, and stop witness rows");

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
        RuntimeStore::open(Some(&root.path().join("state"))).expect("restart after speech"),
    );
    let receipt = voice::provider::read_speech_receipt(&store, "agent", "session", "matrix-speech")
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
        .expect("change speech content under its existing identity");
    let before_changed =
        voice::provider::read_speech_receipt(&store, "agent", "session", "matrix-speech")
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
        RuntimeStore::open(Some(&root.path().join("state"))).expect("restart after approve"),
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
    store =
        Arc::new(RuntimeStore::open(Some(&root.path().join("state"))).expect("restart after stop"));
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
    let (root, store) = seeded_store();
    let mut store = Arc::new(store);
    let provider = Arc::new(ReceiptProvider {
        certainty: VoiceWriteCertainty::Written,
        calls: Mutex::new(Vec::new()),
    });

    let start_written = voice::provider::start(
        store.clone(),
        provider.clone(),
        "other-agent".into(),
        "written-start".into(),
        "v=0\r\n".into(),
        20.0,
    )
    .await
    .expect("provider reports Written before the process restart");
    assert_eq!(start_written["state"], "connecting");
    assert_eq!(provider.calls.lock().expect("provider calls").len(), 1);
    drop(store);
    store = Arc::new(
        RuntimeStore::open(Some(&root.path().join("state"))).expect("restart after written start"),
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
        let fingerprint = studio_runtime_store::sha256_hex(studio_runtime_store::python_json(&request).as_bytes());
        tx.execute("INSERT INTO runtime_requests(id,record) VALUES('written-request',?1)", [request.to_string()])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('written-approval-speech','agent','','orchestrator','approval','','',?1,1)", [json!({"request_id":"written-request","fingerprint":fingerprint}).to_string()])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('written-approval-playback','agent','written-approval','playback','played','','',?1,2)", [json!({"record_id":"written-approval-speech"}).to_string()])?;
        tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('written-approval-transcript','agent','written-approval','user','разрешаю','','','{}',3)", [])?;
        tx.execute_batch("CREATE TRIGGER fail_approval_finalize BEFORE UPDATE ON voice_approvals WHEN OLD.id='written-request' BEGIN SELECT RAISE(ABORT,'crash after provider Written'); END;")?;
        tx.execute("INSERT INTO voice_sessions(id,agent,created,native_thread,account_key,connection_id,state) VALUES('written-end','agent',1,'thread','account','receipt-matrix-connection','ready')", [])?;
        Ok(())
    }).expect("seed written-effect failpoints");

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
    assert_eq!(end_written["state"], "stopping");
    assert_eq!(provider.calls.lock().expect("provider calls").len(), 4);
    drop(store);
    store = Arc::new(
        RuntimeStore::open(Some(&root.path().join("state"))).expect("restart after written end"),
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
    assert_eq!(end_replay["state"], "stopping");
    assert_eq!(provider.calls.lock().expect("provider calls").len(), 4);
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
