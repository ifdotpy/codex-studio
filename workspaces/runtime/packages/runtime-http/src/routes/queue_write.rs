//! HTTP adapter for durable queue mutations.

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};
use studio_runtime_schema::queue_command::QueueCommand;
use studio_runtime_services::queue_write::{self, QueueWriteError};

use super::extractors::ApiJson;
use crate::error::ApiError;
use crate::router::HttpState;

pub(crate) async fn queue_action(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<QueueCommand>,
) -> Result<Json<Value>, ApiError> {
    validate_queue_action(&body)?;
    let agent_id = body
        .agent()
        .map(str::to_owned)
        .unwrap_or_else(|| state.workspace_id.clone());
    let signature =
        super::work_items::python_operation_signature(&json!({"agent": agent_id, "body": body}))?;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let result = super::sync_delta::write(
        store,
        move |transaction| {
            queue_write::action(
                transaction,
                &agent_id,
                &body,
                &signature,
                |connection, agent| super::queue_read::queue_snapshot(connection, agent),
            )
        },
        write_error,
    )
    .await?;
    state.event_wake.notify_one();
    Ok(Json(result))
}

fn validate_queue_action(body: &QueueCommand) -> Result<(), ApiError> {
    let has_id = body.target_id().is_some_and(|id| !id.is_empty());
    if matches!(
        body,
        QueueCommand::Edit(_) | QueueCommand::First(_) | QueueCommand::SendNow(_)
    ) && !has_id
    {
        return Err(ApiError::validation(vec![Value::String("body".into())]));
    }
    if matches!(body, QueueCommand::SendNow(_))
        && !body.request_id().is_some_and(|value| !value.is_empty())
    {
        return Err(ApiError::validation(vec![Value::String("body".into())]));
    }
    Ok(())
}

