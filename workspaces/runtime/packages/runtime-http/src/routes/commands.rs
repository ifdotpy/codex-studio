//! HTTP edge of the one command path: `POST /api/commands`.
//! Tracking: https://github.com/ifdotpy/layr-studio/issues/630
//!
//! The edge only reads the principal and calls `studio_runtime_services::commands`.
//! `withdraw` runs in one transaction. `send` and `stop` need I/O, so they run between
//! two short transactions: `begin` (permission, id, replay) and `finish` (reference and
//! watermark).

use std::sync::Arc;

use axum::http::StatusCode;
use axum::{Extension, Json, extract::State};
use studio_runtime_schema::command::{Command, CommandRequest, CommandResponse};
use studio_runtime_services::agent_lifecycle_runtime::LifecyclePorts;
use studio_runtime_services::agent_management::Action;
use studio_runtime_services::commands::{self, CommandError, Start};
use studio_runtime_services::principal::Principal;

use super::extractors::{ApiJson, Caller};
use super::manifest::{OperationStatus, RouteGroup, post};
use crate::error::ApiError;
use crate::router::HttpState;

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![post(
        "/api/commands",
        OperationStatus::Implemented,
        command,
    )])
}

pub(crate) fn command_error(error: CommandError) -> ApiError {
    match error {
        CommandError::Forbidden => ApiError::new(
            StatusCode::FORBIDDEN,
            "This caller may not run this command",
        ),
        CommandError::TooOld => ApiError::new(StatusCode::GONE, error.to_string()),
        CommandError::Invalid(message) => ApiError::new(StatusCode::BAD_REQUEST, message),
        CommandError::Conflict(message) => ApiError::new(StatusCode::CONFLICT, message),
        CommandError::NotFound => ApiError::new(StatusCode::NOT_FOUND, error.to_string()),
        CommandError::RequestResult(error) => error.into(),
        CommandError::Store(error) => error.into(),
    }
}

fn now_ms() -> Result<i64, ApiError> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| ApiError::internal())?;
    i64::try_from(elapsed.as_millis()).map_err(|_| ApiError::internal())
}

async fn command(
    State(state): State<HttpState>,
    ports: Option<Extension<Arc<dyn LifecyclePorts>>>,
    Caller(principal): Caller,
    ApiJson(request): ApiJson<CommandRequest>,
) -> Result<Json<CommandResponse>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let now_ms = now_ms()?;
    let chat = request.command.target().to_owned();
    let reference = match request.command.clone() {
        Command::Withdraw { target, body } => {
            let (principal, request) = (principal.clone(), request.clone());
            let reference = store
                .transaction_with_error(move |tx| {
                    match commands::begin(tx, &principal, &request, now_ms)? {
                        Start::Replay(reference) => Ok(reference),
                        Start::Fresh(begun) => {
                            // The text returns to the composer from the client's own row.
                            // TODO(#630): return it as a server draft.
                            let (reference, _text) =
                                commands::withdraw(tx, &target, &body.message)?;
                            commands::finish(tx, &begun, &reference)
                        }
                    }
                })
                .await
                .map_err(command_error)?;
            state.event_wake.notify_one();
            reference
        }
        Command::Stop { target, body } => {
            match begin(&state, &principal, &request, now_ms).await? {
                Start::Replay(reference) => reference,
                Start::Fresh(begun) => {
                    let (sender, sender_epoch) = match &principal {
                        Principal::Agent { id, epoch } => {
                            (Some(id.clone()), i64::try_from(*epoch).ok())
                        }
                        _ => (None, None),
                    };
                    // TODO(#630): today a stop pauses the chat and cancels its queue. The
                    // rule "stop does not pause and does not clear the queue" needs the Turn
                    // table of the domain model.
                    let _ = super::agent_management::dispatch::<crate::generated::StopResponse>(
                        state.clone(),
                        Action::Stop {
                            target: target.clone(),
                            descendants: body.descendants,
                            reason: None,
                            sender,
                            sender_epoch,
                        },
                        ports.map(|Extension(ports)| ports),
                    )
                    .await?;
                    store
                        .transaction_with_error(move |tx| {
                            let reference = commands::agent_reference(tx, &target)?;
                            commands::finish(tx, &begun, &reference)
                        })
                        .await
                        .map_err(command_error)?
                }
            }
        }
        Command::Send { target, body } => {
            match begin(&state, &principal, &request, now_ms).await? {
                Start::Replay(reference) => reference,
                Start::Fresh(begun) => {
                    // The command id is the message id: the message row is the receipt.
                    let mut wire = serde_json::json!({
                        "id": request.id,
                        "room": target,
                        "text": body.text,
                        "assets": body.assets,
                    });
                    if let Some(delivery) = body.delivery {
                        wire["delivery"] = serde_json::json!(delivery);
                    }
                    let message: crate::generated::ManagedMessageBody =
                        serde_json::from_value(wire).map_err(|_| ApiError::internal())?;
                    let _ = super::messages::send_message(state.clone(), message)
                        .await
                        .map_err(same_id_conflict)?;
                    let id = request.id.clone();
                    store
                        .transaction_with_error(move |tx| {
                            let reference = commands::message_reference(tx, &id)?;
                            commands::finish(tx, &begun, &reference)
                        })
                        .await
                        .map_err(command_error)?
                }
            }
        }
    };
    let response = store
        .read_async("runtime-http/routes/commands", move |connection| {
            commands::response(connection, &reference, &chat)
        })
        .await
        .map_err(command_error)?;
    Ok(Json(response))
}

