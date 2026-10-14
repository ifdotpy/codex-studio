//! Mac folder sync configuration and conflict resolution routes.

use axum::{Json, extract::State, http::StatusCode};
use serde_json::{Value, json};
use studio_runtime_services::{linux_vm_guest::UnixGuestRpc, mac_sync};
use studio_runtime_store::{AgentId, AgentRepository, Transaction};

use super::{
    extractors::ApiJson,
    manifest::{OperationStatus, RouteGroup, post},
};
use crate::{error::ApiError, generated, router::HttpState};

/// Narrow adapter for Python's `runtime.send(..., manual=false, delivery="after_turn")`.
/// `send_system_followup` persists the actorless non-manual follow-up event.
trait HandoffPort {
    fn send_after_turn(
        &self,
        transaction: &Transaction<'_>,
        recipient: &str,
        request_id: &str,
        text: &str,
        now: f64,
        admission: &dyn studio_runtime_services::message_admission::MessageAdmission,
    ) -> Result<
        studio_runtime_services::coordination::PostMessageResult,
        studio_runtime_services::coordination::CoordinationError,
    >;
}

struct CoordinationHandoffPort;

impl HandoffPort for CoordinationHandoffPort {
    fn send_after_turn(
        &self,
        transaction: &Transaction<'_>,
        recipient: &str,
        request_id: &str,
        text: &str,
        now: f64,
        admission: &dyn studio_runtime_services::message_admission::MessageAdmission,
    ) -> Result<
        studio_runtime_services::coordination::PostMessageResult,
        studio_runtime_services::coordination::CoordinationError,
    > {
        studio_runtime_services::coordination::send_system_followup(
            transaction,
            studio_runtime_services::coordination::SystemFollowup {
                recipient_id: recipient,
                id: request_id,
                text,
                delivery: "after_turn",
                now,
            },
            admission,
        )
    }
}

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post(
            "/api/mac-sync/config",
            OperationStatus::Implemented,
            configure,
        ),
        post(
            "/api/mac-sync/resolve",
            OperationStatus::Implemented,
            resolve,
        ),
    ])
}

async fn configure(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::MacSyncConfigRequest>,
) -> Result<Json<Value>, ApiError> {
    let root = std::path::PathBuf::from(state.state_dir);
    let status = tokio::task::spawn_blocking(move || {
        mac_sync::configure(
            &root,
            &body.project_id,
            body.enabled.flatten(),
            body.allow_deletes.unwrap_or(false),
        )
        .map_err(mac_error)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    state.sync_changes.notify_one();
    Ok(Json(status))
}

async fn resolve(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::MacSyncResolveRequest>,
) -> Result<Json<Value>, ApiError> {
    let notifications = state.sync_changes.clone();
    let root = std::path::PathBuf::from(state.state_dir.clone());
    let guest = UnixGuestRpc::for_runtime_state(std::path::Path::new(&state.state_dir));
    let project_id = body.project_id;
    let resolve_project_id = project_id.clone();
    let path = body.path;
    let choice = body.choice.as_str().to_owned();
    let result = tokio::task::spawn_blocking(move || {
        mac_sync::resolve(&root, &resolve_project_id, &path, &choice, &guest).map_err(mac_error)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    if result.response["choice"] == "agent" {
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        let lead = lead_for_project(store.clone(), project_id.clone()).await?;
        let mut response = result.response;
        response["leadId"] = lead.as_ref().map_or(Value::Null, |lead| json!(lead));
        if let (Some(lead), Some(text), Some(request_id)) =
            (lead, result.handoff_text, result.handoff_id)
        {
            send_handoff(state, store, &lead, &request_id, &text).await?;
        }
        notifications.notify_one();
        return Ok(Json(response));
    }
    notifications.notify_one();
    Ok(Json(result.response))
}

async fn lead_for_project(
    store: std::sync::Arc<studio_runtime_store::RuntimeStore>,
    project_id: String,
) -> Result<Option<String>, ApiError> {
    tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let mut statement = connection
            .prepare("SELECT id FROM runtime_agents")
            .map_err(|_| ApiError::internal())?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|_| ApiError::internal())?;
        let mut leads = Vec::new();
        for id in ids {
            let id = id.map_err(|_| ApiError::internal())?;
            let Some(agent) = AgentRepository::get(&connection, &AgentId::from(id.as_str()))
                .map_err(|_| ApiError::internal())?
            else {
                continue;
            };
            if agent.extra.get("isLead").and_then(Value::as_bool) == Some(true)
                && agent.extra.get("layrProjectId").and_then(Value::as_str) == Some(&project_id)
                && agent.extra.get("layrLine").and_then(Value::as_str) == Some("main")
                && agent.extra.get("deletedAt").is_none_or(Value::is_null)
            {
                leads.push((
                    agent.status.as_str() != "failed",
                    agent
                        .extra
                        .get("created")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0),
                    agent.id.0,
                ));
            }
        }
        leads.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.total_cmp(&b.1)));
        Ok(leads.pop().map(|lead| lead.2))
    })
    .await
    .map_err(|_| ApiError::internal())?
}

