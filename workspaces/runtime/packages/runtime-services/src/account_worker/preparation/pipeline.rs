//! The only adapter allowed to advance a claimed batch to provider submission.
use super::*;
use crate::diagnostics::measure_runtime_stage as stage;
use crate::{submit_gates as gates, thread_prepare as thread, turn_prepare as turn};
use snapshot::*;
use std::path::{Path, PathBuf};
use std::time::Instant;
use studio_runtime_store::Transaction;

pub(crate) struct CommitGuard {
    native_params: Option<Value>,
    selection: gates::ExecutionSelection,
    token: crate::workspace_ready::ReadyToken,
    resources: PreparationResources,
    thread: thread::Snapshot,
    compiler_snapshot: Option<turn::PreparationSnapshot>,
    state_root: std::path::PathBuf,
    filesystem: FilesystemSnapshot,
    prepared_host: Option<crate::host_project_runtime::PreparedHost>,
}
#[derive(Clone)]
pub(crate) struct Admission {
    selection: gates::ExecutionSelection,
}
pub(crate) struct RequestPreparation {
    pub intent: TurnIntent,
    pub method: Method,
    pub params: Value,
    pub thread: Option<thread::PreparedAttempt>,
    pub compiled: Option<turn::PreparedSubmission>,
    pub submission: Option<crate::notification_dispatch::PreparedSubmission>,
    pub guard: CommitGuard,
    pub connection_id: String,
}
impl RequestPreparation {
    /// The workspace token this request's commit validates.
    pub(crate) fn workspace_token(&self) -> crate::workspace_ready::ReadyToken {
        self.guard.token.clone()
    }
}

async fn compile_turn(
    snapshot: turn::PreparationSnapshot,
    events: Vec<turn::Event>,
) -> Result<
    (
        turn::PreparationSnapshot,
        Vec<turn::Event>,
        Result<turn::PreparedSubmission, turn::PrepareError>,
    ),
    WorkerError,
> {
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/account_worker_turn_compile",
        move || {
            let result = turn::compile(&snapshot, &events);
            (snapshot, events, result)
        },
    )
    .await
    .map_err(|_| WorkerError::TaskFailed)
}

fn catalog(
    resources: &PreparationResources,
    intent: &TurnIntent,
) -> Result<(gates::CatalogIdentity, Option<gates::CatalogObservation>), WorkerError> {
    use crate::model_catalog::AdmissionCatalogRead;
    match resources.model_catalog.read_for_admission(
        &intent.account_key,
        &resources.account_home,
        intent.provider.key(),
    ) {
        AdmissionCatalogRead::Fresh {
            value,
            account_key,
            home,
            provider,
            generation,
            expires_at,
        } => {
            let identity = gates::CatalogIdentity {
                account_key,
                home,
                provider,
                generation,
            };
            let models = value["data"]
                .as_array()
                .ok_or_else(|| invalid("model catalog"))?
                .iter()
                .map(|v| {
                    Ok(gates::ModelCapabilities {
                        model: v["model"]
                            .as_str()
                            .ok_or_else(|| invalid("catalog/model"))?
                            .into(),
                        resolved_model: v
                            .get("resolvedModel")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        is_default: v.get("isDefault").and_then(Value::as_bool).unwrap_or(false),
                        hidden: v.get("hidden").and_then(Value::as_bool).unwrap_or(false),
                        access_programs: v
                            .pointer("/availableAccessPrograms/cyber")
                            .and_then(Value::as_array)
                            .map(|values| {
                                values
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_owned)
                                    .collect()
                            }),
                        supported_efforts: v
                            .get("supportedReasoningEfforts")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| {
                                        v["reasoningEffort"].as_str().map(str::to_owned)
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                        default_effort: v
                            .get("defaultReasoningEffort")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        priority: v
                            .get("serviceTiers")
                            .and_then(Value::as_array)
                            .is_some_and(|a| a.iter().any(|v| v["id"] == "priority")),
                    })
                })
                .collect::<Result<Vec<_>, WorkerError>>()?;
            Ok((
                identity.clone(),
                Some(gates::CatalogObservation {
                    identity,
                    expires_at,
                    models,
                }),
            ))
        }
        _ => Ok((
            gates::CatalogIdentity {
                account_key: intent.account_key.clone(),
                home: resources.account_home.clone(),
                provider: intent.provider.key().to_owned(),
                generation: 0,
            },
            None,
        )),
    }
}
fn preconditions(
    tx: &Transaction<'_>,
    intent: &TurnIntent,
    filesystem: &FilesystemSnapshot,
    prepared_host: Option<&crate::host_project_runtime::PreparedHost>,
) -> Result<Value, WorkerError> {
    preconditions_for_purpose(tx, intent, filesystem, false, prepared_host)
}
fn preconditions_for_purpose(
    tx: &Transaction<'_>,
    intent: &TurnIntent,
    filesystem: &FilesystemSnapshot,
    allow_paused_start: bool,
    prepared_host: Option<&crate::host_project_runtime::PreparedHost>,
) -> Result<Value, WorkerError> {
    let record = agent(tx, &intent.agent_id)?.value();
    let capacity_retry = record["startAttempt"]["action"] == "capacity"
        && record["startAttempt"]["capacityRetryId"] == record["capacityRetry"]["id"]
        && intent.attempt_id.starts_with("capacity:");
    if record["epoch"] != intent.epoch
        || (!allow_paused_start
            && (record["startAttempt"]["id"] != intent.attempt_id
                || record["startAttempt"]["submitted"] != false
                || record["autoWake"] != true))
        || (record["nativeFailureHold"] == true && !capacity_retry)
        || native_thread_blocked(&record)
        || context_repair_blocked(&record)
    {
        return Err(refusal("preparation/ownership-changed".into()));
    }
    if record
        .get("claudePreInputRetry")
        .is_some_and(|v| !v.is_null())
    {
        return Err(refusal(
            "preparation/claude-frozen-input-recovery-required".into(),
        ));
    }
    gates::budget_admission(record.get("tokenBudget")).map_err(|e| refusal(e.to_string()))?;
    authorize(tx, intent, filesystem, prepared_host)?;
    selected_inputs_current(tx, intent)?;
    Ok(record)
}
/// Refuse a claim whose stored input rows changed after the claim. The claim
/// carries the model projection of each row, so the stored text is compared
/// through that same projection.
fn selected_inputs_current(tx: &Transaction<'_>, intent: &TurnIntent) -> Result<(), WorkerError> {
    for selected in &intent.events {
        let current = studio_runtime_store::QueueEventRepository::get(
            tx,
            &studio_runtime_store::QueueEventId::from(selected.id.as_str()),
        )?;
        let metadata =
            studio_runtime_store::NotificationDispatchRepository::event_metadata(tx, &selected.id)?
                .unwrap_or_else(|| json!({}));
        if current.is_none_or(|event| {
            event.agent.0 != intent.agent_id
                || event.epoch != selected.epoch
                || event.kind != selected.kind
                || SchedulerRepository::claimed_event_text(&event.text, &metadata) != selected.text
                || event.created != selected.created
                || event.error != selected.error
                || event.status != "reserved"
        }) {
            return Err(refusal("preparation/input-changed".into()));
        }
        if SchedulerRepository::input_metadata_identity(&metadata)
            != SchedulerRepository::input_metadata_identity(&selected.metadata)
        {
            return Err(refusal("preparation/input-metadata-changed".into()));
        }
    }
    Ok(())
}
/// Refusal of a ready snapshot whose workspace token no longer validates.
pub(crate) const WORKSPACE_TOKEN_STALE: &str = "workspace-readiness/token-stale";
/// Pending reason of a turn that waits for a model catalog refresh.
pub(crate) const CATALOG_PENDING_REASON: &str = "model-catalog/refresh-pending";
fn catalog_pending() -> WorkerError {
    WorkerError::PreparationPending(CATALOG_PENDING_REASON.into())
}
/// The admission of a record whose settings an accepted thread commit froze:
/// it needs no catalog read and no transaction of its own.
fn frozen_selection(
    record: &Value,
    identity: &gates::CatalogIdentity,
) -> Result<gates::ExecutionSelection, WorkerError> {
    let observed = gates::ExecutionSnapshot::from_record(record, identity.clone())
        .map_err(|e| refusal(e.to_string()))?;
    if !observed.settings_fixed {
        return Err(refusal("execution-settings/not-frozen".into()));
    }
    let selection = gates::select_execution(&observed, &observed, None, Instant::now())
        .map_err(|e| refusal(e.to_string()))?;
    if matches!(
        selection.settings.native_effort,
        gates::StoredField::Missing
    ) {
        return Err(refusal("execution-settings/native-effort-missing".into()));
    }
    Ok(selection)
}
fn catalog_admission_error(error: gates::ExecutionDenial) -> WorkerError {
    match error {
        gates::ExecutionDenial::CatalogRequired | gates::ExecutionDenial::CatalogExpired => {
            catalog_pending()
        }
        _ => refusal(error.to_string()),
    }
}

