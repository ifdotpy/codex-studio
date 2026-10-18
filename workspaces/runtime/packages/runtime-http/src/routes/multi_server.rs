//! Multi-server local audit reads and explicit transport-dependent operations.

use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rusqlite::OptionalExtension;
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use studio_runtime_services::federation_credentials::CredentialError;
use studio_runtime_services::multi_server::{self, MultiServerError};
use studio_runtime_services::remote_transport::{self, MultiServerExchangeInput};
use studio_runtime_services::resource_notify::ResourceNotifications;

use crate::error::ApiError;
use crate::router::HttpState;

const PLATFORM_DEPENDENCIES: [Dependency; 1] =
    [Dependency::new("server-credentials/platform-support")];
const MANAGEMENT_DEPENDENCIES: &[Dependency] = &[
    Dependency::new("multi-server-runtime/orchestration"),
    Dependency::new("multi-server-runtime/pairing"),
    Dependency::new("remote-transport/owner-lookup"),
    Dependency::new("remote-transport/signed-server-requests"),
    #[cfg(not(unix))]
    Dependency::new("server-credentials/platform-support"),
];
const ACCESS_ACTIVITY_TIMESTAMP_FIELD: &str = "lastAccess";
const DISCOVERY_OBSERVATION_TIMESTAMP_FIELD: &str = "lastSeen";

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/multi-server",
            if cfg!(unix) {
                OperationStatus::Implemented
            } else {
                OperationStatus::partial(&PLATFORM_DEPENDENCIES)
            },
            snapshot,
        ),
        get(
            "/api/multi-server/audit",
            OperationStatus::Implemented,
            audit,
        ),
        post(
            "/api/multi-server",
            if cfg!(unix) {
                OperationStatus::Implemented
            } else {
                OperationStatus::partial(MANAGEMENT_DEPENDENCIES)
            },
            manage,
        ),
        post(
            "/api/multi-server/v1/auto-pair",
            OperationStatus::Implemented,
            auto_pair,
        ),
        get(
            "/api/multi-server/v1/identity",
            OperationStatus::Implemented,
            identity,
        ),
        post(
            "/api/multi-server/v1/name",
            OperationStatus::Implemented,
            rename,
        ),
        post(
            "/api/multi-server/v1/pair",
            OperationStatus::Implemented,
            pair,
        ),
        get(
            "/api/multi-server/v1/status",
            OperationStatus::Implemented,
            status,
        ),
        post(
            "/api/servers/orchestration",
            OperationStatus::Implemented,
            orchestration,
        ),
    ])
}

async fn snapshot(
    State(state): State<HttpState>,
) -> Result<Json<crate::generated::AccessSnapshot>, ApiError> {
    if !cfg!(unix) {
        return Err(unavailable_for(
            "GET",
            "/api/multi-server",
            &PLATFORM_DEPENDENCIES,
        ));
    }
    let credentials = state.credentials;
    let origin = state.public_origin;
    let store = state.store.ok_or_else(ApiError::internal)?;
    let identity = credentials
        .identity(Arc::clone(&store), origin.as_deref())
        .await
        .map_err(map_credential_error)?;
    let result = multi_server::snapshot(&store, &identity)
        .await
        .map_err(map_service_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::AccessSnapshot,
    >(result)?))
}

pub(super) async fn read_snapshot(state: &HttpState) -> Result<Value, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let credentials = state.credentials.clone();
    let origin = state.public_origin.clone();
    let identity = credentials
        .identity(Arc::clone(&store), origin.as_deref())
        .await
        .map_err(map_credential_error)?;
    multi_server::snapshot_for_comparison(&store, &identity)
        .await
        .map_err(map_service_error)
}

pub(super) fn publish_resource_change(notifications: &ResourceNotifications, request_id: &str) {
    let request = serde_json::json!({
        "requestId": request_id,
        "resources": [{"kind": "multi-server"}],
    });
    if let Err(error) = notifications.publish_once(&request) {
        // The in-memory publisher is unavailable only while its stream hub is
        // closing or its publication state is poisoned. Closing forces streams
        // to reconnect and receive the current baseline. Never fail the already
        // committed command response.
        eprintln!("Could not publish committed multi-server change: {error}");
    }
}

