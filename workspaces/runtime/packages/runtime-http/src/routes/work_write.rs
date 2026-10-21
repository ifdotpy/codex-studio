//! HTTP adapter for work mutations.

use super::extractors::ApiJson;
use super::manifest::{OperationStatus, RouteGroup, post};
use super::work_items;
use axum::extract::State;
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde_json::Value;
use studio_runtime_services::work_write::{
    self, AcceptWork, ClaimWork, CreateWork, SubmitWork, WorkMutation, WorkWrite, WorkWriteError,
};

use crate::error::ApiError;
use crate::router::HttpState;
use std::sync::Arc;
use studio_runtime_services::monitor_runtime_wiring::MonitorRuntime;

enum WorkCommitError {
    Store,
    Write(WorkWriteError),
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
                Err(error) if error.status().is_client_error() => {
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
    let mut command = match action {
        "create" => WorkWrite::Create(CreateWork {
            result_schema: body.get("result_schema").cloned(),
            actor_id: agent.clone(),
            title: required_text(&body, "title")?,
            description: optional_text(&body, "description")?.unwrap_or_default(),
            owner: body.get("owner").and_then(Value::as_str).map(str::to_owned),
            status: optional_text(&body, "status")?,
            dependencies: string_list(&body, "dependencies")?,
        }),
        "update" => WorkWrite::Mutate(WorkMutation::Update {
            result_schema: body.get("result_schema").cloned(),
            task_id: required_text(&body, "task_id")?,
            version: version(&body)?,
            title: optional_text(&body, "title")?,
            description: optional_text(&body, "description")?,
            owner: body
                .get("owner")
                .map(|value| value.as_str().map(str::to_owned)),
            status: optional_text(&body, "status")?,
            dependencies: body
                .get("dependencies")
                .map(|_| string_list(&body, "dependencies"))
                .transpose()?,
        }),
        "cancel" => WorkWrite::Mutate(WorkMutation::Cancel {
            task_id: required_text(&body, "task_id")?,
            version: version(&body)?,
            reason: required_text(&body, "reason")?,
        }),
        "reject" => WorkWrite::Mutate(WorkMutation::Reject {
            task_id: required_text(&body, "task_id")?,
            version: version(&body)?,
            reason: review_result(&body)?,
        }),
        "claim" => WorkWrite::Claim(ClaimWork {
            task_id: required_text(&body, "task_id")?,
            version: version(&body)?,
            claimant_id: body
                .get("owner")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| agent.clone()),
        }),
        "submit" => WorkWrite::Submit(SubmitWork {
            task_id: required_text(&body, "task_id")?,
            version: version(&body)?,
            result: body.get("result").cloned().ok_or_else(|| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "Supply result for this task action",
                )
            })?,
            checks: required_text(&body, "checks")?,
            revision: required_text(&body, "revision")?,
            files: string_list(&body, "files")?,
            prepared_files: None,
            state_dir: state.state_dir.clone().into(),
            vm_evidence: None,
            vm_evidence_request_id: None,
        }),
        "accept" => WorkWrite::Accept(AcceptWork {
            task_id: required_text(&body, "task_id")?,
            version: version(&body)?,
            reason: review_result(&body)?,
            merged_result_id: None,
        }),
        _ => return Err(invalid_request()),
    };
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
    let agent_id = agent.to_owned();
    let task_id = input.task_id.clone();
    let expected_version = input.version;
    let request = request_id.map(str::to_owned);
    let request_signature = signature.to_owned();
    let reason = input.reason.clone();
    let preparation = prepare(
        &store,
        &agent_id,
        &task_id,
        expected_version,
        request.as_deref(),
        &request_signature,
        &reason,
    )
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
            let response = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-http/routes/work_write",
                move || {
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
            let response = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-http/routes/work_write",
                move || {
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
            let evidence = serde_json::json!({
                "layrProjectId": response.get("projectId").cloned().unwrap_or(Value::Null),
                "layrLine": response.get("line").cloned().unwrap_or(Value::Null),
                "layrStateId": expected_revision,
            });
            if evidence["layrProjectId"].as_str().is_none()
                || evidence["layrLine"].as_str().is_none()
            {
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
    let request_id_for_archive = request_id.clone();
    let committed_wake = wake.clone();
    let ((result, checks, rule_now), _) = store
        .transaction_with_post_commit(
            move |tx| -> Result<_, WorkCommitError> {
                let result = if let Some(actor_id) = actor_id.as_deref() {
                    work_write::write_action_as_agent(
                        tx,
                        &agent,
                        actor_id,
                        request_id.as_deref(),
                        &signature,
                        command,
                    )?
                } else {
                    work_write::write(tx, &agent, request_id.as_deref(), &signature, command)?
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
        return finalize_accepted_archive(&state, result.record, request_id_for_archive.as_deref())
            .await;
    }
    Ok(result.record)
}

enum AcceptedArchivePreparation {
    Finished(Value, bool),
    Archive {
        owner_id: String,
        root_id: String,
        lead_epoch: u64,
    },
}

/// A host owner's working directory and result revision, checked with Git after the read.
type HostGitCheck = (String, String);

async fn finalize_accepted_archive(
    state: &HttpState,
    record: Value,
    request_id: Option<&str>,
) -> Result<Value, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let work_id = record["id"].as_str().unwrap_or_default().to_owned();
    let work_id_for_read = work_id.clone();
    let read = store.read_async("runtime-http/routes/work_write", move |connection| -> Result<(Value, AcceptedArchivePreparation, Option<HostGitCheck>), ApiError> {
        let work = studio_runtime_store::WorkItemsRepository::work_record(connection, &work_id_for_read)
            .map_err(|_| ApiError::internal())?
            .ok_or_else(ApiError::internal)?;
        let owner_id = work["owner"].as_str().unwrap_or_default().to_owned();
        let root_id = work["rootId"].as_str().unwrap_or_default().to_owned();
        if owner_id.is_empty() || owner_id == root_id {
            return Ok((work, AcceptedArchivePreparation::Finished(serde_json::json!({"status":"skipped","reason":"The task owner is the lead or is unassigned"}), false), None));
        }
        let owner = studio_runtime_store::AgentRepository::get(
            connection,
            &studio_runtime_store::AgentId::from(owner_id.as_str()),
        )?.ok_or_else(ApiError::internal)?;
        let lead = studio_runtime_store::AgentRepository::get(
            connection,
            &studio_runtime_store::AgentId::from(root_id.as_str()),
        )?.ok_or_else(ApiError::internal)?;
        let owner_value = owner.value();
        let open_tasks = studio_runtime_store::WorkItemsRepository::work(connection)?
            .into_iter()
            .filter_map(|item| item.record.and_then(|raw| serde_json::from_str::<Value>(&raw).ok()))
            .filter(|item| item["owner"] == owner_id && !matches!(item["status"].as_str(), Some("accepted" | "cancelled")))
            .filter_map(|item| item["id"].as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        if !open_tasks.is_empty() {
            return Ok((work, AcceptedArchivePreparation::Finished(serde_json::json!({"status":"kept","reason":"The owner has another open task","tasks":open_tasks.into_iter().take(20).collect::<Vec<_>>(),"retryable":true}), true), None));
        }
        if owner_value.get("deletedAt").is_some_and(|value| !value.is_null()) {
            return Ok((work, AcceptedArchivePreparation::Finished(serde_json::json!({"status":"kept","reason":"The owner is already removed"}), false), None));
        }
        if owner_value.get("remoteWorker").is_some_and(|value| {
            !value.is_null() && value != false && value != ""
        }) {
            return Ok((work, AcceptedArchivePreparation::Finished(serde_json::json!({"status":"kept","reason":"The remote worker workspace requires review on its own server"}), false), None));
        }
        let latest = work["results"].as_array().and_then(|values| values.last()).cloned().unwrap_or(Value::Null);
        let revision = latest["revision"].as_str().unwrap_or_default();
        let execution_mode = owner_value.get("executionMode").and_then(Value::as_str).unwrap_or_default();
        // A host owner's Git checks run after this read ends.
        let host_check = (execution_mode != "vm").then(|| {
            (
                owner_value.get("cwd").and_then(Value::as_str).unwrap_or_default().to_owned(),
                revision.to_owned(),
            )
        });
        if host_check.is_none() && !studio_runtime_services::work_write::vm_merge_confirmed(
            connection,
            &work_id_for_read,
            latest["id"].as_str().unwrap_or_default(),
            revision,
        )
        .map_err(write_error)?
        {
            return Ok((work, AcceptedArchivePreparation::Finished(serde_json::json!({"status":"kept","reason":"The layr result merge is not confirmed"}), false), None));
        }
        Ok((work, AcceptedArchivePreparation::Archive {
            owner_id,
            root_id,
            lead_epoch: u64::try_from(lead.epoch).map_err(|_| ApiError::internal())?,
        }, host_check))
    }).await?;
    let result = match read {
        (work, archive @ AcceptedArchivePreparation::Archive { .. }, Some((cwd, revision))) => {
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-http/routes/work_write",
                move || {
                    #[allow(clippy::disallowed_methods, reason = "repository validation runs on the labeled work_write blocking worker")]
                    let directory_exists = std::path::Path::new(&cwd).is_dir();
                    if cwd.is_empty()
                        || !directory_exists
                        || !work_write::accepted_revision_is_on_local_main(&cwd, &revision)
                    {
                        let remote_origin =
                            !cwd.is_empty() && work_write::repository_has_origin(&cwd);
                        let reason = if remote_origin {
                            "Remote-origin main verification requires cross-server/remote-transport"
                        } else {
                            "The result commit is not reachable from local main"
                        };
                        return (
                            work,
                            AcceptedArchivePreparation::Finished(
                                serde_json::json!({"status":"kept","reason":reason}),
                                false,
                            ),
                        );
                    }
                    (work, archive)
                },
            )
            .await
            .map_err(|_| ApiError::internal())?
        }
        (work, preparation, _) => (work, preparation),
    };
    let (mut work, outcome, retryable) = match result {
        (work, AcceptedArchivePreparation::Finished(outcome, retryable)) => {
            (work, outcome, retryable)
        }
        (
            work,
            AcceptedArchivePreparation::Archive {
                owner_id,
                root_id,
                lead_epoch,
            },
        ) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            work_write::begin_accepted_archive_attempt(&store, &work_id, now)
                .await
                .map_err(write_error)?;
            let archived = studio_runtime_services::agent_management_store::execute(
                store.clone(),
                &studio_runtime_services::agent_management::Action::Archive {
                    actor: root_id,
                    epoch: i64::try_from(lead_epoch).map_err(|_| ApiError::internal())?,
                    target: owner_id,
                    reason: "Accepted task result is on main".to_owned(),
                    unassign_work: false,
                },
                now,
            )
            .await;
            let outcome = match archived {
                Ok(value) => value,
                Err(error) => serde_json::json!({
                    "status":"kept",
                    "reason":format!("The archive check failed: {error}"),
                }),
            };
            let retryable = outcome["status"] == "blocked"
                || outcome["retryable"] == true
                || outcome["cleanupPending"] == true;
            (work, outcome, retryable)
        }
    };
    let archive_result_id = work["decisions"]
        .as_array()
        .and_then(|values| values.last())
        .and_then(|decision| decision["resultId"].as_str())
        .unwrap_or("accepted")
        .to_owned();
    work["archive"] = outcome.clone();
    let updated = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    if let Some(object) = work.as_object_mut() {
        object.remove("archivePending");
    }
    if retryable {
        let intent_id = format!("{work_id}:{archive_result_id}");
        work["archiveIntent"] = serde_json::json!({
            "id": intent_id,
            "status": "pending",
            "owner": work["owner"],
            "attempts": 0,
            "created": updated,
        });
    } else if work.get("archiveIntent").is_some() {
        let intent_status = if work["archive"]["status"] == "archived" {
            "complete"
        } else {
            "terminal"
        };
        let archive_outcome = work["archive"].clone();
        if let Some(intent) = work.get_mut("archiveIntent") {
            intent["status"] = serde_json::json!(intent_status);
            intent["updated"] = serde_json::json!(updated);
            intent["outcome"] = archive_outcome;
        }
    }
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let request = request_id.map(str::to_owned);
    let committed_wake = state.event_wake.clone();
    let ((response, _event_inserted), _) = store
        .transaction_with_post_commit(
            move |tx| -> Result<_, studio_runtime_store::StoreError> {
                studio_runtime_store::WorkItemsRepository::update_work(tx, &work_id, &work)?;
                let attempt_status = if retryable {
                    "retryable"
                } else if work["archive"]["status"] == "archived" {
                    "complete"
                } else if work["archive"]["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.starts_with("The archive check failed:"))
                {
                    "unknown"
                } else {
                    "terminal"
                };
                let actor_epoch = studio_runtime_store::AgentRepository::get(
                    tx,
                    &studio_runtime_store::AgentId::from(
                        work["rootId"].as_str().unwrap_or_default(),
                    ),
                )?
                .map(|actor| actor.epoch)
                .unwrap_or_default();
                if work["owner"]
                    .as_str()
                    .is_some_and(|owner| owner != work["rootId"].as_str().unwrap_or_default())
                {
                    let attempt_receipt_id =
                        format!("work-archive:{}:{archive_result_id}", work_id);
                    let prior_attempt = tx.query_row(
                        "SELECT result FROM runtime_operation_receipts WHERE id=?1",
                        [attempt_receipt_id.as_str()],
                        |row| row.get::<_, String>(0),
                    )?;
                    let prior_attempt =
                        serde_json::from_str::<Value>(&prior_attempt).map_err(|_| {
                            studio_runtime_store::StoreError::RecordContract(
                                "archive attempt receipt",
                            )
                        })?;
                    let attempts = prior_attempt["attempts"].as_u64().unwrap_or_default();
                    let delay = 2_u64.pow(attempts.min(6) as u32).min(60);
                    let mut attempt_receipt = prior_attempt;
                    attempt_receipt["status"] = Value::String(if retryable {
                        "pending".to_owned()
                    } else {
                        attempt_status.to_owned()
                    });
                    attempt_receipt["taskId"] = serde_json::json!(work_id);
                    attempt_receipt["intentId"] =
                        serde_json::json!(format!("{}:{archive_result_id}", work_id));
                    attempt_receipt["actor"] = work["rootId"].clone();
                    attempt_receipt["actorEpoch"] = serde_json::json!(actor_epoch);
                    attempt_receipt["target"] = work["owner"].clone();
                    attempt_receipt["attempts"] = serde_json::json!(attempts);
                    attempt_receipt["nextAttemptAt"] =
                        serde_json::json!(updated + if retryable { delay as f64 } else { 60.0 });
                    attempt_receipt["lastOutcome"] = work["archive"].clone();
                    attempt_receipt["updated"] = serde_json::json!(updated);
                    if !retryable {
                        attempt_receipt["outcome"] = work["archive"].clone();
                    }
                    tx.execute(
                        "UPDATE runtime_operation_receipts SET result=?1 WHERE id=?2",
                        [attempt_receipt.to_string(), attempt_receipt_id],
                    )?;
                }
                let mut event_inserted = false;
                if outcome_status_is_terminal(&work)
                    && work["archive"]["status"] != "archived"
                    && let Some(owner_id) = work["owner"].as_str()
                    && owner_id != work["rootId"].as_str().unwrap_or_default()
                    && let Some(owner) = studio_runtime_store::AgentRepository::get(
                        tx,
                        &studio_runtime_store::AgentId::from(owner_id),
                    )?
                    && owner.extra.get("deletedAt").is_none_or(Value::is_null)
                {
                    let decision = work["decisions"]
                        .as_array()
                        .and_then(|values| values.last())
                        .cloned()
                        .unwrap_or(Value::Null);
                    event_inserted = studio_runtime_services::event_write::enqueue_recovery_event(
                        tx,
                        &owner,
                        format!(
                            "work-decision:{}:{}",
                            work_id,
                            work["version"].as_i64().unwrap_or_default() - 1
                        ),
                        "work_decision",
                        serde_json::to_string(&serde_json::json!({
                            "task": work_id,
                            "decision": "accept",
                            "reason": decision["reason"],
                        }))?,
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs_f64(),
                    )
                    .map_err(|_| {
                        studio_runtime_store::StoreError::RecordContract("work decision event")
                    })?;
                }
                let mut response = studio_runtime_services::work_write::work_item_view(tx, &work)?;
                response["archive"] = work["archive"].clone();
                response["archivePending"] = Value::Bool(retryable);
                if let Some(object) = response.as_object_mut() {
                    object.remove("archiveIntent");
                }
                let receipt_result = serde_json::to_string(&response).map_err(|_| {
                    studio_runtime_store::StoreError::RecordContract("work response")
                })?;
                if let Some(request_id) = request {
                    tx.execute(
                        "UPDATE runtime_operation_receipts SET result=?1 WHERE id=?2",
                        (&receipt_result, request_id),
                    )?;
                }
                Ok((response, event_inserted))
            },
            move |(_, event_inserted)| {
                if *event_inserted {
                    committed_wake.notify_one();
                }
            },
        )
        .await
        .map_err(|_| ApiError::internal())?;
    Ok(response)
}

fn outcome_status_is_terminal(work: &Value) -> bool {
    work["archive"]["retryable"] != true && work["archive"]["cleanupPending"] != true
}

fn commit_error(error: WorkCommitError) -> ApiError {
    match error {
        WorkCommitError::Store => ApiError::internal(),
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

fn write_error(error: WorkWriteError) -> ApiError {
    match error {
        WorkWriteError::Event(error) => work_items::write_error(error),
        WorkWriteError::PreparationWorker => ApiError::internal(),
        WorkWriteError::Store(_) => ApiError::internal(),
        WorkWriteError::NotImplemented(dependency) => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            format!("Not implemented: requires {dependency}"),
        ),
    }
}

/// Python `codex_work.py` refuses a review decision without its text by this name.
const REVIEW_RESULT_REQUIRED: &str =
    "Supply result with 1 to 32000 characters for accept or reject";

fn required_text(body: &Value, field: &str) -> Result<String, ApiError> {
    body.get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            // Name the field so the caller can correct the request.
            ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("Supply {field} as text for this task action"),
            )
        })
}

/// The review decision text of accept and reject.
fn review_result(body: &Value) -> Result<String, ApiError> {
    body.get("result")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, REVIEW_RESULT_REQUIRED))
}

