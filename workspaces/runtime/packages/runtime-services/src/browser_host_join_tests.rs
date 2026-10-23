//! One card for one sign-in (docs/vm-browser-bridge.md, "One card for the same sign-in"):
//! a link of another chat with the same Linux user, sign-in host and client joins the open
//! request. When that sign-in ends, every joined request ends with it: signed in, or the
//! oldest joined request becomes the card.

use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

const FIRST: &str = "6f1c2b3a-4d5e-4f60-8a9b-0c1d2e3f4a5b";
const SECOND: &str = "7a2d3c4b-5e6f-4071-9bac-1d2e3f4a5b6c";
const THIRD: &str = "8b3e4d5c-6f70-4182-acbd-2e3f4a5b6c7d";

fn link(client: &str, port: u16, state: &str) -> String {
    format!(
        "https://id.example.com/authorize?client_id={client}&response_type=code&redirect_uri=http%3A%2F%2F127.0.0.1%3A{port}%2Fcallback&state={state}"
    )
}

struct EveryChat;

impl ChatLookup for EveryChat {
    fn chat<'a>(
        &'a self,
        session: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Option<ChatContext>> {
        Box::pin(std::future::ready(Some(ChatContext {
            uid: current_uid(),
            project_id: None,
            owner: "person".into(),
            agent_id: session.to_owned(),
        })))
    }
}

struct Fixture {
    _folder: tempfile::TempDir,
    bridge: Arc<HostBridge>,
    now: Arc<AtomicU64>,
}

impl Fixture {
    fn new() -> Self {
        let folder = tempfile::tempdir().expect("bridge folder");
        let now = Arc::new(AtomicU64::new(1000.0_f64.to_bits()));
        let clock = Arc::clone(&now);
        let bridge = HostBridge::open(
            folder.path(),
            Arc::new(EveryChat),
            HostBridgeOptions {
                // Every command of the test listens on its own port.
                listeners: Arc::new(|| {
                    Some(
                        (4240..4250)
                            .map(|port| Listener {
                                address: "127.0.0.1".into(),
                                port,
                                uid: current_uid(),
                            })
                            .collect(),
                    )
                }),
                clock: Arc::new(move || f64::from_bits(clock.load(Ordering::SeqCst))),
                ..HostBridgeOptions::default()
            },
        )
        .expect("bridge");
        Self {
            _folder: folder,
            bridge,
            now,
        }
    }

    async fn submit(&self, agent: &str, url: &str) -> Value {
        self.bridge
            .call("browser.submit", &json!({"agentId": agent, "url": url}))
            .await
            .expect("the link queues")
    }

    fn item(&self, id: &Value) -> Value {
        self.bridge.lock().items[id.as_str().expect("id")].clone()
    }

    fn end(&self, id: &Value, fields: Value) {
        let mut queue = self.bridge.lock();
        self.bridge
            .update(&mut queue, id.as_str().expect("id"), fields);
    }
}

#[tokio::test]
async fn a_second_chat_with_the_same_user_host_and_client_joins_the_open_request() {
    let fixture = Fixture::new();
    let first = fixture.submit(FIRST, &link("cli", 4241, "s1")).await;
    assert_eq!(first["state"], "pending");
    fixture.now.store(1001.0_f64.to_bits(), Ordering::SeqCst);
    let second = fixture.submit(SECOND, &link("cli", 4242, "s2")).await;
    assert_eq!(
        (second["state"].clone(), second["joinedTo"].clone()),
        (json!(JOINED), first["id"].clone())
    );
    // Another client gets its own card.
    let other = fixture.submit(THIRD, &link("other", 4243, "s3")).await;
    assert_eq!(other["state"], "pending");
    assert!(other.get("joinedTo").is_none());
    // The same chat with a newer link still replaces its own link.
    let again = fixture.submit(FIRST, &link("cli", 4244, "s4")).await;
    assert_eq!(again["state"], "pending");
    assert_eq!(fixture.item(&first["id"])["state"], "replaced");
}

#[tokio::test]
async fn a_finished_sign_in_ends_every_joined_request_as_signed_in() {
    let fixture = Fixture::new();
    let first = fixture.submit(FIRST, &link("cli", 4241, "s1")).await;
    let second = fixture.submit(SECOND, &link("cli", 4242, "s2")).await;
    let third = fixture.submit(THIRD, &link("cli", 4243, "s3")).await;
    assert_eq!(third["joinedTo"], first["id"]);
    fixture.bridge.check().await;
    assert_eq!(
        fixture.item(&second["id"])["state"],
        JOINED,
        "it waits on the card"
    );
    // The person signed in on the card: the command took the result.
    fixture.end(
        &first["id"],
        json!({"state": "closed", "closeReason": "completed"}),
    );
    fixture.bridge.check().await;
    for joined in [&second, &third] {
        let item = fixture.item(&joined["id"]);
        assert_eq!(
            (item["state"].clone(), item["closeReason"].clone()),
            (json!("closed"), json!(JOINED_SIGNED_IN_REASON))
        );
    }
}

#[tokio::test]
async fn a_card_that_ends_without_a_sign_in_gives_the_oldest_joined_request_the_card() {
    let fixture = Fixture::new();
    let first = fixture.submit(FIRST, &link("cli", 4241, "s1")).await;
    fixture.now.store(1001.0_f64.to_bits(), Ordering::SeqCst);
    let second = fixture.submit(SECOND, &link("cli", 4242, "s2")).await;
    fixture.now.store(1002.0_f64.to_bits(), Ordering::SeqCst);
    let third = fixture.submit(THIRD, &link("cli", 4243, "s3")).await;
    fixture.end(&first["id"], json!({"state": "declined"}));
    fixture.bridge.check().await;
    let promoted = fixture.item(&second["id"]);
    assert_eq!(promoted["state"], "pending");
    assert!(promoted.get("joinedTo").is_none_or(Value::is_null));
    assert_eq!(fixture.item(&third["id"])["joinedTo"], second["id"]);
}

#[tokio::test]
async fn a_device_code_link_without_a_client_never_joins() {
    let fixture = Fixture::new();
    let first = fixture
        .submit(FIRST, "https://id.example.com/device?user_code=AAAA-BBBB")
        .await;
    let second = fixture
        .submit(SECOND, "https://id.example.com/device?user_code=CCCC-DDDD")
        .await;
    assert_eq!(
        (first["state"].clone(), second["state"].clone()),
        (json!("pending"), json!("pending"))
    );
}
