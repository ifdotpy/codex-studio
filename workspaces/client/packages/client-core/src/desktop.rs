//! Desktop permissions are issued from a recent native event, never a JS flag.
// The native paths here run only on macOS; other targets build the module for checks.
#![cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, unused_imports, unused_variables)
)]
use crate::protocol::{Request, Response};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const ACTION_WINDOW: Duration = Duration::from_millis(1200);
const DENIED: &str = "Use a desktop button or keyboard action.";
#[derive(Default)]
struct Permissions {
    last_event: Option<(isize, u64)>,
    permits: HashMap<u64, (u32, Instant)>,
}
impl Permissions {
    fn issue(
        &mut self,
        event: (isize, u64),
        purpose: u32,
        token: u64,
        now: Instant,
        age: Duration,
    ) -> u64 {
        self.permits.retain(|_, (_, expires)| *expires > now);
        if self.last_event == Some(event)
            || age > ACTION_WINDOW
            || token == 0
            || self.permits.contains_key(&token)
            || self.permits.len() >= 32
        {
            return 0;
        }
        self.last_event = Some(event);
        // At most one permit per real event. No renderer-controlled expiry.
        self.permits
            .insert(token, (purpose, now + (ACTION_WINDOW - age)));
        token
    }
    fn consume(&mut self, token: u64, purpose: u32, now: Instant) -> Result<(), String> {
        match self.permits.remove(&token) {
            Some((action, expires)) if action == purpose && now < expires => Ok(()),
            _ => Err(DENIED.into()),
        }
    }
}
fn permissions() -> &'static Mutex<Permissions> {
    static VALUE: OnceLock<Mutex<Permissions>> = OnceLock::new();
    VALUE.get_or_init(Mutex::default)
}
fn purpose(method: &str) -> Option<u32> {
    Some(match method {
        "desktop.pickDirectory" => 1,
        "desktop.pickFiles" | "desktop.pickAttachments" => 2,
        "desktop.revealPath" => 3,
        "desktop.openExternal" | "desktop.openSignIn" => 4,
        "desktop.saveBegin"
        | "desktop.savePreview"
        | "desktop.saveTerminal"
        | "desktop.saveRecording" => 5,
        "desktop.fileAction" => 6,
        "desktop.requestMicrophone" | "desktop.reserveMicrophone" => 7,
        "desktop.prepareTranscription" => 8,
        "desktop.cancelTranscription" => 9,
        "desktop.copyText" => 10,
        "desktop.showVaultKeys" => 11,
        _ => return None,
    })
}

/// Issue an action-specific permit on the native UI thread.
/// The caller must keep the returned token out of renderer responses.
pub fn gesture_for_method(method: &str) -> u64 {
    purpose(method)
        .map(|value| studio_core_gesture_v1(value))
        .unwrap_or(0)
}

