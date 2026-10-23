//! Cards from a fake guest `browser.wait` (the Python server's `Bridge.apply` rules).

use super::{CardSource, card_id, poll_once};
use crate::linux_vm_guest::{GuestError, GuestRpc};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct WaitGuest {
    batches: Mutex<VecDeque<Value>>,
    calls: Mutex<Vec<(String, Value)>>,
}

impl GuestRpc for WaitGuest {
    fn stream(
        &self,
        method: &str,
        params: &Value,
        _request_id: Option<&str>,
        _timeout: Duration,
        _on_frame: &mut dyn FnMut(Value) -> Result<(), GuestError>,
    ) -> Result<Value, GuestError> {
        self.calls
            .lock()
            .expect("test step")
            .push((method.to_owned(), params.clone()));
        match method {
            "browser.wait" => self
                .batches
                .lock()
                .expect("test step")
                .pop_front()
                .ok_or(GuestError::InvalidRequest),
            "browser.close" => Ok(json!({"state":"closed"})),
            _ => Err(GuestError::InvalidRequest),
        }
    }
}

const VM_AGENT: &str = "vm-agent";
const KUBE: &str = "https://id.example.com/auth?client_id=kube&redirect_uri=http%3A%2F%2Flocalhost%3A8000&scope=openid&state=S1";
const DEVICE: &str = "https://device.sso.us-east-1.amazonaws.com/?user_code=ABCD-EFGH";

fn item(id: &str, agent: &str, url: &str, redirect: Value, state: &str, seq: u64) -> Value {
    let host = url.split('/').nth(2).expect("test step");
    json!({"id": id, "agentId": agent, "projectId": "p1", "uid": 1010, "linuxUser": "studio-p-1",
           "url": url, "startUrl": null, "host": host, "redirect": redirect, "clientId": "kube",
           "scope": "openid", "state": state, "createdAt": 1.0, "expiresAt": 601.0, "relays": 0,
           "lease": null, "seq": seq})
}

