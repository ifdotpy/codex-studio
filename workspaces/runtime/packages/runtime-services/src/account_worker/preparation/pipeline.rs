//! The only adapter allowed to advance a claimed batch to provider submission.
use super::*;
use crate::{submit_gates as gates, thread_prepare as thread, turn_prepare as turn};
use snapshot::*;
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
}
pub(crate) struct Admission {
    selection: gates::ExecutionSelection,
    filesystem: FilesystemSnapshot,
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
                        hidden: v.get("hidden").and_then(Value::as_bool).unwrap_or(false),
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
) -> Result<Value, WorkerError> {
    let record = agent(tx, &intent.agent_id)?.value();
    if record["epoch"] != intent.epoch
        || record["startAttempt"]["id"] != intent.attempt_id
        || record["startAttempt"]["submitted"] != false
        || record["autoWake"] != true
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
    authorize(tx, intent, filesystem)?;
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
pub(crate) fn admit(
    store: &RuntimeStore,
    resources: &PreparationResources,
    intent: &TurnIntent,
) -> Result<Admission, WorkerError> {
    let filesystem = FilesystemSnapshot::capture(store, &intent.agent_id)?;
    let (identity, observation) = catalog(resources, intent)?;
    scheduler::lifecycle_transaction_with(store, |tx| {
        let record = preconditions(tx, intent, &filesystem)?;
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
            filesystem,
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
    let selection = admit(store, resources, intent)?;
    let token = workspace(store, intent)?;
    prepare_ready(
        store, resources, intent, method, generation, selection, token,
    )
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
    let Admission {
        selection,
        mut filesystem,
    } = admission;
    // Workspace provisioning may have created the progress directory.
    filesystem.progress_directory =
        FilesystemSnapshot::capture(store, &intent.agent_id)?.progress_directory;
    let (identity, _) = catalog(resources, intent)?;
    let (mut snapshot, mut turn_snapshot, events, mut prepared_intent) =
        scheduler::lifecycle_transaction_with(store, |tx| {
            let record = preconditions(tx, intent, &filesystem)?;
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
    let record = preconditions(tx, &context.intent, &guard.filesystem)?;
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