/// Called synchronously by the Attar native adapter on the UI thread.
/// Zero means no authorization. The token remains private to this process.
#[unsafe(no_mangle)]
pub extern "C" fn studio_core_gesture_v1(purpose: u32) -> u64 {
    std::panic::catch_unwind(|| {
        if !(1..=11).contains(&purpose) {
            return 0;
        }
        #[cfg(target_os = "macos")]
        {
            use objc2::{MainThreadMarker, rc::autoreleasepool};
            use objc2_app_kit::{NSApplication, NSEventType};
            use objc2_foundation::NSProcessInfo;
            let Some(mtm) = MainThreadMarker::new() else {
                return 0;
            };
            autoreleasepool(|_| {
                let app = NSApplication::sharedApplication(mtm);
                if !app.isActive() {
                    return 0;
                }
                let (Some(event), Some(window)) = (app.currentEvent(), app.mainWindow()) else {
                    return 0;
                };
                if event.windowNumber() != window.windowNumber()
                    || !matches!(
                        event.r#type(),
                        NSEventType::LeftMouseUp | NSEventType::KeyDown | NSEventType::KeyUp
                    )
                {
                    return 0;
                }
                let age = NSProcessInfo::processInfo().systemUptime() - event.timestamp();
                if !age.is_finite() || age < 0.0 || age >= ACTION_WINDOW.as_secs_f64() {
                    return 0;
                }
                let mut bytes = [0u8; 8];
                if unsafe { libc::getentropy(bytes.as_mut_ptr().cast(), bytes.len()) } != 0 {
                    return 0;
                }
                let token = u64::from_ne_bytes(bytes) & ((1u64 << 48) - 1);
                let Ok(mut guard) = permissions().lock() else {
                    return 0;
                };
                guard.issue(
                    (event.windowNumber(), event.timestamp().to_bits()),
                    purpose,
                    token,
                    Instant::now(),
                    Duration::from_secs_f64(age),
                )
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            0
        }
    })
    .unwrap_or(0)
}

fn external_url(value: &Value) -> Result<String, String> {
    let value = value
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 4096 && !v.contains('\0'))
        .ok_or("Invalid native action value.")?;
    let url = reqwest::Url::parse(value).map_err(|_| "Invalid URL.")?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
    {
        return Err("Only HTTP and HTTPS links without credentials are supported.".into());
    }
    Ok(url.to_string())
}

fn validate_sign_in_forward(value: &Value, owner: Option<&str>) -> Result<(), String> {
    let string = |name: &str, max: usize| {
        value[name]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= max && !s.contains('\0'))
    };
    let server = string("server", 200).ok_or("Invalid sign-in server.")?;
    if owner != Some(server) || server == "local" {
        return Err("This sign-in belongs to another server view.".into());
    }
    let bridge = string("bridgeId", 32).ok_or("Invalid sign-in bridge.")?;
    if bridge.len() != 32
        || !bridge
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        || !matches!(string("host", 16), Some("127.0.0.1" | "localhost" | "::1"))
        || !value["port"]
            .as_u64()
            .is_some_and(|p| (1024..=65535).contains(&p))
        || !string("path", 2048).is_some_and(|s| s.starts_with('/'))
        || !value["expiresAt"].as_f64().is_some_and(f64::is_finite)
        || string("token", 200).is_none()
    {
        return Err("Invalid sign-in forward.".into());
    }
    Ok(())
}

async fn prepare_sign_in(
    value: &Value,
    owner: Option<&str>,
    id: &str,
    backend: Option<&crate::backend::Backend>,
) -> Result<Value, String> {
    let url = external_url(&value["url"])?;
    if let Some(forward) = value.get("forward") {
        validate_sign_in_forward(forward, owner)?;
        let response = backend.ok_or("The native backend is unavailable.")?.dispatch(Request {
                    id: format!("{}:forward", id),
                    method: "api.request".into(),
                    params: serde_json::json!({"path":"/api/browser/forward", "method":"POST", "body":forward, "requestId":id, "timeoutMs":20000}),
                }).await;
        if let Some(error) = response.error {
            return Err(error.message);
        }
        if response.result.as_ref().and_then(|v| v["status"].as_str()) != Some("forwarding") {
            return Err(
                "This Mac did not confirm the sign-in port. The request was not repeated.".into(),
            );
        }
    }
    Ok(Value::String(url))
}

