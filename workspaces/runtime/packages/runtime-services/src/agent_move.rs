//! Durable move/teleport observations and conservative recovery decisions.
//! Native import, activation and transport remain gated until their drivers exist.
use crate::event_write::truthy;
pub use crate::relocation::RelocationError as MoveError;
use crate::relocation::{checked, identity, invalid, timestamp};
use serde::Deserialize;
use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentMoveRepository as Repository, AgentRecord, AgentRepository, RuntimeStore,
    Transaction,
};

pub const NATIVE_MOVE_DEPENDENCY: &str = "agent-move/exact-native-history-and-paired-driver";
const DEADLINE_SECONDS: f64 = 300.0;
const DEADLINE_ERROR: &str = "The move recovery deadline expired after 300 seconds. The outcome stays unknown; no input was repeated";

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Waiting,
    Exported,
    Importing,
    Prepared,
    Redirected,
    Complete,
    Unknown,
    Failed,
    Receiving,
    NativeSubmitted,
    Ready,
    Active,
}
#[derive(Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Side {
    Source,
    Target,
}
#[derive(Deserialize)]
struct Fields {
    id: String,
    side: Side,
    phase: Phase,
    created: f64,
    #[serde(default)]
    deadline: Option<f64>,
    #[serde(default, rename = "uncertainPhase")]
    uncertain_phase: Option<Phase>,
}
pub struct MoveSnapshot {
    fields: Fields,
    raw: Value,
    agent: String,
}
pub struct MoveDecision {
    operation: Value,
    agent: Option<Value>,
}
impl MoveSnapshot {
    pub fn from_record(raw: &Value) -> Result<Self, MoveError> {
        checked::<Value>("relocation/MoveRecord", "runtime_agent_moves", raw)?;
        let fields: Fields = serde_json::from_value(raw.clone())
            .map_err(|_| invalid("runtime_agent_moves", "record"))?;
        let agent = if fields.side == Side::Source {
            raw.get("agent")
        } else {
            raw.pointer("/descriptor/agent/id")
        }
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("runtime_agent_moves", "agent"))?
        .to_owned();
        if fields.side == Side::Target
            && raw
                .get("move")
                .and_then(Value::as_str)
                .is_none_or(|key| identity(&[key, "target"]) != fields.id)
        {
            return Err(invalid("runtime_agent_moves", "/id"));
        }
        Ok(Self {
            fields,
            raw: raw.clone(),
            agent,
        })
    }
    fn expired(&self, now: f64) -> bool {
        now >= self
            .fields
            .deadline
            .unwrap_or(self.fields.created + DEADLINE_SECONDS)
    }
    pub fn status(&self, actor: &str, local_server: &str) -> Result<Value, MoveError> {
        if self.agent != actor {
            return Err(MoveError::Refused("The move belongs to another agent"));
        }
        if self.fields.side == Side::Source
            && self.fields.phase == Phase::Complete
            && !truthy(self.raw.get("firstTurnCache"))
        {
            return Err(MoveError::Requires("cross-server-driver/move-cache-status"));
        }
        Ok(
            json!({"requestId":if self.fields.side==Side::Source {json!(self.fields.id)} else {self.raw["move"].clone()},"phase":self.raw["phase"],"error":self.raw.get("error"),"acceptance":if self.fields.side==Side::Source {&self.raw["result"]} else {&self.raw["descriptor"]["acceptedResult"]},"server":self.raw.get("server").cloned().unwrap_or(json!(local_server)),"firstTurnCache":self.raw.get("firstTurnCache")}),
        )
    }
    /// Expiry never resumes execution, repeats native import, or invents a remote receipt.
    pub fn expire(
        &self,
        agent: Option<&Value>,
        now: f64,
    ) -> Result<Option<MoveDecision>, MoveError> {
        timestamp(now)?;
        if matches!(
            self.fields.phase,
            Phase::Complete | Phase::Failed | Phase::Active
        ) || !self.expired(now)
        {
            return Ok(None);
        }
        if self.fields.phase == Phase::Unknown && truthy(self.raw.get("recoveryExpiredAt")) {
            return Ok(None);
        }
        let mut operation = self.raw.clone();
        if self.fields.side == Side::Source && self.fields.phase != Phase::Unknown {
            operation["uncertainPhase"] = self.raw["phase"].clone();
        }
        operation["phase"] = json!("unknown");
        operation["error"] = json!(DEADLINE_ERROR);
        operation["recoveryExpiredAt"] = json!(now);
        let mut updated = self.checked_agent(agent)?;
        if let Some(row) = updated.as_mut() {
            let key = if self.fields.side == Side::Source {
                self.fields.id.as_str()
            } else {
                self.raw["move"]
                    .as_str()
                    .ok_or_else(|| invalid("runtime_agent_moves", "/move"))?
            };
            let bound = row.pointer("/executionMove/id").and_then(Value::as_str) == Some(key)
                && (self.fields.side == Side::Source || truthy(row.get("moveImportPending")));
            if bound {
                row["executionMove"]["phase"] = json!("unknown");
                row["error"] = json!(DEADLINE_ERROR);
            } else {
                updated = None;
            }
        }
        Ok(Some(MoveDecision {
            operation,
            agent: updated,
        }))
    }
    /// Release only when a bound status receipt proves that this target cannot run input.
    pub fn release(
        &self,
        agent: &Value,
        target: &Value,
        pending: bool,
        now: f64,
    ) -> Result<Option<MoveDecision>, MoveError> {
        timestamp(now)?;
        if self.fields.side != Side::Source || self.fields.phase != Phase::Unknown {
            return Ok(None);
        }
        checked::<Value>("relocation/TargetStatus", "move status receipt", target)?;
        let phase = target
            .get("phase")
            .and_then(Value::as_str)
            .ok_or(MoveError::Refused("Invalid move status receipt"))?;
        let release = self.fields.uncertain_phase == Some(Phase::Waiting) && phase == "not_started"
            || self.fields.uncertain_phase == Some(Phase::Exported)
                && self.expired(now)
                && phase != "active";
        if !release {
            return Ok(None);
        }
        let reason = if phase == "not_started" {
            "The move stopped before any transfer"
        } else {
            "The move did not finish before its deadline"
        };
        let mut operation = self.raw.clone();
        operation["phase"] = json!("failed");
        operation["releasedAt"] = json!(now);
        operation["error"] = json!(format!("{reason}. The chat stays on this server"));
        let mut updated = self.checked_agent(Some(agent))?;
        if let Some(row) = updated.as_mut() {
            if row.pointer("/executionMove/id").and_then(Value::as_str) == Some(&self.fields.id) {
                row.as_object_mut()
                    .ok_or_else(|| invalid("runtime_agents", "record"))?
                    .remove("executionMove");
                row["error"] = json!(format!(
                    "{reason}. The chat stays on this server; ask for the move again"
                ));
                if row["status"] == "queued" && !truthy(row.get("inFlight")) && !pending {
                    row["status"] = json!("waiting");
                }
            } else {
                updated = None;
            }
        }
        Ok(Some(MoveDecision {
            operation,
            agent: updated,
        }))
    }
    fn checked_agent(&self, agent: Option<&Value>) -> Result<Option<Value>, MoveError> {
        agent
            .map(|row| {
                checked::<Value>("relocation/NativeAgent", "runtime_agents", row)?;
                if row["id"] != self.agent {
                    return Err(MoveError::Refused("The move belongs to another agent"));
                }
                if self.fields.side == Side::Source && row["epoch"] != self.raw["epoch"] {
                    return Err(MoveError::Refused(
                        "The move belongs to another agent epoch",
                    ));
                }
                Ok(row.clone())
            })
            .transpose()
    }
}

