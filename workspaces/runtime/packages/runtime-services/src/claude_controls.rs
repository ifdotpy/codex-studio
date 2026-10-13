//! Pure Claude control decisions. Effect adapters reserve before native I/O.
pub mod service;
pub mod store;
use crate::event_write::{request_signature, truthy};
use serde::Deserialize;
use serde_json::{Value, json};
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("Invalid Claude control record at {0}")]
    Invalid(String),
    #[error("Claude session changed before the action")]
    Changed,
    #[error("Pause Claude before changing its session")]
    Busy,
    #[error("This request identity has different content")]
    Conflict,
    #[error("Claude control outcome is unknown; workspace recovery is required")]
    Unknown,
    #[error("Claude control service is unavailable")]
    Unavailable,
    #[error("Invalid Claude settings or action")]
    Request,
}
pub type Result<T> = std::result::Result<T, ControlError>;
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Agent {
    id: String,
    epoch: u64,
    cwd: String,
    #[serde(default = "default_account")]
    account_key: String,
    thread_id: Option<String>,
    status: String,
    provider: String,
}
fn default_account() -> String {
    "default".into()
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Operation {
    id: String,
    request_id: String,
    kind: String,
    agent: String,
    signature: String,
    source: Value,
    phase: String,
    submitted: Option<bool>,
}
#[derive(Clone)]
pub struct ControlSnapshot {
    agent: Agent,
    raw: Value,
    operation: Option<Operation>,
    saved: Option<Value>,
}
#[derive(Clone)]
pub struct ControlRequest {
    id: String,
    request_id: String,
    action: Action,
    settings: Option<Value>,
    turn: Option<String>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Settings,
    Rollback,
}
impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::Settings => "settings",
            Self::Rollback => "rollback",
        }
    }
}
pub struct Writes {
    pub agent: Value,
    pub operation: Value,
    pub removed: Vec<String>,
    pub result: Value,
}
impl ControlRequest {
    pub fn from_value(body: &Value) -> Result<Self> {
        let text = |k| {
            body.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty() && s.len() <= 1024)
                .map(str::to_owned)
                .ok_or(ControlError::Request)
        };
        let action = match body.get("action").and_then(Value::as_str) {
            Some("settings") => Action::Settings,
            Some("rollback") => Action::Rollback,
            _ => return Err(ControlError::Request),
        };
        Ok(Self {
            id: text("id")?,
            request_id: text("request_id")?,
            action,
            settings: if action == Action::Settings {
                Some(settings(
                    body.get("settings").ok_or(ControlError::Request)?,
                )?)
            } else {
                None
            },
            turn: if action == Action::Rollback {
                Some(text("turn_id")?)
            } else {
                None
            },
        })
    }
    pub fn operation_id(&self) -> String {
        format!("claude-control:{}", self.request_id)
    }
    fn signature(&self) -> String {
        request_signature(
            &json!({"agent":self.id,"action":self.action.name(),"settings":self.settings,"turn":self.turn}),
        )
    }
}
impl ControlSnapshot {
    pub fn from_records(agent: Value, operation: Option<Value>) -> Result<Self> {
        validate("PersistedAgentRecord", &agent)?;
        let typed: Agent = serde_json::from_value(agent.clone())
            .map_err(|_| ControlError::Invalid("agent".into()))?;
        if typed.provider != "claude" || truthy(agent.get("deletedAt")) {
            return Err(ControlError::Request);
        }
        let saved = operation;
        let operation = saved
            .as_ref()
            .map(|o| {
                validate("PersistedClaudeControlRecord", o)?;
                serde_json::from_value(o.clone())
                    .map_err(|_| ControlError::Invalid("operation".into()))
            })
            .transpose()?;
        Ok(Self {
            agent: typed,
            raw: agent,
            operation,
            saved,
        })
    }
    pub fn agent(&self) -> &Value {
        &self.raw
    }
    pub fn source(&self) -> Value {
        studio_runtime_store::CheckpointsRepository::workspace_source(&self.raw)
    }
    pub fn epoch(&self) -> u64 {
        self.agent.epoch
    }
    pub fn reserve(&self, request: &ControlRequest, pending: bool, now: f64) -> Result<Writes> {
        if request.id != self.agent.id || truthy(self.raw.get("accountTransferId")) {
            return Err(ControlError::Changed);
        }
        if let Some(op) = &self.operation {
            if op.id != request.operation_id()
                || op.request_id != request.request_id
                || op.agent != request.id
                || op.kind != format!("claude_{}", request.action.name())
                || op.signature != request.signature()
            {
                return Err(ControlError::Conflict);
            }
            if op.source != self.source() {
                return Err(ControlError::Changed);
            }
            let saved = self.saved.clone().ok_or(ControlError::Unknown)?;
            if op.phase == "completed" {
                return Ok(Writes {
                    agent: self.raw.clone(),
                    result: saved.get("result").cloned().unwrap_or(Value::Null),
                    operation: saved,
                    removed: vec![],
                });
            }
            if op.phase == "failed" {
                return Err(ControlError::Request);
            }
            if saved.get("provider").is_none_or(Value::is_null) && op.submitted == Some(true) {
                return Err(ControlError::Unknown);
            }
            return Ok(Writes {
                agent: self.raw.clone(),
                operation: saved,
                removed: vec![],
                result: Value::Null,
            });
        }
        if truthy(self.raw.get("inFlight"))
            || ["running", "starting", "approval"].contains(&self.agent.status.as_str())
        {
            return Err(ControlError::Busy);
        }
        if request.action == Action::Rollback && (pending || self.agent.thread_id.is_none()) {
            return Err(ControlError::Busy);
        }
        let operation = json!({"id":request.operation_id(),"requestId":request.request_id,"kind":format!("claude_{}",request.action.name()),"agent":request.id,"turnId":request.turn,"signature":request.signature(),"source":self.source(),"phase":"provider_pending","created":now});
        let mut agent = self.raw.clone();
        agent["workspaceOperation"] = json!("branch");
        agent["workspaceReservationId"] = json!(request.operation_id());
        Ok(Writes {
            agent,
            operation,
            removed: vec![],
            result: Value::Null,
        })
    }
    pub fn submitted(
        &self,
        request: &ControlRequest,
        native: &Value,
        queue: &Value,
        now: f64,
    ) -> Result<Value> {
        let mut op = self.saved.clone().ok_or(ControlError::Changed)?;
        self.check_owned(&op)?;
        if truthy(op.get("submitted")) {
            return Err(ControlError::Unknown);
        }
        if truthy(native.get("tasks"))
            || native
                .get("turns")
                .and_then(Value::as_array)
                .is_some_and(|turns| {
                    turns
                        .iter()
                        .any(|t| t.get("status") == Some(&json!("inProgress")))
                })
        {
            return Err(ControlError::Busy);
        }
        if request.action == Action::Rollback {
            if truthy(queue.get("data").or_else(|| queue.get("items"))) {
                return Err(ControlError::Busy);
            }
            let turns = native
                .get("turns")
                .and_then(Value::as_array)
                .ok_or(ControlError::Request)?;
            let index = turns
                .iter()
                .position(|t| t.get("id").and_then(Value::as_str) == request.turn.as_deref())
                .ok_or(ControlError::Request)?;
            let removed: Vec<&str> = turns[index..]
                .iter()
                .map(|t| {
                    t.get("id")
                        .and_then(Value::as_str)
                        .ok_or(ControlError::Request)
                })
                .collect::<Result<_>>()?;
            op["removedTurns"] = json!(removed);
            op["retainedTurn"] = if index > 0 {
                turns[index - 1].get("id").cloned().unwrap_or(Value::Null)
            } else {
                Value::Null
            };
        }
        op["submitted"] = json!(true);
        op["updated"] = json!(now);
        Ok(op)
    }
    pub fn provider_ready(&self, result: Value, now: f64) -> Result<Value> {
        let mut op = self.saved.clone().ok_or(ControlError::Changed)?;
        self.check_owned(&op)?;
        if !result.is_object() {
            return Err(ControlError::Unknown);
        }
        if op["kind"] == "claude_rollback"
            && (result.get("threadId") != self.raw.get("threadId")
                || !truthy(result.get("nativeId")))
        {
            return Err(ControlError::Unknown);
        }
        op["provider"] = result;
        op["phase"] = json!("provider_ready");
        op["updated"] = json!(now);
        Ok(op)
    }
    pub fn complete(&self, request: &ControlRequest, now: f64) -> Result<Writes> {
        let mut op = self.saved.clone().ok_or(ControlError::Changed)?;
        self.check_owned(&op)?;
        let result = op
            .get("provider")
            .filter(|v| v.is_object())
            .cloned()
            .ok_or(ControlError::Unknown)?;
        let mut agent = self.raw.clone();
        let mut removed = vec![];
        if request.action == Action::Settings {
            let settings = request.settings.clone().ok_or(ControlError::Request)?;
            agent["claudeOptions"] = settings.clone();
            if let Some(mode) = settings.get("permissionMode") {
                agent["yoloMode"] = json!(mode == "bypassPermissions");
            }
            if agent["workspaceOperation"] == "branch_recovery" {
                agent["status"] = json!("paused");
                agent["error"] = Value::Null;
            }
        } else {
            removed = serde_json::from_value(
                op.get("removedTurns")
                    .cloned()
                    .ok_or(ControlError::Unknown)?,
            )
            .map_err(|_| ControlError::Unknown)?;
            for (k, v) in [
                ("status", json!("paused")),
                ("autoWake", json!(false)),
                ("inFlight", json!(false)),
                ("turnId", Value::Null),
                (
                    "lastCompletedTurn",
                    op.get("retainedTurn").cloned().unwrap_or(Value::Null),
                ),
                ("activity", Value::Null),
                ("activeTools", json!([])),
                ("error", Value::Null),
            ] {
                agent[k] = v;
            }
        }
        agent["workspaceOperation"] = Value::Null;
        agent
            .as_object_mut()
            .ok_or(ControlError::Changed)?
            .remove("workspaceReservationId");
        op["phase"] = json!("completed");
        op["result"] = result.clone();
        op["error"] = Value::Null;
        op["updated"] = json!(now);
        Ok(Writes {
            agent,
            operation: op,
            removed,
            result,
        })
    }
    fn check_owned(&self, op: &Value) -> Result<()> {
        if op.get("agent").and_then(Value::as_str) != Some(&self.agent.id)
            || op.get("source") != Some(&self.source())
            || self.raw.get("workspaceReservationId") != op.get("id")
        {
            return Err(ControlError::Changed);
        }
        Ok(())
    }
}
pub fn settings(value: &Value) -> Result<Value> {
    let object = value.as_object().ok_or(ControlError::Request)?;
    if object
        .keys()
        .any(|k| !["permissionMode", "thinking", "autoCompactWindow"].contains(&k.as_str()))
    {
        return Err(ControlError::Request);
    }
    if let Some(mode) = object.get("permissionMode")
        && !mode.as_str().is_some_and(|m| {
            [
                "default",
                "acceptEdits",
                "auto",
                "plan",
                "bypassPermissions",
            ]
            .contains(&m)
        })
    {
        return Err(ControlError::Request);
    }
    if object.get("thinking").is_some_and(|v| !v.is_boolean())
        || object.get("autoCompactWindow").is_some_and(|v| {
            !v.is_null() && !v.as_u64().is_some_and(|n| (100000..=1000000).contains(&n))
        })
    {
        return Err(ControlError::Request);
    }
    Ok(value.clone())
}
fn validate(schema: &str, value: &Value) -> Result<()> {
    if let Some((path, _)) = studio_runtime_schema::first_error(schema, value)
        .map_err(|_| ControlError::Invalid("schema".into()))?
    {
        return Err(ControlError::Invalid(path));
    }
    Ok(())
}
