//! HTTP adapters for persisted coordination chats and messages.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use rusqlite::OptionalExtension;
use serde_json::Value;
use std::sync::Arc;
use studio_runtime_services::coordination::{self, CoordinationError};

use super::extractors::{ApiJson, QueryParams};
use super::manifest::{OperationStatus, RouteGroup, get, post};
use crate::error::ApiError;
use crate::router::HttpState;

#[derive(serde::Deserialize)]
struct ChatCreate {
    id: String,
    name: String,
    members: Vec<String>,
}

#[derive(serde::Deserialize)]
struct Connection {
    source: String,
    target: String,
    #[serde(default = "default_connected")]
    connected: bool,
}

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get("/api/agent-chat", OperationStatus::Implemented, agent_chat),
        get("/api/messages", OperationStatus::Implemented, messages),
        post("/api/messages", OperationStatus::Implemented, post_message),
        get(
            "/api/messages/receipts",
            OperationStatus::Implemented,
            receipts,
        ),
        post("/api/chats", OperationStatus::Implemented, create_chat),
        post(
            "/api/connections",
            OperationStatus::Implemented,
            connect_chat,
        ),
        post(
            "/api/room/delete",
            OperationStatus::Implemented,
            delete_room,
        ),
    ])
}

async fn messages(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Vec<crate::generated::MessageRecord>>, ApiError> {
    let room = query
        .typed_value("room")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let store = state.store.ok_or_else(ApiError::internal)?;
    let result = store
        .read_async("runtime-http/routes/messages", move |connection| {
            let value = coordination::history(connection, &room).map_err(coordination_error)?;
            super::generated_json::<Vec<crate::generated::MessageRecord>>(value)
        })
        .await?;
    Ok(Json(result))
}

async fn create_chat(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<ChatCreate>,
) -> Result<Json<crate::generated::ChatCreated>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let value = store
        .transaction_with_error(move |transaction| {
            coordination::create_chat(transaction, &body.id, &body.name, &body.members)
        })
        .await
        .map_err(coordination_error)?;
    let result = super::generated_json::<crate::generated::ChatCreated>(value)?;
    Ok(Json(result))
}

async fn connect_chat(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Connection>,
) -> Result<Json<crate::generated::ChatReceipt>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let value = store
        .transaction_with_error(move |transaction| {
            coordination::connect_chat(transaction, &body.source, &body.target, body.connected)
        })
        .await
        .map_err(coordination_error)?;
    let result = super::generated_json::<crate::generated::ChatReceipt>(value)?;
    Ok(Json(result))
}

