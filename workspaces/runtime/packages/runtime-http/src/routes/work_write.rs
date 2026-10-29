//! HTTP adapter for work mutations.

use super::extractors::ApiJson;
use super::manifest::{OperationStatus, RouteGroup, post};
use super::work_items;
use axum::extract::State;
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde_json::Value;
use studio_runtime_services::work_write::{
    self, AcceptWork, SubmitWork, WorkWrite, WorkWriteError,
};

use crate::error::ApiError;
use crate::router::HttpState;
use std::sync::Arc;
use studio_runtime_services::monitor_runtime_wiring::MonitorRuntime;
use studio_runtime_store::{JsonRecord, WorkItemRecord};

enum WorkCommitError {
    Store,
    Write(WorkWriteError),
    RefusedClaim(studio_runtime_store::StoreError),
    Rules(studio_runtime_services::rules_tick::RulesTickError),
}

impl From<studio_runtime_store::StoreError> for WorkCommitError {
    fn from(_error: studio_runtime_store::StoreError) -> Self {
        Self::Store
    }
}
impl From<WorkWriteError> for WorkCommitError {
    fn from(error: WorkWriteError) -> Self {
        Self::Write(error)
    }
}
impl From<studio_runtime_services::rules_tick::RulesTickError> for WorkCommitError {
    fn from(error: studio_runtime_services::rules_tick::RulesTickError) -> Self {
        Self::Rules(error)
    }
}

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![post(
        "/api/work",
        OperationStatus::Implemented,
        work_action,
    )])
}

pub(crate) async fn work_action(
    State(state): State<HttpState>,
    runtime: Option<Extension<Arc<MonitorRuntime>>>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    perform(state, runtime.map(|Extension(runtime)| runtime), body, None)
        .await
        .map(Json)
}

/// The work-action owner for agent tools: Python `model_work` hands claim,
/// submit and accept to the same `work_action` as `POST /api/work`.
pub fn managed_work_port(
    state: HttpState,
    runtime: Option<Arc<MonitorRuntime>>,
) -> Arc<dyn studio_runtime_services::provider_callbacks::WorkActionPort> {
    Arc::new(ManagedWorkPort { state, runtime })
}

struct ManagedWorkPort {
    state: HttpState,
    runtime: Option<Arc<MonitorRuntime>>,
}

impl studio_runtime_services::provider_callbacks::WorkActionPort for ManagedWorkPort {
    fn act<'a>(
        &'a self,
        actor_id: &'a str,
        args: &'a Value,
        request_id: &'a str,
    ) -> futures_util::future::BoxFuture<
        'a,
        studio_runtime_services::provider_callbacks::WorkActionOutcome,
    > {
        use studio_runtime_services::provider_callbacks::WorkActionOutcome;
        Box::pin(async move {
            let mut body = args.clone();
            let Some(object) = body.as_object_mut() else {
                return WorkActionOutcome::Refused("Invalid task arguments".to_owned());
            };
            object.insert("agent".to_owned(), Value::String(actor_id.to_owned()));
            object.insert("id".to_owned(), Value::String(request_id.to_owned()));
            match perform(
                self.state.clone(),
                self.runtime.clone(),
                body,
                Some(actor_id.to_owned()),
            )
            .await
            {
                Ok(record) => WorkActionOutcome::Written(record),
                // Client errors mean no task mutation. Schema refusal receipts and
                // counters may already be committed; replay uses the same request id.
                // Other errors may follow a task commit and remain uncertain.
                Err(error)
                    if error.status().is_client_error()
                        || matches!(
                            error.outcome(),
                            Some(crate::generated::ErrorResponseOutcome::NotApplied)
                        ) =>
                {
                    WorkActionOutcome::Refused(error.message().to_owned())
                }
                Err(error) => WorkActionOutcome::Unknown(format!(
                    "The task action outcome is unknown: {}. Check the task with orchestration_task get before repeating it",
                    error.message()
                )),
            }
        })
    }
}

pub(crate) async fn perform(
    state: HttpState,
    runtime: Option<Arc<MonitorRuntime>>,
    body: Value,
    actor_id: Option<String>,
) -> Result<Value, ApiError> {
    let agent = body
        .get("agent")
        .and_then(Value::as_str)
        .unwrap_or(&state.workspace_id)
        .to_owned();
    let action = body.get("action").and_then(Value::as_str).unwrap_or("list");
    if action == "list" {
        return work_items::work_for_agent(state, agent).await;
    }
    #[cfg(not(unix))]
    if action == "submit" {
        return Err(ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires workspace-state/submission-files",
        ));
    }
    let request_id = body
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    let signature = work_items::work_request_signature(&agent, actor_id.as_deref(), &body)?;
    use studio_runtime_schema::task_command::TaskCommand;
    let command = if actor_id.is_some() {
        TaskCommand::parse_tool(&body)
    } else {
        TaskCommand::parse(&body)
    }
    .map_err(|message| ApiError::new(StatusCode::BAD_REQUEST, message))?;
    if let studio_runtime_schema::task_command::TaskCommand::SetDefaultGate { gate } = command {
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        return store
            .transaction_with(move |tx| {
                studio_runtime_services::task_gate::set_default(
                    tx,
                    &agent,
                    gate,
                    request_id.as_deref(),
                    &signature,
                )
            })
            .await
            .map_err(|error| write_error(WorkWriteError::Event(error)));
    }
    let mut command =
        work_write::task_command(command, agent.clone(), state.state_dir.clone().into())
            .map_err(|error| write_error(WorkWriteError::Event(error)))?;
    if action == "accept" {
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        let receipt = work_write::read_work_receipt(
            &store,
            agent.clone(),
            request_id.clone(),
            signature.clone(),
        )
        .await
        .map_err(write_error)?;
        if let Some(receipt) = receipt {
            return Ok(receipt);
        }
    }
    if let WorkWrite::Accept(input) = &command
        && studio_runtime_services::event_write::python_strip(&input.reason).is_empty()
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Field must not be empty",
        ));
    }
    #[cfg(unix)]
    if let WorkWrite::Submit(input) = &mut command {
        prepare_vm_submission(&state, &agent, request_id.as_deref(), &signature, input).await?;
    }
    #[cfg(unix)]
    if let WorkWrite::Accept(input) = &mut command {
        prepare_vm_accept(&state, &agent, request_id.as_deref(), &signature, input).await?;
    }
    if let WorkWrite::Submit(input) = &mut command {
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        let task_id = input.task_id.clone();
        let actor_id = agent.clone();
        let owner = store
            .read_async("task-gate/prepare", move |tx| {
                let actor = studio_runtime_services::event_write::checked_agent(tx, &actor_id)?;
                let task = studio_runtime_store::WorkItemsRepository::work_record(tx, &task_id)?
                    .ok_or(
                        studio_runtime_services::event_write::EventWriteError::InvalidRequest(
                            "Unknown work item",
                        ),
                    )?;
                if task.value()["rootId"] != actor.root_id.0 {
                    return Err(
                        studio_runtime_services::event_write::EventWriteError::InvalidRequest(
                            "Unknown task in this team",
                        ),
                    );
                }
                if task.get("gate").is_none_or(Value::is_null) {
                    return Ok(None);
                }
                let owner = task.owner().unwrap_or(&actor_id);
                studio_runtime_services::event_write::checked_agent(tx, owner).map(Some)
            })
            .await
            .map_err(|e| write_error(WorkWriteError::Event(e)))?;
        if let Some(owner) = owner {
            // Monitor output validates paths against the store's resolved state identity.
            input.state_dir = store.root().to_owned();
            input.prepared_gate_workspace = super::agents_create::prepare_parent_workspace(&owner)
                .ok()
                .map(Arc::from);
        }
    }
    commit(
        state, runtime, agent, actor_id, request_id, signature, command,
    )
    .await
}