async fn send_handoff(
    state: HttpState,
    store: std::sync::Arc<studio_runtime_store::RuntimeStore>,
    lead: &str,
    request_id: &str,
    text: &str,
) -> Result<(), ApiError> {
    let lead = lead.to_owned();
    let request_id = request_id.to_owned();
    let text = text.to_owned();
    let wake = state.event_wake;
    let custom_admission = state.message_admission;
    tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        AgentRepository::get(&connection, &AgentId::from(lead.as_str()))
            .map_err(|_| ApiError::internal())?
            .ok_or_else(ApiError::internal)?;
        let ingress_admission = if custom_admission.is_none() {
            Some(
                studio_runtime_services::message_admission::MessageIngressAdmission::load(
                    &store,
                    &connection,
                    &lead,
                )
                .map_err(|_| ApiError::internal())?,
            )
        } else {
            None
        };
        let default_admission =
            studio_runtime_services::message_admission::PreparedAdmission::load(&connection, &lead)
                .map_err(|_| ApiError::internal())?;
        let admission: &dyn studio_runtime_services::message_admission::MessageAdmission =
            custom_admission
                .as_deref()
                .or_else(|| {
                    ingress_admission.as_ref().map(|value| {
                        value as &dyn studio_runtime_services::message_admission::MessageAdmission
                    })
                })
                .unwrap_or(&default_admission);
        let posted = store
            .transaction_with_error(|transaction| {
                CoordinationHandoffPort.send_after_turn(
                    transaction,
                    &lead,
                    &request_id,
                    &text,
                    now_seconds(),
                    admission,
                )
            })
            .map_err(|_| ApiError::internal())?;
        if posted.event_inserted {
            wake.notify_one();
        }
        Ok::<(), ApiError>(())
    })
    .await
    .map_err(|_| ApiError::internal())?
}

