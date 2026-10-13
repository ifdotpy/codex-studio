//! Pure lifetime accounting and team admission. All time and history observations
//! are supplied by the caller; this module never opens native session files.
use crate::event_write::truthy;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use studio_runtime_store::{python_json_with_separators, sha256_hex};
use thiserror::Error;

pub const MAX_TOKENS: u64 = 9_007_199_254_740_991;
const MAX_TAIL: u64 = 65_536;
const MAX_RECORD_BYTES: usize = 1_048_576;

#[derive(Debug, Error)]
pub enum BudgetError {
    #[error("invalid budget input at {0}")]
    Invalid(String),
    #[error("budget-policy/token-overflow")]
    Overflow,
    #[error(transparent)]
    Store(#[from] studio_runtime_store::StoreError),
}
fn invalid(location: &str) -> BudgetError {
    BudgetError::Invalid(location.into())
}
pub(crate) fn decode<T: serde::de::DeserializeOwned>(
    value: &Value,
    location: &str,
) -> Result<T, BudgetError> {
    if value.to_string().len() > MAX_RECORD_BYTES {
        return Err(invalid(location));
    }
    serde_json::from_value(value.clone()).map_err(|_| invalid(location))
}
pub(crate) fn validate(schema: &str, value: &Value, location: &str) -> Result<(), BudgetError> {
    if value.to_string().len() > MAX_RECORD_BYTES {
        return Err(invalid(location));
    }
    if let Some((pointer, _)) =
        studio_runtime_schema::first_error(schema, value).map_err(|_| invalid(location))?
    {
        return Err(invalid(&format!("{location}{pointer}")));
    }
    Ok(())
}
fn add(a: u64, b: u64) -> Result<u64, BudgetError> {
    a.checked_add(b)
        .filter(|n| *n <= MAX_TOKENS)
        .ok_or(BudgetError::Overflow)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BudgetState {
    cutoff: f64,
    floor: u64,
    spent: u64,
    before: u64,
    after: u64,
    historical_notices: u64,
    notice_spent: u64,
    counter: Option<u64>,
    thread: Option<String>,
    notice_at: f64,
    created: f64,
    last_amount: Option<u64>,
    ambiguous_notices: Vec<String>,
    faults: Vec<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}
impl BudgetState {
    fn fault(&mut self, fault: &str) {
        if !self.faults.iter().any(|f| f == fault) {
            self.faults.push(fault.into());
        }
    }
    fn settle(&mut self) -> Result<(), BudgetError> {
        self.spent = self
            .spent
            .max(add(self.floor, self.after)?)
            .max(add(self.before, self.after)?)
            .max(self.notice_spent)
            .max(self.historical_notices);
        Ok(())
    }
    fn accounting(&self) -> &'static str {
        if self.notice_spent > self.before.saturating_add(self.after)
            || !self.faults.is_empty()
            || !self.ambiguous_notices.is_empty()
        {
            "provisional"
        } else {
            "responseRecords"
        }
    }
    pub fn patch(&self) -> Value {
        json!({"tokensUsed":self.spent,"tokenUsageAccounting":self.accounting()})
    }
    pub fn record(&self) -> Result<Value, BudgetError> {
        let value = serde_json::to_value(self).map_err(|_| invalid("runtime_budget"))?;
        validate("PersistedBudgetState", &value, "runtime_budget")?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Agent {
    id: String,
    #[serde(default)]
    root_id: Option<String>,
    #[serde(default = "default_account")]
    account_key: String,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    last_completed_turn: Option<String>,
    #[serde(default)]
    tokens_used: Option<u64>,
    #[serde(default)]
    token_budget: Option<u64>,
    #[serde(default)]
    created: f64,
}
fn default_account() -> String {
    "default".into()
}

/// One checked constructor for Python-written records. The durable agent owns
/// accounting and identity; supplied identity fields are assertions, not overrides.
#[derive(Clone, Debug)]
pub struct BudgetSnapshot {
    agent: Agent,
    state: BudgetState,
    initialized: bool,
}
impl BudgetSnapshot {
    pub fn from_records(
        agent: &Value,
        durable_agent: &Value,
        state: Option<&Value>,
        now: f64,
    ) -> Result<Self, BudgetError> {
        validate("PersistedBudgetAgent", agent, "runtime_agents")?;
        validate("PersistedBudgetAgent", durable_agent, "runtime_agents")?;
        if let Some(state) = state {
            validate("PersistedBudgetState", state, "runtime_budget")?;
        }
        let supplied: Agent = decode(agent, "runtime_agents/budget")?;
        let durable: Agent = decode(durable_agent, "runtime_agents/budget")?;
        if supplied.id.is_empty() || durable.id != supplied.id || !now.is_finite() {
            return Err(invalid("runtime_agents/id"));
        }
        for name in ["rootId", "accountKey", "threadId", "turnId", "epoch"] {
            let expected = durable_agent.get(name).cloned().unwrap_or_else(|| {
                if name == "accountKey" {
                    json!("default")
                } else {
                    Value::Null
                }
            });
            if agent.get(name).is_some_and(|value| *value != expected) {
                return Err(invalid(&format!("runtime_agents/{name}/identity")));
            }
        }
        let floor = durable.tokens_used.unwrap_or(0);
        let initialized = state.is_none();
        let state: BudgetState = match state {
            Some(value) => decode(value, "runtime_budget")?,
            None => BudgetState {
                cutoff: now,
                floor,
                spent: floor,
                before: 0,
                after: 0,
                historical_notices: 0,
                notice_spent: floor,
                counter: (floor != 0).then_some(floor),
                thread: durable.thread_id.clone(),
                notice_at: 0.,
                created: durable.created,
                last_amount: None,
                ambiguous_notices: vec![],
                faults: vec![],
                extra: BTreeMap::new(),
            },
        };
        Ok(Self {
            agent: durable,
            state,
            initialized,
        })
    }
    pub fn state(&self) -> &BudgetState {
        &self.state
    }
    pub fn initialized(&self) -> bool {
        self.initialized
    }
    pub fn agent_id(&self) -> &str {
        &self.agent.id
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CaptureSource {
    Live,
    History,
}

#[derive(Clone, Debug)]
pub struct UsageObservation {
    key: String,
    exact: bool,
    amount: Option<u64>,
    counter: Option<u64>,
    response: bool,
    thread: Option<String>,
    estimated: bool,
    at: f64,
    source: CaptureSource,
}
fn tokens(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64)
}
impl UsageObservation {
    pub fn from_record(
        snapshot: &BudgetSnapshot,
        params: &Value,
        at: f64,
        source: CaptureSource,
    ) -> Result<Self, BudgetError> {
        if !params.is_object() || params.to_string().len() > MAX_RECORD_BYTES || !at.is_finite() {
            return Err(invalid("tokenUsage"));
        }
        validate("BudgetCapture", params, "tokenUsage")?;
        let a = &snapshot.agent;
        let usage = params
            .get("tokenUsage")
            .filter(|v| truthy(Some(v)))
            .cloned()
            .unwrap_or(json!({}));
        let last = params
            .get("requestUsage")
            .filter(|v| truthy(Some(v)))
            .or_else(|| usage.get("last"))
            .cloned()
            .unwrap_or(json!({}));
        let response = truthy(params.get("responseId"));
        let claude = params.get("usageSource").and_then(Value::as_str) == Some("claudeResponse");
        let exact = response && (truthy(params.get("rawTokenUsageRecord")) || claude);
        let mut amount = tokens(last.get("totalTokens"));
        if amount.is_none() && claude {
            amount = [
                "inputTokens",
                "cachedInputTokens",
                "cacheWriteInputTokens",
                "outputTokens",
            ]
            .iter()
            .map(|field| tokens(last.get(*field)))
            .collect::<Option<Vec<_>>>()
            .map(|values| values.into_iter().try_fold(0, add))
            .transpose()?;
        }
        let counter = tokens(usage.get("total").and_then(|v| v.get("totalTokens")));
        if amount.is_none() && !exact {
            amount = counter;
        }
        let identity = if exact {
            json!([a.id, "response", params["responseId"]])
        } else {
            json!([
                a.id,
                a.account_key,
                "notice",
                params
                    .get("turnId")
                    .filter(|v| truthy(Some(v)))
                    .cloned()
                    .unwrap_or(json!(a.turn_id)),
                usage.get("total"),
                usage.get("last")
            ])
        };
        let thread = params
            .get("threadId")
            .filter(|v| truthy(Some(v)))
            .map(|v| decode::<String>(v, "tokenUsage/threadId"))
            .transpose()?
            .or_else(|| a.thread_id.clone());
        Ok(Self {
            key: sha256_hex(python_json_with_separators(&identity, ",", ":").as_bytes()),
            exact,
            amount,
            counter,
            response,
            thread,
            estimated: params
                .get("_analyticsTimestampSource")
                .and_then(Value::as_str)
                == Some("fileModifiedEstimate"),
            at,
            source,
        })
    }
    pub fn key(&self) -> &str {
        &self.key
    }
}

pub use studio_runtime_store::budget::BudgetUsageRow as UsageRow;
#[derive(Clone, Debug, Serialize)]
pub struct CaptureDecision {
    pub state: BudgetState,
    pub usage: Option<UsageRow>,
    pub changed: bool,
}

pub fn capture(
    snapshot: &BudgetSnapshot,
    observation: &UsageObservation,
    previous: Option<&UsageRow>,
) -> Result<CaptureDecision, BudgetError> {
    let mut state = snapshot.state.clone();
    let p = observation;
    let mut inserted = None;
    if p.source == CaptureSource::Live || p.at >= state.created {
        if let Some(previous) = previous {
            if previous.id != p.key || previous.agent != snapshot.agent.id {
                return Err(invalid("runtime_budget_usage/identity"));
            }
            if !p.exact && p.response {
                state.ambiguous_notices.retain(|key| key != &p.key);
            }
            if p.exact {
                if previous.kind != "response" || p.amount != Some(previous.tokens) {
                    return Err(invalid("runtime_budget_usage/responseReplayConflict"));
                }
                return Ok(CaptureDecision {
                    state: snapshot.state.clone(),
                    usage: None,
                    changed: false,
                });
            }
        } else if let Some(amount) = p.amount {
            inserted = Some(UsageRow {
                id: p.key.clone(),
                agent: snapshot.agent.id.clone(),
                kind: if p.exact { "response" } else { "notice" }.into(),
                tokens: amount,
                at: p.at,
            });
            if p.exact {
                if p.estimated {
                    state.fault("estimatedResponseTime");
                    state.before = add(state.before, amount)?;
                } else if p.at > state.cutoff {
                    state.after = add(state.after, amount)?;
                } else {
                    state.before = add(state.before, amount)?;
                }
            } else if p.source != CaptureSource::Live {
                if !p.response {
                    state.historical_notices = add(state.historical_notices, amount)?;
                }
            } else if let Some(counter) = p.counter {
                if p.at < state.notice_at {
                    state.notice_spent = state.notice_spent.max(counter);
                } else {
                    let reset = state.counter.is_some_and(|old| counter < old);
                    let mut delta = if state.counter.is_none() || p.thread != state.thread {
                        amount
                    } else if reset {
                        counter
                    } else {
                        counter.saturating_sub(state.counter.unwrap_or(0))
                    };
                    if reset && Some(amount) == state.last_amount {
                        delta = 0;
                        if !p.response {
                            state.ambiguous_notices.push(p.key.clone());
                        }
                    }
                    state.notice_spent = add(state.notice_spent, delta)?;
                    state.counter = Some(counter);
                    state.thread = p.thread.clone();
                    state.notice_at = p.at;
                    state.last_amount = Some(amount);
                }
            } else {
                state.notice_spent = add(state.notice_spent, amount)?;
            }
        } else {
            state.fault("missingRequestUsage");
        }
    }
    state.settle()?;
    Ok(CaptureDecision {
        state,
        usage: inserted,
        changed: true,
    })
}

/// A bounded synthetic or collected file observation. Collection happens outside
/// the transaction; callers must invalidate it when history/session identity changes.
#[derive(Clone, Debug, Deserialize)]
pub struct HistoryFileObservation {
    pub identity: [u64; 2],
    pub size: u64,
    pub final_size: u64,
    pub tail: String,
    #[serde(default)]
    pub modified_seconds: u64,
    #[serde(default)]
    pub modified_nanos: u32,
    #[serde(default)]
    pub content_hash: String,
}
#[derive(Clone, Debug)]
pub struct HistoryObservation {
    agent: String,
    account: String,
    thread: Option<String>,
    reason: Option<String>,
    request_usage: bool,
    runtime_binding: Option<HistoryRuntimeBinding>,
}
#[derive(Clone, Debug)]
pub struct HistoryRuntimeBinding {
    pub agent: Value,
    pub budget: Option<Value>,
    pub checkpoint_id: String,
    pub checkpoint_record: Option<String>,
    pub file: Option<HistoryFileObservation>,
}
impl HistoryObservation {
    pub fn from_records(
        agent: &Value,
        history: Option<&Value>,
        file: Option<&HistoryFileObservation>,
    ) -> Result<Self, BudgetError> {
        validate("PersistedBudgetAgent", agent, "runtime_agents")?;
        if let Some(history) = history {
            validate("BudgetHistory", history, "analytics_history")?;
        }
        let a: Agent = decode(agent, "runtime_agents/history")?;
        let mut observation = Self {
            agent: a.id,
            account: a.account_key,
            thread: a.thread_id,
            reason: None,
            request_usage: false,
            runtime_binding: None,
        };
        let Some(history) = history else {
            observation.reason = Some("Native usage history is unavailable".into());
            return Ok(observation);
        };
        if !history.is_object() {
            return Err(invalid("analytics_history"));
        }
        observation.request_usage = truthy(
            history
                .get("context")
                .and_then(|v| v.get("requestUsageAvailable")),
        );
        let reason = if history.get("status").and_then(Value::as_str) != Some("current")
            || history.get("coverage").and_then(Value::as_str) == Some("partial")
        {
            Some("Native usage history is incomplete")
        } else if let Some(file) = file {
            let offset = history
                .get("offset")
                .map(|v| {
                    v.as_u64()
                        .ok_or_else(|| invalid("analytics_history/offset"))
                })
                .transpose()?
                .unwrap_or(0);
            if history.get("identity") != Some(&json!(file.identity)) || offset > file.size {
                Some("Native usage history identity changed")
            } else if file.size > offset {
                let tail = file.tail.as_bytes();
                if file.size - offset > MAX_TAIL
                    || tail.len() as u64 > MAX_TAIL
                    || tail.len() as u64 != file.size - offset
                    || !tail.ends_with(b"\n")
                    || file.final_size != file.size
                {
                    Some("Native usage history has not caught up")
                } else {
                    let mut reason = None;
                    for line in file.tail.lines() {
                        match serde_json::from_str::<Value>(line) {
                            Ok(record) if record.is_object() => {
                                if record
                                    .get("payload")
                                    .is_some_and(|p| truthy(Some(p)) && !p.is_object())
                                {
                                    reason = Some("Native usage history is unavailable");
                                    break;
                                }
                                if matches!(
                                    record.get("type").and_then(Value::as_str),
                                    Some("token_usage_record" | "compacted")
                                ) || record
                                    .get("payload")
                                    .and_then(|v| v.get("type"))
                                    .and_then(Value::as_str)
                                    == Some("token_count")
                                {
                                    reason = Some("Native usage history has not caught up");
                                    break;
                                }
                            }
                            _ => {
                                reason = Some("Native usage history is unavailable");
                                break;
                            }
                        }
                    }
                    reason
                }
            } else {
                None
            }
        } else {
            Some("Native usage history is unavailable")
        };
        observation.reason = reason.map(str::to_owned);
        Ok(observation)
    }
    pub fn bind_runtime(&mut self, binding: HistoryRuntimeBinding) {
        self.runtime_binding = Some(binding);
    }
    pub(crate) fn runtime_binding(&self) -> Option<&HistoryRuntimeBinding> {
        self.runtime_binding.as_ref()
    }
}
fn coverage(snapshot: &BudgetSnapshot, observation: Option<&HistoryObservation>) -> Option<String> {
    let a = &snapshot.agent;
    let state = &snapshot.state;
    if !state.faults.is_empty() {
        return Some(state.faults.join(", "));
    }
    if !state.ambiguous_notices.is_empty() {
        return Some("Native counter reset has no response identity".into());
    }
    if a.thread_id.as_deref().is_none_or(str::is_empty) {
        return (state.spent != 0).then(|| "Native usage history is unavailable".into());
    }
    if state.spent == 0
        && a.turn_id.as_deref().is_none_or(str::is_empty)
        && a.last_completed_turn.as_deref().is_none_or(str::is_empty)
    {
        return None;
    }
    let Some(observation) = observation
        .filter(|o| o.agent == a.id && o.account == a.account_key && o.thread == a.thread_id)
    else {
        return Some("Native usage history is unavailable".into());
    };
    if observation.reason.is_some() {
        return observation.reason.clone();
    }
    if state.spent != 0 && !observation.request_usage {
        return Some("Native history lacks response usage records".into());
    }
    if state.before.saturating_add(state.after)
        < state
            .floor
            .max(state.notice_spent)
            .max(state.historical_notices)
    {
        return Some("Response usage does not cover the observed lifetime counter".into());
    }
    None
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BudgetStatus {
    pub root_id: String,
    pub token_budget: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_used: Option<u64>,
    pub reached: bool,
    pub incomplete: Vec<Incomplete>,
}
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Incomplete {
    pub agent_id: String,
    pub reason: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct AdmissionDecision {
    pub status: BudgetStatus,
    pub denial: Option<AdmissionDenial>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct AdmissionDenial {
    pub message: String,
    pub receipt: Value,
}

/// Team selection must come from the transaction's canonical scoped agent query,
/// including deleted agents. This function verifies membership again.
pub fn admission(
    agent_record: &Value,
    root_record: &Value,
    members: &[BudgetSnapshot],
    histories: &BTreeMap<String, HistoryObservation>,
) -> Result<AdmissionDecision, BudgetError> {
    validate("PersistedBudgetAgent", agent_record, "runtime_agents")?;
    validate("PersistedBudgetAgent", root_record, "runtime_agents")?;
    let agent: Agent = decode(agent_record, "runtime_agents/budget")?;
    let root: Agent = decode(root_record, "runtime_agents/root")?;
    if agent
        .root_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&agent.id)
        != root.id
    {
        return Err(invalid("runtime_agents/rootId"));
    }
    let mut status = BudgetStatus {
        root_id: root.id.clone(),
        token_budget: root.token_budget,
        tokens_used: None,
        reached: false,
        incomplete: vec![],
    };
    if root.token_budget.is_none() {
        return Ok(AdmissionDecision {
            status,
            denial: None,
        });
    }
    status = team_status(root_record, members, histories, true)?;
    let message = if status.reached {
        Some("Team token budget reached. Increase the budget before resuming".into())
    } else {
        status
            .incomplete
            .first()
            .map(|i| format!("Team token budget cannot be verified: {}", i.reason))
    };
    let denial = message.map(|message| {
        let mut receipt = json!(status);
        receipt["agentId"] = json!(agent.id);
        receipt["epoch"] = agent_record.get("epoch").cloned().unwrap_or(Value::Null);
        receipt["accountKey"] = json!(agent.account_key);
        receipt["threadId"] = json!(agent.thread_id);
        receipt["attemptId"] = agent_record
            .get("startAttempt")
            .and_then(|v| v.get("id"))
            .cloned()
            .unwrap_or(Value::Null);
        AdmissionDenial { message, receipt }
    });
    Ok(AdmissionDecision { status, denial })
}

/// Display totals even for unlimited teams; coverage can be skipped explicitly.
pub fn team_status(
    root_record: &Value,
    members: &[BudgetSnapshot],
    histories: &BTreeMap<String, HistoryObservation>,
    check_coverage: bool,
) -> Result<BudgetStatus, BudgetError> {
    validate("PersistedBudgetAgent", root_record, "runtime_agents/root")?;
    let root: Agent = decode(root_record, "runtime_agents/root")?;
    let mut status = BudgetStatus {
        root_id: root.id.clone(),
        token_budget: root.token_budget,
        tokens_used: None,
        reached: false,
        incomplete: vec![],
    };
    let mut spent = 0;
    for member in members {
        if member
            .agent
            .root_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&member.agent.id)
            != root.id
        {
            continue;
        }
        spent = add(spent, member.state.spent)?;
        if check_coverage
            && root.token_budget.is_some()
            && let Some(reason) = coverage(member, histories.get(&member.agent.id))
        {
            status.incomplete.push(Incomplete {
                agent_id: member.agent.id.clone(),
                reason,
            });
        }
    }
    status.tokens_used = Some(spent);
    status.reached = root.token_budget.is_some_and(|budget| spent >= budget);
    Ok(status)
}