#[cfg(unix)]
async fn prepare_vm_accept(
    state: &HttpState,
    agent: &str,
    request_id: Option<&str>,
    signature: &str,
    input: &mut AcceptWork,
) -> Result<(), ApiError> {
    use std::time::Duration;
    use studio_runtime_services::linux_vm_guest::{GuestRpc, UnixGuestRpc};
    use studio_runtime_services::work_write::{
        VmAcceptPreparation, finish_vm_accept, prepare_vm_accept as prepare,
    };

    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let preparation = prepare(&store, agent, request_id, signature, input)
        .await
        .map_err(write_error)?;
    match preparation {
        VmAcceptPreparation::NotVm => {}
        VmAcceptPreparation::Merged(result_id) => input.merged_result_id = Some(result_id),
        VmAcceptPreparation::Call(call) => {
            let params = serde_json::json!({
                "agentId": call.lead_id,
                "sourceAgentId": call.owner_id,
                "expectedStateId": call.revision,
            });
            let operation_id = call.request_id.clone();
            let task_id = input.task_id.clone();
            let runtime_state = std::path::PathBuf::from(state.state_dir.clone());
            let native_lead =
                studio_runtime_services::layr_native::agent_value(&store, &call.lead_id)
                    .await
                    .map_err(|_| ApiError::internal())?;
            let native_owner =
                studio_runtime_services::layr_native::agent_value(&store, &call.owner_id)
                    .await
                    .map_err(|_| ApiError::internal())?;
            let response = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-http/routes/work_write",
                move || {
                    if studio_runtime_services::layr_native::is_native(&native_lead) {
                        return studio_runtime_services::layr_native::merge(
                            &runtime_state,
                            &native_lead,
                            &native_owner,
                            params["expectedStateId"].as_str().unwrap_or_default(),
                            &operation_id,
                        )
                        .map_err(native_layr_error);
                    }
                    UnixGuestRpc::for_runtime_state(&runtime_state).call(
                        "line.merge",
                        &params,
                        Some(&operation_id),
                        Duration::from_secs(20),
                    )
                },
            )
            .await
            .map_err(|_| ApiError::internal())?;
            let result_id = call.result_id;
            match response {
                Ok(_) => {
                    let request = call.request_id;
                    finish_vm_accept(&store, &task_id, &request, None)
                        .await
                        .map_err(write_error)?;
                    input.merged_result_id = Some(result_id);
                }
                Err(error) => {
                    let request = call.request_id;
                    let uncertain = error.uncertain();
                    finish_vm_accept(&store, &task_id, &request, Some(uncertain))
                        .await
                        .map_err(write_error)?;
                    return Err(ApiError::new(
                        StatusCode::BAD_GATEWAY,
                        format!("Layr merge failed: {error}"),
                    ));
                }
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
async fn prepare_vm_submission(
    state: &HttpState,
    agent: &str,
    request_id: Option<&str>,
    signature: &str,
    input: &mut SubmitWork,
) -> Result<(), ApiError> {
    use std::time::Duration;
    use studio_runtime_services::linux_vm_guest::{GuestRpc, UnixGuestRpc};
    use studio_runtime_services::work_write::{
        VmSubmissionPreparation, finish_vm_submission, prepare_vm_submission as prepare,
    };

    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let agent_id = agent.to_owned();
    let request = request_id.map(str::to_owned);
    let signature_owned = signature.to_owned();
    let preparation = prepare(
        &store,
        &agent_id,
        request.as_deref(),
        &signature_owned,
        input,
    )
    .await
    .map_err(write_error)?;
    match preparation {
        VmSubmissionPreparation::NotVm | VmSubmissionPreparation::Replay => {}
        VmSubmissionPreparation::Evidence {
            evidence,
            request_id,
        } => {
            input.vm_evidence = Some(evidence);
            input.vm_evidence_request_id = Some(request_id);
        }
        VmSubmissionPreparation::Call(call) => {
            let params = serde_json::json!({
                "agentId": call.owner_id,
                "stateId": call.revision,
            });
            let request_id = call.request_id.clone();
            input.vm_evidence_request_id = Some(request_id.clone());
            let expected_revision = call.revision.clone();
            let runtime_state = std::path::PathBuf::from(state.state_dir.clone());
            let native_owner =
                studio_runtime_services::layr_native::agent_value(&store, &call.owner_id)
                    .await
                    .map_err(|_| ApiError::internal())?;
            let response = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-http/routes/work_write",
                move || {
                    if studio_runtime_services::layr_native::is_native(&native_owner) {
                        return studio_runtime_services::layr_native::evidence(
                            &runtime_state,
                            &native_owner,
                            params["stateId"].as_str().unwrap_or_default(),
                        )
                        .map_err(native_layr_error);
                    }
                    UnixGuestRpc::for_runtime_state(&runtime_state).call(
                        "line.evidence",
                        &params,
                        Some(&request_id),
                        Duration::from_secs(20),
                    )
                },
            )
            .await
            .map_err(|_| ApiError::internal())?;
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    let task_id = input.task_id.clone();
                    let operation_id = call.request_id;
                    let uncertain = error.uncertain();
                    finish_vm_submission(&store, &task_id, &operation_id, Err(uncertain))
                        .await
                        .map_err(write_error)?;
                    return Err(ApiError::new(
                        StatusCode::BAD_GATEWAY,
                        format!("Layr evidence request failed: {error}"),
                    ));
                }
            };
            if response.get("stateId").and_then(Value::as_str) != Some(expected_revision.as_str()) {
                let task_id = input.task_id.clone();
                let operation_id = call.request_id;
                finish_vm_submission(&store, &task_id, &operation_id, Err(false))
                    .await
                    .map_err(write_error)?;
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "Submit the exact layr state ID",
                ));
            }
            let evidence = match (
                response.get("projectId").and_then(Value::as_str),
                response.get("line").and_then(Value::as_str),
            ) {
                (Some(project), Some(line)) => studio_runtime_store::vm_work_intent::Evidence {
                    layr_project_id: project.to_owned(),
                    layr_line: line.to_owned(),
                    layr_state_id: expected_revision,
                },
                _ => {
                    let task_id = input.task_id.clone();
                    let operation_id = call.request_id;
                    finish_vm_submission(&store, &task_id, &operation_id, Err(false))
                        .await
                        .map_err(write_error)?;
                    return Err(ApiError::new(
                        StatusCode::BAD_GATEWAY,
                        "Invalid layr evidence",
                    ));
                }
            };
            let task_id = input.task_id.clone();
            let operation_id = call.request_id;
            let evidence = finish_vm_submission(&store, &task_id, &operation_id, Ok(evidence))
                .await
                .map_err(write_error)?
                .ok_or_else(ApiError::internal)?;
            input.vm_evidence = Some(evidence);
        }
    }
    Ok(())
}

