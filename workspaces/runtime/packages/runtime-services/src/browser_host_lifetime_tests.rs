//! The lifetime of a pending sign-in request (docs/vm-browser-bridge.md, "Card lifetime"):
//! while its command listens, the request stays open past the request time, up to
//! `SIGN_IN_OPEN_LIMIT_SECONDS` from its creation.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use studio_runtime_schema::shared_values::SIGN_IN_OPEN_LIMIT_SECONDS;

struct NoChats;

impl ChatLookup for NoChats {
    fn chat<'a>(
        &'a self,
        _session: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Option<ChatContext>> {
        Box::pin(async { None })
    }
}

const PORT: u16 = 4242;
const UID: u32 = 501;
const CREATED: f64 = 100.0;

/// A saved pending request with a loopback redirect on `PORT`.
fn saved(id: &str) -> Value {
    json!({
        "id": id, "agentId": "agent", "uid": UID, "state": "pending",
        "url": "https://id.example.com/authorize", "startUrl": null, "host": "id.example.com",
        "redirect": {"host": "127.0.0.1", "port": PORT, "path": "/callback"},
        "createdAt": CREATED, "expiresAt": CREATED + TTL_SECONDS, "relays": 0, "lease": null,
        "seq": 1
    })
}

struct Fixture {
    _folder: tempfile::TempDir,
    bridge: Arc<HostBridge>,
    now: Arc<AtomicU64>,
    listening: Arc<AtomicBool>,
}

impl Fixture {
    fn new(ids: &[&str]) -> Self {
        let folder = tempfile::tempdir().expect("bridge folder");
        let directory = folder.path().join("browser");
        std::fs::create_dir_all(&directory).expect("browser folder");
        let items: serde_json::Map<String, Value> = ids
            .iter()
            .map(|id| ((*id).to_owned(), saved(id)))
            .collect();
        std::fs::write(
            directory.join("requests.json"),
            json!({"seq": 1, "items": items}).to_string(),
        )
        .expect("saved queue");
        let now = Arc::new(AtomicU64::new(CREATED.to_bits()));
        let listening = Arc::new(AtomicBool::new(true));
        let (clock, listener) = (Arc::clone(&now), Arc::clone(&listening));
        let bridge = HostBridge::open(
            folder.path(),
            Arc::new(NoChats),
            HostBridgeOptions {
                listeners: Arc::new(move || {
                    if listener.load(Ordering::SeqCst) {
                        vec![Listener {
                            address: "127.0.0.1".into(),
                            port: PORT,
                            uid: UID,
                        }]
                    } else {
                        Vec::new()
                    }
                }),
                http_providers: BTreeSet::new(),
                clock: Arc::new(move || f64::from_bits(clock.load(Ordering::SeqCst))),
            },
        )
        .expect("bridge");
        Self {
            _folder: folder,
            bridge,
            now,
            listening,
        }
    }

    fn at(&self, seconds: f64) {
        self.now.store(seconds.to_bits(), Ordering::SeqCst);
    }

    fn item(&self, id: &str) -> Value {
        self.bridge.lock().items[id].clone()
    }
}

#[tokio::test]
async fn a_listening_command_keeps_its_request_open_up_to_the_open_limit() {
    let fixture = Fixture::new(&["card"]);
    let limit = CREATED + SIGN_IN_OPEN_LIMIT_SECONDS as f64;
    // One check every 5 s, as the check loop runs, from creation to past the limit.
    let mut now = CREATED;
    while now <= limit {
        fixture.at(now);
        fixture.bridge.check().await;
        let item = fixture.item("card");
        assert_eq!(item["state"], "pending", "the command listens at {now}");
        let expires = item["expiresAt"].as_f64().expect("deadline");
        assert!(expires >= now, "the deadline {expires} passed at {now}");
        assert!(expires <= limit, "the deadline {expires} is past the limit");
        now += CHECK_SECONDS as f64;
    }
    fixture.at(limit + 1.0);
    fixture.bridge.check().await;
    assert_eq!(fixture.item("card")["state"], "expired");
}

#[tokio::test]
async fn the_person_can_approve_a_listening_request_after_the_request_time() {
    let fixture = Fixture::new(&["card"]);
    let mut now = CREATED;
    while now < CREATED + 2.0 * TTL_SECONDS {
        fixture.at(now);
        fixture.bridge.check().await;
        now += CHECK_SECONDS as f64;
    }
    let answer = fixture
        .bridge
        .call("browser.answer", &json!({"id": "card", "decision": "approve"}))
        .await
        .expect("the request is still open");
    assert_eq!(answer["state"], "approved");
}

#[tokio::test]
async fn a_request_whose_command_stopped_listening_becomes_stale_at_once() {
    let fixture = Fixture::new(&["card"]);
    fixture.at(CREATED + TTL_SECONDS / 2.0);
    fixture.bridge.check().await;
    assert_eq!(fixture.item("card")["state"], "pending");
    fixture.listening.store(false, Ordering::SeqCst);
    fixture.bridge.check().await;
    assert_eq!(fixture.item("card")["state"], "stale");
}
