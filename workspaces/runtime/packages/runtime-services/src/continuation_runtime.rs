//! Completion and timer composition for the persisted continuation policies.
//! Decisions are staged under the caller's writer; wake ports only signal after commit.

use crate::{
    capacity_retry::{self, Action as CapacityAction, Failure as CapacityFailure},
    claude_auth_wait,
    continuation::{
        ContinuationError, Decision, Effect, Result, Snapshot, account, flag, present, text,
    },
    continuation_driver,
    usage_resume::{self, Cause as UsageCause, FailureObservation, LimitsObservation},
};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use studio_runtime_store::{ContinuationRepository, ContinuationTable, Transaction};

/// The exact settings copied by Python's `preparation_settings` helper.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparationSettings {
    model: Value,
    effort: Value,
    native_effort: Value,
    fast_mode: Value,
    yolo_mode: Value,
    profile_instructions: Value,
    role: Value,
    daybreak_enabled: Option<Value>,
    cyber_access_program: Option<Value>,
}

impl PreparationSettings {
    pub fn from_agent(agent: &Value) -> Result<Self> {
        if !agent.is_object() {
            return Err(ContinuationError::Invalid("/agent".into()));
        }
        Ok(Self {
            model: agent.get("model").cloned().unwrap_or(Value::Null),
            effort: agent.get("effort").cloned().unwrap_or(Value::Null),
            native_effort: agent.get("nativeEffort").cloned().unwrap_or(Value::Null),
            fast_mode: agent.get("fastMode").cloned().unwrap_or(Value::Null),
            yolo_mode: agent.get("yoloMode").cloned().unwrap_or(Value::Null),
            profile_instructions: agent
                .get("profileInstructions")
                .cloned()
                .unwrap_or(Value::Null),
            role: agent.get("role").cloned().unwrap_or(Value::Null),
            daybreak_enabled: agent.get("daybreakEnabled").cloned(),
            cyber_access_program: agent.get("cyberAccessProgram").cloned(),
        })
    }

    fn value(&self) -> Value {
        let mut value = json!({
            "model": self.model.clone(),
            "effort": self.effort.clone(),
            "nativeEffort": self.native_effort.clone(),
            "fastMode": self.fast_mode.clone(),
            "yoloMode": self.yolo_mode.clone(),
            "profileInstructions": self.profile_instructions.clone(),
            "role": self.role.clone(),
        });
        if let Some(enabled) = &self.daybreak_enabled {
            value["daybreakEnabled"] = enabled.clone();
        }
        if let Some(program) = &self.cyber_access_program {
            value["cyberAccessProgram"] = program.clone();
        }
        value
    }
}

/// Account data used by the local Claude auth probe. The account pin is already
/// projected by the P08 account adapter before the store transaction begins.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeAccountPin(Value);

