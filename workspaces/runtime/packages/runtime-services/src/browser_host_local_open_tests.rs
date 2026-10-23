//! The host fast path (docs/vm-browser-bridge.md, "Native chats"): on a Mac host the
//! launcher opens the link in the browser of this Mac at once and Studio also shows the card,
//! so the person can answer from another window or a phone. When the command stops listening
//! after the Mac opened its page, the card closes as answered by that browser.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

const SESSION: &str = "6f1c2b3a-4d5e-4f60-8a9b-0c1d2e3f4a5b";
const PORT: u16 = 4243;
const LINK: &str = "https://id.example.com/authorize?client_id=cli&response_type=code&redirect_uri=http%3A%2F%2F127.0.0.1%3A4243%2Fcallback&state=s1";

struct OneChat;

impl ChatLookup for OneChat {
    fn chat<'a>(
        &'a self,
        session: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Option<ChatContext>> {
        let chat = (session == SESSION).then(|| ChatContext {
            uid: current_uid(),
            project_id: None,
            owner: "person".into(),
            agent_id: SESSION.into(),
        });
        Box::pin(std::future::ready(chat))
    }
}

struct Host {
    _state: tempfile::TempDir,
    service: HostBridgeService,
    listening: Arc<AtomicBool>,
}

fn host(local_open: bool) -> Host {
    let state = tempfile::tempdir().expect("state folder");
    let listening = Arc::new(AtomicBool::new(true));
    let listener = Arc::clone(&listening);
    let service = HostBridgeService::start(
        state.path(),
        Arc::new(OneChat),
        HostBridgeOptions {
            listeners: Arc::new(move || {
                if listener.load(Ordering::SeqCst) {
                    vec![Listener {
                        address: "127.0.0.1".into(),
                        port: PORT,
                        uid: current_uid(),
                    }]
                } else {
                    Vec::new()
                }
            }),
            local_open,
            ..HostBridgeOptions::default()
        },
        None,
    )
    .expect("host bridge");
    Host {
        _state: state,
        service,
        listening,
    }
}

/// Run the launcher exchange with a fake system `open` that records its links.
fn launch(host: &Host, link: &str, opens: bool) -> (i32, String, Vec<String>) {
    let opened = Mutex::new(Vec::new());
    let socket = host.service.open_socket.to_string_lossy().into_owned();
    let (code, message) = tokio::task::block_in_place(|| {
        shim(&[link.to_owned()], &socket, Some(SESSION), &|url: &str| {
            opened.lock().expect("test step").push(url.to_owned());
            opens
        })
    });
    let opened = opened.into_inner().expect("test step");
    (code, message, opened)
}

fn only_item(host: &Host) -> Value {
    let queue = host.service.bridge.lock();
    let mut items = queue.items.values();
    let item = items.next().expect("one request").clone();
    assert!(items.next().is_none(), "one request");
    item
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mac_host_opens_the_link_at_once_and_still_shows_a_card() {
    let host = host(true);
    let (code, message, opened) = launch(&host, LINK, true);
    assert_eq!(code, 0, "{message}");
    assert_eq!(
        opened,
        vec![LINK.to_owned()],
        "the browser of this Mac opens at once"
    );
    assert_eq!(
        message,
        "studio-browser: Opened the page in the browser of this Mac. Studio also shows the person a sign-in card for id.example.com, so the person can answer from another window or a phone."
    );
    let item = only_item(&host);
    assert_eq!(
        (item["state"].clone(), item["localOpen"].clone()),
        (json!("pending"), json!(true))
    );
    host.service.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_link_that_studio_refuses_still_opens_in_the_browser_of_this_mac() {
    let host = host(true);
    let (code, message, opened) = launch(&host, "ftp://files.example.com/", true);
    assert_eq!(code, 0, "{message}");
    assert_eq!(opened, vec!["ftp://files.example.com/".to_owned()]);
    assert_eq!(
        message,
        "studio-browser: Opened the page in the browser of this Mac. Studio shows no sign-in card: Only https sign-in links and loopback sign-in pages can open"
    );
    let (code, message, _) = launch(&host, "ftp://files.example.com/", false);
    assert_eq!(
        (code, message.as_str()),
        (
            1,
            "studio-browser: Only https sign-in links and loopback sign-in pages can open"
        ),
        "a failed system open keeps the refusal"
    );
    host.service.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_mac_fast_path_the_launcher_only_queues_the_card() {
    let host = host(false);
    let (code, message, opened) = launch(&host, LINK, true);
    assert_eq!(code, 0, "{message}");
    assert!(opened.is_empty(), "{opened:?}");
    assert_eq!(
        message,
        "studio-browser: Studio asks the person to open id.example.com. The sign-in continues in the browser on the Mac after the approval."
    );
    assert_eq!(only_item(&host).get("localOpen"), None);
    host.service.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_request_of_a_page_that_this_mac_opened_closes_when_its_command_stops_listening() {
    let host = host(true);
    launch(&host, LINK, true);
    host.service.bridge.check().await;
    assert_eq!(only_item(&host)["state"], "pending");
    host.listening.store(false, Ordering::SeqCst);
    host.service.bridge.check().await;
    let item = only_item(&host);
    assert_eq!(
        (item["state"].clone(), item["closeReason"].clone()),
        (json!("closed"), json!(LOCAL_CLOSE_REASON))
    );
    host.service.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_production_options_open_links_on_a_mac_only() {
    assert_eq!(
        HostBridgeOptions::for_this_host().local_open,
        cfg!(target_os = "macos")
    );
    assert!(!HostBridgeOptions::default().local_open);
}
