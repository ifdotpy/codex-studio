//! Multi-server local audit reads and explicit transport-dependent operations.

use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use rusqlite::OptionalExtension;
use serde_json::Value;
use studio_runtime_services::federation_credentials::CredentialError;
use studio_runtime_services::multi_server::{self, MultiServerError};
use studio_runtime_services::remote_transport::{self, MultiServerExchangeInput};

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
            OperationStatus::partial(MANAGEMENT_DEPENDENCIES),
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
        get(
            "/api/multi-server/v1/pair",
            OperationStatus::Implemented,
            no_pair_get,
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
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/multi_server",
        move || {
            let identity = credentials
                .identity(&store, origin.as_deref())
                .map_err(map_credential_error)?;
            multi_server::snapshot(&store, &identity).map_err(map_service_error)
        },
    )
    .await
    .map_err(|_| ApiError::internal())?;
    let result = result?;
    Ok(Json(super::generated_json::<
        crate::generated::AccessSnapshot,
    >(result)?))
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
) -> Result<Json<crate::generated::AccessSnapshot>, ApiError> {
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
    if action == "discover"
        && let Some(origin) = state.public_origin.clone()
    {
        let store = state.store.ok_or_else(ApiError::internal)?;
        let credentials = state.credentials;
        let discovery = state
            .server_discovery
            .ok_or_else(|| unavailable_for("POST", "/api/multi-server", MANAGEMENT_DEPENDENCIES))?;
        let result = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/routes/multi_server",
            move || {
                let identity = credentials
                    .identity(&store, Some(&origin))
                    .map_err(map_credential_error)?;
                discovery
                    .discover(&store, &identity, &origin)
                    .map_err(map_discovery_error)
            },
        )
        .await
        .map_err(|_| ApiError::internal())??;
        return Ok(Json(super::generated_json::<
            crate::generated::AccessSnapshot,
        >(result)?));
    }
    if matches!(
        action,
        "settings" | "alias" | "revoke" | "unrevoke" | "name"
    ) || (action == "discover" && state.public_origin.is_none())
    {
        let store = state.store.ok_or_else(ApiError::internal)?;
        let credentials = state.credentials;
        let origin = state.public_origin;
        let result = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/routes/multi_server",
            move || {
                let action = body["action"].as_str().unwrap_or_default();
                if action == "alias"
                    && !credentials
                        .is_local_target(&store, body["serverId"].as_str().unwrap_or_default())
                        .map_err(map_credential_error)?
                {
                    return Err(unavailable_for(
                        "POST",
                        "/api/multi-server",
                        MANAGEMENT_DEPENDENCIES,
                    ));
                }
                if action == "name" {
                    let target = body["serverId"].as_str().unwrap_or_default();
                    if credentials
                        .is_local_target(&store, target)
                        .map_err(map_credential_error)?
                    {
                        credentials
                            .rename(
                                &store,
                                target,
                                body["label"].as_str().unwrap_or_default(),
                                body["requestId"].as_str().unwrap_or_default(),
                                "local",
                            )
                            .map_err(map_credential_error)?;
                    } else {
                        remote_set_server_name(
                            &store,
                            &credentials,
                            origin.as_deref(),
                            target,
                            body["label"].as_str().unwrap_or_default(),
                            body["requestId"].as_str().unwrap_or_default(),
                        )?;
                    }
                }
                let identity = credentials
                    .identity(&store, origin.as_deref())
                    .map_err(map_credential_error)?;
                if matches!(action, "name" | "discover") {
                    multi_server::snapshot(&store, &identity).map_err(map_service_error)
                } else {
                    multi_server::manage(&store, &identity, &body, "local")
                        .map_err(map_service_error)
                }
            },
        )
        .await
        .map_err(|_| ApiError::internal())??;
        return Ok(Json(super::generated_json::<
            crate::generated::AccessSnapshot,
        >(result)?));
    }
    Err(unavailable_for(
        "POST",
        "/api/multi-server",
        MANAGEMENT_DEPENDENCIES,
    ))
}