pub(super) async fn post_message(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::ManagedMessageBody>,
) -> Result<Json<crate::generated::StudioApiWorkModelsMessageReceipt>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let wake = state.event_wake;
    let message_admission = state.message_admission.clone();
    let orchestration_driver = state.orchestration_driver.clone();
    let assets = body
        .assets
        .unwrap_or_default()
        .into_iter()
        .map(|asset| serde_json::to_value(asset).map_err(|_| ApiError::internal()))
        .collect::<Result<Vec<_>, _>>()?;
    let delivery = body.delivery.unwrap_or_default();
    let admission_room = body.room.clone();
    let admission_override = message_admission.clone();
    let (federated_room, remote_worker, prepared) = store
        .read_async("runtime-http/routes/messages", move |connection| {
                let federated_room = connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM runtime_federation_rooms WHERE id=?1)",
                        [&admission_room],
                        |row| row.get::<_, bool>(0),
                    )
                    .map_err(|_| ApiError::internal())?;
                if federated_room {
                    return Ok::<_, ApiError>((true, false, None));
                }
                let remote_worker: bool = connection
                    .query_row(
                        "SELECT json_type(record,'$.remoteWorker') IS NOT NULL FROM runtime_agents WHERE id=?1",
                        [&admission_room],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|_| ApiError::internal())?
                    .unwrap_or(false);
                let prepared: studio_runtime_services::message_admission::AdmissionSnapshot =
                    if admission_override.is_some() {
                        studio_runtime_services::message_admission::AdmissionSnapshot::Workspace(
                            studio_runtime_services::message_admission::PreparedAdmission::read(
                                connection,
                                &admission_room,
                            )
                            .map_err(coordination_error)?,
                        )
                    } else {
                        studio_runtime_services::message_admission::AdmissionSnapshot::Ingress(
                            studio_runtime_services::message_admission::MessageIngressAdmission::read(
                                connection,
                                &admission_room,
                            )
                            .map_err(coordination_error)?,
                        )
                    };
                Ok((false, remote_worker, Some(prepared)))
        })
        .await?;
    let prepared = match prepared {
        Some(snapshot) => Some(snapshot.prepare().await.map_err(coordination_error)?),
        None => None,
    };
    #[cfg(test)]
    {
        let room = body.room.clone();
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/routes/messages/test-gate",
            move || ingress_tests::support::after_preparation(&room),
        )
        .await
        .map_err(|_| ApiError::internal())?;
    }
    if federated_room {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| ApiError::internal())?
            .as_secs_f64();
        let mut result = studio_runtime_services::federation_delivery::user_message(
            Arc::clone(&store),
            &body.room,
            &body.text,
            &body.id,
            delivery,
            now,
        )
        .await
        .map_err(federation_error)?;
        result["id"] = Value::String(body.id.clone());
        let result =
            super::generated_json::<crate::generated::StudioApiWorkModelsMessageReceipt>(result)?;
        wake.notify_one();
        return Ok(Json(result));
    }
    let prepared = prepared.ok_or_else(ApiError::internal)?;
    let admission = message_admission.unwrap_or_else(|| Arc::from(prepared));
    let posted = store
        .transaction_with_error(move |transaction| {
            coordination::post_message_with_admission(
                transaction,
                coordination::PostManagedMessage {
                    room: &body.room,
                    id: &body.id,
                    text: &body.text,
                    delivery,
                    assets: &assets,
                },
                admission.as_ref(),
            )
        })
        .await
        .map_err(coordination_error)?;
    let result = super::generated_json::<crate::generated::StudioApiWorkModelsMessageReceipt>(
        posted.receipt.clone(),
    )?;
    posted.notify_committed(|| wake.notify_one());
    if remote_worker && let Some(driver) = &orchestration_driver {
        let now = studio_runtime_services::event_write::unix_time()
            .map_err(|_| ApiError::internal())? as i64;
        driver
            .pump_due(now)
            .await
            .map_err(|_| ApiError::internal())?;
    }
    Ok(Json(result))
}

