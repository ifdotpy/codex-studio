//! Hosted client core: the server runs one `studio-client-core` for each
//! browser device, so a plain browser page uses the same core as the desktop
//! app. See docs/desktop/hosted-client-core.md.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde_json::{Value, json};
use studio_client_core::client::Client;
use studio_client_core::native::{self, ConnectionConfig};
use studio_client_core::protocol::{Request, Response as CoreResponse};
use tokio::sync::broadcast;
use tokio::time::Instant;

use super::extractors::ApiJson;
use super::manifest::{OperationStatus, RouteGroup, get, post};
use crate::error::ApiError;
use crate::router::HttpState;

/// Name of the HttpOnly cookie that holds the browser device identity.
pub(crate) const CLIENT_DEVICE_COOKIE: &str = "studio_client";
/// Time after the last request or event stream of a device before its core closes, in seconds.
pub(crate) const CLIENT_CORE_IDLE_SECONDS: u64 = 600;
/// Largest number of device cores that one server keeps open at the same time.
pub(crate) const CLIENT_CORE_MAX_OPEN: usize = 16;
/// Browser lifetime of the device cookie, in seconds (400 days, the longest that browsers keep).
const CLIENT_DEVICE_COOKIE_MAX_AGE_SECONDS: u64 = 34_560_000;
/// Core events buffered for each device event stream, in events. A slower stream closes.
const CLIENT_EVENT_BUFFER: usize = 1024;
/// Interval between keep-alive comments on an idle device event stream, in seconds.
const CLIENT_EVENT_KEEP_ALIVE_SECONDS: u64 = 15;
/// Longest wait for the closing core of a device to release its state, in milliseconds.
const CLIENT_CORE_CLOSE_WAIT_MILLISECONDS: u64 = 5_000;
/// Pause between two checks of the state lock of a closing core, in milliseconds.
const CLIENT_CORE_CLOSE_CHECK_MILLISECONDS: u64 = 20;
/// Time after the last use of a device before the server deletes its state folder, in seconds (90 days).
const CLIENT_STATE_RETENTION_SECONDS: u64 = 7_776_000;
/// Folder below the server state directory that holds one core state folder per device.
const CLIENT_STATE_FOLDER: &str = "web-clients";
/// Error code of a command that needs the device of the person.
const UNSUPPORTED_HERE: &str = "unsupported_here";
const UNSUPPORTED_MESSAGE: &str =
    "This action needs your device. Open Studio in the desktop app on your device to use it.";

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post("/api/client/request", OperationStatus::Implemented, request),
        get("/api/client/events", OperationStatus::Implemented, events),
    ])
}

/// Time of the last use and the number of requests and streams in progress.
struct Activity {
    active: usize,
    used: Instant,
}

struct ActivityGuard(Arc<Mutex<Activity>>);

impl ActivityGuard {
    fn new(activity: &Arc<Mutex<Activity>>) -> Self {
        let mut value = activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        value.active += 1;
        value.used = Instant::now();
        Self(Arc::clone(activity))
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        let mut value = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        value.active -= 1;
        value.used = Instant::now();
    }
}

/// Views belong to the event stream connection that delivers their events.
#[derive(Default)]
struct Views {
    connections: HashSet<String>,
    /// Subscription identity to the connection that owns it.
    owners: HashMap<String, String>,
}

struct DeviceCore {
    client: Client,
    /// Only the core event callback holds the sender, so streams end when the core closes.
    events: broadcast::WeakSender<Value>,
    activity: Arc<Mutex<Activity>>,
    /// Held while the server changes views, so their core commands keep their order.
    views: tokio::sync::Mutex<Views>,
}

fn core_request(method: &str, params: Value) -> Request {
    Request {
        id: uuid::Uuid::new_v4().to_string(),
        method: method.to_owned(),
        params,
    }
}

impl DeviceCore {
    async fn end_view(&self, subscription: &str, reason: &str) -> Result<(), String> {
        self.client
            .request(core_request(
                "view.unsubscribe",
                json!({"subscription": subscription, "reason": reason}),
            ))
            .await
            .map(drop)
    }

