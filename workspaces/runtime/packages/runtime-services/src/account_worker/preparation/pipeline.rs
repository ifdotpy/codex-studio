//! The only adapter allowed to advance a claimed batch to provider submission.
use super::*;
use crate::{submit_gates as gates, thread_prepare as thread, turn_prepare as turn};
use snapshot::*;
use std::path::PathBuf;
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
}
fn catalog(
    resources: &PreparationResources,
    intent: &TurnIntent,
) -> Result<(gates::CatalogIdentity, Option<gates::CatalogObservation>), WorkerError> {
    use crate::model_catalog::AdmissionCatalogRead;
    match resources.model_catalog.read_for_admission(
        &intent.account_key,
        &resources.account_home,
        &intent.provider,
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
                provider: intent.provider.clone(),
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
    if record["epoch"] != intent.epoch
        || (!allow_paused_start
            && (record["startAttempt"]["id"] != intent.attempt_id
                || record["startAttempt"]["submitted"] != false
                || record["autoWake"] != true))
        || record["nativeFailureHold"] == true
        || native_thread_blocked(&record)
        || context_repair_blocked(&record)
    {
        return Err(refusal("preparation/ownership-changed".into()));
    }
    if !matches!(intent.provider.as_str(), "codex" | "claude")
        || ["claudePreInputRetry", "claudeAuthWait"]
            .iter()
            .any(|k| record.get(*k).is_some_and(|v| !v.is_null()))
    {
        return Err(refusal(
            "preparation/claude-frozen-input-recovery-required".into(),
        ));
    }
    gates::budget_admission(record.get("tokenBudget")).map_err(|e| refusal(e.to_string()))?;
    authorize(tx, intent, filesystem, prepared_host)?;
    for selected in &intent.events {
        let current = studio_runtime_store::QueueEventRepository::get(
            tx,
            &studio_runtime_store::QueueEventId::from(selected.id.as_str()),
        )?;
        if current.is_none_or(|event| {
            event.agent.0 != intent.agent_id
                || event.epoch != selected.epoch
                || event.kind != selected.kind
                || event.text != selected.text
                || event.created != selected.created
                || event.error != selected.error
                || event.status != "reserved"
        }) {
            return Err(refusal("preparation/input-changed".into()));
        }
        let metadata =
            studio_runtime_store::NotificationDispatchRepository::event_metadata(tx, &selected.id)?
                .unwrap_or_else(|| json!({}));
        if SchedulerRepository::input_metadata_identity(&metadata)
            != SchedulerRepository::input_metadata_identity(&selected.metadata)
        {
            return Err(refusal("preparation/input-metadata-changed".into()));
        }
    }
    Ok(record)
}
fn catalog_pending() -> WorkerError {
    WorkerError::PreparationPending("model-catalog/refresh-pending".into())
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
    if snapshot.account.provider == thread::Provider::Claude {
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

pub(crate) fn admit(
    store: &RuntimeStore,
    resources: &PreparationResources,
    intent: &TurnIntent,
) -> Result<Admission, WorkerError> {
    admit_for_purpose(store, resources, intent, false)
}
fn admit_for_purpose(
    store: &RuntimeStore,
    resources: &PreparationResources,
    intent: &TurnIntent,
    allow_paused_start: bool,
) -> Result<Admission, WorkerError> {
    let filesystem = FilesystemSnapshot::capture(store, &intent.agent_id)?;
    let prepared_host = if intent.radio_authorized {
        let prepared = crate::host_project_runtime::PreparedHost::load(store)
            .map_err(|error| refusal(error.to_string()))?;
        prepared
            .recheck_paths()
            .map_err(|error| refusal(error.to_string()))?;
        Some(prepared)
    } else {
        None
    };
    let (identity, observation) = catalog(resources, intent)?;
    scheduler::lifecycle_transaction_with(store, |tx| {
        let record = preconditions_for_purpose(
            tx,
            intent,
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
            let (effort, native) = gates::validate_execution(&selected.settings, catalog, false)
                .map_err(|e| refusal(e.to_string()))?;
            selected.settings.effort = effort;
            selected.settings.native_effort =
                native.map_or(gates::StoredField::Null, gates::StoredField::Value);
            selected.catalog_precondition = Some((catalog.identity.clone(), catalog.expires_at));
        }
        Ok(Admission {
            selection: selected,
        })
    })
}
pub(crate) fn workspace(
    store: &RuntimeStore,
    intent: &TurnIntent,
) -> Result<crate::workspace_ready::ReadyToken, WorkerError> {
    // Git provisioning acquires its own short transactions. No store transaction is held here.
    match crate::workspace_ready_git::prepare(store, &intent.agent_id)
        .map_err(|e| refusal(e.to_string()))?
    {
        crate::workspace_ready::WorkspaceDecision::Ready { token } => {
            let record =
                AgentRepository::get(&store.read()?, &AgentId::from(intent.agent_id.as_str()))?
                    .ok_or_else(|| invalid(&intent.agent_id))?;
            if record.extra.get("isLead") == Some(&json!(true)) {
                if intent.agent_id.is_empty()
                    || intent.agent_id.len() > 200
                    || !intent
                        .agent_id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    return Err(invalid("progress agent"));
                }
                for path in [
                    store.root().join("progress"),
                    store.root().join("progress").join(&intent.agent_id),
                ] {
                    match std::fs::create_dir(&path) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(_) => {
                            return Err(refusal("workspace-readiness/progress-directory".into()));
                        }
                    }
                    if std::fs::canonicalize(&path).ok().as_ref() != Some(&path) || !path.is_dir() {
                        return Err(refusal("workspace-readiness/progress-directory".into()));
                    }
                }
            }
            Ok(token)
        }
        crate::workspace_ready::WorkspaceDecision::Refuse(e) => Err(refusal(e.to_string())),
        _ => Err(WorkerError::PreparationPending(
            "workspace-readiness/pending".into(),
        )),
    }
}
pub(crate) fn prepare(
    store: &RuntimeStore,
    resources: &PreparationResources,
    intent: &TurnIntent,
    method: Method,
    generation: u64,
) -> Result<RequestPreparation, WorkerError> {
    if matches!(method, Method::ThreadCompactStart | Method::ReviewStart) {
        return Err(refusal(
            "native-action/preparation-requires-action-entry".into(),
        ));
    }
    let selection = admit(store, resources, intent)?;
    let token = workspace(store, intent)?;
    prepare_ready(
        store, resources, intent, method, generation, selection, token,
    )
}

/// Build the same immutable thread/settings snapshot used by turn preparation,
/// without compiling or submitting user input. Native settings are handed to
/// the native-action owner for its separately receipted provider request.
pub(crate) fn prepare_native_action(
    store: &RuntimeStore,
    resources: &PreparationResources,
    agent_id: &str,
    attempt_id: &str,
    generation: u64,
) -> Result<super::PreparedNativeAction, WorkerError> {
    let mut snapshot =
        prepare_native_snapshot(store, resources, agent_id, attempt_id, generation, false)?;
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
pub(crate) fn prepare_native_thread(
    store: &RuntimeStore,
    resources: &PreparationResources,
    agent_id: &str,
    attempt_id: &str,
    generation: u64,
) -> Result<super::PreparedNativeThread, WorkerError> {
    let mut snapshot =
        prepare_native_snapshot(store, resources, agent_id, attempt_id, generation, true)?;
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
pub(crate) fn prepare_monitor_command(
    store: &RuntimeStore,
    resources: &PreparationResources,
    agent_id: &str,
    epoch: u64,
) -> Result<super::PreparedMonitorCommand, WorkerError> {
    if agent_id.is_empty() || agent_id.len() > 200 {
        return Err(refusal("monitor-preparation/agent-id-invalid".into()));
    }
    let filesystem = FilesystemSnapshot::capture(store, agent_id)?;
    let observed = AgentRepository::get(&store.read()?, &AgentId::from(agent_id))?
        .ok_or_else(|| invalid(agent_id))?
        .value();
    validate_monitor_actor(&observed, epoch)?;
    let image_temp = if observed["imageWorkspace"] == true
        && observed["imageWorkspaceReady"] != true
        && observed["provider"] != "claude"
    {
        Some(ensure_image_workspace_temp(store, agent_id)?)
    } else {
        None
    };
    scheduler::lifecycle_transaction_with(store, |tx| {
        let current = agent(tx, agent_id)?.value();
        validate_monitor_actor(&current, epoch)?;
        let connection = current["startAttempt"]["connectionId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .unwrap_or("monitor-preparation");
        let snapshot = thread_snapshot(
            tx,
            resources,
            &current,
            connection,
            store.root(),
            filesystem.progress_directory.clone(),
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
        if record["provider"] == "claude" {
            json!({"approvalPolicy":"never","sandboxPolicy":{"type":"readOnly"}})
        } else {
            let temp = image_temp
                .ok_or_else(|| refusal("monitor-preparation/image-temp-required".into()))?;
            json!({"approvalPolicy":"never","sandboxPolicy":{"type":"workspaceWrite","writableRoots":[temp],"networkAccess":false}})
        }
    } else if record["executionMode"] == "vm" && role == thread::Role::Reviewer {
        json!({"approvalPolicy":"never","sandboxPolicy":{"type":"readOnly"}})
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

fn prepare_native_snapshot(
    store: &RuntimeStore,
    resources: &PreparationResources,
    agent_id: &str,
    attempt_id: &str,
    generation: u64,
    allow_paused_start: bool,
) -> Result<thread::Snapshot, WorkerError> {
    let record = AgentRepository::get(&store.read()?, &AgentId::from(agent_id))?
        .ok_or_else(|| invalid("agent"))?
        .value();
    let intent = TurnIntent {
        agent_id: agent_id.into(),
        epoch: decode(record["epoch"].clone(), "agent/epoch")?,
        attempt_id: attempt_id.into(),
        account_key: record
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            .into(),
        provider: record
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("codex")
            .into(),
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
    let admission = admit_for_purpose(store, resources, &intent, allow_paused_start)?;
    let token = if allow_paused_start {
        None
    } else {
        Some(workspace(store, &intent)?)
    };
    let restore_cwd = if allow_paused_start {
        let cwd = intent
            .cwd
            .as_ref()
            .map(PathBuf::from)
            .ok_or_else(|| refusal("workspace-readiness/cwd-required".into()))?;
        if std::fs::canonicalize(&cwd).ok().as_ref() != Some(&cwd) || !cwd.is_dir() {
            return Err(refusal("workspace-readiness/cwd-unavailable".into()));
        }
        Some(cwd)
    } else {
        None
    };
    let filesystem = FilesystemSnapshot::capture(store, agent_id)?;
    let (identity, _) = catalog(resources, &intent)?;
    scheduler::lifecycle_transaction_with(store, |tx| {
        let current =
            preconditions_for_purpose(tx, &intent, &filesystem, allow_paused_start, None)?;
        let frozen = admission
            .selection
            .freeze_record(&current, &identity, Instant::now())
            .map_err(catalog_admission_error)?;
        let cwd = if let Some(token) = &token {
            token
                .validate(tx)?
                .map_err(|_| refusal("workspace-readiness/token-stale".into()))?
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
            resources,
            &frozen,
            &generation.to_string(),
            store.root(),
            filesystem.progress_directory.clone(),
        )
    })
}

/// Prepare the safety retry's thread fork and turn start from the same checked
/// thread snapshot and permission builders used by ordinary turn preparation.
/// The safety operation owns durable intent and provider receipts; this helper
/// only returns immutable request maps.
pub(crate) fn prepare_safety_retry(
    store: &RuntimeStore,
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
    let record = AgentRepository::get(&store.read()?, &AgentId::from(request.agent_id.as_str()))?
        .ok_or_else(|| invalid("agent"))?
        .value();
    if record.get("threadId").and_then(Value::as_str) != Some(&request.source_thread_id)
        || record
            .get("deletedAt")
            .is_some_and(|value| !value.is_null())
    {
        return Err(refusal("safety-retry/agent-changed".into()));
    }
    let filesystem = FilesystemSnapshot::capture(store, &request.agent_id)?;
    let connection_id = record
        .get("startAttempt")
        .and_then(|value| value.get("connectionId"))
        .and_then(Value::as_str)
        .unwrap_or("safety-retry-preparation");
    let mut snapshot = scheduler::lifecycle_transaction_with(store, |tx| {
        let current = agent(tx, &request.agent_id)?.value();
        if current.get("threadId").and_then(Value::as_str) != Some(&request.source_thread_id)
            || current.get("epoch") != record.get("epoch")
        {
            return Err(refusal("safety-retry/agent-changed".into()));
        }
        thread_snapshot(
            tx,
            resources,
            &current,
            connection_id,
            store.root(),
            filesystem.progress_directory.clone(),
        )
    })?;
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
pub(crate) fn prepare_ready(
    store: &RuntimeStore,
    resources: &PreparationResources,
    intent: &TurnIntent,
    method: Method,
    generation: u64,
    admission: Admission,
    token: crate::workspace_ready::ReadyToken,
) -> Result<RequestPreparation, WorkerError> {
    let Admission { selection } = admission;
    // Workspace provisioning can update stored agent/project paths as well as
    // create the progress directory. Capture all filesystem observations after
    // that work, before the transaction rechecks the current records.
    let filesystem = FilesystemSnapshot::capture(store, &intent.agent_id)?;
    let prepared_host = if intent.radio_authorized {
        let prepared = crate::host_project_runtime::PreparedHost::load(store)
            .map_err(|error| refusal(error.to_string()))?;
        prepared
            .recheck_paths()
            .map_err(|error| refusal(error.to_string()))?;
        Some(prepared)
    } else {
        None
    };
    let (identity, _) = catalog(resources, intent)?;
    let (mut snapshot, mut turn_snapshot, events, mut prepared_intent) =
        scheduler::lifecycle_transaction_with(store, |tx| {
            let record = preconditions(tx, intent, &filesystem, prepared_host.as_ref())?;
            let mut frozen = selection
                .freeze_record(&record, &identity, Instant::now())
                .map_err(|e| refusal(e.to_string()))?;
            let cwd = token
                .validate(tx)?
                .map_err(|_| refusal("workspace-readiness/token-stale".into()))?;
            frozen["cwd"] = json!(cwd);
            if frozen.get("nativeEffort").is_none() {
                frozen["effort"] = json!(selection.settings.effort);
                frozen["nativeEffort"] = json!(selection.settings.native_effort);
            }
            let snapshot = thread_snapshot(
                tx,
                resources,
                &frozen,
                &generation.to_string(),
                store.root(),
                filesystem.progress_directory.clone(),
            )?;
            let (turn_snapshot, events) =
                submission_snapshot(tx, resources, &frozen, intent, store.root())?;
            let mut prepared_intent = intent.clone();
            prepared_intent.cwd = Some(snapshot.actor.cwd.clone());
            prepared_intent.model = snapshot.settings.model.clone();
            prepared_intent.thread_id = snapshot.actor.thread_id.clone();
            Ok::<_, WorkerError>((snapshot, turn_snapshot, events, prepared_intent))
        })?;
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
    if method == Method::ThreadResume {
        let method = if snapshot.actor.thread_id.is_some() {
            Method::ThreadResume
        } else {
            Method::ThreadStart
        };
        let operation = format!(
            "account:{}:{}:{}",
            intent.account_key,
            intent.attempt_id,
            method.as_str()
        );
        let attempt = match thread::plan(&snapshot, &intent.attempt_id, &operation) {
            thread::Outcome::Start(attempt) | thread::Outcome::Resume(attempt) => *attempt,
            thread::Outcome::Hold(e) => return Err(refusal(format!("thread-preparation/{e:?}"))),
            thread::Outcome::Refuse(e) => return Err(refusal(format!("thread-preparation/{e:?}"))),
            thread::Outcome::Ready(_) => {
                return Err(refusal("thread-preparation/unexpected-loaded-proof".into()));
            }
        };
        for artifact in &attempt.artifacts {
            let thread::ArtifactIntent::RememberRole { version, text } = artifact;
            materialize(
                store.root(),
                &format!("context-snapshots/{version}.txt"),
                text,
            )?;
        }
        return Ok(RequestPreparation {
            intent: prepared_intent,
            method,
            params: json!(attempt.params),
            thread: Some(attempt),
            compiled: None,
            submission: None,
            guard,
        });
    }
    continuation_gate(&snapshot, &intent.attempt_id)?;
    snapshot.actor.thread_id = prepared_intent.thread_id.clone();
    turn_snapshot.operation_id = format!(
        "account:{}:{}:{}",
        intent.account_key,
        intent.attempt_id,
        method.as_str()
    );
    let compiled = match turn::compile(&turn_snapshot, &events) {
        Ok(compiled) => compiled,
        Err(turn::PrepareError::ArtifactsRequired(artifacts)) => {
            for artifact in artifacts {
                let path = materialize(store.root(), &artifact.relative_path, &artifact.body)?;
                turn_snapshot.artifact_paths.insert(artifact.event_id, path);
            }
            turn::compile(&turn_snapshot, &events).map_err(|e| refusal(e.to_string()))?
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
    params.insert("input".into(), json!(compiled.provider_inputs));
    params.insert(
        "clientUserMessageId".into(),
        json!(compiled.event_ids.first()),
    );
    let mut submission = crate::notification_dispatch::PreparedSubmission {
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
    if intent.provider == "claude" {
        let prepared = scheduler::lifecycle_transaction_with(store, |tx| {
            Ok::<_, WorkerError>(crate::claude_input_recovery::runtime::start_input(
                tx,
                &intent.agent_id,
                &intent.attempt_id,
                claude_transport(&intent.account_key, generation)?,
                Value::Object(params.clone()),
                submission.text.clone(),
            )?)
        })?;
        params = prepared
            .params
            .as_object()
            .cloned()
            .ok_or_else(|| invalid("claude params"))?;
        submission.text = prepared.text;
    }
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
    let record = preconditions(
        tx,
        &context.intent,
        &guard.filesystem,
        guard.prepared_host.as_ref(),
    )?;
    let (identity, _) = catalog(&guard.resources, &context.intent)?;
    let mut frozen = guard
        .selection
        .freeze_record(&record, &identity, Instant::now())
        .map_err(|e| refusal(e.to_string()))?;
    let cwd = guard
        .token
        .validate(tx)?
        .map_err(|_| refusal("workspace-readiness/token-stale".into()))?;
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
        let (mut current_submission, current_events) = submission_snapshot(
            tx,
            &guard.resources,
            &current,
            &context.intent,
            &guard.state_root,
        )?;
        // Recompile with the exact operation, clock and verified artifact paths.
        // The compiler's manifest and output record the effective dependencies;
        // unrelated worker state must not invalidate a non-lead submission.
        current_submission.prepared_at = baseline.prepared_at;
        current_submission.operation_id = baseline.operation_id.clone();
        current_submission.artifact_paths = baseline.artifact_paths.clone();
        let recompiled = turn::compile(&current_submission, &current_events)
            .map_err(|e| refusal(e.to_string()))?;
        if serde_json::to_value(recompiled).map_err(|_| invalid("compiler result"))?
            != serde_json::to_value(compiled).map_err(|_| invalid("compiler result"))?
        {
            return Err(refusal("turn-preparation/snapshot-changed".into()));
        }
    }
    let current = thread_snapshot(
        tx,
        &guard.resources,
        &current,
        &guard.thread.account.connection_id,
        &guard.state_root,
        guard.filesystem.progress_directory.clone(),
    )?;
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
    if let Some(attempt) = frozen
        .get_mut("startAttempt")
        .and_then(Value::as_object_mut)
    {
        attempt.remove("prepareError");
    }
    AgentRepository::put(tx, &decode(frozen, &context.intent.agent_id)?)?;
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
    generation: u64,
) -> Result<crate::claude_input_recovery::TransportSnapshot, WorkerError> {
    decode(
        json!({"accountKey":account,"connectionId":generation.to_string(),"supervisorIdentity":null,"closed":false}),
        "retained Claude binding",
    )
}
pub(crate) fn commit_claude(
    tx: &Transaction<'_>,
    intent: &TurnIntent,
    request_id: &str,
    generation: u64,
    submission: &crate::notification_dispatch::PreparedSubmission,
    guard: &CommitGuard,
) -> Result<bool, WorkerError> {
    crate::claude_input_recovery::runtime::submission::commit_start(
        tx,
        intent,
        request_id,
        crate::claude_input_recovery::runtime::SubmittedBinding {
            transport: claude_transport(&intent.account_key, generation)?,
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
                provider: thread::Provider::Codex,
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
    }
}