async fn commit(
    state: HttpState,
    runtime: Option<Arc<MonitorRuntime>>,
    agent: String,
    actor_id: Option<String>,
    request_id: Option<String>,
    signature: String,
    command: WorkWrite,
) -> Result<Value, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let wake = state.event_wake.clone();
    let is_submit = matches!(&command, WorkWrite::Submit(_));
    let is_accept = matches!(&command, WorkWrite::Accept(_));
    let has_monitor = runtime.is_some();
    let committed_wake = wake.clone();
    let ((result, checks, rule_now), _) = store
        .transaction_with_post_commit(
            move |tx| -> Result<_, WorkCommitError> {
                let write = |command| if let Some(actor_id) = actor_id.as_deref() {
                    work_write::write_action_as_agent(
                        tx,
                        &agent,
                        actor_id,
                        request_id.as_deref(),
                        &signature,
                        command,
                    )
                } else {
                    work_write::write(tx, &agent, request_id.as_deref(), &signature, command)
                };
                let result = match command {
                    // Work-record field checks run only after write_internal
                    // reconciles the exact receipt. Agent checks precede that
                    // reconciliation and must retain uncertainty about replay.
                    WorkWrite::Claim(input) => write(WorkWrite::Claim(input)).map_err(|error| match error {
                        WorkWriteError::Event(studio_runtime_services::event_write::EventWriteError::Store(error))
                        | WorkWriteError::Store(error) if matches!(&error,studio_runtime_store::StoreError::InvalidSyncObjectField {scope,..} if scope == WorkItemRecord::TABLE) => WorkCommitError::RefusedClaim(error),
                        error @ (WorkWriteError::Expired | WorkWriteError::Event(_)
                        | WorkWriteError::Store(_)
                        | WorkWriteError::NotImplemented(_)
                        | WorkWriteError::PreparationWorker) => WorkCommitError::Write(error),
                    })?,
                    command @ (WorkWrite::Create(_)
                    | WorkWrite::Mutate(_)
                    | WorkWrite::Submit(_)
                    | WorkWrite::Accept(_)) => write(command)?,
                };
                let mut checks = Vec::new();
                let mut rule_now = 0.0;
                if is_submit
                    && result.event_inserted
                    && result.record.get("submissionRefusal").is_none()
                {
                    let submission = result.record["results"]
                        .as_array()
                        .and_then(|results| results.last())
                        .ok_or_else(|| WorkCommitError::Store)?;
                    let root = result.record["rootId"].as_str().unwrap_or_default();
                    let result_id = submission["id"].as_str().unwrap_or_default();
                    let key = format!("work-result:{result_id}");
                    let text = serde_json::to_string(&serde_json::json!({
                        "task": result.record["id"],
                        "title": result.record["title"],
                        "result": submission,
                    }))
                    .map_err(studio_runtime_store::StoreError::from)?;
                    rule_now = submission["created"].as_f64().unwrap_or_default();
                    let mut after_rowid = 0;
                    loop {
                        let (report, batch) =
                            studio_runtime_services::rules_tick::event_in_transaction(
                                tx,
                                studio_runtime_services::rules_tick::RuleEvent {
                                    agent_id: root,
                                    kind: "work_review",
                                    text: &text,
                                    key: &key,
                                    now: rule_now,
                                },
                                after_rowid,
                                has_monitor,
                            )?;
                        checks.extend(batch);
                        let Some(next) = report.next_rowid else { break };
                        after_rowid = next;
                    }
                }
                Ok((result, checks, rule_now))
            },
            move |(result, _, _)| {
                if result.event_inserted {
                    committed_wake.notify_one();
                }
            },
        )
        .await
        .map_err(commit_error)?;
    if let Some(refusal) = result.record.get("submissionRefusal") {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, refusal.to_string()));
    }
    if is_submit && let Some(runtime) = runtime.as_ref() {
        runtime.wake_tick();
    }
    if !checks.is_empty() {
        let Some(runtime) = runtime else {
            return Err(ApiError::internal());
        };
        runtime
            .submit_event_checks(&checks, rule_now)
            .await
            .map_err(|_| ApiError::internal())?;
    }
    if let Some(artifact) = result.artifact {
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/routes/work_write",
            move || work_write::write_result_artifact(&artifact),
        )
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(|_| ApiError::internal())?;
    }
    if is_accept && result.record["status"] == "accepted" {
        let result = work_write::finalize_accepted_archive(
            store,
            WorkItemRecord::checked(&result.record).map_err(ApiError::stored)?,
        )
        .await
        .map_err(write_error)?;
        state.event_wake.notify_one();
        return Ok(result.into_value());
    }
    Ok(result.record)
}

fn commit_error(error: WorkCommitError) -> ApiError {
    match error {
        WorkCommitError::Store => ApiError::internal(),
        WorkCommitError::RefusedClaim(error) => ApiError::stored(error).with_not_applied(),
        WorkCommitError::Write(error) => write_error(error),
        WorkCommitError::Rules(
            studio_runtime_services::rules_tick::RulesTickError::MonitorUnavailable,
        ) => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires monitor-driver/rule-checks",
        ),
        WorkCommitError::Rules(_) => ApiError::internal(),
    }
}

pub(super) fn write_error(error: WorkWriteError) -> ApiError {
    match error {
        WorkWriteError::Expired => ApiError::new(StatusCode::GONE, error.to_string()),
        WorkWriteError::Event(error) => work_items::write_error(error),
        WorkWriteError::PreparationWorker => ApiError::internal(),
        WorkWriteError::Store(error) => ApiError::stored(error),
        WorkWriteError::NotImplemented(dependency) => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            format!("Not implemented: requires {dependency}"),
        ),
    }
}

/// Python `codex_work.py` refuses a review decision without its text by this name.
#[cfg(test)]
const REVIEW_RESULT_REQUIRED: &str = studio_runtime_schema::task_command::REVIEW_RESULT_REQUIRED;

#[cfg(test)]
mod tests {
    use serde_json::json;
    use studio_runtime_schema::task_command::Version;

