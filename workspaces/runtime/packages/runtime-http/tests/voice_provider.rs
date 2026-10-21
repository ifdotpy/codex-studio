use axum::Extension;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use sha2::Digest;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use studio_runtime_http::{HttpState, router};
use studio_runtime_services::voice::runtime::{
    Method, VoiceConnectionScope, VoiceFuture, VoiceReply, VoiceRequestPort, VoiceRequestReceipt,
    VoiceWriteCertainty,
};
use studio_runtime_store::RuntimeStore;
use tower::ServiceExt;

#[path = "../../runtime-services/tests/multi_server/common.rs"]
#[allow(dead_code)]
mod common;
#[path = "../../runtime-services/tests/wave7_p15_support/fixtures.rs"]
#[allow(dead_code, unused_imports)]
mod monitor_fixtures;
#[path = "../../runtime-services/tests/wave7_p15_support/mod.rs"]
#[allow(dead_code)]
mod monitor_support;
#[path = "../../runtime-services/tests/support/prepared_python.rs"]
#[allow(dead_code)]
pub(crate) mod prepared_python_support;

#[derive(Default)]
struct RecordingPort {
    calls: Mutex<Vec<(String, Method, Value)>>,
    replies: Mutex<Vec<(String, VoiceReply)>>,
}

impl VoiceRequestPort for RecordingPort {
    fn prepare<'a>(
        &'a self,
        agent_id: &'a str,
        _session_id: &'a str,
        account_key: &'a str,
        thread_id: &'a str,
        epoch: i64,
    ) -> VoiceFuture<'a, Result<VoiceConnectionScope, VoiceWriteCertainty>> {
        Box::pin(async move {
            Ok(VoiceConnectionScope {
                agent_id: agent_id.to_owned(),
                epoch,
                account_key: account_key.to_owned(),
                thread_id: thread_id.to_owned(),
                connection_id: "voice-http-connection".into(),
                generation: 1,
            })
        })
    }

    fn request<'a>(
        &'a self,
        scope: VoiceConnectionScope,
        operation_id: String,
        method: Method,
        params: Value,
        deadline: Instant,
    ) -> VoiceFuture<'a, VoiceRequestReceipt> {
        Box::pin(async move {
            assert!(matches!(
                scope.agent_id.as_str(),
                "voice-http-agent" | "voice-start-agent"
            ));
            assert!(deadline > Instant::now());
            self.calls
                .lock()
                .expect("record requests")
                .push((operation_id, method, params));
            VoiceRequestReceipt {
                certainty: VoiceWriteCertainty::Written,
            }
        })
    }

    fn reply<'a>(
        &'a self,
        scope: VoiceConnectionScope,
        operation_id: String,
        reply: VoiceReply,
        deadline: Instant,
    ) -> VoiceFuture<'a, VoiceRequestReceipt> {
        Box::pin(async move {
            assert_eq!(scope.agent_id, "voice-http-agent");
            assert!(deadline > Instant::now());
            self.replies
                .lock()
                .expect("record replies")
                .push((operation_id, reply));
            VoiceRequestReceipt {
                certainty: VoiceWriteCertainty::Written,
            }
        })
    }
}

