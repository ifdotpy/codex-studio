//! Mac folder sync configuration and conflict resolution routes.

use axum::{Json, extract::State, http::StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_services::{linux_vm_guest::UnixGuestRpc, mac_sync};
use studio_runtime_store::{AgentId, AgentRepository, Transaction};

use super::{
    extractors::ApiJson,
    manifest::{OperationStatus, RouteGroup, post},
};
use crate::{error::ApiError, generated, router::HttpState};

/// Narrow adapter for a non-manual follow-up delivered in the next turn.
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
                delivery: studio_runtime_schema::DeliveryMoment::NextTurn,
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
    // The VM state directory holds the layr project records (`layr-projects/<id>`).
    let root =
        studio_runtime_services::vault::linux_vm_state_dir(std::path::Path::new(&state.state_dir));
    let status = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/mac_sync",
        move || {
            mac_sync::configure(
                &root,
                &body.project_id,
                body.enabled.flatten(),
                body.allow_deletes.unwrap_or(false),
            )
            .map_err(mac_error)
        },
    )
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
    let root = state.vm_state_dir.clone();
    let guest = UnixGuestRpc::new(state.vm_state_dir.clone());
    let project_id = body.project_id;
    let resolve_project_id = project_id.clone();
    let path = body.path;
    let choice = body.choice.as_str().to_owned();
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/mac_sync",
        move || {
            mac_sync::resolve(&root, &resolve_project_id, &path, &choice, &guest).map_err(mac_error)
        },
    )
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
            send_handoff(
                state,
                store,
                &lead,
                &request_id,
                &text,
                result.legacy_handoff_text.as_deref(),
            )
            .await?;
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
    store
        .read_async("runtime-http/routes/mac_sync", move |connection| {
            let mut statement = connection
                .prepare_cached("SELECT id FROM runtime_agents")
                .map_err(|_| ApiError::internal())?;
            let ids = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|_| ApiError::internal())?;
            let mut leads = Vec::new();
            for id in ids {
                let id = id.map_err(|_| ApiError::internal())?;
                let Some(agent) = AgentRepository::get(connection, &AgentId::from(id.as_str()))
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
            Ok::<_, ApiError>(leads.pop().map(|lead| lead.2))
        })
        .await
}

