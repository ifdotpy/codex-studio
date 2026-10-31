//! Atomic visible task recovery states; terminal task output still uses the
//! integrator's canonical task completion writer.
use crate::{
    recovery::RecoveryError,
    recovery_absence::{LOST_COMMAND, RecoveryObservation, TaskAbsenceDecision},
    recovery_history::ConnectionScope,
    recovery_runtime::ObservationSource,
    scheduler::lifecycle_transaction_with,
    task_recovery::{TaskRecoveryEffects, TaskRecoverySnapshot},
};
use serde_json::Value;
use std::sync::Arc;
use studio_runtime_store::{
    AgentId, AgentRepository, RuntimeStore, TaskId, TaskRecord, TaskRepository, Transaction,
};

pub trait TaskCompletionWriter: Send + Sync {
    fn resolve_receipt<'a>(
        &'a self,
        receipt: Arc<Value>,
    ) -> futures_util::future::BoxFuture<'a, Result<Value, RecoveryError>>;
    fn complete<'a>(
        &'a self,
        expected: &'a TaskRecoverySnapshot,
        scope: &'a ConnectionScope,
        item: Value,
    ) -> futures_util::future::BoxFuture<'a, Result<bool, RecoveryError>>;
}
pub struct TaskRecoveryStore {
    pub store: Arc<RuntimeStore>,
    /// Synchronous worker-owned observation, checked under the writer lock.
    pub observation_current: Arc<ObservationSource>,
    pub completion: Arc<dyn TaskCompletionWriter>,
}
impl TaskRecoveryEffects for TaskRecoveryStore {
    fn resolve_receipt<'a>(
        &'a mut self,
        receipt: Arc<Value>,
    ) -> futures_util::future::BoxFuture<'a, Result<Value, RecoveryError>> {
        self.completion.resolve_receipt(receipt)
    }
    fn complete<'a>(
        &'a mut self,
        expected: &'a TaskRecoverySnapshot,
        scope: &'a ConnectionScope,
        item: Value,
    ) -> futures_util::future::BoxFuture<'a, Result<bool, RecoveryError>> {
        self.completion.complete(expected, scope, item)
    }
    fn apply_absence<'a>(
        &'a mut self,
        expected: &'a TaskRecoverySnapshot,
        scope: &'a ConnectionScope,
        observation: &'a RecoveryObservation,
        decision: TaskAbsenceDecision,
    ) -> futures_util::future::BoxFuture<'a, Result<bool, RecoveryError>> {
        let store = Arc::clone(&self.store);
        let observation_current = Arc::clone(&self.observation_current);
        let expected = expected.clone();
        let scope = scope.clone();
        let observation = observation.clone();
        Box::pin(async move {
            lifecycle_transaction_with(&store, move |tx| {
            let Some(mut current) = (observation_current)(&scope) else {
                return Ok(false);
            };
            let a = expected.agent().record();
            let t = expected.task();
            let agent = a["id"].as_str().ok_or(RecoveryError::CommitFailed)?;
            let id = t.id().as_ref();
            if !current.valid(&scope)
                || !observation.valid(&scope)
                || current.supervisor != observation.supervisor
                || a.get("accountKey")
                    .and_then(Value::as_str)
                    .unwrap_or("default")
                    != scope.account
                || !expected.agent().eligibility().task_owner
                || t.status() != studio_runtime_schema::task_record::TaskStatus::Running
                || t.agent() != agent
                || id != format!("{agent}:{}", t.item_id())
            {
                return Ok(false);
            }
            let raw = AgentRepository::get_raw(tx, &AgentId::from(agent))?;
            if raw
                .as_deref()
                .map(serde_json::from_str::<Value>)
                .transpose()
                .map_err(|_| RecoveryError::CommitFailed)?
                .as_ref()
                != Some(a)
            {
                return Ok(false);
            }
            let stored_task = TaskRepository::get(tx, &TaskId::from(id))?;
            if stored_task.as_ref() != Some(t)
                || !receipts_current(tx, &expected)?
            {
                return Ok(false);
            }
            let completed = format!(
                "{agent}:{}",
                t.turn_id()
            );
            current.completed_turn = studio_runtime_store::domain::turn_completed(
                tx,
                &agent,
                t.turn_id(),
            )?
            .then_some(completed);
            let decision = match decision {
                TaskAbsenceDecision::Lost => expected.absence_decision(&current, observation.quiet),
                hold @ TaskAbsenceDecision::Hold { .. } => hold,
            };
            let mut updated = stored_task.ok_or(RecoveryError::CommitFailed)?;
            match decision {
                TaskAbsenceDecision::Lost => {
                    updated.set_status(studio_runtime_schema::task_record::TaskStatus::Lost);
                    updated.set_finished(current.at);
                    updated.set_error(LOST_COMMAND.into());
                    updated.remove_recovery_hold();
                }
                TaskAbsenceDecision::Hold { reason } => {
                    updated.set_recovery_hold(studio_runtime_schema::task_record::TaskRecoveryHold {
                        stage: studio_runtime_schema::task_record::RecoveryHoldStage::Held,
                        at: current.at,
                        reason: reason.to_owned(),
                        outcome: studio_runtime_schema::task_record::RecoveryHoldOutcome::Unknown,
                    });
                    updated.set_error(format!("Task recovery held: {reason}. Outcome unknown; work was not replayed."));
                }
            }
            TaskRepository::put(tx, &updated)?;
            if updated.status() == studio_runtime_schema::task_record::TaskStatus::Lost {
                tx.execute("UPDATE runtime_items SET record=json_set(record,'$.toolStatus','lost') WHERE id=?1 AND agent=?2 AND json_extract(record,'$.turnId')=?3",
                    rusqlite::params![id, agent, t.turn_id()])?;
            }
            let projection = crate::sync_views::project_persisted_record("task", &studio_runtime_store::projection_value(&updated))
                .ok_or(RecoveryError::CommitFailed)?;
            studio_runtime_store::SyncDocumentsRepository::put_entity(
                tx, "task", id, projection, false,
            )?;
            Ok(true)
        }).await
        })
    }
}
pub(crate) fn receipts_current(
    tx: &Transaction<'_>,
    expected: &TaskRecoverySnapshot,
) -> Result<bool, RecoveryError> {
    let (aliases, receipts) = receipt_state(tx, expected.agent().record(), expected.task())?;
    Ok(aliases == expected.aliases
        && receipts.len() == expected.receipts.len()
        && receipts.iter().zip(&expected.receipts).all(
            |((key, current), (expected_key, expected_value))| {
                key == expected_key && current == expected_value.as_ref()
            },
        ))
}
type ReceiptState = (Vec<String>, Vec<(String, Value)>);
fn receipt_error(error: crate::tool_requests::ToolRequestError, key: &str) -> RecoveryError {
    use crate::tool_requests::ToolRequestError;
    use studio_runtime_store::StoreError;
    match error {
        ToolRequestError::Store(
            error @ (StoreError::InvalidStoredObject { .. }
            | StoreError::InvalidStoredRecord { .. }
            | StoreError::InvalidSyncObject { .. }
            | StoreError::InvalidSyncObjectField { .. }),
        ) => error.into(),
        ToolRequestError::Store(StoreError::Json(_))
        | ToolRequestError::RequestResult(
            studio_runtime_store::request_results::RequestResultError::Json(_),
        ) => StoreError::InvalidStoredObject {
            table: "runtime_tool_requests",
            id: key.into(),
        }
        .into(),
        ToolRequestError::Store(error) => error.into(),
        ToolRequestError::RequestResult(error) => RecoveryError::Store(error.into()),
        ToolRequestError::InvalidRequest(message) => StoreError::RecordContract(message).into(),
        ToolRequestError::RequiresDependency(name) => RecoveryError::Dependency(name),
        ToolRequestError::Payload(_) => RecoveryError::EffectsUnavailable,
    }
}
pub(crate) fn receipt_state(
    tx: &rusqlite::Connection,
    a: &Value,
    t: &TaskRecord,
) -> Result<ReceiptState, RecoveryError> {
    if t.task_type() != studio_runtime_schema::task_record::TaskType::DynamicToolCall {
        return Ok((vec![], vec![]));
    }
    let actor = a["id"].as_str().ok_or(RecoveryError::CommitFailed)?;
    let call = t.item_id();
    let Some(mut row) = crate::tool_requests::resolve(tx, actor, call)
        .map_err(|error| receipt_error(error, call))?
    else {
        return Ok((vec![], vec![]));
    };
    if row["stage"] == "ambiguous" {
        return Err(RecoveryError::CommitFailed);
    }
    let key = row["id"]
        .as_str()
        .ok_or(RecoveryError::CommitFailed)?
        .to_owned();
    if let Some(result) =
        crate::tool_requests::saved_result(tx, &key).map_err(|error| receipt_error(error, &key))?
    {
        row["result"] = result;
    } else if !row["finished"].is_null() {
        return Err(RecoveryError::CommitFailed);
    }
    Ok((vec![key.clone()], vec![(key, row)]))
}

