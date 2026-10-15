#![allow(clippy::unwrap_used)]

#[path = "multi_server/common.rs"]
mod common;
use common::*;
use serde_json::{Value, json};
use studio_runtime_services::multi_server_auth::{AccessSnapshot, OwnerObservation, authorize_at};
use studio_runtime_services::multi_server_discovery::DiscoverySnapshot;
use studio_runtime_store::RuntimeStore;

#[test]
fn peer_delivery_replays_same_identity_after_lost_receipt_and_restart_once() {
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use studio_runtime_services::multi_server_auth::{AccessError, refused};
    use studio_runtime_services::multi_server_orchestration::{
        ExchangeOutcome, OrchestrationDriver, OrchestrationEffects, PairedTransport,
    };

    struct Effects;
    impl OrchestrationEffects for Effects {
        fn supports(&self, _: &str) -> bool {
            false
        }
        fn execute(&self, _: &str, _: &Value) -> Result<ExchangeOutcome, AccessError> {
            Ok(ExchangeOutcome::Unknown)
        }
    }
    #[derive(Default)]
    struct Peer {
        seen: Mutex<HashMap<String, Value>>,
        applied: Mutex<HashSet<String>>,
        calls: Mutex<HashMap<String, usize>>,
    }
    impl PairedTransport for Peer {
        fn exchange(
            &self,
            server: &str,
            envelope: &Value,
            _: Duration,
        ) -> Result<Value, AccessError> {
            assert_eq!(server, "peer-server");
            let id = envelope["requestId"]
                .as_str()
                .expect("request id")
                .to_owned();
            let mut seen = self.seen.lock().unwrap();
            if let Some(previous) = seen.get(&id) {
                assert_eq!(
                    previous, envelope,
                    "replay keeps exact identity and content"
                );
            } else {
                seen.insert(id.clone(), envelope.clone());
            }
            self.applied.lock().unwrap().insert(id.clone());
            let mut calls = self.calls.lock().unwrap();
            let count = calls.entry(id.clone()).or_default();
            *count += 1;
            if *count == 1 {
                return Err(refused(503, "exchange_unknown", "reply was lost"));
            }
            Ok(json!({"requestId":id,"outcome":"applied","value":{"stored":true}}))
        }
    }

    let (root, store) = store();
    let peer = Arc::new(Peer::default());
    let driver = OrchestrationDriver::new(
        Arc::clone(&store),
        Arc::clone(&peer) as _,
        Arc::new(Effects),
    );
    let payload = json!({"link":"link-1","worker":"worker-1","args":{"text":"same"}});
    assert_eq!(
        driver
            .exchange("peer-server", "message", &payload, "stable-message-1", 100)
            .unwrap(),
        ExchangeOutcome::Unknown
    );
    // Simulate process loss after the request was claimed and sent but before
    // the retry schedule was persisted. Restart recovery must replay only this
    // idempotent peer action with the same identity and content.
    store
        .transaction_with(|tx| {
            tx.execute(
                "UPDATE runtime_server_outbox SET state='running',next_at=0 WHERE id='stable-message-1'",
                [],
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .expect("simulate restart between claim and retry persistence");
    drop(driver);
    drop(store);

    let restarted =
        Arc::new(RuntimeStore::open(Some(root.path())).expect("reopen persistent outbox"));
    let driver = OrchestrationDriver::new(
        Arc::clone(&restarted),
        Arc::clone(&peer) as _,
        Arc::new(Effects),
    );
    assert!(driver.pump_due(105).unwrap() > 0);
    assert!(
        peer.applied.lock().unwrap().contains("stable-message-1"),
        "receiver commits stable message identity once"
    );
    assert_eq!(
        peer.calls.lock().unwrap()["stable-message-1"],
        2,
        "lost response causes one exact replay"
    );
    let connection = restarted.read().expect("reopened store reads");
    let state: String = connection
        .query_row(
            "SELECT state FROM runtime_server_outbox WHERE id='stable-message-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "complete");
}

#[test]
fn received_remote_event_replays_cached_receipt_after_restart_without_duplicate_event() {
    use std::sync::Arc;
    use std::time::Duration;
    use studio_runtime_services::multi_server_auth::{AccessError, refused};
    use studio_runtime_services::multi_server_orchestration::{
        ExchangeOutcome, OrchestrationDriver, PairedTransport, RuntimeOrchestrationEffects,
    };
    use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, AgentStatus};

    struct NoTransport;
    impl PairedTransport for NoTransport {
        fn exchange(&self, _: &str, _: &Value, _: Duration) -> Result<Value, AccessError> {
            Err(refused(503, "unused", "transport unused in receive test"))
        }
    }
    let (root, store) = store();
    let mut parent = AgentRecord {
        id: AgentId::from("test-parent"),
        root_id: AgentId::from("test-parent"),
        epoch: 3,
        status: AgentStatus::Idle,
        extra: Default::default(),
    };
    parent.extra.insert("autoWake".into(), json!(true));
    parent.extra.insert(
        "cwd".into(),
        json!(std::env::current_dir().unwrap().display().to_string()),
    );
    parent.extra.insert("concurrency".into(), json!(32));
    parent.extra.insert("moveImportPending".into(), json!(true));
    let link = json!({"id":"test-link","side":"home","server":"peer-server","home":"local-server","parent":"test-parent","parentEpoch":3,"workers":[]});
    store
        .transaction_with(|tx| {
            tx.execute(
                "INSERT INTO runtime_server_links(id,record) VALUES('test-link',?1)",
                [link.to_string()],
            )?;
            AgentRepository::put(tx, &parent)?;
            Ok::<_, AccessError>(())
        })
        .expect("link and owner agent fixture");
    let envelope = json!({"requestId":"stable-event-test","action":"event","payload":{"link":"test-link","parentEpoch":3,"kind":"agent_message","text":"owner monitor exit"}});
    let make_driver = |store: Arc<RuntimeStore>| {
        OrchestrationDriver::new(
            store.clone(),
            Arc::new(NoTransport),
            Arc::new(RuntimeOrchestrationEffects::new(store)),
        )
    };
    let driver = make_driver(Arc::clone(&store));
    let lost_reply = driver
        .receive("peer-server", &envelope, 100)
        .expect("event applied");
    assert!(matches!(lost_reply, ExchangeOutcome::Applied(_)));
    drop(driver);
    drop(store);

    let restarted = Arc::new(RuntimeStore::open(Some(root.path())).expect("restart state"));
    let driver = make_driver(Arc::clone(&restarted));
    let receipt = driver
        .receive("peer-server", &envelope, 101)
        .expect("cached receipt replays");
    assert_eq!(receipt, lost_reply);
    let connection = restarted.read().expect("event rows");
    let count: i64 = connection
        .query_row(
            "SELECT count(*) FROM runtime_events WHERE id='stable-event-test'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    let text: String = connection
        .query_row(
            "SELECT text FROM runtime_events WHERE id='stable-event-test'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(text, "owner monitor exit");
}

#[test]
fn moved_worker_send_reaches_a_local_descendant_once() {
    use std::sync::Arc;
    use std::time::Duration;
    use studio_runtime_services::multi_server_auth::{AccessError, refused};
    use studio_runtime_services::multi_server_orchestration::{
        ExchangeOutcome, OrchestrationDriver, PairedTransport, RuntimeOrchestrationEffects,
    };
    use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, AgentStatus};

    struct NoTransport;
    impl PairedTransport for NoTransport {
        fn exchange(&self, _: &str, _: &Value, _: Duration) -> Result<Value, AccessError> {
            Err(refused(503, "unused", "transport unused in receive test"))
        }
    }

    let (_root, store) = store();
    let mut parent = AgentRecord {
        id: AgentId::from("move-parent"),
        root_id: AgentId::from("move-parent"),
        epoch: 3,
        status: AgentStatus::Idle,
        extra: Default::default(),
    };
    parent.extra.insert("autoWake".into(), json!(true));
    parent.extra.insert(
        "cwd".into(),
        json!(std::env::current_dir().unwrap().display().to_string()),
    );
    parent.extra.insert("concurrency".into(), json!(32));
    let mut worker = AgentRecord {
        id: AgentId::from("remote-worker"),
        root_id: AgentId::from("move-parent"),
        epoch: 4,
        status: AgentStatus::Idle,
        extra: Default::default(),
    };
    worker.extra.insert("autoWake".into(), json!(true));
    worker.extra.insert(
        "cwd".into(),
        json!(std::env::current_dir().unwrap().display().to_string()),
    );
    worker.extra.insert("movedTo".into(), json!("server-two"));
    worker.extra.insert(
        "remoteWorker".into(),
        json!({"server":"peer-server","link":"move-link"}),
    );
    let mut child = AgentRecord {
        id: AgentId::from("local-child"),
        root_id: AgentId::from("move-parent"),
        epoch: 1,
        status: AgentStatus::Idle,
        extra: Default::default(),
    };
    child.extra.insert("autoWake".into(), json!(true));
    child.extra.insert(
        "cwd".into(),
        json!(std::env::current_dir().unwrap().display().to_string()),
    );
    child
        .extra
        .insert("parentId".into(), json!("remote-worker"));
    let link = json!({"id":"move-link","side":"home","server":"peer-server","parent":"move-parent","parentEpoch":3,"workers":["remote-worker"]});
    store
        .transaction_with(|tx| {
            tx.execute(
                "INSERT INTO runtime_server_links(id,record) VALUES('move-link',?1)",
                [link.to_string()],
            )?;
            AgentRepository::put(tx, &parent)?;
            AgentRepository::put(tx, &worker)?;
            AgentRepository::put(tx, &child)?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .expect("moved worker and descendant fixture");

    let envelope = json!({
        "requestId":"stable-move-send",
        "action":"move_send",
        "payload":{
            "link":"move-link",
            "worker":"remote-worker",
            "epoch":4,
            "parentEpoch":3,
            "args":{"agent_id":"local-child","text":"continue this task","delivery":"queue"}
        }
    });
    let driver = OrchestrationDriver::new(
        Arc::clone(&store),
        Arc::new(NoTransport),
        Arc::new(RuntimeOrchestrationEffects::new(Arc::clone(&store))),
    );
    let receipt = driver
        .receive("peer-server", &envelope, 100)
        .expect("move-send is applied through the incoming caller");
    assert!(matches!(receipt, ExchangeOutcome::Applied(_)));
    let replay = driver
        .receive("peer-server", &envelope, 101)
        .expect("stable move-send replays its saved receipt");
    assert_eq!(receipt, replay);
    let connection = store.read().expect("event rows");
    let count: i64 = connection
        .query_row(
            "SELECT count(*) FROM runtime_events WHERE id='stable-move-send' AND agent='local-child' AND kind='followup' AND text='continue this task'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn remote_worker_call_refuses_stopped_parent_without_writes() {
    use std::sync::Arc;
    use studio_runtime_services::multi_server_orchestration::{
        OrchestrationEffects, RuntimeOrchestrationEffects,
    };
    use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, AgentStatus};

    let (_root, store) = store();
    let mut parent = AgentRecord {
        id: AgentId::from("stopped-parent"),
        root_id: AgentId::from("stopped-parent"),
        epoch: 4,
        status: AgentStatus::Idle,
        extra: Default::default(),
    };
    parent.extra.insert("autoWake".into(), json!(false));
    let mut worker = AgentRecord {
        id: AgentId::from("stopped-worker"),
        root_id: AgentId::from("stopped-parent"),
        epoch: 2,
        status: AgentStatus::Idle,
        extra: Default::default(),
    };
    worker.extra.insert("autoWake".into(), json!(true));
    worker.extra.insert("movedTo".into(), json!("server-two"));
    worker.extra.insert(
        "remoteWorker".into(),
        json!({"server":"peer-server","link":"stopped-link"}),
    );
    store.transaction(|tx| {
        tx.execute("INSERT INTO runtime_server_links(id,record) VALUES('stopped-link',?1)", [json!({"side":"home","server":"peer-server","parent":"stopped-parent","parentEpoch":3,"workers":["stopped-worker"]}).to_string()])?;
        AgentRepository::put(tx, &parent)?;
        AgentRepository::put(tx, &worker)?;
        Ok::<_, studio_runtime_store::StoreError>(())
    }).expect("stopped parent fixture");
    let effects = RuntimeOrchestrationEffects::new(Arc::clone(&store));
    let request = json!({"requestId":"stopped-call","action":"message","payload":{"link":"stopped-link","worker":"stopped-worker","epoch":2,"parentEpoch":3,"args":{}}});
    let before = rows(&store);
    assert_eq!(
        effects
            .execute("peer-server", &request)
            .expect_err("parent stopped")
            .status(),
        409
    );
    assert_eq!(
        rows(&store),
        before,
        "stale or stopped parent causes no writes"
    );
}

#[test]
fn child_result_requires_link_membership_and_updates_proxy() {
    use std::sync::Arc;
    use studio_runtime_services::multi_server_orchestration::{
        OrchestrationEffects, RuntimeOrchestrationEffects,
    };
    use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, AgentStatus};

    let (_root, store) = store();
    let mut parent = AgentRecord {
        id: AgentId::from("result-parent"),
        root_id: AgentId::from("result-parent"),
        epoch: 3,
        status: AgentStatus::Idle,
        extra: Default::default(),
    };
    parent.extra.insert("autoWake".into(), json!(true));
    let mut child = AgentRecord {
        id: AgentId::from("result-child"),
        root_id: AgentId::from("result-parent"),
        epoch: 2,
        status: AgentStatus::Idle,
        extra: Default::default(),
    };
    child.extra.insert("lastAnswer".into(), json!("old"));
    store.transaction(|tx| {
        tx.execute("INSERT INTO runtime_server_links(id,record) VALUES('result-link',?1)", [json!({"side":"home","server":"peer-server","parent":"result-parent","parentEpoch":3,"workers":["result-child"]}).to_string()])?;
        AgentRepository::put(tx, &parent)?;
        AgentRepository::put(tx, &child)?;
        Ok::<_, studio_runtime_store::StoreError>(())
    }).expect("result link and proxy");
    let effects = RuntimeOrchestrationEffects::new(Arc::clone(&store));
    let request = |id: &str, result: &str| json!({"requestId":id,"action":"event","payload":{"link":"result-link","parentEpoch":3,"kind":"child_result","text":json!({"agent_id":"result-child","result":result}).to_string()}});
    effects
        .execute("peer-server", &request("child-result-1", "done"))
        .expect("linked child result");
    let connection = store.read().expect("stored result");
    let proxy = AgentRepository::get(&connection, &AgentId::from("result-child"))
        .unwrap()
        .unwrap();
    assert_eq!(proxy.extra.get("lastAnswer"), Some(&json!("done")));
    assert_eq!(
        proxy.extra.get("lastCompletedTurn"),
        Some(&json!("child-result-1"))
    );
    drop(connection);
    store.transaction(|tx| {
        tx.execute("UPDATE runtime_server_links SET record=?1 WHERE id='result-link'", [json!({"side":"home","server":"peer-server","parent":"result-parent","parentEpoch":3,"workers":[]}).to_string()])?;
        Ok::<_, studio_runtime_store::StoreError>(())
    }).expect("unlink child");
    let before = rows(&store);
    assert_eq!(
        effects
            .execute("peer-server", &request("child-result-2", "bad"))
            .expect_err("unlinked child")
            .status(),
        403
    );
    assert_eq!(
        rows(&store),
        before,
        "unlinked result refuses without writes"
    );
}

#[test]
fn browser_callback_matches_python_and_replays_without_second_loopback_relay() {
    use sha2::Digest;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use studio_runtime_services::multi_server_auth::{AccessError, refused};
    use studio_runtime_services::multi_server_orchestration::{
        ExchangeOutcome, OrchestrationDriver, PairedTransport, RuntimeOrchestrationEffects,
    };
    const CALLBACK_FIXTURE_TIMEOUT: Duration = Duration::from_secs(2);
    struct NoTransport;
    impl PairedTransport for NoTransport {
        fn exchange(&self, _: &str, _: &Value, _: Duration) -> Result<Value, AccessError> {
            Err(refused(503, "unused", "transport unused in receive test"))
        }
    }
    let expected = &fixture().data["browserCallback"];
    let bridge_id = "0123456789abcdef0123456789abcdef";
    let token = "generated-browser-forward-token";
    let request_id = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("studio-browser-open:{bridge_id}").as_bytes(),
    )
    .to_string();
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback callback fixture");
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).expect("nonblocking fixture");
    let (start_relay_tx, start_relay_rx) = std::sync::mpsc::channel();
    let relay = std::thread::spawn(move || {
        start_relay_rx
            .recv()
            .expect("start callback relay after store setup");
        let deadline = Instant::now() + CALLBACK_FIXTURE_TIMEOUT;
        let mut calls = 0;
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    calls += 1;
                    stream
                        .set_read_timeout(Some(CALLBACK_FIXTURE_TIMEOUT))
                        .expect("bounded callback read");
                    stream
                        .set_write_timeout(Some(CALLBACK_FIXTURE_TIMEOUT))
                        .expect("bounded callback write");
                    let mut request = [0; 1024];
                    let count = stream.read(&mut request).expect("callback request");
                    assert!(
                        String::from_utf8_lossy(&request[..count])
                            .starts_with("GET /callback?code=ok HTTP/1.1")
                    );
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 9\r\nConnection: close\r\n\r\nsigned in")
                        .expect("callback response");
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("bounded loopback accept failed: {error}"),
            }
        }
        calls
    });
    let (_root, store) = store();
    let mut record = json!({"id":request_id,"method":"browser/open","status":"answered","params":{"bridgeId":bridge_id,"forward":{"tokenHash":""},"redirect":{"host":"127.0.0.1","port":port}}});
    let digest = sha2::Sha256::digest(token.as_bytes());
    let digest = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    record["params"]["forward"]["tokenHash"] = json!(digest);
    store
        .transaction(|tx| {
        tx.execute(
            "INSERT INTO runtime_requests(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
                rusqlite::params![request_id, record.to_string()],
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .expect("saved browser callback request");
    let driver = OrchestrationDriver::new(
        Arc::clone(&store),
        Arc::new(NoTransport),
        Arc::new(RuntimeOrchestrationEffects::new(Arc::clone(&store))),
    );
    let envelope = json!({"requestId":"browser-callback:stable-1","action":"browser_callback","payload":{"bridgeId":bridge_id,"token":token,"target":"/callback?code=ok","headers":{"accept":"text/plain"}}});
    // Start the fixture's short network deadline only after RuntimeStore setup;
    // cold store initialization is unrelated to callback delivery.
    start_relay_tx
        .send(())
        .expect("start callback relay after store setup");
    let first = driver
        .receive("peer-server", &envelope, 100)
        .expect("valid callback relayed");
    let expected_result = expected["valid"].clone();
    assert_eq!(
        first.to_value("browser-callback:stable-1")["value"],
        expected_result
    );
    let replay = driver
        .receive("peer-server", &envelope, 101)
        .expect("cached callback receipt");
    assert_eq!(first, replay);
    for (label, payload, status) in [
        (
            "wrongToken",
            json!({"bridgeId":bridge_id,"token":"wrong","target":"/callback","headers":{}}),
            403,
        ),
        (
            "unknownBridge",
            json!({"bridgeId":"fedcba9876543210fedcba9876543210","token":token,"target":"/callback","headers":{}}),
            404,
        ),
    ] {
        let request = json!({"requestId":format!("browser-{label}"),"action":"browser_callback","payload":payload});
        let receipt = driver
            .receive("peer-server", &request, 102)
            .expect("definite callback refusal is returned as a receipt");
        let ExchangeOutcome::NotApplied(message) = &receipt else {
            panic!("{label} must remain a definite refusal: {receipt:?}");
        };
        assert!(!message.is_empty(), "{label} refusal names its cause");
        assert_eq!(
            driver
                .receive("peer-server", &request, 103)
                .expect("refusal receipt is idempotently replayed"),
            receipt
        );
        assert_eq!(
            expected[label]["refused"],
            if status == 403 {
                "PermissionError"
            } else {
                "ValueError"
            }
        );
        let connection = store.read().expect("saved request remains readable");
        let saved: String = connection
            .query_row(
                "SELECT record FROM runtime_requests WHERE id=?1",
                [&request_id],
                |row| row.get(0),
            )
            .expect("wrong and unknown bridge attempts do not modify saved request");
        assert_eq!(
            serde_json::from_str::<Value>(&saved).expect("saved callback JSON"),
            record
        );
    }
    assert_eq!(relay.join().expect("bounded local relay server"), 1);
}

#[test]
fn rust_remote_event_outbox_row_matches_python_queue_writer() {
    let expected = &fixture().data["eventQueued"];
    let request_id = expected["id"].as_str().expect("Python request identity");
    let server = expected["server"].as_str().expect("Python destination");
    let envelope: Value = serde_json::from_str(expected["body"].as_str().unwrap()).unwrap();
    let payload = envelope["payload"].clone();
    let now = fixture().data["now"].as_i64().unwrap();
    let (_root, store) = store();
    store
        .transaction_with(|tx| {
            tx.execute(
                "DELETE FROM runtime_server_outbox WHERE id=?1",
                [request_id],
            )?;
            studio_runtime_services::multi_server_orchestration::enqueue_in_transaction(
                tx, server, "event", &payload, request_id, now,
            )
            .expect("valid Python event envelope");
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .expect("Rust owner transaction queues event");
    let connection = store.read().unwrap();
    let raw: String = connection.query_row(
        "SELECT json_object('id',id,'server',server,'body',body,'state',state,'attempts',attempts,'next_at',next_at,'created_at',created_at,'fingerprint',fingerprint) FROM runtime_server_outbox WHERE id=?1",
        [request_id], |row| row.get(0),
    ).unwrap();
    let actual: Value = serde_json::from_str(&raw).unwrap();
    for key in [
        "id",
        "server",
        "state",
        "attempts",
        "next_at",
        "created_at",
        "fingerprint",
    ] {
        assert_eq!(actual[key], expected[key], "Python parity field {key}");
    }
    assert_eq!(
        serde_json::from_str::<Value>(actual["body"].as_str().unwrap()).unwrap(),
        envelope
    );
}

fn full_matrix() -> bool {
    std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some()
}

fn python_authentication_outputs_match_complete_principal_and_writes() {
    let data = &fixture().data;
    for case in data["auth"].as_array().expect("cases") {
        let result = (|| {
            let request = request(case)?;
            let snapshot = AccessSnapshot::from_records(
                case["clientId"].as_str().expect("id"),
                case["client"].as_str(),
                case["inviteId"].as_str().expect("invite"),
                case["invite"].as_str(),
                true,
            )?;
            let logins: Vec<String> =
                serde_json::from_value(case["logins"].clone()).expect("logins");
            let owner = OwnerObservation::from_json(
                case["address"].as_str().expect("address"),
                &logins,
                &data["status"],
                &case["whois"],
            )?;
            authorize_at(
                &snapshot,
                &request,
                "local-server",
                &owner,
                case.get("probe"),
                case["now"].as_f64().expect("now"),
            )
        })();
        if let Some(expected) = case.get("refusal") {
            assert_eq!(
                refusal(result.err().expect("refused")),
                *expected,
                "{}",
                case["name"]
            );
            assert_eq!(case["noWrite"], true);
            let (_root, store) = store();
            store
                .transaction(|tx| {
                    tx.execute("DELETE FROM runtime_access_clients", [])?;
                    if let Some(client) = case["client"].as_str() {
                        tx.execute(
                            "INSERT INTO runtime_access_clients(id,record) VALUES(?1,?2)",
                            rusqlite::params![case["clientId"].as_str().expect("client"), client],
                        )?;
                    }
                    tx.execute(
                        "UPDATE runtime_access_invites SET record=?1 WHERE id=?2",
                        rusqlite::params![
                            case["invite"].as_str().expect("invite"),
                            case["inviteId"].as_str().expect("id")
                        ],
                    )?;
                    Ok(())
                })
                .expect("install exact Python precondition");
            let before = rows(&store);
            let lookup = CountingOwner::new(case.clone());
            let boundary = studio_runtime_services::multi_server_pairing::MultiServerBoundary::new(
                data["identity"].clone(),
                lookup,
            )
            .expect("boundary");
            let incoming = (|| {
                let request = request(case)?;
                let logins: Vec<String> =
                    serde_json::from_value(case["logins"].clone()).expect("logins");
                boundary.handle_incoming(
                    &store,
                    &request,
                    case["address"].as_str().expect("address"),
                    &logins,
                    case["now"].as_f64().expect("now"),
                )
            })();
            assert_eq!(
                refusal(incoming.expect_err("refused at boundary")),
                *expected,
                "{}",
                case["name"]
            );
            assert_eq!(rows(&store), before, "{}: refusal writes", case["name"]);
        } else {
            let actual = result.unwrap_or_else(|e| panic!("{}: {e}", case["name"]));
            assert_eq!(
                json!({"principal":actual.principal.value(),"clientWrite":actual.client_write}),
                case["expected"],
                "{}",
                case["name"]
            );
        }
    }
}
fn malformed_stored_rows_are_located_without_panics() {
    let data = &fixture().data;
    for field in ["id", "clientId", "kind", "publicKey", "status"] {
        for value in [Value::Null, json!(9), json!([]), json!({})] {
            let mut client: Value =
                serde_json::from_str(data["client"].as_str().expect("client")).expect("JSON");
            client[field] = value;
            let error = AccessSnapshot::from_records(
                "peer-server",
                Some(&client.to_string()),
                "",
                None,
                true,
            )
            .err()
            .expect("invalid row refused");
            assert!(
                error
                    .to_string()
                    .contains("runtime_access_clients/peer-server"),
                "{error}"
            );
        }
    }
}
fn owner_observations_reject_tags_wrong_ip_and_spoofed_login() {
    let data = &fixture().data;
    assert!(
        OwnerObservation::from_json("127.0.0.1", &[], &data["status"], &data["whois"]).is_err()
    );
    assert!(
        OwnerObservation::from_json(
            "100.64.0.2",
            &["foreign".into()],
            &data["status"],
            &data["whois"]
        )
        .is_err()
    );
    let mut whois = data["whois"].clone();
    whois["Node"]["Tags"] = json!(["tag:server"]);
    assert!(OwnerObservation::from_json("100.64.0.2", &[], &data["status"], &whois).is_err());
    assert!(
        OwnerObservation::from_json(
            "100.64.0.2",
            &["owner@example.test".into(), "owner@example.test".into()],
            &data["status"],
            &data["whois"]
        )
        .is_err()
    );
}
fn discovery_includes_local_other_port_and_only_online_owner_nodes() {
    let data = &fixture().data;
    let snapshot = DiscoverySnapshot::from_status(
        &data["status"],
        "local-server",
        "https://local.fixture.ts.net",
    )
    .expect("status");
    assert_eq!(serde_json::json!(snapshot.candidates()), data["candidates"]);
    let decision = studio_runtime_services::multi_server_discovery::decide_probe(
        &snapshot,
        "https://peer.fixture.ts.net",
        &data["discoveryProbe"],
        None,
        None,
        true,
        data["now"].as_i64().expect("now"),
    )
    .expect("probe");
    assert_eq!(decision.record(), Some(&data["discovered"]));
    assert!(!decision.auto_pair());
    assert_eq!(
        snapshot.candidates(),
        vec![
            "https://local.fixture.ts.net:8443",
            "https://peer.fixture.ts.net",
            "https://peer.fixture.ts.net:8443"
        ]
    );
}
fn pairing_is_atomic_at_every_write_and_replays_after_restart() {
    let data = &fixture().data;
    let case = &data["auth"][0];
    let now = data["now"].as_i64().expect("now");
    for (table, operation) in [
        ("runtime_access_nonces", "INSERT"),
        ("runtime_access_invites", "UPDATE"),
        ("runtime_access_clients", "INSERT"),
        ("runtime_access_receipts", "INSERT"),
        ("runtime_access_audit", "INSERT"),
    ]
    .into_iter()
    .take(if full_matrix() { 5 } else { 1 })
    {
        let (root, store) = store();
        let before = rows(&store);
        store.transaction(|tx|{tx.execute_batch(&format!("CREATE TRIGGER failpoint BEFORE {operation} ON {table} BEGIN SELECT RAISE(ABORT,'fixture failpoint'); END"))?;Ok(())}).expect("failpoint");
        assert!(
            boundary()
                .handle(
                    &store,
                    &request(case).expect("request"),
                    &owner(),
                    None,
                    now
                )
                .is_err(),
            "{table}"
        );
        assert_eq!(rows(&store), before, "rollback at {table}");
        store
            .transaction(|tx| {
                tx.execute_batch("DROP TRIGGER failpoint")?;
                Ok(())
            })
            .expect("remove failpoint");
        drop(store);
        let reopened =
            studio_runtime_store::RuntimeStore::open(Some(root.path())).expect("restart");
        let result = boundary()
            .handle(
                &reopened,
                &request(case).expect("request"),
                &owner(),
                None,
                now,
            )
            .expect("first apply after rollback");
        assert_eq!(result["paired"], true);
        let status: String = reopened
            .read()
            .expect("read")
            .query_row(
                "SELECT json_extract(record,'$.status') FROM runtime_access_invites WHERE id=?1",
                [data["invitation"]["inviteId"].as_str().expect("invite ID")],
                |r| r.get(0),
            )
            .expect("invite state");
        assert_eq!(status, "used");
        let fresh = request(&data["freshPair"]).expect("fresh nonce");
        assert_eq!(
            boundary()
                .handle(&reopened, &fresh, &owner(), None, now)
                .expect("recover pairing result"),
            result
        );
        let audit:i64=reopened.read().expect("read").query_row("SELECT count(*) FROM runtime_access_audit WHERE client='new-client' AND action='pair'",[],|r|r.get(0)).expect("audit");
        assert_eq!(audit, 1, "pairing applied once");
        assert!(
            boundary()
                .handle(
                    &reopened,
                    &request(case).expect("request"),
                    &owner(),
                    None,
                    now
                )
                .is_err(),
            "nonce replay"
        );
    }
}

#[test]
fn multi_server_corpus_witness() {
    let (_root, store) = store();
    let snapshot =
        studio_runtime_services::multi_server::snapshot(&store, &fixture().data["identity"])
            .expect("access snapshot");
    assert_eq!(
        snapshot["protocol"],
        serde_json::json!(
            studio_runtime_services::multi_server_protocol_generated::ACCESS_PROTOCOL_VERSION
        )
    );

    let report = studio_runtime_store::check_state_readonly(fixture().root.path())
        .expect("state-check accepts the real Python writers");
    let names: std::collections::BTreeSet<_> = report.tables.iter().map(|t| t.table).collect();
    assert_eq!(names.len(), report.tables.len(), "one report per table");
    for table in [
        "runtime_access_clients",
        "runtime_access_discovered",
        "runtime_server_outbox",
    ] {
        let checked = report
            .tables
            .iter()
            .find(|t| t.table == table)
            .expect("registered table");
        assert!(
            checked.row_count > 0,
            "Python fixture must populate {table}"
        );
        assert_eq!(checked.rejected_count, 0, "{table}: {:?}", checked.failures);
    }
    python_authentication_outputs_match_complete_principal_and_writes();
    malformed_stored_rows_are_located_without_panics();
    owner_observations_reject_tags_wrong_ip_and_spoofed_login();
    discovery_includes_local_other_port_and_only_online_owner_nodes();
    pairing_is_atomic_at_every_write_and_replays_after_restart();
    orchestration_claims_survive_lost_receipts_and_reject_changed_content();
    python_receipt_states_preserve_completed_results_and_uncertainty();
    nonce_capacity_is_rate_limited_without_committing_access_writes();
    signatures_precede_owner_lookups_and_probes();
    queried_status_crosses_service_dispatch();
    fractional_invitation_boundary_matches_python();
}

fn orchestration_claims_survive_lost_receipts_and_reject_changed_content() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use studio_runtime_services::multi_server_auth::AccessError;
    use studio_runtime_services::multi_server_orchestration::{
        ExchangeOutcome, OrchestrationDriver, OrchestrationEffects, PairedTransport,
    };
    struct Effects(Arc<AtomicUsize>);
    impl OrchestrationEffects for Effects {
        fn supports(&self, action: &str) -> bool {
            action == "fixture" || action == "input"
        }
        fn execute(&self, _: &str, _: &Value) -> Result<ExchangeOutcome, AccessError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ExchangeOutcome::Applied(json!({"applied":true})))
        }
    }
    struct Transport(Arc<AtomicUsize>, bool);
    impl PairedTransport for Transport {
        fn exchange(
            &self,
            _: &str,
            envelope: &Value,
            _: std::time::Duration,
        ) -> Result<Value, AccessError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(if self.1 {
                json!({"requestId":envelope["requestId"],"outcome":"unknown"})
            } else {
                json!({"requestId":envelope["requestId"],"outcome":"applied","value":{"applied":true}})
            })
        }
    }
    let now = fixture().data["now"].as_i64().expect("now");
    for direction in if full_matrix() {
        &["inbox", "outbox"][..]
    } else {
        &["outbox"][..]
    } {
        for stage in ["claim", "finish"] {
            let (root, store) = store();
            let calls = Arc::new(AtomicUsize::new(0));
            let event = if stage == "claim" { "INSERT" } else { "UPDATE" };
            store.transaction(|tx|{tx.execute_batch(&format!("CREATE TRIGGER failpoint BEFORE {event} ON runtime_server_{direction} BEGIN SELECT RAISE(ABORT,'fixture failpoint'); END"))?;Ok(())}).expect("failpoint");
            let run = |store: Arc<studio_runtime_store::RuntimeStore>| {
                let driver = OrchestrationDriver::new(
                    store,
                    Arc::new(Transport(calls.clone(), false)),
                    Arc::new(Effects(calls.clone())),
                );
                if *direction == "inbox" {
                    driver.receive("peer-server",&json!({"requestId":"effect-a","action":"fixture","payload":{"actor":"fixture"}}),now)
                } else {
                    driver.exchange(
                        "peer-server",
                        "fixture",
                        &json!({"actor":"fixture"}),
                        "effect-a",
                        now,
                    )
                }
            };
            assert!(run(store.clone()).is_err(), "{direction}/{stage}");
            assert_eq!(calls.load(Ordering::SeqCst), usize::from(stage == "finish"));
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER failpoint")?;
                    Ok(())
                })
                .expect("remove failpoint");
            drop(store);
            let reopened = Arc::new(
                studio_runtime_store::RuntimeStore::open(Some(root.path())).expect("restart"),
            );
            let result = run(reopened.clone()).expect("recovery");
            assert_eq!(
                result,
                if stage == "claim" {
                    ExchangeOutcome::Applied(json!({"applied":true}))
                } else {
                    ExchangeOutcome::Unknown
                }
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "never repeat effect at {direction}/{stage}"
            );
            let driver = OrchestrationDriver::new(
                reopened,
                Arc::new(Transport(calls.clone(), false)),
                Arc::new(Effects(calls.clone())),
            );
            let changed = if *direction == "inbox" {
                driver.receive("peer-server",&json!({"requestId":"effect-a","action":"fixture","payload":{"actor":"different"}}),now)
            } else {
                driver.exchange(
                    "peer-server",
                    "fixture",
                    &json!({"actor":"different"}),
                    "effect-a",
                    now,
                )
            };
            assert_eq!(changed.expect_err("conflict").status(), 409);
        }
    }
    for case in fixture().data["deliveryStates"].as_array().expect("states") {
        let (root, store) = store();
        // Template rows come from Python receive(), including waiting's proof
        // that no effect crossed the boundary. Reopen before attempting it.
        drop(store);
        let store =
            Arc::new(studio_runtime_store::RuntimeStore::open(Some(root.path())).expect("restart"));
        let calls = Arc::new(AtomicUsize::new(0));
        let driver = OrchestrationDriver::new(
            store.clone(),
            Arc::new(Transport(calls.clone(), false)),
            Arc::new(Effects(calls.clone())),
        );
        let result = driver
            .receive("peer-server", &case["envelope"], now)
            .expect("Python receipt state");
        let actual = match result {
            ExchangeOutcome::Applied(value) => {
                json!({"requestId":case["envelope"]["requestId"],"outcome":"applied","value":value})
            }
            ExchangeOutcome::Unknown => {
                json!({"requestId":case["envelope"]["requestId"],"outcome":"unknown"})
            }
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(actual, case["result"], "{}", case["state"]);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            case["calls"].as_u64().expect("calls") as usize
        );
        driver
            .receive("peer-server", &case["envelope"], now)
            .expect("replay");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            case["calls"].as_u64().expect("calls") as usize
        );
    }
    for stage in if full_matrix() {
        &["claim", "finish"][..]
    } else {
        &["claim"][..]
    } {
        let (root, store) = store();
        let case = fixture().data["deliveryStates"]
            .as_array()
            .expect("states")
            .iter()
            .find(|c| c["state"] == "waiting")
            .expect("waiting");
        let state = if *stage == "claim" {
            "running"
        } else {
            "complete"
        };
        store.transaction(|tx| { tx.execute_batch(&format!("CREATE TRIGGER waiting_fail BEFORE UPDATE ON runtime_server_inbox WHEN NEW.state='{state}' BEGIN SELECT RAISE(ABORT,'waiting failpoint'); END"))?; Ok(()) }).expect("failpoint");
        let calls = Arc::new(AtomicUsize::new(0));
        let run = |store: Arc<studio_runtime_store::RuntimeStore>| {
            OrchestrationDriver::new(
                store,
                Arc::new(Transport(calls.clone(), false)),
                Arc::new(Effects(calls.clone())),
            )
            .receive("peer-server", &case["envelope"], now)
        };
        assert!(run(store.clone()).is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            usize::from(*stage == "finish")
        );
        store
            .transaction(|tx| {
                tx.execute_batch("DROP TRIGGER waiting_fail")?;
                Ok(())
            })
            .expect("remove failpoint");
        drop(store);
        let store =
            Arc::new(studio_runtime_store::RuntimeStore::open(Some(root.path())).expect("restart"));
        assert_eq!(
            run(store).expect("recovery"),
            if *stage == "claim" {
                ExchangeOutcome::Applied(json!({"applied":true}))
            } else {
                ExchangeOutcome::Unknown
            }
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "waiting claim never executes twice"
        );
    }
    let (_root, store) = store();
    let calls = Arc::new(AtomicUsize::new(0));
    let driver = OrchestrationDriver::new(
        store.clone(),
        Arc::new(Transport(calls.clone(), true)),
        Arc::new(Effects(calls.clone())),
    );
    assert_eq!(
        driver
            .exchange("peer-server", "fixture", &json!({}), "unknown", now)
            .expect("unknown"),
        ExchangeOutcome::Unknown
    );
    assert_eq!(
        driver
            .exchange("peer-server", "fixture", &json!({}), "unknown", now + 100)
            .expect("unknown retained"),
        ExchangeOutcome::Unknown
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let before = rows(&store);
    assert_eq!(
        driver
            .receive(
                "peer-server",
                &json!({"requestId":"missing","action":"absent","payload":{}}),
                now
            )
            .expect_err("named refusal")
            .status(),
        501
    );
    assert_eq!(rows(&store), before);
    let queued = &fixture().data["queued"];
    let envelope: Value =
        serde_json::from_str(queued["body"].as_str().expect("body")).expect("envelope");
    assert_eq!(
        driver
            .exchange(
                "peer-server",
                "browser_callback",
                &envelope["payload"],
                "python-queued",
                now
            )
            .expect("Python-written queue"),
        ExchangeOutcome::Unknown
    );
}

