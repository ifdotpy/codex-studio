//! The whole sign-in path of a native chat on a Rust server, as a phone uses it
//! (docs/vm-browser-bridge.md, section 6): the launcher, the host bridge sockets, the card
//! poll, paste mode, the pasted redirect and the one relayed GET. A fake OIDC provider and a
//! fake tool in the measured kubelogin and AWS shapes stand in for real ones; a test HTTP
//! client is the phone browser, which cannot reach the tool's loopback port.
#![cfg(unix)]

use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use studio_runtime_services::browser_answer::{self, BrowserAnswerContext};
use studio_runtime_services::browser_cards::{self, CardSource};
use studio_runtime_services::browser_host::{
    self, HostBridgeOptions, HostBridgeService, StoreChats,
};
use studio_runtime_services::linux_vm_guest::{GuestRpc, UnixSocketGuestRpc};

fn read_request(stream: &mut TcpStream) -> String {
    let mut data = Vec::new();
    let mut block = [0u8; 4096];
    while !data.windows(4).any(|window| window == b"\r\n\r\n") {
        let count = stream.read(&mut block).unwrap_or(0);
        if count == 0 {
            break;
        }
        data.extend_from_slice(&block[..count]);
    }
    String::from_utf8_lossy(&data).into_owned()
}

fn reply(stream: &mut TcpStream, status: &str, headers: &str, body: &str) {
    drop(write!(
        stream,
        "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
}

fn query_value(target: &str, name: &str) -> Option<String> {
    target.split_once('?')?.1.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| value.to_owned())
    })
}

/// The identity provider: `/authorize` sends the browser back with a code; `/token` counts.
struct Provider {
    port: u16,
    tokens: Arc<AtomicUsize>,
}

fn provider() -> Provider {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test step");
    let port = listener.local_addr().expect("test step").port();
    let tokens = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&tokens);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let request = read_request(&mut stream);
            let target = request.split(' ').nth(1).unwrap_or("/").to_owned();
            if target.starts_with("/authorize") {
                let redirect = query_value(&target, "redirect_uri")
                    .expect("test step")
                    .replace("%3A", ":")
                    .replace("%2F", "/");
                let state = query_value(&target, "state").expect("test step");
                let location = format!("{redirect}?code=fake-code&state={state}");
                reply(
                    &mut stream,
                    "302 Found",
                    &format!("Location: {location}\r\n"),
                    "",
                );
            } else if target.starts_with("/token") {
                counter.fetch_add(1, Ordering::SeqCst);
                reply(
                    &mut stream,
                    "200 OK",
                    "Content-Type: application/json\r\n",
                    "{\"id_token\":\"fake\"}",
                );
            } else {
                reply(&mut stream, "404 Not Found", "", "");
            }
        }
    });
    Provider { port, tokens }
}

/// A kubelogin-shaped tool: its own start page on 127.0.0.1 redirects to the provider, and its
/// callback takes one code and sends one token request.
struct Tool {
    port: u16,
    codes: Arc<Mutex<Vec<String>>>,
}

fn kubelogin(provider_port: u16, state: &'static str) -> Tool {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test step");
    let port = listener.local_addr().expect("test step").port();
    let codes = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&codes);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let request = read_request(&mut stream);
            let target = request.split(' ').nth(1).unwrap_or("/").to_owned();
            if let Some(code) = query_value(&target, "code") {
                assert_eq!(query_value(&target, "state").as_deref(), Some(state));
                assert!(!request.to_ascii_lowercase().contains("cookie:"));
                seen.lock().expect("test step").push(code);
                let mut token =
                    TcpStream::connect(("127.0.0.1", provider_port)).expect("test step");
                write!(
                    token,
                    "POST /token HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n"
                )
                .expect("test step");
                let mut answer = String::new();
                drop(token.read_to_string(&mut answer));
                reply(
                    &mut stream,
                    "200 OK",
                    "Content-Type: text/html\r\n",
                    "<p>Signed in</p><script>window.close()</script>",
                );
            } else {
                let location = format!(
                    "http://127.0.0.1:{provider_port}/authorize?client_id=kube&redirect_uri=http%3A%2F%2Flocalhost%3A{port}&response_type=code&scope=openid&state={state}&code_challenge_method=S256&code_challenge=c"
                );
                reply(
                    &mut stream,
                    "302 Found",
                    &format!("Location: {location}\r\n"),
                    "",
                );
            }
        }
    });
    Tool { port, codes }
}

/// The phone browser: it follows the provider page and stops at the loopback redirect,
/// because the phone has nothing on that port. It returns the address of that page.
fn phone_browser(url: &str) -> String {
    let rest = url.strip_prefix("http://").expect("test step");
    let (authority, path) = rest.split_at(rest.find('/').expect("test step"));
    let mut stream = TcpStream::connect(authority).expect("test step");
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
    )
    .expect("test step");
    let mut answer = String::new();
    stream.read_to_string(&mut answer).expect("test step");
    answer
        .lines()
        .find_map(|line| line.strip_prefix("Location: "))
        .expect("test step")
        .trim()
        .to_owned()
}