/// Match Python's `turn_program` plus `turn_params`: disabled Daybreak always
/// submits `standard`; enabled Daybreak resolves grants for the effective
/// model selected by preparation, never the stale program stored on the agent.
fn daybreak_program(
    resources: &PreparationResources,
    snapshot: &thread::Snapshot,
) -> Result<Option<String>, WorkerError> {
    if !crate::provider_adapter::adapter(snapshot.account.provider)
        .capabilities()
        .daybreak
    {
        if snapshot.settings.daybreak_enabled == thread::Stored::Present(Some(true)) {
            return Err(refusal("daybreak/provider-unsupported".into()));
        }
        return Ok(None);
    }
    let enabled = match &snapshot.settings.daybreak_enabled {
        thread::Stored::Present(Some(enabled)) => *enabled,
        thread::Stored::Present(None) | thread::Stored::Absent => false,
    };
    if !enabled {
        return Ok(Some("standard".into()));
    }
    let catalog = match resources.model_catalog.read_for_admission(
        &snapshot.account.key,
        &resources.account_home,
        "codex",
    ) {
        crate::model_catalog::AdmissionCatalogRead::Fresh { value, .. } => value,
        crate::model_catalog::AdmissionCatalogRead::Pending => return Err(catalog_pending()),
        crate::model_catalog::AdmissionCatalogRead::Unavailable => {
            return Err(refusal("daybreak/model-catalog-unavailable".into()));
        }
    };
    resolve_daybreak_program(&catalog, snapshot.settings.model.as_deref(), true).map(Some)
}

fn resolve_daybreak_program(
    catalog: &Value,
    model: Option<&str>,
    enabled: bool,
) -> Result<String, WorkerError> {
    if !enabled {
        return Ok("standard".into());
    }
    let programs = catalog
        .get("data")
        .and_then(Value::as_array)
        .and_then(|rows| {
            rows.iter().find(|row| {
                row.get("model").and_then(Value::as_str) == model
                    && row.get("hidden").and_then(Value::as_bool) != Some(true)
            })
        })
        .and_then(|row| row.get("availableAccessPrograms"))
        .and_then(|value| value.get("cyber"))
        .and_then(Value::as_array);
    let Some(programs) = programs else {
        return Err(refusal("Daybreak is not available for this model on this account. Choose another model or turn off Daybreak".into()));
    };
    if programs.iter().any(|program| !program.is_string()) {
        return Err(refusal("Daybreak model catalog is invalid".into()));
    }
    for candidate in ["daybreakBlue", "daybreakRed"] {
        if programs.iter().any(|program| program == candidate) {
            return Ok(candidate.into());
        }
    }
    Err(refusal("Daybreak is not available for this model on this account. Choose another model or turn off Daybreak".into()))
}

/// Native start params resolve Daybreak separately against the selected model.
/// Temporarily clear the planner's legacy unconditional refusal, then restore
/// the recorded setting before emitting provider params.
fn plan_native_thread(
    snapshot: &mut thread::Snapshot,
    attempt_id: &str,
    transport_ticket: &str,
) -> thread::Outcome {
    let daybreak = std::mem::replace(
        &mut snapshot.settings.daybreak_enabled,
        thread::Stored::Present(Some(false)),
    );
    let outcome = thread::plan(snapshot, attempt_id, transport_ticket);
    snapshot.settings.daybreak_enabled = daybreak;
    outcome
}

/// Build native action/retry turn settings with the Daybreak value resolved for
/// the selected model. The generic planner refuses enabled Daybreak because
/// ordinary turns resolve it elsewhere; temporarily clear that legacy gate,
/// then restore the stored setting and apply the resolved provider value.
fn native_turn_params(
    snapshot: &mut thread::Snapshot,
    input: &thread::TurnInput,
    program: Option<&str>,
) -> Result<serde_json::Map<String, Value>, WorkerError> {
    let enabled = std::mem::replace(
        &mut snapshot.settings.daybreak_enabled,
        thread::Stored::Present(Some(false)),
    );
    let params = thread::turn_params(snapshot, input);
    snapshot.settings.daybreak_enabled = enabled;
    let mut params = params.map_err(|reason| refusal(format!("thread-preparation/{reason:?}")))?;
    if let Some(program) = program {
        params.insert("cyberAccessProgram".into(), json!(program));
    }
    Ok(params)
}

pub(crate) async fn admit(
    store: Arc<RuntimeStore>,
    resources: &PreparationResources,
    intent: &TurnIntent,
) -> Result<Admission, WorkerError> {
    admit_for_purpose(store, resources, intent, false).await
}
async fn admit_for_purpose(
    store: Arc<RuntimeStore>,
    resources: &PreparationResources,
    intent: &TurnIntent,
    allow_paused_start: bool,
) -> Result<Admission, WorkerError> {
    let preparation_store = store.clone();
    let admission_intent = intent.clone();
    let radio_authorized = intent.radio_authorized;
    let (filesystem, prepared_host) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/account_worker_admission_snapshot",
        move || {
            let filesystem = FilesystemSnapshot::capture_for_authorization(
                &preparation_store,
                &admission_intent,
            )?;
            let prepared_host = if radio_authorized {
                let prepared = crate::host_project_runtime::PreparedHost::load(&preparation_store)
                    .map_err(|error| refusal(error.to_string()))?;
                prepared
                    .recheck_paths()
                    .map_err(|error| refusal(error.to_string()))?;
                Some(prepared)
            } else {
                None
            };
            Ok::<_, WorkerError>((filesystem, prepared_host))
        },
    )
    .await
    .map_err(|_| WorkerError::TaskFailed)??;
    let (identity, observation) = catalog(resources, intent)?;
    let intent = intent.clone();
    // Validation only: one reader snapshot, and the submitted commit rechecks.
    store
        .read_async("runtime-services/account_worker_admission", move |tx| {
            let record = preconditions_for_purpose(
                tx,
                &intent,
                &filesystem,
                allow_paused_start,
                prepared_host.as_ref(),
            )?;
            let observed = gates::ExecutionSnapshot::from_record(&record, identity.clone())
                .map_err(|e| refusal(e.to_string()))?;
            let mut selected =
                gates::select_execution(&observed, &observed, observation.as_ref(), Instant::now())
                    .map_err(catalog_admission_error)?;
            // Python resolves absent nativeEffort against the account catalog before
            // preparing a thread. Explicit null remains a deliberate provider default.
            if matches!(selected.settings.native_effort, gates::StoredField::Missing) {
                let catalog = observation.as_ref().ok_or_else(catalog_pending)?;
                let (effort, native) =
                    gates::validate_execution(&selected.settings, catalog, false)
                        .map_err(|e| refusal(e.to_string()))?;
                selected.settings.effort = effort;
                selected.settings.native_effort =
                    native.map_or(gates::StoredField::Null, gates::StoredField::Value);
                selected.catalog_precondition =
                    Some((catalog.identity.clone(), catalog.expires_at));
            }
            Ok(Admission {
                selection: selected,
            })
        })
        .await
}
pub(crate) async fn workspace(
    store: Arc<RuntimeStore>,
    resources: &PreparationResources,
    intent: &TurnIntent,
) -> Result<crate::workspace_ready::ReadyToken, WorkerError> {
    workspace_with_vm_action(store, resources, intent, false).await
}