async fn store() -> (tempfile::TempDir, studio_runtime_store::RuntimeStore) {
    let dir = tempfile::tempdir().expect("temporary runtime state");
    let store = studio_runtime_store::RuntimeStore::open(Some(dir.path()))
        .await
        .expect("runtime store opens");
    store
        .transaction_with(|tx| {
            tx.execute(
                "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                [
                    VM_AGENT,
                    &json!({"id":VM_AGENT,"rootId":VM_AGENT,"epoch":1,"status":"running","autoWake":true,
                             "executionMode":"vm","layrProjectId":"p1"})
                    .to_string(),
                ],
            )
            .map_err(studio_runtime_store::StoreError::from)?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("agent is seeded");
    (dir, store)
}

fn request(store: &studio_runtime_store::RuntimeStore, bridge: &str) -> Option<Value> {
    store
        .test_read_transaction(|tx| {
            studio_runtime_store::QuestionsWriteRepository::request(tx, &card_id(bridge))
        })
        .expect("request reads")
        .map(|raw| serde_json::from_str(&raw).expect("request JSON"))
}

#[tokio::test]
async fn pending_links_become_cards_once_and_unknown_chats_are_refused() {
    let (_dir, store) = store().await;
    let kube = "a".repeat(32);
    let device = "b".repeat(32);
    let stranger = "c".repeat(32);
    let redirect = json!({"host":"localhost","port":8000,"path":"/"});
    let guest = Arc::new(WaitGuest {
        batches: Mutex::new(VecDeque::from([
            json!({"items":[
                item(&kube, VM_AGENT, KUBE, redirect.clone(), "pending", 1),
                item(&device, VM_AGENT, DEVICE, Value::Null, "pending", 2),
                item(&stranger, "missing-agent", KUBE, redirect.clone(), "pending", 3),
            ], "next": 3}),
            json!({"items":[item(&kube, VM_AGENT, KUBE, redirect.clone(), "pending", 4)], "next": 4}),
        ])),
        calls: Mutex::new(Vec::new()),
    });
    let source = CardSource {
        source: "vm",
        guest: guest.clone(),
        forwarder: None,
        http_providers: Default::default(),
    };
    assert_eq!(
        poll_once(&store, &source, 0, 0, None)
            .await
            .expect("test step"),
        3
    );
    let card = request(&store, &kube).expect("kubelogin card");
    assert_eq!(card["method"], "browser/open");
    assert_eq!(card["agent"], VM_AGENT);
    assert_eq!(card["status"], "pending");
    assert_eq!(card["params"]["redirect"], redirect);
    assert_eq!(card["params"]["source"], "vm");
    assert_eq!(card["params"]["userCode"], Value::Null);
    let device_card = request(&store, &device).expect("device card");
    assert_eq!(device_card["params"]["userCode"], "ABCD-EFGH");
    assert_eq!(device_card["params"]["redirect"], Value::Null);
    assert!(request(&store, &stranger).is_none());
    let closes: Vec<Value> = guest
        .calls
        .lock()
        .expect("test step")
        .iter()
        .filter(|call| call.0 == "browser.close")
        .map(|call| call.1.clone())
        .collect();
    assert_eq!(closes, vec![json!({"id": stranger, "reason": "refused"})]);
    let created = card["createdAt"].clone();
    assert_eq!(
        poll_once(&store, &source, 3, 0, None)
            .await
            .expect("test step"),
        4
    );
    assert_eq!(
        request(&store, &kube).expect("test step")["createdAt"],
        created,
        "a second poll never makes a second card"
    );
}

#[tokio::test]
async fn a_final_guest_state_expires_the_card_and_tells_the_chat_after_an_approval() {
    let (_dir, store) = store().await;
    let waiting = "d".repeat(32);
    let pasting = "e".repeat(32);
    let redirect = json!({"host":"localhost","port":8000,"path":"/"});
    let guest = Arc::new(WaitGuest {
        batches: Mutex::new(VecDeque::from([
            json!({"items":[
                item(&waiting, VM_AGENT, KUBE, redirect.clone(), "pending", 1),
                item(&pasting, VM_AGENT, KUBE, redirect.clone(), "pending", 2),
            ], "next": 2}),
            json!({"items":[
                item(&waiting, VM_AGENT, KUBE, redirect.clone(), "stale", 3),
                item(&pasting, VM_AGENT, KUBE, redirect.clone(), "closed", 4),
            ], "next": 4}),
        ])),
        calls: Mutex::new(Vec::new()),
    });
    let source = CardSource {
        source: "vm",
        guest,
        forwarder: None,
        http_providers: Default::default(),
    };
    poll_once(&store, &source, 0, 0, None)
        .await
        .expect("test step");
    for (bridge, paste) in [(&waiting, Value::Null), (&pasting, json!({"at": 5.0}))] {
        let key = card_id(bridge);
        store
            .transaction_with(move |tx| {
                let raw = studio_runtime_store::QuestionsWriteRepository::request(tx, &key)?
                    .expect("test step");
                let mut record: Value = serde_json::from_str(&raw).expect("test step");
                record["params"]["phase"] = json!("waiting");
                if !paste.is_null() {
                    record["params"]["paste"] = paste;
                }
                studio_runtime_store::QuestionsWriteRepository::put_request(
                    tx,
                    &key,
                    &studio_runtime_store::python_json_utf8(&record),
                )
            })
            .await
            .expect("test step");
    }
    poll_once(&store, &source, 2, 0, None)
        .await
        .expect("test step");
    let expired = request(&store, &waiting).expect("test step");
    assert_eq!(expired["status"], "expired");
    assert_eq!(expired["closeReason"], "stale");
    assert_eq!(expired["params"]["outcome"], "expired");
    let in_flight = request(&store, &pasting).expect("test step");
    assert_eq!(
        in_flight["status"], "pending",
        "a pasted result in flight writes its own outcome"
    );
    let connection = store.test_reader().expect("test step");
    let events: Vec<(String, String)> = connection
        .prepare("SELECT id,text FROM runtime_events")
        .expect("test step")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("test step")
        .collect::<Result<_, _>>()
        .expect("test step");
    assert_eq!(
        events,
        vec![(
            format!("browser-outcome:{waiting}"),
            "The sign-in at id.example.com ended without a result: the request expired or the command stopped.".to_owned()
        )]
    );
}

fn events(store: &studio_runtime_store::RuntimeStore) -> Vec<(String, String, String)> {
    let connection = store.test_reader().expect("test step");
    connection
        .prepare("SELECT id,kind,text FROM runtime_events ORDER BY id")
        .expect("test step")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("test step")
        .collect::<Result<_, _>>()
        .expect("test step")
}

#[tokio::test]
async fn a_card_follows_the_request_deadline_and_tells_the_chat_when_nobody_answered() {
    let (_dir, store) = store().await;
    let stopped = "f".repeat(32);
    let timed_out = "1".repeat(32);
    let replaced = "2".repeat(32);
    let redirect = json!({"host":"localhost","port":8000,"path":"/"});
    let pending = |id: &str, seq: u64| item(id, VM_AGENT, KUBE, redirect.clone(), "pending", seq);
    let mut moved = pending(&stopped, 4);
    moved["expiresAt"] = json!(1201.0);
    let ended =
        |id: &str, state: &str, seq: u64| item(id, VM_AGENT, KUBE, redirect.clone(), state, seq);
    let guest = Arc::new(WaitGuest {
        batches: Mutex::new(VecDeque::from([
            json!({"items":[pending(&stopped, 1), pending(&timed_out, 2), pending(&replaced, 3)], "next": 3}),
            // The command still listens: the bridge moved the deadline.
            json!({"items":[moved], "next": 4}),
            json!({"items":[
                ended(&stopped, "stale", 5), ended(&timed_out, "expired", 6),
                ended(&replaced, "replaced", 7),
            ], "next": 7}),
            // A repeated final state changes nothing.
            json!({"items":[ended(&stopped, "stale", 8)], "next": 8}),
        ])),
        calls: Mutex::new(Vec::new()),
    });
    let source = CardSource {
        source: "vm",
        guest,
        forwarder: None,
        http_providers: Default::default(),
    };
    poll_once(&store, &source, 0, 0, None)
        .await
        .expect("test step");
    assert_eq!(
        request(&store, &stopped).expect("card")["params"]["expiresAt"],
        601.0
    );
    poll_once(&store, &source, 3, 0, None)
        .await
        .expect("test step");
    assert_eq!(
        request(&store, &stopped).expect("card")["params"]["expiresAt"],
        1201.0,
        "the card shows the moved deadline"
    );
    poll_once(&store, &source, 4, 0, None)
        .await
        .expect("test step");
    poll_once(&store, &source, 7, 0, None)
        .await
        .expect("test step");
    for (bridge, reason) in [
        (&stopped, "stale"),
        (&timed_out, "expired"),
        (&replaced, "replaced"),
    ] {
        let card = request(&store, bridge).expect("card");
        assert_eq!(
            (card["status"].clone(), card["closeReason"].clone()),
            (json!("expired"), json!(reason))
        );
    }
    assert_eq!(
        events(&store),
        vec![
            (
                format!("browser-unanswered:{timed_out}"),
                "browser_unanswered".to_owned(),
                "Nobody answered the sign-in card for id.example.com in time, and Studio closed it. The command gets no sign-in result from this card. Do not wait for it: stop the command if it still waits, start the sign-in again when the person can answer, and tell the person in one short message.".to_owned()
            ),
            (
                format!("browser-unanswered:{stopped}"),
                "browser_unanswered".to_owned(),
                "Nobody answered the sign-in card for id.example.com, and it closed because the sign-in command stopped waiting. The command got no sign-in result from this card. Do not wait for it: start the sign-in again when the person can answer, and tell the person in one short message.".to_owned()
            ),
        ],
        "a replaced card gets no event: the chat gave a newer link"
    );
}

#[tokio::test]
async fn the_host_bridge_shows_a_card_for_a_vm_chat_command_that_runs_on_the_mac() {
    let (_dir, store) = store().await;
    let bridge = "3".repeat(32);
    let redirect = json!({"host":"localhost","port":8000,"path":"/"});
    let guest = Arc::new(WaitGuest {
        batches: Mutex::new(VecDeque::from([json!({"items":[
            item(&bridge, VM_AGENT, KUBE, redirect, "pending", 1),
        ], "next": 1})])),
        calls: Mutex::new(Vec::new()),
    });
    let source = CardSource {
        source: "host",
        guest: guest.clone(),
        forwarder: None,
        http_providers: Default::default(),
    };
    poll_once(&store, &source, 0, 0, None)
        .await
        .expect("test step");
    let card = request(&store, &bridge).expect("a host_exec command of a VM chat gets a card");
    assert_eq!(
        (card["status"].clone(), card["params"]["source"].clone()),
        (json!("pending"), json!("host"))
    );
    assert!(
        guest
            .calls
            .lock()
            .expect("test step")
            .iter()
            .all(|call| call.0 != "browser.close"),
        "the host bridge keeps the request"
    );
}

#[tokio::test]
async fn a_card_whose_page_this_mac_opened_closes_as_answered_when_its_command_stops() {
    let (_dir, store) = store().await;
    let bridge = "4".repeat(32);
    let redirect = json!({"host":"localhost","port":8000,"path":"/"});
    let mut finished = item(&bridge, VM_AGENT, KUBE, redirect.clone(), "closed", 2);
    finished["localOpen"] = json!(true);
    finished["closeReason"] = json!("local");
    let guest = Arc::new(WaitGuest {
        batches: Mutex::new(VecDeque::from([
            json!({"items":[item(&bridge, VM_AGENT, KUBE, redirect, "pending", 1)], "next": 1}),
            json!({"items":[finished], "next": 2}),
        ])),
        calls: Mutex::new(Vec::new()),
    });
    let source = CardSource {
        source: "host",
        guest,
        forwarder: None,
        http_providers: Default::default(),
    };
    poll_once(&store, &source, 0, 0, None)
        .await
        .expect("test step");
    poll_once(&store, &source, 1, 0, None)
        .await
        .expect("test step");
    let card = request(&store, &bridge).expect("card");
    assert_eq!(
        (
            card["status"].clone(),
            card["answeredBy"].clone(),
            card["closeReason"].clone()
        ),
        (json!("answered"), json!("browser"), json!("local"))
    );
    assert!(
        events(&store).is_empty(),
        "the browser of this Mac had the page, so the chat gets no unanswered event"
    );
}

/// A backend restart expires every pending request, but a sign-in request lives in its
/// bridge, which keeps it across the restart. The first poll after the restart finds the
/// open request and gives the person the card again, also in the waiting phase of a phone
/// sign-in. A card that a stopped chat expired stays expired.
#[tokio::test]
async fn a_card_comes_back_after_a_backend_restart_while_its_request_is_open() {
    let (_dir, store) = store().await;
    let open = "5".repeat(32);
    let waiting = "6".repeat(32);
    let ended = "7".repeat(32);
    let redirect = json!({"host":"localhost","port":8000,"path":"/"});
    let pending = |id: &str, seq: u64| item(id, VM_AGENT, KUBE, redirect.clone(), "pending", seq);
    let mut approved = item(&waiting, VM_AGENT, KUBE, redirect.clone(), "approved", 5);
    approved["lease"] = json!({"host":"localhost","port":8000,"path":"/","expiresAt": 900.0});
    let guest = Arc::new(WaitGuest {
        batches: Mutex::new(VecDeque::from([
            json!({"items":[pending(&open, 1), pending(&waiting, 2), pending(&ended, 3)], "next": 3}),
            // The first poll after the restart reads the whole queue again.
            json!({"items":[
                pending(&open, 1), approved,
                item(&ended, VM_AGENT, KUBE, redirect.clone(), "stale", 6),
            ], "next": 6}),
        ])),
        calls: Mutex::new(Vec::new()),
    });
    let source = CardSource {
        source: "vm",
        guest,
        forwarder: None,
        http_providers: Default::default(),
    };
    poll_once(&store, &source, 0, 0, None)
        .await
        .expect("test step");
    // The person opened the second card on a phone: the waiting phase of a paste.
    let key = card_id(&waiting);
    store
        .transaction_with(move |tx| {
            let raw = studio_runtime_store::QuestionsWriteRepository::request(tx, &key)?
                .expect("test step");
            let mut record: Value = serde_json::from_str(&raw).expect("test step");
            record["params"]["phase"] = json!("waiting");
            record["params"]["waiting"] = json!({"expiresAt": 900.0});
            studio_runtime_store::QuestionsWriteRepository::put_request(
                tx,
                &key,
                &studio_runtime_store::python_json_utf8(&record),
            )
        })
        .await
        .expect("test step");
    crate::monitor_runtime::recover_on_startup(&store, 50.0)
        .await
        .expect("startup recovery");
    for bridge in [&open, &waiting, &ended] {
        assert_eq!(request(&store, bridge).expect("card")["status"], "expired");
    }
    poll_once(&store, &source, 0, 0, None)
        .await
        .expect("test step");
    let back = request(&store, &open).expect("card");
    assert_eq!(
        (back["status"].clone(), back.get("closeReason").cloned()),
        (json!("pending"), None),
        "the open request gets its card back"
    );
    let phone = request(&store, &waiting).expect("card");
    assert_eq!(
        (phone["status"].clone(), phone["params"]["phase"].clone()),
        (json!("pending"), json!("waiting")),
        "a phone sign-in keeps waiting for its pasted result"
    );
    assert_eq!(
        request(&store, &ended).expect("card")["status"],
        "expired",
        "an ended request keeps its card closed"
    );
    assert_eq!(
        events(&store)
            .into_iter()
            .map(|(id, kind, _)| (id, kind))
            .collect::<Vec<_>>(),
        vec![(
            format!("browser-unanswered:{ended}"),
            "browser_unanswered".to_owned()
        )],
        "the chat learns that nobody answered the ended request"
    );
}
