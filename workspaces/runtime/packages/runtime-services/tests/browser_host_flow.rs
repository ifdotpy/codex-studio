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
use studio_runtime_services::sign_in;

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
    let connection = store.test_reader().expect("test step");
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

/// On macOS `bin/open` sends exactly one http or https URL to the launcher and gives every
/// other use to `/usr/bin/open` with the same arguments. The passthrough cases here make the
/// system `open` fail before it starts an application.
#[cfg(target_os = "macos")]
#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "the shim and /usr/bin/open exit at once on these arguments"
)]
fn the_open_shim_sends_only_one_web_link_to_the_launcher() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("test step");
    let record = dir.path().join("launched.txt");
    let fake = dir.path().join("fake-server");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
            record.to_string_lossy()
        ),
    )
    .expect("test step");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("test step");
    browser_host::write_launcher(dir.path(), &fake).expect("test step");
    let shim = dir.path().join("bin").join(browser_host::OPEN_NAME);
    assert_eq!(
        std::fs::read_to_string(&shim).expect("test step"),
        browser_host::OPEN_SHIM
    );
    for link in [
        "https://id.example.com/device?user_code=AB-CD",
        "HTTP://127.0.0.1:8000/",
    ] {
        let status = std::process::Command::new(&shim)
            .arg(link)
            .status()
            .expect("test step");
        assert!(status.success(), "{link}");
        assert_eq!(
            std::fs::read_to_string(&record).expect("test step"),
            format!("--studio-browser\n{link}\n")
        );
        std::fs::remove_file(&record).expect("test step");
    }
    let missing = dir.path().join("missing file.txt");
    let cases: [Vec<std::ffi::OsString>; 3] = [
        vec![],
        vec![missing.clone().into()],
        vec!["-R".into(), missing.into()],
    ];
    for arguments in cases {
        let shimmed = std::process::Command::new(&shim)
            .args(&arguments)
            .output()
            .expect("test step");
        let system = std::process::Command::new("/usr/bin/open")
            .args(&arguments)
            .output()
            .expect("test step");
        assert_eq!(shimmed.status.code(), system.status.code(), "{arguments:?}");
        assert_eq!(shimmed.stdout, system.stdout, "{arguments:?}");
        assert_eq!(shimmed.stderr, system.stderr, "{arguments:?}");
        assert!(!shimmed.status.success(), "{arguments:?}");
    }
    assert!(!record.exists());
}

fn cards(store: &studio_runtime_store::RuntimeStore) -> Vec<Value> {
    let connection = store.test_reader().expect("test step");
    let mut statement = connection
        .prepare("SELECT record FROM runtime_requests ORDER BY id")
        .expect("test step");
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("test step")
        .map(|raw| serde_json::from_str(&raw.expect("test step")).expect("test step"))
        .collect()
}