async fn workspace_with_vm_action(
    store: Arc<RuntimeStore>,
    resources: &PreparationResources,
    intent: &TurnIntent,
    allow_vm_native_action: bool,
) -> Result<crate::workspace_ready::ReadyToken, WorkerError> {
    let read_store = Arc::clone(&store);
    let agent_id = intent.agent_id.clone();
    let record = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/account_worker_workspace_agent",
        move || {
            let read = read_store.read()?;
            AgentRepository::get(&read, &AgentId::from(agent_id.as_str()))?
                .ok_or_else(|| invalid(&agent_id))
        },
    )
    .await
    .map_err(|_| WorkerError::TaskFailed)??;
    if record.extra.get("executionMode").and_then(Value::as_str) == Some("vm") {
        if record.extra.get("layrReady").and_then(Value::as_bool) != Some(true) {
            let start_attempt = record.extra.get("startAttempt").unwrap_or(&Value::Null);
            let attempt_id = start_attempt
                .get("id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| refusal("workspace-state/vm-start-attempt".into()))?;
            let action_request_id = start_attempt
                .get("actionRequestId")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("vm-turn-preparation:{attempt_id}"));
            let epoch = u64::try_from(record.epoch)
                .map_err(|_| refusal("workspace-state/vm-epoch".into()))?;
            #[cfg(unix)]
            let guest: Arc<dyn crate::linux_vm_guest::GuestRpc> =
                resources.guest_rpc.clone().unwrap_or_else(|| {
                    Arc::new(crate::linux_vm_guest::UnixGuestRpc::for_runtime_state(
                        store.state_dir(),
                    ))
                });
            #[cfg(not(unix))]
            let guest = resources.guest_rpc.clone().ok_or_else(|| {
                WorkerError::PreparationPending("workspace-readiness/vm-guest-unavailable".into())
            })?;
            // Python's `codex_runtime.py:6405-6413` calls VM preparation from
            // `prepare_locked` before connecting the provider for every VM turn.
            let prepared = if record.extra.get("isLead").and_then(Value::as_bool) == Some(true) {
                crate::vm_preparation::prepare_lead(
                    &store,
                    Arc::clone(&guest),
                    &intent.agent_id,
                    attempt_id,
                    &action_request_id,
                    epoch,
                )
                .await
            } else {
                crate::vm_preparation::prepare_child(
                    &store,
                    Arc::clone(&guest),
                    &intent.agent_id,
                    attempt_id,
                    &action_request_id,
                    epoch,
                )
                .await
            };
            prepared.map_err(|error| match error {
                error @ (crate::native_actions::NativeError::Unknown
                | crate::native_actions::NativeError::Unavailable(_)) => {
                    WorkerError::PreparationPending(format!("workspace-readiness/{error}"))
                }
                error => refusal(error.to_string()),
            })?;
        }
        let read_store = Arc::clone(&store);
        let agent_id = intent.agent_id.clone();
        let ready = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/account_worker_workspace_agent",
            move || {
                let connection = read_store.read()?;
                AgentRepository::get(&connection, &AgentId::from(agent_id.as_str()))?
                    .ok_or_else(|| invalid(&agent_id))
            },
        )
        .await
        .map_err(|_| WorkerError::TaskFailed)??;
        if ready.extra.get("layrReady").and_then(Value::as_bool) != Some(true) {
            return Err(WorkerError::PreparationPending(
                "workspace-readiness/vm-not-ready".into(),
            ));
        }
        // Native actions operate on the host project; guest turn execution uses
        // the guest's cwd. Keep the project binding frozen from creation time.
        let cwd = if allow_vm_native_action {
            ready
                .extra
                .get("hostProjectPath")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| refusal("workspace-readiness/vm-host-project-required".into()))?
        } else {
            ready
                .extra
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|value| value.starts_with('/'))
                .ok_or_else(|| refusal("workspace-state/vm-cwd".into()))?
        };
        return Ok(crate::workspace_ready::ReadyToken::new(
            &ready.value(),
            PathBuf::from(cwd),
        ));
    }
    // Git provisioning acquires its own short transactions. No store transaction is held here.
    match crate::workspace_ready_git::prepare(Arc::clone(&store), intent.agent_id.clone())
        .await
        .map_err(|e| refusal(e.to_string()))?
    {
        crate::workspace_ready::WorkspaceDecision::Ready { token } => {
            let read_store = Arc::clone(&store);
            let agent_id = intent.agent_id.clone();
            let root = store.root().to_path_buf();
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/account_worker_workspace_progress",
                move || {
                    let record = AgentRepository::get(
                        &*read_store.read()?,
                        &AgentId::from(agent_id.as_str()),
                    )?
                    .ok_or_else(|| invalid(&agent_id))?;
                    if record.extra.get("isLead") == Some(&json!(true)) {
                        if agent_id.is_empty()
                            || agent_id.len() > 200
                            || !agent_id
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                        {
                            return Err(invalid("progress agent"));
                        }
                        for path in [root.join("progress"), root.join("progress").join(&agent_id)] {
                            match std::fs::create_dir(&path) {
                                Ok(()) => {}
                                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                                Err(_) => {
                                    return Err(refusal(
                                        "workspace-readiness/progress-directory".into(),
                                    ));
                                }
                            }
                            if std::fs::canonicalize(&path).ok().as_ref() != Some(&path)
                                || !path.is_dir()
                            {
                                return Err(refusal(
                                    "workspace-readiness/progress-directory".into(),
                                ));
                            }
                        }
                    }
                    Ok::<_, WorkerError>(())
                },
            )
            .await
            .map_err(|_| WorkerError::TaskFailed)??;
            Ok(token)
        }
        crate::workspace_ready::WorkspaceDecision::Refuse(e) => Err(refusal(e.to_string())),
        _ => Err(WorkerError::PreparationPending(
            "workspace-readiness/pending".into(),
        )),
    }
}

pub(crate) async fn prepare(
    store: Arc<RuntimeStore>,
    resources: &PreparationResources,
    intent: &TurnIntent,
    method: Method,
    generation: u64,
) -> Result<RequestPreparation, WorkerError> {
    let connection_id = uuid::Uuid::new_v4().to_string();
    prepare_with_connection_id(store, resources, intent, method, generation, &connection_id).await
}

pub(crate) async fn prepare_with_connection_id(
    store: Arc<RuntimeStore>,
    resources: &PreparationResources,
    intent: &TurnIntent,
    method: Method,
    generation: u64,
    connection_id: &str,
) -> Result<RequestPreparation, WorkerError> {
    if matches!(method, Method::ThreadCompactStart | Method::ReviewStart) {
        return Err(refusal(
            "native-action/preparation-requires-action-entry".into(),
        ));
    }
    let selection = admit(Arc::clone(&store), resources, intent).await?;
    let token = workspace(Arc::clone(&store), resources, intent).await?;
    prepare_ready(
        store,
        resources,
        intent,
        method,
        generation,
        connection_id,
        Some(selection),
        token,
        None,
    )
    .await
}

/// Build the same immutable thread/settings snapshot used by turn preparation,
/// without compiling or submitting user input. Native settings are handed to
/// the native-action owner for its separately receipted provider request.
pub(crate) async fn prepare_native_action(
    store: Arc<RuntimeStore>,
    resources: &PreparationResources,
    agent_id: &str,
    attempt_id: &str,
    generation: u64,
) -> Result<super::PreparedNativeAction, WorkerError> {
    let mut snapshot = prepare_native_snapshot(
        Arc::clone(&store),
        resources,
        agent_id,
        attempt_id,
        generation,
        false,
        None,
    )
    .await?;
    let program = daybreak_program(resources, &snapshot)?;
    let settings = native_turn_params(
        &mut snapshot,
        &thread::TurnInput {
            events: Vec::new(),
            has_assets: false,
        },
        program.as_deref(),
    )?;
    let thread_id = snapshot
        .actor
        .thread_id
        .filter(|id| !id.is_empty())
        .ok_or_else(|| refusal("native-action/thread-required".into()))?;
    Ok(super::PreparedNativeAction {
        thread_id,
        settings: Value::Object(settings),
    })
}

/// Python's `Runtime.new_thread_params` counterpart. Reuses the exact checked
/// snapshot and thread planner used by normal startup, but forces a fresh start.
pub(crate) async fn prepare_native_thread(
    store: Arc<RuntimeStore>,
    resources: &PreparationResources,
    agent_id: &str,
    attempt_id: &str,
    generation: u64,
) -> Result<super::PreparedNativeThread, WorkerError> {
    prepare_native_thread_for_transfer_owner(
        store, resources, agent_id, attempt_id, generation, None,
    )
    .await
}

/// Prepare a fresh destination thread only for the active durable transfer
/// that owns this member. The owner is rechecked in the snapshot transaction.
pub(crate) async fn prepare_account_transfer_native_thread(
    store: Arc<RuntimeStore>,
    resources: &PreparationResources,
    agent_id: &str,
    transfer_id: &str,
    attempt_id: &str,
    generation: u64,
) -> Result<super::PreparedNativeThread, WorkerError> {
    prepare_native_thread_for_transfer_owner(
        store,
        resources,
        agent_id,
        attempt_id,
        generation,
        Some(transfer_id),
    )
    .await
}

async fn prepare_native_thread_for_transfer_owner(
    store: Arc<RuntimeStore>,
    resources: &PreparationResources,
    agent_id: &str,
    attempt_id: &str,
    generation: u64,
    transfer_owner: Option<&str>,
) -> Result<super::PreparedNativeThread, WorkerError> {
    let mut snapshot = prepare_native_snapshot(
        store,
        resources,
        agent_id,
        attempt_id,
        generation,
        true,
        transfer_owner,
    )
    .await?;
    if snapshot.actor.native_teleport_tool.is_none() && snapshot.actor.thread_id.is_some() {
        snapshot.actor.native_teleport_tool = Some(thread::SessionTool::Move);
    }
    snapshot.actor.thread_id = None;
    snapshot.catalog.saved_thread_id = None;
    // A fresh restore start still uses the source agent's saved tool catalog,
    // which Python's new_thread_params preserves when threadId is present.
    snapshot.actor.native_tool_source = Some(snapshot.catalog.source);
    match plan_native_thread(
        &mut snapshot,
        attempt_id,
        &format!("native-thread:{agent_id}:{attempt_id}"),
    ) {
        thread::Outcome::Start(attempt) => Ok(super::PreparedNativeThread {
            // Python `Runtime.new_thread_params` emits no cyberAccessProgram and
            // does not validate Daybreak at this point.
            params: Value::Object(attempt.params),
        }),
        thread::Outcome::Refuse(reason) => Err(refusal(format!("thread-preparation/{reason:?}"))),
        thread::Outcome::Hold(reason) => Err(refusal(format!("thread-preparation/{reason:?}"))),
        _ => Err(refusal("native-thread/start-required".into())),
    }
}