    /// Runs `view.subscribe` for the open connection that the page names.
    async fn subscribe(&self, mut request: Request) -> Result<CoreResponse, String> {
        let connection = request
            .params
            .as_object_mut()
            .and_then(|params| params.remove("connection"))
            .and_then(|value| value.as_str().map(str::to_owned));
        let subscription = request.params["subscription"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        let mut views = self.views.lock().await;
        let Some(connection) = connection.filter(|id| views.connections.contains(id)) else {
            return Ok(CoreResponse::failure(
                request.id,
                "invalid_subscription",
                "The event stream of this view is closed. Subscribe again after it reconnects.",
            ));
        };
        if views
            .owners
            .get(&subscription)
            .is_some_and(|owner| *owner != connection)
        {
            self.end_view(&subscription, "moved to a new event stream")
                .await?;
            views.owners.remove(&subscription);
        }
        let response = self.client.request(request).await?;
        if response.error.is_none() {
            views.owners.insert(subscription, connection);
        }
        Ok(response)
    }

    async fn unsubscribe(&self, request: Request) -> Result<CoreResponse, String> {
        let mut views = self.views.lock().await;
        if let Some(subscription) = request.params["subscription"].as_str() {
            views.owners.remove(subscription);
        }
        self.client.request(request).await
    }

    /// Ends every view of a closed event stream connection.
    async fn end_connection(&self, connection: &str) {
        let mut views = self.views.lock().await;
        views.connections.remove(connection);
        let owned = views
            .owners
            .iter()
            .filter(|(_, owner)| *owner == connection)
            .map(|(subscription, _)| subscription.clone())
            .collect::<Vec<_>>();
        for subscription in owned {
            views.owners.remove(&subscription);
            // A failed core closes its views with it.
            drop(self.end_view(&subscription, "event stream closed").await);
        }
    }

    fn idle_for(&self) -> Duration {
        let activity = self
            .activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if activity.active > 0 {
            Duration::ZERO
        } else {
            activity.used.elapsed()
        }
    }
}

/// The open device cores of this server.
pub(crate) struct ClientCores {
    open: tokio::sync::Mutex<HashMap<String, Arc<DeviceCore>>>,
    directory: PathBuf,
    backend_origin: String,
    idle: Duration,
}

impl ClientCores {
    pub(crate) fn new(state_dir: &str, port: u16) -> Self {
        Self::with_idle(
            state_dir,
            port,
            Duration::from_secs(CLIENT_CORE_IDLE_SECONDS),
        )
    }

    fn with_idle(state_dir: &str, port: u16, idle: Duration) -> Self {
        let directory = PathBuf::from(state_dir).join(CLIENT_STATE_FOLDER);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let folder = directory.clone();
            // Server start: no core is open yet.
            runtime.spawn(async move { delete_unused(&folder, &HashMap::new()).await });
        }
        Self {
            open: tokio::sync::Mutex::default(),
            directory: PathBuf::from(state_dir).join(CLIENT_STATE_FOLDER),
            backend_origin: format!("http://127.0.0.1:{port}/"),
            idle,
        }
    }

    async fn known(&self, device: &str) -> Result<bool, ApiError> {
        if self.open.lock().await.contains_key(device) {
            return Ok(true);
        }
        tokio::fs::try_exists(self.directory.join(device))
            .await
            .map_err(|_| ApiError::internal())
    }

    async fn create(&self) -> Result<String, ApiError> {
        let device = uuid::Uuid::new_v4().simple().to_string();
        let mut builder = tokio::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(self.directory.join(&device))
            .await
            .map_err(|_| ApiError::internal())?;
        Ok(device)
    }

