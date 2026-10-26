mod archive_import;
#[cfg(feature = "native-check")]
mod attachment_event_privacy;
mod close;
#[cfg(desktop)]
mod desktop_menu;
#[cfg(desktop)]
mod desktop_recovery;
mod frame_assets;
mod legacy_capture;
mod local_backend;
mod migration_host;
#[cfg(feature = "native-check")]
mod native_check;
mod native_origin_inventory;
mod native_registry;
mod profile;
mod service_context;
mod ui_events;
#[cfg(desktop)]
mod window_state;

use std::sync::Arc;
use studio_client_core::{
    client::Client,
    protocol::{Request, Response},
};
use tauri::{Emitter, Manager, State, WebviewWindow};
use tokio::sync::Mutex;

#[derive(Default)]
struct Clients {
    contexts: Mutex<service_context::Contexts>,
    migration: migration_host::MigrationHost,
    cutovers: Mutex<service_context::Cutovers>,
    legacy_write_admission: Mutex<bool>,
    archive_preparation: Mutex<()>,
    local: Mutex<Option<Arc<Client>>>,
    local_backend: Mutex<Option<studio_client_core::backend::Backend>>,
    sources: Mutex<
        std::collections::HashMap<String, (Arc<Client>, studio_client_core::backend::Backend)>,
    >,
    #[cfg(target_os = "macos")]
    remote: Mutex<Option<Arc<studio_client_core::server_transport::RemoteClient>>>,
    backend_verified: Mutex<bool>,
    workspace: Mutex<Option<String>>,
}

// Held for the entire host lifetime, including before local client creation.
struct ProfileLock {
    #[allow(dead_code)]
    file: std::fs::File,
    directory: std::path::PathBuf,
}
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
    Ok(ProfileLock {
        file,
        directory: directory.canonicalize().map_err(|e| e.to_string())?,
    })
}

async fn cutover_source(
    window: &WebviewWindow,
    state: &Clients,
    server: &str,
) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    trusted(window)?;
    if window.app_handle().webview_windows().len() != 1 {
        return Err("Close other native WebViews before the storage cutover.".into());
    }
    let lock = window
        .app_handle()
        .try_state::<ProfileLock>()
        .ok_or("The native profile lock is not held.")?;
    let pairing = if server == "local" {
        "local".into()
    } else {
        #[cfg(not(target_os = "macos"))]
        return Err("The native server key store is unavailable on this platform.".into());
        #[cfg(target_os = "macos")]
        remote_client(window, state)
            .await?
            .bind(server)?
            .storage_key()
    };
    let owner = if server == "local" {
        "root".to_owned()
    } else {
        format!("server:{server}")
    };
    let archive = profile::seal_identity(&owner)?;
    let bytes = serde_json::to_vec(&(
        &lock.directory,
        server,
        pairing,
        &archive["id"],
        &archive["sha256"],
    ))
    .map_err(|e| e.to_string())?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Select a Store without a network request or a renderer-selected path. Keep
/// the Client alive so a status read and later context share one file lock.
async fn service_source(
    window: &WebviewWindow,
    state: State<'_, Clients>,
    server: &str,
) -> Result<(Arc<Client>, Option<studio_client_core::backend::Backend>), String> {
    if server == "local" {
        Box::pin(client_request(
            window.clone(),
            state.clone(),
            Request {
                id: studio_client_core::server_keys::random_id()?,
                method: "preferences.raw_all".into(),
                params: serde_json::json!({}),
            },
        ))
        .await?;
        return Ok((
            state
                .local
                .lock()
                .await
                .as_ref()
                .cloned()
                .ok_or("The native Store is unavailable.")?,
            Some(
                state
                    .local_backend
                    .lock()
                    .await
                    .as_ref()
                    .cloned()
                    .ok_or("The local transport identity is unavailable.")?,
            ),
        ));
    }
    #[cfg(not(target_os = "macos"))]
    return Err("The native server key store is unavailable on this platform.".into());
    #[cfg(target_os = "macos")]
    {
        let mut sources = state.sources.lock().await;
        if let Some((client, backend)) = sources.get(server) {
            backend.validate_identity()?;
            return Ok((client.clone(), Some(backend.clone())));
        }
        let binding = remote_client(window, &state).await?.bind(server)?;
        let directory = native_profile_directory(window)?
            .join("client/servers")
            .join(binding.storage_key());
        let backend = studio_client_core::backend::Backend::from_remote(binding)?;
        let events = window.clone();
        let client = Arc::new(Client::open_with_backend(
            directory,
            backend.clone(),
            move |event| {
                let _ = events.emit("studio-client-event", event);
            },
        )?);
        sources.insert(server.into(), (client.clone(), backend.clone()));
        Ok((client, Some(backend)))
    }
}