/// Prepare the permission portion of Python's monitor command path. Monitor
/// ownership is checked by the monitor service; this snapshot only rechecks
/// the actor epoch/deletion state and intentionally has no startAttempt gate.
pub(crate) async fn prepare_monitor_command(
    store: Arc<RuntimeStore>,
    resources: &PreparationResources,
    agent_id: &str,
    epoch: u64,
) -> Result<super::PreparedMonitorCommand, WorkerError> {
    if agent_id.is_empty() || agent_id.len() > 200 {
        return Err(refusal("monitor-preparation/agent-id-invalid".into()));
    }
    let snapshot_store = Arc::clone(&store);
    let snapshot_agent_id = agent_id.to_owned();
    let (filesystem, observed) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/account_worker_monitor_snapshot",
        move || {
            let filesystem = FilesystemSnapshot::capture(&snapshot_store, &snapshot_agent_id)?;
            let observed = AgentRepository::get(
                &*snapshot_store.read()?,
                &AgentId::from(snapshot_agent_id.as_str()),
            )?
            .ok_or_else(|| invalid(snapshot_agent_id.as_str()))?
            .value();
            Ok::<_, WorkerError>((filesystem, observed))
        },
    )
    .await
    .map_err(|_| WorkerError::TaskFailed)??;
    validate_monitor_actor(&observed, epoch)?;
    let image_temp = if observed["imageWorkspace"] == true
        && observed["imageWorkspaceReady"] != true
        && writable_sandbox_roots(&observed)
    {
        let store = Arc::clone(&store);
        let agent_id = agent_id.to_owned();
        Some(
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/account_worker_monitor_image_temp",
                move || ensure_image_workspace_temp(&store, &agent_id),
            )
            .await
            .map_err(|_| WorkerError::TaskFailed)??,
        )
    } else {
        None
    };
    let agent_id = agent_id.to_owned();
    let resources_for_snapshot = resources.clone();
    let state_root = store.root().to_path_buf();
    scheduler::lifecycle_transaction_with(&store, move |tx| {
        let current = agent(tx, &agent_id)?.value();
        validate_monitor_actor(&current, epoch)?;
        let connection = current["startAttempt"]["connectionId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .unwrap_or("monitor-preparation");
        let snapshot = thread_snapshot(
            tx,
            &resources_for_snapshot,
            &current,
            connection,
            &state_root,
            &filesystem.thread_inputs,
            None,
        )?;
        let thread_id = snapshot
            .actor
            .thread_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| refusal("monitor-preparation/thread-required".into()))?
            .to_owned();
        let progress_directory = match &snapshot.workspace {
            thread::WorkspaceReadiness::Ready { progress_directory } => progress_directory.clone(),
            _ => None,
        };
        let settings = monitor_command_settings(
            &current,
            &snapshot.actor.cwd,
            snapshot.settings.role,
            snapshot.settings.is_lead,
            snapshot.settings.yolo_mode,
            progress_directory,
            image_temp.as_deref(),
        )?;
        Ok(super::PreparedMonitorCommand {
            thread_id,
            settings,
        })
    })
    .await
}

fn validate_monitor_actor(record: &Value, epoch: u64) -> Result<(), WorkerError> {
    if record["epoch"] != json!(epoch)
        || record
            .get("deletedAt")
            .is_some_and(|value| !value.is_null())
        || record["autoWake"] != true
    {
        return Err(refusal("monitor-preparation/actor-changed".into()));
    }
    Ok(())
}

fn execution_cwd(record: &Value, validated_workspace_cwd: &Path) -> Option<String> {
    if record.get("executionMode").and_then(Value::as_str) == Some("vm") {
        record.get("cwd").and_then(Value::as_str).map(str::to_owned)
    } else {
        Some(validated_workspace_cwd.to_string_lossy().into_owned())
    }
}

/// The turn permissions of a VM reviewer. Its line is read-only on disk and owned by root. With
/// the private scratch folder of a current guest (`layrScratch`), it may write only there. It
/// has no network access; it reads states through layr outside its sandbox (Python's
/// `layr_read` tool). Without a scratch folder (an older guest) it stays read-only. Python keeps
/// the same rule (codex_vm_agents.reviewer_permissions).
fn vm_reviewer_permissions(record: &Value) -> Option<Value> {
    if record["executionMode"] != "vm" || record["role"] != "reviewer" {
        return None;
    }
    let scratch = record["layrScratch"]
        .as_str()
        .filter(|path| !path.is_empty());
    let sandbox = match scratch {
        Some(scratch) => {
            json!({"type":"workspaceWrite","writableRoots":[scratch],"networkAccess":false})
        }
        None => json!({"type":"readOnly"}),
    };
    Some(json!({"approvalPolicy":"never","sandboxPolicy":sandbox}))
}

fn apply_vm_reviewer_permissions(
    params: &mut serde_json::Map<String, Value>,
    vm_reviewer: Option<&Value>,
) {
    if let Some(Value::Object(permissions)) = vm_reviewer {
        for (key, value) in permissions {
            params.insert(key.clone(), value.clone());
        }
    }
}

/// The agent's provider can give an unready image workspace a writable temp root.
fn writable_sandbox_roots(record: &Value) -> bool {
    studio_runtime_providers::ProviderKind::of_record(record).is_ok_and(|kind| {
        crate::provider_adapter::adapter(kind)
            .capabilities()
            .writable_sandbox_roots
    })
}

fn monitor_command_settings(
    record: &Value,
    cwd: &str,
    role: thread::Role,
    is_lead: bool,
    yolo_mode: Option<bool>,
    progress_directory: Option<String>,
    image_temp: Option<&str>,
) -> Result<Value, WorkerError> {
    let permissions = if record["imageWorkspace"] == true && record["imageWorkspaceReady"] != true {
        if !writable_sandbox_roots(record) {
            json!({"approvalPolicy":"never","sandboxPolicy":{"type":"readOnly"}})
        } else {
            let temp = image_temp
                .ok_or_else(|| refusal("monitor-preparation/image-temp-required".into()))?;
            json!({"approvalPolicy":"never","sandboxPolicy":{"type":"workspaceWrite","writableRoots":[temp],"networkAccess":false}})
        }
    } else if let Some(permissions) =
        vm_reviewer_permissions(record).filter(|_| role == thread::Role::Reviewer)
    {
        permissions
    } else {
        match yolo_mode {
            Some(true) => {
                json!({"approvalPolicy":"never","sandboxPolicy":{"type":"dangerFullAccess"}})
            }
            Some(false) if role == thread::Role::Reviewer => {
                json!({"approvalPolicy":"on-request","sandboxPolicy":{"type":"readOnly"}})
            }
            Some(false) => {
                let mut roots = vec![Value::String(cwd.into())];
                if is_lead && let Some(progress) = progress_directory {
                    roots.push(Value::String(progress));
                }
                json!({"approvalPolicy":"on-request","sandboxPolicy":{"type":"workspaceWrite","writableRoots":roots,"networkAccess":false}})
            }
            None => json!({}),
        }
    };
    let mut permissions = permissions;
    let object = permissions
        .as_object_mut()
        .ok_or_else(|| refusal("monitor-preparation/permissions-invalid".into()))?;
    if !object.contains_key("sandboxPolicy") {
        let sandbox = record.get("sandbox").filter(|value| json_truthy(value));
        let Some(sandbox) = sandbox else {
            return Err(refusal("monitor-preparation/sandbox-unknown".into()));
        };
        if let Some(profile_id) = record["profile"]["id"].as_str().filter(|id| !id.is_empty()) {
            object.insert("permissionProfile".into(), json!(profile_id));
        } else {
            object.insert("sandboxPolicy".into(), sandbox.clone());
        }
    }
    Ok(permissions)
}

fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn ensure_image_workspace_temp(
    store: &RuntimeStore,
    agent_id: &str,
) -> Result<String, WorkerError> {
    use std::fs;
    let root = store
        .root()
        .canonicalize()
        .map_err(|_| refusal("monitor-preparation/image-temp-root".into()))?;
    let temp_root = root.join("image-workspace-temp");
    ensure_private_directory(&temp_root)?;
    if fs::canonicalize(&temp_root)
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from))
        .as_deref()
        != Some(root.as_path())
    {
        return Err(refusal("monitor-preparation/image-temp-escape".into()));
    }
    let name = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, agent_id.as_bytes()).to_string();
    let temp = temp_root.join(name);
    ensure_private_directory(&temp)?;
    let resolved_root = fs::canonicalize(&temp_root)
        .map_err(|_| refusal("monitor-preparation/image-temp-root".into()))?;
    let resolved = fs::canonicalize(&temp)
        .map_err(|_| refusal("monitor-preparation/image-temp-directory".into()))?;
    if resolved.parent() != Some(resolved_root.as_path()) {
        return Err(refusal("monitor-preparation/image-temp-escape".into()));
    }
    Ok(resolved.to_string_lossy().into_owned())
}