async fn fixture() -> (tempfile::TempDir, Arc<RuntimeStore>, Arc<RecordingPort>) {
    let (root, store) = common::store().await;
    let actor = json!({
        "id":"voice-http-agent", "rootId":"voice-http-agent", "epoch":7,
        "status":"idle", "isLead":true, "autoWake":true,
        "threadId":"voice-http-thread", "accountKey":"voice-http-account"
    });
    store.transaction(move |tx| {
        tx.execute(
            "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
            rusqlite::params!["voice-http-agent", actor.to_string()],
        )?;
        let start_actor = json!({
            "id":"voice-start-agent", "rootId":"voice-start-agent", "epoch":7,
            "status":"idle", "isLead":true, "autoWake":true,
            "threadId":"voice-http-thread", "accountKey":"voice-http-account"
        });
        tx.execute(
            "INSERT INTO runtime_agents(id,record) VALUES('voice-start-agent',?1)",
            [start_actor.to_string()],
        )?;
        for (id, state, native) in [
            ("end-http", "ready", Some("voice-http-thread")),
            ("speech-http", "ready", Some("voice-http-thread")),
            ("approve-http", "ready", None),
        ] {
            tx.execute(
                "INSERT INTO voice_sessions(id,agent,created,native_thread,account_key,connection_id,state) \
                 VALUES(?1,'voice-http-agent',1,?2,'voice-http-account','voice-http-connection',?3)",
                rusqlite::params![id, native, state],
            )?;
        }
        tx.execute(
            "INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) \
             VALUES('speech-record','voice-http-agent','', 'orchestrator','hello','item','', '{}',1)",
            [],
        )?;
        let request = json!({
            "id":"request-1", "agent":"voice-http-agent", "epoch":7,
            "status":"pending", "method":"item/commandExecution/requestApproval",
            "params":{"command":"echo safe","threadId":"voice-http-thread"},
            "rpcId":"provider-rpc-http", "accountKey":"voice-http-account",
            "connectionId":"voice-http-connection"
        });
        let fingerprint = sha2::Sha256::digest(studio_runtime_store::python_json(&request).as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        tx.execute(
            "INSERT INTO runtime_requests(id,record) VALUES('request-1',?1)",
            [request.to_string()],
        )?;
        tx.execute(
            "INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) \
             VALUES('approval-speech','voice-http-agent','','orchestrator',?1,'','',?2,1)",
            rusqlite::params!["Запрос разрешения. Для подтверждения скажите «разрешаю».", json!({"request_id":"request-1","fingerprint":fingerprint}).to_string()],
        )?;
        tx.execute(
            "INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) \
             VALUES('approval-playback','voice-http-agent','approve-http','playback','played','','',?1,2)",
            [json!({"record_id":"approval-speech"}).to_string()],
        )?;
        tx.execute(
            "INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) \
             VALUES('approval-transcript','voice-http-agent','approve-http','user','разрешаю','','','{}',1)",
            [],
        )?;
        Ok(())
    })
    .await
    .expect("seed voice HTTP rows");
    (root, store, Arc::new(RecordingPort::default()))
}

async fn post(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let body = serde_json::to_vec(&body).expect("request JSON");
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "127.0.0.1:4620")
        .header("x-canvas-token", "token")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("HTTP request");
    request.extensions_mut().insert(ConnectInfo(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        32100,
    )));
    let response = app.clone().oneshot(request).await.expect("HTTP response");
    let status = response.status();
    let value = serde_json::from_slice(
        &to_bytes(response.into_body(), 1_000_000)
            .await
            .expect("body"),
    )
    .expect("JSON response");
    (status, value)
}

