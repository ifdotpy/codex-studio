//! Thin HTTP lifecycle edge.
use super::{
    extractors::ApiJson,
    manifest::{OperationStatus, RouteGroup, post},
};
use crate::{error::ApiError, generated, router::HttpState};
use axum::{Extension, Json, extract::State, http::StatusCode};
use serde_json::Value;
use std::sync::Arc;
use studio_runtime_services::{
    agent_lifecycle_runtime::{self, LifecyclePorts},
    agent_management::{Action, ManagementError},
    agent_management_store,
    message_admission::MessageAdmission,
    native_action_ports,
    native_actions::{self, Admission, NativeError},
};
use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, RuntimeStore};

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post("/api/action", OperationStatus::Implemented, native_action),
        post(
            "/api/conversation/delete",
            OperationStatus::Implemented,
            delete,
        ),
        post("/api/stop", OperationStatus::Implemented, stop),
    ])
}

async fn stop(
    State(state): State<HttpState>,
    ports: Option<Extension<Arc<dyn LifecyclePorts>>>,
    ApiJson(body): ApiJson<generated::StopRequest>,
) -> Result<Json<generated::StopResponse>, ApiError> {
    dispatch::<generated::StopResponse>(
        state,
        Action::Stop {
            target: body.id,
            descendants: body.descendants.unwrap_or(true),
            reason: None,
            sender: None,
            sender_epoch: None,
        },
        ports.map(|Extension(ports)| ports),
    )
    .await
}
async fn delete(
    State(state): State<HttpState>,
    ports: Option<Extension<Arc<dyn LifecyclePorts>>>,
    ApiJson(body): ApiJson<generated::IdRequest>,
) -> Result<Json<generated::DeletedResponse>, ApiError> {
    dispatch::<generated::DeletedResponse>(
        state,
        Action::Delete { target: body.id },
        ports.map(|Extension(ports)| ports),
    )
    .await
}

type NativeActionPorts = (
    Arc<dyn studio_runtime_services::account_session_runtime::AccountSessionPort>,
    Option<Arc<dyn studio_runtime_services::linux_vm_guest::GuestRpc>>,
);

pub(super) fn native_action_provider(
    store: &RuntimeStore,
    agent: &AgentRecord,
    host_port: Option<
        Arc<dyn studio_runtime_services::account_session_runtime::AccountSessionPort>,
    >,
    configured_guest: Option<Arc<dyn studio_runtime_services::linux_vm_guest::GuestRpc>>,
) -> Result<NativeActionPorts, ApiError> {
    if agent.extra.get("executionMode").and_then(Value::as_str) != Some("vm") {
        return host_port
            .map(|port| (port, configured_guest))
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::NOT_IMPLEMENTED,
                    "Not implemented: requires provider-session/native-request",
                )
            });
    }
    #[cfg(unix)]
    let guest = configured_guest.unwrap_or_else(|| {
        Arc::new(studio_runtime_services::linux_vm_guest::UnixGuestRpc::new(
            vm_guest_state_dir(
                store.state_dir(),
                std::env::var_os("CODEX_LINUX_VM_STATE_DIR").map(std::path::PathBuf::from),
            ),
        ))
    });
    #[cfg(not(unix))]
    let guest = configured_guest.ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires workspace-state/vm-guest",
        )
    })?;
    let account = agent
        .extra
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let epoch = u64::try_from(agent.epoch).map_err(|_| ApiError::internal())?;
    let notification_sink = host_port.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires account-worker/vm-notification",
        )
    })?;
    let provider = super::vm_native_provider::VmNativeProvider::new(
        Arc::clone(&guest),
        notification_sink,
        account,
        agent.id.0.as_str(),
        epoch,
    );
    Ok((Arc::new(provider), Some(guest)))
}

#[cfg(unix)]
fn vm_guest_state_dir(
    runtime_state_dir: &std::path::Path,
    configured: Option<std::path::PathBuf>,
) -> std::path::PathBuf {
    configured.unwrap_or_else(|| runtime_state_dir.join("linux-vm"))
}

