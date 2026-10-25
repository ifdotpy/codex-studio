//! Sign-in links of native chats on this server (docs/vm-browser-bridge.md, "Native chats").
//!
//! The Rust port of `vm-guest/browser_bridge.py` as `codex_browser_host.py` runs it in the
//! Python server: the same sockets, the same queue file, the same JSON-RPC methods. Agents run
//! the launcher `<state>/browser-host/studio-browser URL` (it is also `BROWSER`, and
//! `xdg-open`, and `open` on macOS, in `<state>/browser-host/bin`). The launcher sends the
//! link on `open.sock`; this bridge checks it with the URL rules (`browser_policy`), finds the
//! chat by its session hint and queues a request. The server reads the queue on `control.sock` with `browser.wait` and
//! shows the person a card (`browser_cards`). After an approval the one sign-in redirect is
//! relayed to the tool's loopback listener with `browser.callback`. Every connection to a
//! listener first checks that the listener belongs to this server's user.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

use crate::browser_policy::{self, Checked, PolicyError};

use crate::browser_environment::{
    BROWSER_HOST_FOLDER as FOLDER, BROWSER_LAUNCHER as LAUNCHER, BROWSER_OPEN_SOCKET as OPEN_SOCKET,
};
/// The JSON-RPC socket of the server's card poll and answers
/// (`HOST_BROWSER_CONTROL_SOCKET_NAME` of `codex_browser_host.py`).
pub const CONTROL_SOCKET: &str = "control.sock";
/// The folder of `xdg-open` style links to the launcher, for tools that search `PATH`
/// (`PATH_FOLDER` of `codex_browser_host.py`).
pub const PATH_FOLDER: &str = "bin";
/// The file that gives a Claude bridge the session environment.
pub const SESSION_FILE: &str = "studio-browser.json";
/// The hidden argument that makes the runtime server binary act as the launcher.
pub const SHIM_ARGUMENT: &str = "--studio-browser";
/// The guest's launcher socket when `STUDIO_BROWSER_SOCKET` is unset.
const GUEST_SOCKET: &str = "/run/codex-studio-browser/open.sock";
const PATH_NAMES: [&str; 3] = ["xdg-open", "x-www-browser", "www-browser"];
/// Studio's `open` in the `PATH` folder of a macOS host, for tools that run `open URL` and
/// ignore `BROWSER`. It sends one http or https URL to the launcher and passes every other use
/// to `/usr/bin/open` unchanged. Python `codex_browser_host.OPEN_SHIM` holds a copy;
/// `test_codex_browser_host_env.py` checks that the copy equals this file.
pub const OPEN_SHIM: &str = include_str!("browser_open.sh");
/// The name of `OPEN_SHIM` in the `PATH` folder. Only a macOS host gets it: on Linux `open`
/// is a different program (openvt), and tools there run `xdg-open`.
pub const OPEN_NAME: &str = "open";

/// A request lives this long, in seconds (`TTL_SECONDS`).
const TTL_SECONDS: f64 = 600.0;
/// A closed request stays in the queue this long, in seconds.
const KEEP_SECONDS: f64 = 3600.0;
/// A pending request whose command still listens gets a new deadline when its current one is
/// at most this many seconds away (`EXTEND_BEFORE_SECONDS`, docs/vm-browser-bridge.md,
/// "Card lifetime").
const EXTEND_BEFORE_SECONDS: f64 = TTL_SECONDS / 2.0;
/// The open limit of a listening request, in seconds from its creation. Its owner is
/// `SIGN_IN_OPEN_LIMIT_SECONDS`; the final state at this bound must still be in the queue.
const OPEN_LIMIT_SECONDS: f64 =
    studio_runtime_schema::shared_values::SIGN_IN_OPEN_LIMIT_SECONDS as f64;
const _: () = assert!(OPEN_LIMIT_SECONDS + TTL_SECONDS < KEEP_SECONDS);
const LIMIT_WINDOW_SECONDS: f64 = 600.0;
const LIMIT_COUNT: usize = 6;
const CONNECTIONS_PER_UID: usize = 2;
const LINE_LIMIT: usize = 16 * 1024;
const CONTROL_LINE_LIMIT: usize = 64 * 1024;
const READ_SECONDS: u64 = 5;
const CHECK_SECONDS: u64 = 5;
const MAX_RELAYS: u64 = 5;
const BODY_LIMIT: usize = 256 * 1024;
const HEAD_LIMIT: usize = 64 * 1024;
const MAX_WAIT_SECONDS: f64 = 20.0;
const SENT_HEADERS: [&str; 3] = ["user-agent", "accept", "accept-language"];
/// A request of another chat that waits on the open request of the same Linux user,
/// sign-in host and client (`joinedTo`) instead of a card of its own.
pub const JOINED: &str = studio_runtime_schema::shared_values::SIGN_IN_JOINED_STATE;
/// The `closeReason` of a joined request whose sign-in finished on the card it waited on.
pub const JOINED_SIGNED_IN_REASON: &str =
    studio_runtime_schema::shared_values::SIGN_IN_JOINED_SIGNED_IN_REASON;
const OPEN_STATES: [&str; 3] = ["pending", "approved", JOINED];
/// States after which the same link of a chat can queue again (`REOPEN_STATES`).
const REOPEN_STATES: [&str; 3] = ["stale", "expired", "replaced"];
const IPV4_REACH: [&str; 4] = ["127.0.0.1", "0.0.0.0", "::ffff:127.0.0.1", "::"];
const IPV6_REACH: [&str; 2] = ["::1", "::"];

/// A refusal with the guest's error code.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct BridgeError {
    pub code: &'static str,
    pub message: String,
}

fn bridge_error<T>(code: &'static str, message: impl Into<String>) -> Result<T, BridgeError> {
    Err(BridgeError {
        code,
        message: message.into(),
    })
}

impl From<PolicyError> for BridgeError {
    fn from(error: PolicyError) -> Self {
        Self {
            code: "invalid_url",
            message: error.0,
        }
    }
}

/// One listening TCP socket and its owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listener {
    pub address: String,
    pub port: u16,
    pub uid: u32,
}

fn proc_address(text: &str) -> Option<String> {
    let data = data_encoding::HEXUPPER_PERMISSIVE
        .decode(text.as_bytes())
        .ok()?;
    match data.len() {
        4 => Some(std::net::Ipv4Addr::new(data[3], data[2], data[1], data[0]).to_string()),
        16 => {
            // /proc writes IPv6 as four 32-bit words in host byte order.
            let mut bytes = [0u8; 16];
            for (word, chunk) in data.chunks(4).enumerate() {
                for (index, byte) in chunk.iter().rev().enumerate() {
                    bytes[word * 4 + index] = *byte;
                }
            }
            Some(std::net::Ipv6Addr::from(bytes).to_string())
        }
        _ => None,
    }
}

/// The listening sockets of /proc/net/tcp or tcp6 text, with their owner UIDs.
pub fn parse_proc_listeners(text: &str) -> Vec<Listener> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 8 || fields[3] != "0A" {
                return None;
            }
            let (address, port) = fields[1].split_once(':')?;
            Some(Listener {
                address: proc_address(address)?,
                port: u16::from_str_radix(port, 16).ok()?,
                uid: fields[7].parse().ok()?,
            })
        })
        .collect()
}

/// The TCP listeners in `lsof -F ptun` output, with their owner UIDs.
pub fn parse_lsof_listeners(text: &str) -> Vec<Listener> {
    let mut rows = Vec::new();
    let mut uid: Option<u32> = None;
    let mut family = String::new();
    for line in text.lines() {
        let Some(tag) = line.chars().next() else {
            continue;
        };
        let value = &line[tag.len_utf8()..];
        match tag {
            'p' => {
                uid = None;
                family.clear();
            }
            'u' => {
                uid = value
                    .parse()
                    .ok()
                    .filter(|_| value.bytes().all(|b| b.is_ascii_digit()))
            }
            't' => family = value.to_owned(),
            'n' => {
                let Some(uid) = uid else { continue };
                let Some((host, port)) = value.rsplit_once(':') else {
                    continue;
                };
                if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
                    continue;
                }
                let Ok(port) = port.parse::<u16>() else {
                    continue;
                };
                let host = host.trim_matches(['[', ']']);
                let host = if host == "*" {
                    if family == "IPv6" { "::" } else { "0.0.0.0" }
                } else {
                    host
                };
                rows.push(Listener {
                    address: host.to_owned(),
                    port,
                    uid,
                });
            }
            _ => {}
        }
    }
    rows
}