fn python_receipt_states_preserve_completed_results_and_uncertainty() {
    let data = &fixture().data;
    let case = &data["auth"][0];
    let now = data["now"].as_i64().expect("now");
    for state in if full_matrix() {
        &["pending", "complete", "tombstone", "expired", "malformed"][..]
    } else {
        &["pending", "expired"][..]
    } {
        let state = *state;
        let (_root, store) = store();
        let result = boundary()
            .handle(
                &store,
                &request(case).expect("request"),
                &owner(),
                None,
                now,
            )
            .expect("pair");
        store.transaction(|tx| {
            if state == "expired" {
                tx.execute("UPDATE runtime_access_receipts SET record=json_set(record,'$.completed',?1) WHERE client='new-client'", [now-8*86400])?;
            } else if state == "malformed" {
                tx.execute("UPDATE runtime_access_receipts SET record=json_set(record,'$.body',9) WHERE client='new-client'", [])?;
            } else {
                tx.execute("UPDATE runtime_access_receipts SET status=?1 WHERE client='new-client'", [state])?;
            }
            Ok(())
        }).expect("saved Python-compatible state");
        let before = rows(&store);
        let replay = boundary().handle(
            &store,
            &request(&data["freshPair"]).expect("request"),
            &owner(),
            None,
            now,
        );
        if matches!(state, "pending" | "complete") {
            assert_eq!(replay.expect("recovered receipt"), result);
            let saved: String = store
                .read()
                .expect("read")
                .query_row(
                    "SELECT status FROM runtime_access_receipts WHERE client='new-client'",
                    [],
                    |r| r.get(0),
                )
                .expect("saved");
            assert_eq!(saved, "complete");
        } else {
            assert_eq!(
                replay.expect_err("refused").status(),
                if state == "malformed" { 503 } else { 410 }
            );
            assert_eq!(rows(&store), before, "refusal is atomic");
        }
    }
}