/// Read a move receipt in the caller's existing transaction; no native or network effects.
pub fn status(
    tx: &Transaction<'_>,
    actor: &str,
    request: &str,
    local_server: &str,
) -> Result<Value, MoveError> {
    let key = identity(&["move", actor, request]);
    let row = match Repository::get(tx, &key)? {
        Some(row) => Some(row),
        None => Repository::get(tx, &identity(&[&key, "target"]))?,
    };
    match row {
        Some(row) => MoveSnapshot::from_record(&row)?.status(actor, local_server),
        None => Ok(json!({"phase":"unknown","requestId":key})),
    }
}

/// Until exact history/preflight/import drivers exist, neither move nor teleport writes state.
pub fn start(_tx: &Transaction<'_>, _actor: &str, _args: &Value) -> Result<Value, MoveError> {
    Err(MoveError::Requires(NATIVE_MOVE_DEPENDENCY))
}

pub fn expire(store: &RuntimeStore, key: &str, now: f64) -> Result<bool, MoveError> {
    store.transaction_with(|tx| {
        let Some(row) = Repository::get(tx, key)? else {
            return Ok(false);
        };
        let snapshot = MoveSnapshot::from_record(&row)?;
        let agent =
            AgentRepository::get(tx, &AgentId::from(snapshot.agent.as_str()))?.map(|a| a.value());
        apply(tx, snapshot.expire(agent.as_ref(), now)?)
    })
}

