//! Pure deferral/resumption of proven unsubmitted attempts. No provider effects:
//! a resumed job still requires the ordinary final submission admission check.
use crate::budget::{AdmissionDenial, BudgetError, decode, validate};
use crate::event_write::truthy;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const MAX_EVENTS: usize = 32;
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct WaitEvent {
    pub id: String,
    pub agent: String,
    pub epoch: i64,
    pub status: String,
    pub turn_id: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug)]
pub struct WaitSnapshot {
    agent: Value,
    events: BTreeMap<String, WaitEvent>,
    closed: bool,
}
impl WaitSnapshot {
    pub fn from_records(
        agent: &Value,
        events: &[Value],
        closed: bool,
    ) -> Result<Self, BudgetError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Checked {
            id: String,
            epoch: i64,
            #[serde(default)]
            start_attempt: Option<Value>,
        }
        validate("PersistedBudgetAgent", agent, "runtime_agents")?;
        let checked: Checked = decode(agent, "runtime_agents/budgetWait")?;
        if checked.id.is_empty()
            || checked.epoch < 0
            || checked
                .start_attempt
                .as_ref()
                .is_some_and(|a| !a.is_object())
        {
            return Err(BudgetError::Invalid("runtime_agents/startAttempt".into()));
        }
        let mut rows = BTreeMap::new();
        for row in events {
            let event: WaitEvent = decode(row, "runtime_events")?;
            if rows.insert(event.id.clone(), event).is_some() {
                return Err(BudgetError::Invalid("runtime_events/id".into()));
            }
        }
        Ok(Self {
            agent: agent.clone(),
            events: rows,
            closed,
        })
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct WaitDecision {
    pub agent: Option<Value>,
    pub events: Vec<WaitEvent>,
    pub retired_action: Option<RetiredAction>,
    pub job: Option<Value>,
}
#[derive(Clone, Debug, Serialize)]
pub struct RetiredAction {
    pub request_id: String,
    pub attempt_id: Value,
}
impl WaitDecision {
    fn unchanged() -> Self {
        Self {
            agent: None,
            events: vec![],
            retired_action: None,
            job: None,
        }
    }
}
fn field<'a>(record: &'a Value, key: &str) -> &'a Value {
    record.get(key).unwrap_or(&Value::Null)
}
fn account(record: &Value) -> Value {
    record
        .get("accountKey")
        .cloned()
        .unwrap_or(json!("default"))
}
fn identity(left: &Value, right: &Value) -> bool {
    account(left) == account(right)
        && field(left, "epoch") == field(right, "epoch")
        && field(left, "threadId") == field(right, "threadId")
}
fn unsubmitted(agent: &Value, attempt_id: &Value) -> bool {
    let attempt = field(agent, "startAttempt");
    field(attempt, "id") == attempt_id
        && field(attempt, "submitted") == &Value::Bool(false)
        && !truthy(attempt.get("turnId"))
        && !truthy(attempt.get("observedTurnId"))
        && !truthy(attempt.get("notSubmittedReason"))
        && (!truthy(agent.get("turnId"))
            || (field(agent, "turnId") == field(agent, "lastCompletedTurn")
                && matches!(
                    agent.get("lastCompletedTurnStatus").and_then(Value::as_str),
                    Some("completed" | "failed" | "interrupted")
                )))
        && field(attempt, "epoch") == field(agent, "epoch")
        && truthy(agent.get("autoWake"))
        && !truthy(agent.get("deletedAt"))
        && attempt
            .get("accountKey")
            .cloned()
            .unwrap_or_else(|| account(agent))
            == account(agent)
        && (field(attempt, "threadId").is_null()
            || field(attempt, "threadId") == field(agent, "threadId"))
}
fn action_identity(agent: &Value, attempt: &Value) -> bool {
    let pin = field(attempt, "actionIdentity");
    if !truthy(Some(pin)) {
        return true;
    }
    let Some(fields) = pin.as_object() else {
        return false;
    };
    if fields.iter().all(|(name, value)| {
        if name == "accountKey" {
            account(agent) == *value
        } else {
            field(agent, name) == value
        }
    }) {
        return true;
    }
    let repair = field(agent, "contextRepair");
    let source = field(repair, "source");
    let current = field(agent, "startAttempt");
    field(repair, "phase") == "completed"
        && field(repair, "agent") == field(agent, "id")
        && field(source, "id") == field(agent, "id")
        && ["accountKey", "threadId", "epoch"]
            .iter()
            .all(|name| field(source, name) == field(pin, name))
        && account(agent) == *field(pin, "accountKey")
        && field(agent, "epoch") == field(pin, "epoch")
        && truthy(attempt.get("id"))
        && field(source, "attemptId") == field(attempt, "id")
        && field(attempt, "id") == field(current, "id")
        && field(current, "actionRequestId") == field(attempt, "actionRequestId")
        && field(current, "actionIdentity") == pin
        && repair
            .get("newThreadId")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
        && field(repair, "newThreadId") == field(agent, "threadId")
        && field(agent, "threadId") == field(current, "threadId")
}
fn event_ids(attempt: &Value) -> Option<Vec<String>> {
    let ids: Vec<String> = serde_json::from_value(attempt.get("events")?.clone()).ok()?;
    (ids.len() <= MAX_EVENTS && ids.iter().collect::<BTreeSet<_>>().len() == ids.len())
        .then_some(ids)
}
fn event<'a>(snapshot: &'a WaitSnapshot, id: &str, pending_only: bool) -> Option<&'a WaitEvent> {
    snapshot.events.get(id).filter(|row| {
        json!(row.agent) == *field(&snapshot.agent, "id")
            && json!(row.epoch) == *field(&snapshot.agent, "epoch")
            && row.turn_id.is_none()
            && if pending_only {
                row.status == "pending"
            } else {
                matches!(row.status.as_str(), "reserved" | "dispatching" | "pending")
            }
    })
}