/// The listeners of this host: /proc on Linux, `lsof` elsewhere. `None` when the
/// probe failed, timed out or was cut short: the listeners are unknown, which is
/// not the same as no listener.
pub fn host_listeners() -> Option<Vec<Listener>> {
    if cfg!(target_os = "linux") {
        let mut rows = Vec::new();
        for name in ["tcp", "tcp6"] {
            match std::fs::read_to_string(Path::new("/proc/net").join(name)) {
                Ok(text) => rows.extend(parse_proc_listeners(&text)),
                // A host without IPv6 has no tcp6 table.
                Err(error) if name == "tcp6" && error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return None,
            }
        }
        return Some(rows);
    }
    // Bound the external listener probe even on a blocking-pool worker.
    const LISTENER_OUTPUT_LIMIT_BYTES: usize = 1024 * 1024;
    // `lsof` reads every process of the host. On a loaded Mac one probe took
    // up to 1.7 s; a probe past this bound gives unknown listeners.
    const LISTENER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    let output = crate::bounded_child::run(crate::bounded_child::Request {
        program: std::ffi::OsStr::new("lsof"),
        git_directory: None,
        args: &["-nP", "-iTCP", "-sTCP:LISTEN", "-F", "ptun"],
        environment: &[],
        input: None,
        stdout_limit: LISTENER_OUTPUT_LIMIT_BYTES,
        stderr_limit: LISTENER_OUTPUT_LIMIT_BYTES,
        timeout: LISTENER_TIMEOUT,
    });
    match output {
        Ok(output) if !output.stdout.truncated => Some(parse_lsof_listeners(
            &String::from_utf8_lossy(&output.stdout.bytes),
        )),
        _ => None,
    }
}

/// The error code of a bridge request that sent nothing to the sign-in command,
/// so a retry cannot repeat a delivery. `browser_answer` keeps the card open.
pub const NOT_SENT_CODE: &str = studio_runtime_schema::shared_values::SIGN_IN_NOT_SENT_CODE;

/// The answer when the listeners of this host are unknown. An open card stays
/// open; only a known missing listener makes it stale.
fn listeners_unknown() -> BridgeError {
    BridgeError {
        code: NOT_SENT_CODE,
        message: "Studio could not read the listening ports of this computer. Try again".into(),
    }
}

/// The address of the listener of `uid` on `port`. Every listener that a loopback connection
/// to that port can reach must belong to `uid`.
pub fn connect_address(
    uid: u32,
    port: u16,
    listeners: &[Listener],
) -> Result<&'static str, BridgeError> {
    let reachable: Vec<&Listener> = listeners
        .iter()
        .filter(|row| {
            row.port == port
                && (IPV4_REACH.contains(&row.address.as_str())
                    || IPV6_REACH.contains(&row.address.as_str()))
        })
        .collect();
    if reachable.is_empty() {
        return bridge_error(
            "stale",
            format!("The sign-in command no longer listens on port {port}"),
        );
    }
    if reachable.iter().any(|row| row.uid != uid) {
        return bridge_error("forbidden", format!("Another user listens on port {port}"));
    }
    let ipv4 = reachable
        .iter()
        .any(|row| IPV4_REACH[..3].contains(&row.address.as_str()));
    Ok(if ipv4 { "127.0.0.1" } else { "::1" })
}

/// The reply of a tool's loopback listener.
#[derive(Clone, Debug)]
pub struct Reply {
    pub status: u16,
    pub location: Option<String>,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// One GET to a loopback listener. No redirect is followed.
pub fn fetch(
    address: &str,
    port: u16,
    target: &str,
    headers: &[(String, String)],
    timeout: Duration,
    limit: usize,
) -> Result<Reply, BridgeError> {
    let unavailable = || BridgeError {
        code: "unavailable",
        message: format!("The sign-in command did not answer on port {port}"),
    };
    let authority = if address.contains(':') {
        format!("[{address}]:{port}")
    } else {
        format!("{address}:{port}")
    };
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(timeout))
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let mut request = agent.get(format!("http://{authority}{target}"));
    for (name, value) in headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request.call().map_err(|_| unavailable())?;
    let status = response.status().as_u16();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let location = header("location");
    let content_type = header("content-type").unwrap_or_default();
    let body = response
        .into_body()
        .into_with_config()
        .limit(limit as u64 + 1)
        .read_to_vec()
        .map_err(|_| unavailable())?;
    if body.len() > limit {
        return bridge_error(
            "output_limit",
            format!("The page on port {port} is larger than {limit} bytes"),
        );
    }
    Ok(Reply {
        status,
        location,
        content_type,
        body,
    })
}

/// The chat that a session hint names.
#[derive(Clone, Debug)]
pub struct ChatContext {
    pub uid: u32,
    pub project_id: Option<String>,
    pub owner: String,
    pub agent_id: String,
}

/// Finds the chat of a session hint (`codex_browser_host.chat`).
pub trait ChatLookup: Send + Sync {
    fn chat<'a>(
        &'a self,
        session: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Option<ChatContext>>;
}

/// The chat lookup of a runtime store: a not deleted chat whose ID or native `threadId` is the
/// hint. A VM chat reaches this bridge only through a `host_exec` command, which runs on this
/// Mac as this user (`host_exec_service`); its own guest commands use the guest bridge. Agents
/// of one user can act for each other, so the hint is not a security boundary; the approval
/// card is the consent.
pub struct StoreChats {
    pub store: Arc<studio_runtime_store::RuntimeStore>,
}

pub fn current_uid() -> u32 {
    nix::unistd::getuid().as_raw()
}

fn user_name() -> String {
    nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|user| user.name)
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_default()
}

impl ChatLookup for StoreChats {
    fn chat<'a>(
        &'a self,
        session: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Option<ChatContext>> {
        Box::pin(self.chat_record(session.to_owned()))
    }
}

impl StoreChats {
    async fn chat_record(&self, session: String) -> Option<ChatContext> {
        let rows: Vec<String> = self
            .store
            .read_async("runtime-services/browser_host", move |connection| {
                let mut statement = connection.prepare(
                    "SELECT record FROM runtime_agents WHERE id=?1 OR thread_id=?1 LIMIT 2",
                )?;
                let rows = statement
                    .query_map([&session], |row| row.get(0))?
                    .collect::<Result<_, _>>()?;
                Ok::<_, studio_runtime_store::StoreError>(rows)
            })
            .await
            .ok()?;
        let [raw] = rows.as_slice() else {
            return None;
        };
        let agent: Value = serde_json::from_str(raw).ok()?;
        if truthy(agent.get("deletedAt")) {
            return None;
        }
        Some(ChatContext {
            uid: current_uid(),
            project_id: agent
                .get("layrProjectId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            owner: user_name(),
            agent_id: agent.get("id").and_then(Value::as_str)?.to_owned(),
        })
    }
}

pub(crate) fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Number(number)) => number.as_f64() != Some(0.0),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(fields)) => !fields.is_empty(),
    }
}

fn canonical_session(value: &Value) -> Result<String, BridgeError> {
    if let Some(text) = value.as_str()
        && let Ok(parsed) = uuid::Uuid::parse_str(text)
        && parsed.hyphenated().to_string() == text
    {
        return Ok(text.to_owned());
    }
    bridge_error(
        "unknown_agent",
        "Run the command in a Studio chat: LAYR_SESSION is missing or invalid",
    )
}

fn clean_headers(headers: Option<&Value>) -> Vec<(String, String)> {
    let Some(headers) = headers.and_then(Value::as_object) else {
        return Vec::new();
    };
    SENT_HEADERS
        .iter()
        .filter_map(|name| {
            headers
                .get(*name)
                .and_then(Value::as_str)
                .filter(|value| value.len() <= 1024 && !value.chars().any(|c| (c as u32) < 32))
                .map(|value| ((*name).to_owned(), value.to_owned()))
        })
        .collect()
}