impl ClaudeAccountPin {
    /// Keep the exact Python pin fields and reject malformed adapter projections.
    pub fn from_value(value: Value) -> Result<Self> {
        let fields = [
            "id",
            "provider",
            "home",
            "claudeOptions",
            "accountId",
            "_credentialIdentity",
            "deleted",
            "disconnected",
            "duplicateOf",
            "updatedAt",
        ];
        if !value.is_object() {
            return Err(ContinuationError::Invalid("/claude/accountPin".into()));
        }
        let value = fields
            .iter()
            .map(|field| {
                (
                    (*field).to_owned(),
                    value.get(*field).cloned().unwrap_or(Value::Null),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        let value = Value::Object(value);
        let strings = ["id", "provider", "home", "accountId", "duplicateOf"];
        let booleans = ["deleted", "disconnected"];
        if !value.is_object()
            || strings
                .iter()
                .any(|key| !value.get(*key).is_none_or(Value::is_null) && !value[*key].is_string())
            || booleans
                .iter()
                .any(|key| !value.get(*key).is_none_or(Value::is_null) && !value[*key].is_boolean())
        {
            return Err(ContinuationError::Invalid("/claude/accountPin".into()));
        }
        Ok(Self(value))
    }

    fn value(&self) -> &Value {
        &self.0
    }

    /// End the cached auth status of the pinned account profile.
    fn invalidate_cached_auth(&self) {
        use crate::account_driver::{AuthProfile, ProviderAuthReader, SystemProviderAuthReader};
        SystemProviderAuthReader.invalidate(&AuthProfile {
            provider: studio_runtime_providers::ProviderKind::Claude,
            home: self.0["home"].as_str().unwrap_or_default().into(),
            claude_options: self
                .0
                .get("claudeOptions")
                .filter(|v| !v.is_null())
                .cloned(),
        });
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeAuthObservation {
    pub account_pin: ClaudeAccountPin,
    pub settings: PreparationSettings,
}

/// Identity captured by the auth timeout owner before opening its settlement transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeAttemptIdentity {
    pub attempt_id: String,
    pub epoch: i64,
}

impl ClaudeAuthObservation {
    pub fn checked(account_pin: ClaudeAccountPin, settings: PreparationSettings) -> Self {
        Self {
            account_pin,
            settings,
        }
    }
}

pub enum Completion<'a> {
    TerminalTurn {
        agent_id: &'a str,
        turn: &'a Value,
        known: bool,
        now: f64,
        limits: Option<&'a LimitsObservation>,
        claude_auth: Option<&'a ClaudeAuthObservation>,
    },
    ClaudeAuthTimeout {
        agent_id: &'a str,
        now: f64,
        observation: Option<&'a ClaudeAuthObservation>,
        attempt: Option<&'a ClaudeAttemptIdentity>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Staged {
    agent_id: String,
    epoch: i64,
    changed: bool,
    event_wake: bool,
    effects: Vec<Effect>,
    account_wakes: Vec<(String, i64)>,
}

impl Staged {
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub fn epoch(&self) -> i64 {
        self.epoch
    }

    pub fn changed(&self) -> bool {
        self.changed
    }

    pub fn effects(&self) -> &[Effect] {
        &self.effects
    }
}

/// The integrator maps this to the account worker's existing bounded wake lane
/// and, for queued events, the shared HTTP event wake handle.
pub trait WakePort: Send + Sync {
    fn wake_account(&self, agent_id: &str, epoch: i64) -> Result<()>;
    fn wake_events(&self, agent_id: &str, epoch: i64) -> Result<()>;
}

struct RuntimeCapacityAdmission;

impl continuation_driver::CapacityAdmission for RuntimeCapacityAdmission {
    fn check(&mut self, tx: &Transaction<'_>, snapshot: &Snapshot, _: &Value) -> Result<()> {
        let agent = &snapshot.agent;
        let agent_id = text(agent, "id");
        let root_id = text(agent, "rootId");
        let root = if agent_id == root_id {
            agent
        } else {
            snapshot
                .ancestors
                .iter()
                .find(|ancestor| text(ancestor, "id") == root_id)
                .ok_or_else(|| ContinuationError::Invalid("/agent/rootId".into()))?
        };
        if present(agent, "deletedAt")
            || !flag(agent, "autoWake")
            || flag(agent, "inFlight")
            || present(agent, "turnId")
            || !matches!(text(agent, "status"), "failed" | "queued")
            || !snapshot
                .record
                .as_ref()
                .is_some_and(|record| snapshot.claims_valid(record))
        {
            return Err(ContinuationError::Unavailable(
                "turn-driver/capacity-state-changed",
            ));
        }
        if root
            .get("excludedAccountKeys")
            .and_then(Value::as_array)
            .is_some_and(|keys| keys.iter().any(|key| key.as_str() == Some(account(agent))))
        {
            return Err(ContinuationError::Unavailable(
                "turn-driver/capacity-account-excluded",
            ));
        }
        if agent_id != root_id && !snapshot.parents_running(false) {
            return Err(ContinuationError::Unavailable(
                "turn-driver/capacity-parent-not-running",
            ));
        }
        let pending: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_requests \
                 WHERE json_extract(record,'$.agent')=?1 \
                 AND json_extract(record,'$.status') IN ('pending','answering','uncertain'))",
                [agent_id],
                |row| row.get(0),
            )
            .map_err(studio_runtime_store::StoreError::from)?;
        if pending {
            return Err(ContinuationError::Unavailable(
                "turn-driver/capacity-request-pending",
            ));
        }
        let token_budget = root.get("tokenBudget").and_then(Value::as_u64);
        if let Some(token_budget) = token_budget {
            let used: i64 = tx
                .query_row(
                    "SELECT COALESCE(SUM(CAST(COALESCE(json_extract(record,'$.tokensUsed'),0) AS INTEGER)),0) \
                     FROM runtime_agents WHERE json_extract(record,'$.rootId')=?1 OR id=?1",
                    [root_id],
                    |row| row.get(0),
                )
                .map_err(studio_runtime_store::StoreError::from)?;
            if used >= token_budget.min(i64::MAX as u64) as i64 {
                return Err(ContinuationError::Unavailable(
                    "turn-driver/capacity-token-budget",
                ));
            }
        }
        let active_count =
            studio_runtime_store::AgentIndexedReads::global_slots_in_use(tx, agent_id)?;
        let global_limit = std::env::var("CODEX_CANVAS_CONCURRENCY")
            .ok()
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(studio_runtime_schema::agent_limits_generated::DEFAULT_GLOBAL_CONCURRENCY)
            .max(studio_runtime_schema::agent_limits_generated::MIN_GLOBAL_CONCURRENCY);
        if active_count >= global_limit {
            return Err(ContinuationError::Unavailable(
                "turn-driver/capacity-global-slots",
            ));
        }
        if agent_id != root_id {
            let team_active =
                studio_runtime_store::AgentIndexedReads::team_slots_in_use(tx, agent_id, root_id)?;
            let team_limit = root.get("concurrency").and_then(Value::as_i64).unwrap_or(0);
            if team_active >= team_limit {
                return Err(ContinuationError::Unavailable(
                    "turn-driver/capacity-team-slots",
                ));
            }
        }
        Ok(())
    }
}

/// Claim an explicit or automatic retry only after its durable identity and
/// scheduler admission gates pass in the same transaction as the attempt write.
pub fn stage_capacity_retry(
    tx: &Transaction<'_>,
    agent_id: &str,
    retry_id: &str,
    now: f64,
    automatic: bool,
) -> Result<Staged> {
    let mut snapshot = load(tx, agent_id, ContinuationTable::Capacity, Some(retry_id))?;
    if let Some(pending) = snapshot
        .agent
        .get("pendingSettings")
        .filter(|value| value.is_object())
        .cloned()
    {
        if snapshot
            .agent
            .get("pendingSettingsAccountKey")
            .and_then(Value::as_str)
            .unwrap_or_else(|| account(&snapshot.agent))
            != account(&snapshot.agent)
        {
            return Err(ContinuationError::Unavailable(
                "turn-driver/capacity-pending-settings-account-changed",
            ));
        }
        for (key, value) in pending
            .as_object()
            .ok_or_else(|| ContinuationError::Invalid("/agent/pendingSettings".into()))?
        {
            snapshot.agent[key] = value.clone();
        }
        let agent = snapshot
            .agent
            .as_object_mut()
            .ok_or_else(|| ContinuationError::Invalid("/agent".into()))?;
        agent.remove("pendingSettings");
        agent.remove("pendingSettingsAccountKey");
    }
    let settings = PreparationSettings::from_agent(&snapshot.agent)?.value();
    if let Some(record) = snapshot.record.as_mut()
        && snapshot.agent.get("pendingSettings").is_none()
    {
        record["settings"] = settings.clone();
    }
    let exclusion = capacity_exclusion_reason(&snapshot);
    let decision = match exclusion {
        Some(reason) if automatic => {
            capacity_retry::transition(&snapshot, CapacityAction::Excluded(&reason), now)?
        }
        Some(_) => {
            return Err(ContinuationError::Unavailable(
                "turn-driver/capacity-account-excluded",
            ));
        }
        None => capacity_retry::transition(
            &snapshot,
            CapacityAction::Claim {
                settings: &settings,
                automatic,
            },
            now,
        )?,
    };
    let mut fault = continuation_driver::NoFault;
    let mut admission = RuntimeCapacityAdmission;
    let mut hooks = continuation_driver::DriverHooks {
        fault: &mut fault,
        capacity_admission: &mut admission,
    };
    let changed = decision
        .agent
        .as_ref()
        .is_some_and(|agent| agent != &snapshot.agent)
        || decision
            .record
            .as_ref()
            .is_some_and(|record| snapshot.record.as_ref() != Some(record));
    let mut staged = empty_stage(tx, agent_id)?;
    if changed {
        let applied = continuation_driver::apply_in_transaction_with_hooks(
            tx,
            &snapshot,
            ContinuationTable::Capacity,
            &decision,
            now,
            &mut hooks,
        );
        match applied {
            Err(ContinuationError::Unavailable(reason)) => {
                let deferred =
                    capacity_retry::transition(&snapshot, CapacityAction::Wait(reason), now)?;
                let mut staged = empty_stage(tx, agent_id)?;
                collect_apply(
                    tx,
                    &mut staged,
                    ContinuationTable::Capacity,
                    &snapshot,
                    deferred,
                    now,
                )?;
                return Ok(staged);
            }
            Err(error) => return Err(error),
            Ok(()) => {}
        }
        staged.changed = true;
        add_account_wake(
            &mut staged,
            agent_id,
            snapshot.agent["epoch"].as_i64().unwrap_or(-1),
        );
        staged.effects.extend(decision.effects);
    }
    Ok(staged)
}

/// Cancel a capacity retry and return its committed projection in one writer
/// transaction so a replay cannot observe a later concurrent mutation.
pub fn stage_capacity_cancel(
    tx: &Transaction<'_>,
    agent_id: &str,
    retry_id: &str,
    now: f64,
) -> Result<Value> {
    let snapshot = load(tx, agent_id, ContinuationTable::Capacity, Some(retry_id))?;
    let decision = capacity_retry::transition(&snapshot, CapacityAction::Cancel, now)?;
    let value = decision
        .record
        .clone()
        .or_else(|| snapshot.record.clone())
        .ok_or_else(|| ContinuationError::Invalid("/capacity/record".into()))?;
    continuation_driver::apply_in_transaction(
        tx,
        &snapshot,
        ContinuationTable::Capacity,
        &decision,
        now,
    )?;
    Ok(value)
}

fn capacity_exclusion_reason(snapshot: &Snapshot) -> Option<String> {
    let agent = &snapshot.agent;
    let root_id = text(agent, "rootId");
    let root = if text(agent, "id") == root_id {
        agent
    } else {
        snapshot
            .ancestors
            .iter()
            .find(|ancestor| text(ancestor, "id") == root_id)?
    };
    let account_key = account(agent);
    root.get("excludedAccountKeys")
        .and_then(Value::as_array)
        .filter(|keys| keys.iter().any(|key| key.as_str() == Some(account_key)))
        .map(|_| format!("Account {account_key} is disabled for this chat"))
}

/// Persist acceptance of the exact capacity attempt in the same transaction as
/// the provider's positive turn/start receipt.
pub fn stage_capacity_accepted(
    tx: &Transaction<'_>,
    agent_id: &str,
    retry_id: &str,
    attempt: &Value,
    turn_id: &str,
    now: f64,
) -> Result<Staged> {
    let snapshot = load(tx, agent_id, ContinuationTable::Capacity, Some(retry_id))?;
    let decision = capacity_retry::transition(
        &snapshot,
        CapacityAction::Accepted { attempt, turn_id },
        now,
    )?;
    let mut staged = empty_stage(tx, agent_id)?;
    collect_apply(
        tx,
        &mut staged,
        ContinuationTable::Capacity,
        &snapshot,
        decision,
        now,
    )?;
    Ok(staged)
}

/// Stage all durable continuation state in the transaction that settles the
/// completion. A missing observation aborts that transaction with a named port.
pub fn stage(tx: &Transaction<'_>, completion: Completion<'_>) -> Result<Staged> {
    match completion {
        Completion::TerminalTurn {
            agent_id,
            turn,
            known,
            now,
            limits,
            claude_auth,
        } => stage_terminal(tx, agent_id, turn, known, now, limits, claude_auth),
        Completion::ClaudeAuthTimeout {
            agent_id,
            now,
            observation,
            attempt,
        } => stage_auth_timeout(tx, agent_id, now, observation, attempt),
    }
}

fn stage_terminal(
    tx: &Transaction<'_>,
    agent_id: &str,
    turn: &Value,
    known: bool,
    now: f64,
    limits: Option<&LimitsObservation>,
    claude_auth: Option<&ClaudeAuthObservation>,
) -> Result<Staged> {
    let mut staged = empty_stage(tx, agent_id)?;
    if !known || turn.get("id").and_then(Value::as_str).is_none() {
        return Ok(staged);
    }
    let turn_id = text(turn, "id");
    let status = text(turn, "status");
    // Check every required observation before the first write. The enclosing
    // settlement transaction can therefore handle a dependency without a
    // partially staged continuation.
    if status == "failed" {
        let error = turn
            .get("error")
            .filter(|value| !value.is_null())
            .unwrap_or(&Value::Null);
        let snapshot = load(tx, agent_id, ContinuationTable::Usage, None)?;
        if usage_resume::failure_eligible(&snapshot)
            && usage_failure(error, &snapshot.agent).is_some()
        {
            let limits = limits.ok_or(ContinuationError::Unavailable(
                "usage-policy/limits-snapshot",
            ))?;
            if limits.account_key() != account(&snapshot.agent) {
                return Err(ContinuationError::Unavailable(
                    "usage-policy/limits-account-mismatch",
                ));
            }
            if usage_failure(error, &snapshot.agent) == Some(UsageCause::Auth)
                && text(&snapshot.agent, "provider") == "claude"
                && claude_auth.is_none()
            {
                return Err(ContinuationError::Unavailable("claude-auth/account-pin"));
            }
        }
    }
    if status == "completed" {
        let current = load(tx, agent_id, ContinuationTable::Usage, None)?;
        let account_key = account(&current.agent);
        let proof_at = now;
        let pending = auth_wait_ids(tx, account_key, 33)?;
        for (resume_id, resume_agent) in pending.iter().take(32) {
            let snapshot = load(tx, resume_agent, ContinuationTable::Usage, Some(resume_id))?;
            let decision =
                usage_resume::completed_proof(&snapshot, account(&snapshot.agent), proof_at)?;
            collect_apply(
                tx,
                &mut staged,
                ContinuationTable::Usage,
                &snapshot,
                decision,
                now,
            )?;
        }
        if pending.len() > 32 {
            // A durable cursor lets bounded timer ticks finish the account-wide proof fanout.
            let mut agent = load(tx, agent_id, ContinuationTable::Usage, None)?.agent;
            let pending_by_account = agent
                .as_object_mut()
                .ok_or_else(|| ContinuationError::Invalid("/agent".into()))?
                .entry("authResumeProofPending")
                .or_insert_with(|| json!({}));
            let pending_by_account = pending_by_account.as_object_mut().ok_or_else(|| {
                ContinuationError::Invalid("/agent/authResumeProofPending".into())
            })?;
            pending_by_account.insert(
                account_key.to_owned(),
                json!({"proofAt":proof_at,"afterId":pending[31].0}),
            );
            ContinuationRepository::put_agent(tx, &agent)?;
            staged.changed = true;
            let wake_epoch = staged.epoch;
            add_account_wake(&mut staged, agent_id, wake_epoch);
        }
        if let Some(resume_id) = child_id(tx, agent_id, "usageResume")? {
            let snapshot = load(tx, agent_id, ContinuationTable::Usage, Some(&resume_id))?;
            let decision = usage_resume::completed_proof(&snapshot, account(&snapshot.agent), now)?;
            collect_apply(
                tx,
                &mut staged,
                ContinuationTable::Usage,
                &snapshot,
                decision,
                now,
            )?;
        }
    }

    if let Some(retry_id) = child_id(tx, agent_id, "capacityRetry")? {
        let snapshot = load(tx, agent_id, ContinuationTable::Capacity, Some(&retry_id))?;
        let decision = capacity_retry::transition(
            &snapshot,
            CapacityAction::Completed {
                turn_id,
                completed: status == "completed",
            },
            now,
        )?;
        collect_apply(
            tx,
            &mut staged,
            ContinuationTable::Capacity,
            &snapshot,
            decision,
            now,
        )?;
    }

    if status != "failed" {
        return Ok(staged);
    }
    let error = turn
        .get("error")
        .filter(|value| !value.is_null())
        .unwrap_or(&Value::Null);
    if let Some(kind) = capacity_failure(error) {
        let snapshot = load(tx, agent_id, ContinuationTable::Capacity, None)?;
        let settings = PreparationSettings::from_agent(&snapshot.agent)?.value();
        let decision = capacity_retry::failed(&snapshot, kind, turn_id, &settings, now)?;
        if let Some(record) = decision.record.as_ref()
            && record_exists(
                tx,
                ContinuationTable::Capacity,
                agent_id,
                text(record, "id"),
            )?
        {
            return Ok(staged);
        }
        collect_apply(
            tx,
            &mut staged,
            ContinuationTable::Capacity,
            &snapshot,
            decision,
            now,
        )?;
    }
    let usage_snapshot = load(tx, agent_id, ContinuationTable::Usage, None)?;
    if usage_resume::failure_eligible(&usage_snapshot)
        && let Some(cause) = usage_failure(error, &usage_snapshot.agent)
    {
        let snapshot = usage_snapshot;
        let limits = limits.ok_or(ContinuationError::Unavailable(
            "usage-policy/limits-snapshot",
        ))?;
        if limits.account_key() != account(&snapshot.agent) {
            return Err(ContinuationError::Unavailable(
                "usage-policy/limits-account-mismatch",
            ));
        }
        if cause == UsageCause::Auth
            && text(&snapshot.agent, "provider") == "claude"
            && claude_auth.is_none()
        {
            return Err(ContinuationError::Unavailable("claude-auth/account-pin"));
        }
        if cause == UsageCause::Auth
            && let Some(observation) = claude_auth
        {
            // The provider rejected the credentials this turn used, so the cached
            // account status is stale: the next read probes once.
            observation.account_pin.invalidate_cached_auth();
        }
        let reset = error.get("resetsAt").and_then(Value::as_f64);
        let decision = usage_resume::record_failure(
            &snapshot,
            FailureObservation {
                turn_id,
                cause,
                error_reset: reset,
                auth_attempt: snapshot.agent["authResumeAttempt"].as_u64().unwrap_or(0),
                limits,
            },
            now,
        )?;
        if let Some(record) = decision.record.as_ref()
            && record_exists(tx, ContinuationTable::Usage, agent_id, text(record, "id"))?
        {
            return Ok(staged);
        }
        collect_apply(
            tx,
            &mut staged,
            ContinuationTable::Usage,
            &snapshot,
            decision,
            now,
        )?;
    }
    Ok(staged)
}

fn auth_wait_ids(
    tx: &Transaction<'_>,
    account_key: &str,
    limit: usize,
) -> Result<Vec<(String, String)>> {
    let mut statement = tx
        .prepare_cached(
            "SELECT id,agent FROM runtime_usage_resumes \
             WHERE json_extract(record,'$.status')='scheduled' \
               AND json_extract(record,'$.cause')='auth' \
               AND json_extract(record,'$.accountKey')=?1 \
               AND json_extract(record,'$.proofAt') IS NULL ORDER BY id LIMIT ?2",
        )
        .map_err(|error| ContinuationError::Store(studio_runtime_store::StoreError::from(error)))?;
    statement
        .query_map((account_key, limit as i64), |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .map_err(|error| ContinuationError::Store(studio_runtime_store::StoreError::from(error)))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| ContinuationError::Store(studio_runtime_store::StoreError::from(error)))
}

/// Reconcile one due usage wait under the caller's transaction. The host bounds
/// the scan to a fixed batch and fetches `limits` before opening this transaction.
pub fn stage_tick(
    tx: &Transaction<'_>,
    agent_id: &str,
    now: f64,
    limits: Option<&LimitsObservation>,
) -> Result<Staged> {
    let mut staged = empty_stage(tx, agent_id)?;
    let owner = load(tx, agent_id, ContinuationTable::Usage, None)?;
    if let Some(pending_by_account) = owner
        .agent
        .get("authResumeProofPending")
        .and_then(Value::as_object)
        .filter(|pending| !pending.is_empty())
    {
        // One account cursor per bounded tick keeps both cursor fanout and row work bounded.
        let (key, pending) = pending_by_account
            .iter()
            .next()
            .ok_or_else(|| ContinuationError::Invalid("/agent/authResumeProofPending".into()))?;
        let proof_at = pending["proofAt"].as_f64().ok_or_else(|| {
            ContinuationError::Invalid("/agent/authResumeProofPending/proofAt".into())
        })?;
        let after = pending["afterId"].as_str().ok_or_else(|| {
            ContinuationError::Invalid("/agent/authResumeProofPending/afterId".into())
        })?;
        for (id, resume_agent) in auth_wait_ids_after(tx, key, after, 32)? {
            let snapshot = load(tx, &resume_agent, ContinuationTable::Usage, Some(&id))?;
            let decision = usage_resume::completed_proof(&snapshot, key, proof_at)?;
            collect_apply(
                tx,
                &mut staged,
                ContinuationTable::Usage,
                &snapshot,
                decision,
                now,
            )?;
        }
        let remaining = auth_wait_ids_after(tx, key, after, 33)?;
        let mut agent = load(tx, agent_id, ContinuationTable::Usage, None)?.agent;
        if remaining.len() > 32 {
            agent["authResumeProofPending"][key]["afterId"] = json!(remaining[31].0);
        } else {
            let pending = agent["authResumeProofPending"]
                .as_object_mut()
                .ok_or_else(|| {
                    ContinuationError::Invalid("/agent/authResumeProofPending".into())
                })?;
            pending.remove(key);
            if pending.is_empty() {
                agent
                    .as_object_mut()
                    .ok_or_else(|| ContinuationError::Invalid("/agent".into()))?
                    .remove("authResumeProofPending");
            }
        }
        if agent != owner.agent {
            ContinuationRepository::put_agent(tx, &agent)?;
            staged.changed = true;
            let wake_epoch = staged.epoch;
            add_account_wake(&mut staged, agent_id, wake_epoch);
        }
    }
    // Capacity claims and admission remain owned by the account worker. A due
    // durable row is enough to re-signal its existing scheduler lane.
    if let Some(id) = child_id(tx, agent_id, "capacityRetry")? {
        let capacity = load(tx, agent_id, ContinuationTable::Capacity, Some(&id))?;
        if capacity.record.as_ref().is_some_and(|record| {
            text(record, "status") == "scheduled"
                && record
                    .get("dueAt")
                    .and_then(Value::as_f64)
                    .is_some_and(|due| due <= now)
        }) {
            let capacity_stage = stage_capacity_retry(tx, agent_id, &id, now, true)?;
            staged.changed |= capacity_stage.changed;
            staged.event_wake |= capacity_stage.event_wake;
            staged.effects.extend(capacity_stage.effects);
            for wake in capacity_stage.account_wakes {
                if !staged.account_wakes.contains(&wake) {
                    staged.account_wakes.push(wake);
                }
            }
        }
    }
    let Some(id) = child_id(tx, agent_id, "usageResume")? else {
        return Ok(staged);
    };
    let snapshot = load(tx, agent_id, ContinuationTable::Usage, Some(&id))?;
    if snapshot.record.as_ref().is_none_or(|record| {
        text(record, "status") != "scheduled"
            || record
                .get("dueAt")
                .and_then(Value::as_f64)
                .is_none_or(|due| due > now)
    }) {
        return Ok(staged);
    }
    let decision = match limits {
        Some(limits) => usage_resume::tick(&snapshot, limits, now)?,
        None => usage_resume::tick_without_observation(&snapshot, now)?,
    };
    collect_apply(
        tx,
        &mut staged,
        ContinuationTable::Usage,
        &snapshot,
        decision,
        now,
    )?;
    Ok(staged)
}

/// Reconcile scheduled usage waits when fresh account telemetry is persisted.
/// This mirrors Python's `usage_resume_limits_changed`: non-usage/auth waits
/// are ignored by the policy after the indexed account scan.
pub fn stage_limits_changed_for_account(
    tx: &Transaction<'_>,
    account_key: &str,
    raw_limits: &Value,
    now: f64,
) -> Result<bool> {
    if raw_limits
        .get("error")
        .is_some_and(|value| value.as_str().is_some_and(|text| !text.is_empty()))
        || raw_limits
            .get("data")
            .and_then(Value::as_object)
            .is_none_or(|data| data.is_empty())
    {
        return Ok(false);
    }
    let mut observation = raw_limits.clone();
    observation["accountKey"] = json!(account_key);
    let limits = LimitsObservation::from_record(observation, None)?;
    let rows = tx
        .prepare_cached(
            "SELECT id,agent FROM runtime_usage_resumes WHERE json_extract(record,'$.status')='scheduled' AND json_extract(record,'$.accountKey')=?1 ORDER BY id",
        )
        .map_err(|error| ContinuationError::Store(studio_runtime_store::StoreError::from(error)))?
        .query_map([account_key], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| ContinuationError::Store(studio_runtime_store::StoreError::from(error)))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| ContinuationError::Store(studio_runtime_store::StoreError::from(error)))?;
    let Some((first_id, first_agent)) = rows.first().cloned() else {
        return Ok(false);
    };
    let first = load(tx, &first_agent, ContinuationTable::Usage, Some(&first_id))?;
    let mut staged = Staged {
        agent_id: first_agent.clone(),
        epoch: first.agent["epoch"]
            .as_i64()
            .ok_or_else(|| ContinuationError::Invalid("/agent/epoch".into()))?,
        changed: false,
        event_wake: false,
        effects: Vec::new(),
        account_wakes: Vec::new(),
    };
    for (id, agent_id) in rows {
        let snapshot = if id == first_id && agent_id == first_agent {
            first.clone()
        } else {
            load(tx, &agent_id, ContinuationTable::Usage, Some(&id))?
        };
        let decision = usage_resume::limits_changed(&snapshot, &limits, now)?;
        collect_apply(
            tx,
            &mut staged,
            ContinuationTable::Usage,
            &snapshot,
            decision,
            now,
        )?;
    }
    Ok(staged.changed)
}

fn auth_wait_ids_after(
    tx: &Transaction<'_>,
    account_key: &str,
    after_id: &str,
    limit: usize,
) -> Result<Vec<(String, String)>> {
    let mut statement = tx.prepare_cached("SELECT id,agent FROM runtime_usage_resumes WHERE json_extract(record,'$.status')='scheduled' AND json_extract(record,'$.cause')='auth' AND json_extract(record,'$.accountKey')=?1 AND json_extract(record,'$.proofAt') IS NULL AND id>?2 ORDER BY id LIMIT ?3")
        .map_err(|error| ContinuationError::Store(studio_runtime_store::StoreError::from(error)))?;
    statement
        .query_map((account_key, after_id, limit as i64), |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .map_err(|error| ContinuationError::Store(studio_runtime_store::StoreError::from(error)))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| ContinuationError::Store(studio_runtime_store::StoreError::from(error)))
}

fn stage_auth_timeout(
    tx: &Transaction<'_>,
    agent_id: &str,
    now: f64,
    observation: Option<&ClaudeAuthObservation>,
    attempt: Option<&ClaudeAttemptIdentity>,
) -> Result<Staged> {
    let attempt = attempt.ok_or(ContinuationError::Unavailable(
        "claude-auth/attempt-identity",
    ))?;
    let snapshot = load(tx, agent_id, ContinuationTable::Usage, None)?;
    if snapshot.agent["epoch"].as_i64() != Some(attempt.epoch)
        || snapshot.agent["startAttempt"]["id"].as_str() != Some(&attempt.attempt_id)
    {
        return empty_stage(tx, agent_id);
    }
    let observation =
        observation.ok_or(ContinuationError::Unavailable("claude-auth/account-pin"))?;
    let settings = observation.settings.value();
    let pin = observation.account_pin.value();
    let Some(capture) = claude_auth_wait::capture(&snapshot, &settings, pin) else {
        return empty_stage(tx, agent_id);
    };
    let decision = claude_auth_wait::record_timeout(&snapshot, &capture, &settings, pin, now)?;
    let mut staged = empty_stage(tx, agent_id)?;
    collect_apply(
        tx,
        &mut staged,
        ContinuationTable::Usage,
        &snapshot,
        decision,
        now,
    )?;
    Ok(staged)
}

fn empty_stage(tx: &Transaction<'_>, agent_id: &str) -> Result<Staged> {
    let raw = ContinuationRepository::snapshot(tx, agent_id, ContinuationTable::Usage, None)?;
    let snapshot = Snapshot::from_records(raw)?;
    Ok(Staged {
        agent_id: agent_id.into(),
        epoch: snapshot.agent["epoch"]
            .as_i64()
            .ok_or_else(|| ContinuationError::Invalid("/agent/epoch".into()))?,
        changed: false,
        event_wake: false,
        effects: Vec::new(),
        account_wakes: Vec::new(),
    })
}

fn load(
    tx: &Transaction<'_>,
    agent_id: &str,
    table: ContinuationTable,
    record_id: Option<&str>,
) -> Result<Snapshot> {
    Snapshot::from_records(ContinuationRepository::snapshot(
        tx, agent_id, table, record_id,
    )?)
}

fn child_id(tx: &Transaction<'_>, agent_id: &str, field: &str) -> Result<Option<String>> {
    let snapshot = load(tx, agent_id, ContinuationTable::Usage, None)?;
    Ok(snapshot.agent[field]
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned))
}

