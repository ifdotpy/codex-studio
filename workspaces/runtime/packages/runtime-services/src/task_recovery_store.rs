//! Atomic visible task recovery states; terminal task output still uses the
//! integrator's canonical task completion writer.
use crate::{
    recovery::RecoveryError,
    recovery_absence::{LOST_COMMAND, RecoveryObservation, TaskAbsenceDecision},
    recovery_history::ConnectionScope,
    scheduler::lifecycle_transaction_with,
    task_recovery::{TaskRecoveryEffects, TaskRecoverySnapshot},
};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRepository, RuntimeStore, TaskId, TaskRecord, TaskRepository, Transaction,
};

pub trait TaskCompletionWriter {
    fn resolve_receipt(&mut self, receipt: &Value) -> Result<Value, RecoveryError>;
    fn complete(
        &mut self,
        expected: &TaskRecoverySnapshot,
        scope: &ConnectionScope,
        item: Value,
    ) -> Result<bool, RecoveryError>;
}
pub struct TaskRecoveryStore<'a> {
    pub store: &'a RuntimeStore,
    /// Synchronous worker-owned observation, checked under the writer lock.
    pub observation_current: &'a dyn Fn(&ConnectionScope) -> Option<RecoveryObservation>,
    pub completion: &'a mut dyn TaskCompletionWriter,
}
impl TaskRecoveryEffects for TaskRecoveryStore<'_> {
    fn resolve_receipt(&mut self, receipt: &Value) -> Result<Value, RecoveryError> {
        self.completion.resolve_receipt(receipt)
    }
    fn complete(
        &mut self,
        expected: &TaskRecoverySnapshot,
        scope: &ConnectionScope,
        item: Value,
    ) -> Result<bool, RecoveryError> {
        self.completion.complete(expected, scope, item)
    }
    fn apply_absence(
        &mut self,
        expected: &TaskRecoverySnapshot,
        scope: &ConnectionScope,
        observation: &RecoveryObservation,
        decision: TaskAbsenceDecision,
    ) -> Result<bool, RecoveryError> {
        lifecycle_transaction_with(self.store, |tx| {
            let Some(mut current) = (self.observation_current)(scope) else {
                return Ok(false);
            };
            let a = expected.agent().record();
            let t = expected.task();
            let agent = a["id"].as_str().ok_or(RecoveryError::CommitFailed)?;
            let id = t["id"].as_str().ok_or(RecoveryError::CommitFailed)?;
            if !current.valid(scope)
                || !observation.valid(scope)
                || current.supervisor != observation.supervisor
                || a.get("accountKey")
                    .and_then(Value::as_str)
                    .unwrap_or("default")
                    != scope.account
                || !expected.agent().eligibility().task_owner
                || t["status"] != "running"
                || t["agent"] != agent
                || id != format!("{agent}:{}", t["itemId"].as_str().unwrap_or_default())
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
                || TaskRepository::get(tx, &TaskId::from(id))?
                    .as_ref()
                    .map(TaskRecord::value)
                    != Some(t)
                || !receipts_current(tx, expected)?
            {
                return Ok(false);
            }
            let completed = format!(
                "{agent}:{}",
                t["turnId"].as_str().ok_or(RecoveryError::CommitFailed)?
            );
            current.completed_turn = tx
                .query_row(
                    "SELECT id FROM runtime_completed_turns WHERE id=?1",
                    [&completed],
                    |r| r.get(0),
                )
                .optional()?;
            let decision = match decision {
                TaskAbsenceDecision::Lost => expected.absence_decision(&current, observation.quiet),
                hold => hold,
            };
            let mut updated = t.clone();
            match decision {
                TaskAbsenceDecision::Lost => {
                    updated["status"] = json!("lost");
                    updated["finished"] = json!(current.at);
                    updated["error"] = json!(LOST_COMMAND);
                    updated
                        .as_object_mut()
                        .ok_or(RecoveryError::CommitFailed)?
                        .remove("recoveryHold");
                }
                TaskAbsenceDecision::Hold { reason } => {
                    updated["recoveryHold"] =
                        json!({"stage":"held","at":current.at,"reason":reason,"outcome":"unknown"});
                    updated["error"] = json!(format!(
                        "Task recovery held: {reason}. Outcome unknown; work was not replayed."
                    ));
                }
            }
            TaskRepository::put(
                tx,
                &TaskRecord::from_value(updated.clone())
                    .map_err(|_| RecoveryError::CommitFailed)?,
            )?;
            if updated["status"] == "lost" {
                tx.execute("UPDATE runtime_items SET record=json_set(record,'$.toolStatus','lost') WHERE id=?1 AND agent=?2 AND json_extract(record,'$.turnId')=?3",
                    rusqlite::params![id, agent, t["turnId"].as_str()])?;
            }
            let projection = crate::sync_views::project_persisted_record("task", &updated)
                .ok_or(RecoveryError::CommitFailed)?;
            studio_runtime_store::SyncDocumentsRepository::put_entity(
                tx, "task", id, projection, false,
            )?;
            Ok(true)
        })
    }
}
pub(crate) fn receipts_current(
    tx: &Transaction<'_>,
    expected: &TaskRecoverySnapshot,
) -> Result<bool, RecoveryError> {
    let (aliases, receipts) = receipt_state(tx, expected.agent().record(), expected.task())?;
    Ok(aliases == expected.aliases && receipts == expected.receipts)
}
type ReceiptState = (Vec<String>, Vec<(String, Value)>);
pub(crate) fn receipt_state(
    tx: &rusqlite::Connection,
    a: &Value,
    t: &Value,
) -> Result<ReceiptState, RecoveryError> {
    if t["type"] != "dynamicToolCall" {
        return Ok((vec![], vec![]));
    }
    let aliases: Vec<String> = tx.prepare("SELECT request FROM runtime_tool_request_aliases WHERE agent=?1 AND alias=?2 ORDER BY request LIMIT 2")?
        .query_map(rusqlite::params![a["id"].as_str(),t["itemId"].as_str()], |r| r.get(0))?.collect::<Result<_,_>>()?;
    if aliases.len() > 1 {
        return Ok((aliases, vec![]));
    }
    let account = a
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let prefix = if account == "default" {
        String::new()
    } else {
        format!("{account}:")
    };
    let key = aliases.first().cloned().unwrap_or_else(|| {
        format!(
            "{prefix}{}:{}",
            a["threadId"].as_str().unwrap_or_default(),
            t["itemId"].as_str().unwrap_or_default()
        )
    });
    let row: Option<(String, String)> = tx
        .query_row(
            "SELECT id,record FROM runtime_tool_requests WHERE id=?1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let rows = row
        .map(|(key, raw)| {
            serde_json::from_str(&raw)
                .map_err(|_| studio_runtime_store::StoreError::InvalidStoredObject {
                    table: "runtime_tool_requests",
                    id: key.clone(),
                })
                .map(|v| (key, v))
        })
        .transpose()?
        .into_iter()
        .collect::<Vec<_>>();
    Ok((aliases, rows))
}

/// P06 supplies notification_dispatch::recover_task_in_transaction directly.
/// The signature deliberately matches that accessor; no provider cursor is invented.
pub type TaskRowWriter<'a> = dyn Fn(
        &Transaction<'_>,
        &Value,
        &Value,
        &Value,
        f64,
    ) -> Result<(), crate::notification_dispatch::NotificationDispatchError>
    + 'a;