fn write_error(error: QueueWriteError) -> ApiError {
    super::queue_read::api_error(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::extract::ConnectInfo;
    use axum::http::Request;
    use std::sync::Arc;
    use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore};
    use tower::ServiceExt;

    async fn call(app: &axum::Router, body: Value) -> (u16, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/queue")
                    .header("host", "127.0.0.1:4620")
                    .header("content-type", "application/json")
                    .header("x-canvas-token", "token")
                    .header(crate::API_SCHEMA_HASH_HEADER, crate::api_schema_hash())
                    .extension(ConnectInfo(
                        "127.0.0.1:12345"
                            .parse::<std::net::SocketAddr>()
                            .expect("peer"),
                    ))
                    .body(Body::from(body.to_string()))
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = response.status().as_u16();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (
            status,
            serde_json::from_slice(&bytes).expect("JSON response"),
        )
    }

    #[test]
    fn queue_typed_signatures_preserve_wire_presence_and_aliases() {
        for action in ["cancel", "edit", "first", "send_now", "reorder"] {
            let mut original = json!({"action":action,"id":"a","request_id":" request "});
            if action == "edit" {
                original["text"] = json!(" \u{2003}text ");
            }
            if action == "reorder" {
                original["expected_revision"] = json!("revision");
                original["ordered_ids"] = json!(["b", "a"]);
            }
            let fields: &[&str] = if action == "reorder" {
                &["agent", "id", "request_id"]
            } else {
                &[
                    "agent",
                    "message_id",
                    "expected_revision",
                    "expectedText",
                    "request_id",
                ]
            };
            for field in fields {
                for value in [Value::Null, json!(""), json!(" \u{2003}value ")] {
                    let mut wire = original.clone();
                    wire[*field] = value;
                    let typed: QueueCommand =
                        serde_json::from_value(wire.clone()).expect("command");
                    // The generated request must be the shared command itself.
                    let typed: crate::generated::QueueActionApiQueuePostRequest = typed;
                    assert_eq!(
                        super::super::work_items::python_operation_signature(
                            &json!({"agent":"workspace","body":wire})
                        )
                        .expect("old signature"),
                        super::super::work_items::python_operation_signature(
                            &json!({"agent":"workspace","body":typed})
                        )
                        .expect("typed signature")
                    );
                }
            }
        }
    }

    async fn failure(app: &axum::Router, body: Value, message: &str) {
        assert_eq!(call(app, body).await, (400, json!({"error":message})));
    }

    #[tokio::test]
    async fn queue_http_typed_commands_preserve_order_guards_and_replay() {
        let directory = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("store"),
        );
        store.transaction(|tx| {
            AgentRepository::put(tx, &AgentRecord::from_value(json!({"id":"workspace","rootId":"workspace","epoch":0,"status":"idle","provider":studio_runtime_schema::ProviderKind::Codex}))?)?;
            tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES('a','workspace','user','A','pending',1,0),('b','workspace','user','B','pending',2,0),('c','workspace','followup','C','pending',3,0)", [])?;
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
        let app = crate::router(state).expect("router");
        let reader = store.test_reader().expect("reader");
        let agent = AgentRepository::get(&reader, &"workspace".into())
            .expect("agent read")
            .expect("agent");
        let revision = queue_write::queue_snapshot(&reader, &agent)
            .expect("queue")
            .revision;
        let reorder = json!({"action":"reorder","ordered_ids":["b","a","c"],"expected_revision":revision,"request_id":" reorder "});
        let (status, receipt) = call(&app, reorder.clone()).await;
        assert_eq!(status, 200, "{receipt}");
        assert_eq!(
            call(&app, reorder.clone()).await,
            (200, receipt.clone()),
            "stale revision replay returns original receipt"
        );
        let mut changed_identity = reorder;
        changed_identity["agent"] = Value::Null;
        failure(
            &app,
            changed_identity,
            "This request id has different content",
        )
        .await;
        let snapshot = queue_write::queue_snapshot(&reader, &agent).expect("reordered queue");
        assert_eq!(
            snapshot
                .items
                .iter()
                .map(|item| item["id"].as_str().expect("id"))
                .collect::<Vec<_>>(),
            vec!["b", "a", "c"]
        );
        for revision in [Value::Null, json!("stale")] {
            failure(
                &app,
                json!({"action":"edit","id":"a","text":"changed","expected_revision":revision}),
                "This queue changed. Reload before editing or reordering",
            )
            .await;
        }
        failure(
            &app,
            json!({"action":"edit","id":"a","text":"changed","expectedText":"different"}),
            "This queued message changed",
        )
        .await;
        let edit = json!({"action":"edit","id":"a","message_id":"b","text":" Edited ","request_id":"edit"});
        let edit_receipt = call(&app, edit.clone()).await;
        assert_eq!(edit_receipt.0, 200, "{}", edit_receipt.1);
        let text: String = reader
            .query_row("SELECT text FROM runtime_events WHERE id='b'", [], |row| {
                row.get(0)
            })
            .expect("edited message");
        assert_eq!(text, "Edited", "message_id takes precedence");
        assert_eq!(
            call(&app, json!({"action":"first","message_id":"","id":"a"}))
                .await
                .0,
            200,
            "empty message_id falls back to id"
        );
        failure(
            &app,
            json!({"action":"send_now","id":"c","request_id":"followup"}),
            "Only a user message can be sent now",
        )
        .await;
        assert_eq!(
            call(
                &app,
                json!({"action":"send_now","id":"b","request_id":"send"})
            )
            .await
            .0,
            200
        );
        assert_eq!(call(&app, json!({"action":"cancel","id":"a"})).await.0, 200);
        assert_eq!(
            call(&app, edit.clone()).await,
            edit_receipt,
            "replay does not rerun an edit after promotion"
        );
        let mut conflicting_edit = edit;
        conflicting_edit["text"] = json!("Different");
        failure(
            &app,
            conflicting_edit,
            "This request id has different content",
        )
        .await;
        for invalid in [
            json!({"action":"edit","id":"b"}),
            json!({"action":"cancel","id":"b","extra":true}),
            json!({"action":"first","id":42}),
            json!({"action":"send_now","id":"b"}),
            json!({"action":"reorder","expected_revision":receipt["revision"],"ordered_ids":["b","c"]}),
        ] {
            assert_eq!(call(&app, invalid.clone()).await.0, 400, "{invalid}");
        }
    }
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "queue_write",
        routes: None,
        dependencies: &["event-driver/queue-mutations"],
    }
}
