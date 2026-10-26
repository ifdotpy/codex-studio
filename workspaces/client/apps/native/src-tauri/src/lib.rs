mod close;
#[cfg(desktop)]
mod desktop_menu;
#[cfg(desktop)]
mod desktop_recovery;
mod frame_assets;
mod local_backend;
#[cfg(feature = "native-check")]
mod native_check;
mod profile;
#[cfg(desktop)]
mod window_state;

use std::sync::Arc;
use studio_client_core::{
    client::Client,
    native::ConnectionConfig,
    protocol::{Request, Response},
};
use tauri::{Emitter, Manager, State, WebviewWindow};
use tokio::sync::Mutex;

#[derive(Default)]
struct Clients {
    local: Mutex<Option<Arc<Client>>>,
    #[cfg(target_os = "macos")]
    remote: Mutex<Option<Arc<studio_client_core::server_transport::RemoteClient>>>,
    backend_verified: Mutex<bool>,
    workspace: Mutex<Option<String>>,
}

// Held for the entire host lifetime, including before local client creation.
struct ProfileLock(#[allow(dead_code)] std::fs::File);
fn lock_profile(directory: &std::path::Path) -> Result<ProfileLock, String> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("native-profile.lock"))
        .map_err(|e| e.to_string())?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(
            "The native profile is already open. Close its desktop before importing keys.".into(),
        );
    }
    Ok(ProfileLock(file))
}

fn draft_workspace_allowed(verified: Option<&str>, params: &serde_json::Value) -> bool {
    verified.is_some() && verified == params["workspace"].as_str()
}

fn packaged_shell(url: &tauri::Url) -> bool {
    matches!(
        (url.scheme(), url.host_str()),
        ("tauri", Some("localhost")) | ("http" | "https", Some("tauri.localhost"))
    ) && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(url.path(), "" | "/" | "/index.html")
}