/// Python `urljoin` for the first hop: an absolute `Location` stays as it is.
fn join_location(base: &str, location: &str) -> String {
    let absolute = location.split_once(':').is_some_and(|(scheme, rest)| {
        !scheme.is_empty()
            && scheme.as_bytes()[0].is_ascii_alphabetic()
            && scheme
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
            && rest.starts_with("//")
    });
    if absolute {
        return location.to_owned();
    }
    url::Url::parse(base)
        .and_then(|base| base.join(location))
        .map(|joined| joined.to_string())
        .unwrap_or_else(|_| location.to_owned())
}

fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs_f64())
        .unwrap_or_default()
}

type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;
/// This host's listeners, or `None` when the probe failed (see [`host_listeners`]).
type Listeners = Arc<dyn Fn() -> Option<Vec<Listener>> + Send + Sync>;

struct Queue {
    seq: u64,
    items: BTreeMap<String, Value>,
}

/// The in-process queue of sign-in links of native chats.
pub struct HostBridge {
    path: PathBuf,
    queue: Mutex<Queue>,
    changed: watch::Sender<u64>,
    chats: Arc<dyn ChatLookup>,
    listeners: Listeners,
    http_providers: BTreeSet<String>,
    clock: Clock,
    connections: Mutex<BTreeMap<u32, usize>>,
    local_open: bool,
}

/// The `closeReason` of a request whose command stopped listening after the launcher opened
/// its page in the browser of this Mac (`HostBridgeOptions::local_open`). The card then
/// closes as answered by that browser (`browser_cards::apply`).
pub const LOCAL_CLOSE_REASON: &str = "local";

/// Options of the bridge: listener tables, test provider origins, a clock, and the Mac fast
/// path. `Default` is for tests and opens nothing; the server uses [`Self::for_this_host`].
pub struct HostBridgeOptions {
    pub listeners: Listeners,
    pub http_providers: BTreeSet<String>,
    pub clock: Clock,
    /// The launcher also opens each link in the browser of this Mac at once, as the tool would
    /// without Studio, and the card stays for another window or a phone
    /// (docs/vm-browser-bridge.md, "Native chats").
    pub local_open: bool,
}

impl Default for HostBridgeOptions {
    fn default() -> Self {
        Self {
            listeners: Arc::new(host_listeners),
            http_providers: BTreeSet::new(),
            clock: Arc::new(now_seconds),
            local_open: false,
        }
    }
}

impl HostBridgeOptions {
    /// The server's options: a macOS host opens links in its own browser at once. A Linux
    /// host has no person at a browser, and its `xdg-open` is this launcher.
    pub fn for_this_host() -> Self {
        Self {
            local_open: cfg!(target_os = "macos"),
            ..Self::default()
        }
    }
}

fn private_folder(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    if std::fs::read(path).is_ok_and(|current| current == bytes) {
        return Ok(());
    }
    let folder = path.parent().unwrap_or(Path::new("."));
    let mut file = tempfile::Builder::new()
        .prefix(".studio-browser-")
        .tempfile_in(folder)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(mode))?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