fn nonce_capacity_is_rate_limited_without_committing_access_writes() {
    let data = &fixture().data;
    let now = data["now"].as_i64().expect("now");
    for (case_name, client, count) in [
        ("status-server", "peer-server", 10_000),
        ("pair-ui", "unpaired-fixture", 1_280),
    ] {
        let (_root, store) = store();
        store.transaction(|tx| {
            tx.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<?1) INSERT INTO runtime_access_nonces(client,nonce,created) SELECT ?2,printf('%032d',x),?3 FROM n",rusqlite::params![count,client,now])?;
            Ok(())
        }).expect("nonce capacity fixture");
        let before = rows(&store);
        let case = data["auth"]
            .as_array()
            .expect("cases")
            .iter()
            .find(|v| v["name"] == case_name)
            .expect("case");
        let error = boundary()
            .handle(
                &store,
                &request(case).expect("request"),
                &owner(),
                None,
                now,
            )
            .expect_err("capacity refusal");
        assert_eq!(error.status(), 429);
        assert_eq!(rows(&store), before, "rate refusal must not commit writes");
    }
}

fn fractional_invitation_boundary_matches_python() {
    let data = &fixture().data;
    for case in data["auth"].as_array().expect("cases").iter().filter(|c| {
        c["name"]
            .as_str()
            .expect("name")
            .starts_with("invite-expiry-")
    }) {
        let (_root, store) = store();
        store
            .transaction(|tx| {
                tx.execute(
                    "UPDATE runtime_access_invites SET record=?1 WHERE id=?2",
                    rusqlite::params![
                        case["invite"].as_str().expect("invite"),
                        case["inviteId"].as_str().expect("id")
                    ],
                )?;
                Ok(())
            })
            .expect("Python fractional expiry");
        let before = rows(&store);
        let result = boundary().handle_at(
            &store,
            &request(case).expect("request"),
            &owner(),
            None,
            case["now"].as_f64().expect("time"),
        );
        if let Some(expected) = case.get("refusal") {
            assert_eq!(refusal(result.expect_err("expired")), *expected);
            assert_eq!(rows(&store), before);
        } else {
            assert_eq!(result.expect("not expired")["paired"], true);
        }
    }
}