pub fn defer(
    snapshot: &WaitSnapshot,
    attempt_id: &str,
    denial: &AdmissionDenial,
    unknown: bool,
    now: f64,
) -> Result<WaitDecision, BudgetError> {
    if !now.is_finite() {
        return Err(BudgetError::Invalid("clock".into()));
    }
    let a = &snapshot.agent;
    let d = &denial.receipt;
    let attempt = field(a, "startAttempt");
    if unknown
        || snapshot.closed
        || field(d, "tokenBudget").is_null()
        || (!truthy(d.get("reached")) && !truthy(d.get("incomplete")))
        || !unsubmitted(a, &json!(attempt_id))
        || field(d, "agentId") != field(a, "id")
        || field(d, "attemptId") != attempt_id
        || !identity(d, a)
    {
        return Ok(WaitDecision::unchanged());
    }
    let Some(ids) = event_ids(attempt) else {
        return Ok(WaitDecision::unchanged());
    };
    let action = truthy(attempt.get("action"));
    if (action
        && (!ids.is_empty()
            || !matches!(
                attempt.get("action").and_then(Value::as_str),
                Some("compact" | "review" | "capacity")
            )))
        || (!action && ids.is_empty())
        || (action && !action_identity(a, attempt))
    {
        return Ok(WaitDecision::unchanged());
    }
    let Some(mut rows) = ids
        .iter()
        .map(|id| event(snapshot, id, false).cloned())
        .collect::<Option<Vec<_>>>()
    else {
        return Ok(WaitDecision::unchanged());
    };
    for row in &mut rows {
        row.status = "pending".into();
        row.extra.insert("error".into(), Value::Null);
    }
    let name = if action {
        "budgetActionWait"
    } else {
        "budgetStartWait"
    };
    let mut wait = json!({"attemptId":attempt_id,"agentId":field(a,"id"),"accountKey":account(a),"epoch":field(a,"epoch"),"threadId":field(a,"threadId"),"events":ids,
        "action":field(attempt,"action"),"actionRequestId":field(attempt,"actionRequestId"),"actionIdentity":field(attempt,"actionIdentity"),"error":denial.message,"at":now,"admission":d});
    let previous = field(a, name);
    if field(previous, "attemptId") == attempt_id {
        wait["at"] = field(previous, "at").clone();
    }
    let mut result = a.clone();
    result[name] = wait;
    result["status"] = json!("queued");
    result["inFlight"] = json!(false);
    result["budgetBlocked"] = json!(denial.message);
    result["error"] = json!(denial.message);
    Ok(WaitDecision {
        agent: Some(result),
        events: rows,
        retired_action: None,
        job: None,
    })
}

pub fn claim(snapshot: &WaitSnapshot, now: f64) -> Result<WaitDecision, BudgetError> {
    if !now.is_finite() {
        return Err(BudgetError::Invalid("clock".into()));
    }
    let a = &snapshot.agent;
    let name = if truthy(a.get("budgetActionWait")) {
        "budgetActionWait"
    } else {
        "budgetStartWait"
    };
    let wait = field(a, name);
    if !wait.is_object() {
        return Ok(WaitDecision::unchanged());
    }
    let attempt = field(a, "startAttempt");
    let valid = unsubmitted(a, field(wait, "attemptId"))
        && !snapshot.closed
        && field(a, "status") == "queued"
        && !truthy(a.get("inFlight"))
        && field(wait, "agentId") == field(a, "id")
        && identity(wait, a)
        && ["events", "action", "actionRequestId", "actionIdentity"]
            .iter()
            .all(|name| field(wait, name) == field(attempt, name))
        && (!truthy(wait.get("action")) || action_identity(a, attempt));
    let rows = if valid {
        event_ids(wait).and_then(|ids| {
            ids.iter()
                .map(|id| event(snapshot, id, true).cloned())
                .collect::<Option<Vec<_>>>()
        })
    } else {
        None
    };
    let valid = rows.is_some();
    let mut rows = rows.unwrap_or_default();
    let mut result = a.clone();
    let mut last = wait.clone();
    last["status"] = json!(if valid { "resumed" } else { "superseded" });
    last["finishedAt"] = json!(now);
    let object = result
        .as_object_mut()
        .ok_or_else(|| BudgetError::Invalid("runtime_agents".into()))?;
    object.remove(name);
    object.insert("lastBudgetWait".into(), last);
    if field(a, "budgetBlocked") == field(wait, "error") {
        object.remove("budgetBlocked");
    }
    if field(a, "error") == field(wait, "error") {
        object.insert("error".into(), Value::Null);
    }
    let mut retired_action = None;
    let mut job = None;
    if valid {
        let job_rows = rows.clone();
        for row in &mut rows {
            row.status = "reserved".into();
        }
        result["status"] = json!("starting");
        result["inFlight"] = json!(true);
        result["turnEpoch"] = field(a, "epoch").clone();
        job = Some(
            json!({"kind":if truthy(wait.get("action")) {"action"} else {"turn"},"agent":result,"attempt":attempt,"rows":job_rows}),
        );
    } else if let Some(request_id) = wait
        .get("actionRequestId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        && field(attempt, "id") == field(wait, "attemptId")
        && field(attempt, "submitted") == &Value::Bool(false)
        && !truthy(attempt.get("turnId"))
        && !truthy(attempt.get("observedTurnId"))
    {
        retired_action = Some(RetiredAction {
            request_id: request_id.into(),
            attempt_id: field(wait, "attemptId").clone(),
        });
    }
    Ok(WaitDecision {
        agent: Some(result),
        events: rows,
        retired_action,
        job,
    })
}