async fn agent_chat(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<crate::generated::ChatRead>, ApiError> {
    let room = query
        .typed_value("room")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let before = query.typed_value("before").and_then(Value::as_i64);
    let after = query.typed_value("after").and_then(Value::as_i64);
    let limit = query
        .typed_value("limit")
        .and_then(Value::as_i64)
        .unwrap_or(100);
    if before.is_some() && after.is_some() {
        return Err(ApiError::validation(vec![
            serde_json::json!("query"),
            serde_json::json!("before"),
        ]));
    }
    let store = state.store.ok_or_else(ApiError::internal)?;
    let value = store
        .read_async("runtime-http/routes/messages", move |tx| {
            let value = studio_runtime_services::coordination_reads::chat_read(
                tx, &room, before, after, limit,
            )
            .map_err(coordination_error)?;
            super::generated_json::<crate::generated::ChatRead>(value)
        })
        .await?;
    Ok(Json(value))
}
async fn receipts(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<crate::generated::MessageReceipts>, ApiError> {
    let agent = query
        .typed_value("agent")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let ids = query
        .typed_value("ids")
        .and_then(Value::as_str)
        .unwrap_or("[]");
    let ids: Vec<String> = serde_json::from_str(ids).map_err(|_| {
        ApiError::validation(vec![serde_json::json!("query"), serde_json::json!("ids")])
    })?;
    let workspace = state.workspace_id;
    let store = state.store.ok_or_else(ApiError::internal)?;
    let value = store
        .read_async("runtime-http/routes/messages", move |tx| {
            let mut value =
                studio_runtime_services::coordination_reads::delivery_receipts(tx, &agent, &ids)
                    .map_err(coordination_error)?;
            value["workspaceId"] = Value::String(workspace);
            super::generated_json::<crate::generated::MessageReceipts>(value)
        })
        .await?;
    Ok(Json(value))
}
async fn delete_room(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::RoomDeleteBody>,
) -> Result<Json<crate::generated::MutationReceipt>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let value = store
        .transaction_with_error(move |tx| {
            let value =
                studio_runtime_services::coordination_reads::delete_room(tx, &body.id, |v| {
                    super::generated_json::<crate::generated::RoomEntityDto>(v.clone()).is_ok()
                })?;
            super::generated_json::<crate::generated::MutationReceipt>(value).map_err(|_| {
                CoordinationError::InvalidStored {
                    object: "runtime_rooms",
                    id: body.id.clone(),
                }
            })
        })
        .await
        .map_err(coordination_error)?;
    Ok(Json(value))
}

fn default_connected() -> bool {
    true
}

fn coordination_error(error: CoordinationError) -> ApiError {
    match error {
        CoordinationError::RequiresDependency(dependency) => ApiError::dependency(dependency),
        CoordinationError::InvalidRequest(message) | CoordinationError::Conflict(message) => {
            ApiError::new(StatusCode::BAD_REQUEST, message)
        }
        CoordinationError::InvalidStored { object, id } => invalid_stored(object, &id),
        CoordinationError::AdmissionDenied(message) => {
            ApiError::new(StatusCode::BAD_REQUEST, message)
        }
        CoordinationError::Store(error) => ApiError::stored(error),
    }
}

fn federation_error(
    error: studio_runtime_services::federation_messaging::FederationMessageError,
) -> ApiError {
    use studio_runtime_services::federation_messaging::FederationMessageError as Error;
    match error {
        Error::Invalid(_) | Error::Forbidden(_) => {
            ApiError::new(StatusCode::BAD_REQUEST, "Federation room is unavailable")
        }
        Error::Conflict(_) => ApiError::new(
            StatusCode::CONFLICT,
            "Federation message conflicts with stored state",
        ),
        Error::Unavailable(dependency) => ApiError::dependency(dependency),
        Error::Store(_) | Error::Event(_) | Error::Transport(_) => ApiError::internal(),
    }
}

fn invalid_stored(object: &'static str, id: &str) -> ApiError {
    ApiError::invalid_stored(object, id)
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "messages",
        routes: Some(routes),
        dependencies: &["event-driver/message-send"],
    }
}

#[cfg(test)]
#[path = "../../../runtime-services/tests/wave7_p03_gate_tests/http.rs"]
mod ingress_tests;

#[cfg(test)]
mod delivery_tests {
    use crate::generated::ManagedMessageBody;
    use serde_json::json;
    use studio_runtime_schema::{DeliveryMoment, delivery::LEGACY_DELIVERY_MOMENTS};

    #[test]
    fn managed_message_serde_uses_the_owner_at_ingress() {
        let body =
            json!({"id":"00000000-0000-4000-8000-000000000001", "room":"agent", "text":"hello"});
        let missing: ManagedMessageBody =
            serde_json::from_value(body.clone()).expect("missing timing");
        assert_eq!(
            missing.delivery.unwrap_or_default(),
            DeliveryMoment::CurrentTurn
        );
        for (value, expected) in DeliveryMoment::ALL
            .map(|moment| (moment.as_str(), moment))
            .into_iter()
            .chain(LEGACY_DELIVERY_MOMENTS)
        {
            let mut input = body.clone();
            input["delivery"] = json!(value);
            let parsed: ManagedMessageBody =
                serde_json::from_value(input).expect("supported input");
            assert_eq!(parsed.delivery, Some(expected));
            assert_eq!(
                serde_json::to_value(parsed).expect("canonical output")["delivery"],
                json!(expected)
            );
        }
        for value in [
            json!("unknown"),
            json!(null),
            json!(7),
            json!({}),
            json!([]),
            json!(true),
        ] {
            let mut input = body.clone();
            input["delivery"] = value;
            assert!(serde_json::from_value::<ManagedMessageBody>(input).is_err());
        }
    }
}