async fn send_handoff(
    state: HttpState,
    store: std::sync::Arc<studio_runtime_store::RuntimeStore>,
    lead: &str,
    request_id: &str,
    text: &str,
    legacy_text: Option<&str>,
) -> Result<(), ApiError> {
    let lead = lead.to_owned();
    let request_id = request_id.to_owned();
    let text = text.to_owned();
    let legacy_text = legacy_text.map(str::to_owned);
    let wake = state.event_wake;
    let custom_admission = state.message_admission;
    let admission_override = custom_admission.clone();
    let lead_for_read = lead.clone();
    let (prepared, ingress_admission) = store
        .read_async("runtime-http/routes/mac_sync", move |connection| {
            AgentRepository::get(connection, &AgentId::from(lead_for_read.as_str()))
                .map_err(|_| ApiError::internal())?
                .ok_or_else(ApiError::internal)?;
            let ingress_admission = if admission_override.is_none() {
                Some(
                    studio_runtime_services::message_admission::MessageIngressAdmission::read(
                        connection,
                        &lead_for_read,
                    )
                    .map_err(|_| ApiError::internal())?,
                )
            } else {
                None
            };
            let default_admission =
                studio_runtime_services::message_admission::PreparedAdmission::read(
                    connection,
                    &lead_for_read,
                )
                .map_err(|_| ApiError::internal())?;
            Ok::<_, ApiError>((default_admission, ingress_admission))
        })
        .await?;
    let prepared = prepared.prepare().await.map_err(|_| ApiError::internal())?;
    let ingress_admission = match ingress_admission {
        Some(snapshot) => Some(snapshot.prepare().await.map_err(|_| ApiError::internal())?),
        None => None,
    };
    let admission: Arc<dyn studio_runtime_services::message_admission::MessageAdmission> =
        custom_admission
            .or_else(|| ingress_admission.map(|value| Arc::new(value) as _))
            .unwrap_or_else(|| Arc::new(prepared));
    let posted = store
        .transaction_with_error(move |transaction| {
            let saved = studio_runtime_store::QueueEventRepository::get(
                transaction,
                &studio_runtime_store::QueueEventId::from(request_id.as_str()),
            )?;
            // Reuse only the exact historical payload of this handoff. Coordination still
            // validates the saved recipient, kind and requested delivery in this transaction.
            let text = saved
                .as_ref()
                .filter(|event| legacy_text.as_deref() == Some(event.text.as_str()))
                .map_or(text.as_str(), |event| event.text.as_str());
            CoordinationHandoffPort.send_after_turn(
                transaction,
                &lead,
                &request_id,
                text,
                now_seconds(),
                admission.as_ref(),
            )
        })
        .await
        .map_err(|_| ApiError::internal())?;
    if posted.event_inserted {
        wake.notify_one();
    }
    Ok(())
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
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use axum::body::Body;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use tokio_util::task::AbortOnDropHandle;
    use tower::ServiceExt;

    struct AllowAdmission;

    impl studio_runtime_services::message_admission::MessageAdmission for AllowAdmission {
        fn workspace(
            &self,
            _: &Transaction<'_>,
            _: &studio_runtime_store::AgentRecord,
        ) -> Result<Vec<Value>, studio_runtime_services::coordination::CoordinationError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn platform_neutral_handoff_http_reuses_legacy_receipt_and_rejects_mismatches() {
        use std::time::Duration;
        use studio_runtime_services::coordination::{SystemFollowup, send_system_followup};
        use studio_runtime_services::linux_vm_guest::{GuestError, GuestRpc};
        use studio_runtime_store::{AgentRecord, RuntimeStore};

        struct Guest;
        impl GuestRpc for Guest {
            fn stream(
                &self,
                method: &str,
                _: &Value,
                _: Option<&str>,
                _: Duration,
                _: &mut dyn FnMut(Value) -> Result<(), GuestError>,
            ) -> Result<Value, GuestError> {
                Ok(match method {
                    "sync.mac.head" => json!({"mainStateId":"main-1"}),
                    "sync.mac.read" => {
                        json!({"kind":"file","mode":420,"data":"Z3Vlc3Q=","eof":true})
                    }
                    "sync.mac.handoff" => {
                        json!({"line":"mac-conflict","stateId":"handoff-1","mainStateId":"main-1"})
                    }
                    _ => json!({"ok":true}),
                })
            }
        }

        async fn saved(store: &RuntimeStore) -> Value {
            store
                .read_async("handoff-test", |tx| {
                    let event: (String, String, String, String) = tx.query_row(
                        "SELECT agent,kind,text,status FROM runtime_events",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )?;
                    let metadata: String =
                        tx.query_row("SELECT record FROM runtime_event_meta", [], |row| {
                            row.get(0)
                        })?;
                    let count: i64 =
                        tx.query_row("SELECT COUNT(*) FROM runtime_events", [], |row| row.get(0))?;
                    Ok::<_, studio_runtime_store::StoreError>(
                        json!({"event":event,"metadata":metadata,"count":count}),
                    )
                })
                .await
                .expect("saved handoff reads")
        }

        let state_dir = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(
            RuntimeStore::open(Some(state_dir.path()))
                .await
                .expect("store opens"),
        );
        let project = state_dir.path().join("layr-projects/p1");
        let folder = state_dir.path().join("host-folder");
        let socket_dir = state_dir.path().join("vm");
        std::fs::create_dir_all(project.join("sync")).expect("project fixture");
        std::fs::create_dir_all(&folder).expect("host fixture");
        std::fs::create_dir_all(&socket_dir).expect("socket fixture");
        std::fs::write(
            project.join("project.json"),
            json!({"projectId":"p1","source":folder}).to_string(),
        )
        .expect("project record");
        std::fs::write(
            project.join("sync/held.json"),
            json!({"file.txt":{"h":"old","why":"conflict"}}).to_string(),
        )
        .expect("held conflict");
        std::fs::write(
            project.join("sync/status.json"),
            json!({"state":"conflict","conflicts":["file.txt"]}).to_string(),
        )
        .expect("sync status");
        std::fs::write(folder.join("file.txt"), b"host").expect("host file");
        let first = mac_sync::resolve(state_dir.path(), "p1", "file.txt", "agent", &Guest)
            .expect("initial guest handoff");
        assert!(
            first
                .handoff_text
                .as_deref()
                .expect("new handoff text")
                .starts_with("[Studio folder sync]")
        );
        let id = first.handoff_id.expect("stable handoff id");
        let legacy = "[Studio Mac sync] The user's Mac folder and main changed file.txt in different ways. The user asked you to combine the two versions. The Mac version is layr state handoff-1 in the line mac-conflict. Read the Mac version with `layr show handoff-1:file.txt`. Put one version that keeps both changes in main and commit it. Studio then writes main's file to the Mac folder.";
        let cwd = folder.to_string_lossy().into_owned();
        let seed_id = id.clone();
        store.transaction_with_error(move |tx| {
            for agent_id in ["lead", "other"] {
                let agent = AgentRecord::from_value(json!({"id":agent_id,"rootId":agent_id,"epoch":0,"status":"waiting","autoWake":true,"cwd":cwd,"isLead":agent_id=="lead","layrProjectId":"p1","layrLine":"main"})).expect("valid lead fixture");
                AgentRepository::put(tx, &agent)?;
            }
            send_system_followup(tx, SystemFollowup { recipient_id:"lead", id:&seed_id, text:legacy, delivery:studio_runtime_schema::DeliveryMoment::NextTurn, now:123.0 }, &AllowAdmission)?;
            tx.execute("UPDATE runtime_events SET status='delivered'", [])?;
            Ok::<_, studio_runtime_services::coordination::CoordinationError>(())
        }).await.expect("historical followup seeds");
        let before = saved(&store).await;
        let listener = UnixListener::bind(socket_dir.join("guest.sock")).expect("guest socket");
        let server = AbortOnDropHandle::new(tokio::spawn(async move {
            for method in ["sync.mac.head", "sync.mac.read"]
                .into_iter()
                .cycle()
                .take(12)
            {
                let (stream, _) = listener.accept().await.expect("guest request");
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.expect("guest frame");
                let request: Value = serde_json::from_str(&line).expect("guest request JSON");
                assert_eq!(
                    request["method"], method,
                    "retry must never repeat sync.mac.handoff"
                );
                let result = if method == "sync.mac.head" {
                    json!({"mainStateId":"main-1"})
                } else {
                    json!({"kind":"file","mode":420,"data":"Z3Vlc3Q=","eof":true})
                };
                reader
                    .into_inner()
                    .write_all(
                        format!("{}\n", json!({"id":request["id"],"result":result})).as_bytes(),
                    )
                    .await
                    .expect("guest answer");
            }
        }));
        let mut state = HttpState::new(
            "fixture-token".into(),
            "fixture".into(),
            state_dir.path().to_string_lossy().into_owned(),
            49171,
            None,
            Some(Arc::clone(&store)),
            None,
        )
        .with_vm_state_dir(socket_dir);
        state.message_admission = Some(Arc::new(AllowAdmission));
        let app = crate::router::router(state).expect("HTTP routes");
        let request = || {
            axum::http::Request::builder()
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
                    "127.0.0.1:12345"
                        .parse::<std::net::SocketAddr>()
                        .expect("local address"),
                ))
                .body(Body::from(
                    json!({"projectId":"p1","path":"file.txt","choice":"agent"}).to_string(),
                ))
                .expect("HTTP request")
        };
        for _ in 0..2 {
            let response = app.clone().oneshot(request()).await.expect("HTTP replay");
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .expect("response body");
            assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
            assert_eq!(
                saved(&store).await,
                before,
                "saved receipt, text and metadata remain exact"
            );
        }
        for mismatch in ["text", "agent", "kind", "delivery"] {
            let id = id.clone();
            store.transaction_with_error(move |tx| {
                tx.execute("UPDATE runtime_events SET agent='lead',kind='followup',text=?1",[legacy])?;
                tx.execute("UPDATE runtime_event_meta SET record=json_set(record,'$.requestedDelivery','next_turn')",[])?;
                match mismatch {
                    "text" => { tx.execute("UPDATE runtime_events SET text=text || ' changed' WHERE id=?1",[&id])?; }
                    "agent" => { tx.execute("UPDATE runtime_events SET agent='other' WHERE id=?1",[&id])?; }
                    "kind" => { tx.execute("UPDATE runtime_events SET kind='user' WHERE id=?1",[&id])?; }
                    "delivery" => { tx.execute("UPDATE runtime_event_meta SET record=json_set(record,'$.requestedDelivery','current_turn') WHERE id=?1",[&id])?; }
                    _ => unreachable!(),
                }
                Ok::<_,studio_runtime_store::StoreError>(())
            }).await.expect("mismatched fixture writes");
            let changed = saved(&store).await;
            let response = app
                .clone()
                .oneshot(request())
                .await
                .expect("mismatch response");
            assert_eq!(
                response.status(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "{mismatch} must not bypass identity checks"
            );
            assert_eq!(
                saved(&store).await,
                changed,
                "mismatch changes no saved receipt"
            );
        }
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("guest responder ends")
            .expect("guest responder succeeds");
    }

    #[tokio::test]
    async fn resolve_http_uses_a_fake_guest_endpoint_and_writes_vm_choice() {
        let state_dir = tempfile::tempdir().expect("fixture operation succeeds");
        // The VM state directory owns the layr project records, as on a real install.
        let project = state_dir.path().join("linux-vm/layr-projects/p1");
        let folder = state_dir.path().join("mac-folder");
        let socket_dir = state_dir.path().join("private-vm");
        std::fs::create_dir_all(project.join("sync")).expect("fixture operation succeeds");
        std::fs::create_dir_all(&folder).expect("fixture operation succeeds");
        std::fs::create_dir_all(&socket_dir).expect("fixture operation succeeds");
        std::fs::write(
            project.join("project.json"),
            json!({"projectId":"p1","source":folder}).to_string(),
        )
        .expect("fixture operation succeeds");
        std::fs::write(
            project.join("sync/held.json"),
            json!({"file.txt":{"h":"old","why":"conflict"}}).to_string(),
        )
        .expect("fixture operation succeeds");
        std::fs::write(project.join("sync/status.json"), json!({"state":"conflict","conflicts":["file.txt"],"choices":[],"withAgent":[],"deletes":0}).to_string()).expect("fixture operation succeeds");
        std::fs::write(folder.join("file.txt"), b"mac").expect("fixture operation succeeds");
        let listener =
            UnixListener::bind(socket_dir.join("guest.sock")).expect("fixture operation succeeds");
        let server = AbortOnDropHandle::new(tokio::spawn(async move {
            for method in ["sync.mac.head", "sync.mac.read"] {
                let (stream, _) = listener.accept().await.expect("fixture operation succeeds");
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader
                    .read_line(&mut line)
                    .await
                    .expect("fixture operation succeeds");
                let request: Value =
                    serde_json::from_str(&line).expect("fixture operation succeeds");
                assert_eq!(request["method"], method);
                let result = match method {
                    "sync.mac.head" => json!({"mainStateId":"main-1"}),
                    "sync.mac.read" => {
                        json!({"kind":"file","mode":420,"data":"Z3Vlc3Q=","eof":true})
                    }
                    other => panic!("unexpected guest method: {other}"),
                };
                let mut stream = reader.into_inner();
                stream
                    .write_all(
                        format!("{}\n", json!({"id":request["id"],"result":result})).as_bytes(),
                    )
                    .await
                    .expect("fake guest sends response");
            }
        }));
        let state = crate::router::HttpState::new(
            "fixture-token".into(),
            "fixture".into(),
            state_dir.path().to_string_lossy().into_owned(),
            49171,
            None,
            None,
            None,
        )
        .with_vm_state_dir(state_dir.path().join("private-vm"));
        let app = crate::router::router(state).expect("fixture operation succeeds");
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
                "127.0.0.1:12345"
                    .parse::<std::net::SocketAddr>()
                    .expect("fixture operation succeeds"),
            ))
            .body(Body::from(
                json!({"projectId":"p1","path":"file.txt","choice":"vm"}).to_string(),
            ))
            .expect("fixture operation succeeds");
        let response = app
            .oneshot(request)
            .await
            .expect("fixture operation succeeds");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("fixture operation succeeds");
        let value = serde_json::from_slice::<Value>(&bytes).expect("fixture operation succeeds");
        assert_eq!(value["mainStateId"], "main-1");
        assert_eq!(value["status"]["state"], "synced");
        assert_eq!(
            std::fs::read(folder.join("file.txt")).expect("fixture operation succeeds"),
            b"guest"
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("socket responder stops")
            .expect("socket responder succeeds");
    }

    #[tokio::test]
    async fn config_http_contract_and_invalid_resolve_refusal_are_quiescent() {
        let state_dir = tempfile::tempdir().expect("fixture operation succeeds");
        // The VM state directory owns the layr project records, as on a real install.
        let project = state_dir.path().join("linux-vm/layr-projects/p1");
        let folder = state_dir.path().join("mac-folder");
        std::fs::create_dir_all(project.join("sync")).expect("fixture operation succeeds");
        std::fs::create_dir_all(&folder).expect("fixture operation succeeds");
        std::fs::write(
            project.join("project.json"),
            json!({"projectId":"p1","source":folder}).to_string(),
        )
        .expect("fixture operation succeeds");
        std::fs::write(
            project.join("sync/status.json"),
            json!({"state":"syncing","conflicts":[],"choices":[],"withAgent":[],"deletes":0})
                .to_string(),
        )
        .expect("fixture operation succeeds");
        let state = crate::router::HttpState::new(
            "fixture-token".into(),
            "fixture".into(),
            state_dir.path().to_string_lossy().into_owned(),
            49171,
            None,
            None,
            None,
        )
        .with_vm_state_dir(state_dir.path().join("private-vm"));
        let app = crate::router::router(state).expect("fixture operation succeeds");
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
                    "127.0.0.1:12345"
                        .parse::<std::net::SocketAddr>()
                        .expect("fixture operation succeeds"),
                ))
                .body(Body::from(body.to_string()))
                .expect("fixture operation succeeds")
        };
        let before =
            std::fs::read(project.join("sync/status.json")).expect("fixture operation succeeds");
        let refused = app
            .clone()
            .oneshot(request(
                "/api/mac-sync/resolve",
                json!({"projectId":"p1","path":"file.txt","choice":"both"}),
            ))
            .await
            .expect("fixture operation succeeds");
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            std::fs::read(project.join("sync/status.json")).expect("fixture operation succeeds"),
            before
        );
        assert!(!project.join("sync/round.lock").exists());

        let response = app
            .oneshot(request(
                "/api/mac-sync/config",
                json!({"projectId":"p1","enabled":false,"allowDeletes":false}),
            ))
            .await
            .expect("fixture operation succeeds");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("fixture operation succeeds");
        let value: Value = serde_json::from_slice(&bytes).expect("fixture operation succeeds");
        assert_eq!(value["state"], "paused");
        assert_eq!(value["enabled"], false);
        assert!(project.join("sync/config.json").is_file());
    }
}
