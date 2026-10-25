//! Pure lifetime accounting and team admission. All time and history observations
//! are supplied by the caller; this module never opens native session files.
use crate::event_write::truthy;
use crate::provider_adapter::{self, ProviderKind};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use studio_runtime_schema::usage::ledger::{UsageCorrection, UsageLedgerRow, correction};
use studio_runtime_schema::usage::{UsageEvidence, UsageTokens};
use studio_runtime_store::{python_json_with_separators, sha256_hex};
use thiserror::Error;

pub const MAX_TOKENS: u64 = 9_007_199_254_740_991;
const MAX_TAIL: u64 = 65_536;
const MAX_RECORD_BYTES: usize = 1_048_576;

#[derive(Debug, Error)]
pub enum BudgetError {
    #[error(transparent)]
    Analytics(#[from] studio_runtime_store::AnalyticsStoreError),
    #[error("invalid budget input at {0}")]
    Invalid(String),
    #[error("budget-policy/token-overflow")]
    Overflow,
    #[error("budget/admission/staleHistory; pre-read histories and retry admission")]
    StaleHistory,
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
    /// Last native cumulative categories; absent on pre-contract ledgers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    counter_tokens: Option<UsageTokens>,
    /// Previous native last-response categories, used only for reset ambiguity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_tokens: Option<UsageTokens>,
    thread: Option<String>,
    notice_at: f64,
    created: f64,
    last_amount: Option<u64>,
    ambiguous_notices: Vec<String>,
    faults: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    unresolved_gaps: Vec<String>,
    #[serde(default)]
    gap_coverage: studio_runtime_schema::usage::ledger::GapCoverage,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    replay_conflicts: Vec<studio_runtime_schema::usage::ledger::UsageReplayConflict>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    unknown_receipts: Vec<String>,
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

pub(crate) fn needs_legacy_gap_coverage(value: &Value) -> Result<bool, BudgetError> {
    let state: BudgetState = decode(value, "runtime_budget")?;
    Ok(
        state.gap_coverage == studio_runtime_schema::usage::ledger::GapCoverage::LegacyUnknown
            || value.get("gapCoverage").is_none()
                && state.faults.iter().any(|fault| {
                    fault == studio_runtime_schema::usage::ledger::MISSING_FINAL_USAGE
                }),
    )
}

pub(crate) fn establish_gap_coverage(
    value: &Value,
    unresolved: Vec<String>,
) -> Result<BudgetState, BudgetError> {
    let mut state: BudgetState = decode(value, "runtime_budget")?;
    state.gap_coverage = studio_runtime_schema::usage::ledger::GapCoverage::Complete;
    for key in unresolved {
        if !state.unresolved_gaps.contains(&key) {
            state.unresolved_gaps.push(key);
        }
    }
    if state.unresolved_gaps.is_empty() {
        state
            .faults
            .retain(|fault| fault != studio_runtime_schema::usage::ledger::MISSING_FINAL_USAGE);
    }
    Ok(state)
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
    live_only: bool,
    live_usage: Option<u64>,
    provider: studio_runtime_schema::usage::UsageProvider,
}
impl BudgetSnapshot {
    pub fn from_records(
        agent: &Value,
        durable_agent: &Value,
        state: Option<&Value>,
        now: f64,
    ) -> Result<Self, BudgetError> {
        Self::from_records_with_history_turn(agent, durable_agent, state, now, false)
    }

    /// Build a budget snapshot while importing historical usage. The supplied
    /// agent's turn may be rollout context newer than the event turn; all other
    /// supplied identity fields remain assertions against the durable agent.
    pub(crate) fn from_history_records(
        agent: &Value,
        durable_agent: &Value,
        state: Option<&Value>,
        now: f64,
    ) -> Result<Self, BudgetError> {
        Self::from_records_with_history_turn(agent, durable_agent, state, now, true)
    }

    fn from_records_with_history_turn(
        agent: &Value,
        durable_agent: &Value,
        state: Option<&Value>,
        now: f64,
        historical: bool,
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
            if name == "turnId" && historical {
                continue;
            }
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
        let legacy_unknown = state
            .map(needs_legacy_gap_coverage)
            .transpose()?
            .unwrap_or(false);
        let mut state: BudgetState = match state {
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
                counter_tokens: None,
                last_tokens: None,
                thread: durable.thread_id.clone(),
                notice_at: 0.,
                created: durable.created,
                last_amount: None,
                ambiguous_notices: vec![],
                faults: vec![],
                unresolved_gaps: vec![],
                gap_coverage: studio_runtime_schema::usage::ledger::GapCoverage::Complete,
                replay_conflicts: vec![],
                unknown_receipts: vec![],
                extra: BTreeMap::new(),
            },
        };
        if legacy_unknown {
            state.gap_coverage = studio_runtime_schema::usage::ledger::GapCoverage::LegacyUnknown;
        }
        Ok(Self {
            agent: durable,
            state,
            initialized,
            live_only: uses_live_usage(durable_agent),
            live_usage: None,
            provider: match ProviderKind::of_record(durable_agent)
                .map_err(|_| invalid("runtime_agents/provider"))?
            {
                ProviderKind::Claude => studio_runtime_schema::usage::UsageProvider::Claude,
                ProviderKind::Codex => studio_runtime_schema::usage::UsageProvider::Codex,
            },
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
    pub(crate) fn needs_live_usage(&self) -> bool {
        self.live_only
    }
    pub(crate) fn bind_live_usage(&mut self, tokens: u64) {
        self.live_usage = Some(tokens);
    }
}

fn uses_live_usage(agent: &Value) -> bool {
    ProviderKind::of_record(agent).is_ok_and(|provider| {
        !provider_adapter::adapter(provider)
            .capabilities()
            .native_history
    })
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CaptureSource {
    Live,
    History,
}

#[derive(Clone, Debug)]
pub struct UsageObservation {
    ledger: Option<UsageLedgerRow>,
    identity: studio_runtime_schema::usage::ledger::UsageIdentity,
    key: String,
    legacy_keys: Vec<String>,
    unresolved_receipt: bool,
    scoped_capture_key: Option<String>,
    exact: bool,
    amount: Option<u64>,
    counter: Option<u64>,
    tokens: Option<UsageTokens>,
    last_tokens: Option<UsageTokens>,
    response: bool,
    thread: Option<String>,
    estimated: bool,
    gap: bool,
    informational: bool,
    at: f64,
    source: CaptureSource,
}
impl UsageObservation {
    pub fn from_record(
        snapshot: &BudgetSnapshot,
        params: &Value,
        at: f64,
        source: CaptureSource,
    ) -> Result<Self, BudgetError> {
        Self::from_input(snapshot, params, at, source, None)
    }

    /// Restore a typed analytics receipt without reconstructing or parsing a notification.
    pub fn from_persisted(
        snapshot: &BudgetSnapshot,
        row: &Value,
        at: f64,
        source: CaptureSource,
    ) -> Result<Self, BudgetError> {
        if row["agentId"].as_str() != Some(&snapshot.agent.id) {
            return Err(invalid("analytics_usage/agentId"));
        }
        let ledger = UsageLedgerRow::from_analytics(snapshot.provider, row)
            .map_err(|_| invalid("analytics_usage/ledger"))?;
        Self::from_input(snapshot, row, at, source, Some(ledger))
    }

    fn from_input(
        snapshot: &BudgetSnapshot,
        params: &Value,
        at: f64,
        source: CaptureSource,
        supplied: Option<UsageLedgerRow>,
    ) -> Result<Self, BudgetError> {
        use studio_runtime_schema::usage::{
            UsageObservation as ParsedUsage, parse_legacy_totals, parse_token_usage,
        };
        if !params.is_object() || params.to_string().len() > MAX_RECORD_BYTES || !at.is_finite() {
            return Err(invalid("tokenUsage"));
        }
        validate("BudgetCapture", params, "tokenUsage")?;
        let parsed = supplied
            .as_ref()
            .map(|row| Ok(row.studio_usage.clone()))
            .unwrap_or_else(|| parse_token_usage(snapshot.provider, params));
        let typed = parsed.as_ref().ok().cloned();
        let informational = matches!(
            &parsed,
            Ok(ParsedUsage::Cumulative {
                scope: studio_runtime_schema::usage::UsageScope::ClaudeQuery,
                ..
            })
        );
        let tokens = match &parsed {
            Ok(ParsedUsage::Response { tokens, .. }) => Some(tokens.clone()),
            Ok(ParsedUsage::Cumulative { total, .. }) => Some(total.clone()),
            _ => None,
        };
        let last_tokens = match &parsed {
            Ok(ParsedUsage::Cumulative { last, .. }) => last.clone(),
            _ => None,
        };
        let (exact, amount, counter, response, turn, identity_usage, gap) = match parsed {
            Ok(ref observation) => {
                let identity_usage =
                    serde_json::to_value(observation).map_err(|_| invalid("tokenUsage"))?;
                match observation {
                    ParsedUsage::Response {
                        response_id,
                        turn_id,
                        tokens,
                        ..
                    } => (
                        true,
                        Some(tokens.total().ok_or(BudgetError::Overflow)?),
                        None,
                        Some(response_id.clone()),
                        turn_id.clone(),
                        identity_usage,
                        false,
                    ),
                    ParsedUsage::Cumulative {
                        turn_id,
                        total,
                        last,
                        ..
                    } => (
                        false,
                        Some(
                            last.as_ref()
                                .unwrap_or(total)
                                .total()
                                .ok_or(BudgetError::Overflow)?,
                        ),
                        Some(total.total().ok_or(BudgetError::Overflow)?),
                        studio_runtime_schema::usage::legacy_notice_response_association(params)
                            .map(str::to_owned),
                        turn_id.clone(),
                        identity_usage,
                        false,
                    ),
                    ParsedUsage::Gap {
                        response_id,
                        turn_id,
                        ..
                    } => (
                        false,
                        None,
                        None,
                        Some(response_id.clone()),
                        turn_id.clone(),
                        identity_usage,
                        true,
                    ),
                }
            }
            Err(_) => {
                let legacy = parse_legacy_totals(snapshot.provider, params)
                    .map_err(|_| invalid("tokenUsage"))?;
                (
                    legacy.response_id.is_some(),
                    Some(legacy.last_total_tokens.unwrap_or(legacy.total_tokens)),
                    if legacy.response_id.is_none() {
                        Some(legacy.total_tokens)
                    } else {
                        None
                    },
                    legacy.response_id,
                    legacy.turn_id,
                    json!([legacy.total_tokens, legacy.last_total_tokens]),
                    false,
                )
            }
        };
        if amount.is_some_and(|n| n > MAX_TOKENS) || counter.is_some_and(|n| n > MAX_TOKENS) {
            return Err(BudgetError::Overflow);
        }
        let a = &snapshot.agent;
        let identity_turn = match source {
            CaptureSource::History => turn.clone(),
            CaptureSource::Live => turn.clone().or_else(|| a.turn_id.clone()),
        };
        let identity = if exact {
            json!([a.id, "response", response])
        } else {
            studio_runtime_schema::usage::legacy_notice_identity(
                &a.id,
                &a.account_key,
                identity_turn.as_deref(),
                params,
            )
            .unwrap_or_else(|| {
                json!([a.id, a.account_key, "notice", identity_turn, identity_usage])
            })
        };
        let thread = params
            .get("threadId")
            .filter(|v| truthy(Some(v)))
            .map(|v| decode::<String>(v, "tokenUsage/threadId"))
            .transpose()?
            .or_else(|| a.thread_id.clone());
        let restoring = supplied.is_some();
        let ledger = supplied.or_else(|| {
            typed.map(|observation| {
                UsageLedgerRow::capture(
                    &a.id,
                    &a.account_key,
                    thread.as_deref(),
                    identity_turn.as_deref(),
                    params,
                    observation,
                )
            })
        });
        let unresolved_receipt = ledger.as_ref().is_some_and(|row| row.receipt_key.is_none());
        let key = match &ledger {
            Some(row) => row.receipt_key.clone().unwrap_or_default(),
            None => sha256_hex(python_json_with_separators(&identity, ",", ":").as_bytes()),
        };
        let legacy_keys = ledger
            .as_ref()
            .map(|row| {
                if restoring {
                    studio_runtime_schema::usage::ledger::legacy_analytics_receipt_keys(
                        params,
                        &row.studio_usage,
                    )
                } else {
                    studio_runtime_schema::usage::ledger::legacy_receipt_keys(
                        &a.id,
                        &a.account_key,
                        identity_turn.as_deref(),
                        params,
                        &row.studio_usage,
                    )
                }
            })
            .unwrap_or_default();
        let scoped_capture_key = if !restoring
            && matches!(
                ledger.as_ref().map(|row| &row.studio_usage),
                Some(studio_runtime_schema::usage::UsageObservation::Cumulative { .. })
            ) {
            Some(key.clone())
        } else {
            None
        };
        let identity = ledger
            .as_ref()
            .map(|row| row.usage_identity.clone())
            .unwrap_or_else(|| match &response {
                Some(response_id) if exact => {
                    studio_runtime_schema::usage::ledger::UsageIdentity::Response {
                        agent: a.id.clone(),
                        response_id: response_id.clone(),
                    }
                }
                _ => studio_runtime_schema::usage::ledger::UsageIdentity::Cumulative {
                    agent: a.id.clone(),
                    thread_id: thread.clone(),
                    scope: studio_runtime_schema::usage::UsageScope::ClaudeQuery,
                },
            });
        Ok(Self {
            key,
            legacy_keys,
            unresolved_receipt,
            scoped_capture_key,
            ledger,
            identity,
            exact,
            amount,
            counter,
            tokens,
            last_tokens,
            response: response.is_some(),
            thread,
            gap,
            informational,
            estimated: params
                .get("_analyticsTimestampSource")
                .and_then(Value::as_str)
                == Some("fileModifiedEstimate"),
            at,
            source,
        })
    }
    pub(crate) fn receipt_candidates(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.key.as_str())
            .filter(|key| !key.is_empty())
            .chain(self.legacy_keys.iter().map(String::as_str))
    }
    pub(crate) fn needs_receipt_ownership(&self, row: &UsageRow) -> bool {
        matches!(
            self.identity,
            studio_runtime_schema::usage::ledger::UsageIdentity::Cumulative { .. }
        ) && row.details.is_none()
            && self.scoped_capture_key.as_deref() != Some(row.id.as_str())
    }
    pub(crate) fn accepts_receipt(&self, row: &UsageRow) -> bool {
        !self.needs_receipt_ownership(row)
            && row
                .details
                .as_ref()
                .is_none_or(|details| details.ledger.usage_identity == self.identity)
    }
    pub(crate) fn reuse_receipt(&mut self, key: String) {
        self.key = key.clone();
        self.unresolved_receipt = false;
        if let Some(ledger) = &mut self.ledger {
            ledger.receipt_key = Some(key);
        }
    }
    pub(crate) fn mark_unknown_receipt(&mut self) {
        self.unresolved_receipt = true;
    }
    pub fn key(&self) -> &str {
        &self.key
    }
}

pub use studio_runtime_schema::usage::ledger::UsageAcceptance;
pub use studio_runtime_store::budget::BudgetUsageRow as UsageRow;
#[derive(Clone, Debug, Serialize)]
pub struct CaptureDecision {
    pub acceptance: UsageAcceptance,
    pub state: BudgetState,
    pub usage: Option<UsageRow>,
    pub changed: bool,
}

fn rejected_replay(
    mut state: BudgetState,
    observation: &UsageObservation,
    previous: &UsageRow,
) -> Result<CaptureDecision, BudgetError> {
    let conflict = studio_runtime_schema::usage::ledger::UsageReplayConflict {
        identity: observation.identity.clone(),
        retained_total_tokens: previous.tokens,
        incoming_total_tokens: observation
            .amount
            .ok_or_else(|| invalid("tokenUsage/conflict"))?,
        retained_tokens: previous.breakdown.clone(),
        incoming_tokens: observation.tokens.clone(),
    };
    state.fault(studio_runtime_schema::usage::ledger::RESPONSE_REPLAY_CONFLICT);
    if !state.replay_conflicts.contains(&conflict) {
        state.replay_conflicts.push(conflict.clone());
    }
    Ok(CaptureDecision {
        acceptance: UsageAcceptance::RejectedReplayConflict(conflict),
        state,
        usage: None,
        changed: true,
    })
}

pub fn capture(
    snapshot: &BudgetSnapshot,
    observation: &UsageObservation,
    previous: Option<&UsageRow>,
) -> Result<CaptureDecision, BudgetError> {
    let mut state = snapshot.state.clone();
    let p = observation;
    if p.informational {
        return Ok(CaptureDecision {
            acceptance: UsageAcceptance::Accepted,
            state,
            usage: None,
            changed: false,
        });
    }
    let observation_key = p.ledger.as_ref().map(UsageLedgerRow::observation_key);
    if previous.is_some() && !p.unresolved_receipt {
        if let Some(key) = &observation_key {
            state.unknown_receipts.retain(|unknown| unknown != key);
        }
        if state.unknown_receipts.is_empty() {
            state.faults.retain(|fault| {
                fault != studio_runtime_schema::usage::ledger::UNKNOWN_USAGE_RECEIPT
            });
        }
    }
    if p.unresolved_receipt
        || observation_key
            .as_ref()
            .is_some_and(|key| state.unknown_receipts.contains(key))
    {
        if let Some(key) = observation_key
            && !state.unknown_receipts.contains(&key)
        {
            state.unknown_receipts.push(key);
        }
        state.fault(studio_runtime_schema::usage::ledger::UNKNOWN_USAGE_RECEIPT);
        return Ok(CaptureDecision {
            acceptance: UsageAcceptance::Accepted,
            state,
            usage: None,
            changed: true,
        });
    }
    if let Some(row) = previous {
        if row.id != p.key || row.agent != snapshot.agent.id {
            return Err(invalid("runtime_budget_usage/identity"));
        }
        if p.gap && row.kind == "response" && row.details.is_none() {
            return Ok(CaptureDecision {
                acceptance: UsageAcceptance::Accepted,
                state,
                usage: None,
                changed: false,
            });
        }
    }
    let mut inserted = None;
    let mut previous = previous;
    let mut replacement_before = None;
    if let (Some(incoming), Some(old)) = (&p.ledger, previous.and_then(|row| row.details.as_ref()))
    {
        if incoming.usage_identity != old.ledger.usage_identity {
            return Err(invalid("runtime_budget_usage/identity"));
        }
        match correction(&old.ledger.studio_usage, &incoming.studio_usage) {
            UsageCorrection::UnresolvedGap => {}
            UsageCorrection::Replay if !p.exact && !p.gap => {}
            UsageCorrection::Replay => {
                return Ok(CaptureDecision {
                    acceptance: UsageAcceptance::Accepted,
                    state,
                    usage: None,
                    changed: false,
                });
            }
            UsageCorrection::ResponseReplayConflict => {
                return rejected_replay(
                    state,
                    p,
                    previous.ok_or_else(|| invalid("runtime_budget_usage/conflict"))?,
                );
            }
            UsageCorrection::Apply => {
                if let Some(row) = previous.filter(|row| row.kind == "response") {
                    let bucket = if old.before_cutoff {
                        &mut state.before
                    } else {
                        &mut state.after
                    };
                    *bucket = bucket
                        .checked_sub(row.tokens)
                        .ok_or_else(|| invalid("runtime_budget_usage/placement"))?;
                    state.spent = state.spent.saturating_sub(row.tokens);
                    replacement_before = Some(old.before_cutoff);
                }
                previous = None;
            }
        }
    }
    let is_final = p.ledger.as_ref().is_some_and(|row| matches!(&row.studio_usage, studio_runtime_schema::usage::UsageObservation::Response { evidence, .. } if *evidence != UsageEvidence::Legacy));
    if is_final {
        state.unresolved_gaps.retain(|key| key != &p.key);
        if state.unresolved_gaps.is_empty()
            && state.gap_coverage == studio_runtime_schema::usage::ledger::GapCoverage::Complete
        {
            state
                .faults
                .retain(|fault| fault != studio_runtime_schema::usage::ledger::MISSING_FINAL_USAGE);
        }
    }
    let before_cutoff = replacement_before.unwrap_or(p.estimated || p.at <= state.cutoff);
    let details = p
        .ledger
        .clone()
        .map(|ledger| studio_runtime_store::budget::BudgetUsageDetails {
            ledger,
            before_cutoff,
        });
    if p.gap {
        if !state.unresolved_gaps.contains(&p.key) {
            state.unresolved_gaps.push(p.key.clone());
        }
        state.fault(studio_runtime_schema::usage::ledger::MISSING_FINAL_USAGE);
        if previous.is_none() {
            inserted = Some(UsageRow {
                id: p.key.clone(),
                agent: snapshot.agent.id.clone(),
                kind: "notice".into(),
                tokens: 0,
                at: p.at,
                breakdown: None,
                details,
            });
        }
    } else if p.source == CaptureSource::Live || p.at >= state.created {
        if let Some(previous) = previous {
            if previous.id != p.key || previous.agent != snapshot.agent.id {
                return Err(invalid("runtime_budget_usage/identity"));
            }
            if !p.exact && p.response {
                state.ambiguous_notices.retain(|key| key != &p.key);
            }
            if p.exact {
                if previous.kind != "response"
                    || p.amount != Some(previous.tokens)
                    || previous
                        .breakdown
                        .as_ref()
                        .is_some_and(|old| p.tokens.as_ref() != Some(old))
                {
                    return rejected_replay(state, p, previous);
                }
                return Ok(CaptureDecision {
                    acceptance: UsageAcceptance::Accepted,
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
                breakdown: p.exact.then(|| p.tokens.clone()).flatten(),
                details,
            });
            if p.exact {
                if p.estimated {
                    state.fault("estimatedResponseTime");
                    state.before = add(state.before, amount)?;
                } else if before_cutoff {
                    state.before = add(state.before, amount)?;
                } else {
                    state.after = add(state.after, amount)?;
                }
            } else if p.source != CaptureSource::Live {
                if !p.response {
                    state.historical_notices = add(state.historical_notices, amount)?;
                }
            } else if let Some(counter) = p.counter {
                if p.at < state.notice_at {
                    state.notice_spent = state.notice_spent.max(counter);
                } else {
                    let reset = match (&p.tokens, &state.counter_tokens) {
                        (Some(tokens), Some(previous)) => tokens.decreased_from(previous),
                        _ => state.counter.is_some_and(|old| counter < old),
                    };
                    let mut delta = if state.counter.is_none() || p.thread != state.thread {
                        amount
                    } else if reset {
                        counter
                    } else {
                        counter.saturating_sub(state.counter.unwrap_or(0))
                    };
                    let ambiguous_reset = match (&p.tokens, &state.counter_tokens) {
                        (Some(total), Some(_)) => p.last_tokens.as_ref().is_some_and(|last| {
                            Some(last) == state.last_tokens.as_ref() && total == last
                        }),
                        // Until the first typed counter is stored, preserve the old heuristic.
                        _ => Some(amount) == state.last_amount,
                    };
                    if reset && ambiguous_reset {
                        delta = 0;
                        if !p.response {
                            state.ambiguous_notices.push(p.key.clone());
                        }
                    }
                    state.notice_spent = add(state.notice_spent, delta)?;
                    state.counter = Some(counter);
                    state.counter_tokens = p.tokens.clone();
                    state.last_tokens = p.last_tokens.clone();
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
        acceptance: UsageAcceptance::Accepted,
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
        // These providers have no rollout to import. Admission checks their
        // durable response rows against the ledger in the caller's transaction.
        if uses_live_usage(agent) {
            if history.is_some_and(|history| history["coverage"] == "partial") {
                observation.reason = Some("Live usage history is incomplete".into());
            }
            return Ok(observation);
        }
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
    if !snapshot.live_only && a.thread_id.as_deref().is_none_or(str::is_empty) {
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
    if snapshot.live_only {
        return (!snapshot.live_usage.is_some_and(|tokens| {
            tokens == state.before.saturating_add(state.after)
                && tokens == state.spent
                && tokens >= a.tokens_used.unwrap_or(0)
        }))
        .then(|| "Live response usage is missing or incomplete".into());
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

#[cfg(test)]
mod typed_usage_tests {
    use super::*;
    fn snapshot() -> BudgetSnapshot {
        let agent = json!({"id":"a","provider":"claude","threadId":"t","turnId":"u","created":0});
        BudgetSnapshot::from_records(&agent, &agent, None, 100.).expect("snapshot")
    }
    #[test]
    fn claude_query_totals_never_charge_budget_across_restarts() {
        let mut snapshot = snapshot();
        for (index, total) in [100, 200, 50, 500].into_iter().enumerate() {
            let params = json!({"studioUsage":{"kind":"cumulative","scope":"claudeQuery","total":{"uncachedInput":total,"cacheReadInput":0,"cacheWriteInput":0,"output":0}}});
            let observation = UsageObservation::from_record(
                &snapshot,
                &params,
                101. + index as f64,
                CaptureSource::Live,
            )
            .expect("query totals");
            let decision =
                capture(&snapshot, &observation, None).expect("informational query usage");
            assert_eq!(
                decision.state.spent, 0,
                "higher and lower restarted query totals are informational"
            );
            assert!(decision.usage.is_none());
            snapshot.state = decision.state;
        }
    }

    #[test]
    fn native_live_and_imported_notices_recognize_pre_upgrade_receipt_keys() {
        let agent = json!({"id":"a","provider":"codex","accountKey":"default","threadId":"t","turnId":"u","created":0});
        let snapshot = BudgetSnapshot::from_records(&agent, &agent, None, 100.).expect("snapshot");
        let total = json!({"inputTokens":20,"cachedInputTokens":5,"cacheWriteInputTokens":0,"outputTokens":4,"reasoningOutputTokens":1,"totalTokens":24});
        let last = json!({"inputTokens":5,"cachedInputTokens":1,"cacheWriteInputTokens":0,"outputTokens":1,"reasoningOutputTokens":0,"totalTokens":6});
        let params = json!({"threadId":"t","turnId":"u","tokenUsage":{"total":total,"last":last}});
        let old = json!(["a", "default", "notice", "u", total, last]);
        let expected = sha256_hex(python_json_with_separators(&old, ",", ":").as_bytes());
        for source in [CaptureSource::Live, CaptureSource::History] {
            let observation = UsageObservation::from_record(&snapshot, &params, 101., source)
                .expect("native notice");
            assert!(
                observation.receipt_candidates().any(|key| key == expected),
                "pre-upgrade receipts remain deduplication aliases"
            );
            assert_ne!(
                observation.key(),
                expected,
                "new receipt keys include thread and counter scope"
            );
        }
    }

    #[test]
    fn pre_contract_notice_receipt_identity_survives_typed_parsing() {
        let snapshot = snapshot();
        let params = json!({"turnId":"u","tokenUsage":{"total":{"totalTokens":20},"last":{"totalTokens":5}}});
        let observation =
            UsageObservation::from_record(&snapshot, &params, 101., CaptureSource::History)
                .expect("old aggregate");
        let old_identity = json!(["a","default","notice","u",{"totalTokens":20},{"totalTokens":5}]);
        let old_key = sha256_hex(python_json_with_separators(&old_identity, ",", ":").as_bytes());
        assert_eq!(
            observation.key(),
            old_key,
            "pre-upgrade committed history receipts must be found on replay"
        );
    }

    #[test]
    fn typed_response_replay_gap_and_cumulative_reset() {
        let mut snapshot = snapshot();
        let observation = |snapshot: &BudgetSnapshot, usage: Value, at| {
            UsageObservation::from_record(
                snapshot,
                &json!({"threadId":"t","turnId":"u","studioUsage":usage}),
                at,
                CaptureSource::Live,
            )
            .expect("typed observation")
        };
        let response = json!({"kind":"response","responseId":"r","model":"claude","evidence":"live","tokens":{"uncachedInput":10,"cacheReadInput":0,"cacheWriteInput":0,"output":2}});
        let first = capture(
            &snapshot,
            &observation(&snapshot, response.clone(), 101.),
            None,
        )
        .expect("first");
        let previous = first.usage.expect("response row");
        snapshot.state = first.state;
        let replay = capture(
            &snapshot,
            &observation(&snapshot, response, 102.),
            Some(&previous),
        )
        .expect("replay");
        assert!(!replay.changed);
        assert_eq!(replay.state.spent, 12);
        let gap = capture(
            &snapshot,
            &observation(
                &snapshot,
                json!({"kind":"gap","responseId":"missing","reason":"queryClosed"}),
                103.,
            ),
            None,
        )
        .expect("gap");
        assert_eq!(gap.state.accounting(), "provisional");
        assert_eq!(gap.state.spent, 12);
        let receipt = gap.usage.as_ref().expect("persisted gap identity");
        assert_eq!(
            receipt.tokens, 0,
            "a gap has no debit; its typed usage remains unknown"
        );
        assert!(matches!(
            receipt
                .details
                .as_ref()
                .expect("typed gap")
                .ledger
                .studio_usage,
            studio_runtime_schema::usage::UsageObservation::Gap { .. }
        ));
        snapshot.state = gap.state;
        for (at, total, spent) in [(104., 20, 20), (105., 5, 25)] {
            let usage = json!({"kind":"cumulative","scope":"codexThread","total":{"uncachedInput":total,"cacheReadInput":0,"cacheWriteInput":0,"output":0}});
            let decision =
                capture(&snapshot, &observation(&snapshot, usage, at), None).expect("cumulative");
            assert_eq!(decision.state.spent, spent);
            snapshot.state = decision.state;
        }
    }

    #[test]
    fn response_count_does_not_grow_budget_state() {
        let size_after = |responses| {
            let mut snapshot = snapshot();
            for response in 0..responses {
                let params = json!({"studioUsage":{"kind":"response","responseId":format!("r-{response}"),"model":"claude","evidence":"live","tokens":{"uncachedInput":0,"cacheReadInput":0,"cacheWriteInput":0,"output":0}}});
                let observation =
                    UsageObservation::from_record(&snapshot, &params, 101., CaptureSource::Live)
                        .expect("response");
                snapshot.state = capture(&snapshot, &observation, None)
                    .expect("capture")
                    .state;
            }
            snapshot.state.record().expect("state").to_string().len()
        };
        assert_eq!(
            size_after(2),
            size_after(200),
            "response evidence belongs in rows, not the bounded agent state"
        );
    }

    #[test]
    fn category_reset_survives_persistence_and_total_only_upgrade() {
        let mut snapshot = snapshot();
        // Start from a pre-contract ledger: compare its total until typed evidence arrives.
        snapshot.state.counter = Some(100);
        snapshot.state.notice_spent = 100;
        snapshot.state.thread = Some("t".into());
        for (at, input, output, spent) in [(101., 100, 10, 110), (102., 20, 150, 280)] {
            let params = json!({"threadId":"t","studioUsage":{"kind":"cumulative","scope":"codexThread","total":{"uncachedInput":input,"cacheReadInput":0,"cacheWriteInput":0,"output":output}}});
            let observation =
                UsageObservation::from_record(&snapshot, &params, at, CaptureSource::Live)
                    .expect("counter");
            let decision = capture(&snapshot, &observation, None).expect("capture");
            assert_eq!(
                decision.state.spent, spent,
                "any category decrease starts a new epoch"
            );
            snapshot.state = decode(&decision.state.record().expect("persist"), "runtime_budget")
                .expect("reopen");
        }
    }

    #[test]
    fn category_reset_with_repeated_last_and_larger_total_is_charged() {
        let mut snapshot = snapshot();
        snapshot.state.counter = Some(100);
        snapshot.state.notice_spent = 100;
        snapshot.state.thread = Some("t".into());
        for (at, input, output, spent) in [(101., 100, 10, 110), (102., 20, 150, 280)] {
            let params = json!({"threadId":"t","studioUsage":{"kind":"cumulative","scope":"codexThread","total":{"uncachedInput":input,"cacheReadInput":0,"cacheWriteInput":0,"output":output},"last":{"uncachedInput":5,"cacheReadInput":0,"cacheWriteInput":0,"output":1}}});
            let observation =
                UsageObservation::from_record(&snapshot, &params, at, CaptureSource::Live)
                    .expect("counter");
            let decision = capture(&snapshot, &observation, None).expect("capture");
            assert_eq!(
                decision.state.spent, spent,
                "repeated last alone cannot suppress a typed reset"
            );
            snapshot.state = decode(&decision.state.record().expect("persist"), "runtime_budget")
                .expect("reopen");
        }
    }

    #[test]
    fn same_total_response_replay_compares_persisted_breakdown() {
        let mut snapshot = snapshot();
        let params = |input, output| json!({"studioUsage":{"kind":"response","responseId":"r","model":"claude","evidence":"live","tokens":{"uncachedInput":input,"cacheReadInput":0,"cacheWriteInput":0,"output":output}}});
        let observation =
            UsageObservation::from_record(&snapshot, &params(10, 2), 101., CaptureSource::Live)
                .expect("response");
        let decision = capture(&snapshot, &observation, None).expect("capture");
        let previous: UsageRow = serde_json::from_value(
            serde_json::to_value(decision.usage.expect("response row")).expect("persist row"),
        )
        .expect("reopen row");
        snapshot.state =
            decode(&decision.state.record().expect("persist"), "runtime_budget").expect("reopen");
        let conflict =
            UsageObservation::from_record(&snapshot, &params(9, 3), 102., CaptureSource::Live)
                .expect("replay");
        let decision = capture(&snapshot, &conflict, Some(&previous)).expect("conflict continues");
        assert!(decision.changed);
        assert!(
            decision
                .state
                .faults
                .iter()
                .any(|fault| fault == "responseReplayConflict")
        );
        assert_eq!(decision.state.spent, 12);
        assert!(decision.usage.is_none());
        snapshot.state = decision.state;
        let replay =
            capture(&snapshot, &observation, Some(&previous)).expect("first value retained");
        assert!(!replay.changed);

        // Records predating the typed ledger keep their total-only comparison.
        let mut previous = previous;
        previous.breakdown = None;
        previous.details = None;
        assert!(
            !capture(&snapshot, &conflict, Some(&previous))
                .expect("legacy comparison")
                .changed
        );
    }

    #[test]
    fn response_replay_conflict_records_fault_without_aborting_capture() {
        let snapshot = snapshot();
        let params = json!({"threadId":"t","usageSource":"claudeResponse","responseId":"r","model":"claude","requestUsage":{"inputTokens":10,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":2,"totalTokens":12}});
        let observation =
            UsageObservation::from_record(&snapshot, &params, 101., CaptureSource::Live)
                .expect("observation");
        let first = capture(&snapshot, &observation, None).expect("first");
        let previous = first.usage.expect("usage row");
        let mut snapshot = snapshot;
        snapshot.state = first.state;
        let mut changed = params;
        changed["requestUsage"]["outputTokens"] = json!(3);
        changed["requestUsage"]["totalTokens"] = json!(13);
        let changed = UsageObservation::from_record(&snapshot, &changed, 102., CaptureSource::Live)
            .expect("changed observation");
        let decision = capture(&snapshot, &changed, Some(&previous))
            .expect("conflict must not abort dispatch");
        assert!(decision.usage.is_none());
        assert_eq!(decision.state.spent, 12);
        assert!(
            decision
                .state
                .faults
                .iter()
                .any(|fault| fault == "responseReplayConflict")
        );
    }
}