fn ensure_private_directory(path: &std::path::Path) -> Result<(), WorkerError> {
    use std::fs;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(refusal("monitor-preparation/image-temp-unsafe".into()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Err(error) = fs::create_dir(path)
                && error.kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(refusal("monitor-preparation/image-temp-create".into()));
            }
        }
        Err(_) => return Err(refusal("monitor-preparation/image-temp-create".into())),
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| refusal("monitor-preparation/image-temp-create".into()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(refusal("monitor-preparation/image-temp-unsafe".into()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| refusal("monitor-preparation/image-temp-permissions".into()))?;
    }
    Ok(())
}

async fn prepare_native_snapshot(
    store: Arc<RuntimeStore>,
    resources: &PreparationResources,
    agent_id: &str,
    attempt_id: &str,
    generation: u64,
    allow_paused_start: bool,
    account_transfer_owner: Option<&str>,
) -> Result<thread::Snapshot, WorkerError> {
    let read_store = Arc::clone(&store);
    let read_agent_id = agent_id.to_owned();
    let record = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/account_worker_native_snapshot_agent",
        move || {
            AgentRepository::get(&*read_store.read()?, &AgentId::from(read_agent_id.as_str()))?
                .map(|record| record.value())
                .ok_or_else(|| invalid("agent"))
        },
    )
    .await
    .map_err(|_| WorkerError::TaskFailed)??;
    let intent = TurnIntent {
        agent_id: agent_id.into(),
        epoch: decode(record["epoch"].clone(), "agent/epoch")?,
        attempt_id: attempt_id.into(),
        account_key: record
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            .into(),
        provider: studio_runtime_providers::ProviderKind::of_record(&record)
            .map_err(|_| invalid("agent/provider"))?,
        thread_id: record
            .get("threadId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        cwd: record.get("cwd").and_then(Value::as_str).map(str::to_owned),
        model: record
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned),
        events: Vec::new(),
        radio_authorized: false,
    };
    let admission =
        admit_for_purpose(Arc::clone(&store), resources, &intent, allow_paused_start).await?;
    let token = if allow_paused_start {
        None
    } else {
        Some(workspace_with_vm_action(Arc::clone(&store), resources, &intent, true).await?)
    };
    let restore_cwd = if allow_paused_start {
        let cwd = intent
            .cwd
            .as_ref()
            .map(PathBuf::from)
            .ok_or_else(|| refusal("workspace-readiness/cwd-required".into()))?;
        Some(
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/account_worker_native_snapshot_cwd",
                move || {
                    if std::fs::canonicalize(&cwd).ok().as_ref() != Some(&cwd) || !cwd.is_dir() {
                        return Err(refusal("workspace-readiness/cwd-unavailable".into()));
                    }
                    Ok::<_, WorkerError>(cwd)
                },
            )
            .await
            .map_err(|_| WorkerError::TaskFailed)??,
        )
    } else {
        None
    };
    let filesystem_store = Arc::clone(&store);
    let filesystem_intent = intent.clone();
    let filesystem = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/account_worker_native_snapshot_filesystem",
        move || {
            FilesystemSnapshot::capture_for_authorization(&filesystem_store, &filesystem_intent)
        },
    )
    .await
    .map_err(|_| WorkerError::TaskFailed)??;
    let (identity, _) = catalog(resources, &intent)?;
    let state_root = store.root().to_path_buf();
    let intent = intent.clone();
    let resources_for_snapshot = resources.clone();
    let generation = generation.to_string();
    let transfer_owner = account_transfer_owner.map(str::to_owned);
    scheduler::lifecycle_transaction_with(&store, move |tx| {
        let current =
            preconditions_for_purpose(tx, &intent, &filesystem, allow_paused_start, None)?;
        let frozen = admission
            .selection
            .freeze_record(&current, &identity, Instant::now())
            .map_err(catalog_admission_error)?;
        let cwd = if let Some(token) = &token {
            token
                .validate(tx)?
                .map_err(|_| refusal(WORKSPACE_TOKEN_STALE.into()))?
        } else {
            restore_cwd
                .as_ref()
                .cloned()
                .ok_or_else(|| refusal("workspace-readiness/cwd-required".into()))?
        };
        let mut frozen = frozen;
        frozen["cwd"] = json!(cwd);
        thread_snapshot(
            tx,
            &resources_for_snapshot,
            &frozen,
            &generation,
            &state_root,
            &filesystem.thread_inputs,
            transfer_owner.as_deref(),
        )
    })
    .await
}