impl HostBridge {
    /// Read the saved queue in `<folder>/browser/requests.json`, or start an empty one.
    pub fn open(
        folder: &Path,
        chats: Arc<dyn ChatLookup>,
        options: HostBridgeOptions,
    ) -> std::io::Result<Arc<Self>> {
        let directory = folder.join("browser");
        private_folder(&directory)?;
        let path = directory.join("requests.json");
        let saved: Value = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({})),
            Err(_) => json!({}),
        };
        let seq = saved.get("seq").and_then(Value::as_u64).unwrap_or(0);
        let items = saved
            .get("items")
            .and_then(Value::as_object)
            .map(|items| {
                items
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let (changed, _) = watch::channel(seq);
        Ok(Arc::new(Self {
            path,
            queue: Mutex::new(Queue { seq, items }),
            changed,
            chats,
            listeners: options.listeners,
            http_providers: options.http_providers,
            clock: options.clock,
            connections: Mutex::new(BTreeMap::new()),
            local_open: options.local_open,
        }))
    }

    /// This host's listeners from the blocking pool, or `None` when they are unknown.
    async fn probe_listeners(&self) -> Option<Vec<Listener>> {
        let listeners = Arc::clone(&self.listeners);
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/browser_host",
            move || listeners(),
        )
        .await
        .ok()
        .flatten()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn update(&self, queue: &mut Queue, id: &str, fields: Value) {
        queue.seq += 1;
        let seq = queue.seq;
        if let Some(Value::Object(item)) = queue.items.get_mut(id) {
            if let Value::Object(fields) = fields {
                item.extend(fields);
            }
            item.insert("seq".into(), json!(seq));
        }
        self.changed.send_replace(seq);
    }

    fn save(&self, queue: &mut Queue) {
        let now = (self.clock)();
        queue.items.retain(|_, item| {
            let state = item
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or_default();
            OPEN_STATES.contains(&state)
                || item.get("createdAt").and_then(Value::as_f64).unwrap_or(0.0)
                    >= now - KEEP_SECONDS
        });
        let value = json!({"seq": queue.seq, "items": queue.items});
        if let Err(error) = write_atomic(
            &self.path,
            serde_json::to_string(&value).unwrap_or_default().as_bytes(),
            0o600,
        ) {
            eprintln!("browser host queue write failed: {error}");
        }
    }

    /// Queue one link of a chat (the launcher's request).
    pub async fn open_link(
        &self,
        uid: u32,
        url: &Value,
        session: &Value,
    ) -> Result<Value, BridgeError> {
        let agent = canonical_session(session)?;
        let context = self.chats.chat(&agent).await.ok_or_else(|| BridgeError {
            code: "unknown_agent",
            message: "Studio does not know the chat in LAYR_SESSION".into(),
        })?;
        if context.uid != uid {
            return bridge_error("unknown_agent", "LAYR_SESSION names a chat of another user");
        }
        self.queue_link(uid, url, agent, context).await
    }

    /// `browser.submit`: the server queues a link for a chat, from the agent's
    /// `orchestration_sign_in` call or from a sign-in link in its message (section 7). The chat's
    /// own user owns the request, so the listener checks stay those of a link that its command
    /// opened itself (`BrowserBridge.submit`).
    async fn submit(&self, params: &Value) -> Result<Value, BridgeError> {
        let agent = canonical_session(params.get("agentId").unwrap_or(&Value::Null))?;
        let context = self.chats.chat(&agent).await.ok_or_else(|| BridgeError {
            code: "unknown_agent",
            message: "Studio does not know this chat".into(),
        })?;
        self.queue_link(
            context.uid,
            params.get("url").unwrap_or(&Value::Null),
            agent,
            context,
        )
        .await
    }

    /// The request of this chat for the same link while it can still answer
    /// (`BrowserBridge._existing`): a link that a command opens and that the agent also gives
    /// in a message or a tool call gets one card.
    fn existing(queue: &Queue, agent: &str, url: &Value, now: f64) -> Option<Value> {
        let url = url.as_str()?;
        queue
            .items
            .values()
            .find(|item| {
                item.get("agentId").and_then(Value::as_str) == Some(agent)
                    && (item.get("url").and_then(Value::as_str) == Some(url)
                        || item.get("startUrl").and_then(Value::as_str) == Some(url))
                    && !REOPEN_STATES.contains(
                        &item
                            .get("state")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    )
                    && item.get("createdAt").and_then(Value::as_f64).unwrap_or(0.0)
                        > now - TTL_SECONDS
            })
            .cloned()
    }

    async fn queue_link(
        &self,
        uid: u32,
        url: &Value,
        agent: String,
        context: ChatContext,
    ) -> Result<Value, BridgeError> {
        let agent = if context.agent_id.is_empty() {
            agent
        } else {
            context.agent_id.clone()
        };
        let now = (self.clock)();
        let original = url.clone();
        if let Some(item) = Self::existing(&self.lock(), &agent, &original, now) {
            return Ok(item);
        }
        {
            let queue = self.lock();
            let recent = queue
                .items
                .values()
                .filter(|item| {
                    item.get("uid").and_then(Value::as_u64) == Some(u64::from(uid))
                        && item.get("createdAt").and_then(Value::as_f64).unwrap_or(0.0)
                            > now - LIMIT_WINDOW_SECONDS
                })
                .count();
            if recent >= LIMIT_COUNT {
                return bridge_error(
                    "limit",
                    format!(
                        "This user asked {LIMIT_COUNT} times in 10 minutes. Wait, then try again"
                    ),
                );
            }
        }
        let url = url
            .as_str()
            .ok_or_else(|| BridgeError::from(PolicyError("Give one link".into())))?;
        let checked = browser_policy::classify(url, &self.http_providers)?;
        let mut start_url = Value::Null;
        let final_link = match checked {
            Checked::StartPage { url, port } => {
                start_url = json!(url);
                let rows = self.probe_listeners().await.ok_or_else(listeners_unknown)?;
                let address = connect_address(uid, port, &rows)?;
                let after = url
                    .split_once("://")
                    .map(|(_, rest)| rest)
                    .unwrap_or_default();
                let end = after.find(['/', '?', '#']).unwrap_or(after.len());
                let netloc = after[..end].to_owned();
                let rest = after[end..].split('#').next().unwrap_or_default();
                let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
                let path = if path.is_empty() { "/" } else { path };
                let target = if query.is_empty() {
                    path.to_owned()
                } else {
                    format!("{path}?{query}")
                };
                let headers = vec![
                    ("Host".to_owned(), netloc),
                    ("Accept".to_owned(), "text/html".to_owned()),
                ];
                let reply = studio_runtime_process::blocking_pool::spawn_blocking(
                    "runtime-services/browser_host",
                    move || {
                        fetch(
                            address,
                            port,
                            &target,
                            &headers,
                            Duration::from_secs(READ_SECONDS),
                            HEAD_LIMIT,
                        )
                    },
                )
                .await
                .map_err(|_| BridgeError {
                    code: "unavailable",
                    message: format!("The sign-in command did not answer on port {port}"),
                })??;
                let Some(location) = reply
                    .location
                    .filter(|_| (300..400).contains(&reply.status))
                else {
                    return bridge_error(
                        "invalid_url",
                        format!(
                            "The page on port {port} does not redirect to a sign-in page. Studio opens only pages that redirect to https"
                        ),
                    );
                };
                browser_policy::provider(&join_location(&url, &location), &self.http_providers)?
            }
            Checked::Provider(provider) => provider,
        };
        if let Some(redirect) = &final_link.redirect {
            let rows = self.probe_listeners().await.ok_or_else(listeners_unknown)?;
            connect_address(uid, redirect.port, &rows)?;
        }
        let mut queue = self.lock();
        if let Some(item) = Self::existing(&queue, &agent, &original, now) {
            return Ok(item);
        }
        // One card for one sign-in: a link of another chat with the same Linux user, sign-in
        // host and client waits on the open request (docs/vm-browser-bridge.md, "One card for
        // the same sign-in"). A device code page has no client and stays on its own.
        let leader = final_link.client_id.as_deref().and_then(|client| {
            queue
                .items
                .values()
                .filter(|item| {
                    matches!(item["state"].as_str(), Some("pending" | "approved"))
                        && item.get("agentId").and_then(Value::as_str) != Some(agent.as_str())
                        && item.get("uid").and_then(Value::as_u64) == Some(u64::from(uid))
                        && item.get("host").and_then(Value::as_str)
                            == Some(final_link.host.as_str())
                        && item.get("clientId").and_then(Value::as_str) == Some(client)
                        && now <= item["expiresAt"].as_f64().unwrap_or(0.0)
                })
                .min_by(|left, right| {
                    left["createdAt"]
                        .as_f64()
                        .unwrap_or(0.0)
                        .total_cmp(&right["createdAt"].as_f64().unwrap_or(0.0))
                })
                .and_then(|item| item.get("id").cloned())
        });
        // The latest link of a chat replaces its earlier pending link.
        let replaced: Vec<String> = queue
            .items
            .iter()
            .filter(|(_, item)| {
                item.get("agentId").and_then(Value::as_str) == Some(agent.as_str())
                    && matches!(item["state"].as_str(), Some("pending" | JOINED))
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in replaced {
            self.update(&mut queue, &key, json!({"state": "replaced"}));
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        let redirect = final_link
            .redirect
            .as_ref()
            .map(|redirect| json!({"host": redirect.host, "port": redirect.port, "path": redirect.path}))
            .unwrap_or(Value::Null);
        let item = json!({
            "id": id, "agentId": agent, "projectId": context.project_id, "uid": uid,
            "linuxUser": context.owner, "url": final_link.url, "startUrl": start_url,
            "host": final_link.host, "redirect": redirect, "clientId": final_link.client_id,
            "scope": final_link.scope, "state": "pending", "createdAt": now,
            "expiresAt": now + TTL_SECONDS, "relays": 0, "lease": null,
        });
        let mut item = item;
        if let Some(leader) = leader {
            item["state"] = json!(JOINED);
            item["joinedTo"] = leader;
        }
        queue.items.insert(id.clone(), item);
        self.update(&mut queue, &id, json!({}));
        self.save(&mut queue);
        Ok(queue.items[&id].clone())
    }

    /// One JSON-RPC method of the server side.
    pub async fn call(&self, method: &str, params: &Value) -> Result<Value, BridgeError> {
        match method {
            "browser.wait" => self.wait(params).await,
            "browser.submit" => self.submit(params).await,
            "browser.answer" => self.answer(params).await,
            "browser.close" => {
                let mut queue = self.lock();
                let (id, state) = item_state(&queue, params)?;
                if OPEN_STATES.contains(&state.as_str()) {
                    let reason = params
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("closed");
                    self.update(
                        &mut queue,
                        &id,
                        json!({"state": "closed", "closeReason": reason}),
                    );
                    self.save(&mut queue);
                }
                Ok(json!({"state": queue.items[&id]["state"]}))
            }
            "browser.callback" => self.callback(params).await,
            _ => bridge_error("invalid_request", "The browser method is not supported"),
        }
    }

    async fn wait(&self, params: &Value) -> Result<Value, BridgeError> {
        let after = params.get("after").cloned().unwrap_or(json!(0));
        let seconds = params.get("waitSeconds").cloned().unwrap_or(json!(0));
        let (Some(mut after), Some(seconds)) = (after.as_u64(), seconds.as_f64()) else {
            return bridge_error(
                "invalid_params",
                "after must be a cursor and waitSeconds at most 20",
            );
        };
        if !(0.0..=MAX_WAIT_SECONDS).contains(&seconds)
            || params.get("after").is_some_and(Value::is_f64)
        {
            return bridge_error(
                "invalid_params",
                "after must be a cursor and waitSeconds at most 20",
            );
        }
        let mut changes = self.changed.subscribe();
        {
            let queue = self.lock();
            if after > queue.seq {
                after = 0; // A queue reset. Send everything again.
            }
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(seconds);
        loop {
            if *changes.borrow_and_update() > after {
                break;
            }
            if tokio::time::timeout_at(deadline, changes.changed())
                .await
                .is_err()
            {
                break;
            }
        }
        let queue = self.lock();
        let mut items: Vec<Value> = queue
            .items
            .values()
            .filter(|item| item.get("seq").and_then(Value::as_u64).unwrap_or(0) > after)
            .cloned()
            .collect();
        items.sort_by_key(|item| item.get("seq").and_then(Value::as_u64).unwrap_or(0));
        Ok(json!({"items": items, "next": queue.seq}))
    }

    async fn answer(&self, params: &Value) -> Result<Value, BridgeError> {
        let rows = self.probe_listeners().await;
        let mut queue = self.lock();
        let (id, state) = item_state(&queue, params)?;
        let item = queue.items[&id].clone();
        match params.get("decision").and_then(Value::as_str) {
            Some("decline") => {
                if state == "pending" {
                    self.update(&mut queue, &id, json!({"state": "declined"}));
                    self.save(&mut queue);
                }
                let item = &queue.items[&id];
                return Ok(
                    json!({"state": item["state"], "lease": item.get("lease").cloned().unwrap_or(Value::Null)}),
                );
            }
            Some("approve") => {}
            _ => return bridge_error("invalid_params", "decision must be approve or decline"),
        }
        if matches!(state.as_str(), "approved" | "opened") {
            return Ok(json!({"state": state, "lease": item["lease"]}));
        }
        if state != "pending" || (self.clock)() > item["expiresAt"].as_f64().unwrap_or(0.0) {
            return bridge_error(
                "stale",
                "The sign-in request is no longer open. Ask the agent to run the command again",
            );
        }
        let Some(redirect) = item.get("redirect").filter(|value| value.is_object()) else {
            // A link without a loopback redirect (a device code page) needs no forward.
            self.update(&mut queue, &id, json!({"state": "opened"}));
            self.save(&mut queue);
            return Ok(json!({"state": "opened", "lease": null}));
        };
        let uid = item["uid"].as_u64().unwrap_or(u64::MAX) as u32;
        let port = redirect["port"].as_u64().unwrap_or(0) as u16;
        let rows = rows.ok_or_else(listeners_unknown)?;
        if let Err(error) = connect_address(uid, port, &rows) {
            self.update(&mut queue, &id, json!({"state": "stale"}));
            self.save(&mut queue);
            return bridge_error("stale", error.message);
        }
        let mut lease = redirect.as_object().cloned().unwrap_or_default();
        lease.insert("expiresAt".into(), json!((self.clock)() + TTL_SECONDS));
        let lease = Value::Object(lease);
        self.update(
            &mut queue,
            &id,
            json!({"state": "approved", "lease": lease}),
        );
        self.save(&mut queue);
        Ok(json!({"state": "approved", "lease": lease}))
    }

    async fn callback(&self, params: &Value) -> Result<Value, BridgeError> {
        let (id, lease, uid, target) = {
            let queue = self.lock();
            let (id, state) = item_state(&queue, params)?;
            let item = &queue.items[&id];
            let lease = item.get("lease").cloned().unwrap_or(Value::Null);
            if state != "approved"
                || !lease.is_object()
                || (self.clock)() > lease["expiresAt"].as_f64().unwrap_or(0.0)
            {
                return bridge_error("stale", "The sign-in forward is closed");
            }
            if item.get("relays").and_then(Value::as_u64).unwrap_or(0) >= MAX_RELAYS {
                return bridge_error(
                    "limit",
                    "The sign-in forward relayed its maximum number of requests",
                );
            }
            let target = params
                .get("target")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let path = target.split(['?', '#']).next().unwrap_or_default();
            let path = if path.is_empty() { "/" } else { path };
            if !target.starts_with('/')
                || target.starts_with("//")
                || path != lease["path"].as_str().unwrap_or_default()
                || target.chars().count() > 8192
            {
                return bridge_error("invalid_params", "Only the sign-in redirect path can pass");
            }
            (
                id,
                lease,
                item["uid"].as_u64().unwrap_or(u64::MAX) as u32,
                target,
            )
        };
        let port = lease["port"].as_u64().unwrap_or(0) as u16;
        let rows = self.probe_listeners().await.ok_or_else(listeners_unknown)?;
        let address = connect_address(uid, port, &rows)?;
        let host = lease["host"].as_str().unwrap_or("127.0.0.1");
        let host = if host == "::1" { "[::1]" } else { host };
        let mut headers = vec![("Host".to_owned(), format!("{host}:{port}"))];
        headers.extend(clean_headers(params.get("headers")));
        headers.push(("Connection".into(), "close".into()));
        {
            let mut queue = self.lock();
            if let Some(Value::Object(item)) = queue.items.get_mut(&id) {
                let relays = item.get("relays").and_then(Value::as_u64).unwrap_or(0) + 1;
                item.insert("relays".into(), json!(relays));
            }
            self.save(&mut queue);
        }
        let fetch_target = target.clone();
        let reply = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/browser_host",
            move || {
                fetch(
                    address,
                    port,
                    &fetch_target,
                    &headers,
                    Duration::from_secs(10),
                    BODY_LIMIT,
                )
            },
        )
        .await
        .map_err(|_| BridgeError {
            code: "unavailable",
            message: format!("The sign-in command did not answer on port {port}"),
        })??;
        let query = target
            .split_once('?')
            .map(|(_, query)| query)
            .unwrap_or_default();
        let query = query.split('#').next().unwrap_or_default();
        // Python parse_qs without blank values: a name counts only with a value.
        let done = query.split('&').any(|pair| {
            pair.split_once('=')
                .is_some_and(|(name, value)| matches!(name, "code" | "error") && !value.is_empty())
        });
        {
            let mut queue = self.lock();
            if done && queue.items[&id]["state"] == "approved" {
                self.update(
                    &mut queue,
                    &id,
                    json!({"state": "closed", "closeReason": "completed"}),
                );
            }
            self.save(&mut queue);
        }
        Ok(json!({
            "status": reply.status,
            "contentType": reply.content_type.chars().take(200).collect::<String>(),
            "body": base64::engine::general_purpose::STANDARD.encode(&reply.body),
            "done": done,
        }))
    }

    /// Mark requests whose time ran out or whose tool stopped listening. A pending request
    /// whose tool still listens stays open up to `OPEN_LIMIT_SECONDS` (`listening_deadline`).
    pub async fn check(&self) {
        let needs_rows = {
            let queue = self.lock();
            queue.items.values().any(|item| {
                OPEN_STATES.contains(&item["state"].as_str().unwrap_or_default())
                    && item.get("redirect").is_some_and(Value::is_object)
            })
        };
        // `None`: the probe failed, so no card becomes stale in this pass.
        let rows = if needs_rows {
            self.probe_listeners().await
        } else {
            Some(Vec::new())
        };
        let now = (self.clock)();
        let mut queue = self.lock();
        let mut changed = false;
        let keys: Vec<String> = queue.items.keys().cloned().collect();
        for key in keys {
            let item = queue.items[&key].clone();
            if !OPEN_STATES.contains(&item["state"].as_str().unwrap_or_default()) {
                continue;
            }
            let mut expires = item
                .get("lease")
                .and_then(|lease| lease.get("expiresAt"))
                .and_then(Value::as_f64)
                .unwrap_or_else(|| item["expiresAt"].as_f64().unwrap_or(0.0));
            // `None`: no loopback redirect, or the probe failed and the listener is unknown. An
            // unknown listener neither makes the request stale nor moves its deadline: expiry by
            // time still applies (`a_failed_listener_probe_keeps_an_open_card_and_answers_retryably`).
            let listening = item
                .get("redirect")
                .filter(|value| value.is_object())
                .zip(rows.as_ref())
                .map(|(redirect, rows)| {
                    let uid = item["uid"].as_u64().unwrap_or(u64::MAX) as u32;
                    let port = redirect["port"].as_u64().unwrap_or(0) as u16;
                    connect_address(uid, port, rows).is_ok()
                });
            if matches!(item["state"].as_str(), Some("pending" | JOINED))
                && listening == Some(true)
                && let Some(deadline) = listening_deadline(&item, now)
            {
                self.update(&mut queue, &key, json!({"expiresAt": deadline}));
                changed = true;
                expires = deadline;
            }
            if now > expires {
                self.update(&mut queue, &key, json!({"state": "expired"}));
                changed = true;
            } else if listening == Some(false) {
                // The browser of this Mac had the page: the command ended its sign-in there.
                let fields = if item["state"] == "pending" && truthy(item.get("localOpen")) {
                    json!({"state": "closed", "closeReason": LOCAL_CLOSE_REASON})
                } else {
                    json!({"state": "stale"})
                };
                self.update(&mut queue, &key, fields);
                changed = true;
            }
        }
        changed |= self.settle_joined(&mut queue);
        if changed {
            self.save(&mut queue);
        }
    }

    /// End the joined requests whose card ended: signed in when its sign-in finished, else the
    /// oldest joined request becomes the card and the others wait on it. Returns whether
    /// something changed.
    fn settle_joined(&self, queue: &mut Queue) -> bool {
        let mut changed = false;
        let joined: Vec<(String, String, f64)> = queue
            .items
            .iter()
            .filter(|(_, item)| item["state"] == JOINED)
            .map(|(key, item)| {
                (
                    key.clone(),
                    item["joinedTo"].as_str().unwrap_or_default().to_owned(),
                    item["createdAt"].as_f64().unwrap_or(0.0),
                )
            })
            .collect();
        let mut leaders: BTreeMap<String, Vec<(f64, String)>> = BTreeMap::new();
        for (key, leader, created) in joined {
            leaders.entry(leader).or_default().push((created, key));
        }
        for (leader, mut waiting) in leaders {
            let ended = queue.items.get(&leader).map(|card| {
                let state = card["state"].as_str().unwrap_or_default();
                (!OPEN_STATES.contains(&state), signed_in(card))
            });
            let (ended, signed) = ended.unwrap_or((true, false));
            if !ended {
                continue;
            }
            changed = true;
            if signed {
                for (_, key) in waiting {
                    self.update(
                        queue,
                        &key,
                        json!({"state": "closed", "closeReason": JOINED_SIGNED_IN_REASON}),
                    );
                }
                continue;
            }
            waiting.sort_by(|left, right| left.0.total_cmp(&right.0));
            let (_, card) = waiting.remove(0);
            self.update(queue, &card, json!({"state": "pending", "joinedTo": null}));
            for (_, key) in waiting {
                self.update(queue, &key, json!({"joinedTo": card}));
            }
        }
        changed
    }

    async fn launcher_client(self: Arc<Self>, stream: UnixStream) {
        let Ok(uid) = stream.peer_cred().map(|credentials| credentials.uid()) else {
            return;
        };
        {
            let mut connections = self
                .connections
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let count = connections.entry(uid).or_default();
            if *count >= CONNECTIONS_PER_UID {
                return;
            }
            *count += 1;
        }
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader).take(LINE_LIMIT as u64 + 1);
        let mut line = Vec::new();
        let read = tokio::time::timeout(
            Duration::from_secs(READ_SECONDS),
            reader.read_until(b'\n', &mut line),
        )
        .await;
        let reply = match read {
            Ok(Ok(_)) => self.handle_line(uid, &line).await,
            _ => {
                json!({"ok": false, "code": "invalid_request", "message": "Send one JSON line within 5 seconds"})
            }
        };
        let mut bytes = serde_json::to_vec(&reply).unwrap_or_default();
        bytes.push(b'\n');
        drop(
            tokio::time::timeout(Duration::from_secs(READ_SECONDS), writer.write_all(&bytes)).await,
        );
        let mut connections = self
            .connections
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(count) = connections.get_mut(&uid) {
            *count -= 1;
            if *count == 0 {
                connections.remove(&uid);
            }
        }
    }

    async fn handle_line(&self, uid: u32, line: &[u8]) -> Value {
        if line.len() > LINE_LIMIT || !line.ends_with(b"\n") {
            return json!({"ok": false, "code": "invalid_request", "message": "Send one JSON line of at most 16 KB"});
        }
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            return json!({"ok": false, "code": "invalid_request", "message": "The request is not valid JSON"});
        };
        if !value.is_object() || value.get("v") != Some(&json!(1)) {
            return json!({"ok": false, "code": "invalid_request", "message": "Use protocol version 1"});
        }
        // `open`: the launcher also opens the link in the browser of this Mac (`local_open`).
        match self
            .open_link(
                uid,
                value.get("url").unwrap_or(&Value::Null),
                value.get("session").unwrap_or(&Value::Null),
            )
            .await
        {
            Ok(item) => {
                // A joined link waits on a card whose page is already open: no second page.
                let joined = item["state"] == JOINED;
                if self.local_open && !joined {
                    self.mark_local_open(&item);
                }
                json!({"ok": true, "id": item["id"], "host": item["host"],
                       "open": self.local_open && !joined, "joined": joined})
            }
            Err(error) => {
                json!({"ok": false, "code": error.code, "message": error.message, "open": self.local_open})
            }
        }
    }

    /// Note that the launcher opens this request's page in the browser of this Mac, so its
    /// card closes as answered when the command stops listening (`check`).
    fn mark_local_open(&self, item: &Value) {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            return;
        };
        let mut queue = self.lock();
        let open = queue
            .items
            .get(id)
            .is_some_and(|item| item["state"] == "pending" && !truthy(item.get("localOpen")));
        if open {
            self.update(&mut queue, id, json!({"localOpen": true}));
            self.save(&mut queue);
        }
    }

    async fn control_client(self: Arc<Self>, stream: UnixStream) {
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader).take(CONTROL_LINE_LIMIT as u64 + 1);
        let mut line = Vec::new();
        let read = tokio::time::timeout(
            Duration::from_secs(READ_SECONDS),
            reader.read_until(b'\n', &mut line),
        )
        .await;
        let request: Option<Value> = match read {
            Ok(Ok(_)) if line.len() <= CONTROL_LINE_LIMIT && line.ends_with(b"\n") => {
                serde_json::from_slice(&line).ok()
            }
            _ => None,
        };
        let identity = request
            .as_ref()
            .and_then(|request| request.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let response = match request.as_ref().and_then(Value::as_object) {
            Some(fields)
                if !identity.is_empty()
                    && fields.get("method").is_some_and(Value::is_string)
                    && fields.get("params").is_none_or(Value::is_object) =>
            {
                let params = fields.get("params").cloned().unwrap_or_else(|| json!({}));
                match self
                    .call(fields["method"].as_str().unwrap_or_default(), &params)
                    .await
                {
                    Ok(result) => json!({"id": identity, "result": result}),
                    Err(error) => {
                        json!({"id": identity, "error": {"code": error.code, "message": error.message}})
                    }
                }
            }
            _ => {
                json!({"id": identity, "error": {"code": "invalid_request", "message": "The browser request is invalid"}})
            }
        };
        let mut bytes = serde_json::to_vec(&response).unwrap_or_default();
        bytes.push(b'\n');
        drop(
            tokio::time::timeout(Duration::from_secs(READ_SECONDS), writer.write_all(&bytes)).await,
        );
    }
}