fn queried_status_crosses_service_dispatch() {
    let data = &fixture().data;
    let case = data["auth"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["name"] == "status-query")
        .expect("queried status");
    let (_root, store) = store();
    store
        .transaction(|tx| {
            tx.execute(
                "UPDATE runtime_access_clients SET record=?1 WHERE id='peer-server'",
                [case["client"].as_str().expect("Python precondition")],
            )?;
            Ok(())
        })
        .expect("install exact Python authentication precondition");
    let lookup = CountingOwner::new(case.clone());
    let boundary = studio_runtime_services::multi_server_pairing::MultiServerBoundary::new(
        data["identity"].clone(),
        lookup,
    )
    .expect("boundary");
    let result = boundary
        .handle_incoming(
            &store,
            &request(case).expect("signed target"),
            "100.64.0.2",
            &[],
            case["now"].as_f64().expect("clock"),
        )
        .expect("dispatch by path after checking full signed target");
    assert_eq!(result, data["identity"]);
    let stored: String = store
        .read()
        .expect("read")
        .query_row(
            "SELECT record FROM runtime_access_clients WHERE id='peer-server'",
            [],
            |r| r.get(0),
        )
        .expect("client");
    assert_eq!(
        serde_json::from_str::<Value>(&stored).expect("client JSON"),
        case["expected"]["clientWrite"]
    );
}

