//! Pure Claude control decisions. Effect adapters reserve before native I/O.
pub mod service;
pub mod store;
use crate::event_write::{request_signature, truthy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
pub use studio_runtime_schema::claude_controls::{ControlError, ControlOutcome};
pub(crate) use studio_runtime_schema::claude_permissions::PERMISSION_MODES;
use studio_runtime_schema::workspace_operation::{
    OperationKind, OperationPhase, WorkspaceOperationRecord,
};
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
}
fn default_account() -> String {
    "default".into()
}
fn checked_agent(agent: &Value) -> Result<Agent> {
    validate("PersistedAgentRecord", agent)?;
    let typed = Agent::deserialize(agent).map_err(|_| ControlError::Invalid("agent".into()))?;
    if truthy(agent.get("deletedAt")) {
        return Err(ControlError::Request);
    }
    Ok(typed)
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Operation {
    id: String,
    kind: OperationKind,
    agent: String,
    signature: String,
    source: Value,
    phase: OperationPhase,
    #[serde(flatten)]
    extensions: OperationExtensions,
    result: Option<Box<serde_json::value::RawValue>>,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OperationExtensions {
    request_id: String,
    submitted: Option<bool>,
    final_response: Option<ControlOutcome>,
}
fn operation_kind(request: &ControlRequest) -> OperationKind {
    match request {
        ControlRequest::Settings { .. } => OperationKind::ClaudeSettings,
    }
}
// Claude admission validates a stricter schema and replay compares the complete
// saved semantic JSON, including opaque extensions and missing versus null.
fn control_evidence(record: &WorkspaceOperationRecord) -> Result<Value> {
    record
        .serialize(serde_json::value::Serializer)
        .map_err(|_| ControlError::Invalid("operation".into()))
}
#[derive(Clone)]
pub struct ControlSnapshot {
    agent: Agent,
    raw: Value,
    operation: Option<Operation>,
    saved: Option<Value>,
}
pub use studio_runtime_schema::claude_controls::ClaudeControlCommand as ControlRequest;
pub struct Writes {
    pub agent: Value,
    pub operation: Value,
    pub result: Value,
}
fn signature(request: &ControlRequest) -> String {
    let ControlRequest::Settings { settings, .. } = request;
    // The null turn keeps the signatures of existing receipts.
    request_signature(
        &json!({"agent":request.id(),"action":request.action_name(),"settings":settings,"turn":Value::Null}),
    )
}
impl ControlSnapshot {
    fn replay(&self, request: &ControlRequest) -> Result<Option<Result<Value>>> {
        let Some(op) = &self.operation else {
            return Ok(None);
        };
        if op.id != request.operation_id()
            || op.extensions.request_id != request.request_id()
            || op.agent != request.id()
            || op.kind != operation_kind(request)
            || op.signature != signature(request)
        {
            return Err(ControlError::Conflict);
        }
        Ok(match &op.extensions.final_response {
            Some(ControlOutcome::Ok) => Some(Ok(serde_json::from_str(
                op.result.as_ref().ok_or(ControlError::Unknown)?.get(),
            )
            .map_err(|_| ControlError::Unknown)?)),
            Some(ControlOutcome::Err(error)) => Some(Err(error.clone())),
            None => None,
        })
    }
    pub fn from_records(agent: Value, operation: Option<Value>) -> Result<Self> {
        let typed = checked_agent(&agent)?;
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
    pub fn from_stored(agent: Value, operation: Option<&WorkspaceOperationRecord>) -> Result<Self> {
        let typed = checked_agent(&agent)?;
        let saved = operation.map(control_evidence).transpose()?;
        if let Some(saved) = &saved {
            validate("PersistedClaudeControlRecord", saved)?;
        }
        let parsed = operation
            .map(|op| {
                let extensions =
                    OperationExtensions::deserialize(serde::de::value::MapDeserializer::<
                        _,
                        serde_json::Error,
                    >::new(
                        op.extensions()
                            .iter()
                            .map(|(key, value)| (key.as_str(), value)),
                    ))
                    .map_err(|_| ControlError::Invalid("operation".into()))?;
                Ok::<_, ControlError>(Operation {
                    id: op.id.clone(),
                    kind: op.kind,
                    agent: op.agent.clone(),
                    signature: op
                        .signature
                        .value()
                        .cloned()
                        .ok_or_else(|| ControlError::Invalid("operation".into()))?,
                    source: Value::Object(
                        op.source
                            .value()
                            .cloned()
                            .ok_or_else(|| ControlError::Invalid("operation".into()))?,
                    ),
                    phase: op.phase,
                    extensions,
                    result: op
                        .result
                        .value()
                        .filter(|v| !v.is_null())
                        .map(serde_json::value::to_raw_value)
                        .transpose()
                        .map_err(|_| ControlError::Invalid("operation".into()))?,
                })
            })
            .transpose()?;
        Ok(Self {
            agent: typed,
            raw: agent,
            operation: parsed,
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
    /// Permission mode is the only setting with an SDK live control. Missing
    /// thinking means enabled; absent and null compact limits both mean default.
    pub(crate) fn live_permissions(&self, request: &ControlRequest) -> bool {
        let ControlRequest::Settings { settings, .. } = request;
        let previous = &self.raw["claudeOptions"];
        let next_window = settings
            .auto_compact_window
            .value()
            .map(|window| json!(window));
        settings.permission_mode.is_some()
            && (settings.thinking != Some(false))
                == (previous.get("thinking").and_then(Value::as_bool) != Some(false))
            && next_window.as_ref()
                == previous
                    .get("autoCompactWindow")
                    .filter(|value| !value.is_null())
    }
    pub fn reserve(&self, request: &ControlRequest, now: f64) -> Result<Writes> {
        if request.id() != self.agent.id || truthy(self.raw.get("accountTransferId")) {
            return Err(ControlError::Changed);
        }
        if let Some(op) = &self.operation {
            if op.id != request.operation_id()
                || op.extensions.request_id != request.request_id()
                || op.agent != request.id()
                || op.kind != operation_kind(request)
                || op.signature != signature(request)
            {
                return Err(ControlError::Conflict);
            }
            if op.source != self.source() {
                return Err(ControlError::Changed);
            }
            let saved = self.saved.clone().ok_or(ControlError::Unknown)?;
            if op.phase == OperationPhase::Completed {
                return Ok(Writes {
                    agent: self.raw.clone(),
                    result: saved.get("result").cloned().unwrap_or(Value::Null),
                    operation: saved,
                });
            }
            if op.phase != OperationPhase::Failed {
                if saved.get("provider").is_none_or(Value::is_null)
                    && op.extensions.submitted == Some(true)
                {
                    return Err(ControlError::Unknown);
                }
                return Ok(Writes {
                    agent: self.raw.clone(),
                    operation: saved,
                    result: Value::Null,
                });
            }
            if op.extensions.submitted == Some(true) || truthy(saved.get("provider")) {
                return Err(ControlError::Unknown);
            }
            // A definite no-send result can reserve again after fresh idle proof.
        }
        if !self.live_permissions(request)
            && (truthy(self.raw.get("inFlight"))
                || ["running", "starting", "approval"].contains(&self.agent.status.as_str()))
        {
            return Err(ControlError::Busy);
        }
        let operation = json!({"id":request.operation_id(),"requestId":request.request_id(),"kind":format!("claude_{}",request.action_name()),"agent":request.id(),"turnId":Value::Null,"signature":signature(request),"source":self.source(),"phase":"provider_pending","created":now});
        let mut agent = self.raw.clone();
        agent["workspaceOperation"] = json!("branch");
        agent["workspaceReservationId"] = json!(request.operation_id());
        Ok(Writes {
            agent,
            operation,
            result: Value::Null,
        })
    }
    pub fn submitted(
        &self,
        request: &ControlRequest,
        proof: &service::NativeProof,
        now: f64,
    ) -> Result<Value> {
        let mut op = self.saved.clone().ok_or(ControlError::Changed)?;
        self.check_owned(&op)?;
        if truthy(op.get("submitted")) {
            return Err(ControlError::Unknown);
        }
        let (ControlRequest::Settings { .. }, service::NativeProof::Settings(native)) =
            (request, proof);
        if !self.live_permissions(request)
            && (native.tasks.value().is_some_and(|tasks| !tasks.is_empty())
                || native.turns.iter().any(|turn| {
                    turn.status
                        .value()
                        .is_some_and(|status| status == "inProgress")
                }))
        {
            return Err(ControlError::Busy);
        }
        op["submitted"] = json!(true);
        op["updated"] = json!(now);
        Ok(op)
    }
    pub fn provider_ready(
        &self,
        request: &ControlRequest,
        result: service::MutationReply,
        now: f64,
    ) -> Result<Value> {
        let mut op = self.saved.clone().ok_or(ControlError::Changed)?;
        self.check_owned(&op)?;
        if self
            .operation
            .as_ref()
            .is_none_or(|operation| operation.kind != operation_kind(request))
        {
            return Err(ControlError::Unknown);
        }
        let (ControlRequest::Settings { settings, .. }, service::MutationReply::Settings(reply)) =
            (request, &result);
        let expected = serde_json::to_value(settings).map_err(|_| ControlError::Unknown)?;
        let expected = expected.as_object().ok_or(ControlError::Unknown)?;
        if studio_runtime_schema::claude_controls::CLAUDE_SETTINGS_WIRE_FIELDS
            .iter()
            .any(|key| expected.get(*key) != reply.settings.get(*key))
        {
            return Err(ControlError::Unknown);
        }
        op["provider"] = result.into_value()?;
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
        let ControlRequest::Settings { settings, .. } = request;
        agent["claudeOptions"] = json!(settings);
        if let Some(mode) = settings.permission_mode {
            agent["yoloMode"] = json!(
                mode
                    == studio_runtime_schema::claude_permissions::PermissionMode::BypassPermissions
            );
        }
        if agent["workspaceOperation"] == "branch_recovery" {
            agent["status"] = json!("paused");
            agent["error"] = Value::Null;
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
fn validate(schema: &str, value: &Value) -> Result<()> {
    if let Some((path, _)) = studio_runtime_schema::first_error(schema, value)
        .map_err(|_| ControlError::Invalid("schema".into()))?
    {
        return Err(ControlError::Invalid(path));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_snapshot_preserves_control_validation_and_replay_evidence() {
        let agent = json!({"compactions":0,"id":"agent","rootId":"agent","epoch":2,"cwd":"/fixture",
            "provider":"claude","accountKey":"account","threadId":"thread","status":"paused"});
        let base = json!({"id":"op","requestId":"request","kind":"claude_settings","agent":"agent",
            "signature":"a".repeat(64),"source":{},"phase":"completed","created":1,
            "finalResponse":{"status":"ok"},"result":{"value":[1,null]},"future":{"opaque":true}});
        for result in [Some(json!({"value":[1,null]})), Some(Value::Null), None] {
            let mut raw = base.clone();
            match result {
                Some(value) => raw["result"] = value,
                None => {
                    raw.as_object_mut().unwrap().remove("result");
                }
            }
            let record = WorkspaceOperationRecord::deserialize(&raw).unwrap();
            let typed = ControlSnapshot::from_stored(agent.clone(), Some(&record)).unwrap();
            let legacy = ControlSnapshot::from_records(agent.clone(), Some(raw.clone())).unwrap();
            assert_eq!(typed.saved, Some(raw));
            assert_eq!(
                typed
                    .operation
                    .unwrap()
                    .result
                    .as_deref()
                    .map(|v| v.get().to_owned()),
                legacy
                    .operation
                    .unwrap()
                    .result
                    .as_deref()
                    .map(|v| v.get().to_owned())
            );
        }
        let mut invalid = base;
        invalid["requestId"] = json!("");
        let record = WorkspaceOperationRecord::deserialize(&invalid).unwrap();
        assert!(ControlSnapshot::from_stored(agent, Some(&record)).is_err());
    }

    #[test]
    fn claude_control_signatures_preserve_existing_receipts() {
        for (body, expected) in [
            (
                json!({"action":"settings","id":"agent","request_id":"one","settings":{}}),
                "eb5e71e48860d9a285652d455a37628512cdb609b34480ae0b02f78bec487c38",
            ),
            (
                json!({"action":"settings","id":"agent","request_id":"one","settings":{"autoCompactWindow":null}}),
                "7ef66fdf13bb4634469324e10d82fe6dba891d3c3819f6445d6e1b862ec561df",
            ),
        ] {
            let request: ControlRequest = serde_json::from_value(body).expect("command");
            assert_eq!(request.operation_id(), "claude-control:one");
            assert_eq!(signature(&request), expected);
        }
    }

    #[test]
    fn claude_control_service_consumes_shared_command() {
        fn execute(
            controls: &service::Controls,
            command: studio_runtime_schema::claude_controls::ClaudeControlCommand,
        ) {
            drop(controls.execute(command));
        }
        let _: fn(
            &service::Controls,
            studio_runtime_schema::claude_controls::ClaudeControlCommand,
        ) = execute;
    }
}
