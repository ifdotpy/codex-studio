//! Real native IPC, authenticated stream input, and exact preference HTTP receipts.
use super::{ConnectionConfig, serve};
use crate::{
    protocol::{self, Request, Response},
    store::Store,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

const WORKSPACE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SERVER_KEY: &str = "codex-project-tree:/state";
const USER_KEY: &str = "studio-logical-project-collapsed";
const HANG: Duration = Duration::from_secs(5);

fn request(id: &str, method: &str, params: Value) -> Request {
    Request {
        id: id.into(),
        method: method.into(),
        params,
    }
}
fn success(response: Response) -> Value {
    assert!(
        response.error.is_none(),
        "native preference command failed: {:?}",
        response.error
    );
    response.result.expect("native command result")
}
fn revision(value: &Value) -> u64 {
    value["revision"]
        .as_str()
        .expect("canonical native revision")
        .parse()
        .expect("native revision range")
}

struct Core {
    socket: Option<UnixStream>,
    worker: Option<thread::JoinHandle<()>>,
    closed: std::sync::mpsc::Receiver<Result<(), String>>,
    events: VecDeque<Value>,
}
impl Core {
    fn open(path: &std::path::Path, origin: &str) -> Self {
        let mut store =
            Store::open(&path.join("studio-client.sqlite3")).expect("private native store");
        success(store.dispatch(request(
            "fixture-identity",
            "identity.set",
            json!({"workspaceId":WORKSPACE}),
        )));
        drop(store);
        let (socket, core) = UnixStream::pair().expect("private native socket pair");
        socket
            .set_read_timeout(Some(HANG))
            .expect("bound fixture read");
        let (done, closed) = std::sync::mpsc::channel();
        let config = ConnectionConfig {
            state_directory: path.into(),
            backend_origin: origin.into(),
        };
        let worker = thread::spawn(move || {
            let _ = done.send(serve(core, config));
        });
        Self {
            socket: Some(socket),
            worker: Some(worker),
            closed,
            events: VecDeque::new(),
        }
    }
    fn frame(&mut self) -> Value {
        let socket = self.socket.as_mut().expect("open native socket");
        let mut assembler = protocol::Assembler::default();
        loop {
            let mut header = [0; 4];
            socket.read_exact(&mut header).expect("native frame header");
            let mut frame = vec![0; u32::from_le_bytes(header) as usize];
            socket.read_exact(&mut frame).expect("native frame body");
            if let Some(bytes) = assembler.push(&frame).expect("native framing") {
                return serde_json::from_slice(&bytes).expect("native JSON frame");
            }
        }
    }
    fn call(&mut self, id: &str, method: &str, params: Value) -> Value {
        let encoded =
            serde_json::to_vec(&request(id, method, params)).expect("encode native command");
        let socket = self.socket.as_mut().expect("open native socket");
        for (flag, bytes) in protocol::fragments(&encoded).expect("native request fragments") {
            socket
                .write_all(&((bytes.len() + 1) as u32).to_le_bytes())
                .expect("write frame header");
            socket.write_all(&[flag]).expect("write frame flag");
            socket.write_all(bytes).expect("write frame body");
        }
        loop {
            let frame = self.frame();
            if frame.get("event").is_some() {
                self.events.push_back(frame);
                continue;
            }
            let reply: Response = serde_json::from_value(frame).expect("native response");
            assert_eq!(reply.id, id);
            return success(reply);
        }
    }
    fn event(&mut self, subscription: &str, matches: impl Fn(&Value) -> bool) -> Value {
        loop {
            if let Some(index) = self
                .events
                .iter()
                .position(|event| event["event"] == subscription && matches(&event["value"]))
            {
                return self.events.remove(index).expect("queued native event")["value"].clone();
            }
            let frame = self.frame();
            assert!(
                frame.get("event").is_some(),
                "unexpected command response: {frame}"
            );
            if frame["event"] == subscription {
                assert!(frame.get("error").is_none(), "native view error: {frame}");
                if matches(&frame["value"]) {
                    return frame["value"].clone();
                }
            } else {
                self.events.push_back(frame);
            }
        }
    }
    fn edit(&mut self, id: &str, key: &str, previous: Value, next: Value) -> Value {
        self.call(
            id,
            "preferences.edit",
            json!({"key":key,"previous":previous,"next":next}),
        )
    }
}
impl Drop for Core {
    fn drop(&mut self) {
        self.socket.take();
        let done = self.closed.recv_timeout(HANG);
        if let Ok(result) = done {
            if let Some(worker) = self.worker.take() {
                worker.join().expect("native owner thread exits");
            }
            if !thread::panicking() {
                result.expect("native owner closes without work loss");
            }
        } else if !thread::panicking() {
            panic!("the exact native owner did not close: {done:?}");
        }
    }
}

#[derive(Default)]
struct Ledger {
    receipts: BTreeMap<String, (Value, Value)>,
    fields: BTreeMap<String, Value>,
    posts: usize,
}
struct Posted {
    identity: String,
    body: Value,
    accepted: Value,
    reply: oneshot::Sender<Option<Value>>,
}
impl Posted {
    fn acknowledge(self) {
        let _ = self.reply.send(Some(self.accepted));
    }
    fn lose_reply(self) {
        let _ = self.reply.send(None);
    }
}
struct Stream {
    generation: u64,
    close: oneshot::Sender<()>,
}
struct Server {
    origin: String,
    posted: mpsc::UnboundedReceiver<Posted>,
    streams: mpsc::UnboundedReceiver<Stream>,
    ledger: Arc<Mutex<Ledger>>,
    task: JoinHandle<()>,
}
impl Server {
    async fn open() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("private HTTP fixture");
        let origin = format!("http://{}", listener.local_addr().expect("fixture address"));
        let ledger = Arc::new(Mutex::new(Ledger::default()));
        let saved = ledger.clone();
        let generations = Arc::new(AtomicU64::new(0));
        let (posts, posted) = mpsc::unbounded_channel();
        let (opened, streams) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut children = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted=listener.accept()=>{
                        let (socket,_) = accepted.expect("accept fixture HTTP");
                        children.spawn(serve_http(socket,saved.clone(),generations.clone(),posts.clone(),opened.clone()));
                    },
                    result=children.join_next(), if !children.is_empty()=>{ result.expect("fixture child result").expect("fixture HTTP task"); },
                }
            }
        });
        Self {
            origin,
            posted,
            streams,
            ledger,
            task,
        }
    }
    async fn post(&mut self) -> Posted {
        tokio::time::timeout(HANG, self.posted.recv())
            .await
            .expect("native preference POST reaches server")
            .expect("fixture POST observer")
    }
    async fn stream(&mut self) -> Stream {
        tokio::time::timeout(HANG, self.streams.recv())
            .await
            .expect("native SSE reaches server")
            .expect("fixture SSE observer")
    }
    fn effects(&self) -> usize {
        self.ledger.lock().expect("fixture ledger").receipts.len()
    }
    fn posts(&self) -> usize {
        self.ledger.lock().expect("fixture ledger").posts
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn source_frame(ledger: &Ledger, generation: u64) -> Value {
    let seq = generation * 2;
    let documents = ["user", "server"]
        .into_iter()
        .enumerate()
        .map(|(index, scope)| {
            json!({
                "collection":"uiPreferences","id":scope,"op":"upsert","seq":seq-1+index as u64,
                "value":{"fields":ledger.fields.get(scope).cloned().unwrap_or_else(||json!({}))}
            })
        })
        .collect::<Vec<_>>();
    json!({"protocol":4,"workspaceId":WORKSPACE,"epoch":"preference-source","revision":seq,"reason":"initial",
        "resources":[{"kind":"state"}],"resourceVersions":[{"resource":{"kind":"state"},"revision":seq,
        "cursor":{"workspaceId":WORKSPACE,"seq":seq},"replyKind":"snapshot",
        "entityChanges":{"after":0,"through":seq,"documents":documents}}]})
}

async fn serve_http(
    mut socket: TcpStream,
    ledger: Arc<Mutex<Ledger>>,
    generations: Arc<AtomicU64>,
    posted: mpsc::UnboundedSender<Posted>,
    streams: mpsc::UnboundedSender<Stream>,
) {
    let (headers, body) = crate::backend::tests::read_http(&mut socket).await;
    if headers.starts_with("GET /api/session ") {
        crate::backend::tests::reply(&mut socket, "{\"token\":\"fixture-token\"}").await;
    } else if headers.starts_with("GET /api/sync/identity ") {
        crate::backend::tests::reply(&mut socket, &json!({"workspaceId":WORKSPACE}).to_string())
            .await;
    } else if headers.starts_with("GET /api/sync/stream?") {
        let generation = generations.fetch_add(1, Ordering::SeqCst) + 1;
        let frame = source_frame(&ledger.lock().expect("fixture source fields"), generation);
        let body = format!(
            "event: session\ndata: {{\"sessionId\":\"00000000-0000-4000-8000-{generation:012}\"}}\n\nevent: api-schema\ndata: {{\"hash\":\"{}\"}}\n\nevent: resources\ndata: {frame}\n\n",
            crate::subscriptions::schema_hash()
        );
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n",body.len()).as_bytes()).await.expect("send authenticated preference stream");
        let (close, closed) = oneshot::channel();
        if streams.send(Stream { generation, close }).is_err() {
            return;
        }
        let _ = closed.await;
        let _ = socket.write_all(b"0\r\n\r\n").await;
    } else {
        assert!(
            headers.starts_with("POST /api/sync/preferences "),
            "unexpected fixture path: {headers}"
        );
        assert!(matches!(body["scope"].as_str(), Some("user" | "server")));
        let identity = headers
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("x-studio-request-id"))
                    .map(|(_, value)| value.trim().to_owned())
            })
            .expect("an exact HTTP request identity");
        let accepted = {
            let mut saved = ledger.lock().expect("fixture exact receipt ledger");
            saved.posts += 1;
            if let Some((previous, reply)) = saved.receipts.get(&identity) {
                assert_eq!(previous, &body, "a saved identity cannot change payload");
                reply.clone()
            } else {
                let reply = json!({"fields":body["fields"]});
                saved.fields.insert(
                    body["scope"].as_str().expect("scope").into(),
                    body["fields"].clone(),
                );
                saved
                    .receipts
                    .insert(identity.clone(), (body.clone(), reply.clone()));
                reply
            }
        };
        let (reply, result) = oneshot::channel();
        if posted
            .send(Posted {
                identity: identity.clone(),
                body: body.clone(),
                accepted,
                reply,
            })
            .is_err()
        {
            return;
        }
        let Ok(Some(response)) = result.await else {
            return;
        };
        {
            let mut saved = ledger.lock().expect("fixture receipt settlement");
            saved.receipts.get_mut(&identity).expect("saved effect").1 = response.clone();
            saved.fields.insert(
                body["scope"].as_str().expect("scope").into(),
                response["fields"].clone(),
            );
        }
        crate::backend::tests::reply(&mut socket, &response.to_string()).await;
    }
}