/// Whether the sign-in of a request that ended finished: the command took a relayed result,
/// the browser of this Mac had the page, a device page opened, or the person approved the
/// card and its command then stopped listening.
fn signed_in(item: &Value) -> bool {
    match item["state"].as_str() {
        Some("closed") => matches!(
            item["closeReason"].as_str(),
            Some("completed" | LOCAL_CLOSE_REASON)
        ),
        Some("opened") => true,
        Some("stale") => item.get("lease").is_some_and(Value::is_object),
        _ => false,
    }
}

/// The new deadline of a pending request whose command still listens, or `None` to keep its
/// deadline (`listening_deadline`): one request time from now, at most `OPEN_LIMIT_SECONDS`
/// from its creation. The deadline moves when it is at most `EXTEND_BEFORE_SECONDS` away, so
/// a listening request changes about once in five minutes, not at every check.
fn listening_deadline(item: &Value, now: f64) -> Option<f64> {
    let created = item.get("createdAt").and_then(Value::as_f64)?;
    let expires = item.get("expiresAt").and_then(Value::as_f64)?;
    let limit = created + OPEN_LIMIT_SECONDS;
    let rolled = (now + TTL_SECONDS).min(limit);
    (rolled > expires && (rolled - expires >= EXTEND_BEFORE_SECONDS || rolled >= limit))
        .then_some(rolled)
}