fn signatures_precede_owner_lookups_and_probes() {
    use std::sync::atomic::Ordering;
    use studio_runtime_services::multi_server_pairing::MultiServerBoundary;
    let data = &fixture().data;
    for (bad, good) in [
        ("bad-signature", "pair-ui"),
        ("bad-auto-signature", "auto-pair"),
    ] {
        let (_root, store) = store();
        let case = data["auth"]
            .as_array()
            .expect("cases")
            .iter()
            .find(|c| c["name"] == bad)
            .expect("bad signature");
        let lookup = CountingOwner::new(case.clone());
        let boundary =
            MultiServerBoundary::new(data["identity"].clone(), lookup.clone()).expect("boundary");
        let before = rows(&store);
        let error = boundary
            .handle_incoming(
                &store,
                &request(case).expect("request"),
                "100.64.0.2",
                &[],
                case["now"].as_f64().expect("now"),
            )
            .expect_err("signature rejected");
        assert_eq!(refusal(error), case["refusal"]);
        assert_eq!(
            lookup.calls.load(Ordering::SeqCst),
            0,
            "invalid signature must not invoke owner lookup"
        );
        assert_eq!(
            lookup.probes.load(Ordering::SeqCst),
            0,
            "invalid signature must not probe caller origin"
        );
        assert_eq!(rows(&store), before);
        let good = data["auth"]
            .as_array()
            .expect("cases")
            .iter()
            .find(|c| c["name"] == good)
            .expect("good signature");
        boundary
            .handle_incoming(
                &store,
                &request(good).expect("request"),
                "100.64.0.2",
                &[],
                good["now"].as_f64().expect("now"),
            )
            .expect("valid signature uses host");
        assert_eq!(lookup.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            lookup.probes.load(Ordering::SeqCst),
            usize::from(bad == "bad-auto-signature")
        );
    }
}
fn refusal(error: studio_runtime_services::multi_server_auth::AccessError) -> Value {
    match error {
        studio_runtime_services::multi_server_auth::AccessError::Refused {
            status,
            code,
            message,
        } => serde_json::json!({"status":status,"code":code,"error":message}),
        other => panic!("expected stable refusal, got {other}"),
    }
}
