//! Hidden fixture for the pinned framework event boundary. No OS input is used.
//! Compile only with native-check. The production event broker does not use this API.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use studio_client_core::protocol::{Request, Response};
use tauri::{Emitter, EventTarget, Listener, Manager, WebviewWindow};

pub(crate) const MODE: &str = "attachment-event-privacy";
const BEGIN: &str = "native_check.attachment_event_begin";
const FINISH: &str = "native_check.attachment_event_finish";
const FOREIGN_ATTEMPT: &str = "native_check.attachment_foreign_attempt";
const FRAME_DOCUMENT: &str = "/assets/native-attachment-privacy-frame.html";
const FRAME_SCRIPT: &str = "/assets/native-attachment-privacy-frame.js";
const FRAME_NAME_PREFIX: &str = "attachment-foreign-";
const FRAME_RESULT: &str = "native-attachment-frame-result";
const UI_CONNECT: &str = "ui.events.connect";
const UI_DISCONNECT: &str = "ui.events.disconnect";
static FOREIGN_ADMISSIONS: AtomicUsize = AtomicUsize::new(0);
const EVENTS: [&str; 2] = ["tauri://drag-enter", "tauri://drag-drop"];
const EMISSIONS: usize = 4;
const MARKER_PREFIX: &str = "attachment-event-marker-";
const BARRIER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn directory() -> Result<std::path::PathBuf, String> {
    if std::env::var("STUDIO_NATIVE_CHECK_MODE").as_deref() != Ok(MODE) {
        return Err("The native attachment privacy fixture is not active.".into());
    }
    let directory = crate::native_check::directory()
        .ok_or("The native attachment privacy fixture has no isolated directory.")?;
    if !directory.is_absolute() {
        return Err(
            "The native attachment privacy fixture requires an absolute isolated directory.".into(),
        );
    }
    Ok(directory)
}

/// native_check::loaded selects this script only for this isolated fixture mode.
pub(crate) fn script() -> String {
    format!(
        "(function(config){{\n{}\n}})({});",
        include_str!("attachment-event-privacy.js"),
        json!({
            "begin":BEGIN,"finish":FINISH,"events":EVENTS,"emissions":EMISSIONS,"markerPrefix":MARKER_PREFIX,
            "listenerObject":"__internal_unstable_listeners_object_id__",
            "emitterFunction":"__internal_unstable_listeners_function_id__",
            "forgedListenerId":u32::MAX-1,
            "foreign":{"attempt":FOREIGN_ATTEMPT,"documentPath":FRAME_DOCUMENT,
                "namePrefix":FRAME_NAME_PREFIX,"resultKind":FRAME_RESULT,
                "origin":"studio-view://studio-attachment-privacy.localhost"},
            "broker":{"connect":UI_CONNECT,"disconnect":UI_DISCONNECT},
        "failures":{"raw":Failure::RawNativePath,"admission":Failure::ListenerAdmission,"foreign":Failure::ForeignFrame,"setup":Failure::FixtureSetup},
        })
    )
}

/// Root installs this only in this fixture mode. It injects no privileged API.
pub(crate) fn frame_initialization_script() -> String {
    format!(
        "(function(config){{\n{}\n}})({});",
        include_str!("attachment-frame-privacy.js"),
        json!({"attempt":FOREIGN_ATTEMPT,"documentPath":FRAME_DOCUMENT,
            "namePrefix":FRAME_NAME_PREFIX,"resultKind":FRAME_RESULT})
    )
}