/// P06 supplies notification_dispatch::recover_task_in_transaction directly.
/// The signature deliberately matches that accessor; no provider cursor is invented.
pub type TaskRowWriter = dyn Fn(
        &Transaction<'_>,
        &Value,
        &TaskRecord,
        &Value,
        f64,
    ) -> Result<(), crate::notification_dispatch::NotificationDispatchError>
    + Send
    + Sync;
pub type ReceiptResolver = dyn Fn(&Value) -> Result<Value, RecoveryError> + Send + Sync;

pub struct RuntimeTaskCompletionWriter {
    pub store: Arc<RuntimeStore>,
    pub observation_current: Arc<crate::recovery_runtime::ObservationSource>,
    pub write_task: Arc<TaskRowWriter>,
    pub resolve_payload: Arc<ReceiptResolver>,
}
impl TaskCompletionWriter for RuntimeTaskCompletionWriter {
    fn resolve_receipt<'a>(
        &'a self,
        receipt: Arc<Value>,
    ) -> futures_util::future::BoxFuture<'a, Result<Value, RecoveryError>> {
        let resolver = Arc::clone(&self.resolve_payload);
        Box::pin(async move {
            // Payload IO remains on the blocking pool and completes before the
            // task completion transaction opens.
            let resolved = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/task_recovery_store",
                move || resolver(&receipt),
            )
            .await
            .map_err(|_| RecoveryError::EffectsUnavailable)??;
            if resolved.to_string().len() > crate::recovery_history::MAX_RESPONSE_BYTES {
                return Err(crate::recovery_history::HistoryError::Capacity.into());
            }
            Ok(resolved)
        })
    }
    fn complete<'a>(
        &'a self,
        expected: &'a TaskRecoverySnapshot,
        scope: &'a ConnectionScope,
        item: Value,
    ) -> futures_util::future::BoxFuture<'a, Result<bool, RecoveryError>> {
        let store = Arc::clone(&self.store);
        let observation_current = Arc::clone(&self.observation_current);
        let write_task = Arc::clone(&self.write_task);
        let expected = expected.clone();
        let scope = scope.clone();
        Box::pin(async move {
            lifecycle_transaction_with(&store, move |tx| {
                let Some(current) = (observation_current)(&scope).filter(|o| o.valid(&scope))
                else {
                    return Ok(false);
                };
                let agent = expected.agent().record();
                let task = expected.task();
                let id = task.id().as_ref();
                if agent
                    .get("accountKey")
                    .and_then(Value::as_str)
                    .unwrap_or("default")
                    != scope.account
                    || !expected.agent().eligibility().task_owner
                    || task.status() != studio_runtime_schema::task_record::TaskStatus::Running
                    || Some(task.agent()) != agent["id"].as_str()
                    || item["id"].as_str() != Some(task.item_id())
                    || item["type"].as_str() != Some(task.task_type().as_str())
                    || !matches!(
                        item["status"].as_str(),
                        Some("completed" | "failed" | "declined")
                    )
                {
                    return Ok(false);
                }
                let owner = agent["id"].as_str().ok_or(RecoveryError::CommitFailed)?;
                let raw = AgentRepository::get_raw(tx, &AgentId::from(owner))?;
                if raw
                    .as_deref()
                    .map(serde_json::from_str::<Value>)
                    .transpose()
                    .map_err(|_| RecoveryError::CommitFailed)?
                    .as_ref()
                    != Some(agent)
                    || TaskRepository::get(tx, &TaskId::from(id))?.as_ref() != Some(task)
                    || !receipts_current(tx, &expected)?
                {
                    return Ok(false);
                }
                (write_task)(tx, agent, task, &item, current.at).map_err(|error| match error {
                    crate::notification_dispatch::NotificationDispatchError::RequestResult(error) => RecoveryError::Store(error.into()),
                    crate::notification_dispatch::NotificationDispatchError::Store(error) => {
                        RecoveryError::Store(error)
                    }
                    crate::notification_dispatch::NotificationDispatchError::InvalidNotification { .. } | crate::notification_dispatch::NotificationDispatchError::InvalidAccountState { .. } | crate::notification_dispatch::NotificationDispatchError::StreamCapacity | crate::notification_dispatch::NotificationDispatchError::InvalidStoredObject { .. } | crate::notification_dispatch::NotificationDispatchError::ConflictingDelivery | crate::notification_dispatch::NotificationDispatchError::ConflictingInput | crate::notification_dispatch::NotificationDispatchError::UnauthorizedSubmission | crate::notification_dispatch::NotificationDispatchError::Dependency(_) | crate::notification_dispatch::NotificationDispatchError::Move(_) | crate::notification_dispatch::NotificationDispatchError::Json(_) => RecoveryError::CommitFailed,
                })?;
                let written = TaskRepository::get(tx, &TaskId::from(id))?
                    .ok_or(RecoveryError::CommitFailed)?;
                if written.status() == studio_runtime_schema::task_record::TaskStatus::Running {
                    return Err(RecoveryError::CommitFailed);
                }
                if task.has_recovery_hold() {
                    let mut clean = written;
                    clean.remove_recovery_hold();
                    if clean.error() == task.error()
                        && item.get("error").is_none_or(Value::is_null)
                    {
                        clean.remove_error();
                    }
                    TaskRepository::put(tx, &clean)?;
                    let projection = crate::sync_views::project_persisted_record("task", &studio_runtime_store::projection_value(&clean))
                        .ok_or(RecoveryError::CommitFailed)?;
                    studio_runtime_store::SyncDocumentsRepository::put_entity(
                        tx, "task", id, projection, false,
                    )?;
                }
                Ok(true)
            })
            .await
        })
    }
}
/// Inline receipts are complete on this base. Blob references need the payload
/// owner's resolver; silently returning their preview would lose task output.
pub fn resolve_inline_receipt(receipt: &Value) -> Result<Value, RecoveryError> {
    let invalid = || studio_runtime_store::StoreError::InvalidStoredObject {
        table: "runtime_tool_requests",
        id: receipt["id"].as_str().unwrap_or("<missing>").into(),
    };
    if crate::event_write::truthy(receipt.get("_payloadBlobs"))
        || crate::event_write::truthy(receipt["result"].get("_payloadBlobs"))
    {
        return Err(RecoveryError::Dependency(
            "payload-store/tool-request-recovery",
        ));
    }
    if !studio_runtime_schema::validate("RecoveryToolReceipt", receipt).map_err(|_| invalid())? {
        return Err(invalid().into());
    }
    Ok(receipt.clone())
}
