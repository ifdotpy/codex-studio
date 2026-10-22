#![allow(clippy::unwrap_used)]

use crate::multi_server_fixtures as common;
use crate::prepared_python_support;
use common::*;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use studio_runtime_services::multi_server_auth::{AccessSnapshot, OwnerObservation, authorize_at};
use studio_runtime_services::multi_server_discovery::DiscoverySnapshot;
use studio_runtime_store::RuntimeStore;

#[tokio::test]
async fn tailscale_discovery_uses_only_online_untagged_same_owner_candidates() {
    let status = json!({
        "BackendState": "Running",
        "Self": {"UserID": 7, "DNSName": "local.example.ts.net."},
        "User": {"7": {"LoginName": "Owner@Example.test"}},
        "Peer": {
            "same-owner": {"UserID": 7, "Online": true, "DNSName": "peer.example.ts.net."},
            "offline": {"UserID": 7, "Online": false, "DNSName": "offline.example.ts.net."},
            "other-owner": {"UserID": 8, "Online": true, "DNSName": "other.example.ts.net."},
            "tagged": {"UserID": 7, "Online": true, "Tags": ["tag:server"], "DNSName": "tagged.example.ts.net."}
        }
    });
    let snapshot =
        DiscoverySnapshot::from_status(&status, "local-server", "https://local.example.ts.net")
            .expect("valid fake Tailscale status");
    let candidates = snapshot.candidates();
    assert_eq!(snapshot.owner(), "owner@example.test");
    assert!(candidates.contains(&"https://peer.example.ts.net"));
    assert!(candidates.contains(&"https://peer.example.ts.net:8443"));
    assert!(!candidates.iter().any(|origin| origin.contains("offline")));
    assert!(!candidates.iter().any(|origin| origin.contains("other.")));
    assert!(!candidates.iter().any(|origin| origin.contains("tagged")));
    assert!(!candidates.contains(&"https://local.example.ts.net"));
    assert!(candidates.contains(&"https://local.example.ts.net:8443"));
}

#[tokio::test]
async fn peer_delivery_replays_same_identity_after_lost_receipt_and_restart_once() {
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
        fn execute<'a>(
            &'a self,
            _: &'a str,
            _: &'a Value,
        ) -> futures_util::future::BoxFuture<'a, Result<ExchangeOutcome, AccessError>> {
            Box::pin(async { Ok(ExchangeOutcome::Unknown) })
        }
    }
    #[derive(Default)]
    struct Peer {
        seen: Mutex<HashMap<String, Value>>,
        applied: Mutex<HashSet<String>>,
        calls: Mutex<HashMap<String, usize>>,
    }
    impl PairedTransport for Peer {
        fn exchange<'a>(
            &'a self,
            server: &'a str,
            envelope: &'a Value,
            _timeout: Duration,
        ) -> futures_util::future::BoxFuture<'a, Result<Value, AccessError>> {
            Box::pin(async move {
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
            })
        }
    }

    let (root, store) = store().await;
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
            .await
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
            .await
        .expect("simulate restart between claim and retry persistence");
    drop(driver);
    drop(store);

    let restarted = Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("reopen persistent outbox"),
    );
    let driver = OrchestrationDriver::new(
        Arc::clone(&restarted),
        Arc::clone(&peer) as _,
        Arc::new(Effects),
    );
    assert!(driver.pump_due(105).await.unwrap() > 0);
    assert!(
        peer.applied.lock().unwrap().contains("stable-message-1"),
        "receiver commits stable message identity once"
    );
    assert_eq!(
        peer.calls.lock().unwrap()["stable-message-1"],
        2,
        "lost response causes one exact replay"
    );
    let connection = restarted.test_reader().expect("reopened store reads");
    let state: String = connection
        .query_row(
            "SELECT state FROM runtime_server_outbox WHERE id='stable-message-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "complete");
}

#[tokio::test]
async fn received_remote_event_replays_cached_receipt_after_restart_without_duplicate_event() {
    use std::sync::Arc;
    use std::time::Duration;
    use studio_runtime_services::multi_server_auth::{AccessError, refused};
    use studio_runtime_services::multi_server_orchestration::{
        ExchangeOutcome, OrchestrationDriver, PairedTransport, RuntimeOrchestrationEffects,
    };
    use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, AgentStatus};

    struct NoTransport;
    impl PairedTransport for NoTransport {
        fn exchange<'a>(
            &'a self,
            _server: &'a str,
            _envelope: &'a Value,
            _timeout: Duration,
        ) -> futures_util::future::BoxFuture<'a, Result<Value, AccessError>> {
            Box::pin(async move { Err(refused(503, "unused", "transport unused in receive test")) })
        }
    }
    let (root, store) = store().await;
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
        .transaction_with(move |tx| {
            tx.execute(
                "INSERT INTO runtime_server_links(id,record) VALUES('test-link',?1)",
                [link.to_string()],
            )?;
            AgentRepository::put(tx, &parent)?;
            Ok::<_, AccessError>(())
        })
        .await
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
        .await
        .expect("event applied");
    assert!(matches!(lost_reply, ExchangeOutcome::Applied(_)));
    drop(driver);
    drop(store);

    let restarted = Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("restart state"),
    );
    let driver = make_driver(Arc::clone(&restarted));
    let receipt = driver
        .receive("peer-server", &envelope, 101)
        .await
        .expect("cached receipt replays");
    assert_eq!(receipt, lost_reply);
    let connection = restarted.test_reader().expect("event rows");
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

#[tokio::test]
#[allow(clippy::disallowed_methods)]
async fn paired_project_location_read_uses_the_fake_peer_transport() {
    use std::sync::Arc;
    use std::time::Duration;
    use studio_runtime_services::multi_server_auth::{AccessError, refused};
    use studio_runtime_services::multi_server_orchestration::{
        ExchangeOutcome, OrchestrationDriver, PairedTransport, RuntimeOrchestrationEffects,
    };

    struct NoTransport;
    impl PairedTransport for NoTransport {
        fn exchange<'a>(
            &'a self,
            _server: &'a str,
            _envelope: &'a Value,
            _timeout: Duration,
        ) -> futures_util::future::BoxFuture<'a, Result<Value, AccessError>> {
            Box::pin(async move { Err(refused(503, "unused", "peer transport not expected")) })
        }
    }
    struct FakePeer(Arc<OrchestrationDriver>);
    impl PairedTransport for FakePeer {
        fn exchange<'a>(
            &'a self,
            server: &'a str,
            envelope: &'a Value,
            _timeout: Duration,
        ) -> futures_util::future::BoxFuture<'a, Result<Value, AccessError>> {
            Box::pin(async move {
                assert_eq!(server, "peer-server");
                let key = envelope["requestId"]
                    .as_str()
                    .expect("stable request identity");
                Ok(self
                    .0
                    .receive("local-server", envelope, 100)
                    .await?
                    .to_value(key))
            })
        }
    }

    let (_peer_root, peer_store) = store().await;
    peer_store
        .transaction(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO runtime_access_clients(id,record) VALUES('local-server',?1)",
                [json!({"id":"local-server","clientId":"local-client","kind":"server","label":"Fake local peer","created":1,"publicKey":"fixture-key","status":"paired"}).to_string()],
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
            .await
        .expect("pair reciprocal fake peer");
    let peer = Arc::new(OrchestrationDriver::new(
        Arc::clone(&peer_store),
        Arc::new(NoTransport),
        Arc::new(RuntimeOrchestrationEffects::new(Arc::clone(&peer_store))),
    ));
    let (local_root, local_store) = store().await;
    local_store.transaction(|tx| {
        tx.execute(
            "INSERT OR REPLACE INTO runtime_access_clients(id,record) VALUES('peer-server',?1)",
            [json!({"id":"peer-server","clientId":"peer-client","kind":"server","label":"Fake peer","created":1,"publicKey":"fixture-key","status":"paired"}).to_string()],
        )?;
        Ok::<_, studio_runtime_store::StoreError>(())
    })
            .await.expect("pair fake peer");
    let local = OrchestrationDriver::new(
        Arc::clone(&local_store),
        Arc::new(FakePeer(peer)),
        Arc::new(RuntimeOrchestrationEffects::new(local_store)),
    );
    let folder = local_root.path().join("remote-project");
    std::fs::create_dir_all(&folder).expect("fake remote project directory");
    let initialized = std::process::Command::new("git")
        .args(["init", "-q"])
        .arg(&folder)
        .status()
        .expect("local Git fixture initializes");
    assert!(initialized.success(), "local Git fixture initializes");
    std::fs::write(
        folder.join(".git/config"),
        "[remote \"origin\"]\n\turl = https://example.invalid/team/repo.git\n",
    )
    .expect("fake Git origin");
    let outcome = local
        .exchange(
            "peer-server",
            "location_info",
            &json!({"cwd":folder.to_string_lossy()}),
            "project-location-read-1",
            100,
        )
        .await
        .expect("fake remote location read");
    let ExchangeOutcome::Applied(value) = outcome else {
        panic!("expected applied location read")
    };
    assert_eq!(value["gitOrigin"], "example.invalid/team/repo");
    assert_eq!(value["matches"], json!([]));
    // A browsed folder is reported in its resolved form.
    let browse = studio_runtime_store::path_identity::resolve(local_root.path())
        .expect("resolved fixture root")
        .join("remote-browse");
    std::fs::create_dir_all(browse.join("visible-child")).expect("fake remote child");
    std::fs::create_dir_all(browse.join(".hidden-child")).expect("fake hidden child");
    let folders = local
        .exchange(
            "peer-server",
            "folders",
            &json!({"cwd":browse.to_string_lossy()}),
            "folder-browse-read-1",
            101,
        )
        .await
        .expect("fake remote folder browse");
    let ExchangeOutcome::Applied(folders) = folders else {
        panic!("expected applied folder browse")
    };
    assert_eq!(folders["path"], browse.to_string_lossy().as_ref());
    assert_eq!(folders["directories"].as_array().unwrap().len(), 1);
    assert_eq!(folders["directories"][0]["name"], "visible-child");
}

