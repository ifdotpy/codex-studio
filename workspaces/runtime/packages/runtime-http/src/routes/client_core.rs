//! Hosted client core: the server runs one `studio-client-core` for each
//! browser device, so a plain browser page uses the same core as the desktop
//! app. See docs/desktop/hosted-client-core.md.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde_json::Value;
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

struct DeviceCore {
    client: Client,
    /// Only the core event callback holds the sender, so streams end when the core closes.
    events: broadcast::WeakSender<Value>,
    activity: Arc<Mutex<Activity>>,
}

impl DeviceCore {
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
        self.wait_for_previous_core(device).await?;
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
            match lock.try_lock() {
                Ok(()) => return lock.unlock().map_err(|_| ApiError::internal()),
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

fn cookie_device(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == CLIENT_DEVICE_COOKIE)
        .map(|(_, value)| value.to_owned())
        .filter(|value| {
            value.len() == 32
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
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
    let response = core.client.request(request).await;
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
    drop(core);
    let stream =
        futures_util::stream::unfold((receiver, active), |(mut receiver, active)| async move {
            // A lagged stream ends: the page reconnects and subscribes again.
            let value = receiver.recv().await.ok()?;
            let event = Event::default().data(value.to_string());
            Some((Ok::<_, Infallible>(event), (receiver, active)))
        });
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
        let mut first_events = events(&app, &first).await;
        let mut second_events = events(&app, &second).await;
        let subscribe = |subscription: &str| json!({"subscription": subscription, "topic": "outbox", "workspace": "workspace"});
        let (_, reply) = call(
            &app,
            Some(&second),
            "second-view",
            "view.subscribe",
            subscribe("second-view"),
        )
        .await;
        assert!(reply["error"].is_null(), "{reply}");
        assert_eq!(next_event(&mut second_events).await["event"], "second-view");
        let (_, reply) = call(
            &app,
            Some(&first),
            "first-view",
            "view.subscribe",
            subscribe("first-view"),
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