    /// The open core of `device`, marked active while the guard lives. The mark is
    /// set under the map lock, so the idle close can not remove a core in use.
    async fn core(
        self: &Arc<Self>,
        device: &str,
    ) -> Result<(Arc<DeviceCore>, ActivityGuard), ApiError> {
        if let Some(core) = self.open.lock().await.get(device) {
            let active = ActivityGuard::new(&core.activity);
            return Ok((Arc::clone(core), active));
        }
        // Only this device waits for its previous core; other devices keep working.
        self.wait_for_previous_core(device).await?;
        let mut open = self.open.lock().await;
        if let Some(core) = open.get(device) {
            let active = ActivityGuard::new(&core.activity);
            return Ok((Arc::clone(core), active));
        }
        if open.len() >= CLIENT_CORE_MAX_OPEN {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "This server has too many open browser sessions. Close Studio in another browser and try again.",
            ));
        }
        delete_unused(&self.directory, &open).await;
        let (sender, _) = broadcast::channel(CLIENT_EVENT_BUFFER);
        let events = sender.downgrade();
        let client = Client::open(
            ConnectionConfig {
                state_directory: self.directory.join(device),
                backend_origin: self.backend_origin.clone(),
            },
            move |event| {
                // No open stream means no reader; the page subscribes again after it reconnects.
                drop(sender.send(event));
            },
        )
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "The browser session could not start. Try again.",
            )
        })?;
        let core = Arc::new(DeviceCore {
            client,
            events,
            activity: Arc::new(Mutex::new(Activity {
                active: 0,
                used: Instant::now(),
            })),
            views: tokio::sync::Mutex::default(),
        });
        let active = ActivityGuard::new(&core.activity);
        open.insert(device.to_owned(), Arc::clone(&core));
        self.close_when_idle(device.to_owned(), Arc::downgrade(&core));
        Ok((core, active))
    }

    /// A closed core releases its state lock when its thread stops. Opening a new
    /// core before that would leave the new core without its state.
    async fn wait_for_previous_core(&self, device: &str) -> Result<(), ApiError> {
        let busy = || {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "The browser session is still closing. Try again.",
            )
        };
        let lock = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.directory.join(device).join(native::STATE_LOCK_FILE))
            .await
            .map_err(|_| ApiError::internal())?
            .into_std()
            .await;
        let deadline = Instant::now() + Duration::from_millis(CLIENT_CORE_CLOSE_WAIT_MILLISECONDS);
        loop {
            if self.open.lock().await.contains_key(device) {
                return Ok(());
            }
            match lock.try_lock() {
                Ok(()) => {
                    // The lock file time records the last use of the device state.
                    drop(lock.set_modified(std::time::SystemTime::now()));
                    return lock.unlock().map_err(|_| ApiError::internal());
                }
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(CLIENT_CORE_CLOSE_CHECK_MILLISECONDS))
                        .await;
                }
                Err(_) => return Err(busy()),
            }
        }
    }

    /// Removes the core of `device` only when it is still `core`.
    async fn close(&self, device: &str, core: &Arc<DeviceCore>) {
        let mut open = self.open.lock().await;
        if open
            .get(device)
            .is_some_and(|current| Arc::ptr_eq(current, core))
        {
            open.remove(device);
            touch(&self.directory, device).await;
        }
    }

    fn close_when_idle(self: &Arc<Self>, device: String, core: Weak<DeviceCore>) {
        let cores = Arc::downgrade(self);
        let idle = self.idle;
        tokio::spawn(async move {
            let mut wait = idle;
            loop {
                tokio::time::sleep(wait).await;
                let Some(cores) = cores.upgrade() else { return };
                let mut open = cores.open.lock().await;
                let Some(current) = open.get(&device) else {
                    return;
                };
                if !std::ptr::eq(Arc::as_ptr(current), core.as_ptr()) {
                    return;
                }
                let idle_for = current.idle_for();
                if idle_for >= idle {
                    open.remove(&device);
                    touch(&cores.directory, &device).await;
                    return;
                }
                wait = idle - idle_for;
            }
        });
    }

    #[cfg(test)]
    async fn open_count(&self) -> usize {
        self.open.lock().await.len()
    }
}