fn publish_uncertain_write_hint(notifications: &ResourceNotifications) {
    // An error can occur after a durable write. Keep this recovery hint on a
    // fresh internal ID so it cannot consume the command's success receipt if
    // that same command later retries successfully.
    let notification_id = uuid::Uuid::new_v4().simple().to_string();
    publish_resource_change(notifications, &notification_id);
}

pub(super) fn notify_snapshot_change(
    notifications: &ResourceNotifications,
    request_id: &str,
    before: &Value,
    after: &Value,
) {
    if semantic_snapshot(before) != semantic_snapshot(after) {
        publish_resource_change(notifications, request_id);
    }
}

fn semantic_snapshot(snapshot: &Value) -> Value {
    let mut semantic = snapshot.clone();
    for collection in ["clients", "servers"] {
        let Some(records) = semantic.get_mut(collection).and_then(Value::as_array_mut) else {
            continue;
        };
        for record in records {
            if let Some(record) = record.as_object_mut() {
                record.remove(ACCESS_ACTIVITY_TIMESTAMP_FIELD);
                record.remove(DISCOVERY_OBSERVATION_TIMESTAMP_FIELD);
            }
        }
    }
    semantic
}

async fn audit(
    State(state): State<HttpState>,
) -> Result<Json<crate::generated::AccessAuditResponse>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/multi_server",
        move || multi_server::audit(&store),
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(map_service_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::AccessAuditResponse,
    >(result)?))
}

async fn manage(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Response, ApiError> {
    if !cfg!(unix) {
        return Err(unavailable_for(
            "POST",
            "/api/multi-server",
            MANAGEMENT_DEPENDENCIES,
        ));
    }
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if action == "create_invite" {
        let request_id = body["requestId"]
            .as_str()
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Invalid request ID"))?
            .to_owned();
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        let credentials = state.credentials.clone();
        let origin = state.public_origin.clone();
        let whoami = state.tailscale_whoami.clone();
        let notifications = state.resource_notifications.clone();
        if origin.as_deref().is_none_or(str::is_empty) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "Configure Tailscale Serve HTTPS before pairing",
            ));
        }
        let identity = credentials
            .identity(Arc::clone(&store), origin.as_deref())
            .await
            .map_err(map_credential_error)?;
        let owner = match whoami {
            Some(probe) => probe().await.and_then(|value| tailscale_login(&value)),
            None => None,
        }
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "The Tailscale owner identity is unavailable",
            )
        })?;
        let result = multi_server::create_invite(
            Arc::clone(&store),
            &identity,
            &body,
            "local",
            Some(&owner),
        )
        .await
        .map_err(map_service_error)?;
        publish_resource_change(&notifications, &request_id);
        return Ok(Json(result).into_response());
    }
    if action == "accept_invite" {
        let request_id = body["requestId"]
            .as_str()
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Invalid request ID"))?
            .to_owned();
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        let credentials = state.credentials.clone();
        let origin = state.public_origin.clone();
        let whoami = state.tailscale_whoami.clone();
        let notifications = state.resource_notifications.clone();
        let result = match accept_invite(
            Arc::clone(&store),
            &credentials,
            origin.as_deref(),
            whoami,
            &body,
        )
        .await
        {
            Ok(result) => {
                publish_resource_change(&notifications, &request_id);
                result
            }
            Err(error) => {
                publish_uncertain_write_hint(&notifications);
                return Err(error);
            }
        };
        return Ok(Json(result).into_response());
    }
    if action == "ui_invite" {
        let request_id = body["requestId"]
            .as_str()
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Invalid request ID"))?
            .to_owned();
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        let notifications = state.resource_notifications.clone();
        let result = ui_invite(Arc::clone(&store), &body).await?;
        publish_resource_change(&notifications, &request_id);
        return Ok(Json(result).into_response());
    }
    if action == "discover"
        && let Some(origin) = state.public_origin.clone()
    {
        let before = read_snapshot(&state).await?;
        let request_id = body["requestId"]
            .as_str()
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Invalid request ID"))?
            .to_owned();
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        let credentials = state.credentials.clone();
        let discovery = state
            .server_discovery
            .clone()
            .ok_or_else(|| unavailable_for("POST", "/api/multi-server", MANAGEMENT_DEPENDENCIES))?;
        let notifications = state.resource_notifications.clone();
        let identity = credentials
            .identity(Arc::clone(&store), Some(&origin))
            .await
            .map_err(map_credential_error)?;
        let result = match discovery
            .discover(Arc::clone(&store), &identity, &origin)
            .await
        {
            Ok(result) => {
                notify_snapshot_change(&notifications, &request_id, &before, &result);
                result
            }
            Err(error) => {
                publish_uncertain_write_hint(&notifications);
                return Err(map_discovery_error(error));
            }
        };
        return Ok(
            Json(super::generated_json::<crate::generated::AccessSnapshot>(
                result,
            )?)
            .into_response(),
        );
    }
    if matches!(
        action,
        "settings" | "alias" | "revoke" | "unrevoke" | "name"
    ) || (action == "discover" && state.public_origin.is_none())
    {
        let before = read_snapshot(&state).await?;
        let request_id = body["requestId"]
            .as_str()
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Invalid request ID"))?
            .to_owned();
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        let credentials = state.credentials.clone();
        let origin = state.public_origin.clone();
        let notifications = state.resource_notifications.clone();
        let action = body["action"].as_str().unwrap_or_default();
        if action == "name" {
            let target = body["serverId"].as_str().unwrap_or_default();
            let local_target = credentials
                .is_local_target(Arc::clone(&store), target)
                .await
                .map_err(map_credential_error)?;
            if local_target {
                let notification_sink = Arc::clone(&notifications);
                credentials
                    .rename_with_notification(
                        Arc::clone(&store),
                        target,
                        body["label"].as_str().unwrap_or_default(),
                        body["requestId"].as_str().unwrap_or_default(),
                        "local",
                        move |request_id| publish_resource_change(&notification_sink, request_id),
                    )
                    .await
                    .map_err(map_credential_error)?;
            } else {
                remote_set_server_name(
                    &store,
                    &credentials,
                    origin.as_deref(),
                    target,
                    body["label"].as_str().unwrap_or_default(),
                    body["requestId"].as_str().unwrap_or_default(),
                )
                .await?;
                publish_resource_change(
                    &notifications,
                    body["requestId"].as_str().unwrap_or_default(),
                );
            }
        }
        let identity = credentials
            .identity(Arc::clone(&store), origin.as_deref())
            .await
            .map_err(map_credential_error)?;
        let result = if matches!(action, "name" | "discover") {
            multi_server::snapshot(&store, &identity)
                .await
                .map_err(map_service_error)?
        } else {
            multi_server::manage(&store, &identity, &body, "local")
                .await
                .map_err(map_service_error)?
        };
        if action != "name" {
            notify_snapshot_change(&notifications, &request_id, &before, &result);
        }
        return Ok(
            Json(super::generated_json::<crate::generated::AccessSnapshot>(
                result,
            )?)
            .into_response(),
        );
    }
    Err(unavailable_for(
        "POST",
        "/api/multi-server",
        MANAGEMENT_DEPENDENCIES,
    ))
}