async fn native_action(
    State(state): State<HttpState>,
    lifecycle: Option<Extension<Arc<dyn LifecyclePorts>>>,
    preparation: Option<
        Extension<Arc<studio_runtime_services::account_worker::preparation::PreparationResources>>,
    >,
    repair: Option<Extension<Arc<dyn NativeContextRepairPort>>>,
    ApiJson(body): ApiJson<generated::NativeActionRequest>,
) -> Result<Json<Value>, ApiError> {
    let value = serde_json::to_value(body).map_err(|_| ApiError::internal())?;
    let Some(action) = value.get("action").and_then(Value::as_str) else {
        let port = state.account_session.clone().ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_IMPLEMENTED,
                "Not implemented: requires provider-session/native-request",
            )
        })?;
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        let account_driver = state.account_driver.clone();
        let lifecycle = lifecycle.map(|Extension(ports)| ports);
        let resources = preparation
            .clone()
            .map(|Extension(resources)| resources)
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::NOT_IMPLEMENTED,
                    "Not implemented: requires native-action/preparation-and-settings",
                )
            })?;
        let agent = value["id"].as_str().unwrap_or_default().to_owned();
        let data = value.get("action").cloned().unwrap_or(Value::Null);
        let safety_action = data["safety"]
            .as_str()
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Choose wait, retry or cancel"))?
            .to_owned();
        if !matches!(safety_action.as_str(), "wait" | "retry" | "cancel") {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "Choose wait, retry or cancel",
            ));
        }
        let turn_id = data["turnId"]
            .as_str()
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "A turn ID is required"))?
            .to_owned();
        let read_agent = agent.clone();
        let (account, epoch) = store
            .read_async("runtime-http/routes/agent_management", move |tx| {
                let record = AgentRepository::get(tx, &AgentId::from(read_agent.as_str()))?
                    .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Agent not found"))?;
                let account = record
                    .extra
                    .get("accountKey")
                    .and_then(Value::as_str)
                    .unwrap_or("default")
                    .to_owned();
                let epoch = record.value()["epoch"]
                    .as_i64()
                    .ok_or_else(ApiError::internal)?;
                Ok::<_, ApiError>((account, epoch))
            })
            .await?;
        let actor = studio_runtime_services::account_worker::retained::RequestActor {
            agent: agent.clone(),
            epoch,
        };
        let account_driver = account_driver.ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_IMPLEMENTED,
                "Not implemented: requires account-driver/provider-home",
            )
        })?;
        let accounts = account_driver
            .snapshot()
            .map_err(|_| ApiError::internal())?;
        let provider_home = accounts
            .accounts
            .iter()
            .find(|row| row["id"] == account)
            .and_then(|row| row["home"].as_str())
            .ok_or_else(ApiError::internal)?;
        let mut resources = resources.as_ref().clone();
        resources.account_home = provider_home.to_owned();
        let resources = Arc::new(resources);
        let result = studio_runtime_services::safety_buffering::execute_action(
            store,
            port.as_ref(),
            &resources,
            lifecycle,
            studio_runtime_services::safety_buffering::SafetyAction {
                agent_id: &agent,
                actor,
                turn_id: &turn_id,
                choice: &safety_action,
            },
        )
        .await
        .map_err(safety_error)?;
        return Ok(Json(result));
    };
    let action = match action {
        "compact" => native_actions::Action::Compact,
        "review" => native_actions::Action::Review,
        _ => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "Choose compact or review",
            ));
        }
    };
    let Some(request_id) = value.get("request_id").and_then(Value::as_str) else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Reload Studio before Review or Compact. This client has no durable action request ID.",
        ));
    };
    let context = value
        .get("context")
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let request = native_actions::Request {
        request_id: request_id.to_owned(),
        agent: value["id"].as_str().unwrap_or_default().to_owned(),
        action,
        context,
    };
    // Invalid local state must be rejected before checking whether the provider
    // request consumer is connected, matching Python's read-only preconditions.
    let preflight_store = state.store.clone().ok_or_else(ApiError::internal)?;
    let preflight_request = request.clone();
    let preflight_result = preflight_store
        .read_async("runtime-http/routes/agent_management", move |tx| {
            if let Some(replayed) =
                native_actions::replay_if_settled(tx, &preflight_request).map_err(native_error)?
            {
                return Ok::<_, ApiError>(Some(replayed));
            }
            let agent = AgentRepository::get(tx, &AgentId::from(preflight_request.agent.as_str()))?
                .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown managed agent"))?;
            let agent = agent.value();
            if agent
                .get("threadId")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "This agent has no active conversation",
                ));
            }
            if agent.get("autoWake") != Some(&Value::Bool(true))
                || agent.get("inFlight") == Some(&Value::Bool(true))
                || matches!(
                    agent.get("status").and_then(Value::as_str),
                    Some("queued" | "starting" | "running" | "approval")
                )
            {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "This agent is not ready for a native action",
                ));
            }
            Ok(None)
        })
        .await?;
    if let Some(replayed) = preflight_result {
        return Ok(Json(replayed));
    }
    let repair = repair.map(|Extension(port)| port);
    let resources = preparation
        .map(|Extension(resources)| resources)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_IMPLEMENTED,
                "Not implemented: requires native-action/preparation-and-settings",
            )
        })?;
    let host_port = state.account_session.clone();
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let execute_store = Arc::clone(&store);
    let execute_request = request.clone();
    let account_driver = state.account_driver.clone();
    let guest = state.browser_answer_guest.clone();
    let admission_override = state.message_admission.clone();
    let read_store = Arc::clone(&store);
    let read_agent = request.agent.clone();
    let use_prepared = admission_override.is_some();
    let (agent, prepared) = store
        .read_async("runtime-http/routes/agent_management", move |tx| {
            let agent = AgentRepository::get(tx, &AgentId::from(read_agent.as_str()))?
                .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Agent not found"))?;
            let prepared: studio_runtime_services::message_admission::AdmissionSnapshot =
                if use_prepared {
                    studio_runtime_services::message_admission::AdmissionSnapshot::Workspace(
                        studio_runtime_services::message_admission::PreparedAdmission::read(
                            tx,
                            &read_agent,
                        )
                        .map_err(|_| ApiError::internal())?,
                    )
                } else {
                    studio_runtime_services::message_admission::AdmissionSnapshot::Ingress(
                        studio_runtime_services::message_admission::MessageIngressAdmission::read(
                            read_store,
                            tx,
                            &read_agent,
                        )
                        .map_err(|_| ApiError::internal())?,
                    )
                };
            Ok::<_, ApiError>((agent, prepared))
        })
        .await?;
    let prepared = prepared.prepare().await.map_err(|_| ApiError::internal())?;
    let (provider, guest) = native_action_provider(&store, &agent, host_port, guest)?;
    let admission: Arc<dyn MessageAdmission> =
        admission_override.unwrap_or_else(|| Arc::from(prepared));
    let bridge: Arc<dyn Admission> =
        Arc::new(native_action_ports::MessageNativeAdmission::new(admission));
    let preparation_run = NativeActionPreparationRun {
        store: store.clone(),
        provider: provider.clone(),
        account_driver,
        repair,
        preparation: resources,
        guest,
    };
    let now = unix_time();
    let result = native_actions::execute_provider_prepared(
        &execute_store,
        provider.as_ref(),
        &execute_request,
        now,
        bridge,
        Arc::new(native_action_ports::NativeActionAcceptance),
        &preparation_run,
    )
    .await
    .map_err(native_error)?;
    Ok(Json(result))
}

pub trait NativeContextRepairPort: Send + Sync {
    fn repair_idle(
        &self,
        store: Arc<RuntimeStore>,
        agent_id: String,
        provider_home: std::path::PathBuf,
        broker: Arc<dyn studio_runtime_services::account_session_runtime::AccountSessionPort>,
        deadline: std::time::Instant,
    ) -> futures_util::future::BoxFuture<'static, native_actions::Result<Value>>;
}