pub async fn dispatch(
    request: Request,
    sender: Option<&crate::delivery::StoreSender>,
    backend: Option<&crate::backend::Backend>,
    previews: Option<&crate::file_preview::Previews>,
) -> Response {
    let result = async {
        if matches!(
            request.method.as_str(),
            "desktop.saveDeclare" | "desktop.saveChunk" | "desktop.saveCancel"
        ) {
            return crate::file_save::command(&request.method, &request.params["value"]);
        }
        #[cfg(target_os = "macos")]
        if request.method == "desktop.saveFinish" {
            let (name, bytes) = crate::file_save::finish(&request.params["value"])?;
            return save_snapshot(name, std::sync::Arc::new(bytes)).await;
        }
        let action = purpose(&request.method).ok_or("This native action is not available.")?;
        let token = request
            .params
            .get("permit")
            .and_then(Value::as_u64)
            .ok_or(DENIED)?;
        permissions()
            .lock()
            .map_err(|_| DENIED)?
            .consume(token, action, Instant::now())?;
        let mut value = request.params.get("value").cloned().unwrap_or(Value::Null);
        if request.method == "desktop.openSignIn" && value.is_object() {
            value = prepare_sign_in(
                &value,
                request.params["signInOwner"].as_str(),
                &request.id,
                backend,
            )
            .await?;
        }
        match request.method.as_str() {
            "desktop.showVaultKeys" => return crate::vault_keys::show(&value).await,
            "desktop.saveBegin" => return crate::file_save::command(&request.method, &value),
            "desktop.requestMicrophone" => return crate::media::request_microphone().await,
            "desktop.reserveMicrophone" => return crate::media::reserve_microphone(Instant::now()),
            "desktop.prepareTranscription" => return crate::speech::prepare(Instant::now()),
            "desktop.cancelTranscription" => return crate::speech::cancel(&value),
            _ => {}
        }
        #[cfg(target_os = "macos")]
        {
            if request.method == "desktop.saveTerminal" {
                let backend = backend.ok_or("The native backend is unavailable.")?;
                let (name, bytes) = crate::terminal::history(backend, &value).await?;
                return save_snapshot(name, bytes).await;
            }
            if request.method == "desktop.saveRecording" {
                let sender = sender.ok_or("The native state service is unavailable.")?;
                let id = value
                    .as_str()
                    .filter(|v| !v.is_empty() && v.len() <= 128)
                    .ok_or("Invalid recording.")?;
                let directory = tempfile::Builder::new()
                    .prefix("codex-studio-recording-")
                    .tempdir()
                    .map_err(|e| e.to_string())?;
                let exported = crate::delivery::state(
                    sender,
                    "dictation.wav",
                    serde_json::json!({"id":id,"directory":directory.path()}),
                )
                .await?;
                let path = exported["path"]
                    .as_str()
                    .ok_or("The recording was not exported.")?;
                let bytes = tokio::fs::read(path).await.map_err(|e| e.to_string())?;
                return save_snapshot(format!("dictation-{id}.wav"), std::sync::Arc::new(bytes))
                    .await;
            }
            if request.method == "desktop.savePreview" {
                let (name, bytes) = previews
                    .ok_or("The preview service is unavailable.")?
                    .snapshot(&value)?;
                return save_snapshot(name, bytes).await;
            }
            if request.method == "desktop.fileAction" {
                let backend = backend.ok_or("The native backend is unavailable.")?;
                let file = crate::file_actions::resolve(backend, value).await?;
                return mac::file_action(file).await;
            }
            let value = mac::run(request.method.clone(), value).await?;
            if matches!(
                request.method.as_str(),
                "desktop.pickFiles" | "desktop.pickAttachments"
            ) {
                let files = tokio::task::spawn_blocking(move || read_selected_files(value))
                    .await
                    .map_err(|e| e.to_string())??;
                if request.method == "desktop.pickAttachments" {
                    return Ok(files);
                }
                let sender = sender.ok_or("The native state service is unavailable.")?;
                let (reply, receive) = tokio::sync::oneshot::channel();
                sender
                    .send((
                        Request {
                            id: format!("{}:files", request.id),
                            method: "files.stage_many".into(),
                            params: serde_json::json!({"files":files}),
                        },
                        reply,
                    ))
                    .await
                    .map_err(|e| e.to_string())?;
                let response = receive.await.map_err(|e| e.to_string())?;
                response.result.ok_or_else(|| {
                    response
                        .error
                        .map(|e| e.message)
                        .unwrap_or("Missing file state response.".into())
                })
            } else {
                Ok(value)
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = value;
            Err("Native desktop services require macOS.".into())
        }
    }
    .await;
    match result {
        Ok(value) => Response::success(request.id, value),
        Err(error) => Response::failure(request.id, "desktop_action", error),
    }
}

#[cfg(target_os = "macos")]
async fn save_snapshot(name: String, bytes: std::sync::Arc<Vec<u8>>) -> Result<Value, String> {
    struct Owner(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Owner {
        fn drop(&mut self) {
            self.0.store(false, std::sync::atomic::Ordering::Release);
        }
    }
    let owner = Owner(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
        true,
    )));
    let path = mac::run("desktop.saveChoose".into(), Value::String(name)).await?;
    if path.is_null() {
        return Ok(Value::Bool(false));
    }
    let path = std::path::PathBuf::from(path.as_str().ok_or("Invalid save destination.")?);
    let active = owner.0.clone();
    tokio::task::spawn_blocking(move || {
        crate::file_save::write_atomic_if(&path, &bytes, || {
            active.load(std::sync::atomic::Ordering::Acquire)
        })
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(Value::Bool(true))
}

fn read_selected_files(paths: Value) -> Result<Value, String> {
    use base64::Engine;
    use std::{io::Read, os::unix::fs::OpenOptionsExt};
    let paths = paths
        .as_array()
        .filter(|v| v.len() <= 20)
        .ok_or("Select at most 20 files.")?;
    let mut result = vec![];
    let mut total = 0;
    for value in paths {
        let path = std::path::Path::new(value.as_str().ok_or("Invalid selected path.")?);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .map_err(|e| e.to_string())?;
        let info = file.metadata().map_err(|e| e.to_string())?;
        if !info.is_file() || info.len() == 0 || info.len() > crate::uploads::LIMIT as u64 {
            return Err("Select regular files with 1 byte to 20 MiB.".into());
        }
        let mut bytes = vec![];
        file.take((crate::uploads::LIMIT + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        total += bytes.len();
        if bytes.is_empty() || total > crate::uploads::LIMIT {
            return Err("The selected files exceed 20 MiB or contain an empty file.".into());
        }
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("The file name is not valid UTF-8.")?;
        let mime = match path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "svg" => "image/svg+xml",
            "pdf" => "application/pdf",
            "json" => "application/json",
            "md" => "text/markdown",
            "html" => "text/html",
            "txt" | "rs" | "py" | "js" | "ts" | "tsx" => "text/plain",
            _ => "application/octet-stream",
        };
        result.push(serde_json::json!({"name":name,"mime":mime,"base64":base64::engine::general_purpose::STANDARD.encode(bytes)}));
    }
    Ok(serde_json::json!(result))
}

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use objc2::{MainThreadMarker, rc::autoreleasepool};
    use objc2_app_kit::{
        NSOpenPanel, NSPasteboard, NSPasteboardTypeString, NSSavePanel, NSWorkspace,
        NSWorkspaceOpenConfiguration,
    };
    use objc2_foundation::{NSArray, NSError, NSString, NSURL};

    pub async fn file_action(file: crate::file_actions::ResolvedFile) -> Result<Value, String> {
        use crate::file_actions::Action;
        let (reply, receive) = tokio::sync::oneshot::channel();
        dispatch2::DispatchQueue::main().exec_async(move || {
            if reply.is_closed() {
                return;
            }
            autoreleasepool(|_| {
                let url = NSURL::fileURLWithPath(&NSString::from_str(&file.path.to_string_lossy()));
                match file.action {
                    Action::Reveal => {
                        NSWorkspace::sharedWorkspace()
                            .activateFileViewerSelectingURLs(&NSArray::from_retained_slice(&[url]));
                        let _ = reply.send(Ok(Value::Bool(true)));
                    }
                    Action::Preview => {
                        let _ = reply.send(
                            crate::quick_look::show(&url, &file.name).map(|()| Value::Bool(true)),
                        );
                    }
                    Action::Open => {
                        let config = NSWorkspaceOpenConfiguration::configuration();
                        let reply = Mutex::new(Some(reply));
                        let complete = block2::RcBlock::new(
                            move |_app: *mut objc2_app_kit::NSRunningApplication,
                                  error: *mut NSError| {
                                let result = if error.is_null() {
                                    Ok(Value::Bool(true))
                                } else {
                                    Err(unsafe { &*error }.localizedDescription().to_string())
                                };
                                if let Some(reply) =
                                    reply.lock().ok().and_then(|mut value| value.take())
                                {
                                    let _ = reply.send(result);
                                }
                            },
                        );
                        NSWorkspace::sharedWorkspace().openURL_configuration_completionHandler(
                            &url,
                            &config,
                            Some(&complete),
                        );
                    }
                }
            });
        });
        receive
            .await
            .map_err(|_| "The native file action stopped without a result.".to_owned())?
    }

    pub async fn run(method: String, value: Value) -> Result<Value, String> {
        if method == "desktop.revealPath" {
            let path = value
                .as_str()
                .filter(|v| !v.contains('\0') && v.len() <= 4096)
                .ok_or("Invalid local path.")?;
            let path = std::path::PathBuf::from(path);
            if !path.is_absolute() {
                return Err("A local absolute path is required.".into());
            }
            // A mounted filesystem must not block the AppKit event loop.
            tokio::task::spawn_blocking(move || path.metadata())
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
        }
        let (reply, receive) = tokio::sync::oneshot::channel();
        dispatch2::DispatchQueue::main().exec_async(move || {
            // A disconnected caller cannot start a new privileged action.
            if reply.is_closed() {
                return;
            }
            autoreleasepool(|_| {
                let result = match method.as_str() {
                    "desktop.pickDirectory" => pick_directory(),
                    "desktop.pickFiles" | "desktop.pickAttachments" => pick_files(),
                    "desktop.saveChoose" => save_choose(&value),
                    "desktop.copyText" => copy_text(&value),
                    "desktop.revealPath" => reveal(&value),
                    "desktop.openExternal" | "desktop.openSignIn" => {
                        let address = match external_url(&value) {
                            Ok(value) => value,
                            Err(error) => {
                                let _ = reply.send(Err(error));
                                return;
                            }
                        };
                        let url = NSURL::URLWithString(&NSString::from_str(&address))
                            .expect("validated URL");
                        let config = NSWorkspaceOpenConfiguration::configuration();
                        config.setActivates(method == "desktop.openSignIn");
                        let reply = Mutex::new(Some(reply));
                        let complete = block2::RcBlock::new(
                            move |_app: *mut objc2_app_kit::NSRunningApplication,
                                  error: *mut NSError| {
                                let result = if error.is_null() {
                                    Ok(Value::Bool(true))
                                } else {
                                    Err(unsafe { &*error }.localizedDescription().to_string())
                                };
                                if let Some(reply) =
                                    reply.lock().ok().and_then(|mut value| value.take())
                                {
                                    let _ = reply.send(result);
                                }
                            },
                        );
                        NSWorkspace::sharedWorkspace().openURL_configuration_completionHandler(
                            &url,
                            &config,
                            Some(&complete),
                        );
                        return;
                    }
                    _ => Err("This native action is not available.".into()),
                };
                let _ = reply.send(result);
            });
        });
        receive
            .await
            .map_err(|_| "The native action stopped without a result.".to_owned())?
    }
    fn copy_text(value: &Value) -> Result<Value, String> {
        let text = value
            .as_str()
            .filter(|s| s.len() <= 8 * 1024 * 1024)
            .ok_or("Copy accepts at most 8 MiB of text.")?;
        let clipboard = NSPasteboard::generalPasteboard();
        clipboard.clearContents();
        if clipboard.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString })
        {
            Ok(Value::Null)
        } else {
            Err("The system clipboard did not accept the text.".into())
        }
    }
    fn save_choose(value: &Value) -> Result<Value, String> {
        let mtm = MainThreadMarker::new().ok_or("The save dialog requires the main thread.")?;
        let panel = NSSavePanel::savePanel(mtm);
        panel.setTitle(Some(&NSString::from_str("Save As")));
        panel.setNameFieldStringValue(&NSString::from_str(
            value.as_str().ok_or("A file name is required.")?,
        ));
        panel.setCanCreateDirectories(true);
        if panel.runModal() != 1 {
            return Ok(Value::Null);
        }
        Ok(Value::String(
            panel
                .URL()
                .and_then(|url| url.path())
                .ok_or("The destination has no local path.")?
                .to_string(),
        ))
    }
    fn pick_files() -> Result<Value, String> {
        let mtm = MainThreadMarker::new().ok_or("The file dialog requires the main thread.")?;
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseDirectories(false);
        panel.setCanChooseFiles(true);
        panel.setAllowsMultipleSelection(true);
        if panel.runModal() != 1 {
            return Ok(serde_json::json!([]));
        }
        let urls = panel.URLs();
        if urls.len() > 20 {
            return Err("Select at most 20 files.".into());
        }
        let mut paths = vec![];
        for index in 0..urls.len() {
            let url = urls.objectAtIndex(index);
            paths.push(url.path().ok_or("The file has no local path.")?.to_string());
        }
        Ok(serde_json::json!(paths))
    }
    fn pick_directory() -> Result<Value, String> {
        let mtm =
            MainThreadMarker::new().ok_or("The directory dialog requires the main thread.")?;
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseDirectories(true);
        panel.setCanChooseFiles(false);
        panel.setAllowsMultipleSelection(false);
        panel.setCanCreateDirectories(true);
        if panel.runModal() != 1 {
            return Ok(Value::Null);
        }
        let path = panel
            .URL()
            .and_then(|url| url.path())
            .ok_or("The selected directory has no local path.")?;
        Ok(Value::String(path.to_string()))
    }
    fn reveal(value: &Value) -> Result<Value, String> {
        let path = value
            .as_str()
            .filter(|v| !v.contains('\0') && v.len() <= 4096)
            .ok_or("Invalid local path.")?;
        let path = std::path::Path::new(path);
        if !path.is_absolute() {
            return Err("A local absolute path is required.".into());
        }
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        NSWorkspace::sharedWorkspace()
            .activateFileViewerSelectingURLs(&NSArray::from_retained_slice(&[url]));
        Ok(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn forward() -> Value {
        serde_json::json!({"server":"remote", "bridgeId":"a".repeat(32), "host":"127.0.0.1", "port":49152, "path":"/callback", "expiresAt":1900000000, "token":"secret"})
    }
    #[tokio::test]
    async fn sign_in_validates_owner_before_network_and_requires_port_confirmation() {
        use crate::backend::tests::{read_http, reply};
        let forward = forward();
        for owner in [None, Some("local"), Some("other")] {
            assert!(validate_sign_in_forward(&forward, owner).is_err());
        }
        for (key, value) in [
            ("host", serde_json::json!("example.com")),
            ("port", serde_json::json!(80)),
            ("bridgeId", serde_json::json!("invalid")),
            ("path", serde_json::json!("callback")),
        ] {
            let mut invalid = forward.clone();
            invalid[key] = value;
            assert!(validate_sign_in_forward(&invalid, Some("remote")).is_err());
        }
        for confirmed in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let backend =
                crate::backend::Backend::new(&format!("http://{}", listener.local_addr().unwrap()))
                    .unwrap();
            let expected = forward.clone();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                assert!(
                    read_http(&mut stream)
                        .await
                        .0
                        .starts_with("GET /api/session ")
                );
                reply(&mut stream, r#"{"token":"local-session"}"#).await;
                let (mut stream, _) = listener.accept().await.unwrap();
                let (headers, body) = read_http(&mut stream).await;
                assert!(headers.starts_with("POST /api/browser/forward "));
                assert!(
                    headers
                        .to_ascii_lowercase()
                        .contains("x-studio-request-id: original-id")
                );
                assert!(
                    headers
                        .to_ascii_lowercase()
                        .contains("x-canvas-token: local-session")
                );
                assert_eq!(body, expected);
                reply(
                    &mut stream,
                    if confirmed {
                        r#"{"status":"forwarding"}"#
                    } else {
                        "{}"
                    },
                )
                .await;
            });
            let result = prepare_sign_in(
                &serde_json::json!({"url":"https://example.com/authorize", "forward":forward}),
                Some("remote"),
                "original-id",
                Some(&backend),
            )
            .await;
            assert_eq!(result.is_ok(), confirmed);
            server.await.unwrap();
        }
    }
    #[test]
    fn permits_are_native_event_scoped_expire_and_cannot_be_replayed() {
        let mut p = Permissions::default();
        let now = Instant::now();
        assert_eq!(p.issue((1, 1), 1, 45, now, Duration::ZERO), 45);
        assert_eq!(p.issue((1, 1), 2, 46, now, Duration::ZERO), 0);
        assert!(p.consume(45, 2, now).is_err());
        assert!(p.consume(45, 1, now).is_err());
        assert_eq!(p.issue((1, 2), 1, 47, now, Duration::ZERO), 47);
        assert!(p.consume(47, 1, now).is_ok());
        assert!(p.consume(47, 1, now).is_err());
        assert_eq!(p.issue((1, 3), 1, 48, now, Duration::from_millis(1199)), 48);
        assert!(p.consume(48, 1, now + Duration::from_millis(2)).is_err());
        assert_eq!(
            p.issue((1, 4), 1, 49, now, ACTION_WINDOW + Duration::from_millis(1)),
            0
        );
    }
    #[tokio::test]
    async fn native_actions_reject_missing_permits_and_unsafe_urls() {
        for method in [
            "desktop.pickDirectory",
            "desktop.pickAttachments",
            "desktop.openExternal",
            "desktop.openSignIn",
            "desktop.revealPath",
            "desktop.copyText",
            "desktop.showVaultKeys",
            "desktop.saveBegin",
            "desktop.savePreview",
            "desktop.saveTerminal",
            "desktop.fileAction",
        ] {
            let reply = dispatch(
                Request {
                    id: "test".into(),
                    method: method.into(),
                    params: serde_json::json!({"value":"https://example.com"}),
                },
                None,
                None,
                None,
            )
            .await;
            assert!(reply.error.is_some());
        }
        for address in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://user@example.com",
            "https://user:secret@example.com",
            "https://example.com\0",
        ] {
            assert!(external_url(&Value::String(address.into())).is_err());
        }
        assert_eq!(
            external_url(&Value::String("https://example.com/hello?q=1".into())).unwrap(),
            "https://example.com/hello?q=1"
        );
    }
    #[test]
    fn selected_file_bytes_are_bounded_and_special_files_are_rejected() {
        use base64::Engine;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hello.txt");
        std::fs::write(&path, "Привет 🦀").unwrap();
        let value = read_selected_files(serde_json::json!([path])).unwrap();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(value[0]["base64"].as_str().unwrap())
                .unwrap(),
            "Привет 🦀".as_bytes()
        );
        assert_eq!(value[0]["name"], "hello.txt");
        assert!(read_selected_files(serde_json::json!([directory.path()])).is_err());
        assert!(read_selected_files(serde_json::json!(["/dev/null"])).is_err());
        let huge = std::fs::File::create(directory.path().join("huge")).unwrap();
        huge.set_len((crate::uploads::LIMIT + 1) as u64).unwrap();
        assert!(read_selected_files(serde_json::json!([directory.path().join("huge")])).is_err());
    }
}