fn draft_workspace_allowed(verified: Option<&str>, params: &serde_json::Value) -> bool {
    verified.is_some() && verified == params["workspace"].as_str()
}

fn native_profile_directory(window: &WebviewWindow) -> Result<std::path::PathBuf, String> {
    match std::env::var_os("CODEX_DESKTOP_PROFILE").filter(|v| !v.is_empty()) {
        Some(path) => Ok(std::path::PathBuf::from(path)),
        None => window
            .app_handle()
            .path()
            .app_data_dir()
            .map_err(|e| e.to_string()),
    }
}

#[cfg(target_os = "macos")]
async fn remote_client(
    window: &WebviewWindow,
    state: &Clients,
) -> Result<Arc<studio_client_core::server_transport::RemoteClient>, String> {
    let mut remote = state.remote.lock().await;
    if remote.is_none() {
        let directory = native_profile_directory(window)?;
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
    Ok(remote
        .as_ref()
        .ok_or("The native server transport is unavailable.")?
        .clone())
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

    // A style nonce makes the WebView ignore 'unsafe-inline' in style-src.
    // Srcdoc previews inherit that policy and lose their styles; see
    // repository-checks/tests/native-style-policy.test.mjs.
    #[test]
    fn bundled_documents_keep_the_configured_inline_style_policy() {
        let context = super::context();
        let modification = &context
            .config()
            .app
            .security
            .dangerous_disable_asset_csp_modification;
        assert!(!modification.can_modify("style-src"));
        assert!(modification.can_modify("script-src"));
        let key = "index.html".into();
        let index = context.assets().get(&key).expect("bundled index.html");
        let index = String::from_utf8_lossy(&index);
        assert!(!index.contains(tauri::utils::assets::STYLE_NONCE_TOKEN));
        assert!(
            context
                .assets()
                .csp_hashes(&key)
                .all(|hash| hash.directive() != "style-src")
        );
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
    #[cfg(feature = "native-check")]
    if let Some(response) = attachment_event_privacy::dispatch(&window, &request).await? {
        return Ok(response);
    }
    if request.method == ui_events::CONNECT_METHOD {
        return ui_events::connect(&window, &request.params)
            .map(|value| Response::success(request.id, value));
    }
    if request.method == ui_events::DISCONNECT_METHOD {
        return ui_events::disconnect(&window, &request.params)
            .map(|value| Response::success(request.id, value));
    }
    // Drain compatibility effects with the same gate as old storage writers.
    // Private bootstrap reads never enter this path.
    let _legacy_effect = if let Some(server) = service_context::legacy_effect_scope(&request) {
        let admission = state.legacy_write_admission.lock().await;
        if *admission {
            let source = cutover_source(&window, &state, server).await?;
            state.cutovers.lock().await.verify(server, None, &source)?;
            let owner = if server == "local" {
                "root".to_owned()
            } else {
                format!("server:{server}")
            };
            if !profile::seal_identity(&owner)?.is_null() {
                return Err("The sealed archive needs privileged source inventory verification before native effects can start.".into());
            }
            let (client, _) = service_source(&window, state.clone(), server).await?;
            let status = client
                .request(Request {
                    id: studio_client_core::server_keys::random_id()?,
                    method: "client_storage.cutover_status".into(),
                    params: serde_json::json!({"sourceIdentity":source,"serverId":server}),
                })
                .await?;
            if !status
                .result
                .as_ref()
                .is_some_and(|value| value["complete"] == true)
            {
                return Err(
                    "Native storage migration is incomplete. Legacy effects remain disabled."
                        .into(),
                );
            }
        }
        Some(admission)
    } else {
        None
    };
    let _legacy_write = if matches!(
        request.method.as_str(),
        "preferences.raw_set"
            | "preferences.raw_remove"
            | "preferences.import"
            | "preferences.set"
            | "draft.set"
            | "drafts.import"
            | "drafts.edit"
            | "drafts.dismiss"
            | "drafts.refresh"
    ) {
        let admission = state.legacy_write_admission.lock().await;
        if *admission {
            return Err("The native storage cutover closed legacy writes. Keep the original pending request for migration.".into());
        }
        Some(admission)
    } else {
        None
    };
    if matches!(
        request.method.as_str(),
        "system.info" | "system.uuid" | "system.uuidBatch" | "system.sha256"
    ) {
        return Ok(studio_client_core::platform::dispatch(&request));
    }
    if request.method == "service.storage_cutover" {
        let selected: service_context::Open = serde_json::from_value(request.params)
            .map_err(|_| "Invalid native storage cutover selector.")?;
        let source = cutover_source(&window, &state, &selected.server_id).await?;
        // This gate drains every already admitted legacy native write.
        let mut admission = state.legacy_write_admission.lock().await;
        *admission = true;
        let mut receipt = state
            .cutovers
            .lock()
            .await
            .begin(&selected.server_id, &source)?;
        let owner = if selected.server_id == "local" {
            "root".to_owned()
        } else {
            format!("server:{}", selected.server_id)
        };
        receipt["archive"] = profile::seal_identity(&owner)?;
        // A process restart needs a fresh private source capture. Reading this
        // guard must not open a writable Store or promote a legacy completion.
        receipt["migrationComplete"] = false.into();
        return Ok(Response::success(request.id, receipt));
    }
    if request.method == "service.verify_cutover" {
        let selected: service_context::CutoverCheck = serde_json::from_value(request.params)
            .map_err(|_| "Invalid native storage cutover proof.")?;
        let source = cutover_source(&window, &state, &selected.server_id).await?;
        state
            .cutovers
            .lock()
            .await
            .verify(&selected.server_id, Some(&selected.token), &source)?;
        return Ok(Response::success(
            request.id,
            serde_json::json!({"verified":true}),
        ));
    }
    if request.method == "service.prepare_archive" {
        let selected: service_context::CutoverCheck = serde_json::from_value(request.params)
            .map_err(|_| "Invalid native archive preparation selector.")?;
        let _preparing = state.archive_preparation.lock().await;
        let source = cutover_source(&window, &state, &selected.server_id).await?;
        state
            .cutovers
            .lock()
            .await
            .verify(&selected.server_id, Some(&selected.token), &source)?;
        let context = state
            .contexts
            .lock()
            .await
            .existing(&selected.server_id)
            .map(|(_, context)| context)
            .ok_or("Open the exact server context before archive preparation.")?;
        if context.source_identity != source {
            return Err("The archive destination identity changed.".into());
        }
        let owner = if selected.server_id == "local" {
            "root".into()
        } else {
            format!("server:{}", selected.server_id)
        };
        let archive = profile::sealed(&owner)?;
        let prepared = archive_import::prepare(&context, &archive).await?;
        let after = cutover_source(&window, &state, &selected.server_id).await?;
        state
            .cutovers
            .lock()
            .await
            .verify(&selected.server_id, Some(&selected.token), &after)?;
        return Ok(Response::success(request.id, prepared));
    }
    if request.method == "service.finish_cutover" {
        return Err(
            "Only the private host source capture can complete native storage migration.".into(),
        );
    }
    if request.method == "service.legacy_preferences" {
        let legacy: service_context::LegacyPreferences = serde_json::from_value(request.params)
            .map_err(|_| "Invalid native preference recovery source.")?;
        let source = cutover_source(&window, &state, &legacy.server_id).await?;
        state
            .cutovers
            .lock()
            .await
            .verify(&legacy.server_id, None, &source)?;
        let contexts = state.contexts.lock().await;
        if (legacy.server_id == "local" && !contexts.is_empty())
            || contexts.existing(&legacy.server_id).is_some()
        {
            return Err(
                "Recover the original preference journal before opening server services.".into(),
            );
        }
        if legacy.server_id != "local" {
            #[cfg(not(target_os = "macos"))]
            return Err("The native server key store is unavailable on this platform.".into());
            #[cfg(target_os = "macos")]
            remote_client(&window, &state)
                .await?
                .bind(&legacy.server_id)?
                .validate()?;
        }
        Box::pin(client_request(
            window.clone(),
            state.clone(),
            Request {
                id: studio_client_core::server_keys::random_id()?,
                method: "preferences.raw_all".into(),
                params: serde_json::json!({}),
            },
        ))
        .await?;
        let client = state
            .local
            .lock()
            .await
            .as_ref()
            .cloned()
            .ok_or("The original native preference store is unavailable.")?;
        let scope = if legacy.server_id == "local" {
            "root".to_owned()
        } else {
            format!("server:{}", legacy.server_id)
        };
        return client
            .request(Request {
                id: request.id,
                method: "client_storage.replay_preferences".into(),
                params: serde_json::json!({"scope":scope,"writes":legacy.writes}),
            })
            .await;
    }
    if request.method == "service.open" {
        let selected: service_context::Open = serde_json::from_value(request.params.clone())
            .map_err(|_| "Invalid native server selector.")?;
        let source = cutover_source(&window, &state, &selected.server_id).await?;
        state
            .cutovers
            .lock()
            .await
            .verify(&selected.server_id, None, &source)?;
        let mut contexts = state.contexts.lock().await;
        if let Some((handle, context)) = contexts.existing(&selected.server_id) {
            if let Some(backend) = &context.backend {
                backend.validate_identity()?;
            }
            return Ok(Response::success(request.id, context.descriptor(&handle)));
        }
        // Only the old local Store supplies native preference receipts.
        Box::pin(client_request(
            window.clone(),
            state.clone(),
            Request {
                id: studio_client_core::server_keys::random_id()?,
                method: "preferences.raw_all".into(),
                params: serde_json::json!({}),
            },
        ))
        .await?;
        let local = state
            .local
            .lock()
            .await
            .as_ref()
            .cloned()
            .ok_or("The local client is unavailable.")?;
        let (client, backend) = service_source(&window, state.clone(), &selected.server_id).await?;
        let workspace = if selected.server_id == "local" {
            let mut identity = Box::pin(client_request(
                window.clone(),
                state.clone(),
                Request {
                    id: studio_client_core::server_keys::random_id()?,
                    method: "identity.get".into(),
                    params: serde_json::json!({}),
                },
            ))
            .await?;
            if identity
                .result
                .as_ref()
                .is_some_and(serde_json::Value::is_null)
            {
                identity = Box::pin(client_request(
                    window.clone(),
                    state.clone(),
                    Request {
                        id: studio_client_core::server_keys::random_id()?,
                        method: "sync.open".into(),
                        params: serde_json::json!({}),
                    },
                ))
                .await?;
            }
            let local_identity = identity.result.ok_or_else(|| {
                identity
                    .error
                    .map(|e| e.message)
                    .unwrap_or_else(|| "The local client workspace is unavailable.".into())
            })?;
            local_identity
        } else {
            #[cfg(not(target_os = "macos"))]
            return Err("Paired native service contexts are unavailable on this platform.".into());
            #[cfg(target_os = "macos")]
            {
                let mut identity = client
                    .request(Request {
                        id: studio_client_core::server_keys::random_id()?,
                        method: "identity.get".into(),
                        params: serde_json::json!({}),
                    })
                    .await?;
                if identity
                    .result
                    .as_ref()
                    .is_some_and(serde_json::Value::is_null)
                {
                    identity = client
                        .request(Request {
                            id: studio_client_core::server_keys::random_id()?,
                            method: "sync.open".into(),
                            params: serde_json::json!({}),
                        })
                        .await?;
                }
                let identity = identity.result.ok_or_else(|| {
                    identity
                        .error
                        .map(|e| e.message)
                        .unwrap_or_else(|| "The paired workspace is unavailable.".into())
                })?;
                identity
            }
        };
        let workspace_id = workspace["workspaceId"]
            .as_str()
            .filter(|value| studio_client_core::projections::valid_workspace(value))
            .ok_or("Invalid native workspace identity.")?
            .to_owned();
        let scope = if selected.server_id == "local" {
            "root".to_owned()
        } else {
            format!("server:{}", selected.server_id)
        };
        let exported = local
            .request(Request {
                id: studio_client_core::server_keys::random_id()?,
                method: "client_storage.export_preferences".into(),
                params: serde_json::json!({"scope":scope}),
            })
            .await?;
        let exported = exported.result.ok_or_else(|| {
            exported.error.map(|e| e.message).unwrap_or_else(|| {
                "The existing native preference receipts could not be read.".into()
            })
        })?;
        let serialized_preferences = serde_json::to_string(&exported).map_err(|e| e.to_string())?;
        let imported = client.request(Request { id: studio_client_core::server_keys::random_id()?, method: "client_storage.import".into(), params: serde_json::json!({"manifestId":format!("legacy-native-preferences:{scope}"),"serializedSource":serialized_preferences}) }).await?;
        if let Some(error) = imported.error {
            return Err(error.message);
        }
        let descriptor = contexts.insert(service_context::Context {
            server_id: selected.server_id,
            workspace_id,
            client,
            backend,
            source_identity: source,
            migration_complete: Default::default(),
            archive_hold: !profile::seal_identity(&scope)?.is_null(),
        })?;
        return Ok(Response::success(request.id, descriptor));
    }
    if request.method == "service.request" {
        let call: service_context::Call = serde_json::from_value(request.params.clone())
            .map_err(|_| "Invalid native service request.")?;
        let context = state.contexts.lock().await.get(&call.handle)?;
        let mut scoped = context.admission(request.id, call)?;
        context.ensure_ready(&scoped).await?;
        // Capture the host gesture before any verification or network wait.
        if matches!(
            scoped.method.as_str(),
            "desktop.pickFiles"
                | "desktop.pickDirectory"
                | "desktop.revealPath"
                | "desktop.showVaultKeys"
                | "desktop.fileAction"
                | "desktop.requestMicrophone"
                | "desktop.prepareTranscription"
                | "desktop.saveRecording"
                | "desktop.downloadAndSave"
                | "desktop.saveText"
        ) {
            let params = scoped
                .params
                .as_object_mut()
                .ok_or("Invalid native picker parameters.")?;
            params.remove("permit");
            let (send, receive) = tokio::sync::oneshot::channel();
            let method = scoped.method.clone();
            window
                .run_on_main_thread(move || {
                    let _ = send.send(studio_client_core::desktop::gesture_for_method(&method));
                })
                .map_err(|e| e.to_string())?;
            let permit = receive
                .await
                .map_err(|_| "The native picker was cancelled.")?;
            if permit == 0 {
                return Err("Use a desktop button or keyboard action.".into());
            }
            params.insert("permit".into(), permit.into());
        }
        let backend_verified = *state.backend_verified.lock().await;
        if context.server_id == "local"
            && service_context::needs_network(&scoped)
            && !backend_verified
        {
            let verified = Box::pin(client_request(
                window.clone(),
                state.clone(),
                Request {
                    id: studio_client_core::server_keys::random_id()?,
                    method: "sync.open".into(),
                    params: serde_json::json!({}),
                },
            ))
            .await?;
            if verified
                .result
                .as_ref()
                .and_then(|value| value["workspaceId"].as_str())
                != Some(context.workspace_id.as_str())
            {
                return Err("The configured backend does not match this server context.".into());
            }
        }
        return context.send(scoped).await;
    }
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
            let client = remote_client(&window, &state).await?;
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
            let resources = if resources.join("bin/studio-runtime-server").is_file() {
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
            if let Some(backend) = state.local_backend.lock().await.as_ref() {
                backend.authorize();
            }
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
            let backend = studio_client_core::backend::Backend::desktop_client(&origin)?;
            if *state.backend_verified.lock().await {
                backend.authorize();
            }
            *state.local_backend.lock().await = Some(backend.clone());
            *local = Some(Arc::new(Client::open_with_backend(
                directory,
                backend,
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

// One expansion embeds the bundled renderer; tests read the same context.
fn context() -> tauri::Context<tauri::Wry> {
    tauri::generate_context!()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(feature = "native-check")]
    eprintln!("Hidden native check: starting host");
    tauri::Builder::default()
        .register_uri_scheme_protocol("studio-view", frame_assets::respond)
        .manage(Clients::default())
        .manage(ui_events::UiEvents::default())
        .manage(close::CloseState::default())
        .invoke_handler(tauri::generate_handler![
            client_request,
            close::close_ready,
            close::close_response
        ])
        .on_window_event(|window, event| {
            if matches!(event, tauri::WindowEvent::Destroyed) {
                if let Err(error) = ui_events::retire_window(window.app_handle(), window.label()) {
                    eprintln!("{error}");
                }
            }
            #[cfg(desktop)]
            window_state::event(window, event);
            if let tauri::WindowEvent::CloseRequested { api, .. } = event
                && window.label() == "main" && close::request(window.app_handle()) {
                    api.prevent_close();
                }
        })
        .on_page_load(|window, payload| {
            if payload.event() == tauri::webview::PageLoadEvent::Started {
                if let Err(error) = ui_events::retire_window(window.app_handle(), window.label()) {
                    eprintln!("{error}");
                }
            }
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
            #[cfg(feature = "native-check")]
            let window = if native_check::directory().is_some()
                && std::env::var("STUDIO_NATIVE_CHECK_MODE").as_deref()
                    == Ok(attachment_event_privacy::MODE)
            {
                window.initialization_script_for_all_frames(
                    attachment_event_privacy::frame_initialization_script(),
                )
            } else {
                window
            };
            #[cfg(desktop)]
            let window = window.focused(!hidden);
            let window = window.build()?;
            #[cfg(desktop)]
            window_state::install(&window, &directory, hidden)?;
            Ok(())
        })
        .build(context())
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