async fn start(core: &mut Core, server: &mut Server) -> Stream {
    core.call("stream","view.subscribe",json!({"subscription":"stream","topic":"subscriptions","workspace":WORKSPACE,"resources":[{"kind":"state"}]}));
    core.event("stream", |value| {
        value["type"] == "resource" && value["snapshot"]["status"] == "ready"
    });
    core.event("stream", |value| {
        value["type"] == "connection" && value["status"] == "live"
    });
    let stream = server.stream().await;
    core.call(
        "preferences",
        "view.subscribe",
        json!({"subscription":"preferences","topic":"preferences","workspace":WORKSPACE}),
    );
    core.event("preferences", |_| true);
    stream
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_preference_replication_sends_both_owned_scopes_and_waits_for_remote_ack() {
    let mut server = Server::open().await;
    let root = tempfile::tempdir().expect("private native preference fixture");
    let mut core = Core::open(root.path(), &server.origin);
    let _stream = start(&mut core, &mut server).await;
    for (scope, key) in [("server", SERVER_KEY), ("user", USER_KEY)] {
        let receipt = core.edit(
            &format!("edit-{scope}"),
            key,
            json!({}),
            json!({"item":true}),
        );
        assert_eq!(
            receipt["sync"][scope]["state"], "saving",
            "SQLite commit cannot confirm a remote write"
        );
        let post = server.post().await;
        assert_eq!(post.body["scope"], scope);
        let field = serde_json::to_string(&json!([key, "item"])).expect("field identity");
        assert_eq!(post.body["fields"][&field]["value"], true);
        post.acknowledge();
        core.event("preferences", |value| {
            value["sync"][scope]["state"] == "saved" && revision(value) > revision(&receipt)
        });
    }
    assert_eq!(server.effects(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_preference_replication_reconnect_replays_lost_success_with_exact_identity() {
    let mut server = Server::open().await;
    let root = tempfile::tempdir().expect("private native preference fixture");
    let mut core = Core::open(root.path(), &server.origin);
    let stream = start(&mut core, &mut server).await;
    let edited = core.edit("edit", SERVER_KEY, json!({}), json!({"item":true}));
    let first = server.post().await;
    let identity = first.identity.clone();
    let body = first.body.clone();
    first.lose_reply();
    let unknown = core.event("preferences", |value| {
        value["sync"]["server"]["state"] == "error" && revision(value) > revision(&edited)
    });
    assert_eq!(unknown["sync"]["server"]["uncertain"], true);
    assert_eq!(unknown["sync"]["server"]["requestId"], identity);
    let generation = stream.generation;
    let _ = stream.close.send(());
    let _next = server.stream().await;
    assert!(_next.generation > generation);
    let retry = server.post().await;
    assert_eq!(retry.identity, identity);
    assert_eq!(retry.body, body);
    let during = core.call("during-retry", "preferences.view", json!({}));
    assert_eq!(
        during["sync"]["server"]["uncertain"], true,
        "reconnect is not an acknowledgement"
    );
    retry.acknowledge();
    core.event("preferences", |value| {
        value["sync"]["server"]["state"] == "saved" && revision(value) > revision(&unknown)
    });
    assert_eq!(server.effects(), 1);
    assert_eq!(server.posts(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_preference_replication_stale_ack_cannot_replace_a_newer_local_edit() {
    let mut server = Server::open().await;
    let root = tempfile::tempdir().expect("private native preference fixture");
    let mut core = Core::open(root.path(), &server.origin);
    let _stream = start(&mut core, &mut server).await;
    core.edit("first", SERVER_KEY, json!({}), json!({"item":true}));
    let first = server.post().await;
    let edited = core.edit(
        "second",
        SERVER_KEY,
        json!({"item":true}),
        json!({"item":false}),
    );
    let field = serde_json::to_string(&json!([SERVER_KEY, "item"])).expect("field identity");
    let mut accepted = first.accepted.clone();
    accepted["fields"][&field]["timestamp"] = json!(0);
    let first_identity = first.identity.clone();
    let _ = first.reply.send(Some(accepted));
    let second = server.post().await;
    assert_ne!(second.identity, first_identity);
    assert_eq!(second.body["fields"][&field]["value"], false);
    let current = core.call("after-old-ack", "preferences.view", json!({}));
    assert_eq!(
        serde_json::from_str::<Value>(current["values"][SERVER_KEY].as_str().expect("native raw"))
            .expect("native JSON"),
        json!({"item":false})
    );
    second.acknowledge();
    core.event("preferences", |value| {
        value["sync"]["server"]["state"] == "saved" && revision(value) > revision(&edited)
    });
    assert_eq!(server.effects(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_preference_replication_adopts_acknowledged_winner_and_omission() {
    let mut server = Server::open().await;
    let root = tempfile::tempdir().expect("private acknowledgement fixture");
    let mut core = Core::open(root.path(), &server.origin);
    let field = serde_json::to_string(&json!([SERVER_KEY, "item"])).expect("field identity");
    server.ledger.lock().expect("fixture source").fields.insert(
        "server".into(),
        json!({(field.clone()):{"value":false,"timestamp":1,"writer":"old-source"}}),
    );
    let stream = start(&mut core, &mut server).await;
    let edited = core.edit("competing", SERVER_KEY, json!({}), json!({"item":true}));
    let first = server.post().await;
    let mut accepted = first.accepted.clone();
    accepted["fields"][&field]["value"] = json!(false);
    accepted["fields"][&field]["timestamp"] = json!(
        accepted["fields"][&field]["timestamp"]
            .as_u64()
            .expect("submitted clock")
            + 1
    );
    accepted["fields"][&field]["writer"] = json!("other-writer");
    let _ = first.reply.send(Some(accepted.clone()));
    let winner = core.event("preferences", |value| {
        value["sync"]["server"]["state"] == "saved" && revision(value) > revision(&edited)
    });
    assert_eq!(
        winner["fields"]["server"][&field],
        accepted["fields"][&field]
    );
    assert_eq!(
        serde_json::from_str::<Value>(winner["values"][SERVER_KEY].as_str().expect("native raw"))
            .expect("native JSON"),
        json!({"item":false})
    );
    let edited = core.edit(
        "omitted",
        SERVER_KEY,
        json!({"item":false}),
        json!({"item":true}),
    );
    let second = server.post().await;
    // The maintained response schema returns the complete merged map. An
    // omitted submitted field is authoritative, including server-side pruning.
    let _ = second.reply.send(Some(json!({"fields":{}})));
    let omitted = core.event("preferences", |value| {
        value["sync"]["server"]["state"] == "saved" && revision(value) > revision(&edited)
    });
    assert!(omitted["fields"]["server"].get(&field).is_none());
    assert_eq!(
        serde_json::from_str::<Value>(omitted["values"][SERVER_KEY].as_str().expect("native raw"))
            .expect("native JSON"),
        json!({})
    );
    assert_eq!(server.effects(), 2);
    assert_eq!(server.posts(), 2);
    assert!(
        core.call("omission-readback", "preferences.view", json!({}))["fields"]["server"]
            .get(&field)
            .is_none()
    );
    server.ledger.lock().expect("later source").fields.insert(
        "server".into(),
        json!({(field.clone()):{"value":true,"timestamp":1,"writer":"later-source"}}),
    );
    let _ = stream.close.send(());
    let _next = server.stream().await;
    let later = core.event("preferences", |value| {
        value["fields"]["server"][&field]["writer"] == "later-source"
            && revision(value) > revision(&omitted)
    });
    assert_eq!(later["sync"]["server"]["state"], "saved");
    assert_eq!(
        server.effects(),
        2,
        "a later source baseline is not local edit intent"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_preference_replication_restart_keeps_unknown_batch_out_of_sibling_store() {
    let mut server = Server::open().await;
    let root = tempfile::tempdir().expect("private native preference fixture");
    let owner = root.path().join("owner");
    std::fs::create_dir(&owner).expect("private owner directory");
    let mut first = Core::open(&owner, &server.origin);
    let stream = start(&mut first, &mut server).await;
    first.edit("edit", SERVER_KEY, json!({}), json!({"item":true}));
    let post = server.post().await;
    let identity = post.identity.clone();
    let body = post.body.clone();
    post.lose_reply();
    first.event("preferences", |value| {
        value["sync"]["server"]["uncertain"] == true
    });
    drop(first);
    drop(stream);
    let sibling = root.path().join("sibling");
    std::fs::create_dir(&sibling).expect("private sibling directory");
    let mut other = Core::open(&sibling, &server.origin);
    let sibling_stream = start(&mut other, &mut server).await;
    let sibling_view = other.call("sibling-state", "preferences.view", json!({}));
    assert_eq!(
        sibling_view["sync"]["server"]["state"], "saved",
        "a projected value does not import another context's uncertain attempt"
    );
    drop(other);
    drop(sibling_stream);
    let mut resumed = Core::open(&owner, &server.origin);
    let _stream = start(&mut resumed, &mut server).await;
    let retry = server.post().await;
    assert_eq!(retry.identity, identity);
    assert_eq!(retry.body, body);
    let before_ack = resumed.call("before-ack", "preferences.view", json!({}));
    assert_eq!(before_ack["sync"]["server"]["uncertain"], true);
    retry.acknowledge();
    resumed.event("preferences", |value| {
        value["sync"]["server"]["state"] == "saved" && revision(value) > revision(&before_ack)
    });
    assert_eq!(server.effects(), 1);
    assert_eq!(server.posts(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_preference_subscription_install_covers_the_initial_snapshot_gap() {
    let server = Server::open().await;
    let root = tempfile::tempdir().expect("private native preference fixture");
    let mut core = Core::open(root.path(), &server.origin);
    let initial = core.call("initial", "preferences.view", json!({}));
    assert_eq!(initial["revision"], "0");
    let committed = core.call(
        "between-read-and-subscribe",
        "preferences.raw_set",
        json!({"key":"device-choice","raw":"latest"}),
    );
    core.call(
        "preferences",
        "view.subscribe",
        json!({"subscription":"preferences","topic":"preferences","workspace":WORKSPACE}),
    );
    let installed = core.event("preferences", |_| true);
    assert_eq!(installed["revision"], committed["revision"]);
    assert_eq!(installed["values"]["device-choice"], "latest");
    assert_ne!(installed["revision"], initial["revision"]);
    assert_eq!(
        server.effects(),
        0,
        "local snapshot installation never fabricates remote intent"
    );
}