#[derive(Clone)]
struct NativeActionPreparationRun {
    store: Arc<RuntimeStore>,
    provider: Arc<dyn studio_runtime_services::account_session_runtime::AccountSessionPort>,
    account_driver: Option<Arc<studio_runtime_services::account_driver::AccountDriver>>,
    repair: Option<Arc<dyn NativeContextRepairPort>>,
    preparation: Arc<studio_runtime_services::account_worker::preparation::PreparationResources>,
    guest: Option<Arc<dyn studio_runtime_services::linux_vm_guest::GuestRpc>>,
}
impl NativeActionPreparationRun {
    async fn agent(
        &self,
        request: &native_actions::Request,
    ) -> native_actions::Result<AgentRecord> {
        let agent_id = AgentId::from(request.agent.as_str());
        self.store
            .read_async(
                "runtime-http/routes/agent_management/preparation",
                move |tx| AgentRepository::get(tx, &agent_id)?.ok_or(NativeError::Changed),
            )
            .await
    }

    /// The provider home of a host agent, or `None` for a VM agent.
    fn host_provider_home(
        &self,
        agent: &AgentRecord,
    ) -> native_actions::Result<Option<std::path::PathBuf>> {
        if agent.extra.get("executionMode").and_then(Value::as_str) == Some("vm") {
            return Ok(None);
        }
        let account = agent
            .extra
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default");
        let account_driver = self
            .account_driver
            .as_ref()
            .ok_or(NativeError::Unavailable("account-driver/provider-home"))?;
        let accounts = account_driver
            .snapshot()
            .map_err(|_| NativeError::Unavailable("account-driver/provider-home"))?;
        accounts
            .accounts
            .iter()
            .find(|row| row["id"] == account)
            .and_then(|row| row["home"].as_str())
            .map(|home| Some(std::path::PathBuf::from(home)))
            .ok_or(NativeError::Unavailable("account-driver/provider-home"))
    }

    fn provider_home(&self, agent: &AgentRecord) -> native_actions::Result<std::path::PathBuf> {
        if let Some(home) = self.host_provider_home(agent)? {
            return Ok(home);
        }
        agent
            .extra
            .get("layrHome")
            .and_then(Value::as_str)
            .filter(|home| !home.is_empty())
            .map(std::path::PathBuf::from)
            .ok_or(NativeError::Unavailable("workspace-state/vm-home"))
    }

    async fn repair_before_reserve(
        &self,
        request: &native_actions::Request,
    ) -> native_actions::Result<()> {
        let agent = self.agent(request).await?;
        let Some(provider_home) = self.host_provider_home(&agent)? else {
            return Ok(());
        };
        self.repair
            .as_ref()
            .ok_or(NativeError::Unavailable(
                "context-repair/repair-idle-broker",
            ))?
            .repair_idle(
                self.store.clone(),
                request.agent.clone(),
                provider_home,
                self.provider.clone(),
                std::time::Instant::now() + std::time::Duration::from_secs(20),
            )
            .await?;
        Ok(())
    }

    async fn prepare_vm_workspace(
        &self,
        request: &native_actions::Request,
        ticket: &studio_runtime_services::workspace_operations::ProviderTicket,
    ) -> native_actions::Result<()> {
        let agent = self.agent(request).await?.value().clone();
        if agent.get("executionMode").and_then(Value::as_str) != Some("vm") {
            return Ok(());
        }
        let guest = self
            .guest
            .clone()
            .ok_or(NativeError::Unavailable("workspace-state/vm-workspace"))?;
        if agent.get("isLead").and_then(Value::as_bool) == Some(true) {
            studio_runtime_services::vm_preparation::prepare_lead(
                self.store.as_ref(),
                guest,
                &request.agent,
                &ticket.operation,
                &request.request_id,
                ticket.epoch,
            )
            .await
        } else {
            studio_runtime_services::vm_preparation::prepare_child(
                self.store.as_ref(),
                guest,
                &request.agent,
                &ticket.operation,
                &request.request_id,
                ticket.epoch,
            )
            .await
        }
    }
}
impl native_actions::Preparation for NativeActionPreparationRun {
    fn before_reserve<'a>(
        &'a self,
        _store: &'a RuntimeStore,
        _provider: &'a dyn studio_runtime_services::account_session_runtime::AccountSessionPort,
        request: &'a native_actions::Request,
        _scope: &'a studio_runtime_services::account_session_runtime::ConnectionScope,
    ) -> studio_runtime_services::native_actions::PreparationFuture<'a> {
        Box::pin(async move { self.repair_before_reserve(request).await })
    }

    fn prepare<'a>(
        &'a self,
        _store: &'a RuntimeStore,
        _provider: &'a dyn studio_runtime_services::account_session_runtime::AccountSessionPort,
        request: &'a native_actions::Request,
        ticket: &'a studio_runtime_services::workspace_operations::ProviderTicket,
        scope: &'a studio_runtime_services::account_session_runtime::ConnectionScope,
    ) -> studio_runtime_services::native_actions::PreparationFuture<'a> {
        Box::pin(async move {
            let preparation = self.clone();
            let request_for_prepare = request.clone();
            let ticket_for_prepare = ticket.clone();
            preparation
                .prepare_vm_workspace(&request_for_prepare, &ticket_for_prepare)
                .await?;
            let provider_home = preparation.provider_home(&preparation.agent(request).await?)?;
            let mut resources = preparation.preparation.as_ref().clone();
            resources.account_home = provider_home.to_string_lossy().into_owned();
            let prepared =
                studio_runtime_services::account_worker::preparation::prepare_native_action(
                    Arc::clone(&preparation.store),
                    &resources,
                    &request_for_prepare.agent,
                    &ticket_for_prepare.operation,
                    ticket_for_prepare.generation,
                )
                .await
                .map_err(|_| NativeError::Unavailable("thread-preparation/native-action"))?;
            if Some(prepared.thread_id.as_str()) != ticket.thread.as_deref() {
                return Err(NativeError::Changed);
            }
            let mut params = prepared.settings;
            if let Some(params) = params.as_object_mut() {
                params.remove("cwd");
                params.remove("cyberAccessProgram");
            }
            native_actions::update_action_settings(
                &self.store,
                self.provider.as_ref(),
                request,
                ticket,
                scope,
                params,
            )
            .await
        })
    }
}