#[tokio::test]
async fn installed_provider_lifts_start_speech_approve_and_end_over_http() {
    let (_root, store, port) = fixture().await;
    let state = HttpState::new(
        "token".into(),
        "workspace".into(),
        _root.path().to_string_lossy().into_owned(),
        4620,
        None,
        Some(store.clone()),
        None,
    )
    .with_voice_request_port(port.clone());
    let app = router(state).expect("router");

    let (status, started) = post(
        &app,
        "/api/voice/start",
        json!({
            "agent":"voice-start-agent", "session_id":"start-http", "sdp":"v=0\r\n"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");

    let (status, spoken) = post(
        &app,
        "/api/voice/speech",
        json!({
            "agent":"voice-http-agent", "session_id":"speech-http", "record_id":"speech-record"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{spoken}");
    assert_eq!(
        spoken["error"],
        Value::Null,
        "first response hides internal digest"
    );
    let calls_after_first_speech = port.calls.lock().expect("provider calls").len();
    let (status, spoken_replay) = post(
        &app,
        "/api/voice/speech",
        json!({
            "agent":"voice-http-agent", "session_id":"speech-http", "record_id":"speech-record"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{spoken_replay}");
    assert_eq!(
        spoken_replay["error"],
        Value::Null,
        "replay hides internal digest"
    );
    assert_eq!(spoken_replay["id"], spoken["id"]);
    assert_eq!(spoken_replay["state"], spoken["state"]);
    assert_eq!(
        port.calls.lock().expect("provider calls").len(),
        calls_after_first_speech
    );

    let (status, approved) = post(
        &app,
        "/api/voice/approve",
        json!({
            "agent":"voice-http-agent", "session_id":"approve-http",
            "speech_id":"approval-speech", "transcript_id":"approval-transcript"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{approved}");

    let (status, ended) = post(
        &app,
        "/api/voice/end",
        json!({
            "agent":"voice-http-agent", "session_id":"end-http"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ended}");

    let calls = port.calls.lock().expect("provider calls");
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].1, Method::ThreadRealtimeStart);
    assert_eq!(calls[1].1, Method::ThreadRealtimeAppendSpeech);
    assert_eq!(calls[2].1, Method::ThreadRealtimeStop);
    assert!(calls.iter().all(|(identity, _, _)| !identity.is_empty()));
    drop(calls);
    let replies = port.replies.lock().expect("provider replies");
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].0, "voice:approve:request-1");
    assert_eq!(
        replies[0].1.rpc_id,
        studio_runtime_services::voice::runtime::VoiceRpcId::String("provider-rpc-http".into())
    );
    assert_eq!(replies[0].1.result, json!({"decision":"accept"}));
}

#[tokio::test]
async fn voice_monitor_approval_uses_monitor_transaction_and_dispatch_wake() {
    use studio_runtime_store::{RequestRecord, Table};

    let root = monitor_fixtures::tempdir();
    let mut case = monitor_fixtures::corpus(root.path())
        .into_iter()
        .find(|case| case["name"] == "finish-0-None-False")
        .expect("running monitor case");
    case["before"]["status"] = json!("approval");
    case["actor"]["isLead"] = json!(true);
    case["before"]
        .as_object_mut()
        .expect("monitor record")
        .remove("operation");
    let store = Arc::new(monitor_fixtures::store(root.path(), "voice-monitor").await);
    studio_runtime_services::monitor_runtime::recover_on_startup(&store, 999.0)
        .await
        .expect("server startup recovery");
    monitor_fixtures::seed(&store, &case).await;
    let agent = case["actor"]["id"].as_str().expect("agent id").to_owned();
    let monitor_id = case["before"]["id"]
        .as_str()
        .expect("monitor id")
        .to_owned();
    let request_id = format!("monitor-approval:{monitor_id}");
    let seed_request_id = request_id.clone();
    let seed_agent = agent.clone();
    let seed_monitor = case["before"].clone();
    store
        .transaction(move |tx| {
            let request = json!({
                "id":seed_request_id,
                "method":"monitor/approve",
                "agent":seed_agent,
                "epoch":seed_monitor["epoch"],
                "params":{"monitorId":monitor_id,"command":seed_monitor["command"],"cwd":seed_monitor["cwd"]},
                "status":"pending",
                "createdAt":1000.0
            });
            let fingerprint = studio_runtime_store::sha256_hex(
                studio_runtime_store::python_json(&request).as_bytes(),
            );
            tx.execute(
                "INSERT INTO runtime_requests(id,record) VALUES(?1,?2)",
                rusqlite::params![request["id"].as_str().expect("request ID"), request.to_string()],
            )?;
            tx.execute(
                "INSERT INTO voice_sessions(id,agent,created,state) VALUES('voice-monitor-session',?1,1,'ready')",
                [request["agent"].as_str().expect("agent ID")],
            )?;
            tx.execute(
                "INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('voice-monitor-speech',?1,'','orchestrator','approval','','',?2,1)",
                rusqlite::params![request["agent"].as_str().expect("agent ID"), json!({"request_id":request["id"],"fingerprint":fingerprint}).to_string()],
            )?;
            tx.execute(
                "INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('voice-monitor-playback',?1,'voice-monitor-session','playback','played','','',?2,2)",
                rusqlite::params![request["agent"].as_str().expect("agent ID"), json!({"record_id":"voice-monitor-speech"}).to_string()],
            )?;
            tx.execute(
                "INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('voice-monitor-transcript',?1,'voice-monitor-session','user','разрешаю','','','{}',3)",
                [request["agent"].as_str().expect("agent ID")],
            )?;
            Ok(())
        })
        .await
        .expect("seed voice monitor approval");

    let approval_body = json!({
        "agent":agent,"session_id":"voice-monitor-session",
        "speech_id":"voice-monitor-speech","transcript_id":"voice-monitor-transcript"
    });
    let app_without_runtime = router(HttpState::new(
        "token".into(),
        "workspace".into(),
        root.path().to_string_lossy().into_owned(),
        4620,
        None,
        Some(store.clone()),
        None,
    ))
    .expect("router without monitor runtime");
    let before_refusal = database_snapshot(&store);
    let (status, refused) = post(
        &app_without_runtime,
        "/api/voice/approve",
        approval_body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{refused}");
    assert_eq!(database_snapshot(&store), before_refusal);

    let state = HttpState::new(
        "token".into(),
        "workspace".into(),
        root.path().to_string_lossy().into_owned(),
        4620,
        None,
        Some(store.clone()),
        None,
    );
    let port = Arc::new(monitor_support::Port::default());
    let runtime = Arc::new(
        studio_runtime_services::monitor_runtime_wiring::MonitorRuntime::new(
            store.clone(),
            port.clone(),
            state.event_wake_handle(),
        ),
    );
    let app = router(state)
        .expect("router")
        .layer(Extension(runtime.clone()));
    let (status, response) = post(
        &app,
        "/api/voice/approve",
        json!({
            "agent":agent,"session_id":"voice-monitor-session",
            "speech_id":"voice-monitor-speech","transcript_id":"voice-monitor-transcript"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["status"], "answered");
    let request = Table::<RequestRecord>::new(&store.test_reader().expect("read request"))
        .get(&request_id)
        .expect("get monitor approval")
        .expect("monitor approval");
    assert_eq!(request.value()["answerDecision"], "accept");

    // A saved accepted voice receipt must replay without the optional runtime.
    let (status, replayed) = post(
        &app_without_runtime,
        "/api/voice/approve",
        approval_body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replayed}");
    assert_eq!(replayed["status"], response["status"]);
    assert_ne!(replayed["replayed"], true);

    // Simulate a crash after the monitor transaction committed but before the
    // accepted response was copied into the voice receipt.
    let request_id_for_update = request_id.clone();
    store
        .transaction(move |tx| {
            tx.execute(
                "UPDATE voice_approvals SET status='uncertain',result=NULL WHERE id=?1",
                [&request_id_for_update],
            )?;
            Ok(())
        })
        .await
        .expect("simulate crash window");
    let (status, recovered) = post(&app_without_runtime, "/api/voice/approve", approval_body).await;
    assert_eq!(status, StatusCode::OK, "{recovered}");
    assert_eq!(recovered["status"], "answered");
    assert_eq!(recovered["replayed"], true);
    let receipt: (String, Option<String>) = store
        .test_reader()
        .expect("read recovered receipt")
        .query_row(
            "SELECT status,result FROM voice_approvals WHERE id=?1",
            [&request_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read recovered voice approval receipt");
    assert_eq!(receipt.0, "accepted");
    let saved: Value = serde_json::from_str(receipt.1.as_deref().expect("saved voice receipt"))
        .expect("valid saved voice receipt");
    assert_eq!(saved, json!({"status":"answered","replayed":true}));

    runtime
        .tick(1001.0)
        .await
        .expect("dispatch accepted monitor start");
    assert_eq!(port.starts.lock().expect("monitor starts").len(), 1);
}

#[tokio::test]
async fn providerless_routes_admit_start_and_refuse_other_actions_without_writes() {
    let (_root, store, _port) = fixture().await;
    let state = HttpState::new(
        "token".into(),
        "workspace".into(),
        _root.path().to_string_lossy().into_owned(),
        4620,
        None,
        Some(store.clone()),
        None,
    );
    let app = router(state).expect("router");
    let before = voice_row_counts(&store);
    for (path, body) in [
        (
            "/api/voice/speech",
            json!({"agent":"voice-http-agent","session_id":"speech-http","record_id":"speech-record"}),
        ),
        (
            "/api/voice/approve",
            json!({"agent":"voice-http-agent","session_id":"approve-http","speech_id":"approval-speech","transcript_id":"approval-transcript"}),
        ),
        (
            "/api/voice/end",
            json!({"agent":"voice-http-agent","session_id":"end-http"}),
        ),
    ] {
        let (status, value) = post(&app, path, body).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{path}: {value}");
        assert!(
            value.to_string().contains("provider-session/voice"),
            "{path}: {value}"
        );
    }
    let (status, accepted) = post(
        &app,
        "/api/voice/start",
        json!({"agent":"voice-start-agent","session_id":"no-port-start","sdp":"v=0\r\n"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(accepted["state"], "connecting");
    studio_runtime_services::voice::provider::wait_for_start_setup(
        "voice-start-agent",
        "no-port-start",
    )
    .await;
    let (saved_state, count): (String, i64) = store
        .test_reader()
        .expect("read accepted session")
        .query_row(
            "SELECT state,(SELECT COUNT(*) FROM voice_sessions WHERE id='no-port-start' AND agent='voice-start-agent') FROM voice_sessions WHERE id='no-port-start' AND agent='voice-start-agent'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read saved session and count");
    assert_eq!(saved_state, "failed");
    assert_eq!(count, 1);
    let (status, replay) = post(
        &app,
        "/api/voice/start",
        json!({"agent":"voice-start-agent","session_id":"no-port-start","sdp":"v=0\r\n"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["state"], "failed");
    assert_eq!(voice_row_counts(&store)[0], before[0] + 1);
    assert_eq!(voice_row_counts(&store)[1..], before[1..]);
}

fn voice_row_counts(store: &RuntimeStore) -> Vec<i64> {
    let connection = store.test_reader().expect("read voice row counts");
    ["voice_sessions", "voice_speech_requests", "voice_approvals"]
        .iter()
        .map(|table| {
            connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .expect("count rows")
        })
        .collect()
}

fn database_snapshot(store: &RuntimeStore) -> Vec<(String, Vec<Vec<rusqlite::types::Value>>)> {
    let connection = store.test_reader().expect("read database snapshot");
    let tables = {
        let mut statement = connection
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .expect("prepare table list");
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query table list")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect table list")
    };
    tables
        .into_iter()
        .map(|table| {
            let quoted_table = table.replace('"', "\"\"");
            let mut statement = connection
                .prepare(&format!("SELECT * FROM \"{quoted_table}\""))
                .expect("prepare snapshot table");
            let column_count = statement.column_count();
            let mut rows = statement.query([]).expect("query snapshot table");
            let mut values = Vec::new();
            while let Some(row) = rows.next().expect("read snapshot row") {
                values.push(
                    (0..column_count)
                        .map(|column| row.get(column).expect("read snapshot value"))
                        .collect::<Vec<rusqlite::types::Value>>(),
                );
            }
            values.sort_by_key(|row| format!("{row:?}"));
            (table, values)
        })
        .collect()
}
