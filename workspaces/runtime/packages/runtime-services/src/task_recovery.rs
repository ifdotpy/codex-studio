//! A local tool receipt takes precedence over native history. Unknown local
//! mutations cannot be retired by a native delivery error.
use crate::recovery_policy::{InvalidRecoveryRecord, RecoverySnapshot};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_schema::task_record::{TaskKind, TaskStatus, TaskType};
use studio_runtime_store::TaskRecord;

const MAX_RECEIPTS: usize = 2;
#[derive(Clone)]
pub struct TaskRecoverySnapshot {
    agent: RecoverySnapshot,
    task: Arc<TaskRecord>,
    pub(crate) aliases: Vec<String>,
    pub(crate) receipts: Vec<(String, Arc<Value>)>,
}
#[derive(Clone, Debug, PartialEq)]
pub enum TaskRecoveryDecision {
    Unconfirmed,
    /// The completion adapter must recheck this snapshot in its write transaction.
    Complete(Value),
}
impl TaskRecoverySnapshot {
    pub fn from_records(
        agent: Value,
        task: TaskRecord,
        aliases: Vec<String>,
        receipts: Vec<(String, Value)>,
    ) -> Result<Self, InvalidRecoveryRecord> {
        let agent_id = agent["id"].as_str().unwrap_or("<missing>").to_owned();
        let task_id = task.id().as_ref().to_owned();
        Self::from_stored_records(&agent_id, &task_id, agent, task, aliases, receipts)
    }
    pub(crate) fn from_stored_records(
        agent_id: &str,
        task_id: &str,
        agent: Value,
        task: TaskRecord,
        aliases: Vec<String>,
        receipts: Vec<(String, Value)>,
    ) -> Result<Self, InvalidRecoveryRecord> {
        let located = |table: &'static str, id: &str| InvalidRecoveryRecord::Stored {
            table,
            id: id.into(),
        };
        let agent = RecoverySnapshot::from_record(agent)
            .map_err(|_| located("runtime_agents", agent_id))?;
        if task.id().as_ref() != task_id
            || task.agent().is_empty()
            || task.item_id().is_empty()
            || task.turn_id().is_empty()
            || aliases.len() > MAX_RECEIPTS
            || receipts.len() > MAX_RECEIPTS
        {
            return Err(located("runtime_tasks", task_id));
        }
        for (key, receipt) in &receipts {
            if key.is_empty()
                || !studio_runtime_schema::validate("RecoveryToolReceipt", receipt)
                    .map_err(|_| located("runtime_tool_requests", key))?
            {
                return Err(located("runtime_tool_requests", key));
            }
        }
        Ok(Self {
            agent,
            task: Arc::new(task),
            aliases,
            receipts: receipts
                .into_iter()
                .map(|(key, receipt)| (key, Arc::new(receipt)))
                .collect(),
        })
    }
    pub fn task(&self) -> &TaskRecord {
        self.task.as_ref()
    }
    pub fn agent(&self) -> &RecoverySnapshot {
        &self.agent
    }
    pub fn has_local_receipt(&self) -> bool {
        !self.aliases.is_empty() || !self.receipts.is_empty()
    }
    pub fn terminal_receipt(&self) -> Option<&Value> {
        if self.aliases.len() > 1 || self.receipts.len() != 1 {
            return None;
        }
        let (key, r) = &self.receipts[0];
        let a = self.agent.record();
        let t = &self.task;
        if r["id"] != key.as_str()
            || r["agent"] != a["id"]
            || r["accountKey"] != a.get("accountKey").cloned().unwrap_or(json!("default"))
            || r["threadId"] != a["threadId"]
            || r["turnId"].as_str() != Some(t.turn_id())
            || r["callId"].as_str() != Some(t.item_id())
            || r["finished"].is_null()
        {
            return None;
        }
        // Cancellation proves non-dispatch only for this exact owner attempt.
        if matches!(
            crate::tool_requests::lifecycle::classify(r),
            Ok(studio_operations::OperationState::CancelledBeforeDispatch)
        ) && r.get("epoch") != a.get("epoch")
        {
            return None;
        }
        // Invalid pairs, like unknown outcomes, supply no completion evidence.
        matches!(crate::tool_requests::lifecycle::terminal(r), Ok(true)).then_some(r.as_ref())
    }
    pub fn terminal_receipt_arc(&self) -> Option<Arc<Value>> {
        self.terminal_receipt()?;
        self.receipts
            .first()
            .map(|(_, receipt)| Arc::clone(receipt))
    }
    pub fn decide(
        &self,
        native_items: &[Value],
        resolved_receipt: Option<&Value>,
    ) -> TaskRecoveryDecision {
        let a = self.agent.record();
        let t = &self.task;
        let kind = t.task_type().as_str();
        if !self.agent.eligibility().task_owner
            || t.status() != TaskStatus::Running
            || Some(t.agent()) != a["id"].as_str()
            || t.id().as_ref()
                != format!("{}:{}", a["id"].as_str().unwrap_or_default(), t.item_id())
            || !matches!(
                kind,
                "commandExecution" | "fileChange" | "dynamicToolCall" | "mcpToolCall"
            )
            || t.kind()
                != if kind == "commandExecution" {
                    TaskKind::Command
                } else {
                    TaskKind::Tool
                }
        {
            return TaskRecoveryDecision::Unconfirmed;
        }
        if self.has_local_receipt() {
            let (Some(receipt), Some(resolved)) = (self.terminal_receipt(), resolved_receipt)
            else {
                return TaskRecoveryDecision::Unconfirmed;
            };
            // Payload resolution must retain the complete operation identity.
            for field in [
                "id",
                "agent",
                "accountKey",
                "threadId",
                "turnId",
                "callId",
                "epoch",
                "supervisor",
                "connectionId",
                "signature",
                "stage",
                "outcome",
                "finished",
            ] {
                if receipt.get(field) != resolved.get(field) {
                    return TaskRecoveryDecision::Unconfirmed;
                }
            }
            let result = &resolved["result"];
            let Some(success) = result["success"].as_bool() else {
                return TaskRecoveryDecision::Unconfirmed;
            };
            if success
                && matches!(
                    crate::tool_requests::lifecycle::classify(resolved),
                    Ok(studio_operations::OperationState::NotApplied
                        | studio_operations::OperationState::CancelledBeforeDispatch)
                )
            {
                return TaskRecoveryDecision::Unconfirmed;
            }
            return TaskRecoveryDecision::Complete(
                json!({"id":t.item_id(),"type":kind,"status":"completed","success":success,"contentItems":result.get("contentItems").cloned().unwrap_or(json!([]))}),
            );
        }
        let mut matches = native_items
            .iter()
            .filter(|item| item["id"].as_str() == Some(t.item_id()));
        let Some(item) = matches.next() else {
            return TaskRecoveryDecision::Unconfirmed;
        };
        if matches.next().is_some()
            || item["type"].as_str() != Some(kind)
            || item.get("threadId").is_some_and(|v| v != &a["threadId"])
            || item
                .get("turnId")
                .is_some_and(|v| v.as_str() != Some(t.turn_id()))
        {
            return TaskRecoveryDecision::Unconfirmed;
        }
        let terminal = matches!(item["status"].as_str(), Some("completed" | "failed"))
            || (matches!(kind, "commandExecution" | "fileChange") && item["status"] == "declined");
        if terminal {
            TaskRecoveryDecision::Complete(item.clone())
        } else {
            TaskRecoveryDecision::Unconfirmed
        }
    }
}
/// The worker supplies payload resolution and its canonical task completion
/// writer. It rechecks owner scope, the raw task and aliases/receipts atomically.
pub trait TaskRecoveryEffects: Send {
    fn apply_absence<'a>(
        &'a mut self,
        expected: &'a TaskRecoverySnapshot,
        scope: &'a crate::recovery_history::ConnectionScope,
        observation: &'a crate::recovery_absence::RecoveryObservation,
        decision: crate::recovery_absence::TaskAbsenceDecision,
    ) -> futures_util::future::BoxFuture<'a, Result<bool, crate::recovery::RecoveryError>>;

    fn resolve_receipt<'a>(
        &'a mut self,
        receipt: Arc<Value>,
    ) -> futures_util::future::BoxFuture<'a, Result<Value, crate::recovery::RecoveryError>>;
    fn complete<'a>(
        &'a mut self,
        expected: &'a TaskRecoverySnapshot,
        scope: &'a crate::recovery_history::ConnectionScope,
        item: Value,
    ) -> futures_util::future::BoxFuture<'a, Result<bool, crate::recovery::RecoveryError>>;
}
/// Recover one exact task without issuing provider mutations. The dispatcher
/// groups at most 32 jobs per scan; each native history read has a five-second
/// budget. Unavailable proof persists a named hold; absence never authorizes replay.
pub async fn recover_task(
    snapshot: &TaskRecoverySnapshot,
    scope: crate::recovery_history::ConnectionScope,
    reader: &mut dyn crate::recovery_history::RecoveryReader,
    effects: &mut dyn TaskRecoveryEffects,
) -> Result<bool, crate::recovery::RecoveryError> {
    use crate::recovery_history::{HistoryError, HistoryRead};
    let a = snapshot.agent.record();
    if a.get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default")
        != scope.account
    {
        return Ok(false);
    }
    if !snapshot.agent.eligibility().task_owner
        || snapshot.task.status() != TaskStatus::Running
        || Some(snapshot.task.agent()) != a["id"].as_str()
    {
        return Ok(false);
    }
    let mut read = HistoryRead::new(
        reader,
        scope.clone(),
        std::time::Instant::now() + std::time::Duration::from_secs(5),
    );
    let decision = if snapshot.has_local_receipt() {
        let Some(receipt) = snapshot.terminal_receipt_arc() else {
            let observation = read.observation()?;
            return effects
                .apply_absence(
                    snapshot,
                    &scope,
                    &observation,
                    crate::recovery_absence::TaskAbsenceDecision::Hold {
                        reason: "local-outcome-unknown".into(),
                    },
                )
                .await;
        };
        let resolved = effects.resolve_receipt(receipt).await?;
        snapshot.decide(&[], Some(&resolved))
    } else {
        let thread = a["threadId"]
            .as_str()
            .ok_or(HistoryError::Invalid("threadId"))?;
        let turn = snapshot.task.turn_id();
        if snapshot.task.task_type() == TaskType::CommandExecution && read.barrier().await.is_err()
        {
            let observation = read.observation()?;
            return effects
                .apply_absence(
                    snapshot,
                    &scope,
                    &observation,
                    crate::recovery_absence::TaskAbsenceDecision::Hold {
                        reason: "native-event-barrier-unavailable".into(),
                    },
                )
                .await;
        }
        let items = match read.task_items(thread, turn).await {
            Ok(items) => items,
            Err(HistoryError::Unsupported) => vec![],
            Err(_) => {
                let observation = read.observation()?;
                return effects
                    .apply_absence(
                        snapshot,
                        &scope,
                        &observation,
                        crate::recovery_absence::TaskAbsenceDecision::Hold {
                            reason: "native-history-unavailable".into(),
                        },
                    )
                    .await;
            }
        };
        if read.barrier().await.is_err() {
            let observation = read.observation()?;
            return effects
                .apply_absence(
                    snapshot,
                    &scope,
                    &observation,
                    crate::recovery_absence::TaskAbsenceDecision::Hold {
                        reason: "native-event-barrier-unavailable".into(),
                    },
                )
                .await;
        }
        let decision = snapshot.decide(&items, None);
        if decision == TaskRecoveryDecision::Unconfirmed {
            let observation = read.observation()?;
            let matches: Vec<_> = items
                .iter()
                .filter(|i| i["id"].as_str() == Some(snapshot.task.item_id()))
                .collect();
            let candidate = matches.is_empty()
                || (matches.len() == 1
                    && matches[0]["type"] == "commandExecution"
                    && matches[0]["status"] == "inProgress"
                    && matches[0]
                        .get("processId")
                        .map(process_identity)
                        .unwrap_or_else(|| snapshot.task.process_id().map(|id| id.normalized()))
                        == snapshot.task.process_id().map(|id| id.normalized()));
            let absent = if snapshot.task.task_type() == TaskType::CommandExecution
                && candidate
                && observation.quiet
            {
                match read.thread_state(thread).await {
                    Ok(state) if matches!(state["type"].as_str(), Some("idle" | "notLoaded")) => {
                        read.background_terminals(thread)
                            .await
                            .ok()
                            .map(|terminals| {
                                terminals.iter().all(|t| {
                                    t["itemId"].as_str() != Some(snapshot.task.item_id())
                                        && t["processId"].as_str()
                                            != snapshot
                                                .task
                                                .process_id()
                                                .map(|id| id.normalized())
                                                .as_deref()
                                })
                            })
                    }
                    _ => None,
                }
            } else {
                None
            };
            let barrier = read.barrier().await.is_ok();
            let observation = read.observation()?;
            let decision = snapshot.absence_decision(&observation, barrier && absent == Some(true));
            return effects
                .apply_absence(snapshot, &scope, &observation, decision)
                .await;
        }
        decision
    };
    match decision {
        TaskRecoveryDecision::Unconfirmed => {
            let observation = read.observation()?;
            effects
                .apply_absence(
                    snapshot,
                    &scope,
                    &observation,
                    crate::recovery_absence::TaskAbsenceDecision::Hold {
                        reason: "task-outcome-unconfirmed".into(),
                    },
                )
                .await
        }
        TaskRecoveryDecision::Complete(item) => effects.complete(snapshot, &scope, item).await,
    }
}

fn process_identity(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| (value.is_i64() || value.is_u64()).then(|| value.to_string()))
}
impl TaskRecoverySnapshot {
    pub fn absence_decision(
        &self,
        observation: &crate::recovery_absence::RecoveryObservation,
        absent_from_complete_terminals: bool,
    ) -> crate::recovery_absence::TaskAbsenceDecision {
        use crate::recovery_absence::{TaskAbsenceDecision, command_absence_source};
        let completed = observation.completed_turn.as_deref()
            == Some(&format!(
                "{}:{}",
                self.agent.record()["id"].as_str().unwrap_or_default(),
                self.task.turn_id()
            ));
        if !self.has_local_receipt()
            && self.agent.eligibility().task_owner
            && self.task.status() == TaskStatus::Running
            && Some(self.task.agent()) == self.agent.record()["id"].as_str()
            && observation.quiet
            && absent_from_complete_terminals
            && command_absence_source(&self.agent, &self.task, observation, completed)
        {
            TaskAbsenceDecision::Lost
        } else {
            TaskAbsenceDecision::Hold {
                reason: "native-command-absence-unproved".into(),
            }
        }
    }
}