fn item_state(queue: &Queue, params: &Value) -> Result<(String, String), BridgeError> {
    let Some(id) = params.get("id").and_then(Value::as_str) else {
        return bridge_error("not_found", "The sign-in request does not exist");
    };
    let Some(item) = queue.items.get(id) else {
        return bridge_error("not_found", "The sign-in request does not exist");
    };
    Ok((
        id.to_owned(),
        item["state"].as_str().unwrap_or_default().to_owned(),
    ))
}

/// The running host bridge: its two sockets and its check loop.
pub struct HostBridgeService {
    pub bridge: Arc<HostBridge>,
    pub open_socket: PathBuf,
    pub control_socket: PathBuf,
    stop: watch::Sender<bool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

fn bind_private(path: &Path) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    drop(std::fs::remove_file(path));
    let listener = UnixListener::bind(path)?;
    // The folder is private; only this user's processes reach the socket.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

fn serve<F, Fut>(
    listener: UnixListener,
    mut stop: watch::Receiver<bool>,
    handler: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn(UnixStream) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut clients = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = stop.wait_for(|stopped| *stopped) => break,
                accepted = listener.accept() => {
                    if let Ok((stream, _)) = accepted {
                        clients.spawn(handler(stream));
                    }
                }
                Some(_) = clients.join_next(), if !clients.is_empty() => {}
            }
        }
        // A long poll of a stopped server must not hold the shutdown.
        clients.abort_all();
        while clients.join_next().await.is_some() {}
    })
}