/// Reconcile a saved paired status ticket. A response alone is insufficient authority.
pub fn release_untransferred(
    store: &RuntimeStore,
    key: &str,
    status_ticket: &str,
    now: f64,
) -> Result<bool, MoveError> {
    store.transaction_with(|tx| {
        let Some(row) = Repository::get(tx, key)? else {
            return Ok(false);
        };
        let snapshot = MoveSnapshot::from_record(&row)?;
        let receipt = studio_runtime_store::ProjectLocationsRepository::outbox(tx, status_ticket)?
            .ok_or(MoveError::Refused("Unknown move status ticket"))?;
        if receipt["server"] != row["server"]
            || receipt["state"] != "complete"
            || receipt.pointer("/body/requestId") != Some(&json!(status_ticket))
            || receipt.pointer("/body/action") != Some(&json!("move_status"))
            || receipt.pointer("/body/payload/move") != Some(&json!(key))
            || receipt.pointer("/result/requestId") != Some(&json!(status_ticket))
            || receipt.pointer("/result/outcome") != Some(&json!("applied"))
        {
            return Err(MoveError::Refused(
                "The move status ticket does not prove this move",
            ));
        }
        let agent = AgentRepository::get(tx, &AgentId::from(snapshot.agent.as_str()))?
            .ok_or(MoveError::Refused("Unknown move agent"))?;
        let pending = Repository::pending_events(tx, &snapshot.agent)?;
        apply(
            tx,
            snapshot.release(&agent.value(), &receipt["result"]["value"], pending, now)?,
        )
    })
}
fn apply(tx: &Transaction<'_>, decision: Option<MoveDecision>) -> Result<bool, MoveError> {
    let Some(decision) = decision else {
        return Ok(false);
    };
    Repository::save(tx, &decision.operation)?;
    if let Some(row) = decision.agent {
        let agent = AgentRecord::from_value(row.clone())
            .map_err(|_| invalid("runtime_agents", "record"))?;
        AgentRepository::put(tx, &agent)?;
        crate::agents_create::publish_agent(tx, &row)?;
    }
    Ok(true)
}

/// Token-usage notification consumer: first current-turn cache observation wins.
pub fn capture_cache(
    tx: &Transaction<'_>,
    agent_id: &str,
    notification: &Value,
    now: f64,
) -> Result<bool, MoveError> {
    let agent = AgentRepository::get(tx, &AgentId::from(agent_id))?
        .ok_or(MoveError::Refused("Unknown move agent"))?
        .value();
    checked::<Value>("relocation/NativeAgent", "runtime_agents", &agent)?;
    let Some(moved) = agent.get("movedFrom").filter(|v| truthy(Some(v))) else {
        return Ok(false);
    };
    if !truthy(agent.get("inFlight"))
        || truthy(notification.get("turnId")) && notification.get("turnId") != agent.get("turnId")
    {
        return Ok(false);
    }
    let key = identity(&[
        moved
            .get("move")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("runtime_agents", "/movedFrom/move"))?,
        "target",
    ]);
    let Some(row) = Repository::get(tx, &key)? else {
        return Ok(false);
    };
    let snapshot = MoveSnapshot::from_record(&row)?;
    let Some(updated) = snapshot.observe_cache(&agent, notification, now)? else {
        return Ok(false);
    };
    Repository::save(tx, &updated)?;
    Ok(true)
}

impl MoveSnapshot {
    fn observe_cache(
        &self,
        agent: &Value,
        notification: &Value,
        now: f64,
    ) -> Result<Option<Value>, MoveError> {
        if self.agent != agent["id"] {
            return Err(MoveError::Refused("The move belongs to another agent"));
        }
        if self.fields.phase != Phase::Active || truthy(self.raw.get("firstTurnCache")) {
            return Ok(None);
        }
        let usage = notification
            .get("requestUsage")
            .filter(|v| truthy(Some(v)))
            .or_else(|| notification.pointer("/tokenUsage/last"));
        let Some(cached) = usage
            .and_then(|u| u.get("cachedInputTokens"))
            .filter(|v| v.is_i64() || v.is_u64())
        else {
            return Ok(None);
        };
        timestamp(now)?;
        let mut row = self.raw.clone();
        row["firstTurnCache"] = json!({"turnId":agent.get("turnId"),"cachedInputTokens":cached,"cacheWriteInputTokens":usage.and_then(|u|u.get("cacheWriteInputTokens")),"inputTokens":usage.and_then(|u|u.get("inputTokens")),"observedAt":now});
        Ok(Some(row))
    }
}