/// Prepare the safety retry's thread fork and turn start from the same checked
/// thread snapshot and permission builders used by ordinary turn preparation.
/// The safety operation owns durable intent and provider receipts; this helper
/// only returns immutable request maps.
pub(crate) async fn prepare_safety_retry(
    store: Arc<RuntimeStore>,
    resources: &PreparationResources,
    request: &super::SafetyRetryPreparation,
) -> Result<super::PreparedSafetyRetry, WorkerError> {
    if request.agent_id.is_empty()
        || request.attempt_id.is_empty()
        || request.source_thread_id.is_empty()
        || request.turn_id.is_empty()
        || request.model.is_empty()
        || !request.input.is_array()
    {
        return Err(refusal("safety-retry/identity-invalid".into()));
    }
    let read_store = Arc::clone(&store);
    let agent_id = request.agent_id.clone();
    let record = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/account_worker_safety_retry_agent",
        move || {
            AgentRepository::get(&*read_store.read()?, &AgentId::from(agent_id.as_str()))?
                .map(|record| record.value())
                .ok_or_else(|| invalid("agent"))
        },
    )
    .await
    .map_err(|_| WorkerError::TaskFailed)??;
    if record.get("threadId").and_then(Value::as_str) != Some(&request.source_thread_id)
        || record
            .get("deletedAt")
            .is_some_and(|value| !value.is_null())
    {
        return Err(refusal("safety-retry/agent-changed".into()));
    }
    let filesystem_store = Arc::clone(&store);
    let filesystem_agent_id = request.agent_id.clone();
    let filesystem = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/account_worker_safety_retry_filesystem",
        move || FilesystemSnapshot::capture(&filesystem_store, &filesystem_agent_id),
    )
    .await
    .map_err(|_| WorkerError::TaskFailed)??;
    let connection_id = record
        .get("startAttempt")
        .and_then(|value| value.get("connectionId"))
        .and_then(Value::as_str)
        .unwrap_or("safety-retry-preparation")
        .to_owned();
    let snapshot_request = request.clone();
    let resources_for_snapshot = resources.clone();
    let state_root = store.root().to_path_buf();
    let mut snapshot = scheduler::lifecycle_transaction_with(&store, move |tx| {
        let current = agent(tx, &snapshot_request.agent_id)?.value();
        if current.get("threadId").and_then(Value::as_str)
            != Some(&snapshot_request.source_thread_id)
            || current.get("epoch") != record.get("epoch")
        {
            return Err(refusal("safety-retry/agent-changed".into()));
        }
        thread_snapshot(
            tx,
            &resources_for_snapshot,
            &current,
            &connection_id,
            &state_root,
            &filesystem.thread_inputs,
            None,
        )
    })
    .await?;
    // Python's safety retry explicitly starts at low effort, retaining the
    // existing account's fast-mode service tier and permission profile.
    snapshot.settings.model = Some(request.model.clone());
    snapshot.settings.effort = Some("low".into());
    snapshot.settings.native_effort = Some("low".into());
    let mut fork_params = match plan_native_thread(
        &mut snapshot,
        &request.attempt_id,
        &format!("safety-retry:{}", request.attempt_id),
    ) {
        thread::Outcome::Resume(attempt) => attempt.params,
        thread::Outcome::Refuse(reason) => {
            return Err(refusal(format!("thread-preparation/{reason:?}")));
        }
        thread::Outcome::Hold(reason) => {
            return Err(refusal(format!("thread-preparation/{reason:?}")));
        }
        _ => return Err(refusal("safety-retry/source-thread-required".into())),
    };
    let program = daybreak_program(resources, &snapshot)?;
    let mut turn_params = native_turn_params(
        &mut snapshot,
        &thread::TurnInput {
            events: Vec::new(),
            has_assets: false,
        },
        program.as_deref(),
    )?;
    // Python's safety retry builds its turn/start map directly and omits cwd;
    // cwd belongs to the preceding fork request only.
    turn_params.remove("cwd");
    let thread_id = request.attempt_id.clone();
    let allowed_fork_keys = [
        "approvalPolicy",
        "approvalsReviewer",
        "sandbox",
        "permissions",
        "config",
        "developerInstructions",
        "runtimeWorkspaceRoots",
    ];
    fork_params.retain(|key, _| allowed_fork_keys.contains(&key.as_str()));
    fork_params.insert("threadId".into(), json!(request.source_thread_id));
    fork_params.insert("beforeTurnId".into(), json!(request.turn_id));
    fork_params.insert("deferGoalContinuation".into(), json!(true));
    fork_params.insert("excludeTurns".into(), json!(true));
    fork_params.insert("model".into(), json!(request.model));
    fork_params.insert("cwd".into(), json!(snapshot.actor.cwd));

    turn_params.insert("threadId".into(), json!(thread_id));
    turn_params.insert("input".into(), request.input.clone());
    turn_params.insert("clientUserMessageId".into(), json!(request.attempt_id));
    turn_params.insert("model".into(), json!(request.model));
    turn_params.insert("effort".into(), json!("low"));
    turn_params.insert(
        "serviceTier".into(),
        json!(if snapshot.settings.fast_mode {
            "priority"
        } else {
            "default"
        }),
    );
    Ok(super::PreparedSafetyRetry {
        attempt_id: request.attempt_id.clone(),
        thread_id,
        fork_params: Value::Object(fork_params),
        turn_params: Value::Object(turn_params),
    })
}
#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare_ready(
    store: Arc<RuntimeStore>,
    resources: &PreparationResources,
    intent: &TurnIntent,
    method: Method,
    generation: u64,
    connection_id: &str,
    admission: Option<Admission>,
    token: crate::workspace_ready::ReadyToken,
    loaded: Option<thread::LoadedIdentity>,
) -> Result<RequestPreparation, WorkerError> {
    let mut method = method;
    // Workspace provisioning can update stored agent/project paths as well as
    // create the progress directory. Capture all filesystem observations after
    // that work, before the transaction rechecks the current records.
    let snapshot_store = Arc::clone(&store);
    let snapshot_intent = intent.clone();
    let radio_authorized = intent.radio_authorized;
    let (filesystem, prepared_host) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/account_worker_ready_snapshot",
        move || {
            let filesystem =
                FilesystemSnapshot::capture_for_authorization(&snapshot_store, &snapshot_intent)?;
            let prepared_host = if radio_authorized {
                let host = crate::host_project_runtime::PreparedHost::load(&snapshot_store)
                    .map_err(|error| refusal(error.to_string()))?;
                host.recheck_paths()
                    .map_err(|error| refusal(error.to_string()))?;
                Some(host)
            } else {
                None
            };
            Ok::<_, WorkerError>((filesystem, prepared_host))
        },
    )
    .await
    .map_err(|_| WorkerError::TaskFailed)??;
    let (identity, _) = catalog(resources, intent)?;
    let intent_for_snapshot = intent.clone();
    let token_for_snapshot = token.clone();
    let generation = generation.to_string();
    let state_root = store.root().to_path_buf();
    let resources_for_snapshot = resources.clone();
    let (
        mut snapshot,
        mut turn_snapshot,
        events,
        mut prepared_intent,
        vm_reviewer,
        filesystem,
        prepared_host,
        selection,
    ) = store
        .read_async("runtime-services/account_worker_ready", move |tx| {
            // Snapshots only: the submitted commit rechecks them under the writer.
            let record = stage("turn_preparation.ready.preconditions", || {
                preconditions(
                    tx,
                    &intent_for_snapshot,
                    &filesystem,
                    prepared_host.as_ref(),
                )
            })?;
            let selection_for_snapshot = match admission {
                Some(Admission { selection }) => selection,
                None => frozen_selection(&record, &identity)?,
            };
            let mut frozen = selection_for_snapshot
                .freeze_record(&record, &identity, Instant::now())
                .map_err(|e| refusal(e.to_string()))?;
            let cwd = token_for_snapshot
                .validate(tx)?
                .map_err(|_| refusal(WORKSPACE_TOKEN_STALE.into()))?;
            let vm_reviewer = vm_reviewer_permissions(&frozen);
            if let Some(execution_cwd) = execution_cwd(&frozen, &cwd) {
                frozen["cwd"] = json!(execution_cwd);
            }
            if frozen.get("nativeEffort").is_none() {
                frozen["effort"] = json!(selection_for_snapshot.settings.effort);
                frozen["nativeEffort"] = json!(selection_for_snapshot.settings.native_effort);
            }
            let snapshot = stage("turn_preparation.ready.thread_snapshot", || {
                thread_snapshot(
                    tx,
                    &resources_for_snapshot,
                    &frozen,
                    &generation,
                    &state_root,
                    &filesystem.thread_inputs,
                    None,
                )
            })?;
            let (turn_snapshot, events) =
                stage("turn_preparation.ready.submission_snapshot", || {
                    submission_snapshot(
                        tx,
                        &resources_for_snapshot,
                        &frozen,
                        &intent_for_snapshot,
                        &state_root,
                    )
                })?;
            let mut prepared_intent = intent_for_snapshot.clone();
            prepared_intent.cwd = Some(snapshot.actor.cwd.clone());
            prepared_intent.model = snapshot.settings.model.clone();
            prepared_intent.thread_id = snapshot.actor.thread_id.clone();
            Ok::<_, WorkerError>((
                snapshot,
                turn_snapshot,
                events,
                prepared_intent,
                vm_reviewer,
                filesystem,
                prepared_host,
                selection_for_snapshot,
            ))
        })
        .await?;
    let mut guard = CommitGuard {
        native_params: None,
        selection,
        token,
        resources: resources.clone(),
        thread: snapshot.clone(),
        compiler_snapshot: None,
        state_root: store.root().to_path_buf(),
        filesystem,
        prepared_host,
    };
    // The commit guard keeps the stored projection; only planning sees the
    // connection's in-memory loaded-thread proof.
    snapshot.loaded = loaded;
    if method == Method::ThreadResume {
        let thread_method = if snapshot.actor.thread_id.is_some() {
            Method::ThreadResume
        } else {
            Method::ThreadStart
        };
        let operation = format!(
            "account:{}:{}:{}",
            intent.account_key,
            intent.attempt_id,
            thread_method.as_str()
        );
        match thread::plan(&snapshot, &intent.attempt_id, &operation) {
            thread::Outcome::Start(attempt) | thread::Outcome::Resume(attempt) => {
                return thread_request(
                    &store,
                    prepared_intent,
                    thread_method,
                    *attempt,
                    guard,
                    connection_id,
                )
                .await;
            }
            thread::Outcome::Hold(e) => return Err(refusal(format!("thread-preparation/{e:?}"))),
            thread::Outcome::Refuse(e) => return Err(refusal(format!("thread-preparation/{e:?}"))),
            // This connection loaded the thread with these settings: start the turn.
            thread::Outcome::Ready(_) => method = Method::TurnStart,
        }
    }
    continuation_gate(&snapshot, &intent.attempt_id)?;
    snapshot.actor.thread_id = prepared_intent.thread_id.clone();
    turn_snapshot.operation_id = format!(
        "account:{}:{}:{}",
        intent.account_key,
        intent.attempt_id,
        method.as_str()
    );
    let (mut turn_snapshot, mut events, first_compile) =
        compile_turn(turn_snapshot, events).await?;
    let compiled = match first_compile {
        Ok(compiled) => compiled,
        Err(turn::PrepareError::ArtifactsRequired(artifacts)) => {
            for artifact in artifacts {
                let state_root = store.root().to_path_buf();
                let relative_path = artifact.relative_path.clone();
                let body = artifact.body.clone();
                let path = studio_runtime_process::blocking_pool::spawn_blocking(
                    "runtime-services/account_worker_ready_materialize_artifact",
                    move || materialize(&state_root, &relative_path, &body),
                )
                .await
                .map_err(|_| WorkerError::TaskFailed)??;
                turn_snapshot.artifact_paths.insert(artifact.event_id, path);
            }
            let (snapshot, compiled_events, result) = compile_turn(turn_snapshot, events).await?;
            turn_snapshot = snapshot;
            events = compiled_events;
            result.map_err(|e| refusal(e.to_string()))?
        }
        Err(e) => return Err(refusal(e.to_string())),
    };
    guard.compiler_snapshot = Some(turn_snapshot.clone());
    let mut params = thread::turn_params(
        &snapshot,
        &thread::TurnInput {
            events: events
                .iter()
                .map(|e| thread::TurnEvent {
                    kind: e.kind.clone(),
                    text: e.text.clone(),
                })
                .collect(),
            has_assets: events.iter().any(|e| !e.metadata.assets.is_empty()),
        },
    )
    .map_err(|e| refusal(format!("thread-preparation/{e:?}")))?;
    apply_vm_reviewer_permissions(&mut params, vm_reviewer.as_ref());
    params.insert("input".into(), json!(compiled.provider_inputs));
    params.insert(
        "clientUserMessageId".into(),
        json!(compiled.event_ids.first()),
    );
    let submission = crate::notification_dispatch::PreparedSubmission {
        first_event_id: compiled.event_ids[0].clone(),
        submitted_at: turn_snapshot.prepared_at,
        created_at: prepared_intent.events[0].created,
        text: compiled
            .fulltext
            .clone()
            .unwrap_or_else(|| compiled.transcript.text.clone()),
        inputs: prepared_intent
            .events
            .iter()
            .zip(&compiled.transcript.inputs)
            .map(|(e, t)| crate::notification_dispatch::SubmittedInput {
                id: e.id.clone(),
                at: e.created,
                kind: e.kind.clone(),
                text: e.text.clone(),
                assets: json!(t.assets),
            })
            .collect(),
        assets: json!(compiled.transcript.assets),
        radio_authorized: intent.radio_authorized,
    };
    guard.native_params = Some(Value::Object(params.clone()));
    prepared_intent.thread_id = snapshot.actor.thread_id;
    Ok(RequestPreparation {
        intent: prepared_intent,
        method,
        params: json!(params),
        thread: None,
        compiled: Some(compiled),
        submission: Some(submission),
        guard,
        connection_id: connection_id.to_owned(),
    })
}
/// A thread start or resume request; its artifacts are written before submission.
async fn thread_request(
    store: &RuntimeStore,
    intent: TurnIntent,
    method: Method,
    attempt: thread::PreparedAttempt,
    guard: CommitGuard,
    connection_id: &str,
) -> Result<RequestPreparation, WorkerError> {
    for artifact in &attempt.artifacts {
        let thread::ArtifactIntent::RememberRole { version, text } = artifact;
        let state_root = store.root().to_path_buf();
        let relative_path = format!("context-snapshots/{version}.txt");
        let text = text.clone();
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/account_worker_ready_materialize_context",
            move || materialize(&state_root, &relative_path, &text),
        )
        .await
        .map_err(|_| WorkerError::TaskFailed)??;
    }
    Ok(RequestPreparation {
        intent,
        method,
        params: json!(attempt.params),
        thread: Some(attempt),
        compiled: None,
        submission: None,
        guard,
        connection_id: connection_id.to_owned(),
    })
}
fn continuation_gate(snapshot: &thread::Snapshot, attempt: &str) -> Result<(), WorkerError> {
    match thread::plan(snapshot, attempt, "continuation-admission") {
        thread::Outcome::Hold(reason) => Err(refusal(format!("thread-preparation/{reason:?}"))),
        thread::Outcome::Refuse(reason) => Err(refusal(format!("thread-preparation/{reason:?}"))),
        thread::Outcome::Start(_) => Err(refusal("thread-preparation/thread-required".into())),
        thread::Outcome::Resume(_) | thread::Outcome::Ready(_) => Ok(()),
    }
}
pub(crate) fn commit(
    tx: &Transaction<'_>,
    context: &Pending,
    guard: &CommitGuard,
) -> Result<(), WorkerError> {
    let record = stage("turn_submission.commit.preconditions", || {
        preconditions(
            tx,
            &context.intent,
            &guard.filesystem,
            guard.prepared_host.as_ref(),
        )
    })?;
    let (identity, _) = catalog(&guard.resources, &context.intent)?;
    let mut frozen = guard
        .selection
        .freeze_record(&record, &identity, Instant::now())
        .map_err(|e| refusal(e.to_string()))?;
    let cwd = stage("turn_submission.commit.token", || guard.token.validate(tx))?
        .map_err(|_| refusal(WORKSPACE_TOKEN_STALE.into()))?;
    if context.intent.cwd.as_deref() != cwd.to_str() {
        return Err(refusal("workspace-readiness/cwd-changed".into()));
    }
    let mut current = frozen.clone();
    current["cwd"] = json!(cwd);
    if current.get("nativeEffort").is_none() {
        current["effort"] = json!(guard.selection.settings.effort);
        current["nativeEffort"] = json!(guard.selection.settings.native_effort);
        frozen["effort"] = current["effort"].clone();
        frozen["nativeEffort"] = current["nativeEffort"].clone();
    }
    if let (Some(baseline), Some(compiled)) = (&guard.compiler_snapshot, &context.compiled) {
        let (mut current_submission, current_events) =
            stage("turn_submission.commit.submission_snapshot", || {
                submission_snapshot(
                    tx,
                    &guard.resources,
                    &current,
                    &context.intent,
                    &guard.state_root,
                )
            })?;
        // Recompile with the exact operation, clock and verified artifact paths.
        // The compiler's manifest and output record the effective dependencies;
        // unrelated worker state must not invalidate a non-lead submission.
        current_submission.prepared_at = baseline.prepared_at;
        current_submission.operation_id = baseline.operation_id.clone();
        current_submission.artifact_paths = baseline.artifact_paths.clone();
        let recompiled = stage("turn_submission.commit.compile", || {
            turn::compile(&current_submission, &current_events)
        })
        .map_err(|e| refusal(e.to_string()))?;
        if stage("turn_submission.commit.compare", || {
            Ok::<_, WorkerError>(
                serde_json::to_value(recompiled).map_err(|_| invalid("compiler result"))?
                    != serde_json::to_value(compiled).map_err(|_| invalid("compiler result"))?,
            )
        })? {
            return Err(refusal("turn-preparation/snapshot-changed".into()));
        }
    }
    let current = stage("turn_submission.commit.thread_snapshot", || {
        thread_snapshot(
            tx,
            &guard.resources,
            &current,
            &guard.thread.account.connection_id,
            &guard.state_root,
            &guard.filesystem.thread_inputs,
            None,
        )
    })?;
    if current != guard.thread {
        return Err(refusal("thread-preparation/snapshot-changed".into()));
    }
    if context.compiled.is_some() {
        continuation_gate(&current, &context.intent.attempt_id)?;
    }
    if let Some(attempt) = &context.thread {
        apply_patch(&mut frozen, &attempt.before_submit)?;
        frozen["nativeEffort"] = json!(attempt.snapshot.settings.native_effort);
    }
    if let Some(compiled) = &context.compiled {
        if compiled.epoch.0 != context.intent.thread_id {
            return Err(refusal("preparation/thread-changed".into()));
        }
        frozen["complaintsPresented"] = json!(compiled.agent_patch.complaints_presented);
    }
    frozen
        .as_object_mut()
        .ok_or_else(|| invalid("agent"))?
        .remove("preparationWake");
    frozen
        .as_object_mut()
        .ok_or_else(|| invalid("agent"))?
        .remove("lastWorkspaceWait");
    if let Some(attempt) = frozen
        .get_mut("startAttempt")
        .and_then(Value::as_object_mut)
    {
        attempt.remove("prepareError");
    }
    let frozen = decode(frozen, &context.intent.agent_id)?;
    stage("turn_submission.commit.agent_put", || {
        AgentRepository::put(tx, &frozen)
    })?;
    Ok(())
}

