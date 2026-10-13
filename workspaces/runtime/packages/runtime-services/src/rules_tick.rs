//! Effect adapter for rules. The host calls bounded batches from its timer, event
//! delivery and monitor completion paths. Files and command ports run outside transactions.

use std::path::Path;

use studio_runtime_store::rules_tick::{RULE_BATCH, RulesTickRepository};
use studio_runtime_store::{
    AgentRecord, RuleRecord, RulesRepository, RuntimeStore, StoreError, Transaction,
};
use thiserror::Error;

use crate::event_write::{EventWriteError, enqueue_recovery_event};
use crate::rules_engine::{
    CheckResult, RuleCheck, RuleDecision, RuleError, RuleSnapshot, TickObservation, active_workers,
};

#[derive(Debug, Error)]
pub enum RulesTickError {
    #[error(transparent)]
    Rule(#[from] RuleError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Event(#[from] EventWriteError),
    #[error("Not implemented: requires monitor-driver/rule-checks")]
    MonitorUnavailable,
    #[error("rule file metadata is unavailable")]
    File,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CheckReconciliation {
    /// Authoritative durable proof that this identity has never reached transport.
    NotSubmitted,
    /// An exact durable monitor owns this identity (and its eventual completion).
    Submitted,
    /// No proof either way. Hold visibly; never infer absence from a lost response.
    Unknown,
}

pub enum CheckSubmission {
    Accepted,
    NotSubmitted { error: String },
    Unknown,
}

/// Implemented by the monitor driver after preparation/approval and native session
/// admission are installed. It MUST revalidate owner and check identity before I/O.
/// Launch MUST be idempotent for the exact monitor identity and reserve its durable
/// operation before transport I/O. A duplicate reservation must not send again.
/// Reconciliation includes native ticket receipts, not only monitor-row existence.
pub trait RuleMonitor {
    fn available(&self) -> bool;
    fn launch(&self, check: &RuleCheck) -> CheckSubmission;
    fn reconcile(&self, _check: &RuleCheck) -> CheckReconciliation {
        CheckReconciliation::Unknown
    }
    fn cancel(&self, monitor_id: &str) -> Result<(), RulesTickError>;
}

pub struct UnavailableMonitor;
impl RuleMonitor for UnavailableMonitor {
    fn available(&self) -> bool {
        false
    }
    fn launch(&self, _check: &RuleCheck) -> CheckSubmission {
        CheckSubmission::Unknown
    }
    fn cancel(&self, _monitor_id: &str) -> Result<(), RulesTickError> {
        Err(RulesTickError::MonitorUnavailable)
    }
}

pub trait RuleFiles {
    fn fingerprint(&self, path: &str) -> Result<Option<(i64, u64, u64)>, RulesTickError>;
}

pub struct LocalRuleFiles;
impl RuleFiles for LocalRuleFiles {
    fn fingerprint(&self, path: &str) -> Result<Option<(i64, u64, u64)>, RulesTickError> {
        match std::fs::metadata(Path::new(path)) {
            #[cfg(unix)]
            Ok(metadata) => {
                use std::os::unix::fs::MetadataExt;
                let nanos = metadata
                    .mtime()
                    .checked_mul(1_000_000_000)
                    .and_then(|seconds| seconds.checked_add(metadata.mtime_nsec()))
                    .ok_or(RulesTickError::File)?;
                Ok(Some((nanos, metadata.len(), metadata.ino())))
            }
            #[cfg(not(unix))]
            Ok(_) => Err(RulesTickError::File),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(RulesTickError::File),
        }
    }
}

#[derive(Default, Debug)]
pub struct TickReport {
    /// Continue from this row on the next batch; None means the scan is complete.
    pub next_rowid: Option<i64>,
    pub changed: usize,
    pub launched: usize,
}

/// A timer invokes batches until next_rowid is None, then starts its next scan at 0.
/// Sampling is optimistic: an edit or owner-permission change discards its old sample.
pub fn tick_batch(
    store: &RuntimeStore,
    after_rowid: i64,
    now: f64,
    files: &impl RuleFiles,
    monitor: &impl RuleMonitor,
) -> Result<TickReport, RulesTickError> {
    let rows = RulesTickRepository::page(&store.read()?, after_rowid)?;
    let mut report = TickReport {
        next_rowid: (rows.len() == RULE_BATCH)
            .then(|| rows.last().map(|row| row.rowid))
            .flatten(),
        ..TickReport::default()
    };
    for row in rows {
        let sampled = load_snapshot(&store.read()?, row.rule)?;
        let fingerprint = sampled
            .fingerprint_path(now)
            .map(|path| files.fingerprint(path))
            .transpose()?;
        let check = store.transaction_with::<_, RulesTickError>(|transaction| {
            let Some(rule) = RulesRepository::get(transaction, sampled.id())? else {
                return Ok(None);
            };
            let owner = owner(transaction, sampled.owner_id())?;
            if !sampled.current(rule.value(), &owner.value()) {
                return Ok(None);
            }
            let activity = if sampled.is_low_workers() {
                let raw = RulesTickRepository::activity(transaction, sampled.owner_id())?;
                Some(active_workers(&raw.agents, &raw.monitors)?)
            } else {
                None
            };
            let decision = sampled.tick(
                now,
                TickObservation {
                    fingerprint,
                    active_workers: activity,
                },
            )?;
            if decision.check.is_some() && !monitor.available() {
                return Err(RulesTickError::MonitorUnavailable);
            }
            if apply(transaction, &owner, &decision, now)? {
                report.changed += 1;
            }
            Ok(decision.check)
        })?;
        if let Some(check) = check
            && submit_check(store, &check, now, monitor)?
        {
            report.launched += 1;
        }
    }
    Ok(report)
}

/// Run before enabling ticks. Reconcile durable intents outside the writer, then
/// revalidate their exact reservation inside it. Legacy checks remain unknown.
pub fn recover_batch(
    store: &RuntimeStore,
    after_rowid: i64,
    now: f64,
    monitor: &impl RuleMonitor,
) -> Result<TickReport, RulesTickError> {
    let rows = RulesTickRepository::page(&store.read()?, after_rowid)?;
    let mut report = TickReport {
        next_rowid: (rows.len() == RULE_BATCH)
            .then(|| rows.last().map(|row| row.rowid))
            .flatten(),
        ..TickReport::default()
    };
    for row in rows {
        let sampled = load_snapshot(&store.read()?, row.rule)?;
        let intent = launch_intent(&sampled)?;
        let state = match &intent {
            Some(check) if sampled.launch_allowed(check) && monitor.available() => {
                monitor.reconcile(check)
            }
            _ => CheckReconciliation::Unknown,
        };
        let check = store.transaction_with::<_, RulesTickError>(|tx| {
            let Some(rule) = RulesRepository::get(tx, sampled.id())? else {
                return Ok(None);
            };
            let owner = owner(tx, sampled.owner_id())?;
            if !sampled.current(rule.value(), &owner.value()) {
                return Ok(None);
            }
            if let Some(check) = &intent
                && sampled.launch_allowed(check)
            {
                match state {
                    CheckReconciliation::NotSubmitted => return Ok(Some(check.clone())),
                    CheckReconciliation::Submitted => return Ok(None),
                    CheckReconciliation::Unknown => {}
                }
            }
            report.changed += usize::from(apply(tx, &owner, &sampled.recover(now)?, now)?);
            Ok(None)
        })?;
        if let Some(check) = check
            && submit_check(store, &check, now, monitor)?
        {
            report.launched += 1;
        }
    }
    Ok(report)
}

/// Timer, recovery and event callers use this only AFTER their reservation commits.
/// The monitor port closes the final ownership race and deduplicates by monitor ID.
pub fn submit_check(
    store: &RuntimeStore,
    check: &RuleCheck,
    now: f64,
    monitor: &impl RuleMonitor,
) -> Result<bool, RulesTickError> {
    if !monitor.available() {
        return Err(RulesTickError::MonitorUnavailable);
    }
    let current = store.transaction_with::<_, RulesTickError>(|tx| {
        let Some(rule) = RulesRepository::get(tx, &check.rule_id)? else {
            return Ok(false);
        };
        let snapshot = load_snapshot(tx, rule)?;
        Ok(snapshot.launch_allowed(check) && launch_intent(&snapshot)?.as_ref() == Some(check))
    })?;
    if !current {
        return Ok(false);
    }
    match monitor.launch(check) {
        CheckSubmission::Accepted => {}
        CheckSubmission::NotSubmitted { error } => {
            finish(
                store,
                check,
                CheckResult {
                    code: None,
                    error: Some(&error),
                    output: "",
                },
                now,
            )?;
        }
        CheckSubmission::Unknown => {
            store.transaction_with::<_, RulesTickError>(|tx| {
                let Some(rule) = RulesRepository::get(tx, &check.rule_id)? else {
                    return Ok(());
                };
                let snapshot = load_snapshot(tx, rule)?;
                if snapshot.launch_allowed(check) {
                    let mut decision = snapshot.recover(now)?;
                    if let Some(record) = &mut decision.record {
                        record["error"] = serde_json::json!(
                            "Monitor submission outcome unknown; check was not repeated."
                        );
                    }
                    apply(tx, &owner(tx, snapshot.owner_id())?, &decision, now)?;
                }
                Ok(())
            })?;
        }
    }
    Ok(true)
}

fn launch_intent(snapshot: &RuleSnapshot) -> Result<Option<RuleCheck>, RulesTickError> {
    let Some(value) = snapshot
        .record()
        .get("launchIntent")
        .filter(|value| !value.is_null())
    else {
        return Ok(None);
    };
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|_| {
            RuleError::Invalid {
                object: format!("rules/{}", snapshot.id()),
                location: "/launchIntent".into(),
            }
            .into()
        })
}

pub fn finish(
    store: &RuntimeStore,
    check: &RuleCheck,
    result: CheckResult<'_>,
    now: f64,
) -> Result<bool, RulesTickError> {
    store.transaction_with(|transaction| finish_in_transaction(transaction, check, result, now))
}

/// Monitor completion calls this inside its own transaction before acknowledging a receipt.
/// Transport tickets/connection generations belong to the monitor driver; this seam binds
/// the callback to the durable rule, check, owner epoch/account/thread, command and monitor.
pub fn finish_in_transaction(
    transaction: &Transaction<'_>,
    check: &RuleCheck,
    result: CheckResult<'_>,
    now: f64,
) -> Result<bool, RulesTickError> {
    let Some(rule) = RulesRepository::get(transaction, &check.rule_id)? else {
        return Ok(false);
    };
    let snapshot = load_snapshot(transaction, rule)?;
    let mut decision = snapshot.finish_check(check, result, now)?;
    if let Some(record) = decision
        .record
        .as_mut()
        .and_then(serde_json::Value::as_object_mut)
    {
        record.remove("launchIntent");
    }
    apply(
        transaction,
        &owner(transaction, snapshot.owner_id())?,
        &decision,
        now,
    )
}

/// Same-agent event hook. Commit, then pass each returned intent to submit_check.
/// Rule wake events must not re-enter this hook. Batch cursors allow bounded fan-out.
pub struct RuleEvent<'a> {
    pub agent_id: &'a str,
    pub kind: &'a str,
    pub text: &'a str,
    pub key: &'a str,
    pub now: f64,
}