async fn agent_message(
    store: &studio_runtime_store::RuntimeStore,
    agent: &str,
    item: &str,
    text: &str,
    streaming: bool,
) -> sign_in::MessageLinks {
    let key = format!("{agent}:{item}");
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("test step")
        .as_secs_f64();
    let record = json!({"id": key, "role": "assistant", "title": "Assistant", "text": text,
                        "truncated": false, "at": at, "streaming": streaming})
    .to_string();
    let (row, owner) = (key.clone(), agent.to_owned());
    store
        .transaction_with(move |tx| {
            tx.execute(
                "INSERT OR REPLACE INTO runtime_items(id,agent,record,created) VALUES(?1,?2,?3,?4)",
                rusqlite::params![row, owner, record, at],
            )
            .map_err(studio_runtime_store::StoreError::from)?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("test step");
    sign_in::MessageLinks {
        agent_id: agent.to_owned(),
        item_key: key,
        links: studio_runtime_services::browser_policy::sign_in_links(text),
    }
}

/// Section 7: a link that the agent gives through `orchestration_sign_in` or in its message
/// reaches the same bridge and card as a link that its command opens.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_link_from_the_tool_and_from_the_message_gets_one_card() {
    let agent = "45c48cce-2e2d-4fbd-8a3c-1d2c3b4a5f62";
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
    let guest: Arc<dyn GuestRpc> =
        Arc::new(UnixSocketGuestRpc::new(service.control_socket.clone()));
    let sources = sign_in::SignInSources::new(vec![CardSource {
        source: "host",
        guest,
        forwarder: None,
        http_providers: BTreeSet::new(),
    }]);
    let record = json!({"id": agent, "executionMode": "host"});
    let device = "https://device.sso.us-east-1.amazonaws.com/?user_code=WDJB-MJHT";
    let first = sign_in::tool(&store, &sources, &record, Some(device), "call-1")
        .await
        .expect("test step");
    let [pending]: [Value; 1] = cards(&store).try_into().expect("one card");
    assert_eq!(first["status"], "pending");
    assert_eq!(first["requestId"], pending["id"]);
    assert!(
        first["message"]
            .as_str()
            .expect("test step")
            .contains("sign-in card")
    );
    assert_eq!(pending["params"]["userCode"], "WDJB-MJHT");
    assert_eq!(pending["params"]["source"], "host");
    // A retry and the same link in the agent's message keep the one card.
    let again = sign_in::tool(&store, &sources, &record, Some(device), "call-2")
        .await
        .expect("test step");
    assert_eq!(again["requestId"], pending["id"]);
    let text = format!("Open **{device}** now.");
    let note = agent_message(&store, agent, "item-1", &text, false).await;
    assert_eq!(note.links, vec![device.to_owned()]);
    sign_in::handle_message(&store, &sources, &note, &[0]).await;
    assert_eq!(cards(&store).len(), 1);
    // A new link in a message gets its own card, but only once the message is complete.
    let other = "https://device.sso.us-east-1.amazonaws.com/?user_code=QQQQ-RRRR";
    let text = format!("Then confirm the code at {other}.");
    let draft = agent_message(&store, agent, "item-2", &text, true).await;
    sign_in::handle_message(&store, &sources, &draft, &[0]).await;
    assert_eq!(cards(&store).len(), 1);
    let done = agent_message(&store, agent, "item-2", &text, false).await;
    sign_in::handle_message(&store, &sources, &done, &[0]).await;
    let all = cards(&store);
    assert_eq!(all.len(), 2);
    assert!(all.iter().any(|card| card["params"]["url"] == other));
    // A refusal reaches the model as text, and no card is written.
    let scheme = sign_in::tool(
        &store,
        &sources,
        &record,
        Some("ftp://files.acme.dev/x"),
        "call-3",
    )
    .await
    .expect_err("refused");
    assert!(scheme.contains("Only https sign-in links"), "{scheme}");
    let vm = json!({"id": agent, "executionMode": "vm"});
    let missing = sign_in::tool(&store, &sources, &vm, Some(device), "call-4")
        .await
        .expect_err("refused");
    assert!(missing.contains("VM chats"), "{missing}");
    assert_eq!(cards(&store).len(), 2);
    service.stop().await;
}

#[cfg(not(target_os = "macos"))]
#[test]
fn only_a_macos_host_gets_the_open_shim() {
    let dir = tempfile::tempdir().expect("test step");
    browser_host::write_launcher(dir.path(), std::path::Path::new("/opt/server"))
        .expect("test step");
    assert!(
        !dir.path()
            .join("bin")
            .join(browser_host::OPEN_NAME)
            .exists()
    );
}

/// A push transport that keeps the endpoint of every push and answers 201.
struct KeptPushes(Mutex<Vec<String>>);

impl studio_runtime_services::web_push::PushTransport for KeptPushes {
    fn post(
        &self,
        endpoint: &str,
        _headers: &[(String, String)],
        _body: &[u8],
    ) -> Result<u16, String> {
        self.0.lock().expect("test step").push(endpoint.to_owned());
        Ok(201)
    }
}

/// A new host card reaches the clients (the request sync entity, for the card and the
/// desktop alerts) and the phone (the server-side push reads the stored card, so it does not
/// wait for a client).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_host_card_is_published_to_clients_and_pushed_to_the_phone() {
    use base64::Engine as _;
    use studio_runtime_services::web_push;
    let agent = "2c9a4e1b-7d3f-4b6a-9e8c-5f1d2a3b4c5d";
    let (dir, store) = seeded_store(agent).await;
    let named = json!({"id":agent,"rootId":agent,"epoch":1,"status":"running","autoWake":true,
                       "threadId":"thread-1","executionMode":"host","name":"Infra"})
    .to_string();
    let id = agent.to_owned();
    store
        .transaction_with(move |tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=?2 WHERE id=?1",
                [&id, &named],
            )
            .map_err(studio_runtime_store::StoreError::from)?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("test step");
    let identity = provider();
    let tool = kubelogin(identity.port, "S-push");
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
    let guest: Arc<dyn GuestRpc> =
        Arc::new(UnixSocketGuestRpc::new(service.control_socket.clone()));
    let source = CardSource {
        source: "host",
        guest,
        forwarder: None,
        http_providers: providers,
    };
    browser_cards::poll_once(&store, &source, 0, 0, None)
        .await
        .expect("test step");
    let pending = card(&store);
    assert_eq!(pending["status"], "pending");
    let card_id = pending["id"].as_str().expect("test step").to_owned();

    // Pushed: one alert after the grace, from the stored card alone, before any client
    // reads it.
    let cards = web_push::pending_cards(&store).await.expect("test step");
    assert_eq!(cards.len(), 1);
    assert_eq!(
        (cards[0].id.as_str(), cards[0].chat.as_str()),
        (card_id.as_str(), "Infra")
    );
    let phone = ring::agreement::EphemeralPrivateKey::generate(
        &ring::agreement::ECDH_P256,
        &ring::rand::SystemRandom::new(),
    )
    .expect("test step");
    let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    web_push::Subscriptions::new(dir.path())
        .add(
            web_push::NewSubscription {
                endpoint: "https://web.push.apple.com/phone".into(),
                p256dh: encode(phone.compute_public_key().expect("test step").as_ref()),
                auth: encode(&[7; 16]),
                device: "iPhone".into(),
            },
            false,
            1.0,
        )
        .expect("test step");
    let sender = web_push::Sender {
        state: dir.path().to_path_buf(),
        key: web_push::ServerKey::load_or_create(dir.path()).expect("test step"),
        subject: "https://studio.example.org".into(),
    };
    let pushes = KeptPushes(Mutex::new(Vec::new()));
    let created = pending["createdAt"].as_f64().expect("test step");
    let grace = studio_runtime_schema::shared_values::SIGN_IN_ALERT_GRACE_SECONDS as f64;
    sender
        .deliver(&pushes, &cards, created + grace)
        .expect("test step");
    assert_eq!(
        *pushes.0.lock().expect("test step"),
        ["https://web.push.apple.com/phone"]
    );

    // Published: the clients read requests as sync entities.
    let connection = store.test_reader().expect("test step");
    let entities =
        studio_runtime_store::SyncDocumentsRepository::entities_after(&connection, 0, 100)
            .expect("test step");
    assert!(
        entities
            .iter()
            .any(|row| row.collection == "request" && row.id == card_id && !row.deleted),
        "the new card is published as a request entity"
    );
}