fn valid_device(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Records the last use of a device state in its lock file time.
async fn touch(directory: &std::path::Path, device: &str) {
    if let Ok(file) = tokio::fs::OpenOptions::new()
        .write(true)
        .open(directory.join(device).join(native::STATE_LOCK_FILE))
        .await
    {
        drop(
            file.into_std()
                .await
                .set_modified(std::time::SystemTime::now()),
        );
    }
}

/// Deletes the state folders of closed devices unused for `CLIENT_STATE_RETENTION_SECONDS`.
async fn delete_unused(directory: &std::path::Path, open: &HashMap<String, Arc<DeviceCore>>) {
    let Ok(mut entries) = tokio::fs::read_dir(directory).await else {
        return;
    };
    let retention = Duration::from_secs(CLIENT_STATE_RETENTION_SECONDS);
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !valid_device(&name) || open.contains_key(&name) {
            continue;
        }
        let path = entry.path();
        let used = match tokio::fs::metadata(path.join(native::STATE_LOCK_FILE)).await {
            Ok(metadata) => metadata.modified().ok(),
            Err(_) => tokio::fs::metadata(&path)
                .await
                .ok()
                .and_then(|metadata| metadata.modified().ok()),
        };
        if used.is_some_and(|time| time.elapsed().is_ok_and(|age| age >= retention)) {
            drop(tokio::fs::remove_dir_all(&path).await);
        }
    }
}

fn cookie_device(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == CLIENT_DEVICE_COOKIE)
        .map(|(_, value)| value.to_owned())
        .filter(|value| valid_device(value))
}

/// The device of this request, and a new cookie when the server made a new device.
async fn device(
    cores: &ClientCores,
    headers: &HeaderMap,
) -> Result<(String, Option<HeaderValue>), ApiError> {
    if let Some(device) = cookie_device(headers)
        && cores.known(&device).await?
    {
        return Ok((device, None));
    }
    let device = cores.create().await?;
    // The boundary accepts a forwarded request only from the HTTPS remote origin.
    let secure = if crate::middleware::forwarded(headers) {
        "; Secure"
    } else {
        ""
    };
    let cookie = HeaderValue::from_str(&format!(
        "{CLIENT_DEVICE_COOKIE}={device}; Path=/api/client; Max-Age={CLIENT_DEVICE_COOKIE_MAX_AGE_SECONDS}; HttpOnly; SameSite=Strict{secure}"
    ))
    .map_err(|_| ApiError::internal())?;
    Ok((device, Some(cookie)))
}

fn with_cookie(mut response: Response, cookie: Option<HeaderValue>) -> Response {
    if let Some(cookie) = cookie {
        response.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    response
}

async fn request(
    State(state): State<HttpState>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<Request>,
) -> Result<Response, ApiError> {
    let cores = &state.client_cores;
    let (device, cookie) = device(cores, &headers).await?;
    if native::device_only(&request) {
        let response = CoreResponse::failure(request.id, UNSUPPORTED_HERE, UNSUPPORTED_MESSAGE);
        return Ok(with_cookie(Json(response).into_response(), cookie));
    }
    let id = request.id.clone();
    let (core, active) = cores.core(&device).await?;
    let response = match request.method.as_str() {
        "view.subscribe" => core.subscribe(request).await,
        "view.unsubscribe" => core.unsubscribe(request).await,
        _ => core.client.request(request).await,
    };
    drop(active);
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            // The core connection failed; its message says when an outcome is unknown.
            cores.close(&device, &core).await;
            CoreResponse::failure(id, "client_core", error)
        }
    };
    Ok(with_cookie(Json(response).into_response(), cookie))
}

/// One open event stream. When it ends, its views end in the core.
struct Connection {
    core: Weak<DeviceCore>,
    id: String,
}

impl Drop for Connection {
    fn drop(&mut self) {
        let (core, id) = (self.core.clone(), std::mem::take(&mut self.id));
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Some(core) = core.upgrade() {
                    core.end_connection(&id).await;
                }
            });
        }
    }
}