pub type ReceiptResolver<'a> = dyn Fn(&Value) -> Result<Value, RecoveryError> + 'a;

pub struct RuntimeTaskCompletionWriter<'a> {
    pub store: &'a RuntimeStore,
    pub observation_current: &'a crate::recovery_runtime::ObservationSource<'a>,
    pub write_task: &'a TaskRowWriter<'a>,
    pub resolve_payload: &'a ReceiptResolver<'a>,
}
impl TaskCompletionWriter for RuntimeTaskCompletionWriter<'_> {
    fn resolve_receipt(&mut self, receipt: &Value) -> Result<Value, RecoveryError> {
        // Payload IO occurs here, before opening the completion transaction.
        let resolved = (self.resolve_payload)(receipt)?;
        if resolved.to_string().len() > crate::recovery_history::MAX_RESPONSE_BYTES {
            return Err(crate::recovery_history::HistoryError::Capacity.into());
        }
        Ok(resolved)
    }
    fn complete(
        &mut self,
        expected: &TaskRecoverySnapshot,
        scope: &ConnectionScope,
        item: Value,
    ) -> Result<bool, RecoveryError> {
        lifecycle_transaction_with(self.store, |tx| {
            let Some(current) = (self.observation_current)(scope).filter(|o| o.valid(scope)) else {
                return Ok(false);
            };
            let agent = expected.agent().record();
            let task = expected.task();
            let id = task["id"].as_str().ok_or(RecoveryError::CommitFailed)?;
            if agent
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
                != scope.account
                || !expected.agent().eligibility().task_owner
                || task["status"] != "running"
                || task["agent"] != agent["id"]
                || item["id"] != task["itemId"]
                || item["type"] != task["type"]
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
                || TaskRepository::get(tx, &TaskId::from(id))?
                    .as_ref()
                    .map(TaskRecord::value)
                    != Some(task)
                || !receipts_current(tx, expected)?
            {
                return Ok(false);
            }
            (self.write_task)(tx, agent, task, &item, current.at).map_err(|error| match error {
                crate::notification_dispatch::NotificationDispatchError::Store(error) => {
                    RecoveryError::Store(error)
                }
                _ => RecoveryError::CommitFailed,
            })?;
            let written =
                TaskRepository::get(tx, &TaskId::from(id))?.ok_or(RecoveryError::CommitFailed)?;
            if written.value()["status"] == "running" {
                return Err(RecoveryError::CommitFailed);
            }
            if task.get("recoveryHold").is_some() {
                let mut clean = written.value().clone();
                clean
                    .as_object_mut()
                    .ok_or(RecoveryError::CommitFailed)?
                    .remove("recoveryHold");
                if clean.get("error") == task.get("error")
                    && item.get("error").is_none_or(Value::is_null)
                {
                    clean
                        .as_object_mut()
                        .ok_or(RecoveryError::CommitFailed)?
                        .remove("error");
                }
                TaskRepository::put(
                    tx,
                    &TaskRecord::from_value(clean.clone())
                        .map_err(|_| RecoveryError::CommitFailed)?,
                )?;
                let projection = crate::sync_views::project_persisted_record("task", &clean)
                    .ok_or(RecoveryError::CommitFailed)?;
                studio_runtime_store::SyncDocumentsRepository::put_entity(
                    tx, "task", id, projection, false,
                )?;
            }
            Ok(true)
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
