//! HTTP adapter for durable queue mutations.

use axum::Extension;
use axum::Json;
use axum::extract::State;
use std::sync::Arc;
use studio_runtime_schema::queue_command::QueueCommand;
use studio_runtime_services::agent_lifecycle_runtime::{self, LifecyclePorts};
use studio_runtime_services::agent_management::Action;
use studio_runtime_services::queue_write;

use super::extractors::ApiJson;
use crate::error::ApiError;
use crate::router::HttpState;

pub(crate) async fn queue_action(
    State(state): State<HttpState>,
    ports: Option<Extension<Arc<dyn LifecyclePorts>>>,
    ApiJson(body): ApiJson<QueueCommand>,
) -> Result<Json<queue_write::QueueReply>, ApiError> {
    let agent_id = body
        .agent()
        .map(str::to_owned)
        .unwrap_or_else(|| state.workspace_id.clone());
    let signature = signature(&agent_id, &body)?;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let held_agent = agent_id.clone();
    let result = store
        .transaction_with_error(move |transaction| {
            queue_write::action(
                transaction,
                &agent_id,
                &body,
                &signature,
                |connection, agent| super::queue_read::queue_snapshot(connection, agent),
            )
        })
        .await;
    if let (Err(queue_write::QueueWriteError::HeldByTurn), Some(Extension(ports))) =
        (&result, ports)
    {
        // Withdraw of a message that a running turn holds: interrupt that turn.
        // The message returns to the queue or is delivered when the turn ends.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| ApiError::internal())?
            .as_secs_f64();
        agent_lifecycle_runtime::execute(
            Arc::clone(&store),
            &Action::Stop {
                target: held_agent,
                descendants: false,
                reason: None,
                sender: None,
                sender_epoch: None,
            },
            now,
            ports,
        )
        .await
        .map_err(|_| ApiError::internal())?;
    }
    let result = result.map_err(super::queue_read::api_error)?;
    state.event_wake.notify_one();
    Ok(Json(result))
}