fn remote_set_server_name(
    store: &studio_runtime_store::RuntimeStore,
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
    let raw_peer = store
        .read_transaction(|tx| {
            tx.query_row(
                "SELECT record FROM runtime_access_clients WHERE id=?1 AND json_extract(record,'$.kind')='server' AND json_extract(record,'$.status')='paired'",
                [target],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
        })
        .map_err(ApiError::stored)?;
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
    let reserved = store
        .transaction_with(|tx| {
            let previous = tx
                .query_row(
                    "SELECT server,label FROM runtime_access_name_targets WHERE id=?1",
                    [request_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            if previous.is_some_and(|(server, old_label)| server != target || old_label != label) {
                return Ok::<_, studio_runtime_store::StoreError>(false);
            }
            tx.execute(
                "INSERT OR IGNORE INTO runtime_access_name_targets(id,server,label) VALUES(?1,?2,?3)",
                rusqlite::params![request_id, target, label],
            )?;
            Ok(true)
        })
        .map_err(ApiError::stored)?;
    if !reserved {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "The server name request ID has different content",
        ));
    }
    credentials
        .identity(store, local_origin)
        .map_err(map_credential_error)?;
    let identity = remote_transport::load_multi_server_identity(store.root()).map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "The local server identity is unavailable",
        )
    })?;
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
    remote_transport::exchange_multi_server(
        store,
        &identity,
        &MultiServerExchangeInput {
            server_id: target,
            method: "POST",
            url: &url,
            request_id,
            timestamp: now,
            nonce: &nonce,
            body: &raw,
            expected_public_key: public_key,
            timeout: std::time::Duration::from_secs(10),
        },
        now,
    )
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
        store,
        &identity,
        &MultiServerExchangeInput {
            server_id: target,
            method: "GET",
            url: &status_url,
            request_id: &status_id,
            timestamp: now,
            nonce: &status_nonce,
            body: &[],
            expected_public_key: public_key,
            timeout: std::time::Duration::from_secs(10),
        },
        now,
    )
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
    store
        .transaction_with(|tx| {
            let raw = tx
                .query_row(
                    "SELECT record FROM runtime_access_clients WHERE id=?1",
                    [target],
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
            if record["status"] != "paired" || record["publicKey"] != public_key {
                return Ok(false);
            }
            record["label"] = Value::String(refreshed_label.to_owned());
            tx.execute(
                "UPDATE runtime_access_clients SET record=?1 WHERE id=?2",
                rusqlite::params![record.to_string(), target],
            )?;
            Ok(true)
        })
        .map_err(ApiError::stored)?;
    Ok(())
}

async fn auto_pair(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "Auto pairing requires a signed paired-server request",
    ))
}

async fn identity(
    State(state): State<HttpState>,
) -> Result<Json<crate::generated::ServerIdentityResponse>, ApiError> {
    let credentials = state.credentials;
    let origin = state.public_origin;
    let store = state.store.ok_or_else(ApiError::internal)?;
    let value = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/multi_server",
        move || {
            credentials
                .identity(&store, origin.as_deref())
                .map_err(map_credential_error)
        },
    )
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(super::generated_json::<
        crate::generated::ServerIdentityResponse,
    >(value)?))
}

async fn rename(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "Renaming requires a signed paired-server request",
    ))
}

async fn no_pair_get() -> Result<Json<Value>, ApiError> {
    Err(ApiError::new(StatusCode::NOT_FOUND, "Not found"))
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
    impl studio_runtime_services::multi_server_discovery::ServerDiscoveryPort for FakeDiscovery {
        fn discover(
            &self,
            _store: &studio_runtime_store::RuntimeStore,
            identity: &Value,
            public_origin: &str,
        ) -> Result<Value, studio_runtime_services::multi_server_auth::AccessError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            assert_eq!(identity["origin"], public_origin);
            Ok(serde_json::json!({
                "protocol": 1,
                "identity": identity,
                "clients": [],
                "servers": [],
                "invites": [],
                "settings": {"autoPair": true, "aliases": {}}
            }))
        }
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
            ApiJson(serde_json::json!({"action":"discover"})),
        )
        .await
        .expect("fake discovery succeeds");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(response.0.settings.auto_pair, true);
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "multi_server",
        routes: Some(routes),
        dependencies: &[
            "multi-server-runtime/orchestration",
            "multi-server-runtime/pairing",
            "remote-transport/owner-lookup",
            "remote-transport/signed-server-requests",
            "remote-transport/tailscale-discovery",
            "server-credentials/platform-support",
        ],
    }
}