pub fn event_in_transaction(
    transaction: &Transaction<'_>,
    event: RuleEvent<'_>,
    after_rowid: i64,
    monitor_available: bool,
) -> Result<(TickReport, Vec<RuleCheck>), RulesTickError> {
    let agent = owner(transaction, event.agent_id)?;
    let rows = RulesTickRepository::page(transaction, after_rowid)?;
    let mut report = TickReport {
        next_rowid: (rows.len() == RULE_BATCH)
            .then(|| rows.last().map(|row| row.rowid))
            .flatten(),
        ..TickReport::default()
    };
    let mut checks = Vec::new();
    for row in rows {
        if row.rule.get("agent").and_then(serde_json::Value::as_str) != Some(agent.id().as_ref()) {
            continue;
        }
        let snapshot = RuleSnapshot::from_records(row.rule.value().clone(), agent.value())?;
        let decision = snapshot.event(event.kind, event.text, event.key, event.now)?;
        if decision.check.is_some() && !monitor_available {
            return Err(RulesTickError::MonitorUnavailable);
        }
        if apply(transaction, &agent, &decision, event.now)? {
            report.changed += 1;
        }
        if let Some(check) = decision.check {
            checks.push(check);
        }
    }
    Ok((report, checks))
}

fn load_snapshot(
    connection: &studio_runtime_store::Connection,
    rule: RuleRecord,
) -> Result<RuleSnapshot, RulesTickError> {
    let agent_id = rule
        .get("agent")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| StoreError::InvalidStoredRecord {
            object: "rules",
            id: rule.id().clone(),
        })?;
    Ok(RuleSnapshot::from_records(
        rule.value().clone(),
        owner(connection, agent_id)?.value(),
    )?)
}
fn owner(
    connection: &studio_runtime_store::Connection,
    id: &str,
) -> Result<AgentRecord, RulesTickError> {
    RulesRepository::agent(connection, id)?.ok_or_else(|| {
        StoreError::InvalidStoredRecord {
            object: "agents",
            id: id.to_owned(),
        }
        .into()
    })
}
fn apply(
    transaction: &Transaction<'_>,
    agent: &AgentRecord,
    decision: &RuleDecision,
    now: f64,
) -> Result<bool, RulesTickError> {
    let Some(record) = &decision.record else {
        return Ok(false);
    };
    // Validate before any write; RuntimeStore rolls back rule, wake and canonical views together.
    let mut record = record.clone();
    if let Some(check) = &decision.check {
        record["launchIntent"] = serde_json::to_value(check).map_err(|_| RuleError::Schema)?;
    }
    let record = RuleRecord::from_value(record).map_err(StoreError::from)?;
    RulesRepository::put(transaction, &record)?;
    if let Some(wake) = &decision.wake {
        enqueue_recovery_event(
            transaction,
            agent,
            wake.id.clone(),
            wake.kind,
            wake.text.clone(),
            now,
        )?;
    }
    Ok(true)
}