fn mac_error(error: mac_sync::MacSyncError) -> ApiError {
    match error {
        mac_sync::MacSyncError::Busy => ApiError::new(StatusCode::CONFLICT, error.to_string()),
        mac_sync::MacSyncError::HandoffUnknown => {
            ApiError::new(StatusCode::CONFLICT, error.to_string())
        }
        mac_sync::MacSyncError::Guest(ref guest) if guest.uncertain() => {
            ApiError::new(StatusCode::CONFLICT, error.to_string())
        }
        mac_sync::MacSyncError::Guest(_) => {
            ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
        }
        mac_sync::MacSyncError::Io(_) | mac_sync::MacSyncError::Json(_) => {
            ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
        }
        _ => ApiError::new(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |value| value.as_secs_f64())
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "mac_sync", routes: Some(routes), dependencies: &[],
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use axum::body::Body;
    use serde_json::json;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use tower::ServiceExt;

    #[tokio::test]
    async fn resolve_http_uses_a_fake_guest_endpoint_and_writes_vm_choice() {
        let state_dir = tempfile::tempdir().unwrap();
        let project = state_dir.path().join("layr-projects/p1");
        let folder = state_dir.path().join("mac-folder");
        let socket_dir = state_dir.path().join("linux-vm");
        std::fs::create_dir_all(project.join("sync")).unwrap();
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(&socket_dir).unwrap();
        std::fs::write(
            project.join("project.json"),
            json!({"projectId":"p1","source":folder}).to_string(),
        )
        .unwrap();
        std::fs::write(
            project.join("sync/held.json"),
            json!({"file.txt":{"h":"old","why":"conflict"}}).to_string(),
        )
        .unwrap();
        std::fs::write(project.join("sync/status.json"), json!({"state":"conflict","conflicts":["file.txt"],"choices":[],"withAgent":[],"deletes":0}).to_string()).unwrap();
        std::fs::write(folder.join("file.txt"), b"mac").unwrap();
        let listener = UnixListener::bind(socket_dir.join("guest.sock")).unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let result = match request["method"].as_str().unwrap() {
                    "sync.mac.head" => json!({"mainStateId":"main-1"}),
                    "sync.mac.read" => {
                        json!({"kind":"file","mode":420,"data":"Z3Vlc3Q=","eof":true})
                    }
                    other => panic!("unexpected guest method: {other}"),
                };
                let mut stream = reader.into_inner();
                writeln!(stream, "{}", json!({"id":request["id"],"result":result})).unwrap();
            }
        });
        let state = crate::router::HttpState::new(
            "fixture-token".into(),
            "fixture".into(),
            None,
            state_dir.path().to_string_lossy().into_owned(),
            49171,
            None,
            None,
            None,
        );
        let app = crate::router::router(state).unwrap();
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/api/mac-sync/resolve")
            .header("host", "127.0.0.1:49171")
            .header("x-canvas-token", "fixture-token")
            .header(
                crate::schema::API_SCHEMA_HASH_HEADER,
                crate::schema::api_schema_hash(),
            )
            .header("content-type", "application/json")
            .extension(axum::extract::ConnectInfo(
                "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
            ))
            .body(Body::from(
                json!({"projectId":"p1","path":"file.txt","choice":"vm"}).to_string(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value = serde_json::from_slice::<Value>(&bytes).unwrap();
        assert_eq!(value["mainStateId"], "main-1");
        assert_eq!(value["status"]["state"], "synced");
        assert_eq!(std::fs::read(folder.join("file.txt")).unwrap(), b"guest");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn config_http_contract_and_invalid_resolve_refusal_are_quiescent() {
        let state_dir = tempfile::tempdir().unwrap();
        let project = state_dir.path().join("layr-projects/p1");
        let folder = state_dir.path().join("mac-folder");
        std::fs::create_dir_all(project.join("sync")).unwrap();
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            project.join("project.json"),
            json!({"projectId":"p1","source":folder}).to_string(),
        )
        .unwrap();
        std::fs::write(
            project.join("sync/status.json"),
            json!({"state":"syncing","conflicts":[],"choices":[],"withAgent":[],"deletes":0})
                .to_string(),
        )
        .unwrap();
        let state = crate::router::HttpState::new(
            "fixture-token".into(),
            "fixture".into(),
            None,
            state_dir.path().to_string_lossy().into_owned(),
            49171,
            None,
            None,
            None,
        );
        let app = crate::router::router(state).unwrap();
        let request = |uri: &'static str, body: Value| {
            axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header("host", "127.0.0.1:49171")
                .header("x-canvas-token", "fixture-token")
                .header(
                    crate::schema::API_SCHEMA_HASH_HEADER,
                    crate::schema::api_schema_hash(),
                )
                .header("content-type", "application/json")
                .extension(axum::extract::ConnectInfo(
                    "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
                ))
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        let before = std::fs::read(project.join("sync/status.json")).unwrap();
        let refused = app
            .clone()
            .oneshot(request(
                "/api/mac-sync/resolve",
                json!({"projectId":"p1","path":"file.txt","choice":"both"}),
            ))
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            std::fs::read(project.join("sync/status.json")).unwrap(),
            before
        );
        assert!(!project.join("sync/round.lock").exists());

        let response = app
            .oneshot(request(
                "/api/mac-sync/config",
                json!({"projectId":"p1","enabled":false,"allowDeletes":false}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["state"], "paused");
        assert_eq!(value["enabled"], false);
        assert!(project.join("sync/config.json").is_file());
    }
}