impl HostBridgeService {
    /// Start the host bridge in `<state>/browser-host` and write its launcher.
    pub fn start(
        state_root: &Path,
        chats: Arc<dyn ChatLookup>,
        options: HostBridgeOptions,
        launcher: Option<&Path>,
    ) -> std::io::Result<Self> {
        let folder = state_root.join(FOLDER);
        private_folder(&folder)?;
        if let Some(executable) = launcher {
            write_launcher(&folder, executable)?;
        }
        let bridge = HostBridge::open(&folder, chats, options)?;
        let open_socket = folder.join(OPEN_SOCKET);
        let control_socket = folder.join(CONTROL_SOCKET);
        let open_listener = bind_private(&open_socket)?;
        let control_listener = bind_private(&control_socket)?;
        let (stop, stopped) = watch::channel(false);
        let mut tasks = Vec::new();
        let open_bridge = Arc::clone(&bridge);
        tasks.push(serve(open_listener, stopped.clone(), move |stream| {
            Arc::clone(&open_bridge).launcher_client(stream)
        }));
        let control_bridge = Arc::clone(&bridge);
        tasks.push(serve(control_listener, stopped.clone(), move |stream| {
            Arc::clone(&control_bridge).control_client(stream)
        }));
        let check_bridge = Arc::clone(&bridge);
        let mut check_stop = stopped;
        tasks.push(tokio::spawn(async move {
            loop {
                if *check_stop.borrow() {
                    break;
                }
                let stopped = tokio::select! {
                    changed = check_stop.changed() => changed.is_err(),
                    () = tokio::time::sleep(Duration::from_secs(CHECK_SECONDS)) => false,
                };
                if stopped || *check_stop.borrow() {
                    break;
                }
                check_bridge.check().await;
            }
        }));
        Ok(Self {
            bridge,
            open_socket,
            control_socket,
            stop,
            tasks,
        })
    }

    /// Stop both sockets and the check loop.
    pub async fn stop(mut self) {
        self.stop.send_replace(true);
        for task in self.tasks.drain(..) {
            drop(task.await);
        }
        drop(std::fs::remove_file(&self.control_socket));
        drop(std::fs::remove_file(&self.open_socket));
    }
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

/// Write the launcher `<folder>/studio-browser`, its `PATH` links and, on macOS, the `open`
/// shim (`OPEN_SHIM`). The launcher runs this server's binary as the shim, so it needs no
/// Python. Tools split `BROWSER` into words, so the launcher path is the value and the binary
/// path stays inside the script.
pub fn write_launcher(folder: &Path, executable: &Path) -> std::io::Result<PathBuf> {
    let launcher = folder.join(LAUNCHER);
    let script = format!(
        "#!/bin/sh\nexec {} {SHIM_ARGUMENT} \"$@\"\n",
        shell_quote(&executable.to_string_lossy())
    );
    write_atomic(&launcher, script.as_bytes(), 0o755)?;
    let bin = folder.join(PATH_FOLDER);
    private_folder(&bin)?;
    // Relative links, as the Python server writes them, so a moved state folder keeps them.
    let target = Path::new("..").join(LAUNCHER);
    for name in PATH_NAMES {
        let link = bin.join(name);
        if std::fs::read_link(&link).ok().as_deref() != Some(target.as_path()) {
            let temporary = bin.join(format!(".{name}-{}", uuid::Uuid::new_v4().simple()));
            std::os::unix::fs::symlink(&target, &temporary)?;
            std::fs::rename(&temporary, &link)?;
        }
    }
    if cfg!(target_os = "macos") {
        write_atomic(&bin.join(OPEN_NAME), OPEN_SHIM.as_bytes(), 0o755)?;
    }
    Ok(launcher)
}

/// The environment that sends a native chat's browser links to this server.
pub fn session_environment(state_root: &Path) -> BTreeMap<String, String> {
    let folder = state_root.join(FOLDER);
    BTreeMap::from([
        (
            "BROWSER".to_owned(),
            folder.join(LAUNCHER).to_string_lossy().into_owned(),
        ),
        (
            "STUDIO_BROWSER_SOCKET".to_owned(),
            folder.join(OPEN_SOCKET).to_string_lossy().into_owned(),
        ),
        (
            "STUDIO_BROWSER_PATH".to_owned(),
            folder.join(PATH_FOLDER).to_string_lossy().into_owned(),
        ),
    ])
}

/// Give a Claude bridge the session environment (`codex_browser_host.publish`). The bridge
/// reads this file when it starts a session; its own process environment stays unchanged.
pub fn publish(bridge_root: &Path, state_root: &Path) -> std::io::Result<()> {
    let environment = session_environment(state_root);
    let mut text = serde_json::to_string(&environment).map_err(std::io::Error::other)?;
    text.push('\n');
    std::fs::create_dir_all(bridge_root)?;
    write_atomic(&bridge_root.join(SESSION_FILE), text.as_bytes(), 0o600)
}

/// The launcher: `studio-runtime-server --studio-browser URL` (`studio_browser.py`). It sends
/// the link to the bridge and exits 0 when Studio shows the person a card, 1 when the link is
/// refused or Studio is unavailable, and 2 for a wrong argument count. Returns None when the
/// arguments do not ask for the launcher.
pub fn shim_main(arguments: impl IntoIterator<Item = OsString>) -> Option<i32> {
    let arguments: Vec<OsString> = arguments.into_iter().collect();
    if arguments.get(1).map(OsString::as_os_str) != Some(std::ffi::OsStr::new(SHIM_ARGUMENT)) {
        return None;
    }
    let links: Vec<String> = arguments[2..]
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect();
    let socket = std::env::var("STUDIO_BROWSER_SOCKET")
        .ok()
        .filter(|value| !value.is_empty());
    // A VM chat names itself with LAYR_SESSION; a native chat with its session.
    let session = std::env::var("LAYR_SESSION")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::var("STUDIO_BROWSER_SESSION")
                .ok()
                .filter(|value| !value.is_empty())
        });
    let (code, message) = shim(
        &links,
        socket.as_deref().unwrap_or(GUEST_SOCKET),
        session.as_deref(),
        &open_on_this_mac,
    );
    eprintln!("{message}");
    Some(code)
}

/// Open one link in the default browser of this Mac with the system `open`, as a tool does
/// without Studio. Only a bridge with `local_open` asks for it. Returns whether `open` ran
/// and exited 0.
fn open_on_this_mac(link: &str) -> bool {
    const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
    const OPEN_OUTPUT_LIMIT_BYTES: usize = 64 * 1024;
    crate::bounded_child::run(crate::bounded_child::Request {
        program: std::ffi::OsStr::new("/usr/bin/open"),
        git_directory: None,
        args: &[link],
        environment: &[],
        input: None,
        stdout_limit: OPEN_OUTPUT_LIMIT_BYTES,
        stderr_limit: OPEN_OUTPUT_LIMIT_BYTES,
        timeout: OPEN_TIMEOUT,
    })
    .is_ok()
}