pub(super) fn tailscale_login(value: &Value) -> Option<String> {
    value
        .get("ownerLogin")
        .or_else(|| value.get("user"))
        .or_else(|| {
            value
                .get("UserProfile")
                .or_else(|| value.get("User"))
                .and_then(|user| user.get("LoginName").or_else(|| user.get("Name")))
        })?
        .as_str()
        .filter(|login| !login.trim().is_empty() && login.len() <= 254)
        .map(|login| login.trim().to_ascii_lowercase())
}

async fn accept_invite(
    store: Arc<studio_runtime_store::RuntimeStore>,
    credentials: &studio_runtime_services::federation_credentials::Credentials,
    local_origin: Option<&str>,
    whoami: Option<
        std::sync::Arc<
            dyn Fn() -> futures_util::future::BoxFuture<'static, Option<Value>> + Send + Sync,
        >,
    >,
    body: &Value,
) -> Result<Value, ApiError> {
    let invitation = body
        .get("invitation")
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "The invitation is invalid"))?;
    let server_id = invitation["serverId"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "The invitation is invalid"))?;
    let origin = invitation["origin"]
        .as_str()
        .and_then(|value| remote_transport::validate_federation_origin(value).ok())
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "The invitation is invalid"))?;
    let public_key = invitation["publicKey"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "The invitation is invalid"))?;
    if invitation["protocol"]
        != studio_runtime_services::multi_server_protocol_generated::ACCESS_PROTOCOL_VERSION
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "The invitation is invalid",
        ));
    }
    let request_id = body["requestId"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "The invitation is invalid"))?;
    let origin_local = local_origin
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "Configure Tailscale Serve HTTPS before pairing",
            )
        })?;
    let owner = match whoami {
        Some(probe) => probe().await.and_then(|value| tailscale_login(&value)),
        None => None,
    }
    .ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "The Tailscale owner identity is unavailable",
        )
    })?;
    let local = credentials
        .identity(Arc::clone(&store), Some(origin_local))
        .await
        .map_err(map_credential_error)?;
    let signing_identity = load_signing_identity(Arc::clone(&store)).await?;
    let local_id = local["serverId"].as_str().unwrap_or_default();
    let local_label = local["label"].as_str().unwrap_or("Studio server");
    let request_body = serde_json::json!({
        "protocol": studio_runtime_services::multi_server_protocol_generated::ACCESS_PROTOCOL_VERSION,
        "inviteId": invitation["inviteId"], "token": invitation["token"],
        "clientId": local_id, "label": local_label, "kind": "server",
        "publicKey": local["publicKey"], "origin": origin_local, "requestId": request_id,
    });
    let raw = remote_transport::canonical_json(&request_body)
        .map_err(|_| ApiError::internal())?
        .into_bytes();
    let timestamp = unix_time_seconds()?;
    let nonce = remote_transport::new_nonce().map_err(|_| ApiError::internal())?;
    let url = format!("{origin}/api/multi-server/v1/pair");
    let response = remote_transport::exchange_multi_server(
        Arc::clone(&store),
        &signing_identity,
        &MultiServerExchangeInput {
            server_id,
            method: "POST",
            url: &url,
            request_id,
            timestamp,
            nonce: &nonce,
            body: &raw,
            expected_public_key: public_key,
            timeout: Duration::from_secs(20),
        },
        timestamp,
    )
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "The pairing outcome is unknown. Keep the request ID",
        )
    })?;
    if response["paired"] != true
        || response["serverId"] != server_id
        || response["publicKey"] != public_key
        || response["origin"] != origin
        || response["clientId"] != local_id
        || response["tailscaleUser"] != owner
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "The paired server identity does not match the invitation",
        ));
    }
    let now = timestamp as f64;
    let prior_store = Arc::clone(&store);
    let server_id_for_read = server_id.to_owned();
    let prior = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/multi_server/accept-invite-prior",
        move || {
            prior_store
                .read_transaction(|tx| {
                    tx.query_row(
                        "SELECT record FROM runtime_access_clients WHERE id=?1",
                        [server_id_for_read],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(Into::into)
                })
                .map_err(ApiError::stored)
        },
    )
    .await
    .map_err(|_| ApiError::internal())??;
    if let Some(prior) = prior {
        let prior: Value = serde_json::from_str(&prior).map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Invalid stored object runtime_access_clients",
            )
        })?;
        if prior["status"] != "paired" || prior["publicKey"] != public_key {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "The server identity is already registered or revoked",
            ));
        }
    } else {
        let record = serde_json::json!({"id":server_id,"clientId":server_id,"serverId":server_id,"kind":"server","origin":origin,"publicKey":public_key,"label":invitation["label"],"tailscaleUser":response["tailscaleUser"],"status":"paired","created":now,"lastAccess":null,"revoked":null});
        let server_id_for_save = server_id.to_owned();
        store
            .transaction_with(move |tx| {
                tx.execute(
                    "INSERT INTO runtime_access_clients(id,record) VALUES(?1,?2)",
                    rusqlite::params![server_id_for_save, record.to_string()],
                )?;
                tx.execute(
                    "INSERT INTO runtime_access_audit(client,actor,action,created) VALUES(?1,'local','accept_invite',?2)",
                    rusqlite::params![server_id_for_save, now],
                )?;
                Ok(())
            })
            .await
            .map_err(ApiError::stored)?;
    }
    multi_server::snapshot(&store, &local)
        .await
        .map_err(map_service_error)
}

