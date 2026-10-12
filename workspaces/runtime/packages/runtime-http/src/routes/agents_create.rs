//! Persisted agent creation and management route slice.

use super::extractors::ApiJson;
use super::manifest::{OperationStatus, RouteGroup, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;
use sha2::{Digest, Sha256};
use studio_runtime_services::agents_create::{self, RenameError};
use studio_runtime_store::SyncDocumentsRepository;

use crate::error::ApiError;
use crate::router::HttpState;

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![post(
        "/api/rename",
        OperationStatus::Implemented,
        rename,
    )])
}

const MAX_MUTATION_SYNC_ENTITIES: i64 = 500;

async fn rename(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::RenameRequest>,
) -> Result<Json<Value>, ApiError> {
    let id = body.id;
    let request_id = body.request_id.flatten();
    let name = body.name.flatten().map(|name| name.trim().to_owned());
    if name.as_ref().is_some_and(String::is_empty) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "A name must have 1 to 80 characters",
        ));
    }
    if name.is_none() && request_id.is_none() {
        return Err(ApiError::validation(vec![Value::String("body".to_owned())]));
    }
    let signature = rename_signature(&id, name.as_deref());
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || {
        let before_connection = store.read().map_err(|_| ApiError::internal())?;
        let sync_after = SyncDocumentsRepository::entity_sequence(&before_connection)
            .map_err(|_| ApiError::internal())?;
        drop(before_connection);
        let mut result = store
            .transaction_with_error(|transaction| {
                agents_create::rename(
                    transaction,
                    &id,
                    name.as_deref(),
                    request_id.as_deref(),
                    &signature,
                )
            })
            .map_err(rename_error)?;
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let rows = SyncDocumentsRepository::entities_after(
            &connection,
            sync_after,
            MAX_MUTATION_SYNC_ENTITIES + 1,
        )
        .map_err(|_| ApiError::internal())?;
        if !rows.is_empty() {
            result["_syncEntities"] = Value::Array(
                rows.iter()
                    .take(MAX_MUTATION_SYNC_ENTITIES as usize)
                    .map(|row| {
                        serde_json::json!({
                            "id": format!("entity:{}:{}", row.collection, row.id),
                            "seq": row.sequence,
                            "payload": row.payload,
                            "_deleted": row.deleted,
                        })
                    })
                    .collect(),
            );
            let tombstone_floor = SyncDocumentsRepository::entity_tombstone_floor(&connection)
                .map_err(|_| ApiError::internal())?;
            if rows.len() <= MAX_MUTATION_SYNC_ENTITIES as usize && tombstone_floor <= sync_after {
                result["_syncEntitiesAfter"] = serde_json::json!(sync_after);
            }
        }
        Ok::<_, ApiError>(result)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(super::generated_json::<
        crate::generated::RenameResponse,
    >(result)?))
}

fn rename_signature(id: &str, name: Option<&str>) -> String {
    let name = name
        .map(python_json_string)
        .unwrap_or_else(|| "null".to_owned());
    let body = format!(
        "{{\"chat\": {}, \"name\": {name}, \"operation\": \"rename\"}}",
        python_json_string(id)
    );
    Sha256::digest(body.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn python_json_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\u{08}' => escaped.push_str("\\b"),
            '\u{0c}' => escaped.push_str("\\f"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            value if (value as u32) < 0x20 => {
                escaped.push_str(&format!("\\u{:04x}", value as u32));
            }
            value if (value as u32) <= 0x7e => escaped.push(value),
            value => {
                let code = value as u32;
                if code <= 0xffff {
                    escaped.push_str(&format!("\\u{code:04x}"));
                } else {
                    let code = code - 0x1_0000;
                    let high = 0xd800 + (code >> 10);
                    let low = 0xdc00 + (code & 0x3ff);
                    escaped.push_str(&format!("\\u{high:04x}\\u{low:04x}"));
                }
            }
        }
    }
    escaped.push('"');
    escaped
}

fn rename_error(error: RenameError) -> ApiError {
    match error {
        RenameError::UnknownAgent => {
            ApiError::new(StatusCode::BAD_REQUEST, "Unknown managed agent")
        }
        RenameError::Deleted => {
            ApiError::new(StatusCode::BAD_REQUEST, "This conversation was deleted")
        }
        RenameError::Conflict => ApiError::new(
            StatusCode::BAD_REQUEST,
            "This request id has different content",
        ),
        RenameError::RequestIdRequired => {
            ApiError::new(StatusCode::BAD_REQUEST, "A rename request id is required")
        }
        RenameError::TitleGenerationUnavailable => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires provider-session/title-generation",
        ),
        RenameError::InvalidStoredObject { object } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {object}"),
        ),
        RenameError::Store(_) => ApiError::internal(),
    }
}

#[cfg(test)]
mod tests {
    use super::rename_signature;
    use sha2::{Digest, Sha256};

    #[test]
    fn rename_signature_matches_python_sorted_json_encoding() {
        let body = r#"{"chat": "chat-1", "name": "Caf\u00e9", "operation": "rename"}"#;
        let expected = Sha256::digest(body.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(rename_signature("chat-1", Some("Café")), expected);
    }
}