    #[test]
    fn integral_decimal_version_keeps_its_stale_value() {
        assert_eq!(
            serde_json::from_value::<Version>(json!(0.0))
                .expect("integral version")
                .get(),
            0
        );
        assert!(serde_json::from_value::<Version>(json!(1.5)).is_err());
    }
}
#[cfg(test)]
mod managed_work_port_tests {
    use super::managed_work_port;
    use crate::router::HttpState;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use studio_runtime_services::provider_callbacks::WorkActionOutcome;
    use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore};

    thread_local! {
        static REQUESTS: std::cell::RefCell<std::collections::HashMap<String, String>> = std::cell::RefCell::new(std::collections::HashMap::new());
    }
    fn request_id(label: &str) -> String {
        REQUESTS.with(|requests| {
            requests
                .borrow_mut()
                .entry(label.to_owned())
                .or_insert_with(studio_runtime_schema::persistence::RequestIdentity::issue_root)
                .clone()
        })
    }

    async fn state() -> (HttpState, Arc<RuntimeStore>, tempfile::TempDir) {
        let directory = tempfile::tempdir().expect("isolated state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("runtime store opens"),
        );
        store
            .transaction(|tx| {
                for agent in [
                    json!({"provider":studio_runtime_schema::ProviderKind::Codex,"id":"lead","rootId":"lead","parentId":null,"epoch":1,
                        "status":"running","autoWake":true,"isLead":true,"name":"lead"}),
                    json!({"provider":studio_runtime_schema::ProviderKind::Codex,"id":"worker","rootId":"lead","parentId":"lead","epoch":1,
                        "status":"running","autoWake":true,"isLead":false,"name":"worker"}),
                ] {
                    AgentRepository::put(tx, &AgentRecord::from_value(agent)?)?;
                }
                Ok(())
            })
            .await
            .expect("agents commit");
        let state = HttpState::new(
            "token".to_owned(),
            "workspace".to_owned(),
            directory.path().to_string_lossy().into_owned(),
            4620,
            None,
            Some(Arc::clone(&store)),
            None,
        );
        REQUESTS.with(|requests| requests.borrow_mut().clear());
        (state, store, directory)
    }

    fn written(outcome: WorkActionOutcome) -> Value {
        match outcome {
            WorkActionOutcome::Written(record) => record,
            other @ WorkActionOutcome::Refused(_) | other @ WorkActionOutcome::Unknown(_) => {
                panic!("expected a written task, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn managed_task_text_bounds_use_the_tool_transport() {
        let (state, _store, _directory) = state().await;
        let port = managed_work_port(state, None);
        let created = written(
            port.act(
                "lead",
                &json!({"action":"create","title":"Task","description":"🙂".repeat(32000)}),
                &request_id("tool-bound-create"),
            )
            .await,
        );
        let task = &created["id"];
        for action in ["create", "update"] {
            let outcome = port.act("lead", &json!({"action":action,"task_id":task,"title":"Task","description":"🙂".repeat(32001)}), &request_id(&format!("tool-bound-{action}"))).await;
            assert!(
                matches!(outcome, WorkActionOutcome::Refused(_)),
                "{outcome:?}"
            );
        }
        written(
            port.act(
                "lead",
                &json!({"action":"update","task_id":task,"description":"🙂".repeat(32000)}),
                &request_id("tool-bound-update"),
            )
            .await,
        );
        let outcome = port
            .act(
                "lead",
                &json!({"action":"cancel","task_id":task,"reason":"🙂".repeat(32001)}),
                &request_id("tool-bound-reason-long"),
            )
            .await;
        assert!(
            matches!(outcome, WorkActionOutcome::Refused(_)),
            "{outcome:?}"
        );
        let cancelled = written(
            port.act(
                "lead",
                &json!({"action":"cancel","task_id":task,"reason":"🙂".repeat(32000)}),
                &request_id("tool-bound-reason"),
            )
            .await,
        );
        assert_eq!(cancelled["status"], "cancelled");
    }

    fn schema() -> Value {
        json!({"type":"object","properties":{"count":{"type":"integer"}},"required":["count"],"additionalProperties":false})
    }

    #[tokio::test]
    async fn task_command_preupgrade_receipt_replays_nullable_unused_fields() {
        let (state, store, _directory) = state().await;
        let created = super::perform(
            state.clone(),
            None,
            json!({"action":"create","agent":"lead","id":request_id("seed-create"),"title":"Task"}),
            None,
        )
        .await
        .expect("seed task");
        for (index, actor) in [None, Some("lead")].into_iter().enumerate() {
            let request = format!("legacy-create-{index}");
            // A successful pre-upgrade create retained these unused nullable
            // fields in its HTTP body and therefore in its receipt signature.
            let original = json!({"action":"create","agent":"lead","id":request,"title":"Task",
                "checks":null,"task_id":null,"reason":null,"revision":null,"files":null,"result":null});
            let signature = super::work_items::work_request_signature("lead", actor, &original)
                .expect("original hash");
            let saved_signature = signature.clone();
            let saved_result = json!({"actor":actor,"reply":created,"references":[]}).to_string();
            let saved_request = request.clone();
            store.transaction(move |tx| {
                tx.execute("INSERT INTO request_results(id,signature,result,created) VALUES (?1,?2,?3,unixepoch()*1000)",
                    rusqlite::params![saved_request,saved_signature,saved_result])?;
                Ok(())
            }).await.expect("pre-upgrade successful receipt");
            let replayed = if actor.is_some() {
                written(
                    managed_work_port(state.clone(), None)
                        .act("lead", &original, &request)
                        .await,
                )
            } else {
                super::perform(state.clone(), None, original.clone(), None)
                    .await
                    .expect("original HTTP body replays")
            };
            assert_eq!(replayed, created);
            let query_request = request.clone();
            let (actual_signature, actual_result, task_count) = store
                .read_async("legacy-replay-test", move |db| {
                    let (signature, result) = db.query_row(
                        "SELECT signature,result FROM request_results WHERE id=?1",
                        [query_request],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )?;
                    let count: i64 =
                        db.query_row("SELECT count(*) FROM runtime_work", [], |row| row.get(0))?;
                    Ok::<_, studio_runtime_store::StoreError>((signature, result, count))
                })
                .await
                .expect("exact saved receipt");
            assert_eq!(actual_signature, signature);
            assert_eq!(
                serde_json::from_str::<Value>(&actual_result).expect("envelope")["reply"],
                created
            );
            assert_eq!(task_count, 1, "replay creates no additional task");
            let mut changed = original;
            changed.as_object_mut().expect("body").remove("checks");
            assert!(
                super::perform(state.clone(), None, changed, actor.map(str::to_owned))
                    .await
                    .is_err(),
                "dropping a null field cannot rewrite the original hash"
            );
        }
    }

    #[tokio::test]
    async fn task_command_ingress_preserves_receipt_body_and_owner_presence() {
        let (state, _store, _directory) = state().await;
        let port = managed_work_port(state, None);
        let created = written(port.act("lead", &json!({"action":"create","title":"Typed ingress","description":"x".repeat(32000),"owner":"worker"}), &request_id("typed-create")).await);
        let task = &created["id"];
        let update =
            json!({"action":"update","task_id":task,"version":1.0,"description":"Changed"});
        let updated = written(port.act("lead", &update, &request_id("typed-update")).await);
        assert_eq!(
            updated["owner"], "worker",
            "omitted owner retains assignment"
        );
        assert_eq!(
            written(port.act("lead", &update, &request_id("typed-update")).await),
            updated
        );
        let mut changed = update;
        changed["version"] = json!(1);
        assert!(
            matches!(port.act("lead", &changed, &request_id("typed-update")).await,
            WorkActionOutcome::Refused(message) if message.contains("different content")),
            "decimal spelling remains part of the original receipt signature"
        );
        let cleared = written(
            port.act(
                "lead",
                &json!({"action":"update","task_id":task,"version":2.0,"owner":null}),
                &request_id("typed-clear"),
            )
            .await,
        );
        assert_eq!(cleared["owner"], Value::Null);
        assert!(matches!(
            port.act(
                "lead",
                &json!({"action":"update","task_id":task,"owner":42}),
                &request_id("typed-malformed")
            )
            .await,
            WorkActionOutcome::Refused(_)
        ));
    }

    #[tokio::test]
    async fn stored_review_boundary_claim_replay_keeps_prior_effect_uncertain() {
        let (state, store, _directory) = state().await;
        let port = managed_work_port(state, None);
        let created = written(
            port.act(
                "lead",
                &json!({"action":"create","title":"Replay boundary","owner":"worker"}),
                &request_id("replay-create"),
            )
            .await,
        );
        let task_id = created["id"].as_str().expect("task id").to_owned();
        let args = json!({"action":"claim","task_id":task_id,"version":created["version"]});
        // Commit the first claim and discard its response, as on response loss.
        drop(port.act("worker", &args, &request_id("same-claim")).await);
        let reader = store.test_reader().expect("reader");
        let snapshot = || {
            reader.query_row("SELECT w.record,r.signature,r.result FROM runtime_work w JOIN request_results r ON json_extract(r.result,'$.reply.id')=w.id WHERE w.id=?1", [&task_id], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?))).expect("successful claim and exact receipt")
        };
        let before = snapshot();
        assert_eq!(
            serde_json::from_str::<Value>(&before.0).expect("stored task")["status"],
            "running"
        );
        store.transaction(|tx| {
            tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.workspaceMode',7) WHERE id='worker'", [])?;
            Ok(())
        }).await.expect("corrupt actor after committed success");
        let replay = port.act("worker", &args, &request_id("same-claim")).await;
        assert!(
            matches!(replay, WorkActionOutcome::Unknown(_)),
            "a prior committed effect cannot become NotApplied: {replay:?}"
        );
        assert_eq!(
            snapshot(),
            before,
            "the retry preserves the exact prior task and receipt"
        );
    }

    #[tokio::test]
    async fn stored_work_boundary_missing_status_returns_error_without_panic() {
        let (state, store, _directory) = state().await;
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_work(id,record) VALUES('missing-status',?1)",
                    [json!({"id":"missing-status","rootId":"lead","owner":"worker"}).to_string()],
                )?;
                Ok(())
            })
            .await
            .expect("existing missing status fixture");
        let error = super::work_items::work_for_agent(state, "lead".into())
            .await
            .expect_err("missing status returns a located error");
        assert!(error.message().contains("runtime_work"), "{error:?}");
    }

    #[tokio::test]
    async fn stored_work_boundary_refuses_corrupt_claims_and_reads_without_writes() {
        for (field, corrupt) in [
            ("dependencies", Some(json!({"0":"prerequisite"}))),
            ("dependencies", Some(json!(7))),
            ("dependencies", Some(json!(["prerequisite", 7]))),
            ("dependencies", Some(Value::Null)),
            ("status", None),
            ("status", Some(Value::Null)),
        ] {
            let (state, store, _directory) = state().await;
            let port = managed_work_port(state.clone(), None);
            store.transaction(|tx| {
                studio_runtime_store::WorkItemsRepository::put_work(tx,"prerequisite",&json!({"id":"prerequisite","rootId":"lead","title":"Unaccepted prerequisite","status":"ready","owner":null,"dependencies":[],"results":[],"decisions":[],"version":1,"created":1,"updated":1}))?;
                Ok(())
            }).await.expect("real unaccepted prerequisite");
            let created = written(
                port.act(
                    "lead",
                    &json!({"action":"create","title":"Stored boundary","owner":"worker","dependencies":["prerequisite"]}),
                    &request_id("boundary-create"),
                )
                .await,
            );
            let id = created["id"].as_str().expect("task id").to_owned();
            let mut corrupt_record = created.clone();
            if let Some(value) = corrupt {
                corrupt_record[field] = value;
            } else {
                corrupt_record
                    .as_object_mut()
                    .expect("work object")
                    .remove(field);
            }
            // Simulate existing stored corruption without going through a fresh writer.
            let corrupt_id = id.clone();
            store
                .transaction(move |tx| {
                    tx.execute(
                        "UPDATE runtime_work SET record=?1 WHERE id=?2",
                        rusqlite::params![corrupt_record.to_string(), corrupt_id],
                    )?;
                    Ok(())
                })
                .await
                .expect("corrupt fixture");
            let connection = store.test_reader().expect("fixture connection");
            let snapshot = || {
                [
                    "SELECT id,record FROM runtime_work ORDER BY id",
                    "SELECT id,json_array(signature,result) FROM request_results ORDER BY id",
                    "SELECT id,json_array(agent,kind,text,status,created,epoch,turn_id,error) FROM runtime_events ORDER BY id",
                ]
                .into_iter()
                .map(|sql| {
                    connection
                        .prepare(sql)
                        .expect("snapshot statement")
                        .query_map([], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })
                        .expect("snapshot query")
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .expect("snapshot rows")
                })
                .collect::<Vec<_>>()
            };
            let before = snapshot();
            let claim = json!({"action":"claim","agent":"worker","task_id":id,"version":created["version"],"id":request_id("boundary-http-claim")});
            let error = super::perform(state.clone(), None, claim.clone(), None)
                .await
                .expect_err("HTTP claim refuses malformed stored work");
            assert!(error.message().contains("runtime_work"), "{error:?}");
            let outcome = port
                .act("worker", &claim, &request_id("boundary-native-claim"))
                .await;
            assert!(
                matches!(outcome, WorkActionOutcome::Refused(_)),
                "{outcome:?}"
            );
            for (actor, action, extra) in [
                (
                    "worker",
                    "submit",
                    json!({"result":"proof","checks":"fixture","revision":"fixture","files":[]}),
                ),
                ("lead", "accept", json!({"result":"review"})),
            ] {
                let mut args = extra;
                args["action"] = json!(action);
                args["task_id"] = json!(id);
                args["version"] = created["version"].clone();
                let outcome = port
                    .act(
                        actor,
                        &args,
                        &request_id(&format!("boundary-native-{action}")),
                    )
                    .await;
                assert!(
                    matches!(outcome, WorkActionOutcome::Unknown(_)),
                    "{action}: {outcome:?}"
                );
            }
            let error = super::work_items::work_for_agent(state, "lead".into())
                .await
                .expect_err("HTTP read returns a located error without panic");
            assert!(error.message().contains("runtime_work"), "{error:?}");
            assert_eq!(
                snapshot(),
                before,
                "refusal preserves exact records, versions and receipts for {field}"
            );
        }
    }

    fn refused(outcome: WorkActionOutcome) -> String {
        match outcome {
            WorkActionOutcome::Refused(message) => message,
            other @ WorkActionOutcome::Written(_) | other @ WorkActionOutcome::Unknown(_) => {
                panic!("expected refusal, got {other:?}")
            }
        }
    }

    async fn task_tool_reply(store: Arc<RuntimeStore>, action: &str, args: Value) -> Value {
        use studio_runtime_providers::{Frame, Method, RequestFrame, RequestId, ResponseBody};
        use studio_runtime_services::provider_callbacks::{
            CallbackContext, CallbackDecision, dispatch,
        };
        store
            .transaction(|tx| {
                let mut agent = AgentRepository::get(tx, &"lead".into())?.expect("lead");
                agent.extra.insert("threadId".into(), json!("thread"));
                agent.extra.insert("turnId".into(), json!("turn"));
                AgentRepository::put(tx, &agent)?;
                Ok(())
            })
            .await
            .expect("callback identity");
        let context = CallbackContext {
            agent: "lead".into(),
            account: "default".into(),
            connection: "connection".into(),
            stream_id: "stream".into(),
            generation: 1,
            epoch: 1,
            thread: "thread".into(),
            turn: "turn".into(),
        };
        let frame = Frame::Request(RequestFrame {
            id: RequestId::String(format!("rpc-{action}")),
            method: Method::ItemToolCall,
            params: Some(
                json!({"threadId":"thread","turnId":"turn","callId":format!("read-{action}"),
                "tool":"orchestration_task","arguments":args}),
            ),
            fields: Default::default(),
        });
        let CallbackDecision::Answer(reply) = dispatch(store, &context, &frame, 100.0)
            .await
            .expect("read callback")
        else {
            panic!("answer");
        };
        let ResponseBody::Result(body) = reply.body else {
            panic!("result");
        };
        body
    }

    async fn read_task_tool(store: Arc<RuntimeStore>, task: &str, action: &str) -> Value {
        let body = task_tool_reply(store, action, json!({"action":action,"task_id":task})).await;
        assert_eq!(body["success"], true, "{body}");
        serde_json::from_str(body["contentItems"][0]["text"].as_str().expect("text"))
            .expect("JSON tool output")
    }

    #[tokio::test]
    async fn task_result_schema_create_update_reject_invalid_without_writes() {
        let (state, store, _dir) = state().await;
        let port = managed_work_port(state, None);
        let body = task_tool_reply(Arc::clone(&store), "create-invalid", json!({"action":"create","title":"invalid tool contract","result_schema":{"type":"wrong"}})).await;
        assert_eq!(body["success"], false, "{body}");
        assert!(
            body["contentItems"][0]["text"]
                .as_str()
                .expect("refusal text")
                .contains("result_schema")
        );
        for (index, contract) in [json!({"type":"wrong"}), json!({"required":true}), json!({"description":"x".repeat(studio_runtime_schema::task_results::MAX_RESULT_SCHEMA_BYTES)})].into_iter().enumerate() {
            let message = refused(port.act("lead", &json!({"action":"create","title":"invalid","result_schema":contract}), &request_id(&format!("invalid-{index}"))).await);
            assert!(message.contains("result_schema"), "{message}");
        }
        let count = store
            .read_async("test", |db| {
                db.query_row("SELECT count(*) FROM runtime_work", [], |r| {
                    r.get::<_, i64>(0)
                })
                .map_err(studio_runtime_store::StoreError::from)
            })
            .await
            .expect("count");
        assert_eq!(count, 0);
        let created = written(
            port.act(
                "lead",
                &json!({"action":"create","title":"valid","result_schema":schema()}),
                &request_id("create"),
            )
            .await,
        );
        assert_eq!(created["resultSchema"], schema());
        let task = created["id"].as_str().expect("id");
        refused(
            port.act(
                "lead",
                &json!({"action":"update","task_id":task,"result_schema":{"properties":false}}),
                &request_id("invalid-update"),
            )
            .await,
        );
        let stored = read_task_tool(store, task, "get").await;
        assert_eq!(stored["version"], created["version"]);
        assert_eq!(stored["resultSchema"], schema());
    }

    #[tokio::test]
    async fn task_result_schema_submit_errors_retry_and_json_get_history() {
        let (state, store, _dir) = state().await;
        let port = managed_work_port(state, None);
        let created = written(port.act("lead", &json!({"action":"create","title":"results","owner":"worker","result_schema":schema()}), &request_id("create")).await);
        let task = created["id"].as_str().expect("id");
        let before = created.clone();
        let args = json!({"action":"submit","task_id":task,"result":{"count":"bad"},"checks":"checked","revision":"abc","files":[]});
        let message = refused(
            port.act("worker", &args, &request_id("invalid-submit"))
                .await,
        );
        let error: Value = serde_json::from_str(&message).expect("structured refusal");
        assert_eq!(error["errors"][0]["path"], "/count");
        assert_eq!(error["failedSubmits"], 1);
        assert_eq!(
            refused(
                port.act("worker", &args, &request_id("invalid-submit"))
                    .await
            ),
            message
        );
        let task_id = task.to_owned();
        let saved = store
            .read_async("test", move |db| {
                studio_runtime_store::WorkItemsRepository::work_record(db, &task_id)
            })
            .await
            .expect("read")
            .expect("task");
        assert_eq!(saved.value()["version"], before["version"]);
        assert_eq!(serde_json::json!(saved.status()), before["status"]);
        assert_eq!(saved.value()["results"], before["results"]);
        assert!(saved.get("submissionRefusal").is_none());
        let mut valid = args;
        valid["result"] = json!({"count":7});
        let submitted = written(
            port.act("worker", &valid, &request_id("valid-submit"))
                .await,
        );
        assert_eq!(submitted["status"], "review");
        assert_eq!(
            written(
                port.act("worker", &valid, &request_id("valid-submit"))
                    .await
            ),
            submitted
        );
        let got = read_task_tool(Arc::clone(&store), task, "get").await;
        assert_eq!(got["latestResult"]["result"], json!({"count":7}));
        let history = read_task_tool(store, task, "history").await;
        assert_eq!(
            history["items"][0]["result"],
            json!({"count":7}),
            "{history}"
        );
    }

    #[tokio::test]
    async fn task_result_schema_retry_cap_emits_one_event_and_lead_update_resets() {
        use studio_runtime_schema::task_results::MAX_FAILED_RESULT_SUBMITS_PER_ATTEMPT;
        let (state, store, _dir) = state().await;
        let port = managed_work_port(state, None);
        let created = written(
            port.act(
                "lead",
                &json!({"action":"create","title":"cap","owner":"worker","result_schema":schema()}),
                &request_id("create"),
            )
            .await,
        );
        let task = created["id"].as_str().expect("id");
        let mut args = json!({"action":"submit","task_id":task,"result":"not JSON","checks":"checked","revision":"abc"});
        for index in 0..MAX_FAILED_RESULT_SUBMITS_PER_ATTEMPT {
            let message = refused(
                port.act("worker", &args, &request_id(&format!("invalid-{index}")))
                    .await,
            );
            let refusal: Value = serde_json::from_str(&message).expect("refusal");
            assert_eq!(refusal["failedSubmits"], index + 1);
        }
        args["result"] = json!({"count":2});
        assert!(
            refused(port.act("worker", &args, &request_id("over-cap")).await)
                .contains("limit reached")
        );
        let events = store.read_async("test", |db| db.query_row("SELECT count(*) FROM runtime_events WHERE kind='work_result_invalid' AND agent='lead' AND status='pending'", [], |r| r.get::<_,i64>(0)).map_err(studio_runtime_store::StoreError::from)).await.expect("events");
        assert_eq!(events, 1);
        written(
            port.act(
                "lead",
                &json!({"action":"update","task_id":task,"result_schema":schema()}),
                &request_id("reset"),
            )
            .await,
        );
        assert_eq!(
            written(port.act("worker", &args, &request_id("new-attempt")).await)["status"],
            "review"
        );
    }

    #[tokio::test]
    async fn task_result_schema_legacy_text_behavior_is_unchanged() {
        let (state, _store, _dir) = state().await;
        let port = managed_work_port(state, None);
        let created = written(
            port.act(
                "lead",
                &json!({"action":"create","title":"legacy","owner":"worker"}),
                &request_id("create"),
            )
            .await,
        );
        let args = json!({"action":"submit","task_id":created["id"],"result":"plain text","checks":"checked","revision":"abc"});
        let result = written(port.act("worker", &args, &request_id("submit")).await);
        assert_eq!(result["results"][0]["text"], "plain text");
        assert!(result["results"][0].get("result").is_none());
    }

    #[tokio::test]
    async fn the_tool_port_runs_the_work_route_owner_with_the_tool_request_identity() {
        let (state, store, _directory) = state().await;
        let port = managed_work_port(state, None);
        let created = written(
            port.act(
                "lead",
                &json!({"action":"create","title":"Port check","owner":"worker"}),
                &request_id("tool-create"),
            )
            .await,
        );
        let task = created["id"].as_str().expect("task id").to_owned();
        let extension_task = task.clone();
        store.transaction(move |tx| {
            tx.execute("UPDATE runtime_work SET record=json_set(record,'$.retainedExtension',json('{\"proof\":[7,null]}')) WHERE id=?1", [extension_task])?;
            Ok(())
        }).await.expect("stored extension");
        let claim = json!({"action":"claim","task_id":task,"version":created["version"]});
        let claimed = written(port.act("worker", &claim, &request_id("tool-claim")).await);
        assert_eq!(claimed["owner"], "worker");
        assert_eq!(claimed["status"], "running");
        assert_eq!(claimed["retainedExtension"], json!({"proof":[7,null]}));
        // The same tool request replays its receipt instead of acting again.
        assert_eq!(
            written(port.act("worker", &claim, &request_id("tool-claim")).await),
            claimed
        );
        // A refusal before any write reaches the model as a refusal.
        let refused = port
            .act(
                "worker",
                &json!({"action":"claim","task_id":"missing-task","version":1}),
                &request_id("tool-claim-missing"),
            )
            .await;
        assert!(
            matches!(refused, WorkActionOutcome::Refused(_)),
            "{refused:?}"
        );
    }

    #[tokio::test]
    async fn acceptance_gate_schema_refusal_never_admits_monitor() {
        let (state, store, _directory) = state().await;
        let port = managed_work_port(state, None);
        let created = written(
            port.act(
                "lead",
                &json!({"action":"create",
            "title":"Schema before gate", "owner":"worker", "gate":"true",
            "result_schema":schema()}),
                &request_id("schema-gate-create"),
            )
            .await,
        );
        let task = &created["id"];
        let args = json!({"action":"submit", "task_id":task,
            "result":{"count":"invalid"}, "checks":"checked",
            "revision":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"});
        let refusal: Value = serde_json::from_str(&refused(
            port.act("worker", &args, &request_id("schema-gate-invalid"))
                .await,
        ))
        .expect("schema refusal parses");
        assert_eq!(refusal["errors"][0]["path"], "/count");
        let count = store
            .read_async("gate-test/schema", |db| {
                db.query_row("SELECT count(*) FROM runtime_monitors", [], |r| {
                    r.get::<_, i64>(0)
                })
                .map_err(studio_runtime_store::StoreError::from)
            })
            .await
            .expect("monitor count loads");
        assert_eq!(count, 0);
        let mut valid = args;
        valid["result"] = json!({"count":1});
        let submitted = written(
            port.act("worker", &valid, &request_id("schema-gate-valid"))
                .await,
        );
        // No prepared workspace is recorded explicitly, never certified as a pass.
        assert_eq!(submitted["results"][0]["gate"]["status"], "not_run");
        assert!(matches!(
            port.act(
                "lead",
                &json!({"action":"accept",
            "task_id":task,"result":"reviewed"}),
                &request_id("schema-gate-accept")
            )
            .await,
            WorkActionOutcome::Refused(_)
        ));
    }

    #[tokio::test]
    async fn acceptance_gate_tool_caller_inherits_default_admits_monitor_and_refuses_failed_accept()
    {
        let (state, store, directory) = state().await;
        let cwd = directory.path().to_string_lossy().into_owned();
        store
            .transaction(move |tx| {
                let mut worker =
                    AgentRepository::get(tx, &"worker".into())?.expect("worker fixture exists");
                worker.extra.insert("cwd".into(), json!(cwd));
                worker.extra.insert("yoloMode".into(), json!(true));
                AgentRepository::put(tx, &worker)?;
                Ok(())
            })
            .await
            .expect("worker fixture updates");
        let port = managed_work_port(state, None);
        let default = written(
            port.act(
                "lead",
                &json!({"action":"set_default_gate","gate":{"command":"false","timeout_ms":4321}}),
                &request_id("set-default"),
            )
            .await,
        );
        assert_eq!(default["defaultGate"]["timeout_ms"], 4321);
        assert!(matches!(
            port.act(
                "worker",
                &json!({"action":"set_default_gate","gate":null}),
                &request_id("worker-default")
            )
            .await,
            WorkActionOutcome::Refused(_)
        ));
        let created = written(
            port.act(
                "lead",
                &json!({"action":"create","title":"Gate","owner":"worker"}),
                &request_id("create-gate"),
            )
            .await,
        );
        assert_eq!(created["gate"], default["defaultGate"]);
        let task = created["id"]
            .as_str()
            .expect("created task identity is text")
            .to_owned();
        let submitted = written(
            port.act(
                "worker",
                &json!({"action":"submit","task_id":task,"result":"done","checks":"reported",
            "revision":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}),
                &request_id("submit-gate"),
            )
            .await,
        );
        let id = submitted["results"][0]["gate"]["monitorId"]
            .as_str()
            .expect("gate monitor identity is text")
            .to_owned();
        let log_root = studio_runtime_services::monitor_output::log_root(store.root());
        store
            .transaction_with(move |tx| {
                let monitor = studio_runtime_store::MonitorsRepository::get(tx, &id)?
                    .expect("gate monitor exists");
                assert_eq!(
                    std::path::Path::new(
                        monitor.value()["log"]
                            .as_str()
                            .expect("monitor log path is text")
                    )
                    .parent(),
                    Some(log_root.as_path())
                );
                assert_eq!(monitor.value()["timeout_ms"], 4321);
                assert_eq!(monitor.value()["agent"], "worker");
                studio_runtime_services::monitor_runtime::finish_in_transaction(
                    tx,
                    &id,
                    None,
                    Some(1),
                    None,
                    2.0,
                )
            })
            .await
            .expect("monitor assertions complete");
        assert!(matches!(
            port.act(
                "lead",
                &json!({"action":"accept","task_id":task,"result":"reviewed"}),
                &request_id("accept-gate")
            )
            .await,
            WorkActionOutcome::Refused(_)
        ));
        assert!(matches!(port.act("worker",&json!({"action":"accept","task_id":task,"result":"reviewed","override_reason":"skip"}),&request_id("worker-override")).await,WorkActionOutcome::Refused(_)));
        let accepted=written(port.act("lead",&json!({"action":"accept","task_id":task,"result":"reviewed","override_reason":"Manual evidence checked"}),&request_id("override-gate")).await);
        assert_eq!(accepted["status"], "accepted");
        assert_eq!(
            accepted["decisions"][0]["gateOverrideReason"],
            "Manual evidence checked"
        );
    }

    #[tokio::test]
    async fn acceptance_gate_typed_input_preserves_null_update_and_replay() {
        let (state, _, _) = state().await;
        let port = managed_work_port(state, None);
        let default_args = json!({"action":"set_default_gate","gate":" check "});
        let default = written(
            port.act("lead", &default_args, &request_id("typed-default"))
                .await,
        );
        assert_eq!(
            default["defaultGate"],
            json!({"command":" check ","timeout_ms":600000})
        );
        assert_eq!(
            written(
                port.act("lead", &default_args, &request_id("typed-default"))
                    .await
            ),
            default
        );
        let created = written(
            port.act(
                "lead",
                &json!({"action":"create","title":"typed"}),
                &request_id("typed-create"),
            )
            .await,
        );
        assert_eq!(created["gate"], default["defaultGate"]);
        let task = created["id"].as_str().expect("task id");
        let updated = written(
            port.act(
                "lead",
                &json!({"action":"update","task_id":task,"gate":null}),
                &request_id("typed-clear"),
            )
            .await,
        );
        assert!(updated["gate"].is_null());
        let updated = written(port.act("lead", &json!({"action":"update","task_id":task,"gate":{"command":"test","timeout_ms":1234}}), &request_id("typed-config")).await);
        assert_eq!(updated["gate"], json!({"command":"test","timeout_ms":1234}));
        let updated = written(
            port.act(
                "lead",
                &json!({"action":"update","task_id":task,"description":"unchanged gate"}),
                &request_id("typed-missing"),
            )
            .await,
        );
        assert_eq!(updated["gate"], json!({"command":"test","timeout_ms":1234}));
        assert!(matches!(
            port.act(
                "lead",
                &json!({"action":"set_default_gate","gate":"different"}),
                &request_id("typed-default")
            )
            .await,
            WorkActionOutcome::Refused(_)
        ));
        assert!(matches!(
            port.act(
                "lead",
                &json!({"action":"create","title":"bad","gate":"  "}),
                &request_id("typed-invalid")
            )
            .await,
            WorkActionOutcome::Refused(_)
        ));
    }

    #[tokio::test]
    async fn authenticated_task_caller_cannot_submit_another_owners_task() {
        let (state, store, directory) = state().await;
        let cwd = directory.path().to_string_lossy().into_owned();
        store
            .transaction(move |tx| {
                let mut lead =
                    AgentRepository::get(tx, &"lead".into())?.expect("lead fixture exists");
                lead.extra.insert("cwd".into(), json!(cwd));
                lead.extra.insert("yoloMode".into(), json!(true));
                AgentRepository::put(tx, &lead)?;
                Ok(())
            })
            .await
            .expect("lead fixture updates");
        let port = managed_work_port(state, None);
        for (name, gate) in [("ungated", None), ("gated", Some("true"))] {
            let created = written(
                port.act(
                    "lead",
                    &json!({
                        "action":"create", "title":"Lead task", "owner":"lead", "gate":gate
                    }),
                    &request_id(&format!("{name}-create")),
                )
                .await,
            );
            let task = created["id"]
                .as_str()
                .expect("created task identity")
                .to_owned();
            let submit = json!({"action":"submit", "task_id":task,
                "result":"done", "checks":"checked", "revision":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"});
            let outcome = port
                .act(
                    "worker",
                    &submit,
                    &request_id(&format!("{name}-spoof-submit")),
                )
                .await;
            assert!(
                matches!(&outcome, WorkActionOutcome::Refused(message)
                if message.contains("Only the assigned worker")),
                "{name}: {outcome:?}"
            );
            let unchanged = store
                .read_async("auth-test/owner", move |tx| {
                    let monitors: i64 =
                        tx.query_row("SELECT COUNT(*) FROM runtime_monitors", [], |row| {
                            row.get(0)
                        })?;
                    assert_eq!(monitors, 0, "refusal must not admit a gate monitor");
                    studio_runtime_store::WorkItemsRepository::work_record(tx, &task)
                })
                .await
                .expect("task query succeeds")
                .expect("created task exists");
            assert!(
                unchanged.value()["results"]
                    .as_array()
                    .expect("task results")
                    .is_empty()
            );
            if gate.is_none() {
                let submitted = written(
                    port.act("lead", &submit, &request_id("auth-owner-submit"))
                        .await,
                );
                assert_eq!(submitted["results"][0]["agent"], "lead");
            }
        }
    }

    #[tokio::test]
    async fn authenticated_task_caller_refuses_worker_review_and_mutation_but_allows_lead() {
        let (state, store, _directory) = state().await;
        let port = managed_work_port(state, None);
        for action in ["accept", "reject", "cancel", "update"] {
            let created = written(
                port.act(
                    "lead",
                    &json!({
                        "action":"create", "title":"Review task", "owner":"worker"
                    }),
                    &request_id(&format!("auth-create-{action}")),
                )
                .await,
            );
            let task = created["id"]
                .as_str()
                .expect("created task identity is text")
                .to_owned();
            let submitted = if action == "update" {
                created
            } else {
                written(
                    port.act(
                        "worker",
                        &json!({
                            "action":"submit", "task_id":task, "result":"done",
                            "checks":"checked", "revision":"abc1234"
                        }),
                        &request_id(&format!("auth-submit-{action}")),
                    )
                    .await,
                )
            };
            let args = json!({"action":action,"task_id":task,"result":"reviewed",
                "reason":"cancelled", "title":"Changed title"});
            let refused = port
                .act(
                    "worker",
                    &args,
                    &request_id(&format!("auth-worker-{action}")),
                )
                .await;
            assert!(
                matches!(refused, WorkActionOutcome::Refused(_)),
                "{action}: {refused:?}"
            );
            let task_id = task.clone();
            let unchanged = store
                .read_async("auth-test/refusal", move |tx| {
                    studio_runtime_store::WorkItemsRepository::work_record(tx, &task_id)
                })
                .await
                .expect("task query succeeds")
                .expect("created task exists");
            assert_eq!(
                unchanged.value()["version"],
                submitted["version"],
                "{action}"
            );
            assert_eq!(
                serde_json::json!(unchanged.status()),
                submitted["status"],
                "{action}"
            );
            let allowed = written(
                port.act("lead", &args, &request_id(&format!("auth-lead-{action}")))
                    .await,
            );
            assert_eq!(
                allowed["status"],
                match action {
                    "accept" => "accepted",
                    "reject" => "ready",
                    "cancel" => "cancelled",
                    _ => "ready",
                }
            );
        }
        // A worker retains the documented right to cancel a task it created.
        let created = written(
            port.act(
                "lead",
                &json!({"action":"create",
            "title":"Worker task", "owner":"worker"}),
                &request_id("auth-worker-create"),
            )
            .await,
        );
        let creator_task = created["id"]
            .as_str()
            .expect("created task identity is text")
            .to_owned();
        store
            .transaction(move |tx| {
                let mut record =
                    studio_runtime_store::WorkItemsRepository::work_record(tx, &creator_task)?
                        .expect("created task exists");
                record.set("createdBy", json!("worker"))?;
                studio_runtime_store::WorkItemsRepository::update_checked_work(
                    tx,
                    &creator_task,
                    &record,
                )
            })
            .await
            .expect("task creator updates");
        let cancelled = written(
            port.act(
                "worker",
                &json!({"action":"cancel",
            "task_id":created["id"],"reason":"No longer needed"}),
                &request_id("auth-creator-cancel"),
            )
            .await,
        );
        assert_eq!(cancelled["status"], "cancelled");
    }

    #[tokio::test]
    async fn authenticated_task_caller_cannot_replay_a_user_accept_receipt() {
        let (state, _store, _directory) = state().await;
        let port = managed_work_port(state.clone(), None);
        let created = written(
            port.act(
                "lead",
                &json!({"action":"create",
            "title":"User receipt", "owner":"worker"}),
                &request_id("auth-receipt-create"),
            )
            .await,
        );
        let task = &created["id"];
        written(
            port.act(
                "worker",
                &json!({"action":"submit", "task_id":task,
            "result":"done", "checks":"checked", "revision":"abc1234"}),
                &request_id("auth-receipt-submit"),
            )
            .await,
        );
        let args = json!({"action":"accept", "task_id":task, "result":"reviewed",
            "agent":"worker", "id":request_id("auth-user-accept")});
        let accepted = super::perform(state, None, args.clone(), None)
            .await
            .expect("gate override accepts");
        assert_eq!(accepted["status"], "accepted");
        let refused = port
            .act("worker", &args, &request_id("auth-user-accept"))
            .await;
        assert!(
            matches!(&refused, WorkActionOutcome::Refused(message)
            if message.contains("different content")),
            "{refused:?}"
        );
    }

    #[tokio::test]
    async fn a_task_action_without_a_required_field_names_that_field() {
        let (state, _store, _directory) = state().await;
        let port = managed_work_port(state, None);
        let created = written(
            port.act(
                "lead",
                &json!({"action":"create","title":"Named refusal","owner":"worker"}),
                &request_id("tool-create-named"),
            )
            .await,
        );
        let task = created["id"].as_str().expect("task id").to_owned();
        let accept = port
            .act(
                "lead",
                &json!({"action":"accept","task_id":task}),
                &request_id("tool-accept-without-result"),
            )
            .await;
        assert!(
            matches!(
                &accept,
                WorkActionOutcome::Refused(message) if message == super::REVIEW_RESULT_REQUIRED
            ),
            "{accept:?}"
        );
        let submit = port
            .act(
                "worker",
                &json!({"action":"submit","task_id":task,"result":"done","revision":"abc"}),
                &request_id("tool-submit-without-checks"),
            )
            .await;
        assert!(
            matches!(
                &submit,
                WorkActionOutcome::Refused(message)
                    if message == "Supply checks as text for this task action"
            ),
            "{submit:?}"
        );
    }
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "work_write",
        routes: Some(routes),
        dependencies: &[
            "cross-server/remote-events",
            "workspace-state/agent-archive",
            "workspace-state/submission-files",
        ],
    }
}

// Both Layr modes use the same durable exact-state task receipt contract.
#[cfg(unix)]
fn native_layr_error(
    error: studio_runtime_services::layr_native::NativeLayrError,
) -> studio_runtime_services::linux_vm_guest::GuestError {
    let uncertain = !matches!(
        error,
        studio_runtime_services::layr_native::NativeLayrError::Refused(_)
    );
    studio_runtime_services::linux_vm_guest::GuestError::Transport {
        message: error.to_string(),
        uncertain,
    }
}