pub(crate) fn apply_patch(
    record: &mut Value,
    patch: &thread::AgentPatch,
) -> Result<(), WorkerError> {
    let object = record
        .as_object_mut()
        .ok_or_else(|| invalid("agent patch"))?;
    object.extend(patch.set.clone());
    for key in &patch.remove {
        object.remove(key);
    }
    Ok(())
}
fn materialize(root: &std::path::Path, relative: &str, body: &str) -> Result<String, WorkerError> {
    use std::io::Write;
    let path = std::path::Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(invalid("artifact path"));
    }
    let mut directory = root.to_path_buf();
    let components = path.components().collect::<Vec<_>>();
    for component in &components[..components.len().saturating_sub(1)] {
        directory.push(component.as_os_str());
        match std::fs::create_dir(&directory) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(invalid("artifact directory")),
        }
        if std::fs::symlink_metadata(&directory)
            .map_err(|_| invalid("artifact directory"))?
            .file_type()
            .is_symlink()
        {
            return Err(invalid("artifact symlink"));
        }
    }
    let target = root.join(path);
    let mut temporary =
        tempfile::NamedTempFile::new_in(&directory).map_err(|_| invalid("artifact temporary"))?;
    temporary
        .write_all(body.as_bytes())
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|_| invalid("artifact write"))?;
    // Publish only complete contents and never replace an existing artifact.
    match temporary.persist_noclobber(&target) {
        Ok(_) => {}
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::symlink_metadata(&target)
                .map_err(|_| invalid("artifact"))?
                .file_type()
                .is_symlink()
                || std::fs::read(&target).map_err(|_| invalid("artifact"))? != body.as_bytes()
            {
                return Err(invalid("artifact collision"));
            }
        }
        Err(_) => return Err(invalid("artifact write")),
    }
    #[cfg(unix)]
    std::fs::File::open(&directory)
        .and_then(|file| file.sync_all())
        .map_err(|_| invalid("artifact directory sync"))?;
    Ok(target.to_string_lossy().into_owned())
}

pub(crate) fn persist_compilation(
    tx: &Transaction<'_>,
    compiled: &turn::PreparedSubmission,
) -> Result<(), WorkerError> {
    for (id, patch) in &compiled.metadata_patches {
        let mut meta =
            studio_runtime_store::NotificationDispatchRepository::event_metadata(tx, id)?
                .unwrap_or_else(|| json!({}));
        let original = meta.clone();
        meta.as_object_mut()
            .ok_or_else(|| invalid(id))?
            .extend(patch.clone());
        if meta == original {
            continue;
        }
        tx.execute(
            "INSERT OR REPLACE INTO runtime_event_meta(id,record) VALUES(?1,?2)",
            rusqlite::params![id, meta.to_string()],
        )?;
    }
    Ok(())
}

/// Binding comes from the retained account generation, never the native reply.
pub(crate) fn claude_transport(
    account: &str,
    connection_id: &str,
) -> Result<crate::claude_input_recovery::TransportSnapshot, WorkerError> {
    decode(
        json!({"accountKey":account,"connectionId":connection_id,"supervisorIdentity":null,"closed":false}),
        "retained Claude binding",
    )
}
pub(crate) fn commit_claude(
    tx: &Transaction<'_>,
    intent: &TurnIntent,
    request_id: &str,
    generation: u64,
    connection_id: &str,
    submission: &crate::notification_dispatch::PreparedSubmission,
    guard: &CommitGuard,
) -> Result<bool, WorkerError> {
    crate::claude_input_recovery::runtime::submission::commit_start(
        tx,
        intent,
        request_id,
        crate::claude_input_recovery::runtime::SubmittedBinding {
            transport: claude_transport(&intent.account_key, connection_id)?,
            generation,
        },
        guard
            .native_params
            .as_ref()
            .ok_or_else(|| invalid("Claude prepared params"))?,
        submission,
    )?;
    Ok(true)
}

#[cfg(test)]
mod daybreak_tests {
    use super::*;

    fn action_snapshot(enabled: bool) -> thread::Snapshot {
        thread::Snapshot {
            actor: thread::Actor {
                id: "agent".into(),
                epoch: 1,
                thread_id: Some("thread".into()),
                cwd: "/tmp/project".into(),
                compactions: 0,
                prepare_attempt: None,
                native_teleport_tool: None,
                native_role_guidance: None,
                native_tool_source: None,
                native_tool_update: None,
                error: None,
                native_release: None,
                root_id: "root".into(),
                parent_id: None,
                auto_wake: true,
                native_effort_present: false,
                deleted: false,
            },
            root: thread::Root {
                can_delegate: true,
                federation_enabled: false,
            },
            account: thread::Account {
                key: "account".into(),
                provider: studio_runtime_providers::ProviderKind::Codex,
                connection_id: "connection".into(),
                available: true,
                supervisor_identity: None,
            },
            settings: thread::Settings {
                model: Some("fallback-model".into()),
                effort: None,
                native_effort: None,
                fast_mode: false,
                yolo_mode: Some(true),
                profile_instructions: String::new(),
                role: thread::Role::Implementer,
                is_lead: false,
                needs_title: false,
                claude_options: serde_json::Map::new(),
                daybreak_enabled: thread::Stored::Present(Some(enabled)),
                cyber_access_program: thread::Stored::Absent,
            },
            guidance: thread::GuidanceBundle {
                instructions: String::new(),
                role: String::new(),
                progress: String::new(),
                thread_config: serde_json::Map::new(),
                browser: None,
            },
            catalog: thread::CatalogSnapshot {
                source: thread::CatalogSource::Current,
                definitions: Vec::new(),
                saved_thread_id: Some("thread".into()),
                saved_digest: None,
            },
            workspace: thread::WorkspaceReadiness::Ready {
                progress_directory: None,
            },
            unsupported: None,
            admission: None,
            pending: None,
            loaded: None,
            closed: false,
        }
    }

    #[test]
    fn disabled_daybreak_uses_standard_for_fallback_model_without_catalog() {
        assert_eq!(
            resolve_daybreak_program(&json!({}), Some("fallback-model"), false)
                .expect("disabled Daybreak resolves to standard"),
            "standard"
        );
    }