async fn ui_invite(
    store: Arc<studio_runtime_store::RuntimeStore>,
    body: &Value,
) -> Result<Value, ApiError> {
    let server_id = body["serverId"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "The server ID is invalid"))?;
    let request_id = body["requestId"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "The request ID is invalid"))?;
    let peer_store = Arc::clone(&store);
    let server_id_for_read = server_id.to_owned();
    let peer = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/multi_server/ui-invite-peer",
        move || {
            peer_store
                .read_transaction(|tx| {
                    tx.query_row("SELECT record FROM runtime_access_clients WHERE id=?1 AND json_extract(record,'$.kind')='server' AND json_extract(record,'$.status')='paired'", [server_id_for_read], |row| row.get::<_, String>(0))
                        .optional()?
                        .map(|raw| serde_json::from_str(&raw).map_err(|_| studio_runtime_store::StoreError::SchemaMismatch("invalid stored object runtime_access_clients".into())))
                        .transpose()
                        .map(|value| value.unwrap_or(Value::Null))
                })
                .map_err(ApiError::stored)
        },
    )
    .await
    .map_err(|_| ApiError::internal())??;
    if peer.is_null() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "The server is not paired or is revoked",
        ));
    }
    let origin = peer["origin"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "The remote server identity is unavailable",
            )
        })?;
    let public_key = peer["publicKey"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "The remote server identity is unavailable",
            )
        })?;
    let identity = load_signing_identity(Arc::clone(&store)).await?;
    let local_id = identity.id();
    let key = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("studio-ui-invite:{local_id}:{server_id}:{request_id}").as_bytes(),
    )
    .to_string();
    let request_body = serde_json::json!({"action":"create_invite","requestId":key});
    let raw = remote_transport::canonical_json(&request_body)
        .map_err(|_| ApiError::internal())?
        .into_bytes();
    let timestamp = unix_time_seconds()?;
    let nonce = remote_transport::new_nonce().map_err(|_| ApiError::internal())?;
    let url = format!("{origin}/api/multi-server");
    let result = remote_transport::exchange_multi_server(
        Arc::clone(&store),
        &identity,
        &MultiServerExchangeInput {
            server_id,
            method: "POST",
            url: &url,
            request_id: &key,
            timestamp,
            nonce: &nonce,
            body: &raw,
            expected_public_key: public_key,
            timeout: Duration::from_secs(20),
        },
        timestamp,
    )
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "The UI invitation outcome is unknown. Keep the request ID",
        )
    })?;
    let invitation = &result["invitation"];
    if invitation["serverId"] != server_id
        || invitation["origin"] != origin
        || invitation["publicKey"] != public_key
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "The UI invitation does not match the paired server",
        ));
    }
    Ok(result)
}