fn record_exists(
    tx: &Transaction<'_>,
    table: ContinuationTable,
    agent_id: &str,
    id: &str,
) -> Result<bool> {
    Ok(tx
        .query_row(
            &format!("SELECT 1 FROM {} WHERE id=?1 AND agent=?2", table.name()),
            [id, agent_id],
            |_| Ok(()),
        )
        .optional()
        .map_err(studio_runtime_store::StoreError::from)?
        .is_some())
}

fn collect_apply(
    tx: &Transaction<'_>,
    staged: &mut Staged,
    table: ContinuationTable,
    snapshot: &Snapshot,
    decision: Decision,
    now: f64,
) -> Result<()> {
    let changed = decision
        .agent
        .as_ref()
        .is_some_and(|agent| agent != &snapshot.agent)
        || decision
            .record
            .as_ref()
            .is_some_and(|record| snapshot.record.as_ref() != Some(record))
        || !decision.events.is_empty()
        || !decision.metadata.is_empty()
        || !decision.effects.is_empty();
    if !changed {
        return Ok(());
    }
    continuation_driver::apply_in_transaction(tx, snapshot, table, &decision, now)?;
    staged.changed = true;
    add_account_wake(
        staged,
        snapshot.agent_id(),
        snapshot.agent["epoch"].as_i64().unwrap_or(-1),
    );
    staged.event_wake |= decision
        .effects
        .iter()
        .any(|effect| matches!(effect, Effect::Followup { .. }));
    staged.effects.extend(decision.effects);
    Ok(())
}