async fn events(State(state): State<HttpState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let cores = &state.client_cores;
    let (device, cookie) = device(cores, &headers).await?;
    let (core, active) = cores.core(&device).await?;
    let receiver = core
        .events
        .upgrade()
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "The browser session closed. Try again.",
            )
        })?
        .subscribe();
    let connection = uuid::Uuid::new_v4().simple().to_string();
    core.views
        .lock()
        .await
        .connections
        .insert(connection.clone());
    let first = Event::default().data(json!({ "connection": connection }).to_string());
    let owner = Connection {
        core: Arc::downgrade(&core),
        id: connection,
    };
    drop(core);
    let stream = futures_util::stream::once(std::future::ready(Ok::<_, Infallible>(first))).chain(
        futures_util::stream::unfold(
            (receiver, active, owner),
            |(mut receiver, active, owner)| async move {
                // A lagged stream ends: the page reconnects and subscribes again.
                let value = receiver.recv().await.ok()?;
                let event = Event::default().data(value.to_string());
                Some((Ok::<_, Infallible>(event), (receiver, active, owner)))
            },
        ),
    );
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(CLIENT_EVENT_KEEP_ALIVE_SECONDS)))
        .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    Ok(with_cookie(response, cookie))
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "client_core",
        routes: Some(routes),
        dependencies: &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use futures_util::StreamExt;
    use serde_json::json;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use tower::ServiceExt;

    const PORT: u16 = 4620;

    fn state(idle: Duration) -> (HttpState, tempfile::TempDir) {
        let directory = tempfile::tempdir().expect("isolated state directory");
        let path = directory.path().to_string_lossy().into_owned();
        let mut state = HttpState::new(
            "token".to_owned(),
            "workspace".to_owned(),
            path.clone(),
            PORT,
            None,
            None,
            None,
        );
        state.client_cores = Arc::new(ClientCores::with_idle(&path, PORT, idle));
        (state, directory)
    }

    fn builder(method: &str, path: &str, cookie: Option<&str>) -> axum::http::request::Builder {
        let builder = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("host", format!("127.0.0.1:{PORT}"))
            .extension(ConnectInfo(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                12345,
            )));
        match cookie {
            Some(cookie) => builder.header(header::COOKIE, cookie),
            None => builder,
        }
    }

    async fn call(
        app: &axum::Router,
        cookie: Option<&str>,
        id: &str,
        method: &str,
        params: Value,
    ) -> (Option<String>, Value) {
        let request = builder("POST", "/api/client/request", cookie)
            .header("x-canvas-token", "token")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"id": id, "method": method, "params": params}).to_string(),
            ))
            .expect("request builds");
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .map(|value| value.to_str().expect("cookie text").to_owned());
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (cookie, serde_json::from_slice(&body).expect("JSON reply"))
    }

    fn pair(set_cookie: &str) -> String {
        set_cookie
            .split(';')
            .next()
            .expect("cookie pair")
            .to_owned()
    }

    async fn new_device(app: &axum::Router) -> String {
        let (cookie, reply) = call(app, None, "first", "preferences.raw_all", Value::Null).await;
        assert_eq!(reply["result"], json!({}));
        pair(&cookie.expect("a new device gets a cookie"))
    }

    async fn events(app: &axum::Router, cookie: &str) -> axum::body::BodyDataStream {
        let request = builder("GET", "/api/client/events", Some(cookie))
            .body(Body::empty())
            .expect("request builds");
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        response.into_body().into_data_stream()
    }

    async fn next_event(stream: &mut axum::body::BodyDataStream) -> Value {
        let chunk = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("event in time")
            .expect("open stream")
            .expect("bytes");
        let text = String::from_utf8(chunk.to_vec()).expect("text");
        let data = text
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("one data line");
        serde_json::from_str(data).expect("JSON event")
    }

    fn view(subscription: &str, connection: &str) -> Value {
        json!({"subscription": subscription, "topic": "outbox", "workspace": "workspace", "connection": connection})
    }

    /// Opens the event stream and reads its connection identity.
    async fn connect(app: &axum::Router, cookie: &str) -> (axum::body::BodyDataStream, String) {
        let mut stream = events(app, cookie).await;
        let first = next_event(&mut stream).await;
        let connection = first["connection"].as_str().expect("connection identity");
        (stream, connection.to_owned())
    }

    fn device_of(cookie: &str) -> &str {
        cookie.trim_start_matches("studio_client=")
    }

    /// Sends a command to the open core of a device directly, without the route.
    async fn direct(state: &HttpState, cookie: &str, method: &str, params: Value) -> CoreResponse {
        let core = state
            .client_cores
            .open
            .lock()
            .await
            .get(device_of(cookie))
            .cloned();
        let request = Request {
            id: uuid::Uuid::new_v4().to_string(),
            method: method.to_owned(),
            params,
        };
        core.expect("open core")
            .client
            .request(request)
            .await
            .expect("core reply")
    }

    async fn wait_until_closed(state: &HttpState) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while state.client_cores.open_count().await > 0 {
            assert!(Instant::now() < deadline, "the idle core did not close");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn a_closed_event_stream_leaves_no_views_in_the_core() {
        let (state, _directory) = state(Duration::from_secs(CLIENT_CORE_IDLE_SECONDS));
        let app = crate::router(state.clone()).expect("router");
        let cookie = new_device(&app).await;
        let (mut stream, connection) = connect(&app, &cookie).await;
        let (_, reply) = call(
            &app,
            Some(&cookie),
            "s",
            "view.subscribe",
            view("v", &connection),
        )
        .await;
        assert!(reply["error"].is_null(), "{reply}");
        assert_eq!(next_event(&mut stream).await["event"], "v");
        drop(stream);
        // The core accepts the same subscription again only after the old view ended.
        let params = json!({"subscription": "v", "topic": "outbox", "workspace": "workspace"});
        let deadline = Instant::now() + Duration::from_secs(10);
        while direct(&state, &cookie, "view.subscribe", params.clone())
            .await
            .error
            .is_some()
        {
            assert!(Instant::now() < deadline, "the view outlived its stream");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn a_view_needs_an_open_stream_of_its_own_device() {
        let (state, _directory) = state(Duration::from_secs(CLIENT_CORE_IDLE_SECONDS));
        let app = crate::router(state).expect("router");
        let first = new_device(&app).await;
        let second = new_device(&app).await;
        let (_stream, connection) = connect(&app, &first).await;
        for (cookie, params) in [
            (&second, view("v", &connection)),
            (&first, view("v", "unknown")),
            (
                &first,
                json!({"subscription": "v", "topic": "outbox", "workspace": "workspace"}),
            ),
        ] {
            let (_, reply) = call(&app, Some(cookie), "s", "view.subscribe", params).await;
            assert_eq!(reply["error"]["code"], "invalid_subscription", "{reply}");
        }
    }

    #[tokio::test]
    async fn a_page_view_keeps_its_events_after_the_stream_and_core_reopen() {
        let (state, _directory) = state(Duration::from_millis(200));
        let app = crate::router(state.clone()).expect("router");
        let cookie = new_device(&app).await;
        let (mut stream, connection) = connect(&app, &cookie).await;
        let (_, reply) = call(
            &app,
            Some(&cookie),
            "s1",
            "view.subscribe",
            view("v", &connection),
        )
        .await;
        assert!(reply["error"].is_null(), "{reply}");
        assert_eq!(next_event(&mut stream).await["event"], "v");
        drop(stream);
        wait_until_closed(&state).await;
        // The page reconnects and subscribes again with the same subscription identity.
        let (mut stream, reconnected) = connect(&app, &cookie).await;
        assert_ne!(reconnected, connection);
        let (_, reply) = call(
            &app,
            Some(&cookie),
            "s2",
            "view.subscribe",
            view("v", &reconnected),
        )
        .await;
        assert!(reply["error"].is_null(), "{reply}");
        assert_eq!(next_event(&mut stream).await["event"], "v");
    }

    #[tokio::test]
    async fn opening_a_core_deletes_old_unused_device_folders() {
        let (state, directory) = state(Duration::from_secs(CLIENT_CORE_IDLE_SECONDS));
        let root = directory.path().join(CLIENT_STATE_FOLDER);
        let (old, recent) = ("0".repeat(32), "1".repeat(32));
        for (device, age) in [
            (&old, CLIENT_STATE_RETENTION_SECONDS + 3600),
            (&recent, 3600),
        ] {
            tokio::fs::create_dir_all(root.join(device))
                .await
                .expect("folder");
            let lock = tokio::fs::File::create(root.join(device).join(native::STATE_LOCK_FILE))
                .await
                .expect("lock file")
                .into_std()
                .await;
            let used = std::time::SystemTime::now() - Duration::from_secs(age);
            lock.set_modified(used).expect("last use");
        }
        let app = crate::router(state).expect("router");
        let _cookie = new_device(&app).await;
        assert!(!tokio::fs::try_exists(root.join(&old)).await.expect("check"));
        assert!(
            tokio::fs::try_exists(root.join(&recent))
                .await
                .expect("check")
        );
    }

    #[tokio::test]
    async fn a_closing_core_delays_only_its_own_device() {
        let (state, directory) = state(Duration::from_secs(CLIENT_CORE_IDLE_SECONDS));
        let app = crate::router(state).expect("router");
        let other = new_device(&app).await;
        let closing = "2".repeat(32);
        let folder = directory.path().join(CLIENT_STATE_FOLDER).join(&closing);
        tokio::fs::create_dir_all(&folder).await.expect("folder");
        let lock = tokio::fs::File::create(folder.join(native::STATE_LOCK_FILE))
            .await
            .expect("lock file")
            .into_std()
            .await;
        lock.try_lock()
            .expect("the previous core still holds the state");
        let cookie = format!("studio_client={closing}");
        let waiting = tokio::spawn({
            let app = app.clone();
            async move {
                call(
                    &app,
                    Some(&cookie),
                    "wait",
                    "preferences.raw_all",
                    Value::Null,
                )
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        let started = Instant::now();
        let (_, reply) = call(
            &app,
            Some(&other),
            "other",
            "preferences.raw_all",
            Value::Null,
        )
        .await;
        assert_eq!(reply["result"], json!({}));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "another device waited"
        );
        lock.unlock().expect("release");
        let (_, reply) = waiting.await.expect("waiting request");
        assert_eq!(reply["result"], json!({}));
    }

    #[tokio::test]
    async fn client_routes_keep_the_server_trust_checks() {
        let (state, _directory) = state(Duration::from_secs(CLIENT_CORE_IDLE_SECONDS));
        let app = crate::router(state.clone()).expect("router");
        let body = json!({"id":"a","method":"preferences.raw_all"}).to_string();
        let no_token = builder("POST", "/api/client/request", None)
            .header("content-type", "application/json")
            .body(Body::from(body.clone()))
            .expect("request builds");
        let response = app.clone().oneshot(no_token).await.expect("response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        let cross_site = builder("GET", "/api/client/events", None)
            .header(header::ORIGIN, "http://example.invalid")
            .body(Body::empty())
            .expect("request builds");
        let response = app.clone().oneshot(cross_site).await.expect("response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let remote = axum::http::Request::builder()
            .method("GET")
            .uri("/api/client/events")
            .header("host", format!("127.0.0.1:{PORT}"))
            .extension(ConnectInfo(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
                12345,
            )))
            .body(Body::empty())
            .expect("request builds");
        let response = app.oneshot(remote).await.expect("response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(state.client_cores.open_count().await, 0);
    }

    #[tokio::test]
    async fn device_cookie_is_made_once_and_reused_by_the_same_core() {
        let (state, directory) = state(Duration::from_secs(CLIENT_CORE_IDLE_SECONDS));
        let app = crate::router(state.clone()).expect("router");
        let (set_cookie, reply) = call(
            &app,
            None,
            "write",
            "preferences.raw_set",
            json!({"key":"draft","raw":"kept"}),
        )
        .await;
        assert!(reply["error"].is_null(), "{reply}");
        let set_cookie = set_cookie.expect("a new device gets a cookie");
        for attribute in ["; Path=/api/client", "; HttpOnly", "; SameSite=Strict"] {
            assert!(set_cookie.contains(attribute), "{set_cookie}");
        }
        assert!(!set_cookie.contains("Secure"), "plain loopback HTTP");
        let cookie = pair(&set_cookie);
        let device = cookie.trim_start_matches("studio_client=");
        let folder = tokio::fs::metadata(directory.path().join("web-clients").join(device)).await;
        assert!(folder.is_ok_and(|metadata| metadata.is_dir()));
        let (again, reply) = call(
            &app,
            Some(&cookie),
            "read",
            "preferences.raw_all",
            Value::Null,
        )
        .await;
        assert!(again.is_none(), "a known device keeps its cookie");
        assert_eq!(reply, json!({"id":"read","result":{"draft":"kept"}}));
        // The page can not choose a device: an unknown identity gets a new one.
        let forged = format!("studio_client={}", "a".repeat(32));
        let (replaced, reply) = call(
            &app,
            Some(&forged),
            "other",
            "preferences.raw_all",
            Value::Null,
        )
        .await;
        assert_eq!(reply["result"], json!({}));
        assert_ne!(pair(&replaced.expect("new device")), forged);
        assert_eq!(state.client_cores.open_count().await, 2);
    }

    #[tokio::test]
    async fn device_events_reach_only_the_streams_of_that_device() {
        let (state, _directory) = state(Duration::from_secs(CLIENT_CORE_IDLE_SECONDS));
        let app = crate::router(state).expect("router");
        let first = new_device(&app).await;
        let second = new_device(&app).await;
        let (mut first_events, first_connection) = connect(&app, &first).await;
        let (mut second_events, second_connection) = connect(&app, &second).await;
        let (_, reply) = call(
            &app,
            Some(&second),
            "second-view",
            "view.subscribe",
            view("second-view", &second_connection),
        )
        .await;
        assert!(reply["error"].is_null(), "{reply}");
        assert_eq!(next_event(&mut second_events).await["event"], "second-view");
        let (_, reply) = call(
            &app,
            Some(&first),
            "first-view",
            "view.subscribe",
            view("first-view", &first_connection),
        )
        .await;
        assert!(reply["error"].is_null(), "{reply}");
        // The second device's earlier event never reaches the first device.
        assert_eq!(next_event(&mut first_events).await["event"], "first-view");
    }

    #[tokio::test]
    async fn device_only_commands_answer_unsupported_here_without_a_core() {
        let (state, _directory) = state(Duration::from_secs(CLIENT_CORE_IDLE_SECONDS));
        let app = crate::router(state.clone()).expect("router");
        for (id, method, params) in [
            ("capture", "media.captureStart", Value::Null),
            ("dialog", "desktop.pickDirectory", Value::Null),
            ("notify", "notifications.show", Value::Null),
            (
                "microphone",
                "view.subscribe",
                json!({"subscription":"m","topic":"microphone"}),
            ),
        ] {
            let (_, reply) = call(&app, None, id, method, params).await;
            assert_eq!(reply["id"], id);
            assert_eq!(reply["error"]["code"], UNSUPPORTED_HERE, "{method}");
            assert_eq!(reply["error"]["message"], UNSUPPORTED_MESSAGE);
        }
        assert_eq!(state.client_cores.open_count().await, 0);
    }

    #[tokio::test]
    async fn idle_core_closes_and_its_state_survives_the_next_open() {
        let idle = Duration::from_millis(200);
        let (state, _directory) = state(idle);
        let app = crate::router(state.clone()).expect("router");
        let cookie = new_device(&app).await;
        let (_, reply) = call(
            &app,
            Some(&cookie),
            "write",
            "preferences.raw_set",
            json!({"key":"draft","raw":"kept"}),
        )
        .await;
        assert!(reply["error"].is_null(), "{reply}");
        let stream = events(&app, &cookie).await;
        tokio::time::sleep(idle * 3).await;
        assert_eq!(
            state.client_cores.open_count().await,
            1,
            "an open event stream keeps the core"
        );
        drop(stream);
        let deadline = Instant::now() + Duration::from_secs(10);
        while state.client_cores.open_count().await > 0 {
            assert!(Instant::now() < deadline, "the idle core did not close");
            tokio::time::sleep(idle / 4).await;
        }
        let (_, reply) = call(
            &app,
            Some(&cookie),
            "read",
            "preferences.raw_all",
            Value::Null,
        )
        .await;
        assert_eq!(reply["result"], json!({"draft":"kept"}));
        assert_eq!(state.client_cores.open_count().await, 1);
    }
}