/// The launcher's exchange, without process state, for tests. `open` opens a link in the
/// browser of this Mac when the bridge asks for it.
pub fn shim(
    links: &[String],
    socket: &str,
    session: Option<&str>,
    open: &dyn Fn(&str) -> bool,
) -> (i32, String) {
    use std::io::{Read, Write};
    let [link] = links else {
        return (2, "studio-browser: give exactly one URL".into());
    };
    let request = json!({"v": 1, "url": link, "session": session});
    let exchange = || -> std::io::Result<Value> {
        let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(15)))?;
        stream.set_write_timeout(Some(Duration::from_secs(15)))?;
        let mut bytes = serde_json::to_vec(&request)?;
        bytes.push(b'\n');
        stream.write_all(&bytes)?;
        let mut data = Vec::new();
        let mut block = [0u8; 65536];
        while !data.ends_with(b"\n") && data.len() <= 65536 {
            let count = stream.read(&mut block)?;
            if count == 0 {
                break;
            }
            data.extend_from_slice(&block[..count]);
        }
        serde_json::from_slice(&data).map_err(std::io::Error::other)
    };
    let reply = match exchange() {
        Ok(reply) => reply,
        Err(error) => {
            let kind = if error
                .get_ref()
                .is_some_and(|inner| inner.is::<serde_json::Error>())
            {
                "ValueError"
            } else {
                "OSError"
            };
            return (
                1,
                format!("studio-browser: Studio is not available here ({kind})"),
            );
        }
    };
    let opened = reply.get("open") == Some(&Value::Bool(true)) && open(link);
    if reply.get("ok") != Some(&Value::Bool(true)) {
        let message = reply
            .get("message")
            .and_then(Value::as_str)
            .filter(|message| !message.is_empty())
            .unwrap_or("Studio refused the link");
        if opened {
            return (
                0,
                format!(
                    "studio-browser: Opened the page in the browser of this computer. Studio shows no sign-in card: {message}"
                ),
            );
        }
        return (1, format!("studio-browser: {message}"));
    }
    let host = match reply.get("host") {
        Some(Value::String(host)) => host.clone(),
        _ => "None".into(),
    };
    if reply.get("joined") == Some(&Value::Bool(true)) {
        return (
            0,
            format!(
                "studio-browser: Another chat already asks the person to sign in at {host}. This command waits on that card; Studio tells this chat when the sign-in ends."
            ),
        );
    }
    if opened {
        return (
            0,
            format!(
                "studio-browser: Opened the page in the browser of this computer. Studio also shows the person a sign-in card for {host}, so the person can answer from another window or a phone."
            ),
        );
    }
    (
        0,
        format!(
            "studio-browser: Studio asks the person to open {host}. The sign-in continues in the browser on this computer after the approval."
        ),
    )
}

#[cfg(test)]
#[path = "browser_host_lifetime_tests.rs"]
mod lifetime_tests;

#[cfg(test)]
#[path = "browser_host_local_open_tests.rs"]
mod local_open_tests;

#[cfg(test)]
#[path = "browser_host_join_tests.rs"]
mod join_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_and_lsof_rows_name_owners() {
        let proc = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid\n   0: 0100007F:1F40 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1010 0 1 1\n";
        assert_eq!(
            parse_proc_listeners(proc),
            vec![Listener {
                address: "127.0.0.1".into(),
                port: 8000,
                uid: 1010
            }]
        );
        let lsof =
            "p12\nu501\nf3\ntIPv4\nn127.0.0.1:8000\nf4\ntIPv6\nn*:18000\np13\nu0\ntIPv4\nn*:9000\n";
        assert_eq!(
            parse_lsof_listeners(lsof),
            vec![
                Listener {
                    address: "127.0.0.1".into(),
                    port: 8000,
                    uid: 501
                },
                Listener {
                    address: "::".into(),
                    port: 18000,
                    uid: 501
                },
                Listener {
                    address: "0.0.0.0".into(),
                    port: 9000,
                    uid: 0
                },
            ]
        );
    }

    #[test]
    fn another_owner_on_a_reachable_address_refuses_the_port() {
        let rows = vec![
            Listener {
                address: "127.0.0.1".into(),
                port: 8000,
                uid: 5,
            },
            Listener {
                address: "::".into(),
                port: 8000,
                uid: 6,
            },
        ];
        assert_eq!(
            connect_address(5, 8000, &rows).expect_err("refused").code,
            "forbidden"
        );
        assert_eq!(
            connect_address(5, 8001, &rows).expect_err("refused").code,
            "stale"
        );
        assert_eq!(
            connect_address(5, 8000, &rows[..1]).expect("own listener"),
            "127.0.0.1"
        );
        let v6 = vec![Listener {
            address: "::1".into(),
            port: 8000,
            uid: 5,
        }];
        assert_eq!(connect_address(5, 8000, &v6).expect("own listener"), "::1");
    }

    #[test]
    fn launcher_refuses_a_wrong_argument_count() {
        let never = |_: &str| -> bool { panic!("nothing opens without a bridge") };
        assert_eq!(shim(&[], "/nonexistent", None, &never).0, 2);
        assert_eq!(
            shim(
                &["https://a.example/".into()],
                "/nonexistent/open.sock",
                None,
                &never
            )
            .0,
            1
        );
    }

    struct NoChats;
    impl ChatLookup for NoChats {
        fn chat<'a>(
            &'a self,
            _session: &'a str,
        ) -> futures_util::future::BoxFuture<'a, Option<ChatContext>> {
            Box::pin(async { None })
        }
    }

    /// A saved card with a loopback redirect in `state`.
    fn saved_card(state: &str) -> Value {
        json!({
            "id": "card", "agentId": "agent", "uid": 501, "state": state,
            "url": "https://id.example.com/authorize", "startUrl": null,
            "redirect": {"host": "localhost", "port": 4242, "path": "/"},
            "createdAt": 100.0, "expiresAt": 100.0 + TTL_SECONDS, "seq": 1
        })
    }

    /// One saved card, read by a bridge whose listener probe fails. The clock
    /// is in seconds.
    fn bridge_with_failed_probe(
        folder: &Path,
        card: Value,
        now: Arc<std::sync::atomic::AtomicU64>,
    ) -> Arc<HostBridge> {
        let directory = folder.join("browser");
        std::fs::create_dir_all(&directory).expect("browser folder");
        std::fs::write(
            directory.join("requests.json"),
            json!({"seq": 1, "items": {"card": card}}).to_string(),
        )
        .expect("saved queue");
        let clock = Arc::clone(&now);
        HostBridge::open(
            folder,
            Arc::new(NoChats),
            HostBridgeOptions {
                // The probe of a loaded host failed: lsof timed out.
                listeners: Arc::new(|| None),
                http_providers: BTreeSet::new(),
                clock: Arc::new(move || {
                    f64::from_bits(clock.load(std::sync::atomic::Ordering::SeqCst))
                }),
                local_open: false,
            },
        )
        .expect("bridge")
    }

    #[tokio::test]
    async fn a_failed_listener_probe_keeps_an_open_card_and_answers_retryably() {
        let folder = tempfile::tempdir().expect("bridge folder");
        let now = Arc::new(std::sync::atomic::AtomicU64::new(200.0_f64.to_bits()));
        let bridge =
            bridge_with_failed_probe(folder.path(), saved_card("pending"), Arc::clone(&now));
        let state = |bridge: &HostBridge| bridge.lock().items["card"]["state"].clone();

        bridge.check().await;
        assert_eq!(
            state(&bridge),
            "pending",
            "an unknown probe is not a stale card"
        );
        let answered = bridge
            .answer(&json!({"id": "card", "decision": "approve"}))
            .await
            .expect_err("approval needs a known listener");
        assert_eq!(answered.code, NOT_SENT_CODE, "{}", answered.message);
        assert_eq!(state(&bridge), "pending", "the card stays open for a retry");

        // Expiry by time still applies while the probe fails.
        now.store(
            (101.0 + TTL_SECONDS).to_bits(),
            std::sync::atomic::Ordering::SeqCst,
        );
        bridge.check().await;
        assert_eq!(state(&bridge), "expired");
    }

    #[tokio::test]
    async fn a_failed_listener_probe_relays_nothing_and_says_so() {
        let folder = tempfile::tempdir().expect("bridge folder");
        let mut card = saved_card("approved");
        card["lease"] = json!({
            "host": "localhost", "port": 4242, "path": "/", "expiresAt": 100.0 + TTL_SECONDS
        });
        let now = Arc::new(std::sync::atomic::AtomicU64::new(200.0_f64.to_bits()));
        let bridge = bridge_with_failed_probe(folder.path(), card, now);
        let refused = bridge
            .callback(&json!({"id": "card", "target": "/?code=fake-code&state=S1"}))
            .await
            .expect_err("a relay needs a known listener");
        assert_eq!(refused.code, NOT_SENT_CODE, "{}", refused.message);
        let item = bridge.lock().items["card"].clone();
        assert_eq!(item["state"], "approved", "the forward stays open");
        assert!(item.get("relays").is_none(), "nothing was relayed");
    }

    #[test]
    fn absolute_location_is_kept_and_relative_is_joined() {
        assert_eq!(
            join_location("http://localhost:8000/", "https://id.example.com/a?b=c"),
            "https://id.example.com/a?b=c"
        );
        assert_eq!(
            join_location("http://localhost:8000/x", "/y"),
            "http://localhost:8000/y"
        );
    }
}