fn trusted(window: &WebviewWindow) -> Result<(), String> {
    let url = window.url().map_err(|error| error.to_string())?;
    if window.label() != "main" || !packaged_shell(&url) {
        return Err("Native commands are restricted to the packaged Studio window.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::packaged_shell;
    #[test]
    fn drafts_require_a_verified_matching_workspace() {
        let params = serde_json::json!({"workspace":"workspace"});
        assert!(!super::draft_workspace_allowed(None, &params));
        assert!(!super::draft_workspace_allowed(Some("other"), &params));
        assert!(!super::draft_workspace_allowed(
            Some("workspace"),
            &serde_json::json!({})
        ));
        assert!(super::draft_workspace_allowed(Some("workspace"), &params));
    }

    #[test]
    fn native_commands_require_the_packaged_shell_url() {
        for value in [
            "tauri://localhost",
            "tauri://localhost/index.html",
            "tauri://localhost/?studio-navigation=combined#chat",
            "http://tauri.localhost/index.html",
            "https://tauri.localhost/",
        ] {
            assert!(packaged_shell(&value.parse().unwrap()), "{value}");
        }
        for value in [
            "http://localhost/index.html",
            "https://localhost/",
            "tauri://tauri.localhost/",
            "tauri://localhost:4620/",
            "http://tauri.localhost:4620/",
            "tauri://user@localhost/",
            "tauri://user:secret@localhost/",
            "tauri://localhost/assets/document.html",
            "studio-view://studio-nrxwgylm.localhost/",
            "https://tauri.localhost.example/index.html",
            "file:///index.html",
        ] {
            assert!(!packaged_shell(&value.parse().unwrap()), "{value}");
        }
    }
}

#[cfg(target_os = "macos")]
fn approved_peer(
    snapshot: &serde_json::Value,
    params: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let approval = &params["approval"];
    let origin = studio_client_core::server_keys::origin(studio_client_core::server_keys::field(
        params, "origin",
    )?)?;
    let peer = snapshot["servers"]
        .as_array()
        .and_then(|rows| rows.iter().find(|v| v["serverId"] == approval["serverId"]))
        .ok_or("The approved server is no longer available.")?;
    let generation = serde_json::json!([peer["created"], peer["publicKey"]]).to_string();
    if snapshot["protocol"] != 1
        || snapshot["identity"]["serverId"] != approval["localServerId"]
        || peer["kind"] != "server"
        || peer["status"] != "paired"
        || studio_client_core::server_keys::origin(studio_client_core::server_keys::field(
            peer, "origin",
        )?)? != origin
        || approval["generation"] != generation
    {
        return Err("The server approval or its identity changed.".into());
    }
    Ok(peer.clone())
}

#[tauri::command]
async fn client_request(
    window: WebviewWindow,
    state: State<'_, Clients>,
    mut request: Request,
) -> Result<Response, String> {
    #[cfg(feature = "native-check")]
    eprintln!("Hidden native command: {}", request.method);
    trusted(&window)?;
    if request.method.starts_with("servers.") {
        #[cfg(not(target_os = "macos"))]
        return Err("The native server key store is unavailable on this platform.".into());
        #[cfg(target_os = "macos")]
        {
            if request.method == "servers.pair"
                && request.params.get("approval").is_some_and(|v| !v.is_null())
            {
                let p = &request.params;
                let approval = &p["approval"];
                for name in ["localServerId", "serverId", "inviteRequestId"] {
                    let id = studio_client_core::server_keys::field(approval, name)?;
                    if id.len() > 128
                        || !id
                            .bytes()
                            .all(|v| v.is_ascii_alphanumeric() || matches!(v, b'_' | b'-'))
                    {
                        return Err("Invalid approved server identity.".into());
                    }
                }
                let read = |params: serde_json::Value| {
                    let window = window.clone();
                    let state = state.clone();
                    async move {
                        let response = Box::pin(client_request(
                            window,
                            state,
                            Request {
                                id: studio_client_core::server_keys::random_id()?,
                                method: "api.request".into(),
                                params,
                            },
                        ))
                        .await?;
                        response.result.ok_or_else(|| {
                            response
                                .error
                                .map(|e| e.message)
                                .unwrap_or_else(|| "The local server access check failed.".into())
                        })
                    }
                };
                let before =
                    read(serde_json::json!({"path":"/api/multi-server","method":"GET"})).await?;
                let peer = approved_peer(&before, p)?;
                let invitation = read(serde_json::json!({"path":"/api/multi-server","method":"POST","requestId":approval["inviteRequestId"],"body":{"action":"ui_invite","serverId":approval["serverId"],"requestId":approval["inviteRequestId"]}})).await?;
                let invitation = &invitation["invitation"];
                if invitation["serverId"] != peer["serverId"]
                    || invitation["publicKey"] != peer["publicKey"]
                    || [
                        "protocol",
                        "inviteId",
                        "token",
                        "serverId",
                        "origin",
                        "publicKey",
                        "tailscaleUser",
                        "expires",
                    ]
                    .iter()
                    .any(|key| invitation[*key] != p["invitation"][*key])
                {
                    return Err("The invitation does not match the approved server.".into());
                }
                let after =
                    read(serde_json::json!({"path":"/api/multi-server","method":"GET"})).await?;
                approved_peer(&after, p)?;
            }
            let client = {
                let mut remote = state.remote.lock().await;
                if remote.is_none() {
                    let directory =
                        match std::env::var_os("CODEX_DESKTOP_PROFILE").filter(|v| !v.is_empty()) {
                            Some(path) => std::path::PathBuf::from(path),
                            None => window
                                .app_handle()
                                .path()
                                .app_data_dir()
                                .map_err(|e| e.to_string())?,
                        };
                    std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
                    let account = directory
                        .canonicalize()
                        .map_err(|e| e.to_string())?
                        .to_string_lossy()
                        .into_owned();
                    let vault = studio_client_core::server_keys::KeychainVault { account };
                    *remote = Some(Arc::new(
                        studio_client_core::server_transport::RemoteClient::new(Arc::new(vault))?,
                    ));
                }
                remote.as_ref().unwrap().clone()
            };
            return match client.dispatch(&request.method, &request.params).await {
                Ok(value) => Ok(Response::success(request.id, value)),
                Err(error) => Ok(Response::failure(request.id, "server_error", error)),
            };
        }
    }
    if request.method == "desktop.notify" {
        return match studio_client_core::notifications::notify(&request.params).await {
            Ok(value) => Ok(Response::success(request.id, value)),
            Err(error) => Ok(Response::failure(request.id, "notification", error)),
        };
    }
    if matches!(
        request.method.as_str(),
        "profile.snapshot" | "profile.chunk" | "profile.workspace"
    ) {
        if request.method == "profile.workspace" {
            return Ok(Response::success(request.id, profile::workspace()?));
        }
        let owner = request.params["owner"]
            .as_str()
            .ok_or("Missing profile owner.")?;
        return Ok(Response {
            id: request.id,
            result: Some(if request.method == "profile.chunk" {
                profile::chunk(
                    owner,
                    request.params["sha256"]
                        .as_str()
                        .ok_or("Missing profile chunk digest.")?,
                )?
            } else {
                profile::snapshot(owner)?
            }),
            error: None,
        });
    }
    // Enable only the independently verified service boundary while migrating
    // callers. Network commands also require the configured backend identity.
    if !matches!(
        request.method.as_str(),
        "system.info"
            | "preferences.raw_all"
            | "preferences.import"
            | "preferences.raw_set"
            | "preferences.raw_remove"
            | "markdown.parse"
            | "identity.get"
            | "sync.open"
            | "drafts.view"
            | "drafts.import"
            | "drafts.edit"
            | "drafts.dismiss"
            | "drafts.refresh"
            | "desktop.openExternal"
            | "desktop.openSignIn"
            | "desktop.pickDirectory"
            | "desktop.pickAttachments"
            | "desktop.fileAction"
            | "desktop.revealPath"
            | "desktop.showVaultKeys"
            | "desktop.requestMicrophone"
            | "desktop.prepareTranscription"
            | "desktop.cancelTranscription"
            | "media.captureStart"
            | "media.captureStop"
            | "voice.create"
            | "voice.answer"
            | "voice.mute"
            | "voice.close"
            | "speech.uploadBegin"
            | "speech.uploadChunk"
            | "speech.uploadCancel"
            | "speech.transcribe"
            | "speech.cancel"
            | "desktop.saveBegin"
            | "desktop.saveChunk"
            | "desktop.saveFinish"
            | "desktop.saveCancel"
            | "api.request"
            | "view.subscribe"
            | "view.unsubscribe"
    ) {
        return Err("This client service is not connected yet.".into());
    }
    if request.method == "view.subscribe"
        && !matches!(
            request.params["topic"].as_str(),
            Some("sse" | "microphone" | "transcription" | "voice")
        )
    {
        return Err("This native subscription topic is unavailable.".into());
    }
    if matches!(
        request.method.as_str(),
        "desktop.fileAction"
            | "desktop.revealPath"
            | "desktop.showVaultKeys"
            | "desktop.requestMicrophone"
            | "desktop.prepareTranscription"
            | "desktop.cancelTranscription"
            | "desktop.saveBegin"
            | "desktop.pickDirectory"
            | "desktop.pickAttachments"
            | "desktop.openExternal"
            | "desktop.openSignIn"
    ) {
        // Renderer-supplied permits never cross the privilege boundary.
        let params = request
            .params
            .as_object_mut()
            .ok_or("Invalid native action parameters.")?;
        params.remove("permit");
        let (send, receive) = tokio::sync::oneshot::channel();
        let method = request.method.clone();
        window
            .run_on_main_thread(move || {
                let _ = send.send(studio_client_core::desktop::gesture_for_method(&method));
            })
            .map_err(|error| error.to_string())?;
        let permit = receive
            .await
            .map_err(|_| "The native action was cancelled.")?;
        if permit == 0 {
            return Err("Use a desktop button or keyboard action.".into());
        }
        params.insert("permit".into(), permit.into());
    }
    let origin = local_backend::origin()?;
    if matches!(
        request.method.as_str(),
        "api.request" | "sync.open" | "drafts.refresh" | "desktop.fileAction"
    ) || (request.method == "desktop.openSignIn"
        && request.params["value"].get("forward").is_some())
        || (request.method == "view.subscribe" && request.params["topic"] == "sse")
    {
        #[cfg(feature = "native-check")]
        if native_check::directory().is_some()
            && std::env::var("STUDIO_NATIVE_CHECK_NETWORK").as_deref() != Ok("1")
        {
            return Err("Network is disabled in the isolated persistence check.".into());
        }
        let mut verified = state.backend_verified.lock().await;
        if !*verified {
            let resources = window
                .app_handle()
                .path()
                .resource_dir()
                .map_err(|error| error.to_string())?
                .join("workspace");
            #[cfg(debug_assertions)]
            let resources = if resources.join("scripts/codex-canvas").is_file() {
                resources
            } else {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .ancestors()
                    .nth(5)
                    .ok_or("Cannot locate development resources.")?
                    .to_path_buf()
            };
            let allow_start = true;
            #[cfg(feature = "native-check")]
            let allow_start = allow_start && native_check::directory().is_none();
            local_backend::verify(&origin, &resources, allow_start).await?;
            *verified = true;
        }
    }
    let client = {
        let mut local = state.local.lock().await;
        if local.is_none() {
            let directory =
                match std::env::var_os("CODEX_DESKTOP_PROFILE").filter(|v| !v.is_empty()) {
                    Some(path) => std::path::PathBuf::from(path),
                    None => window
                        .app_handle()
                        .path()
                        .app_data_dir()
                        .map_err(|error| error.to_string())?,
                }
                .join("client/local");
            #[cfg(feature = "native-check")]
            let directory = native_check::directory().unwrap_or(directory);
            let events_window = window.clone();
            *local = Some(Arc::new(Client::open(
                ConnectionConfig {
                    state_directory: directory,
                    backend_origin: origin,
                },
                move |event| {
                    let _ = events_window.emit("studio-client-event", event);
                },
            )?));
        }
        local
            .as_ref()
            .ok_or("The client did not initialize.")?
            .clone()
    };
    let method = request.method.clone();
    if method.starts_with("drafts.") {
        let workspace = state.workspace.lock().await;
        if !draft_workspace_allowed(workspace.as_deref(), &request.params) {
            return Err("Draft commands require the verified workspace identity.".into());
        }
    }
    let response = client.request(request).await?;
    if matches!(method.as_str(), "sync.open" | "identity.get") && response.error.is_none() {
        let Some(workspace) = response
            .result
            .as_ref()
            .and_then(|value| value["workspaceId"].as_str())
        else {
            if method == "identity.get" {
                return Ok(response);
            }
            return Err("The backend returned no workspace identity.".into());
        };
        let mut current = state.workspace.lock().await;
        if current.as_deref().is_some_and(|value| value != workspace) {
            return Err("The server workspace changed. Reload to synchronize.".into());
        }
        *current = Some(workspace.to_owned());
    }
    Ok(response)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(feature = "native-check")]
    eprintln!("Hidden native check: starting host");
    tauri::Builder::default()
        .register_uri_scheme_protocol("studio-view", frame_assets::respond)
        .manage(Clients::default())
        .manage(close::CloseState::default())
        .invoke_handler(tauri::generate_handler![
            client_request,
            close::close_ready,
            close::close_response
        ])
        .on_window_event(|window, event| {
            #[cfg(desktop)]
            window_state::event(window, event);
            if let tauri::WindowEvent::CloseRequested { api, .. } = event
                && window.label() == "main" && close::request(window.app_handle()) {
                    api.prevent_close();
                }
        })
        .on_page_load(|window, payload| {
            #[cfg(feature = "native-check")]
            if payload.event() == tauri::webview::PageLoadEvent::Finished {
                native_check::loaded(window);
            }
            #[cfg(not(feature = "native-check"))]
            let _ = (window, payload);
        })
        .setup(|app| {
            let hidden = std::env::args().any(|argument| argument == "--hidden");
            #[cfg(feature = "native-check")]
            let hidden = native_check::setup(app)? || hidden;
            let directory = std::env::var_os("CODEX_DESKTOP_PROFILE")
                .map(std::path::PathBuf::from)
                .unwrap_or(app.path().app_data_dir()?);
            #[cfg(feature = "native-check")]
            let directory = native_check::directory().unwrap_or(directory);
            app.manage(lock_profile(&directory)?);
            #[cfg(desktop)]
            desktop_menu::install(app)?;
            #[cfg(all(desktop, unix))]
            let closes_on_signal = close::close_on_terminate(app.handle())
                .inspect_err(|error| eprintln!("{error}"))
                .is_ok();
            #[cfg(all(desktop, not(unix)))]
            let closes_on_signal = false;
            #[cfg(desktop)]
            if let Err(error) = desktop_recovery::install(app, &directory, hidden, closes_on_signal) {
                eprintln!("Desktop recovery is unavailable: {error}");
            }
            let handle = app.handle().clone();
            let mut navigation = studio_client_core::notifications::navigation().subscribe();
            tauri::async_runtime::spawn(async move {
                loop {
                    match navigation.recv().await {
                        Ok(target) => {
                            let _ = handle.emit("studio-native-navigate", target);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });
            let window = tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .title("LAYR Studio")
            // The renderer owns sidebar/queue reordering and file attachments.
            // Tauri's native handler consumes drops before HTML5 can receive them.
            .disable_drag_drop_handler()
            .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny)
            .inner_size(1200.0, 800.0)
            .visible(!hidden);
            // Explicit test profiles never reuse the installed WebKit data store.
            let ephemeral = std::env::var("CODEX_DESKTOP_EPHEMERAL_PROFILE").as_deref() == Ok("1");
            if ephemeral && std::env::var_os("CODEX_DESKTOP_PROFILE").is_none() {
                return Err("An ephemeral profile requires CODEX_DESKTOP_PROFILE.".into());
            }
            #[cfg(feature = "native-check")]
            let ephemeral = ephemeral || (native_check::directory().is_some() && !native_check::persistent_profile());
            let window = window.incognito(ephemeral);
            // Keep the installed default store. Explicit persistent profiles
            // must never fall back to that store or share another profile's IDB.
            // The recovery launcher passes the saved default path explicitly.
            // Keep its original WebKit store instead of treating that restart as
            // a new isolated profile.
            let custom_profile = std::env::var_os("CODEX_DESKTOP_PROFILE").is_some_and(|v| !v.is_empty())
                && !(std::env::args().any(|arg| arg == "--background-recovery")
                    && directory == app.path().app_data_dir()?);
            let window = if custom_profile && !ephemeral {
                window.data_store_identifier(studio_client_core::platform::webkit_profile_identifier(&directory)?)
            } else { window };
            #[cfg(feature = "native-check")]
            let window = match native_check::data_store_identifier()? {
                Some(identifier) => window.incognito(false).data_store_identifier(identifier),
                None => window,
            };
            #[cfg(feature = "native-check")]
            let window = window.initialization_script_for_all_frames(
                r#"
                if (window !== window.parent && location.protocol === 'studio-view:') {
                    addEventListener('DOMContentLoaded', () => {
                        let parentReadable = false;
                        try { parentReadable = !!parent.location.href; } catch {}
                        parent.postMessage({ kind: 'native-frame-probe', parentReadable,
                            native: !!window.__TAURI__, origin: location.origin,
                            parsedOrigin: new URL(location.href).origin }, 'tauri://localhost');
                    }, { once: true });
                }
            "#,
            );
            #[cfg(feature = "native-check")]
            let window = if std::env::var("STUDIO_NATIVE_CHECK_MODE").as_deref() == Ok("network-shell") {
                window.initialization_script_for_all_frames(r#"
                if (window === window.parent) {
                  // A paired registry entry selects the real multi-server shell.
                  // Its absent credentials prevent any external fixture traffic.
                  localStorage.setItem('studio-paired-servers-v1', JSON.stringify([{
                    id:'fixture-remote', label:'Fixture remote', origin:'https://fixture.invalid.ts.net',
                    credentialId:'fixture-missing-credential'
                  }]));
                }
                "#)
            } else { window };
            #[cfg(desktop)]
            let window = window.focused(!hidden);
            let window = window.build()?;
            #[cfg(desktop)]
            window_state::install(&window, &directory, hidden)?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("The native Studio host could not start")
        .run(|app, event| {
            #[cfg(desktop)]
            if matches!(event, tauri::RunEvent::Exit) {
                window_state::flush(app);
                desktop_recovery::close(app);
            }
            if let tauri::RunEvent::ExitRequested {
                code: None, api, ..
            } = event
                && close::request(app) {
                    api.prevent_exit();
                }
        });
}

/// Offline migration entry point. Secrets arrive on stdin, never argv or a
/// renderer message. The client lock excludes concurrent native store owners.
pub fn import_server_credentials() -> Result<(), String> {
    #[cfg(not(target_os = "macos"))]
    return Err("The native server key store is unavailable on this platform.".into());
    #[cfg(target_os = "macos")]
    {
        use std::io::Read;
        let args = std::env::args_os().collect::<Vec<_>>();
        if args.len() != 3 {
            return Err("Supply the destination native profile.".into());
        }
        let directory = std::path::PathBuf::from(&args[2]);
        if !directory.is_absolute() {
            return Err("Use an absolute native profile path.".into());
        }
        std::fs::create_dir_all(directory.join("client/local")).map_err(|e| e.to_string())?;
        let directory = directory.canonicalize().map_err(|e| e.to_string())?;
        let _lock = lock_profile(&directory)?;
        let mut bytes = Vec::new();
        std::io::stdin()
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "Could not read server credentials.")?;
        if bytes.len() > 1024 * 1024 {
            return Err("Server credentials exceed the import limit.".into());
        }
        let source =
            serde_json::from_slice(&bytes).map_err(|_| "Invalid server credential archive.")?;
        let vault = studio_client_core::server_keys::KeychainVault {
            account: directory.to_string_lossy().into_owned(),
        };
        studio_client_core::server_keys::import(&vault, &source)?;
        println!("Server credentials imported.");
        Ok(())
    }
}