fn add_account_wake(staged: &mut Staged, agent_id: &str, epoch: i64) {
    let wake = (agent_id.to_owned(), epoch);
    if !staged.account_wakes.contains(&wake) {
        staged.account_wakes.push(wake);
    }
}

/// The only post-commit action: signal the existing scheduler owner.
pub fn committed(staged: &Staged, port: &dyn WakePort) -> Result<()> {
    let mut failures = Vec::new();
    if staged.changed {
        if staged.account_wakes.is_empty() {
            if let Err(error) = port.wake_account(&staged.agent_id, staged.epoch) {
                failures.push(format!("account {}: {error}", staged.agent_id));
            }
        } else {
            for (agent_id, epoch) in &staged.account_wakes {
                if let Err(error) = port.wake_account(agent_id, *epoch) {
                    failures.push(format!("account {agent_id}: {error}"));
                }
            }
        }
        if staged.event_wake
            && let Err(error) = port.wake_events(&staged.agent_id, staged.epoch)
        {
            failures.push(format!("events {}: {error}", staged.agent_id));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(ContinuationError::WakeFailures(failures))
    }
}

fn capacity_failure(error: &Value) -> Option<CapacityFailure> {
    match error_kind(error) {
        "serverOverloaded" => Some(CapacityFailure::ServerOverloaded),
        "internalServerError" => Some(CapacityFailure::InternalServerError),
        "httpConnectionFailed" => Some(CapacityFailure::HttpConnectionFailed),
        "responseStreamConnectionFailed" => Some(CapacityFailure::ResponseStreamConnectionFailed),
        "responseStreamDisconnected" => Some(CapacityFailure::ResponseStreamDisconnected),
        "responseTooManyFailedAttempts" => Some(CapacityFailure::ResponseTooManyFailedAttempts),
        _ => None,
    }
}

/// Shared usage-failure classifier. `provider_auth` is the provider adapter's
/// extra auth-message pattern; callers go through `ProviderAdapter::classify_error`.
pub(crate) fn classify_usage_failure(
    error: &Value,
    provider_auth: Option<&regex::Regex>,
) -> Option<UsageCause> {
    match error_kind(error) {
        "usageLimitExceeded" => Some(UsageCause::UsageLimit),
        "rateLimitExceeded" => Some(UsageCause::RateLimit),
        kind if kind.to_ascii_lowercase().contains("auth") => Some(UsageCause::Auth),
        _ => {
            let message = error.get("message").and_then(Value::as_str).unwrap_or("");
            let patterns = classifier_patterns();
            let auth_evidence = patterns.0.is_match(message);
            (patterns.1.is_match(message)
                || (patterns.2.is_match(message) && auth_evidence)
                || provider_auth.is_some_and(|pattern| pattern.is_match(message)))
            .then_some(UsageCause::Auth)
        }
    }
}

fn classifier_patterns() -> &'static (regex::Regex, regex::Regex, regex::Regex) {
    static PATTERNS: std::sync::OnceLock<(regex::Regex, regex::Regex, regex::Regex)> =
        std::sync::OnceLock::new();
    PATTERNS.get_or_init(|| (
        regex::Regex::new(r"(?i)\b(?:invalid|expired|revoked).{0,32}\b(?:token|key)\b|\b(?:token|key)\b.{0,32}\b(?:invalid|expired|revoked)\b|\bsign[ -]?in\b|\bauthentication required\b|\bunauthorized\b").expect("static auth evidence pattern"),
        regex::Regex::new(r"(?i)\b401\b").expect("static 401 pattern"),
        regex::Regex::new(r"(?i)\b403\b").expect("static 403 pattern"),
    ))
}

/// Classify a turn error with the agent's provider adapter; an unknown provider
/// never classifies.
fn usage_failure(error: &Value, agent: &Value) -> Option<UsageCause> {
    studio_runtime_providers::ProviderKind::of_record(agent)
        .ok()
        .and_then(|kind| crate::provider_adapter::adapter(kind).classify_error(error))
}

fn error_kind(error: &Value) -> &str {
    error["codexErrorInfo"]
        .as_str()
        .or_else(|| {
            error["codexErrorInfo"]
                .as_object()
                .and_then(|value| value.keys().next().map(String::as_str))
        })
        .unwrap_or("")
}
