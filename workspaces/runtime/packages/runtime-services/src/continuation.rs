//! Checked, bounded snapshots and write intents shared by continuation policies.
//! Stored JSON is retained verbatim except for the fields a decision changes.
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use studio_runtime_store::AgentRecord;
use thiserror::Error;

pub const POLL_SECONDS: f64 = 180.0;
pub const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;
#[derive(Debug, Error)]
pub enum ContinuationError {
    #[error("invalid continuation snapshot at {0}")]
    Invalid(String),
    #[error("Not implemented: requires {0}")]
    Unavailable(&'static str),
    #[error("continuation wake failures: {0:?}")]
    WakeFailures(Vec<String>),
    #[error(transparent)]
    Store(#[from] studio_runtime_store::StoreError),
}
pub type Result<T> = std::result::Result<T, ContinuationError>;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WaitStatus {
    Scheduled,
    Started,
    Starting,
    Finished,
    Cancelled,
    Unknown,
    Exhausted,
    Failed,
    AuthProbeWait,
    Completed,
}
#[derive(Clone, Deserialize)]
struct WaitIdentity {
    id: String,
    status: WaitStatus,
}
#[derive(Clone)]
pub struct Snapshot {
    identity: AgentRecord,
    wait: Option<WaitIdentity>,
    pub(crate) agent: Value,
    pub(crate) ancestors: Vec<Value>,
    pub(crate) claims: Vec<String>,
    pub(crate) record: Option<Value>,
    pub(crate) events: Vec<Value>,
    pub(crate) metadata: BTreeMap<String, Option<Value>>,
    pub(crate) complaints: BTreeMap<String, Value>,
    pub(crate) work: BTreeMap<String, Value>,
    pub(crate) assets: BTreeMap<String, Value>,
}
impl Snapshot {
    /// The only constructor. It accepts raw Python-written records, never trusted flags.
    pub fn from_records(raw: Value) -> Result<Self> {
        static VALIDATOR: OnceLock<std::result::Result<jsonschema::Validator, String>> =
            OnceLock::new();
        let validator = VALIDATOR
            .get_or_init(|| {
                let schema: Value = serde_json::from_str(include_str!(
                    "../../runtime-schema/src/schemas/persisted-continuation-snapshot.schema.json"
                ))
                .map_err(|e| e.to_string())?;
                jsonschema::validator_for(&schema).map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(|_| ContinuationError::Invalid("schema".into()))?;
        if raw.to_string().len() > MAX_RECORD_BYTES {
            return Err(ContinuationError::Invalid("/size".into()));
        }
        if let Some(error) = validator.iter_errors(&raw).next() {
            return Err(ContinuationError::Invalid(
                error.instance_path().to_string(),
            ));
        }
        let get_vec = |key: &str| {
            raw.get(key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let get_map = |key: &str| {
            raw.get(key)
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect()
        };
        let identity = serde_json::from_value(raw["agent"].clone())
            .map_err(|_| ContinuationError::Invalid("/agent".into()))?;
        let wait = raw
            .get("record")
            .filter(|r| !r.is_null())
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| ContinuationError::Invalid("/record".into()))?;
        let snapshot = Self {
            identity,
            wait,
            agent: raw["agent"].clone(),
            ancestors: get_vec("ancestors"),
            claims: get_vec("claims")
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            record: raw.get("record").filter(|v| !v.is_null()).cloned(),
            events: get_vec("events"),
            metadata: raw
                .get("metadata")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|m| m.iter())
                .map(|(k, v)| (k.clone(), (!v.is_null()).then(|| v.clone())))
                .collect(),
            complaints: get_map("complaints"),
            work: get_map("work"),
            assets: get_map("assets"),
        };
        let mut ids = BTreeSet::new();
        if snapshot.events.iter().any(|e| {
            !ids.insert(text(e, "id"))
                || text(e, "agent") != text(&snapshot.agent, "id")
                || e.get("epoch") != snapshot.agent.get("epoch")
        }) {
            return Err(ContinuationError::Invalid("/events/identity".into()));
        }
        Ok(snapshot)
    }
    pub fn agent_id(&self) -> &str {
        &self.identity.id.0
    }
    pub(crate) fn status(&self) -> Option<WaitStatus> {
        self.wait.as_ref().map(|r| r.status)
    }
    pub(crate) fn current(&self, key: &str) -> bool {
        self.wait.as_ref().is_some_and(|r| {
            self.agent
                .get(key)
                .and_then(|a| a.get("id"))
                .and_then(Value::as_str)
                == Some(r.id.as_str())
        })
    }
    pub(crate) fn claims_valid(&self, r: &Value) -> bool {
        let saved: BTreeSet<&str> = r
            .get("taskClaims")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        saved == self.claims.iter().map(String::as_str).collect()
    }
    pub(crate) fn parents_running(&self, auth: bool) -> bool {
        let mut parent = &self.agent;
        let mut seen = BTreeSet::new();
        while let Some(id) = parent.get("parentId").and_then(Value::as_str) {
            if !seen.insert(text(parent, "id")) {
                return false;
            }
            let Some(next) = self.ancestors.iter().find(|p| text(p, "id") == id) else {
                return false;
            };
            if !flag(next, "autoWake")
                || present(next, "deletedAt")
                || auth && text(next, "status") == "paused"
            {
                return false;
            }
            parent = next;
        }
        !auth || text(parent, "id") == text(&self.agent, "rootId")
    }
}
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Followup {
        id: String,
        text: String,
    },
    WorkerHold {
        operation: String,
        transition: &'static str,
        reason: String,
    },
    CapacityStart {
        attempt: Value,
    },
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Decision {
    pub agent: Option<Value>,
    pub record: Option<Value>,
    pub events: Vec<Value>,
    pub metadata: BTreeMap<String, Value>,
    pub effects: Vec<Effect>,
    /// Ephemeral model projection, never written over the original event text.
    pub batch: Vec<Value>,
}
impl Decision {
    pub(crate) fn save(s: &Snapshot, mut record: Value, key: &str, now: f64) -> Self {
        record["updatedAt"] = json!(now);
        let mut agent = s.agent.clone();
        agent[key] = record.clone();
        Self {
            agent: Some(agent),
            record: Some(record),
            ..Self::default()
        }
    }
    pub(crate) fn hold(&mut self, operation: &str, transition: &'static str, reason: &str) {
        self.effects.push(Effect::WorkerHold {
            operation: operation.into(),
            transition,
            reason: reason.into(),
        });
    }
}
pub(crate) fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}
pub(crate) fn number(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}
pub(crate) fn flag(v: &Value, key: &str) -> bool {
    v.get(key) == Some(&Value::Bool(true))
}
pub(crate) fn present(v: &Value, key: &str) -> bool {
    crate::event_write::truthy(v.get(key))
}
pub(crate) fn remove(v: &mut Value, key: &str) {
    if let Some(o) = v.as_object_mut() {
        o.remove(key);
    }
}
pub(crate) fn patch(v: &mut Value, fields: Value) {
    if let (Some(o), Some(fields)) = (v.as_object_mut(), fields.as_object()) {
        o.extend(fields.clone());
    }
}
pub(crate) fn account(v: &Value) -> &str {
    v.get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default")
}
pub(crate) fn now_checked(now: f64) -> Result<()> {
    if !now.is_finite() || !(0.0..=9_007_199_254_740_991.0).contains(&now) {
        Err(ContinuationError::Invalid("/now".into()))
    } else {
        Ok(())
    }
}

pub(crate) fn native_thread_blocked(agent: &Value) -> bool {
    let thread = agent.get("threadId");
    (present(&agent["nativeThreadBlock"], "threadId")
        && agent["nativeThreadBlock"].get("threadId") == thread)
        || (present(agent, "threadId")
            && (text(&agent["error"], "codexErrorInfo") == "misalignmentPolicyViolation"
                || (present(&agent["nativeTurnError"], "turnId")
                    && agent["nativeTurnError"].get("turnId") == agent.get("turnId")
                    && text(&agent["nativeTurnError"]["error"], "codexErrorInfo")
                        == "misalignmentPolicyViolation")))
}