async fn begin(
    state: &HttpState,
    principal: &Principal,
    request: &CommandRequest,
    now_ms: i64,
) -> Result<Start, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let (principal, request) = (principal.clone(), request.clone());
    store
        .transaction_with_error(move |tx| commands::begin(tx, &principal, &request, now_ms))
        .await
        .map_err(command_error)
}

/// The message route answers a reused id with other content as a bad request. The command
/// path answers it as a conflict, like every other command.
fn same_id_conflict(error: ApiError) -> ApiError {
    if error.status() == StatusCode::BAD_REQUEST && error.message() == commands::MESSAGE_ID_CONFLICT
    {
        return ApiError::new(StatusCode::CONFLICT, commands::MESSAGE_ID_CONFLICT);
    }
    error
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "commands",
        routes: Some(routes),
        dependencies: &[],
    }
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::extract::ConnectInfo;
    use axum::http::Request;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use studio_runtime_schema::persistence::{REQUEST_RESULT_TTL_MS, RequestIdentity};
    use studio_runtime_services::principal::{Grant, Principal};
    use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore};
    use tower::ServiceExt;

    use crate::router::HttpState;

    fn uuid_at(ms: i64) -> String {
        let bits = ((ms as u128) << 80) | (7 << 76) | (2 << 62) | 1;
        uuid::Uuid::from_u128(bits).to_string()
    }

    fn now_ms() -> i64 {
        i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_millis(),
        )
        .expect("ms")
    }

    async fn post(app: &axum::Router, principal: Option<Principal>, body: Value) -> (u16, Value) {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/api/commands")
            .header("host", "127.0.0.1:4620")
            .header("content-type", "application/json")
            .header("x-canvas-token", "token")
            .header(crate::API_SCHEMA_HASH_HEADER, crate::api_schema_hash())
            .extension(ConnectInfo(
                "127.0.0.1:12345"
                    .parse::<std::net::SocketAddr>()
                    .expect("peer"),
            ));
        if let Some(principal) = principal {
            builder = builder.extension(principal);
        }
        let response = app
            .clone()
            .oneshot(builder.body(Body::from(body.to_string())).expect("request"))
            .await
            .expect("response");
        let status = response.status().as_u16();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, serde_json::from_slice(&bytes).expect("JSON"))
    }

    fn withdraw(id: &str, number: u64, message: &str) -> Value {
        json!({"id":id,"client":"web-1","number":number,"kind":"withdraw","target":"workspace","body":{"message":message}})
    }

    async fn app_with_queue() -> (axum::Router, Arc<RuntimeStore>, tempfile::TempDir) {
        let directory = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("store"),
        );
        store.transaction(|tx| {
            for id in ["workspace", "other"] {
                AgentRepository::put(tx, &AgentRecord::from_value(json!({"accountKey":"default","id":id,"rootId":id,"epoch":0,"status":"idle","provider":studio_runtime_schema::ProviderKind::Codex}))?)?;
            }
            tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES('a','workspace','user','A','pending',1,0),('b','workspace','user','B','pending',2,0)", [])?;
            Ok(())
        }).await.expect("seed queue");
        let state = HttpState::new(
            "token".into(),
            "workspace".into(),
            directory.path().to_string_lossy().into_owned(),
            4620,
            None,
            Some(store.clone()),
            None,
        );
        (crate::router(state).expect("router"), store, directory)
    }

    async fn status_of(store: &RuntimeStore, id: &'static str) -> String {
        store
            .transaction(move |tx| {
                Ok(tx.query_row(
                    "SELECT status FROM runtime_events WHERE id=?1",
                    [id],
                    |row| row.get::<_, String>(0),
                )?)
            })
            .await
            .expect("status")
    }

    #[tokio::test]
    async fn replay_returns_the_same_reference_and_runs_once() {
        let (app, store, _directory) = app_with_queue().await;
        let id = RequestIdentity::issue_root();
        let (status, first) = post(&app, None, withdraw(&id, 1, "a")).await;
        assert_eq!(status, 200, "{first}");
        assert_eq!(first["status"], "applied");
        assert_eq!(first["ref_kind"], "message");
        assert_eq!(first["ref_id"], "a");
        assert_eq!(first["rev"], 2);
        assert_eq!(status_of(&store, "a").await, "cancelled");
        let (status, again) = post(&app, None, withdraw(&id, 1, "a")).await;
        assert_eq!(status, 200, "{again}");
        for field in ["status", "ref_kind", "ref_id", "rev"] {
            assert_eq!(first[field], again[field], "{field}");
        }
        assert_eq!(status_of(&store, "b").await, "pending");
    }

    #[tokio::test]
    async fn same_id_with_another_body_is_a_conflict() {
        let (app, store, _directory) = app_with_queue().await;
        let id = RequestIdentity::issue_root();
        assert_eq!(post(&app, None, withdraw(&id, 1, "a")).await.0, 200);
        let (status, _) = post(&app, None, withdraw(&id, 1, "b")).await;
        assert_eq!(status, 409);
        assert_eq!(status_of(&store, "b").await, "pending");
    }

    #[tokio::test]
    async fn a_command_older_than_the_window_is_rejected_as_too_old() {
        let (app, store, _directory) = app_with_queue().await;
        let fresh = RequestIdentity::issue_root();
        assert_eq!(post(&app, None, withdraw(&fresh, 5, "a")).await.0, 200);
        let old = uuid_at(now_ms() - REQUEST_RESULT_TTL_MS - 1_000);
        let (status, body) = post(&app, None, withdraw(&old, 3, "b")).await;
        assert_eq!(status, 410, "{body}");
        assert!(
            body["error"]
                .as_str()
                .expect("error")
                .contains("already applied")
        );
        assert_eq!(status_of(&store, "b").await, "pending");
        let (status, body) = post(&app, None, withdraw(&old, 6, "b")).await;
        assert_eq!(status, 410, "{body}");
        assert!(body["error"].as_str().expect("error").contains("expired"));
        assert_eq!(status_of(&store, "b").await, "pending");
    }

    #[tokio::test]
    async fn an_observe_device_cannot_send() {
        let (app, store, _directory) = app_with_queue().await;
        let observe = Principal::Device {
            id: "phone".into(),
            grant: Grant::Observe,
        };
        let (status, _) = post(
            &app,
            Some(observe),
            json!({"id":RequestIdentity::issue_root(),"client":"p","number":1,"kind":"send","target":"workspace","body":{"text":"hi"}}),
        )
        .await;
        assert_eq!(status, 403);
        let count: i64 = store
            .transaction(|tx| {
                Ok(tx.query_row("SELECT count(*) FROM runtime_events", [], |row| row.get(0))?)
            })
            .await
            .expect("count");
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn an_agent_outside_its_descendants_is_refused() {
        let (app, store, _directory) = app_with_queue().await;
        let agent = Principal::Agent {
            id: "other".into(),
            epoch: 0,
        };
        let (status, _) = post(
            &app,
            Some(agent),
            withdraw(&RequestIdentity::issue_root(), 1, "a"),
        )
        .await;
        assert_eq!(status, 403);
        assert_eq!(status_of(&store, "a").await, "pending");
    }
}