fn signature(agent_id: &str, body: &QueueCommand) -> Result<String, ApiError> {
    #[derive(serde::Serialize)]
    struct Signature<'a> {
        agent: &'a str,
        body: &'a QueueCommand,
    }
    let signature = serde_json::to_vec(&Signature {
        agent: agent_id,
        body,
    })
    .map_err(|_| ApiError::internal())?;
    Ok(studio_runtime_store::sha256_hex(&signature))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::extract::ConnectInfo;
    use axum::http::Request;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use studio_runtime_store::{
        AgentRecord, AgentRepository, RuntimeStore, SyncDocumentsRepository,
    };
    use tower::ServiceExt;

    async fn call(app: &axum::Router, body: Value) -> (u16, Value) {
        call_raw(app, body.to_string()).await
    }

    async fn call_raw(app: &axum::Router, body: String) -> (u16, Value) {
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
                    .body(Body::from(body))
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
    fn queue_signature_is_stable_and_normalizes_null() {
        let id: QueueCommand =
            serde_json::from_str(r#"{"action":"first","id":"a","request_id":"request"}"#)
                .expect("command");
        let normalized: QueueCommand = serde_json::from_str(
            r#"{"action":"first","id":"a","request_id":"request","agent":null}"#,
        )
        .expect("normalized command");
        let signature = super::signature("workspace", &id).expect("signature");
        assert_eq!(
            signature,
            "b8eda4ce408a100862a1804cbff673d5540ca2b793adb39c4ead266d74b766b1"
        );
        assert_eq!(
            signature,
            super::signature("workspace", &normalized).expect("normalized signature")
        );
        let legacy = super::super::work_items::python_operation_signature(
            &json!({"agent":"workspace","body":{"action":"first","id":"a","request_id":"request"}}),
        )
        .expect("legacy signature");
        assert_ne!(
            signature, legacy,
            "pre-upgrade receipts refuse replay safely"
        );
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
            AgentRepository::put(tx, &AgentRecord::from_value(json!({"compactions":0,"accountKey":"default","id":"workspace","rootId":"workspace","epoch":0,"status":"idle","provider":studio_runtime_schema::ProviderKind::Codex}))?)?;
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
        for body in [
            json!({"action":"first"}),
            json!({"action":"edit","id":"a","text":"x".repeat(32001)}),
            json!({"action":"first","id":"a","request_id":"x".repeat(201)}),
            json!({"action":"first","id":"a","unknown":true}),
        ] {
            assert_eq!(call(&app, body).await.0, 400);
        }
        assert_eq!(
            call_raw(&app, r#"{"action":"first","id":"a","id":"b"}"#.into())
                .await
                .0,
            400
        );
        let reader = store.test_reader().expect("reader");
        let agent = AgentRepository::get(&reader, &"workspace".into())
            .expect("agent read")
            .expect("agent");
        let revision = queue_write::queue_snapshot(&reader, &agent)
            .expect("queue")
            .revision;
        let reorder = json!({"action":"reorder","ordered_ids":["b","a","c"],"expected_revision":revision,"request_id":studio_runtime_schema::persistence::RequestIdentity::issue_root()});
        let (status, receipt) = call(&app, reorder.clone()).await;
        assert_eq!(status, 200, "{receipt}");
        let stored: String = reader
            .query_row("SELECT result FROM request_results", [], |row| row.get(0))
            .expect("stored queue receipt");
        let envelope: studio_runtime_store::request_results::ResultEnvelope<
            queue_write::QueueReply,
        > = serde_json::from_str(&stored).expect("typed stored queue receipt");
        assert_eq!(
            serde_json::from_str::<Value>(&serde_json::to_string(&envelope).expect("serialized"))
                .expect("JSON"),
            serde_json::from_str::<Value>(&stored).expect("stored JSON")
        );
        assert_eq!(
            call(&app, reorder.clone()).await,
            (200, receipt.clone()),
            "stale revision replay returns original receipt"
        );
        let legacy = super::super::work_items::python_operation_signature(
            &json!({"agent":"workspace", "body":reorder}),
        )
        .expect("legacy signature");
        let typed: QueueCommand = serde_json::from_value(reorder.clone()).expect("queue command");
        let current = super::signature("workspace", &typed).expect("current signature");
        store
            .transaction(move |tx| {
                tx.execute("UPDATE request_results SET signature=?1", [legacy])?;
                Ok(())
            })
            .await
            .expect("seed pre-upgrade receipt");
        assert_eq!(
            call(&app, reorder.clone()).await.0,
            409,
            "old receipt refuses replay"
        );
        store
            .transaction(move |tx| {
                tx.execute("UPDATE request_results SET signature=?1", [current])?;
                Ok(())
            })
            .await
            .expect("restore receipt");
        let mut changed_identity = reorder.clone();
        changed_identity["agent"] = Value::Null;
        assert_eq!(call(&app, changed_identity).await, (200, receipt.clone()));
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
                json!({"action":"edit","id":"a","text":"changed","expected_revision":revision,"request_id":studio_runtime_schema::persistence::RequestIdentity::issue_root()}),
                "This queue changed. Reload before editing or reordering",
            )
            .await;
        }
        failure(
            &app,
            json!({"action":"edit","id":"a","text":"changed","expectedText":"different","request_id":studio_runtime_schema::persistence::RequestIdentity::issue_root()}),
            "This queued message changed",
        )
        .await;
        assert_eq!(call(&app, json!({"action":"edit","id":"a","message_id":"b","text":" Edited ","request_id":studio_runtime_schema::persistence::RequestIdentity::issue_root()})).await.0, 400);
        let edit = json!({"action":"edit","id":"b","text":" Edited ","request_id":studio_runtime_schema::persistence::RequestIdentity::issue_root()});
        let edit_receipt = call(&app, edit.clone()).await;
        assert_eq!(edit_receipt.0, 200, "{}", edit_receipt.1);
        let text: String = reader
            .query_row("SELECT text FROM runtime_events WHERE id='b'", [], |row| {
                row.get(0)
            })
            .expect("edited message");
        assert_eq!(text, "Edited", "id selects the target");
        assert_eq!(
            call(&app, reorder).await,
            (200, receipt.clone()),
            "an earlier root replays its original full response after another mutation"
        );
        let before_expired = queue_write::queue_snapshot(&reader, &agent)
            .expect("queue before expiry refusal")
            .revision;
        let expired = call(
            &app,
            json!({"action":"first","id":"b","request_id":"f43c1885-f4fc-485b-bf99-8b4f31e7e7c4"}),
        )
        .await;
        assert_eq!(expired.0, 410, "{expired:?}");
        assert!(
            expired.1["error"]
                .as_str()
                .expect("expiry error")
                .contains("expired")
        );
        assert_eq!(
            queue_write::queue_snapshot(&reader, &agent)
                .expect("queue after expiry refusal")
                .revision,
            before_expired
        );

        assert_eq!(
            call(&app, json!({"action":"first","message_id":"","id":"a","request_id":studio_runtime_schema::persistence::RequestIdentity::issue_root()}))
                .await
                .0,
            400,
            "simultaneous aliases are rejected"
        );
        failure(
            &app,
            json!({"action":"send_now","id":"c","request_id":studio_runtime_schema::persistence::RequestIdentity::issue_root()}),
            "Only a user message can be sent now",
        )
        .await;
        assert_eq!(
            call(
                &app,
                json!({"action":"send_now","id":"b","request_id":studio_runtime_schema::persistence::RequestIdentity::issue_root()})
            )
            .await
            .0,
            200
        );
        assert_eq!(call(&app, json!({"action":"first","id":"a","request_id":studio_runtime_schema::persistence::RequestIdentity::issue_root()})).await.0, 200);
        assert_eq!(
            call(&app, edit.clone()).await,
            edit_receipt,
            "replay does not rerun an edit after promotion"
        );
        let mut conflicting_edit = edit;
        conflicting_edit["text"] = json!("Different");
        assert_eq!(call(&app, conflicting_edit).await.0, 409);
        for invalid in [
            json!({"action":"edit","id":"b"}),
            json!({"action":"first","id":"b","extra":true}),
            json!({"action":"first","id":42}),
            json!({"action":"send_now","id":"b"}),
            json!({"action":"reorder","expected_revision":receipt["revision"],"ordered_ids":["b","c"]}),
        ] {
            assert_eq!(call(&app, invalid.clone()).await.0, 400, "{invalid}");
        }
    }

    async fn seeded_store(directory: &std::path::Path) -> Arc<RuntimeStore> {
        let store = Arc::new(RuntimeStore::open(Some(directory)).await.expect("store"));
        store.transaction(|tx| {
            AgentRepository::put(tx, &AgentRecord::from_value(json!({"accountKey":"default","id":"workspace","rootId":"workspace","epoch":0,"status":"idle","provider":studio_runtime_schema::ProviderKind::Codex}))?)?;
            tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES('late','workspace','user','Late','pending',3,0),('early','workspace','user','Early','pending',1,0),('note','workspace','followup','Note','pending',2,0)", [])?;
            Ok(())
        }).await.expect("seed queue");
        store
    }

    fn queued_ids(store: &RuntimeStore) -> Vec<String> {
        let reader = store.test_reader().expect("reader");
        let agent = AgentRepository::get(&reader, &"workspace".into())
            .expect("agent read")
            .expect("agent");
        queue_write::queue_snapshot(&reader, &agent)
            .expect("queue")
            .items
            .iter()
            .map(|item| item["id"].as_str().expect("id").to_owned())
            .collect()
    }

    // The queue is server state: a bridge that restarts, or is gone, changes nothing.
    #[tokio::test]
    async fn queue_order_is_server_state_and_survives_a_restart() {
        let directory = tempfile::tempdir().expect("isolated state");
        let first = seeded_store(directory.path()).await;
        let before = queued_ids(&first);
        drop(first);
        assert_eq!(before, ["early", "note", "late"]);
        let reopened = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("reopened store");
        assert_eq!(queued_ids(&reopened), before);
    }

    #[tokio::test]
    async fn withdrawing_a_queued_user_message_returns_its_text_as_a_server_draft() {
        let directory = tempfile::tempdir().expect("isolated state");
        let store = seeded_store(directory.path()).await;
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
        let withdraw = |id: &'static str| json!({"action":"cancel","id":id,"request_id":studio_runtime_schema::persistence::RequestIdentity::issue_root()});
        let draft = || {
            let store = store.clone();
            async move {
                store
                    .transaction(|tx| SyncDocumentsRepository::draft(tx, "server:workspace"))
                    .await
                    .expect("draft read")
                    .map(|document| {
                        serde_json::from_str::<Value>(&document.payload).expect("draft JSON")
                            ["text"]
                            .as_str()
                            .expect("draft text")
                            .to_owned()
                    })
            }
        };
        assert_eq!(call(&app, withdraw("note")).await.0, 200);
        assert_eq!(
            draft().await,
            None,
            "only user messages return to the composer"
        );
        assert_eq!(call(&app, withdraw("early")).await.0, 200);
        assert_eq!(draft().await.as_deref(), Some("Early"));
        assert_eq!(call(&app, withdraw("late")).await.0, 200);
        assert_eq!(draft().await.as_deref(), Some("Early\n\nLate"));
        assert!(
            queued_ids(&store).is_empty(),
            "the withdrawn messages left the queue"
        );
    }

    #[tokio::test]
    async fn withdrawing_a_message_held_by_a_turn_is_refused_until_the_turn_ends() {
        let directory = tempfile::tempdir().expect("isolated state");
        let store = seeded_store(directory.path()).await;
        store
            .transaction(|tx| {
                tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES('held','workspace','user','Held','dispatching',0,0)", [])?;
                Ok(())
            })
            .await
            .expect("held message");
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
        let body = json!({"action":"cancel","id":"held","request_id":studio_runtime_schema::persistence::RequestIdentity::issue_root()});
        assert_eq!(call(&app, body).await.0, 409);
        let status: String = store
            .test_reader()
            .expect("reader")
            .query_row(
                "SELECT status FROM runtime_events WHERE id='held'",
                [],
                |row| row.get(0),
            )
            .expect("status");
        assert_eq!(status, "dispatching", "the held message is not cancelled");
    }
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "queue_write",
        routes: None,
        dependencies: &["event-driver/queue-mutations"],
    }
}