fn native_error(error: NativeError) -> ApiError {
    match error {
        NativeError::Unavailable(_) => {
            ApiError::new(StatusCode::NOT_IMPLEMENTED, error.to_string())
        }
        NativeError::Conflict | NativeError::Invalid(_) | NativeError::Changed => {
            ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
        }
        NativeError::Unknown => ApiError::new(StatusCode::CONFLICT, error.to_string()),
        NativeError::InvalidStored { .. }
        | NativeError::Store(_)
        | NativeError::Json(_)
        | NativeError::Sql(_) => ApiError::internal(),
    }
}

fn safety_error(
    error: studio_runtime_services::safety_buffering::SafetyBufferingError,
) -> ApiError {
    use studio_runtime_services::safety_buffering::SafetyBufferingError as SafetyError;
    match error {
        SafetyError::Requires(name) => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            format!("Not implemented: requires {name}"),
        ),
        SafetyError::ProviderUnknown => ApiError::new(StatusCode::CONFLICT, error.to_string()),
        SafetyError::Persistence | SafetyError::InvalidStoredState => ApiError::internal(),
        _ => ApiError::new(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

fn unix_time() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}
async fn dispatch<T: serde::de::DeserializeOwned + serde::Serialize + Send + 'static>(
    state: HttpState,
    action: Action,
    ports: Option<Arc<dyn LifecyclePorts>>,
) -> Result<Json<T>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| ApiError::internal())?
        .as_secs_f64();
    let result = match ports {
        Some(ports) => {
            agent_lifecycle_runtime::execute(Arc::clone(&store), &action, now, ports).await
        }
        None => agent_management_store::execute(Arc::clone(&store), &action, now).await,
    }
    .map_err(|error| match error {
        ManagementError::Unavailable(_) => {
            ApiError::new(StatusCode::NOT_IMPLEMENTED, error.to_string())
        }
        ManagementError::Invalid(_) => ApiError::new(StatusCode::BAD_REQUEST, error.to_string()),
        ManagementError::Stored { table, id } => ApiError::invalid_stored(&table, &id),
        ManagementError::Store(error) => ApiError::stored(error),
    })?;
    super::generated_json::<T>(result).map(Json)
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "agent_management", routes: Some(routes), dependencies: &[
            "account-worker/agent-interrupt", "native-safety/retry-driver",
            "agent-management/snapshot-limit", "cross-server/agent-management",
            "event-driver/child-stop-notification", "monitor-driver/agent-stop",
            "account-worker/deleted-settlement", "usage-policy/agent-stop", "voice-driver/agent-stop", "work-driver/deletion-release",
            "workspace-state/agent-lifecycle", "cross-server/native-review",
        ],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod native_review_preflight_tests {
    use super::*;
    use base64::Engine;
    use serde_json::json;
    use std::{
        fs::File,
        io::Read,
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use studio_runtime_services::vm_preparation;
    use studio_runtime_services::{
        account_session_runtime::{
            AccountSessionError, AccountSessionPort, ConnectionScope, ProviderRequest,
            SessionReceipt,
        },
        account_worker::preparation::PreparationResources,
        model_catalog::ModelCatalogService,
    };
    struct ProviderSpy(AtomicUsize);
    impl AccountSessionPort for ProviderSpy {
        fn provider_request<'a>(
            &'a self,
            _request: ProviderRequest,
        ) -> futures_util::future::BoxFuture<'a, Result<SessionReceipt, AccountSessionError>>
        {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(AccountSessionError::Requires("test/provider"))
            })
        }
    }

    struct RepairSpy(Arc<AtomicUsize>);
    impl NativeContextRepairPort for RepairSpy {
        fn repair_idle(
            &self,
            _store: Arc<RuntimeStore>,
            _agent_id: String,
            _provider_home: std::path::PathBuf,
            _broker: Arc<dyn AccountSessionPort>,
            _deadline: std::time::Instant,
        ) -> futures_util::future::BoxFuture<'static, native_actions::Result<Value>> {
            let calls = Arc::clone(&self.0);
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Value::Null)
            })
        }
    }

    #[tokio::test]
    async fn remote_origin_and_vm_review_preflight_use_the_provider_home_owner() {
        let directory = tempfile::tempdir().expect("isolated store directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open store"),
        );
        let original = AgentRecord::from_value(json!({
            "id":"remote",
            "rootId":"remote",
            "epoch":7,
            "status":"running",
            "provider":"codex",
            "remoteOrigin":true,
            "threadId":"thread-1",
            "accountKey":"default",
            "startAttempt":{"submitted":true}
        }))
        .expect("valid agent record");
        let original_for_write = original.clone();
        store
            .transaction(move |tx| AgentRepository::put(tx, &original_for_write))
            .await
            .expect("save agent");
        let before_agent = {
            let connection = store.test_reader().expect("read store");
            AgentRepository::get(&connection, &original.id)
                .expect("read agent")
                .expect("agent exists")
                .value()
        };
        let before_receipts: i64 = {
            let connection = store.test_reader().expect("read store");
            connection
                .query_row(
                    "SELECT COUNT(*) FROM runtime_native_action_receipts",
                    [],
                    |row| row.get(0),
                )
                .expect("count receipts")
        };

        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../..")
            .canonicalize()
            .expect("repository root");
        let resources = PreparationResources::bundled_at(
            String::new(),
            ModelCatalogService::new(),
            root.clone(),
            root.join("workspaces/runtime/apps/server/src"),
        )
        .expect("preparation resources");
        let provider = Arc::new(ProviderSpy(AtomicUsize::new(0)));
        let repair = Arc::new(RepairSpy(Arc::new(AtomicUsize::new(0))));
        let preparation = NativeActionPreparationRun {
            store: store.clone(),
            provider: provider.clone(),
            account_driver: None,
            repair: Some(repair.clone()),
            preparation: Arc::new(resources),
            guest: Some(Arc::new(GuestSpy::new(json!({})))),
        };
        let request = native_actions::Request {
            request_id: "review-request".into(),
            agent: "remote".into(),
            action: native_actions::Action::Review,
            context: Value::Null,
        };
        let scope = ConnectionScope {
            account: "default".into(),
            connection: "connection-1".into(),
            generation: 3,
        };
        let result = native_actions::Preparation::before_reserve(
            &preparation,
            store.as_ref(),
            provider.as_ref(),
            &request,
            &scope,
        )
        .await;

        assert!(matches!(
            result,
            Err(NativeError::Unavailable("account-driver/provider-home"))
        ));
        assert_eq!(repair.0.load(Ordering::SeqCst), 0);
        assert_eq!(provider.0.load(Ordering::SeqCst), 0);
        let mut vm_value = original.value();
        vm_value["executionMode"] = json!("vm");
        vm_value["layrHome"] = json!("/guest/provider-home");
        let vm_agent = AgentRecord::from_value(vm_value).expect("valid VM agent record");
        let vm_agent_for_write = vm_agent.clone();
        store
            .transaction(move |tx| AgentRepository::put(tx, &vm_agent_for_write))
            .await
            .expect("save VM agent");
        let vm_result = native_actions::Preparation::before_reserve(
            &preparation,
            store.as_ref(),
            provider.as_ref(),
            &request,
            &scope,
        )
        .await;
        assert!(vm_result.is_ok());
        assert_eq!(
            preparation
                .provider_home(&preparation.agent(&request).await.expect("stored VM agent"))
                .expect("VM provider home comes from layr")
                .as_path(),
            Path::new("/guest/provider-home")
        );
        assert_eq!(repair.0.load(Ordering::SeqCst), 0);
        assert_eq!(provider.0.load(Ordering::SeqCst), 0);
        let connection = store.test_reader().expect("read unchanged store");
        let after_agent = AgentRepository::get(&connection, &original.id)
            .expect("read unchanged agent")
            .expect("agent remains")
            .value();
        let after_receipts: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_native_action_receipts",
                [],
                |row| row.get(0),
            )
            .expect("count unchanged receipts");
        assert_eq!(before_agent["remoteOrigin"], true);
        assert_eq!(after_agent, vm_agent.value());
        assert_eq!(after_receipts, before_receipts);
    }

    struct GuestSpy {
        calls: std::sync::Mutex<Vec<(String, Value, Option<String>)>>,
        result: Value,
        uncertain: bool,
        intent_store: Option<Arc<RuntimeStore>>,
    }

    struct NotificationSink;

    impl studio_runtime_services::account_session_runtime::AccountSessionPort for NotificationSink {
        fn deliver_vm_notification<'a>(
            &'a self,
            _account: &'a str,
            _agent_id: &'a str,
            _epoch: u64,
            _sequence: u64,
            _frame: studio_runtime_providers::Frame,
        ) -> futures_util::future::BoxFuture<
            'a,
            Result<(), studio_runtime_services::account_session_runtime::AccountSessionError>,
        > {
            Box::pin(async { Ok(()) })
        }
    }

    impl GuestSpy {
        fn new(result: Value) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                result,
                uncertain: false,
                intent_store: None,
            }
        }

        fn uncertain(result: Value) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                result,
                uncertain: true,
                intent_store: None,
            }
        }

        fn with_intent_store(mut self, store: Arc<RuntimeStore>) -> Self {
            self.intent_store = Some(store);
            self
        }
    }

    impl studio_runtime_services::linux_vm_guest::GuestRpc for GuestSpy {
        fn stream(
            &self,
            method: &str,
            params: &Value,
            request_id: Option<&str>,
            _timeout: std::time::Duration,
            _on_frame: &mut dyn FnMut(
                Value,
            ) -> std::result::Result<
                (),
                studio_runtime_services::linux_vm_guest::GuestError,
            >,
        ) -> std::result::Result<Value, studio_runtime_services::linux_vm_guest::GuestError>
        {
            self.calls.lock().expect("guest call lock").push((
                method.to_owned(),
                params.clone(),
                request_id.map(str::to_owned),
            ));
            if method == "upload.begin"
                && let Some(store) = &self.intent_store
            {
                let connection = store.test_reader().expect("read durable import intent");
                let agent = AgentRepository::get(&connection, &AgentId::from("vm-lead"))
                    .expect("read VM lead")
                    .expect("VM lead exists")
                    .value();
                let intent = &agent["startAttempt"]["vmWorkspacePreparation"]["projectImport"];
                assert_eq!(intent["phase"], "uploading");
                assert!(intent["projectId"].as_str().is_some());
                assert!(intent["requestId"].as_str().is_some());
                assert!(intent["source"].as_str().is_some());
            }
            if self.uncertain && method == "line.bind" {
                return Err(
                    studio_runtime_services::linux_vm_guest::GuestError::Transport {
                        message: "fixture lost the response".into(),
                        uncertain: true,
                    },
                );
            }
            if method == "project.ensure" {
                return Err(
                    studio_runtime_services::linux_vm_guest::GuestError::Rejected {
                        code: Some("not_found".into()),
                        detail: None,
                        uncertain: false,
                    },
                );
            }
            if method == "project.import" {
                return Ok(
                    serde_json::json!({"projectId":params["projectId"],"stateId":"imported-state"}),
                );
            }
            if method == "provider.rpc" {
                return Ok(match params["action"].as_str() {
                    Some("info") => json!({"lastSeq":0,"acknowledged":0,"state":"running"}),
                    Some("write") => json!({"remoteId":7,"accepted":true}),
                    Some("next") => json!({
                        "event":{"sequence":1,"kind":"stdout","payload":"{\"id\":7,\"result\":{\"ok\":true}}"},
                        "returnCode":null
                    }),
                    Some("ack") => json!({"acknowledged":params["sequence"]}),
                    _ => self.result.clone(),
                });
            }
            Ok(self.result.clone())
        }
    }

    #[tokio::test]
    async fn vm_native_action_selects_fake_guest_with_retained_notifications() {
        use std::time::{Duration, Instant};
        use studio_runtime_providers::Method;
        use studio_runtime_services::account_session_runtime::RequestCertainty;

        let directory = tempfile::tempdir().expect("isolated store directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open store");
        let agent = AgentRecord::from_value(json!({
            "id":"vm-native", "rootId":"vm-native", "epoch":5,
            "status":"idle", "executionMode":"vm", "accountKey":"vm-account"
        }))
        .expect("VM agent record");
        let guest = Arc::new(GuestSpy::new(json!({"ok":true})));
        let guest_port: Arc<dyn studio_runtime_services::linux_vm_guest::GuestRpc> = guest.clone();
        let (provider, selected_guest) = native_action_provider(
            &store,
            &agent,
            Some(Arc::new(NotificationSink)),
            Some(guest_port),
        )
        .expect("VM provider selection uses retained notification ingress");
        let scope = provider.current_scope("vm-account").expect("guest scope");
        let receipt = provider
            .provider_request(ProviderRequest {
                account: "vm-account".into(),
                operation: "vm-native-op".into(),
                actor: None,
                expected_scope: Some(scope.clone()),
                deleted_actor_session: None,
                method: Method::ThreadCompactStart,
                params: json!({"threadId":"guest-thread"}),
                deadline: Instant::now() + Duration::from_secs(3),
            })
            .await
            .expect("guest request receipt");
        assert!(selected_guest.is_some());
        assert_eq!(receipt.scope, scope);
        assert_eq!(receipt.certainty, RequestCertainty::Written);
        assert_eq!(
            guest.calls.lock().expect("guest calls")[0].0,
            "provider.rpc"
        );
    }

    #[cfg(unix)]
    #[test]
    fn vm_guest_default_path_appends_linux_vm_once() {
        let root = tempfile::tempdir().expect("isolated state root");
        assert_eq!(
            vm_guest_state_dir(root.path(), None),
            root.path().join("linux-vm")
        );
        assert_eq!(
            vm_guest_state_dir(root.path(), Some(root.path().join("custom-vm"))),
            root.path().join("custom-vm")
        );
    }

    #[test]
    fn vm_archive_matches_code_only_exclusions_and_refuses_worktree_pointer() {
        let temporary = tempfile::tempdir().expect("source tempdir");
        let root = temporary.path();
        std::fs::write(root.join(".gitignore"), "data/\nignored-dir/\n").unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::create_dir_all(root.join("data/downloads")).unwrap();
        std::fs::create_dir_all(root.join("ignored-dir")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::write(root.join("data/downloads/archive.zip"), "data").unwrap();
        std::fs::write(root.join("ignored-dir/cache"), "ignored").unwrap();
        std::fs::write(root.join("target/debug/output"), "build").unwrap();
        std::fs::write(root.join("source.rs"), "fn main() {}\n").unwrap();
        let archive = vm_preparation::vm_project_archive(root).expect("archive source");
        let mut tar = Vec::new();
        flate2::read::GzDecoder::new(File::open(archive.file.path()).unwrap())
            .read_to_end(&mut tar)
            .unwrap();
        assert!(
            tar.windows(b"source.rs".len())
                .any(|window| window == b"source.rs")
        );
        for excluded in [
            b"data/downloads/archive.zip".as_slice(),
            b"ignored-dir/cache",
            b"target/debug/output",
        ] {
            assert!(!tar.windows(excluded.len()).any(|window| window == excluded));
        }

        std::fs::remove_file(root.join(".gitignore")).unwrap();
        std::fs::write(
            root.join(".git"),
            "gitdir: /outside/repository/.git/worktrees/source\n",
        )
        .unwrap();
        assert!(
            vm_preparation::vm_project_archive(root).is_err(),
            "linked worktree metadata is not self-contained"
        );
    }

    #[tokio::test]
    async fn local_archive_failure_does_not_persist_submitted_intent() {
        let directory = tempfile::tempdir().expect("isolated store directory");
        let missing_source = directory.path().join("missing-project");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open store");
        let record = AgentRecord::from_value(json!({
            "id":"vm-local-archive-failure", "rootId":"vm-local-archive-failure",
            "epoch":1, "status":"starting", "provider":"codex", "executionMode":"vm",
            "isLead":true, "autoWake":true, "cwd":missing_source,
            "startAttempt":{"id":"attempt-local-archive-failure"}
        }))
        .expect("valid VM agent");
        let record_id = record.id.clone();
        store
            .transaction_with(move |tx| AgentRepository::put(tx, &record))
            .await
            .expect("save VM agent");
        let guest = Arc::new(GuestSpy::new(Value::Null));
        let guest_port: Arc<dyn studio_runtime_services::linux_vm_guest::GuestRpc> = guest.clone();

        assert!(matches!(
            vm_preparation::prepare_lead(
                &store,
                guest_port,
                "vm-local-archive-failure",
                "attempt-local-archive-failure",
                "vm-turn-preparation:attempt-local-archive-failure",
                1,
            )
            .await,
            Err(NativeError::Unavailable(_))
        ));
        let connection = store.test_reader().expect("read store");
        let saved = AgentRepository::get(&connection, &record_id)
            .expect("read agent")
            .expect("agent exists")
            .value();
        assert!(
            saved["startAttempt"]
                .get("vmWorkspacePreparation")
                .is_none()
        );
        assert_eq!(
            guest
                .calls
                .lock()
                .expect("guest calls")
                .iter()
                .map(|call| call.0.as_str())
                .collect::<Vec<_>>(),
            ["project.ensure"]
        );
    }

    #[tokio::test]
    async fn vm_lead_workspace_binding_is_durably_recorded_with_python_identity() {
        let directory = tempfile::tempdir().expect("isolated store directory");
        let source_directory = tempfile::tempdir().expect("isolated host project directory");
        let source = source_directory.path().join("project");
        std::fs::create_dir_all(&source).expect("create source tree");
        std::fs::write(source.join("README.md"), "hello from the host\n")
            .expect("write project source");
        let git_object = source.join(".git/objects/aa/fixture-object");
        std::fs::create_dir_all(git_object.parent().unwrap()).expect("create Git object store");
        std::fs::write(&git_object, "fixture git object bytes").expect("write Git object fixture");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open store"),
        );
        let record = AgentRecord::from_value(json!({
            "id":"vm-lead", "rootId":"vm-lead", "epoch":7, "status":"starting",
            "provider":"codex", "executionMode":"vm", "isLead":true, "autoWake":true,
            "cwd":source, "hostProjectPath":source, "projectId":"logical-project-id", "threadId":"thread-vm",
            "accountKey":"default", "startAttempt":{"id":"native-attempt","actionRequestId":"vm-review"}
        })).expect("valid VM agent");
        let record_for_write = record.clone();
        store
            .transaction(move |tx| AgentRepository::put(tx, &record_for_write))
            .await
            .expect("save VM agent");
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../..")
            .canonicalize()
            .unwrap();
        let resources = PreparationResources::bundled_at(
            String::new(),
            ModelCatalogService::new(),
            root.clone(),
            root.join("workspaces/runtime/apps/server/src"),
        )
        .expect("preparation resources");
        let guest = Arc::new(
            GuestSpy::new(json!({
                "cwd":"/home/studio/project","projectId":"project-key","line":"main",
                "owner":"vm-lead","home":"/home/studio","stateId":"state-1"
            }))
            .with_intent_store(store.clone()),
        );
        let preparation = NativeActionPreparationRun {
            store: store.clone(),
            provider: Arc::new(ProviderSpy(AtomicUsize::new(0))),
            account_driver: None,
            repair: Some(Arc::new(RepairSpy(Arc::new(AtomicUsize::new(0))))),
            preparation: Arc::new(resources),
            guest: Some(guest.clone()),
        };
        let request = native_actions::Request {
            request_id: "vm-review".into(),
            agent: "vm-lead".into(),
            action: native_actions::Action::Review,
            context: Value::Null,
        };
        let ticket = studio_runtime_services::workspace_operations::ProviderTicket {
            operation: "native-attempt".into(),
            agent: "vm-lead".into(),
            epoch: 7,
            account: "default".into(),
            thread: Some("thread-vm".into()),
            connection: "connection-vm".into(),
            generation: 3,
            request: "vm-review".into(),
        };
        preparation
            .prepare_vm_workspace(&request, &ticket)
            .await
            .expect("bind layr line");
        let calls = guest.calls.lock().expect("guest calls");
        assert_eq!(calls.len(), 8);
        assert_eq!(calls[0].0, "project.ensure");
        assert_eq!(calls[1].0, "upload.begin");
        assert_eq!(calls[1].1["mode"], "full");
        assert_eq!(calls[2].0, "upload.chunk");
        let compressed = base64::engine::general_purpose::STANDARD
            .decode(calls[2].1["data"].as_str().expect("archive chunk"))
            .expect("base64 archive chunk");
        let mut tar = Vec::new();
        flate2::read::GzDecoder::new(compressed.as_slice())
            .read_to_end(&mut tar)
            .expect("decompress project archive");
        assert!(
            tar.windows(b"hello from the host".len())
                .any(|window| { window == b"hello from the host" })
        );
        assert!(
            tar.windows(b"fixture git object bytes".len())
                .any(|window| { window == b"fixture git object bytes" }),
            "full imports include Git objects as Python does"
        );
        assert_eq!(calls[3].0, "upload.commit");
        assert_eq!(calls[4].0, "project.import");
        assert_eq!(calls[4].1["incremental"], false);
        assert_eq!(calls[5].0, "line.bind");
        assert_eq!(
            calls[5].2.as_deref(),
            Some(vm_preparation::vm_guest_request_id("layr-bind", "vm-lead").as_str())
        );
        assert_eq!(calls[5].1["line"], "main");
        assert_eq!(calls[6].0, "health");
        assert_eq!(calls[7].0, "agent.context");
        assert_eq!(
            calls[7].2.as_deref(),
            Some(vm_preparation::vm_guest_request_id("layr-context", "vm-lead").as_str())
        );
        drop(calls);
        let connection = store.test_reader().expect("read store");
        let saved = AgentRepository::get(&connection, &record.id)
            .expect("read VM agent")
            .expect("agent exists")
            .value();
        assert_eq!(saved["layrReady"], true);
        assert_eq!(saved["cwd"], "/home/studio/project");
        assert_eq!(saved["layrProjectId"], "project-key");
        assert_eq!(
            saved["startAttempt"]["vmWorkspacePreparation"]["phase"],
            "completed"
        );
        assert_eq!(
            saved["startAttempt"]["vmWorkspacePreparation"]["projectImport"]["phase"],
            "imported"
        );
    }

    #[tokio::test]
    async fn vm_reviewer_branches_from_ready_parent_as_read_only() {
        let directory = tempfile::tempdir().expect("isolated store directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open store"),
        );
        let parent = AgentRecord::from_value(json!({
            "id":"vm-parent", "rootId":"vm-parent", "epoch":2, "status":"idle",
            "provider":"codex", "executionMode":"vm", "isLead":true, "autoWake":true,
            "cwd":"/home/studio/project", "layrReady":true,
            "layrProjectId":"project-key", "layrLine":"main"
        }))
        .expect("valid parent");
        let reviewer = AgentRecord::from_value(json!({
            "id":"vm-reviewer", "rootId":"vm-parent", "parentId":"vm-parent",
            "epoch":4, "status":"starting", "provider":"codex", "executionMode":"vm",
            "isLead":false, "role":"reviewer", "autoWake":true, "layrReady":false,
            "layrSubpath":"review", "layrBaseState":"base-1", "cwd":"/host/project",
            "threadId":"thread-review",
            "startAttempt":{"id":"review-attempt","actionRequestId":"review-req"}
        }))
        .expect("valid reviewer");
        store
            .transaction(move |tx| {
                AgentRepository::put(tx, &parent)?;
                AgentRepository::put(tx, &reviewer)
            })
            .await
            .expect("save VM lines");
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../..")
            .canonicalize()
            .unwrap();
        let resources = PreparationResources::bundled_at(
            String::new(),
            ModelCatalogService::new(),
            root.clone(),
            root.join("workspaces/runtime/apps/server/src"),
        )
        .expect("preparation resources");
        let guest = Arc::new(GuestSpy::new(json!({
            "cwd":"/home/studio/project/review","projectId":"project-key","line":"reviewer-vm-reviewer",
            "owner":"vm-reviewer","home":"/home/studio","stateId":"state-review"
        })));
        let preparation = NativeActionPreparationRun {
            store: store.clone(),
            provider: Arc::new(ProviderSpy(AtomicUsize::new(0))),
            account_driver: None,
            repair: Some(Arc::new(RepairSpy(Arc::new(AtomicUsize::new(0))))),
            preparation: Arc::new(resources),
            guest: Some(guest.clone()),
        };
        let request = native_actions::Request {
            request_id: "review-req".into(),
            agent: "vm-reviewer".into(),
            action: native_actions::Action::Review,
            context: Value::Null,
        };
        let ticket = studio_runtime_services::workspace_operations::ProviderTicket {
            operation: "review-attempt".into(),
            agent: "vm-reviewer".into(),
            epoch: 4,
            account: "default".into(),
            thread: Some("thread-review".into()),
            connection: "connection-vm".into(),
            generation: 3,
            request: "review-req".into(),
        };
        preparation
            .prepare_vm_workspace(&request, &ticket)
            .await
            .expect("branch layr reviewer");
        let calls = guest.calls.lock().expect("guest calls");
        assert_eq!(calls[0].0, "line.branch");
        assert_eq!(calls[0].1["parentAgentId"], "vm-parent");
        assert_eq!(calls[0].1["readOnly"], true);
        assert_eq!(calls[0].1["stateId"], "base-1");
        assert_eq!(
            calls[0].2.as_deref(),
            Some(vm_preparation::vm_guest_request_id("layr-branch", "vm-reviewer").as_str())
        );
    }

    #[test]
    fn vm_guest_request_ids_are_stable_and_bounded() {
        let identity = "agent".repeat(10_000);
        let first = vm_preparation::vm_guest_request_id("layr-bind", &identity);
        assert_eq!(
            first,
            vm_preparation::vm_guest_request_id("layr-bind", &identity)
        );
        assert!(
            first.chars().count()
                <= studio_runtime_services::linux_vm_guest::GUEST_REQUEST_ID_MAX_CHARS
        );
        assert!(first.starts_with("layr-bind:"));
    }

    #[tokio::test]
    async fn uncertain_vm_binding_is_durable_and_never_resent() {
        let directory = tempfile::tempdir().expect("isolated store directory");
        let source_directory = tempfile::tempdir().expect("isolated host project directory");
        let source = source_directory.path().join("project");
        std::fs::create_dir_all(&source).expect("create source tree");
        std::fs::write(source.join("main.py"), "print('hello')\n").expect("write project source");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open store"),
        );
        let record = AgentRecord::from_value(json!({
            "id":"vm-unknown", "rootId":"vm-unknown", "epoch":1, "status":"starting",
            "provider":"codex", "executionMode":"vm", "isLead":true, "autoWake":true,
            "cwd":source, "hostProjectPath":source, "threadId":"thread-unknown",
            "startAttempt":{"id":"attempt-unknown"}
        }))
        .expect("valid VM agent");
        let record_for_write = record.clone();
        store
            .transaction(move |tx| AgentRepository::put(tx, &record_for_write))
            .await
            .expect("save VM agent");
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../..")
            .canonicalize()
            .unwrap();
        let resources = PreparationResources::bundled_at(
            String::new(),
            ModelCatalogService::new(),
            root.clone(),
            root.join("workspaces/runtime/apps/server/src"),
        )
        .expect("preparation resources");
        let guest = Arc::new(GuestSpy::uncertain(json!({})));
        let preparation = NativeActionPreparationRun {
            store: store.clone(),
            provider: Arc::new(ProviderSpy(AtomicUsize::new(0))),
            account_driver: None,
            repair: Some(Arc::new(RepairSpy(Arc::new(AtomicUsize::new(0))))),
            preparation: Arc::new(resources),
            guest: Some(guest.clone()),
        };
        let request = native_actions::Request {
            request_id: "request-unknown".into(),
            agent: "vm-unknown".into(),
            action: native_actions::Action::Review,
            context: Value::Null,
        };
        let ticket = studio_runtime_services::workspace_operations::ProviderTicket {
            operation: "attempt-unknown".into(),
            agent: "vm-unknown".into(),
            epoch: 1,
            account: "default".into(),
            thread: Some("thread-unknown".into()),
            connection: "connection-vm".into(),
            generation: 1,
            request: "request-unknown".into(),
        };
        assert!(matches!(
            preparation.prepare_vm_workspace(&request, &ticket).await,
            Err(NativeError::Unknown)
        ));
        assert!(matches!(
            preparation.prepare_vm_workspace(&request, &ticket).await,
            Err(NativeError::Unknown)
        ));
        let calls = guest.calls.lock().expect("guest calls");
        assert_eq!(
            calls
                .iter()
                .filter(|(method, _, _)| method == "line.bind")
                .count(),
            1
        );
        assert_eq!(calls.len(), 6);
        drop(calls);
        let connection = store.test_reader().expect("read store");
        let saved = AgentRepository::get(&connection, &record.id)
            .expect("read agent")
            .unwrap()
            .value();
        assert_eq!(
            saved["startAttempt"]["vmWorkspacePreparation"]["phase"],
            "unknown"
        );
    }
}