fn unix_time_seconds() -> Result<i64, ApiError> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| ApiError::internal())?
        .as_secs() as i64)
}

async fn load_signing_identity(
    store: Arc<studio_runtime_store::RuntimeStore>,
) -> Result<remote_transport::RemoteIdentity, ApiError> {
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/multi_server/signing-identity",
        move || remote_transport::load_multi_server_identity(store.root()),
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(|_| ApiError::internal())
}

async fn remote_set_server_name(
    store: &Arc<studio_runtime_store::RuntimeStore>,
    credentials: &studio_runtime_services::federation_credentials::Credentials,
    local_origin: Option<&str>,
    target: &str,
    label: &str,
    request_id: &str,
) -> Result<(), ApiError> {
    if target.is_empty()
        || request_id.is_empty()
        || label.is_empty()
        || label.chars().count() > 80
        || label.trim() != label
        || label.chars().any(|character| {
            character.is_control() || (character.is_whitespace() && character != ' ')
        })
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Invalid server name request",
        ));
    }
    let peer_store = Arc::clone(store);
    let target_for_read = target.to_owned();
    let raw_peer = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/multi_server/remote-name-peer",
        move || {
            peer_store
                .read_transaction(|tx| {
                    tx.query_row(
                        "SELECT record FROM runtime_access_clients WHERE id=?1 AND json_extract(record,'$.kind')='server' AND json_extract(record,'$.status')='paired'",
                        [target_for_read],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(Into::into)
                })
                .map_err(ApiError::stored)
        },
    )
    .await
    .map_err(|_| ApiError::internal())??;
    let Some(raw_peer) = raw_peer else {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "The server is not paired or is revoked",
        ));
    };
    let peer: Value = serde_json::from_str(&raw_peer).map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Invalid stored object runtime_access_clients",
        )
    })?;
    let origin = peer["origin"].as_str().unwrap_or_default();
    let public_key = peer["publicKey"].as_str().unwrap_or_default();
    if origin.is_empty() || public_key.is_empty() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Invalid stored object runtime_access_clients",
        ));
    }
    let reserve_store = Arc::clone(store);
    let target_for_reservation = target.to_owned();
    let label_for_reservation = label.to_owned();
    let request_for_reservation = request_id.to_owned();
    let reserved = reserve_store
        .transaction_with(move |tx| {
            let previous = tx
                .query_row(
                    "SELECT server,label FROM runtime_access_name_targets WHERE id=?1",
                    [request_for_reservation.as_str()],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            if previous.is_some_and(|(server, old_label)| server != target_for_reservation || old_label != label_for_reservation) {
                return Ok::<_, studio_runtime_store::StoreError>(false);
            }
            tx.execute(
                "INSERT OR IGNORE INTO runtime_access_name_targets(id,server,label) VALUES(?1,?2,?3)",
                rusqlite::params![request_for_reservation, target_for_reservation, label_for_reservation],
            )?;
            Ok(true)
        })
        .await
        .map_err(ApiError::stored)?;
    if !reserved {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "The server name request ID has different content",
        ));
    }
    credentials
        .identity(Arc::clone(store), local_origin)
        .await
        .map_err(map_credential_error)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| ApiError::internal())?
        .as_secs() as i64;
    let url = format!("{origin}/api/multi-server/v1/name");
    let raw = remote_transport::canonical_json(&serde_json::json!({
        "label": label,
        "requestId": request_id
    }))
    .map_err(|_| ApiError::internal())?
    .into_bytes();
    let nonce = remote_transport::new_nonce().map_err(|_| ApiError::internal())?;
    let signing_identity = load_signing_identity(Arc::clone(store)).await?;
    remote_transport::exchange_multi_server(
        Arc::clone(store),
        &signing_identity,
        &MultiServerExchangeInput {
            server_id: target,
            method: "POST",
            url: &url,
            request_id,
            timestamp: now,
            nonce: &nonce,
            body: &raw,
            expected_public_key: public_key,
            timeout: Duration::from_secs(20),
        },
        now,
    )
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "The remote name outcome is unknown",
        )
    })?;

    let status_url = format!("{origin}/api/multi-server/v1/status");
    let status_nonce = remote_transport::new_nonce().map_err(|_| ApiError::internal())?;
    let status_id = format!("status-{}", status_nonce);
    let status = remote_transport::exchange_multi_server(
        Arc::clone(store),
        &signing_identity,
        &MultiServerExchangeInput {
            server_id: target,
            method: "GET",
            url: &status_url,
            request_id: &status_id,
            timestamp: now,
            nonce: &status_nonce,
            body: &[],
            expected_public_key: public_key,
            timeout: Duration::from_secs(20),
        },
        now,
    )
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "The remote server identity is unavailable",
        )
    })?;
    let refreshed_label = status["label"].as_str().unwrap_or_default();
    if status["serverId"] != target
        || status["publicKey"] != public_key
        || refreshed_label.is_empty()
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "The remote server identity does not match",
        ));
    }
    let finish_store = Arc::clone(store);
    let target_for_finish = target.to_owned();
    let public_key_for_finish = public_key.to_owned();
    let refreshed_label = refreshed_label.to_owned();
    finish_store
        .transaction_with(move |tx| {
            let raw = tx
                .query_row(
                    "SELECT record FROM runtime_access_clients WHERE id=?1",
                    [target_for_finish.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let Some(raw) = raw else {
                return Ok::<_, studio_runtime_store::StoreError>(false);
            };
            let mut record: Value = serde_json::from_str(&raw).map_err(|_| {
                studio_runtime_store::StoreError::SchemaMismatch(
                    "invalid stored object runtime_access_clients".into(),
                )
            })?;
            if record["status"] != "paired" || record["publicKey"] != public_key_for_finish {
                return Ok(false);
            }
            record["label"] = Value::String(refreshed_label);
            tx.execute(
                "UPDATE runtime_access_clients SET record=?1 WHERE id=?2",
                rusqlite::params![record.to_string(), target_for_finish],
            )?;
            Ok(true)
        })
        .await
        .map_err(ApiError::stored)?;
    Ok(())
}

async fn auto_pair(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "Auto pairing requires a signed paired-server request",
    ))
}