async fn seeded_store(agent: &str) -> (tempfile::TempDir, Arc<studio_runtime_store::RuntimeStore>) {
    let dir = tempfile::tempdir().expect("test step");
    let store = Arc::new(
        studio_runtime_store::RuntimeStore::open(Some(dir.path()))
            .await
            .expect("test step"),
    );
    let record = json!({"id":agent,"rootId":agent,"epoch":1,"status":"running","autoWake":true,
                        "threadId":"thread-1","executionMode":"host"})
    .to_string();
    let agent = agent.to_owned();
    store
        .transaction_with(move |tx| {
            tx.execute(
                "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                [&agent, &record],
            )
            .map_err(studio_runtime_store::StoreError::from)?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("test step");
    (dir, store)
}

fn card(store: &studio_runtime_store::RuntimeStore) -> Value {
    let connection = store.read().expect("test step");
    let raw: String = connection
        .query_row("SELECT record FROM runtime_requests", [], |row| row.get(0))
        .expect("test step");
    serde_json::from_str(&raw).expect("test step")
}

fn answer_context(
    guest: Arc<dyn GuestRpc>,
    providers: &BTreeSet<String>,
) -> BrowserAnswerContext<impl Fn(&Value) -> bool + Send + Sync + 'static> {
    BrowserAnswerContext {
        local_window: false,
        guest,
        local_forwarder: None,
        now: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("test step")
            .as_secs_f64(),
        validate: |_: &Value| true,
        http_providers: providers.clone(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_phone_signs_in_a_kubelogin_style_command_with_a_pasted_redirect() {
    let agent = "8f14e45f-ceea-467f-a0e6-1d2c3b4a5f60";
    let (dir, store) = seeded_store(agent).await;
    let identity = provider();
    let tool = kubelogin(identity.port, "S-kube");
    let providers = BTreeSet::from([format!("127.0.0.1:{}", identity.port)]);
    let service = HostBridgeService::start(
        dir.path(),
        Arc::new(StoreChats {
            store: Arc::clone(&store),
        }),
        HostBridgeOptions {
            http_providers: providers.clone(),
            ..Default::default()
        },
        None,
    )
    .expect("test step");
    let socket = service.open_socket.to_string_lossy().into_owned();
    let start = format!("http://localhost:{}/", tool.port);
    let (code, message) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/browser_host",
        move || browser_host::shim(&[start], &socket, Some(agent)),
    )
    .await
    .expect("test step");
    assert_eq!(code, 0, "{message}");
    assert!(message.contains("127.0.0.1"), "{message}");

    let guest: Arc<dyn GuestRpc> =
        Arc::new(UnixSocketGuestRpc::new(service.control_socket.clone()));
    let source = CardSource {
        source: "host",
        guest: Arc::clone(&guest),
        forwarder: None,
        http_providers: providers.clone(),
    };
    let next = browser_cards::poll_once(&store, &source, 0, 0, None)
        .await
        .expect("test step");
    assert!(next >= 1);
    let pending = card(&store);
    assert_eq!(pending["status"], "pending");
    assert_eq!(pending["agent"], agent);
    assert_eq!(pending["params"]["source"], "host");
    assert_eq!(pending["params"]["clientId"], "kube");
    assert_eq!(
        pending["params"]["redirect"],
        json!({"host":"localhost","port":tool.port,"path":"/"})
    );
    assert!(
        tool.codes.lock().expect("test step").is_empty(),
        "the first hop uses no code"
    );
    let id = pending["id"].as_str().expect("test step").to_owned();

    let opened = browser_answer::answer(
        &store,
        &id,
        &json!({"decision":"accept","paste":true}),
        answer_context(Arc::clone(&guest), &providers),
    )
    .await
    .expect("test step")
    .expect("test step");
    let open_url = opened.response["openUrl"]
        .as_str()
        .expect("test step")
        .to_owned();
    assert!(open_url.starts_with(&format!("http://127.0.0.1:{}/authorize", identity.port)));
    assert_eq!(card(&store)["params"]["phase"], "waiting");

    let failed_page = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/browser_host",
        move || phone_browser(&open_url),
    )
    .await
    .expect("test step");
    // The provider appends the result to the registered redirect, which has no path.
    assert_eq!(
        failed_page,
        format!("http://localhost:{}?code=fake-code&state=S-kube", tool.port)
    );

    let wrong = failed_page.replace("S-kube", "S-other");
    assert!(
        browser_answer::answer(
            &store,
            &id,
            &json!({"decision":"answer","result":wrong}),
            answer_context(Arc::clone(&guest), &providers)
        )
        .await
        .is_err()
    );
    assert!(
        tool.codes.lock().expect("test step").is_empty(),
        "a state mismatch relays nothing"
    );

    let pasted = json!({"decision":"answer","result":failed_page});
    let done = browser_answer::answer(
        &store,
        &id,
        &pasted,
        answer_context(Arc::clone(&guest), &providers),
    )
    .await
    .expect("test step")
    .expect("test step");
    assert_eq!(done.response, json!({"status":"answered"}));
    let again = browser_answer::answer(
        &store,
        &id,
        &pasted,
        answer_context(Arc::clone(&guest), &providers),
    )
    .await
    .expect("test step")
    .expect("test step");
    assert!(again.replayed);
    assert_eq!(
        *tool.codes.lock().expect("test step"),
        vec!["fake-code".to_owned()]
    );
    assert_eq!(
        identity.tokens.load(Ordering::SeqCst),
        1,
        "the provider gets one token request"
    );
    let finished = card(&store);
    assert_eq!(finished["status"], "answered");
    assert_eq!(finished["params"]["outcome"], "signed-in");
    assert_eq!(finished["params"]["outcomeHttp"], 200);

    // The guest closed its request after the code; the next poll keeps the answered card.
    browser_cards::poll_once(&store, &source, next, 0, None)
        .await
        .expect("test step");
    assert_eq!(card(&store)["status"], "answered");
    let queue: Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("browser-host/browser/requests.json")).expect("test step"),
    )
    .expect("test step");
    let item = queue["items"]
        .as_object()
        .expect("test step")
        .values()
        .next()
        .expect("test step")
        .clone();
    assert_eq!(item["state"], "closed");
    assert_eq!(item["closeReason"], "completed");
    assert_eq!(item["relays"], 1);
    service.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_aws_style_device_code_card_shows_the_code_and_needs_no_forward() {
    let agent = "1f0e3dad-9990-4345-8d5e-1c2c3b4a5f61";
    let (dir, store) = seeded_store(agent).await;
    let service = HostBridgeService::start(
        dir.path(),
        Arc::new(StoreChats {
            store: Arc::clone(&store),
        }),
        HostBridgeOptions::default(),
        None,
    )
    .expect("test step");
    let socket = service.open_socket.to_string_lossy().into_owned();
    let device = "https://device.sso.us-east-1.amazonaws.com/?user_code=WDJB-MJHT".to_owned();
    let link = device.clone();
    // The native session hint may be the chat's native thread ID.
    let (code, message) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/browser_host",
        move || browser_host::shim(&[link], &socket, Some("thread-1")),
    )
    .await
    .expect("test step");
    assert_eq!(code, 1, "a thread ID is not a session UUID: {message}");
    let socket = service.open_socket.to_string_lossy().into_owned();
    let link = device.clone();
    let (code, message) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/browser_host",
        move || browser_host::shim(&[link], &socket, Some(agent)),
    )
    .await
    .expect("test step");
    assert_eq!(code, 0, "{message}");
    let guest: Arc<dyn GuestRpc> =
        Arc::new(UnixSocketGuestRpc::new(service.control_socket.clone()));
    let source = CardSource {
        source: "host",
        guest: Arc::clone(&guest),
        forwarder: None,
        http_providers: BTreeSet::new(),
    };
    browser_cards::poll_once(&store, &source, 0, 0, None)
        .await
        .expect("test step");
    let pending = card(&store);
    assert_eq!(pending["params"]["userCode"], "WDJB-MJHT");
    assert_eq!(pending["params"]["redirect"], Value::Null);
    let id = pending["id"].as_str().expect("test step").to_owned();
    let opened = browser_answer::answer(
        &store,
        &id,
        &json!({"decision":"accept","paste":true}),
        answer_context(guest, &BTreeSet::new()),
    )
    .await
    .expect("test step")
    .expect("test step");
    assert_eq!(opened.response["openUrl"], device);
    assert_eq!(card(&store)["status"], "answered");
    service.stop().await;
}

#[test]
fn the_launcher_and_the_path_links_reach_the_runtime_binary() {
    let dir = tempfile::tempdir().expect("test step");
    let launcher =
        browser_host::write_launcher(dir.path(), std::path::Path::new("/opt/Studio App/server"))
            .expect("test step");
    let script = std::fs::read_to_string(&launcher).expect("test step");
    assert_eq!(
        script,
        "#!/bin/sh\nexec '/opt/Studio App/server' --studio-browser \"$@\"\n"
    );
    for name in ["xdg-open", "x-www-browser", "www-browser"] {
        assert_eq!(
            std::fs::read_link(dir.path().join("bin").join(name)).expect("test step"),
            std::path::Path::new("../studio-browser")
        );
    }
    let environment = browser_host::session_environment(dir.path());
    assert!(environment["BROWSER"].ends_with("browser-host/studio-browser"));
    assert!(environment["STUDIO_BROWSER_PATH"].ends_with("browser-host/bin"));
}