    #[test]
    fn enabled_daybreak_uses_grant_for_selected_fallback_model() {
        let catalog = json!({"data":[
            {"model":"requested-model","hidden":true,"availableAccessPrograms":{"cyber":["daybreakBlue"]}},
            {"model":"fallback-model","availableAccessPrograms":{"cyber":["daybreakRed"]}}
        ]});
        assert_eq!(
            resolve_daybreak_program(&catalog, Some("fallback-model"), true)
                .expect("fallback model has a Daybreak grant"),
            "daybreakRed"
        );
    }

    #[test]
    fn native_action_turn_params_resolve_enabled_and_disabled_daybreak_for_fallback_model() {
        let catalog = json!({"data":[
            {"model":"requested-model","hidden":true,"availableAccessPrograms":{"cyber":["daybreakBlue"]}},
            {"model":"fallback-model","availableAccessPrograms":{"cyber":["daybreakRed"]}}
        ]});
        for (enabled, expected) in [(false, "standard"), (true, "daybreakRed")] {
            let mut snapshot = action_snapshot(enabled);
            let program =
                resolve_daybreak_program(&catalog, snapshot.settings.model.as_deref(), enabled);
            assert!(
                program.is_ok(),
                "fallback model Daybreak resolution must succeed"
            );
            let Some(program) = program.ok() else {
                continue;
            };
            if enabled {
                assert!(thread::turn_params(&snapshot, &thread::TurnInput::default()).is_err());
            }
            let params =
                native_turn_params(&mut snapshot, &thread::TurnInput::default(), Some(&program));
            assert!(params.is_ok(), "native action turn params must be prepared");
            let params = match params {
                Ok(params) => params,
                Err(_) => return,
            };
            assert_eq!(params.get("cyberAccessProgram"), Some(&json!(expected)));
            assert_eq!(
                snapshot.settings.daybreak_enabled,
                thread::Stored::Present(Some(enabled))
            );
        }
    }

    #[test]
    fn monitor_preparation_accepts_submitted_running_actor_without_start_admission() {
        let record = json!({
            "id":"agent", "epoch":7, "autoWake":true, "deletedAt":null,
            "status":"running", "startAttempt":{"id":"submitted","submitted":true}
        });
        assert!(validate_monitor_actor(&record, 7).is_ok());
        assert!(validate_monitor_actor(&record, 8).is_err());
        let deleted = json!({"epoch":7,"autoWake":true,"deletedAt":1});
        assert!(validate_monitor_actor(&deleted, 7).is_err());
    }

    #[test]
    fn monitor_settings_match_turn_permission_and_sandbox_fallback() {
        let yolo = monitor_command_settings(
            &json!({"sandbox":"workspace-write"}),
            "/workspace",
            thread::Role::Implementer,
            false,
            Some(true),
            None,
            None,
        );
        assert_eq!(
            yolo.ok(),
            Some(json!({
                "approvalPolicy":"never","sandboxPolicy":{"type":"dangerFullAccess"}
            }))
        );

        let profile = monitor_command_settings(
            &json!({"sandbox":"workspace-write","profile":{"id":"strict"}}),
            "/workspace",
            thread::Role::Implementer,
            false,
            None,
            None,
            None,
        );
        assert_eq!(profile.ok(), Some(json!({"permissionProfile":"strict"})));

        let reviewer = monitor_command_settings(
            &json!({"sandbox":"workspace-write"}),
            "/workspace",
            thread::Role::Reviewer,
            false,
            Some(false),
            None,
            None,
        );
        assert_eq!(
            reviewer.ok(),
            Some(json!({
                "approvalPolicy":"on-request","sandboxPolicy":{"type":"readOnly"}
            }))
        );

        let vm_reviewer = monitor_command_settings(
            &json!({"executionMode":"vm","role":"reviewer","sandbox":"danger-full-access"}),
            "/workspace",
            thread::Role::Reviewer,
            false,
            Some(true),
            None,
            None,
        );
        assert_eq!(
            vm_reviewer.ok(),
            Some(json!({
                "approvalPolicy":"never","sandboxPolicy":{"type":"readOnly"}
            }))
        );

        let mut yolo_turn = serde_json::Map::from_iter([
            ("approvalPolicy".into(), json!("never")),
            ("sandboxPolicy".into(), json!({"type":"dangerFullAccess"})),
        ]);
        let old_guest = vm_reviewer_permissions(&json!({"executionMode":"vm","role":"reviewer"}));
        apply_vm_reviewer_permissions(&mut yolo_turn, old_guest.as_ref());
        assert_eq!(yolo_turn["approvalPolicy"], "never");
        assert_eq!(yolo_turn["sandboxPolicy"], json!({"type":"readOnly"}));

        // A reviewer of a current guest writes only its scratch folder, without network.
        let scratch = vm_reviewer_permissions(&json!({
            "executionMode":"vm","role":"reviewer","layrScratch":"/home/r/.studio/scratch/a"
        }));
        apply_vm_reviewer_permissions(&mut yolo_turn, scratch.as_ref());
        assert_eq!(
            yolo_turn["sandboxPolicy"],
            json!({"type":"workspaceWrite","writableRoots":["/home/r/.studio/scratch/a"],"networkAccess":false})
        );
        assert_eq!(
            vm_reviewer_permissions(&json!({"executionMode":"vm","role":"implementer"})),
            None
        );
    }

    #[test]
    fn vm_execution_uses_guest_cwd_after_host_workspace_token_validation() {
        assert_eq!(
            execution_cwd(
                &json!({"executionMode":"vm","cwd":"/guest/line"}),
                &PathBuf::from("/host/project"),
            ),
            Some("/guest/line".into())
        );
        assert_eq!(
            execution_cwd(
                &json!({"cwd":"/old"}),
                &PathBuf::from("/validated/host/workspace"),
            ),
            Some("/validated/host/workspace".into())
        );
    }
}

#[cfg(test)]
mod claimed_input_tests {
    use super::*;
    use studio_runtime_store::{AgentRecord, QueueEventId, QueueEventRepository};

    fn progress(version: u64) -> String {
        json!({"importance":"progress","message_id":format!("m{version}"),
            "progress_key":"install","progress_version":version,"room":"private:lead:child",
            "sender":"child","sender_name":"child","text":format!("step {version}")})
        .to_string()
    }

    /// A queued lead with two progress updates from one child, as in the
    /// failure of agent 85bc2231 on 19 October 2026.
    async fn seeded(directory: &Path) -> RuntimeStore {
        let store = RuntimeStore::open(Some(directory))
            .await
            .expect("runtime store opens");
        store
            .transaction(|tx| {
                let lead = json!({"id":"lead","rootId":"lead","epoch":1,"status":"queued",
                    "isLead":true,"autoWake":true,"name":"Lead","cwd":"/work",
                    "provider":"claude","accountKey":"default","threadId":"lead-thread"});
                AgentRepository::put(tx, &AgentRecord::from_value(lead).expect("lead"))?;
                for (id, version) in [("progress-1", 1_u64), ("progress-2", 2)] {
                    tx.execute(
                        "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) \
                         VALUES(?1,'lead','agent_message',?2,'pending',?3,1)",
                        rusqlite::params![id, progress(version), version as f64],
                    )?;
                }
                Ok(())
            })
            .await
            .expect("lead and progress seed");
        store
    }

    async fn claim(store: &RuntimeStore) -> TurnIntent {
        scheduler::lifecycle_transaction_with(store, |tx| {
            scheduler::claim_preparation_in_transaction(tx, "lead", 1, "attempt", 10.0)
        })
        .await
        .expect("claim runs")
        .expect("lead is claimable")
    }

    async fn check(store: &RuntimeStore, intent: &TurnIntent) -> Result<(), WorkerError> {
        let intent = intent.clone();
        scheduler::lifecycle_transaction_with(store, move |tx| selected_inputs_current(tx, &intent))
            .await
    }

    async fn status(store: &RuntimeStore, id: &str) -> String {
        let id = id.to_owned();
        store
            .transaction(move |tx| QueueEventRepository::get(tx, &QueueEventId::from(id.as_str())))
            .await
            .expect("event reads")
            .expect("event exists")
            .status
    }

    #[tokio::test]
    async fn coalesced_progress_claim_passes_preparation_and_submission() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = seeded(directory.path()).await;
        let intent = claim(&store).await;
        let ids = intent
            .events
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["progress-2"]);
        assert!(
            intent.events[0].text.contains("earlierProgressUpdates"),
            "the claim carries the model projection: {}",
            intent.events[0].text
        );
        // Input that arrives during preparation stays queued for the next turn.
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) \
                     VALUES('late-user','lead','user','next','pending',11,1)",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("late input seeds");
        check(&store, &intent)
            .await
            .expect("the projected text matches its unchanged stored row");
        assert!(
            scheduler::mark_submitted_for_provider(&store, &intent, "ticket", 1, "connection")
                .await
                .expect("submission validates the saved claim")
        );
        assert_eq!(status(&store, "progress-1").await, "stored_only");
        assert_eq!(status(&store, "progress-2").await, "dispatching");
        assert_eq!(status(&store, "late-user").await, "pending");
    }

    #[tokio::test]
    async fn changed_stored_input_still_refuses_the_claim() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = seeded(directory.path()).await;
        let intent = claim(&store).await;
        store
            .transaction(|tx| {
                tx.execute(
                    "UPDATE runtime_events SET text=?1 WHERE id='progress-2'",
                    [progress(3)],
                )?;
                Ok(())
            })
            .await
            .expect("stored input changes");
        match check(&store, &intent).await {
            Err(WorkerError::PreparationRefused(reason)) => {
                assert_eq!(reason, "preparation/input-changed");
            }
            other => panic!("changed input must refuse, got {other:?}"),
        }
    }
}