/// Discovery identity is served only through the Serve boundary with owner
/// proof (`incoming_multi_server`); a direct local request is refused like
/// Python `studio_api/multi_server/router.py` `discovery_identity`.
async fn identity() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "error": "The identity endpoint requires owner proof through Serve",
            "code": "serve_required",
        })),
    )
        .into_response()
}

async fn rename(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "Renaming requires a signed paired-server request",
    ))
}

async fn pair(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "Pairing requires a signed paired-server request",
    ))
}

async fn status() -> Result<Json<Value>, ApiError> {
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "Status requires a signed paired-server request",
    ))
}

async fn orchestration(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "Orchestration requires a signed paired-server request",
    ))
}

fn unavailable_for(method: &str, path: &str, dependencies: &'static [Dependency]) -> ApiError {
    dependency_unavailable(method, path, dependencies).unwrap_or_else(|_| ApiError::internal())
}

fn map_credential_error(error: CredentialError) -> ApiError {
    match error {
        CredentialError::UnsupportedPlatform => {
            ApiError::new(StatusCode::NOT_IMPLEMENTED, error.to_string())
        }
        CredentialError::Conflict => ApiError::new(StatusCode::CONFLICT, error.to_string()),
        CredentialError::Identity(_) => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Invalid stored object multi-server/identity.json",
        ),
        CredentialError::InvalidJournal => {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, error.to_string())
        }
        CredentialError::RemoteRequired => {
            unavailable_for("POST", "/api/multi-server", MANAGEMENT_DEPENDENCIES)
        }
        _ => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "The local server credentials are unavailable",
        ),
    }
}

