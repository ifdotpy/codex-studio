//! A local tool receipt takes precedence over native history. Unknown local
//! mutations cannot be retired by a native delivery error.
use crate::recovery_policy::{InvalidRecoveryRecord, RecoverySnapshot};
use serde_json::{Value, json};

const MAX_RECEIPTS: usize = 2;
#[derive(Clone)]
pub struct TaskRecoverySnapshot {
    agent: RecoverySnapshot,
    task: Value,
    pub(crate) aliases: Vec<String>,
    pub(crate) receipts: Vec<(String, Value)>,
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
        task: Value,
        aliases: Vec<String>,
        receipts: Vec<(String, Value)>,
    ) -> Result<Self, InvalidRecoveryRecord> {
        let agent = RecoverySnapshot::from_record(agent)?;
        if !studio_runtime_schema::validate("RecoveryTaskRecord", &task)
            .map_err(|_| invalid("schema"))?
            || aliases.len() > MAX_RECEIPTS
            || receipts.len() > MAX_RECEIPTS
        {
            return Err(invalid("task"));
        }
        for (key, receipt) in &receipts {
            if key.is_empty()
                || !studio_runtime_schema::validate("RecoveryToolReceipt", receipt)
                    .map_err(|_| invalid("schema"))?
            {
                return Err(invalid("receipt"));
            }
        }
        Ok(Self {
            agent,
            task,
            aliases,
            receipts,
        })
    }
    pub fn task(&self) -> &Value {
        &self.task
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
            || r["turnId"] != t["turnId"]
            || r["callId"] != t["itemId"]
            || r["finished"].is_null()
        {
            return None;
        }
        ((r["stage"] == "completed" && r["outcome"] == "applied")
            || ((r["stage"] == "completed" || r["stage"] == "failed")
                && r["outcome"] == "not_applied"))
            .then_some(r)
    }
    pub fn decide(
        &self,
        native_items: &[Value],
        resolved_receipt: Option<&Value>,
    ) -> TaskRecoveryDecision {
        let a = self.agent.record();
        let t = &self.task;
        let kind = t["type"].as_str().unwrap_or_default();
        if !self.agent.eligibility().task_owner
            || t["status"] != "running"
            || t["agent"] != a["id"]
            || t["id"]
                != format!(
                    "{}:{}",
                    a["id"].as_str().unwrap_or_default(),
                    t["itemId"].as_str().unwrap_or_default()
                )
            || !matches!(
                kind,
                "commandExecution" | "fileChange" | "dynamicToolCall" | "mcpToolCall"
            )
            || t["kind"]
                != if kind == "commandExecution" {
                    "command"
                } else {
                    "tool"
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
            if resolved["outcome"] == "not_applied" && success {
                return TaskRecoveryDecision::Unconfirmed;
            }
            return TaskRecoveryDecision::Complete(
                json!({"id":t["itemId"],"type":t["type"],"status":"completed","success":success,"contentItems":result.get("contentItems").cloned().unwrap_or(json!([]))}),
            );
        }
        let mut matches = native_items.iter().filter(|item| item["id"] == t["itemId"]);
        let Some(item) = matches.next() else {
            return TaskRecoveryDecision::Unconfirmed;
        };
        if matches.next().is_some()
            || item["type"] != t["type"]
            || item.get("threadId").is_some_and(|v| v != &a["threadId"])
            || item.get("turnId").is_some_and(|v| v != &t["turnId"])
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
fn invalid(field: &str) -> InvalidRecoveryRecord {
    InvalidRecoveryRecord {
        location: format!("runtime_tasks.{field}"),
    }
}

/// The worker supplies payload resolution and its canonical task completion
/// writer. It rechecks owner scope, the raw task and aliases/receipts atomically.
pub trait TaskRecoveryEffects {
    fn apply_absence(
        &mut self,
        expected: &TaskRecoverySnapshot,
        scope: &crate::recovery_history::ConnectionScope,
        observation: &crate::recovery_absence::RecoveryObservation,
        decision: crate::recovery_absence::TaskAbsenceDecision,
    ) -> Result<bool, crate::recovery::RecoveryError>;

    fn resolve_receipt(&mut self, receipt: &Value)
    -> Result<Value, crate::recovery::RecoveryError>;
    fn complete(
        &mut self,
        expected: &TaskRecoverySnapshot,
        scope: &crate::recovery_history::ConnectionScope,
        item: Value,
    ) -> Result<bool, crate::recovery::RecoveryError>;
}
/// Recover one exact task without issuing provider mutations. The dispatcher
/// groups at most 32 jobs per scan; each native history read has a five-second
/// budget. Unavailable proof persists a named hold; absence never authorizes replay.
pub fn recover_task(
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
        || snapshot.task["status"] != "running"
        || snapshot.task["agent"] != a["id"]
    {
        return Ok(false);
    }
    let mut read = HistoryRead::new(
        reader,
        scope.clone(),
        std::time::Instant::now() + std::time::Duration::from_secs(5),
    );
    let decision = if snapshot.has_local_receipt() {
        let Some(receipt) = snapshot.terminal_receipt() else {
            let observation = read.observation()?;
            return effects.apply_absence(
                snapshot,
                &scope,
                &observation,
                crate::recovery_absence::TaskAbsenceDecision::Hold {
                    reason: "local-outcome-unknown".into(),
                },
            );
        };
        let resolved = effects.resolve_receipt(receipt)?;
        snapshot.decide(&[], Some(&resolved))
    } else {
        let thread = a["threadId"]
            .as_str()
            .ok_or(HistoryError::Invalid("threadId"))?;
        let turn = snapshot.task["turnId"]
            .as_str()
            .ok_or(HistoryError::Invalid("turnId"))?;
        if snapshot.task["type"] == "commandExecution" && read.barrier().is_err() {
            let observation = read.observation()?;
            return effects.apply_absence(
                snapshot,
                &scope,
                &observation,
                crate::recovery_absence::TaskAbsenceDecision::Hold {
                    reason: "native-event-barrier-unavailable".into(),
                },
            );
        }
        let items = match read.task_items(thread, turn) {
            Ok(items) => items,
            Err(HistoryError::Unsupported) => vec![],
            Err(_) => {
                let observation = read.observation()?;
                return effects.apply_absence(
                    snapshot,
                    &scope,
                    &observation,
                    crate::recovery_absence::TaskAbsenceDecision::Hold {
                        reason: "native-history-unavailable".into(),
                    },
                );
            }
        };
        if read.barrier().is_err() {
            let observation = read.observation()?;
            return effects.apply_absence(
                snapshot,
                &scope,
                &observation,
                crate::recovery_absence::TaskAbsenceDecision::Hold {
                    reason: "native-event-barrier-unavailable".into(),
                },
            );
        }
        let decision = snapshot.decide(&items, None);
        if decision == TaskRecoveryDecision::Unconfirmed {
            let observation = read.observation()?;
            let matches: Vec<_> = items
                .iter()
                .filter(|i| i["id"] == snapshot.task["itemId"])
                .collect();
            let candidate = matches.is_empty()
                || (matches.len() == 1
                    && matches[0]["type"] == "commandExecution"
                    && matches[0]["status"] == "inProgress"
                    && process_identity(
                        matches[0]
                            .get("processId")
                            .unwrap_or(&snapshot.task["processId"]),
                    ) == process_identity(&snapshot.task["processId"]));
            let absent = if snapshot.task["type"] == "commandExecution"
                && candidate
                && observation.quiet
            {
                match read.thread_state(thread) {
                    Ok(state) if matches!(state["type"].as_str(), Some("idle" | "notLoaded")) => {
                        read.background_terminals(thread).ok().map(|terminals| {
                            terminals.iter().all(|t| {
                                t["itemId"] != snapshot.task["itemId"]
                                    && t["processId"].as_str()
                                        != process_identity(&snapshot.task["processId"]).as_deref()
                            })
                        })
                    }
                    _ => None,
                }
            } else {
                None
            };
            let barrier = read.barrier().is_ok();
            let observation = read.observation()?;
            let decision = snapshot.absence_decision(&observation, barrier && absent == Some(true));
            return effects.apply_absence(snapshot, &scope, &observation, decision);
        }
        decision
    };
    match decision {
        TaskRecoveryDecision::Unconfirmed => {
            let observation = read.observation()?;
            effects.apply_absence(
                snapshot,
                &scope,
                &observation,
                crate::recovery_absence::TaskAbsenceDecision::Hold {
                    reason: "task-outcome-unconfirmed".into(),
                },
            )
        }
        TaskRecoveryDecision::Complete(item) => effects.complete(snapshot, &scope, item),
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
                self.task["turnId"].as_str().unwrap_or_default()
            ));
        if !self.has_local_receipt()
            && self.agent.eligibility().task_owner
            && self.task["status"] == "running"
            && self.task["agent"] == self.agent.record()["id"]
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