/// Exact fixture assets avoid loading the application inside the probe frames.
pub(crate) fn frame_asset(path: &str, head: bool) -> Option<tauri::http::Response<Vec<u8>>> {
    if !matches!(path, FRAME_DOCUMENT | FRAME_SCRIPT) || directory().is_err() {
        return None;
    }
    let (mime, bytes) = if path == FRAME_DOCUMENT {
        (
            "text/html; charset=utf-8",
            format!(
                "<!doctype html><html><head><script src=\"{FRAME_SCRIPT}\"></script></head><body></body></html>"
            ),
        )
    } else {
        (
            "text/javascript; charset=utf-8",
            frame_initialization_script(),
        )
    };
    Some(
        tauri::http::Response::builder()
            .header("Content-Type", mime)
            .header(
                "Content-Security-Policy",
                "default-src 'none'; script-src 'self'; frame-ancestors tauri://localhost",
            )
            .header("X-Content-Type-Options", "nosniff")
            .body(if head { Vec::new() } else { bytes.into_bytes() })
            .expect("Static fixture response headers are valid"),
    )
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Failure {
    RawNativePath,
    ListenerAdmission,
    ForeignFrame,
    FixtureSetup,
}

struct NativeListeners {
    window: WebviewWindow,
    ids: Vec<tauri::EventId>,
}
impl Drop for NativeListeners {
    fn drop(&mut self) {
        for id in &self.ids {
            self.window.unlisten(*id);
        }
    }
}
struct Marker(std::path::PathBuf);
impl Drop for Marker {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn probe(window: &WebviewWindow) -> Result<Value, String> {
    let directory = directory()?;
    let marker = Marker(directory.join(format!(
        "{MARKER_PREFIX}{}",
        studio_client_core::server_keys::random_id()?
    )));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker.0)
        .map_err(|_| "The native attachment fixture could not create its owned marker.")?;
    drop(file);
    let marker_value = serde_json::to_value(&marker.0)
        .map_err(|_| "The native attachment fixture marker path is not valid text.")?;
    let received = Arc::new(AtomicUsize::new(0));
    let mut listeners = NativeListeners {
        window: window.clone(),
        ids: vec![],
    };
    for event in EVENTS {
        let expected = marker_value.clone();
        let received = received.clone();
        listeners.ids.push(window.listen_any(event, move |event| {
            if let Ok(payload) = serde_json::from_str::<Value>(event.payload())
                && payload["paths"] == json!([expected])
            {
                received.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }
    let payload = json!({"paths":[marker_value],"position":{"x":1,"y":1}});
    // These use the same manager emit/emit_filter paths as the pinned native
    // window and webview drag handlers. They do not simulate an OS drag action.
    window
        .emit(EVENTS[0], &payload)
        .map_err(|_| "The native attachment fixture emit failed.")?;
    window
        .emit(EVENTS[1], &payload)
        .map_err(|_| "The native attachment fixture emit failed.")?;
    window
        .emit_to(
            EventTarget::webview_window(window.label()),
            EVENTS[0],
            &payload,
        )
        .map_err(|_| "The native attachment fixture window emit failed.")?;
    window
        .emit_to(EventTarget::webview(window.label()), EVENTS[1], &payload)
        .map_err(|_| "The native attachment fixture webview emit failed.")?;
    // Queue a same-webview evaluation after the emits. Its callback supplies a
    // completion fence without a JavaScript timer, polling, or arbitrary sleep.
    let (send, receive) = tokio::sync::oneshot::channel();
    let send = Mutex::new(Some(send));
    window
        .eval_with_callback("true", move |value| {
            if let Ok(mut send) = send.lock()
                && let Some(send) = send.take()
            {
                let _ = send.send(value);
            }
        })
        .map_err(|_| "The native attachment fixture fence failed.")?;
    let fence = tokio::time::timeout(BARRIER_TIMEOUT, receive)
        .await
        .map_err(|_| "The native attachment fixture fence timed out.")?
        .map_err(|_| "The native attachment fixture fence closed.")?;
    if fence != "true" {
        return Err("The native attachment fixture returned an invalid fence.".into());
    }
    let delivered = received.load(Ordering::SeqCst);
    if delivered != EMISSIONS {
        return Err("The native attachment fixture lost a Rust event delivery.".into());
    }
    std::fs::remove_file(&marker.0)
        .map_err(|_| "The native attachment fixture could not remove its owned marker.")?;
    Ok(
        json!({"rustDelivered":delivered,"foreignAdmissions":FOREIGN_ADMISSIONS.load(Ordering::SeqCst)}),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Summary {
    passed: bool,
    #[serde(default)]
    rust_delivered: usize,
    #[serde(default)]
    raw_leak_count: usize,
    #[serde(default)]
    allowed: usize,
    #[serde(default)]
    denied: usize,
    #[serde(default)]
    fake_registry: usize,
    #[serde(default)]
    callback_intercept: bool,
    #[serde(default)]
    register_api_absent: bool,
    #[serde(default)]
    foreign_denied: usize,
    #[serde(default)]
    unsafe_control: bool,
    #[serde(default)]
    foreign_admissions: usize,
    #[serde(default)]
    foreign_bridge_accepted: bool,
    #[serde(default)]
    broker_ready: bool,
    #[serde(default)]
    broker_denied: usize,
    #[serde(default)]
    failure: Option<Failure>,
}

/// The root client_request handler calls this immediately after trusted(window).
/// There is no production method, permission, or broker exception for this fixture.
pub(crate) async fn dispatch(
    window: &WebviewWindow,
    request: &Request,
) -> Result<Option<Response>, String> {
    if !matches!(request.method.as_str(), BEGIN | FINISH | FOREIGN_ATTEMPT) {
        return Ok(None);
    }
    crate::trusted(window)?;
    let directory = directory()?;
    if request.method == FOREIGN_ATTEMPT {
        FOREIGN_ADMISSIONS.fetch_add(1, Ordering::SeqCst);
        return Ok(Some(Response::success(
            request.id.clone(),
            json!({"accepted":true}),
        )));
    }
    if request.method == BEGIN {
        return Ok(Some(match probe(window).await {
            Ok(value) => Response::success(request.id.clone(), value),
            Err(error) => Response::failure(request.id.clone(), "native_attachment_privacy", error),
        }));
    }
    let summary: Summary = serde_json::from_value(request.params.clone())
        .map_err(|_| "Invalid sanitized attachment privacy result.")?;
    let passed = summary.passed
        && summary.rust_delivered == EMISSIONS
        && summary.raw_leak_count == 0
        && summary.allowed == 0
        && summary.denied == EVENTS.len() * 4
        && summary.fake_registry == EVENTS.len()
        && summary.callback_intercept
        && summary.register_api_absent
        && summary.foreign_denied == 2
        && summary.unsafe_control
        && summary.foreign_admissions == 1
        && !summary.foreign_bridge_accepted
        && summary.broker_ready
        && summary.broker_denied == 4
        && summary.failure.is_none();
    let result = json!({"passed":passed,"mode":MODE,"rustDelivered":summary.rust_delivered,"rawLeakCount":summary.raw_leak_count,
        "allowed":summary.allowed,"denied":summary.denied,"fakeRegistry":summary.fake_registry,"callbackIntercept":summary.callback_intercept,
        "registerApiAbsent":summary.register_api_absent,"foreignDenied":summary.foreign_denied,
        "unsafeControl":summary.unsafe_control,"foreignAdmissions":summary.foreign_admissions,
        "foreignBridgeAccepted":summary.foreign_bridge_accepted,
        "brokerReady":summary.broker_ready,"brokerDenied":summary.broker_denied,"failure":summary.failure});
    std::fs::write(
        directory.join("webview-result.json"),
        serde_json::to_vec(&result)
            .map_err(|_| "The attachment privacy result could not be encoded.")?,
    )
    .map_err(|_| "The attachment privacy result could not be saved.")?;
    window.app_handle().exit(if passed { 0 } else { 1 });
    Ok(Some(Response::success(
        request.id.clone(),
        json!({"recorded":true}),
    )))
}