fn map_service_error(error: MultiServerError) -> ApiError {
    match error {
        MultiServerError::Store(error) => {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
        }
        MultiServerError::Sqlite(error) => {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
        }
        MultiServerError::Refused(message) => ApiError::new(StatusCode::BAD_REQUEST, message),
        MultiServerError::Conflict(message) => ApiError::new(StatusCode::CONFLICT, message),
        MultiServerError::NeedsSignedRequest => {
            unavailable_for("POST", "/api/multi-server", MANAGEMENT_DEPENDENCIES)
        }
    }
}

fn map_discovery_error(error: studio_runtime_services::multi_server_auth::AccessError) -> ApiError {
    let status = StatusCode::from_u16(error.status()).unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
    ApiError::new(status, "The Tailscale server discovery operation failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct FakeDiscovery(Arc<AtomicUsize>);

    #[test]
    fn semantic_snapshot_ignores_activity_times_but_keeps_peer_status() {
        let before = serde_json::json!({
            "clients": [{"id":"peer","lastAccess":10,"reachability":"reachable"}],
            "servers": [{"id":"peer","lastSeen":10,"reachability":"reachable"}],
        });
        let after = serde_json::json!({
            "clients": [{"id":"peer","lastAccess":20,"reachability":"reachable"}],
            "servers": [{"id":"peer","lastSeen":20,"reachability":"reachable"}],
        });
        assert_eq!(semantic_snapshot(&before), semantic_snapshot(&after));

        let changed = serde_json::json!({
            "clients": [{"id":"peer","lastAccess":20,"reachability":"unreachable"}],
            "servers": [{"id":"peer","lastSeen":20,"reachability":"unreachable"}],
        });
        assert_ne!(semantic_snapshot(&before), semantic_snapshot(&changed));
    }

    impl studio_runtime_services::multi_server_discovery::ServerDiscoveryPort for FakeDiscovery {
        fn discover<'a>(
            &'a self,
            _store: Arc<studio_runtime_store::RuntimeStore>,
            identity: &'a Value,
            public_origin: &'a str,
        ) -> futures_util::future::BoxFuture<
            'a,
            Result<Value, studio_runtime_services::multi_server_auth::AccessError>,
        > {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                assert_eq!(identity["origin"], public_origin);
                Ok(serde_json::json!({
                    "protocol": 1,
                    "identity": identity,
                    "clients": [],
                    "servers": [],
                    "invites": [],
                    "settings": {"autoPair": true, "aliases": {}}
                }))
            })
        }
    }

    #[tokio::test]
    async fn local_identity_request_requires_serve_owner_proof() {
        use tower::ServiceExt;
        let root = tempfile::tempdir().expect("isolated identity state");
        let store = Arc::new(
            studio_runtime_store::RuntimeStore::open(Some(root.path()))
                .await
                .expect("runtime store opens"),
        );
        let state = HttpState::new(
            "fixture-token".into(),
            "fixture".into(),
            None,
            root.path().display().to_string(),
            49171,
            None,
            Some(store),
            None,
        );
        let request = axum::http::Request::builder()
            .uri("/api/multi-server/v1/identity")
            .header("host", "127.0.0.1:49171")
            .header("origin", "http://127.0.0.1:49171")
            .header("x-canvas-token", "fixture-token")
            .extension(axum::extract::ConnectInfo(
                "127.0.0.1:12345"
                    .parse::<std::net::SocketAddr>()
                    .expect("fixture peer address parses"),
            ))
            .body(axum::body::Body::empty())
            .expect("identity request builds");
        let response = crate::router::router(state)
            .expect("valid router")
            .oneshot(request)
            .await
            .expect("identity response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("small refusal body");
        assert_eq!(
            serde_json::from_slice::<Value>(&body).expect("error envelope"),
            serde_json::json!({
                "error": "The identity endpoint requires owner proof through Serve",
                "code": "serve_required",
            })
        );
    }

    #[tokio::test]
    async fn unsupported_credentials_have_a_named_501_refusal() {
        let response = map_credential_error(CredentialError::UnsupportedPlatform).into_response();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("small refusal body");
        assert_eq!(
            serde_json::from_slice::<Value>(&body).expect("error envelope"),
            serde_json::json!({"error":"Not implemented: requires server-credentials/platform-support"})
        );
    }

    #[tokio::test]
    async fn discover_action_calls_the_injected_host_driver() {
        let root = tempfile::tempdir().expect("isolated discovery state");
        let store = Arc::new(
            studio_runtime_store::RuntimeStore::open(Some(root.path()))
                .await
                .expect("runtime store opens"),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let state = HttpState::new(
            "token".into(),
            "workspace".into(),
            None,
            root.path().display().to_string(),
            12345,
            Some("https://local.example".into()),
            Some(store),
            None,
        )
        .with_server_discovery(Arc::new(FakeDiscovery(Arc::clone(&calls))));
        let response = manage(
            State(state),
            ApiJson(serde_json::json!({"action":"discover","requestId":"discovery-fixture"})),
        )
        .await
        .expect("fake discovery succeeds");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let body = axum::body::to_bytes(response.into_body(), 16 * 1024)
            .await
            .expect("small snapshot body");
        assert_eq!(
            serde_json::from_slice::<Value>(&body).expect("snapshot JSON")["settings"]["autoPair"],
            true
        );
    }

    #[tokio::test]
    async fn create_invite_uses_injected_tailscale_identity_without_host_access() {
        let root = tempfile::tempdir().expect("isolated multi-server state");
        let store = Arc::new(
            studio_runtime_store::RuntimeStore::open(Some(root.path()))
                .await
                .expect("runtime store opens"),
        );
        let credentials =
            Arc::new(studio_runtime_services::federation_credentials::Credentials::default());
        let state = HttpState::new(
            "token".into(),
            "workspace".into(),
            None,
            root.path().display().to_string(),
            12345,
            Some("https://local.example.ts.net".into()),
            Some(Arc::clone(&store)),
            None,
        )
        .with_credentials(credentials)
        .with_tailscale_whoami(Arc::new(|| {
            Box::pin(async {
                Some(serde_json::json!({"status":"verified","user":"owner@example.test"}))
            })
        }));
        let response = manage(
            State(state),
            ApiJson(serde_json::json!({
                "action":"create_invite",
                "requestId":"fake-owner-invite",
                "label":"Fixture server"
            })),
        )
        .await
        .expect("fake Tailscale identity creates an invite");
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 16 * 1024)
            .await
            .expect("small invitation response");
        let body: Value = serde_json::from_slice(&body).expect("invitation JSON");
        assert_eq!(body["invitation"]["tailscaleUser"], "owner@example.test");
        assert_eq!(body["invitation"]["label"], "Fixture server");
        assert!(
            !body["invitation"]["token"]
                .as_str()
                .unwrap_or_default()
                .is_empty()
        );
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "multi_server",
        routes: Some(routes),
        dependencies: &[
            "remote-transport/tailscale-discovery",
            "server-credentials/platform-support",
        ],
    }
}