fn optional_text(body: &Value, field: &str) -> Result<Option<String>, ApiError> {
    body.get(field)
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(invalid_request)
        })
        .transpose()
}

fn string_list(body: &Value, field: &str) -> Result<Vec<String>, ApiError> {
    match body.get(field) {
        None => Ok(Vec::new()),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(invalid_request)
            })
            .collect(),
        Some(_) => Err(invalid_request()),
    }
}

fn version(body: &Value) -> Result<Option<i64>, ApiError> {
    let Some(value) = body.get("version") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let number = value.as_number().ok_or_else(invalid_request)?;
    if let Some(value) = number.as_i64() {
        return Ok(Some(value));
    }
    let value = number.as_f64().ok_or_else(invalid_request)?;
    if value.is_finite()
        && value.fract() == 0.0
        && value >= i64::MIN as f64
        && value < 9_223_372_036_854_775_808.0
    {
        Ok(Some(value as i64))
    } else {
        Err(invalid_request())
    }
}

fn invalid_request() -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "Invalid request")
}

#[cfg(test)]
mod tests {
    use super::version;
    use serde_json::json;

    #[test]
    fn integral_decimal_version_keeps_its_stale_value() {
        assert!(matches!(version(&json!({"version": 0.0})), Ok(Some(0))));
        assert!(version(&json!({"version": 1.5})).is_err());
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
                    json!({"id":"lead","rootId":"lead","parentId":null,"epoch":1,
                        "status":"running","autoWake":true,"isLead":true,"name":"lead"}),
                    json!({"id":"worker","rootId":"lead","parentId":"lead","epoch":1,
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
        (state, store, directory)
    }

    fn written(outcome: WorkActionOutcome) -> Value {
        match outcome {
            WorkActionOutcome::Written(record) => record,
            other => panic!("expected a written task, got {other:?}"),
        }
    }

    fn schema() -> Value {
        json!({"type":"object","properties":{"count":{"type":"integer"}},"required":["count"],"additionalProperties":false})
    }

    fn refused(outcome: WorkActionOutcome) -> String {
        match outcome {
            WorkActionOutcome::Refused(message) => message,
            other => panic!("expected refusal, got {other:?}"),
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
            let message = refused(port.act("lead", &json!({"action":"create","title":"invalid","result_schema":contract}), &format!("invalid-{index}")).await);
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
                "create",
            )
            .await,
        );
        assert_eq!(created["resultSchema"], schema());
        let task = created["id"].as_str().expect("id");
        refused(
            port.act(
                "lead",
                &json!({"action":"update","task_id":task,"result_schema":{"properties":false}}),
                "invalid-update",
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
        let created = written(port.act("lead", &json!({"action":"create","title":"results","owner":"worker","result_schema":schema()}), "create").await);
        let task = created["id"].as_str().expect("id");
        let before = created.clone();
        let args = json!({"action":"submit","task_id":task,"result":{"count":"bad"},"checks":"checked","revision":"abc","files":[]});
        let message = refused(port.act("worker", &args, "invalid-submit").await);
        let error: Value = serde_json::from_str(&message).expect("structured refusal");
        assert_eq!(error["errors"][0]["path"], "/count");
        assert_eq!(error["failedSubmits"], 1);
        assert_eq!(
            refused(port.act("worker", &args, "invalid-submit").await),
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
        assert_eq!(saved["version"], before["version"]);
        assert_eq!(saved["status"], before["status"]);
        assert_eq!(saved["results"], before["results"]);
        assert!(saved.get("submissionRefusal").is_none());
        let mut valid = args;
        valid["result"] = json!({"count":7});
        let submitted = written(port.act("worker", &valid, "valid-submit").await);
        assert_eq!(submitted["status"], "review");
        assert_eq!(
            written(port.act("worker", &valid, "valid-submit").await),
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
                "create",
            )
            .await,
        );
        let task = created["id"].as_str().expect("id");
        let mut args = json!({"action":"submit","task_id":task,"result":"not JSON","checks":"checked","revision":"abc"});
        for index in 0..MAX_FAILED_RESULT_SUBMITS_PER_ATTEMPT {
            let message = refused(port.act("worker", &args, &format!("invalid-{index}")).await);
            let refusal: Value = serde_json::from_str(&message).expect("refusal");
            assert_eq!(refusal["failedSubmits"], index + 1);
        }
        args["result"] = json!({"count":2});
        assert!(refused(port.act("worker", &args, "over-cap").await).contains("limit reached"));
        let events = store.read_async("test", |db| db.query_row("SELECT count(*) FROM runtime_events WHERE kind='work_result_invalid' AND agent='lead' AND status='pending'", [], |r| r.get::<_,i64>(0)).map_err(studio_runtime_store::StoreError::from)).await.expect("events");
        assert_eq!(events, 1);
        written(
            port.act(
                "lead",
                &json!({"action":"update","task_id":task,"result_schema":schema()}),
                "reset",
            )
            .await,
        );
        assert_eq!(
            written(port.act("worker", &args, "new-attempt").await)["status"],
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
                "create",
            )
            .await,
        );
        let args = json!({"action":"submit","task_id":created["id"],"result":"plain text","checks":"checked","revision":"abc"});
        let result = written(port.act("worker", &args, "submit").await);
        assert_eq!(result["results"][0]["text"], "plain text");
        assert!(result["results"][0].get("result").is_none());
    }

    #[tokio::test]
    async fn the_tool_port_runs_the_work_route_owner_with_the_tool_request_identity() {
        let (state, _store, _directory) = state().await;
        let port = managed_work_port(state, None);
        let created = written(
            port.act(
                "lead",
                &json!({"action":"create","title":"Port check","owner":"worker"}),
                "tool-create",
            )
            .await,
        );
        let task = created["id"].as_str().expect("task id").to_owned();
        let claim = json!({"action":"claim","task_id":task,"version":created["version"]});
        let claimed = written(port.act("worker", &claim, "tool-claim").await);
        assert_eq!(claimed["owner"], "worker");
        assert_eq!(claimed["status"], "running");
        // The same tool request replays its receipt instead of acting again.
        assert_eq!(
            written(port.act("worker", &claim, "tool-claim").await),
            claimed
        );
        // A refusal before any write reaches the model as a refusal.
        let refused = port
            .act(
                "worker",
                &json!({"action":"claim","task_id":"missing-task","version":1}),
                "tool-claim-missing",
            )
            .await;
        assert!(
            matches!(refused, WorkActionOutcome::Refused(_)),
            "{refused:?}"
        );
    }

    #[tokio::test]
    async fn authenticated_task_caller_cannot_submit_another_owners_task() {
        let (state, store, _directory) = state().await;
        let port = managed_work_port(state, None);
        let created = written(
            port.act(
                "lead",
                &json!({
                    "action":"create", "title":"Lead task", "owner":"lead"
                }),
                "auth-create-owned",
            )
            .await,
        );
        let task = created["id"].as_str().unwrap().to_owned();
        let submit = json!({"action":"submit", "task_id":task,
            "result":"done", "checks":"checked", "revision":"abc1234"});
        let outcome = port.act("worker", &submit, "auth-spoof-submit").await;
        assert!(
            matches!(&outcome, WorkActionOutcome::Refused(message)
            if message.contains("Only the assigned worker")),
            "{outcome:?}"
        );
        let task_id = task.clone();
        let unchanged = store
            .read_async("auth-test/owner", move |tx| {
                studio_runtime_store::WorkItemsRepository::work_record(tx, &task_id)
            })
            .await
            .unwrap()
            .unwrap();
        assert!(unchanged["results"].as_array().unwrap().is_empty());
        let submitted = written(port.act("lead", &submit, "auth-owner-submit").await);
        assert_eq!(submitted["results"][0]["agent"], "lead");
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
                    &format!("auth-create-{action}"),
                )
                .await,
            );
            let task = created["id"].as_str().unwrap().to_owned();
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
                        &format!("auth-submit-{action}"),
                    )
                    .await,
                )
            };
            let args = json!({"action":action,"task_id":task,"result":"reviewed",
                "reason":"cancelled", "title":"Changed title"});
            let refused = port
                .act("worker", &args, &format!("auth-worker-{action}"))
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
                .unwrap()
                .unwrap();
            assert_eq!(unchanged["version"], submitted["version"], "{action}");
            assert_eq!(unchanged["status"], submitted["status"], "{action}");
            let allowed = written(
                port.act("lead", &args, &format!("auth-lead-{action}"))
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
                "auth-worker-create",
            )
            .await,
        );
        let creator_task = created["id"].as_str().unwrap().to_owned();
        store
            .transaction(move |tx| {
                let mut record =
                    studio_runtime_store::WorkItemsRepository::work_record(tx, &creator_task)?
                        .unwrap();
                record["createdBy"] = json!("worker");
                studio_runtime_store::WorkItemsRepository::update_work(tx, &creator_task, &record)
            })
            .await
            .unwrap();
        let cancelled = written(
            port.act(
                "worker",
                &json!({"action":"cancel",
            "task_id":created["id"],"reason":"No longer needed"}),
                "auth-creator-cancel",
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
                "auth-receipt-create",
            )
            .await,
        );
        let task = &created["id"];
        written(
            port.act(
                "worker",
                &json!({"action":"submit", "task_id":task,
            "result":"done", "checks":"checked", "revision":"abc1234"}),
                "auth-receipt-submit",
            )
            .await,
        );
        let args = json!({"action":"accept", "task_id":task, "result":"reviewed",
            "agent":"worker", "id":"auth-user-accept"});
        let accepted = super::perform(state, None, args.clone(), None)
            .await
            .unwrap();
        assert_eq!(accepted["status"], "accepted");
        let refused = port.act("worker", &args, "auth-user-accept").await;
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
                "tool-create-named",
            )
            .await,
        );
        let task = created["id"].as_str().expect("task id").to_owned();
        let accept = port
            .act(
                "lead",
                &json!({"action":"accept","task_id":task}),
                "tool-accept-without-result",
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
                "tool-submit-without-checks",
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