#[tokio::test]
async fn moved_worker_send_reaches_a_local_descendant_once() {
    use std::sync::Arc;
    use std::time::Duration;
    use studio_runtime_services::multi_server_auth::{AccessError, refused};
    use studio_runtime_services::multi_server_orchestration::{
        ExchangeOutcome, OrchestrationDriver, PairedTransport, RuntimeOrchestrationEffects,
    };
    use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, AgentStatus};

    struct NoTransport;
    impl PairedTransport for NoTransport {
        fn exchange<'a>(
            &'a self,
            _server: &'a str,
            _envelope: &'a Value,
            _timeout: Duration,
        ) -> futures_util::future::BoxFuture<'a, Result<Value, AccessError>> {
            Box::pin(async move { Err(refused(503, "unused", "transport unused in receive test")) })
        }
    }

    let (_root, store) = store().await;
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
        .transaction_with(move |tx| {
            tx.execute(
                "INSERT INTO runtime_server_links(id,record) VALUES('move-link',?1)",
                [link.to_string()],
            )?;
            AgentRepository::put(tx, &parent)?;
            AgentRepository::put(tx, &worker)?;
            AgentRepository::put(tx, &child)?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
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
        .await
        .expect("move-send is applied through the incoming caller");
    assert!(matches!(receipt, ExchangeOutcome::Applied(_)));
    let replay = driver
        .receive("peer-server", &envelope, 101)
        .await
        .expect("stable move-send replays its saved receipt");
    assert_eq!(receipt, replay);
    let connection = store.test_reader().expect("event rows");
    let count: i64 = connection
        .query_row(
            "SELECT count(*) FROM runtime_events WHERE id='stable-move-send' AND agent='local-child' AND kind='followup' AND text='continue this task'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn remote_worker_call_refuses_stopped_parent_without_writes() {
    use std::sync::Arc;
    use studio_runtime_services::multi_server_orchestration::{
        OrchestrationEffects, RuntimeOrchestrationEffects,
    };
    use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, AgentStatus};

    let (_root, store) = store().await;
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
    store.transaction(move |tx| {
        tx.execute("INSERT INTO runtime_server_links(id,record) VALUES('stopped-link',?1)", [json!({"side":"home","server":"peer-server","parent":"stopped-parent","parentEpoch":3,"workers":["stopped-worker"]}).to_string()])?;
        AgentRepository::put(tx, &parent)?;
        AgentRepository::put(tx, &worker)?;
        Ok::<_, studio_runtime_store::StoreError>(())
    })
            .await.expect("stopped parent fixture");
    let effects = RuntimeOrchestrationEffects::new(Arc::clone(&store));
    let request = json!({"requestId":"stopped-call","action":"message","payload":{"link":"stopped-link","worker":"stopped-worker","epoch":2,"parentEpoch":3,"args":{}}});
    let before = rows(&store);
    assert_eq!(
        effects
            .execute("peer-server", &request)
            .await
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

#[tokio::test]
async fn remote_stop_receiver_replays_exact_request_and_rejects_superseded_control_epoch() {
    use std::sync::Arc;
    use studio_runtime_services::{
        agent_lifecycle_runtime::{LifecycleEffect, LifecyclePorts},
        agent_management::ManagementError,
        multi_server_orchestration::{
            ExchangeOutcome, OrchestrationEffects, RuntimeOrchestrationEffects,
        },
    };
    use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, AgentStatus, Transaction};

    struct Lifecycle;
    impl LifecyclePorts for Lifecycle {
        fn stage(&self, _: &Transaction<'_>, _: &LifecycleEffect) -> Result<(), ManagementError> {
            Ok(())
        }
        fn committed(
            &self,
        ) -> studio_runtime_services::agent_lifecycle_runtime::LifecycleCommitFuture<'_> {
            Box::pin(async { Ok(()) })
        }
    }

    let (_root, store) = store().await;
    let mut parent = AgentRecord {
        id: AgentId::from("remote-parent"),
        root_id: AgentId::from("remote-parent"),
        epoch: 2,
        status: AgentStatus::Idle,
        extra: Default::default(),
    };
    parent.extra.insert("autoWake".into(), json!(true));
    parent.extra.insert("cwd".into(), json!("/fixture"));
    let mut worker = AgentRecord {
        id: AgentId::from("remote-worker"),
        root_id: AgentId::from("remote-parent"),
        epoch: 4,
        status: AgentStatus::Idle,
        extra: Default::default(),
    };
    worker.extra.insert("autoWake".into(), json!(true));
    worker.extra.insert("movedTo".into(), json!("server-two"));
    worker
        .extra
        .insert("parentId".into(), json!("remote-parent"));
    worker.extra.insert("cwd".into(), json!("/fixture"));
    worker.extra.insert(
        "remoteWorker".into(),
        json!({"server":"home-server","link":"remote-link"}),
    );
    store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_server_links(id,record) VALUES('remote-link',?1)",
                [
                    json!({"side":"remote","home":"home-server","workers":["remote-worker"]})
                        .to_string(),
                ],
            )?;
            AgentRepository::put(tx, &parent)?;
            AgentRepository::put(tx, &worker)?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("remote parent link and worker");
    let lifecycle: Arc<dyn LifecyclePorts> = Arc::new(Lifecycle);
    let effects = RuntimeOrchestrationEffects::with_lifecycle_ports(store.clone(), lifecycle);
    let envelope = json!({
        "requestId":"remote-stop-request-1",
        "action":"stop",
        "payload":{"link":"remote-link","worker":"remote-worker","reason":"Stopped by the parent","controlEpoch":9}
    });

    let first = effects
        .execute("home-server", &envelope)
        .await
        .expect("remote stop");
    assert_eq!(
        first,
        ExchangeOutcome::Applied(json!({"stopped":["remote-worker"]}))
    );
    let replay = effects
        .execute("home-server", &envelope)
        .await
        .expect("same request replay");
    assert_eq!(
        replay,
        ExchangeOutcome::Applied(json!({"stopped":["remote-worker"]}))
    );
    let reader = store.test_reader().expect("read replayed worker");
    let current = AgentRepository::get(&reader, &AgentId::from("remote-worker"))
        .expect("worker row")
        .expect("worker");
    assert_eq!(current.epoch, 5, "replay does not stop twice");
    assert_eq!(
        current.extra.get("remoteStopRequest"),
        Some(&json!("remote-stop-request-1"))
    );
    drop(reader);

    store
        .transaction(|tx| {
            let mut current =
                AgentRepository::get(tx, &AgentId::from("remote-worker"))?.expect("worker");
            current
                .extra
                .insert("remoteStopRequest".into(), json!("newer"));
            current.extra.insert("remoteControlEpoch".into(), json!(10));
            AgentRepository::put(tx, &current)?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("newer remote control epoch");
    let stale = json!({
        "requestId":"remote-stop-request-2",
        "action":"stop",
        "payload":{"link":"remote-link","worker":"remote-worker","reason":"old stop","controlEpoch":9}
    });
    assert!(effects.execute("home-server", &stale).await.is_err());
}

#[tokio::test]
async fn stopping_remote_proxy_persists_and_replays_the_same_signed_intent() {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;
    use studio_runtime_services::{
        account_session_runtime::AccountSessionPort,
        agent_management::Action,
        lifecycle_effects::ProductionLifecyclePorts,
        multi_server_auth::{AccessError, refused},
        multi_server_orchestration::{
            OrchestrationDriver, PairedTransport, RuntimeOrchestrationEffects,
        },
    };
    use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, AgentStatus};
    use tokio::sync::Notify;

    struct NoBroker;
    impl AccountSessionPort for NoBroker {}
    struct Peer {
        calls: Mutex<Vec<Value>>,
        attempts: AtomicUsize,
    }
    impl PairedTransport for Peer {
        fn exchange<'a>(
            &'a self,
            server: &'a str,
            envelope: &'a Value,
            _timeout: Duration,
        ) -> futures_util::future::BoxFuture<'a, Result<Value, AccessError>> {
            Box::pin(async move {
                assert_eq!(server, "peer-server");
                self.calls.lock().unwrap().push(envelope.clone());
                if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(refused(503, "exchange_unknown", "reply was lost"));
                }
                Ok(json!({
                    "requestId": envelope["requestId"],
                    "outcome": "applied",
                    "value": {"stopped": [envelope["payload"]["worker"]]},
                }))
            })
        }
    }

    let (_root, store) = store().await;
    store
        .transaction(|tx| {
            tx.execute("DELETE FROM runtime_server_outbox", [])?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("isolate the stop outbox");
    let mut proxy = AgentRecord {
        id: AgentId::from("remote-stop-proxy"),
        root_id: AgentId::from("remote-stop-proxy"),
        epoch: 6,
        status: AgentStatus::Idle,
        extra: Default::default(),
    };
    proxy.extra.insert("autoWake".into(), json!(true));
    proxy.extra.insert("cwd".into(), json!("/fixture"));
    proxy.extra.insert(
        "remoteWorker".into(),
        json!({"server":"peer-server","link":"remote-stop-link"}),
    );
    store
        .transaction(move |tx| {
            AgentRepository::put(tx, &proxy)?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("remote proxy");

    let peer = Arc::new(Peer {
        calls: Mutex::new(Vec::new()),
        attempts: AtomicUsize::new(0),
    });
    let ports = Arc::new(
        ProductionLifecyclePorts::new(
            Arc::clone(&store),
            Arc::new(Notify::new()),
            Arc::new(Notify::new()),
            Arc::new(NoBroker),
            None,
        )
        .with_remote_transport(Arc::clone(&peer) as _),
    );
    assert_eq!(
        ports
            .execute(
                &Action::Stop {
                    target: "remote-stop-proxy".to_owned(),
                    descendants: false,
                    reason: None,
                    sender: None,
                    sender_epoch: None,
                },
                100.0,
            )
            .await
            .expect("local stop commits with durable remote intent"),
        json!({"stopped":["remote-stop-proxy"]})
    );
    let first = peer.calls.lock().unwrap()[0].clone();
    assert_eq!(first["action"], "stop");
    assert_eq!(first["payload"]["worker"], "remote-stop-proxy");
    let request_id = first["requestId"]
        .as_str()
        .expect("stable request id")
        .to_owned();
    let retry_at: f64 = store
        .test_reader()
        .expect("read queued stop")
        .query_row(
            "SELECT next_at FROM runtime_server_outbox WHERE id=?1 AND state='queued'",
            [&request_id],
            |row| row.get(0),
        )
        .expect("lost receipt leaves durable queued intent");

    let driver = OrchestrationDriver::new(
        Arc::clone(&store),
        Arc::clone(&peer) as _,
        Arc::new(RuntimeOrchestrationEffects::new(Arc::clone(&store))),
    );
    assert_eq!(
        driver
            .pump_due(retry_at.ceil() as i64)
            .await
            .expect("replay due intent"),
        1
    );
    let calls = peer.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1], first, "retry reuses exact signed stop content");
}

#[tokio::test]
async fn child_result_requires_link_membership_and_updates_proxy() {
    use std::sync::Arc;
    use studio_runtime_services::multi_server_orchestration::{
        OrchestrationEffects, RuntimeOrchestrationEffects,
    };
    use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, AgentStatus};

    let (_root, store) = store().await;
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
    store.transaction(move |tx| {
        tx.execute("INSERT INTO runtime_server_links(id,record) VALUES('result-link',?1)", [json!({"side":"home","server":"peer-server","parent":"result-parent","parentEpoch":3,"workers":["result-child"]}).to_string()])?;
        AgentRepository::put(tx, &parent)?;
        AgentRepository::put(tx, &child)?;
        Ok::<_, studio_runtime_store::StoreError>(())
    })
            .await.expect("result link and proxy");
    let effects = RuntimeOrchestrationEffects::new(Arc::clone(&store));
    let request = |id: &str, result: &str| json!({"requestId":id,"action":"event","payload":{"link":"result-link","parentEpoch":3,"kind":"child_result","text":json!({"agent_id":"result-child","result":result}).to_string()}});
    effects
        .execute("peer-server", &request("child-result-1", "done"))
        .await
        .expect("linked child result");
    let connection = store.test_reader().expect("stored result");
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
    })
            .await.expect("unlink child");
    let before = rows(&store);
    assert_eq!(
        effects
            .execute("peer-server", &request("child-result-2", "bad"))
            .await
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

#[tokio::test]
async fn browser_callback_matches_python_and_replays_without_second_loopback_relay() {
    use sha2::Digest;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use studio_runtime_services::multi_server_auth::{AccessError, refused};
    use studio_runtime_services::multi_server_orchestration::{
        ExchangeOutcome, OrchestrationDriver, PairedTransport, RuntimeOrchestrationEffects,
    };
    struct NoTransport;
    impl PairedTransport for NoTransport {
        fn exchange<'a>(
            &'a self,
            _server: &'a str,
            _envelope: &'a Value,
            _timeout: Duration,
        ) -> futures_util::future::BoxFuture<'a, Result<Value, AccessError>> {
            Box::pin(async move { Err(refused(503, "unused", "transport unused in receive test")) })
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
    let (start_relay_tx, start_relay_rx) = std::sync::mpsc::channel();
    let relay = std::thread::spawn(move || {
        start_relay_rx
            .recv()
            .expect("start callback relay after store setup");
        let (mut stream, _) = listener.accept().expect("callback relay connection");
        let mut request = String::new();
        BufReader::new(stream.try_clone().expect("clone callback stream"))
            .read_line(&mut request)
            .expect("callback request line");
        assert!(request.starts_with("GET /callback?code=ok HTTP/1.1"));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 9\r\nConnection: close\r\n\r\nsigned in")
            .expect("callback response");
        1
    });
    let (_root, store) = store().await;
    let mut record = json!({"id":request_id,"method":"browser/open","status":"answered","params":{"bridgeId":bridge_id,"forward":{"tokenHash":""},"redirect":{"host":"127.0.0.1","port":port}}});
    let digest = sha2::Sha256::digest(token.as_bytes());
    let digest = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    record["params"]["forward"]["tokenHash"] = json!(digest);
    let record_for_store = record.clone();
    let request_id_for_store = request_id.clone();
    store
        .transaction(move |tx| {
        tx.execute(
            "INSERT INTO runtime_requests(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
                rusqlite::params![request_id_for_store, record_for_store.to_string()],
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
            .await
        .expect("saved browser callback request");
    let driver = OrchestrationDriver::new(
        Arc::clone(&store),
        Arc::new(NoTransport),
        Arc::new(RuntimeOrchestrationEffects::new(Arc::clone(&store))),
    );
    let envelope = json!({"requestId":"browser-callback:stable-1","action":"browser_callback","payload":{"bridgeId":bridge_id,"token":token,"target":"/callback?code=ok","headers":{"accept":"text/plain"}}});
    // Start the fixture after RuntimeStore setup so cold initialization cannot
    // race the callback accept. The relay then uses event-driven blocking I/O.
    start_relay_tx
        .send(())
        .expect("start callback relay after store setup");
    let first = driver
        .receive("peer-server", &envelope, 100)
        .await
        .expect("valid callback relayed");
    let expected_result = expected["valid"].clone();
    assert_eq!(
        first.to_value("browser-callback:stable-1")["value"],
        expected_result
    );
    let replay = driver
        .receive("peer-server", &envelope, 101)
        .await
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
            .await
            .expect("definite callback refusal is returned as a receipt");
        let ExchangeOutcome::NotApplied(message) = &receipt else {
            panic!("{label} must remain a definite refusal: {receipt:?}");
        };
        assert!(!message.is_empty(), "{label} refusal names its cause");
        assert_eq!(
            driver
                .receive("peer-server", &request, 103)
                .await
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
        let connection = store.test_reader().expect("saved request remains readable");
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

#[tokio::test]
async fn rust_remote_event_outbox_row_matches_python_queue_writer() {
    let expected = &fixture().data["eventQueued"];
    let request_id = expected["id"].as_str().expect("Python request identity");
    let server = expected["server"].as_str().expect("Python destination");
    let envelope: Value = serde_json::from_str(expected["body"].as_str().unwrap()).unwrap();
    let payload = envelope["payload"].clone();
    let now = fixture().data["now"].as_i64().unwrap();
    let (_root, store) = store().await;
    store
        .transaction_with(move |tx| {
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
        .await
        .expect("Rust owner transaction queues event");
    let connection = store.test_reader().unwrap();
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

/// Records each envelope and answers it with one fixed reply; `None` loses it.
struct ExecWire {
    reply: Option<Value>,
    sent: std::sync::Mutex<Vec<Value>>,
}
impl ExecWire {
    fn new(reply: Option<Value>) -> Arc<Self> {
        Arc::new(Self {
            reply,
            sent: std::sync::Mutex::new(Vec::new()),
        })
    }
    fn sent(&self) -> Vec<Value> {
        self.sent.lock().unwrap().clone()
    }
}
impl studio_runtime_services::multi_server_orchestration::PairedTransport for ExecWire {
    fn exchange<'a>(
        &'a self,
        server: &'a str,
        envelope: &'a Value,
        _timeout: Duration,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<Value, studio_runtime_services::multi_server_auth::AccessError>,
    > {
        Box::pin(async move {
            assert_eq!(server, "peer-server");
            self.sent.lock().unwrap().push(envelope.clone());
            self.reply.clone().ok_or_else(|| {
                studio_runtime_services::multi_server_auth::refused(
                    503,
                    "exchange_unknown",
                    "fixture lost reply",
                )
            })
        })
    }
}

fn exec_driver(
    store: &Arc<RuntimeStore>,
    wire: Arc<ExecWire>,
) -> studio_runtime_services::multi_server_orchestration::OrchestrationDriver {
    studio_runtime_services::multi_server_orchestration::OrchestrationDriver::new(
        Arc::clone(store),
        wire,
        Arc::new(
            studio_runtime_services::multi_server_orchestration::RuntimeOrchestrationEffects::new(
                Arc::clone(store),
            ),
        ),
    )
}

/// One SQL row as JSON, with stored JSON text columns parsed for comparison.
fn exec_row(store: &RuntimeStore, sql: &str, key: &str) -> Option<Value> {
    let connection = store.test_reader().unwrap();
    let raw: Option<String> =
        rusqlite::OptionalExtension::optional(connection.query_row(sql, [key], |row| row.get(0)))
            .unwrap();
    raw.map(|raw| parsed_result(serde_json::from_str(&raw).unwrap()))
}

fn parsed_result(mut row: Value) -> Value {
    if let Some(text) = row["result"].as_str() {
        row["result"] = serde_json::from_str(text).unwrap();
    }
    row
}

const EXEC_OUTBOX_ROW: &str = "SELECT json_object('server',server,'body',body,'state',state,'result',result,'attempts',attempts,'next_at',next_at,'fingerprint',fingerprint) FROM runtime_server_outbox WHERE id=?1";
const EXEC_INBOX_ROW: &str = "SELECT json_object('signature',signature,'state',state,'result',result,'principal',principal,'actor',actor) FROM runtime_server_inbox WHERE id=?1";

#[tokio::test]
async fn python_exec_receipt_row_probes_once_and_settles_from_the_reply() {
    let data = &fixture().data["execReceipt"];
    let python_now = fixture().data["now"].as_f64().unwrap();
    let pending = &data["pending"];
    let replies = data["replies"].as_object().unwrap();
    assert_eq!(replies.len(), 5, "every Python reply case is checked");
    for (name, case) in replies {
        let (_root, store) = store().await;
        // Keep only the Python-written command row in receipt form.
        store
            .transaction_with(|tx| {
                tx.execute(
                    "DELETE FROM runtime_server_outbox WHERE id!='python-exec'",
                    [],
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .await
            .unwrap();
        let before = exec_row(&store, EXEC_OUTBOX_ROW, "python-exec").unwrap();
        assert_eq!(
            before["body"], pending["body"],
            "{name}: Python receipt form"
        );
        assert_eq!(before["state"], "queued", "{name}");
        let wire = ExecWire::new(Some(case["reply"].clone()));
        let driver = exec_driver(&store, Arc::clone(&wire));
        let now = pending["next_at"].as_f64().unwrap() as i64;
        assert_eq!(driver.pump_due(now).await.unwrap(), 1, "{name}");
        assert_eq!(
            wire.sent(),
            data["probeSent"].as_array().unwrap().clone(),
            "{name}: only the Python probe identity crosses the wire, never the command"
        );
        assert_eq!(case["sent"], data["probeSent"], "{name}: Python oracle");
        let expected = parsed_result(case["row"].clone());
        let after = exec_row(&store, EXEC_OUTBOX_ROW, "python-exec").unwrap();
        for field in [
            "server",
            "body",
            "state",
            "result",
            "attempts",
            "fingerprint",
        ] {
            assert_eq!(
                after[field], expected[field],
                "{name}: Python field {field}"
            );
        }
        if expected["state"] == "complete" {
            assert_eq!(
                after["next_at"].as_f64(),
                expected["next_at"].as_f64(),
                "{name}"
            );
        } else {
            // Python delivered at its fixture time; compare the retry delay.
            assert_eq!(
                after["next_at"].as_f64().unwrap() - now as f64,
                expected["next_at"].as_f64().unwrap() - python_now,
                "{name}: Python retry delay"
            );
        }
        // A settled or rescheduled row sends nothing more at the same time.
        assert_eq!(driver.pump_due(now).await.unwrap(), 0, "{name}");
        assert_eq!(wire.sent().len(), 1, "{name}: no second probe or command");
    }
}

#[tokio::test]
async fn rust_exec_attempt_writes_the_python_receipt_form_and_never_resends() {
    use studio_runtime_services::multi_server_orchestration::ExchangeOutcome;
    let data = &fixture().data["execReceipt"];
    let python_now = fixture().data["now"].as_i64().unwrap();
    let expected = &data["freshRow"];
    let (_root, store) = store().await;
    store
        .set_local_server_id(Some("local-server".to_owned()))
        .unwrap();
    let lost = ExecWire::new(None);
    let driver = exec_driver(&store, Arc::clone(&lost));
    let outcome = driver
        .exchange(
            "peer-server",
            "exec",
            &data["payload"],
            "fresh-exec",
            python_now,
        )
        .await
        .unwrap();
    assert_eq!(outcome, ExchangeOutcome::Unknown);
    assert_eq!(
        Value::Array(lost.sent()),
        data["freshSent"],
        "original once"
    );
    let row = exec_row(&store, EXEC_OUTBOX_ROW, "fresh-exec").unwrap();
    for field in [
        "server",
        "body",
        "state",
        "result",
        "attempts",
        "fingerprint",
    ] {
        assert_eq!(
            row[field], expected[field],
            "Python _compact_exec field {field}"
        );
    }
    assert_eq!(row["next_at"].as_f64(), expected["next_at"].as_f64());

    // The same request identity now only probes; the command never repeats.
    // The first test checks this identity against the Python probe.
    let probe_id =
        studio_runtime_services::projects::cross_server_identity(&["exec-receipt", "fresh-exec"]);
    let receipt = json!({"requestId":"fresh-exec","outcome":"applied","value":{"status":"exited"}});
    let wire = ExecWire::new(Some(
        json!({"requestId":probe_id,"outcome":"applied","value":receipt}),
    ));
    let driver = exec_driver(&store, Arc::clone(&wire));
    let outcome = driver
        .exchange(
            "peer-server",
            "exec",
            &data["payload"],
            "fresh-exec",
            python_now + 5,
        )
        .await
        .unwrap();
    assert_eq!(
        outcome,
        ExchangeOutcome::Applied(json!({"status":"exited"}))
    );
    let sent = wire.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["action"], "exec_receipt");
    assert_eq!(sent[0]["requestId"], probe_id.as_str());
    assert_eq!(
        sent[0]["payload"],
        json!({"handle":"fresh-exec","actor":data["payload"]["actor"],"digest":serde_json::from_str::<Value>(expected["body"].as_str().unwrap()).unwrap()["payload"]["receiptDigest"]})
    );
    let row = exec_row(&store, EXEC_OUTBOX_ROW, "fresh-exec").unwrap();
    assert_eq!(row["state"], "complete");
    assert_eq!(row["result"], receipt);
    let mut changed = data["payload"].clone();
    changed["cwd"] = json!("/other");
    let conflict = driver
        .exchange(
            "peer-server",
            "exec",
            &changed,
            "fresh-exec",
            python_now + 6,
        )
        .await
        .unwrap_err();
    assert_eq!(refusal(conflict)["code"], "request_conflict");
    assert_eq!(wire.sent().len(), 1, "a changed command is never sent");
}

#[tokio::test]
async fn exec_receipt_probes_are_answered_like_the_python_receiver() {
    let data = &fixture().data["execReceipt"];
    let (_root, store) = store().await;
    store
        .set_local_server_id(Some("local-server".to_owned()))
        .unwrap();
    let wire = ExecWire::new(None);
    let driver = exec_driver(&store, Arc::clone(&wire));
    let cases = data["probes"].as_array().unwrap();
    assert_eq!(cases.len(), 13, "every Python probe case is checked");
    for case in cases {
        let name = case["name"].as_str().unwrap();
        if case["insertExec"] == true {
            let record = data["lateRecord"].clone();
            store
                .transaction_with(move |tx| {
                    tx.execute(
                        "INSERT INTO runtime_server_exec(id,owner,actor,record) VALUES('inbound-late','peer-server','peer-lead',?1)",
                        [record.to_string()],
                    )?;
                    Ok::<_, studio_runtime_store::StoreError>(())
                })
                .await
                .unwrap();
        }
        let envelope = &case["envelope"];
        let key = envelope["requestId"].as_str().unwrap();
        let reply = driver
            .receive(
                case["principal"].as_str().unwrap(),
                envelope,
                case["at"].as_i64().unwrap(),
            )
            .await;
        match reply {
            Ok(outcome) => assert_eq!(outcome.to_value(key), case["result"], "{name}"),
            Err(error) => {
                assert_eq!(case["result"]["refused"], "ValueError", "{name}");
                assert_eq!(refusal(error)["error"], case["result"]["message"], "{name}");
            }
        }
        assert_eq!(
            exec_row(&store, EXEC_INBOX_ROW, key),
            Some(parsed_result(case["row"].clone())),
            "{name}: Python probe receipt row"
        );
        if case["keep"] != true {
            let key = key.to_owned();
            store
                .transaction_with(move |tx| {
                    tx.execute("DELETE FROM runtime_server_inbox WHERE id=?1", [key])?;
                    Ok::<_, studio_runtime_store::StoreError>(())
                })
                .await
                .unwrap();
        }
    }
    assert!(
        wire.sent().is_empty(),
        "a probe answer never crosses the wire"
    );
}

#[tokio::test]
async fn exec_event_owner_checks_accept_the_receipt_form_like_python() {
    use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, AgentStatus};
    let data = &fixture().data["execReceipt"];
    let now = fixture().data["now"].as_i64().unwrap();
    let (_root, store) = store().await;
    store
        .set_local_server_id(Some("local-server".to_owned()))
        .unwrap();
    let driver = exec_driver(&store, ExecWire::new(None));
    let cases = data["events"].as_array().unwrap();
    assert_eq!(cases.len(), 7, "every Python exec_event case is checked");
    for case in cases {
        let name = case["name"].as_str().unwrap();
        if let Some(lead) = case["lead"].as_object() {
            let mut record = AgentRecord {
                id: AgentId::from("fixture-lead"),
                root_id: AgentId::from("fixture-lead"),
                epoch: 1,
                status: AgentStatus::Idle,
                extra: Default::default(),
            };
            record.extra.insert("autoWake".into(), json!(true));
            record.extra.insert(
                "cwd".into(),
                json!(std::env::current_dir().unwrap().display().to_string()),
            );
            record.extra.insert("concurrency".into(), json!(32));
            for (field, value) in lead {
                record.extra.insert(field.clone(), value.clone());
            }
            store
                .transaction_with(move |tx| AgentRepository::put(tx, &record))
                .await
                .unwrap();
        }
        let envelope = &case["envelope"];
        let key = envelope["requestId"].as_str().unwrap();
        let outcome = driver
            .receive(case["principal"].as_str().unwrap(), envelope, now)
            .await
            .unwrap();
        assert_eq!(outcome.to_value(key), case["result"], "{name}");
    }
    let event = &data["event"];
    let connection = store.test_reader().unwrap();
    let rows: Vec<(String, String, String, String)> = connection
        .prepare("SELECT id,agent,kind,text FROM runtime_events WHERE id LIKE 'exec-exit-%'")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![(
            event["key"].as_str().unwrap().to_owned(),
            event["agent"].as_str().unwrap().to_owned(),
            event["kind"].as_str().unwrap().to_owned(),
            event["text"].as_str().unwrap().to_owned(),
        )],
        "one event with the Python text"
    );
}

fn full_matrix() -> bool {
    std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some()
}

async fn python_authentication_outputs_match_complete_principal_and_writes() {
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
            let (_root, store) = store().await;
            store
                .transaction(move |tx| {
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
                .await
                .expect("install exact Python precondition");
            let before = rows(&store);
            let lookup = CountingOwner::new(case.clone());
            let boundary = studio_runtime_services::multi_server_pairing::MultiServerBoundary::new(
                data["identity"].clone(),
                lookup,
            )
            .expect("boundary");
            let incoming = async {
                let request = request(case)?;
                let logins: Vec<String> =
                    serde_json::from_value(case["logins"].clone()).expect("logins");
                boundary
                    .handle_incoming(
                        Arc::clone(&store),
                        &request,
                        case["address"].as_str().expect("address"),
                        &logins,
                        case["now"].as_f64().expect("now"),
                    )
                    .await
            }
            .await;
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

#[tokio::test]
#[allow(clippy::disallowed_methods)]
async fn python_and_rust_signatures_interoperate_for_multi_server_requests() {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
    use std::path::PathBuf;
    use std::process::Command;
    use studio_runtime_services::remote_transport::{
        MultiServerSigningInput, multi_server_signing_bytes, verify_signature,
    };

    const SEED: [u8; 32] = [
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c,
        0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae,
        0x7f, 0x60,
    ];
    let key = Ed25519KeyPair::from_seed_unchecked(&SEED).expect("fixed test seed loads");
    let method = "POST";
    let target = "/api/multi-server/v1/auto-pair";
    let server_id = "peer-server";
    let client_id = "rust-server";
    let timestamp = "1700000000";
    let nonce = "abcdefghijklmnopqrstuvwxyz012345";
    let request_id = "rust-request";
    let body = br#"{"clientId":"rust-server","requestId":"rust-request"}"#;
    let rust_wire = multi_server_signing_bytes(MultiServerSigningInput {
        method,
        target,
        server_id,
        client_id,
        timestamp,
        nonce,
        request_id,
        body,
    });
    let rust_signature = STANDARD.encode(key.sign(&rust_wire).as_ref());
    UnparsedPublicKey::new(&ED25519, key.public_key().as_ref())
        .verify(
            &rust_wire,
            &STANDARD.decode(&rust_signature).expect("signature decodes"),
        )
        .expect("Rust signature verifies locally");
    let mut spki = vec![
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    spki.extend_from_slice(key.public_key().as_ref());
    let public_key = format!(
        "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
        STANDARD.encode(spki)
    );
    let python_input = json!({
        "method":method, "target":target, "serverId":server_id,
        "clientId":client_id, "timestamp":timestamp, "nonce":nonce,
        "requestId":request_id, "body":STANDARD.encode(body),
        "seed":STANDARD.encode(SEED), "rustSignature":rust_signature,
        "publicKey":public_key,
    });
    let server_source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/multi_server/interop_verify.py");
    let mut command = Command::new(prepared_python_support::prepared_python(&server_source));
    command
        .arg(&script)
        .arg(python_input.to_string())
        .env("PYTHONDONTWRITEBYTECODE", "1");
    let environment = prepared_python_support::IsolatedTestEnvironment::new();
    environment.apply(&mut command);
    let output = command.output().expect("run Python signature verifier");
    assert!(
        output.status.success(),
        "Python rejected the Rust signature: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let python_result: Value =
        serde_json::from_slice(&output.stdout).expect("Python signing and verification JSON");
    assert_eq!(python_result["verified"], true);
    let python_signature = python_result["pythonSignature"]
        .as_str()
        .expect("Python produced a request signature");
    let python_wire = multi_server_signing_bytes(MultiServerSigningInput {
        method,
        target,
        server_id,
        client_id,
        timestamp,
        nonce,
        request_id,
        body,
    });
    verify_signature(&public_key, &python_wire, python_signature)
        .expect("Rust verifies Python's fixture request signature");
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
async fn pairing_is_atomic_at_every_write_and_replays_after_restart() {
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
        let (root, store) = store().await;
        let before = rows(&store);
        store.transaction(move |tx|{tx.execute_batch(&format!("CREATE TRIGGER failpoint BEFORE {operation} ON {table} BEGIN SELECT RAISE(ABORT,'fixture failpoint'); END"))?;Ok(())})
            .await.expect("failpoint");
        assert!(
            boundary()
                .handle(
                    &store,
                    &request(case).expect("request"),
                    &owner(),
                    None,
                    now
                )
                .await
                .is_err(),
            "{table}"
        );
        assert_eq!(rows(&store), before, "rollback at {table}");
        store
            .transaction(|tx| {
                tx.execute_batch("DROP TRIGGER failpoint")?;
                Ok(())
            })
            .await
            .expect("remove failpoint");
        drop(store);
        let reopened = studio_runtime_store::RuntimeStore::open(Some(root.path()))
            .await
            .expect("restart");
        let result = boundary()
            .handle(
                &reopened,
                &request(case).expect("request"),
                &owner(),
                None,
                now,
            )
            .await
            .expect("first apply after rollback");
        assert_eq!(result["paired"], true);
        let status: String = reopened
            .test_reader()
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
                .await
                .expect("recover pairing result"),
            result
        );
        let audit:i64=reopened.test_reader().expect("read").query_row("SELECT count(*) FROM runtime_access_audit WHERE client='new-client' AND action='pair'",[],|r|r.get(0)).expect("audit");
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
                .await
                .is_err(),
            "nonce replay"
        );
    }
}

#[tokio::test]
async fn unknown_peer_input_claim_is_not_reopened_as_waiting() {
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
            action == "input"
        }
        fn execute<'a>(
            &'a self,
            _: &'a str,
            _: &'a Value,
        ) -> futures_util::future::BoxFuture<'a, Result<ExchangeOutcome, AccessError>> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(ExchangeOutcome::Unknown)
            })
        }
    }
    struct NoTransport;
    impl PairedTransport for NoTransport {
        fn exchange<'a>(
            &'a self,
            _server: &'a str,
            _envelope: &'a Value,
            _timeout: Duration,
        ) -> futures_util::future::BoxFuture<'a, Result<Value, AccessError>> {
            Box::pin(async move { panic!("inbound input must not call the transport") })
        }
    }

    let (root, store) = store().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let driver = OrchestrationDriver::new(
        store.clone(),
        Arc::new(NoTransport),
        Arc::new(Effects(calls.clone())),
    );
    let envelope = json!({
        "requestId":"uncertain-input",
        "action":"input",
        "payload":{"actor":"fixture"}
    });
    assert_eq!(
        driver
            .receive("peer-server", &envelope, 1_700_000_000)
            .await
            .expect("first input attempt"),
        ExchangeOutcome::Unknown
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let state: String = store
        .test_reader()
        .expect("read receipt")
        .query_row(
            "SELECT state FROM runtime_server_inbox WHERE id='uncertain-input'",
            [],
            |row| row.get(0),
        )
        .expect("saved receipt");
    assert_eq!(state, "running");

    assert_eq!(
        driver
            .receive("peer-server", &envelope, 1_700_000_001)
            .await
            .expect("same input identity"),
        ExchangeOutcome::Unknown
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "unknown input is never retried"
    );
    let state: String = store
        .test_reader()
        .expect("read receipt")
        .query_row(
            "SELECT state FROM runtime_server_inbox WHERE id='uncertain-input'",
            [],
            |row| row.get(0),
        )
        .expect("saved receipt");
    assert_eq!(state, "running");
    drop(root);
}

#[tokio::test]
async fn only_typed_pre_effect_waiting_reopens_peer_input() {
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
            action == "input"
        }
        fn execute<'a>(
            &'a self,
            _: &'a str,
            _: &'a Value,
        ) -> futures_util::future::BoxFuture<'a, Result<ExchangeOutcome, AccessError>> {
            Box::pin(async move {
                if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(ExchangeOutcome::PreEffectWaiting)
                } else {
                    Ok(ExchangeOutcome::Applied(json!({"applied":true})))
                }
            })
        }
    }
    struct NoTransport;
    impl PairedTransport for NoTransport {
        fn exchange<'a>(
            &'a self,
            _server: &'a str,
            _envelope: &'a Value,
            _timeout: Duration,
        ) -> futures_util::future::BoxFuture<'a, Result<Value, AccessError>> {
            Box::pin(async move { panic!("inbound input must not call the transport") })
        }
    }

    let (_root, store) = store().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let driver = OrchestrationDriver::new(
        store,
        Arc::new(NoTransport),
        Arc::new(Effects(calls.clone())),
    );
    let envelope = json!({
        "requestId":"waiting-input",
        "action":"input",
        "payload":{"actor":"fixture"}
    });
    assert_eq!(
        driver
            .receive("peer-server", &envelope, 1_700_000_000)
            .await
            .expect("waiting input attempt"),
        ExchangeOutcome::PreEffectWaiting
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        driver
            .receive("peer-server", &envelope, 1_700_000_001)
            .await
            .expect("retry after known pre-effect waiting"),
        ExchangeOutcome::Applied(json!({"applied":true}))
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn multi_server_corpus_witness() {
    let (_root, store) = store().await;
    let snapshot =
        studio_runtime_services::multi_server::snapshot(&store, &fixture().data["identity"])
            .await
            .expect("access snapshot");
    assert_eq!(
        snapshot["protocol"],
        serde_json::json!(studio_runtime_schema::multi_server_protocol::ACCESS_PROTOCOL_VERSION)
    );

    let report = studio_runtime_store::check_state_readonly(store.root())
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
    python_authentication_outputs_match_complete_principal_and_writes().await;
    malformed_stored_rows_are_located_without_panics();
    owner_observations_reject_tags_wrong_ip_and_spoofed_login();
    discovery_includes_local_other_port_and_only_online_owner_nodes();
    pairing_is_atomic_at_every_write_and_replays_after_restart().await;
    orchestration_claims_survive_lost_receipts_and_reject_changed_content().await;
    python_receipt_states_preserve_completed_results_and_uncertainty().await;
    nonce_capacity_is_rate_limited_without_committing_access_writes().await;
    signatures_precede_owner_lookups_and_probes().await;
    queried_status_crosses_service_dispatch().await;
    fractional_invitation_boundary_matches_python().await;
}

async fn orchestration_claims_survive_lost_receipts_and_reject_changed_content() {
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
        fn execute<'a>(
            &'a self,
            _: &'a str,
            _: &'a Value,
        ) -> futures_util::future::BoxFuture<'a, Result<ExchangeOutcome, AccessError>> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(ExchangeOutcome::Applied(json!({"applied":true})))
            })
        }
    }
    struct Transport(Arc<AtomicUsize>, bool);
    impl PairedTransport for Transport {
        fn exchange<'a>(
            &'a self,
            _server: &'a str,
            envelope: &'a Value,
            _timeout: Duration,
        ) -> futures_util::future::BoxFuture<'a, Result<Value, AccessError>> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(if self.1 {
                    json!({"requestId":envelope["requestId"],"outcome":"unknown"})
                } else {
                    json!({"requestId":envelope["requestId"],"outcome":"applied","value":{"applied":true}})
                })
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
            let (root, store) = store().await;
            let calls = Arc::new(AtomicUsize::new(0));
            let event = if stage == "claim" { "INSERT" } else { "UPDATE" };
            store.transaction(move |tx|{tx.execute_batch(&format!("CREATE TRIGGER failpoint BEFORE {event} ON runtime_server_{direction} BEGIN SELECT RAISE(ABORT,'fixture failpoint'); END"))?;Ok(())})
            .await.expect("failpoint");
            let run = |store: Arc<studio_runtime_store::RuntimeStore>| async {
                let driver = OrchestrationDriver::new(
                    store,
                    Arc::new(Transport(calls.clone(), false)),
                    Arc::new(Effects(calls.clone())),
                );
                if *direction == "inbox" {
                    driver.receive("peer-server",&json!({"requestId":"effect-a","action":"fixture","payload":{"actor":"fixture"}}),now).await
                } else {
                    driver
                        .exchange(
                            "peer-server",
                            "fixture",
                            &json!({"actor":"fixture"}),
                            "effect-a",
                            now,
                        )
                        .await
                }
            };
            assert!(run(store.clone()).await.is_err(), "{direction}/{stage}");
            assert_eq!(calls.load(Ordering::SeqCst), usize::from(stage == "finish"));
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER failpoint")?;
                    Ok(())
                })
                .await
                .expect("remove failpoint");
            drop(store);
            let reopened = Arc::new(
                studio_runtime_store::RuntimeStore::open(Some(root.path()))
                    .await
                    .expect("restart"),
            );
            let result = run(reopened.clone()).await.expect("recovery");
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
                driver.receive("peer-server",&json!({"requestId":"effect-a","action":"fixture","payload":{"actor":"different"}}),now).await
            } else {
                driver
                    .exchange(
                        "peer-server",
                        "fixture",
                        &json!({"actor":"different"}),
                        "effect-a",
                        now,
                    )
                    .await
            };
            assert_eq!(changed.expect_err("conflict").status(), 409);
        }
    }
    for case in fixture().data["deliveryStates"].as_array().expect("states") {
        let (root, store) = store().await;
        // Template rows come from Python receive(), including waiting's proof
        // that no effect crossed the boundary. Reopen before attempting it.
        drop(store);
        let store = Arc::new(
            studio_runtime_store::RuntimeStore::open(Some(root.path()))
                .await
                .expect("restart"),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let driver = OrchestrationDriver::new(
            store.clone(),
            Arc::new(Transport(calls.clone(), false)),
            Arc::new(Effects(calls.clone())),
        );
        let result = driver
            .receive("peer-server", &case["envelope"], now)
            .await
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
            .await
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
        let (root, store) = store().await;
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
        store.transaction(move |tx| { tx.execute_batch(&format!("CREATE TRIGGER waiting_fail BEFORE UPDATE ON runtime_server_inbox WHEN NEW.state='{state}' BEGIN SELECT RAISE(ABORT,'waiting failpoint'); END"))?; Ok(()) })
            .await.expect("failpoint");
        let calls = Arc::new(AtomicUsize::new(0));
        let run = |store: Arc<studio_runtime_store::RuntimeStore>| async {
            OrchestrationDriver::new(
                store,
                Arc::new(Transport(calls.clone(), false)),
                Arc::new(Effects(calls.clone())),
            )
            .receive("peer-server", &case["envelope"], now)
            .await
        };
        assert!(run(store.clone()).await.is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            usize::from(*stage == "finish")
        );
        store
            .transaction(|tx| {
                tx.execute_batch("DROP TRIGGER waiting_fail")?;
                Ok(())
            })
            .await
            .expect("remove failpoint");
        drop(store);
        let store = Arc::new(
            studio_runtime_store::RuntimeStore::open(Some(root.path()))
                .await
                .expect("restart"),
        );
        assert_eq!(
            run(store).await.expect("recovery"),
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
    let (_root, store) = store().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let driver = OrchestrationDriver::new(
        store.clone(),
        Arc::new(Transport(calls.clone(), true)),
        Arc::new(Effects(calls.clone())),
    );
    assert_eq!(
        driver
            .exchange("peer-server", "fixture", &json!({}), "unknown", now)
            .await
            .expect("unknown"),
        ExchangeOutcome::Unknown
    );
    assert_eq!(
        driver
            .exchange("peer-server", "fixture", &json!({}), "unknown", now + 100)
            .await
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
            .await
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
            .await
            .expect("Python-written queue"),
        ExchangeOutcome::Unknown
    );
}

async fn python_receipt_states_preserve_completed_results_and_uncertainty() {
    let data = &fixture().data;
    let case = &data["auth"][0];
    let now = data["now"].as_i64().expect("now");
    for state in if full_matrix() {
        &["pending", "complete", "tombstone", "expired", "malformed"][..]
    } else {
        &["pending", "expired"][..]
    } {
        let state = *state;
        let (_root, store) = store().await;
        let result = boundary()
            .handle(
                &store,
                &request(case).expect("request"),
                &owner(),
                None,
                now,
            )
            .await
            .expect("pair");
        store.transaction(move |tx| {
            if state == "expired" {
                tx.execute("UPDATE runtime_access_receipts SET record=json_set(record,'$.completed',?1) WHERE client='new-client'", [now-8*86400])?;
            } else if state == "malformed" {
                tx.execute("UPDATE runtime_access_receipts SET record=json_set(record,'$.body',9) WHERE client='new-client'", [])?;
            } else {
                tx.execute("UPDATE runtime_access_receipts SET status=?1 WHERE client='new-client'", [state])?;
            }
            Ok(())
        })
            .await.expect("saved Python-compatible state");
        let before = rows(&store);
        let replay = boundary()
            .handle(
                &store,
                &request(&data["freshPair"]).expect("request"),
                &owner(),
                None,
                now,
            )
            .await;
        if matches!(state, "pending" | "complete") {
            assert_eq!(replay.expect("recovered receipt"), result);
            let saved: String = store
                .test_reader()
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

async fn nonce_capacity_is_rate_limited_without_committing_access_writes() {
    let data = &fixture().data;
    let now = data["now"].as_i64().expect("now");
    for (case_name, client, count) in [
        ("status-server", "peer-server", 10_000),
        ("pair-ui", "unpaired-fixture", 1_280),
    ] {
        let (_root, store) = store().await;
        store.transaction(move |tx| {
            tx.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<?1) INSERT INTO runtime_access_nonces(client,nonce,created) SELECT ?2,printf('%032d',x),?3 FROM n",rusqlite::params![count,client,now])?;
            Ok(())
        })
            .await.expect("nonce capacity fixture");
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
            .await
            .expect_err("capacity refusal");
        assert_eq!(error.status(), 429);
        assert_eq!(rows(&store), before, "rate refusal must not commit writes");
    }
}

async fn fractional_invitation_boundary_matches_python() {
    let data = &fixture().data;
    for case in data["auth"].as_array().expect("cases").iter().filter(|c| {
        c["name"]
            .as_str()
            .expect("name")
            .starts_with("invite-expiry-")
    }) {
        let (_root, store) = store().await;
        store
            .transaction(move |tx| {
                tx.execute(
                    "UPDATE runtime_access_invites SET record=?1 WHERE id=?2",
                    rusqlite::params![
                        case["invite"].as_str().expect("invite"),
                        case["inviteId"].as_str().expect("id")
                    ],
                )?;
                Ok(())
            })
            .await
            .expect("Python fractional expiry");
        let before = rows(&store);
        let result = boundary()
            .handle_at(
                &store,
                &request(case).expect("request"),
                &owner(),
                None,
                case["now"].as_f64().expect("time"),
            )
            .await;
        if let Some(expected) = case.get("refusal") {
            assert_eq!(refusal(result.expect_err("expired")), *expected);
            assert_eq!(rows(&store), before);
        } else {
            assert_eq!(result.expect("not expired")["paired"], true);
        }
    }
}

async fn queried_status_crosses_service_dispatch() {
    let data = &fixture().data;
    let case = data["auth"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["name"] == "status-query")
        .expect("queried status");
    let (_root, store) = store().await;
    store
        .transaction(move |tx| {
            tx.execute(
                "UPDATE runtime_access_clients SET record=?1 WHERE id='peer-server'",
                [case["client"].as_str().expect("Python precondition")],
            )?;
            Ok(())
        })
        .await
        .expect("install exact Python authentication precondition");
    let lookup = CountingOwner::new(case.clone());
    let boundary = studio_runtime_services::multi_server_pairing::MultiServerBoundary::new(
        data["identity"].clone(),
        lookup,
    )
    .expect("boundary");
    let result = boundary
        .handle_incoming(
            Arc::clone(&store),
            &request(case).expect("signed target"),
            "100.64.0.2",
            &[],
            case["now"].as_f64().expect("clock"),
        )
        .await
        .expect("dispatch by path after checking full signed target");
    assert_eq!(result, data["identity"]);
    let stored: String = store
        .test_reader()
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

async fn signatures_precede_owner_lookups_and_probes() {
    use std::sync::atomic::Ordering;
    use studio_runtime_services::multi_server_pairing::MultiServerBoundary;
    let data = &fixture().data;
    for (bad, good) in [
        ("bad-signature", "pair-ui"),
        ("bad-auto-signature", "auto-pair"),
    ] {
        let (_root, store) = store().await;
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
                Arc::clone(&store),
                &request(case).expect("request"),
                "100.64.0.2",
                &[],
                case["now"].as_f64().expect("now"),
            )
            .await
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
                Arc::clone(&store),
                &request(good).expect("request"),
                "100.64.0.2",
                &[],
                good["now"].as_f64().expect("now"),
            )
            .await
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

#[tokio::test]
async fn a_row_that_cannot_be_delivered_does_not_hold_back_later_rows() {
    use std::sync::Mutex;
    use studio_runtime_services::multi_server_auth::AccessError;
    use studio_runtime_services::multi_server_orchestration::{
        ExchangeOutcome, OrchestrationDriver, OrchestrationEffects, PairedTransport,
        enqueue_in_transaction,
    };

    struct Effects;
    impl OrchestrationEffects for Effects {
        fn supports(&self, _: &str) -> bool {
            false
        }
        fn execute<'a>(
            &'a self,
            _: &'a str,
            _: &'a Value,
        ) -> futures_util::future::BoxFuture<'a, Result<ExchangeOutcome, AccessError>> {
            Box::pin(async { Ok(ExchangeOutcome::Unknown) })
        }
    }
    #[derive(Default)]
    struct Peer {
        delivered: Mutex<Vec<String>>,
    }
    impl PairedTransport for Peer {
        fn exchange<'a>(
            &'a self,
            _server: &'a str,
            envelope: &'a Value,
            _timeout: Duration,
        ) -> futures_util::future::BoxFuture<'a, Result<Value, AccessError>> {
            Box::pin(async move {
                let id = envelope["requestId"].as_str().unwrap().to_owned();
                self.delivered.lock().unwrap().push(id.clone());
                Ok(json!({"requestId":id,"outcome":"applied","value":{"stored":true}}))
            })
        }
    }

    let (_root, store) = store().await;
    store
        .transaction_with(|tx| {
            for key in ["rewritten-1", "later-2"] {
                enqueue_in_transaction(
                    tx,
                    "peer-server",
                    "message",
                    &json!({"link":"link-1","worker":"worker-1","args":{"text":key}}),
                    key,
                    100,
                )
                .unwrap();
            }
            // Python rewrites some stored bodies after queueing (an exec receipt probe), so
            // the stored body no longer matches the fingerprint of the original request.
            tx.execute(
                "UPDATE runtime_server_outbox SET body=json_set(body,'$.payload.args.text','changed') WHERE id='rewritten-1'",
                [],
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .unwrap();
    let peer = Arc::new(Peer::default());
    let driver = OrchestrationDriver::new(
        Arc::clone(&store),
        Arc::clone(&peer) as _,
        Arc::new(Effects),
    );
    // The Python-written fixture may hold other due rows; only these two matter here.
    assert!(driver.pump_due(105).await.unwrap() >= 1);
    let delivered = peer.delivered.lock().unwrap();
    assert!(delivered.contains(&"later-2".to_owned()), "{delivered:?}");
    assert!(
        !delivered.contains(&"rewritten-1".to_owned()),
        "{delivered:?}"
    );
}
