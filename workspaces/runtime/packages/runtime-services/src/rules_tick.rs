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
    tick_can_change,
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
    NotSubmitted {
        error: String,
    },
    Unknown,
    /// The monitor port runs the launch in its own task and records the
    /// outcome there with [`settle_submission`]. The caller records nothing.
    Detached,
}

/// Rule file identity: modification time in Unix nanoseconds, size in bytes, inode number.
pub type RuleFileFingerprint = (i64, u64, u64);

/// Implemented by the monitor driver after preparation/approval and native session
/// admission are installed. It MUST revalidate owner and check identity before I/O.
/// Launch MUST be idempotent for the exact monitor identity and reserve its durable
/// operation before transport I/O. A duplicate reservation must not send again.
/// A launch that can wait for its command returns `Detached` and settles later, so
/// the monitor timer never waits for a rule command.
/// Reconciliation includes native ticket receipts, not only monitor-row existence.
pub trait RuleMonitor {
    fn available(&self) -> bool;
    fn launch<'a>(
        &'a self,
        check: &'a RuleCheck,
    ) -> futures_util::future::BoxFuture<'a, CheckSubmission>;
    fn reconcile<'a>(
        &'a self,
        _check: &'a RuleCheck,
    ) -> futures_util::future::BoxFuture<'a, CheckReconciliation> {
        Box::pin(async { CheckReconciliation::Unknown })
    }
    fn cancel<'a>(
        &'a self,
        monitor_id: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<(), RulesTickError>>;
}

pub struct UnavailableMonitor;
impl RuleMonitor for UnavailableMonitor {
    fn available(&self) -> bool {
        false
    }
    fn launch<'a>(
        &'a self,
        _check: &'a RuleCheck,
    ) -> futures_util::future::BoxFuture<'a, CheckSubmission> {
        Box::pin(async { CheckSubmission::Unknown })
    }
    fn cancel<'a>(
        &'a self,
        _monitor_id: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<(), RulesTickError>> {
        Box::pin(async { Err(RulesTickError::MonitorUnavailable) })
    }
}

pub trait RuleFiles {
    fn fingerprint<'a>(
        &'a self,
        path: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<Option<RuleFileFingerprint>, RulesTickError>>;
}

pub struct LocalRuleFiles;
impl RuleFiles for LocalRuleFiles {
    fn fingerprint<'a>(
        &'a self,
        path: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<Option<RuleFileFingerprint>, RulesTickError>>
    {
        Box::pin(async move {
            let path = path.to_owned();
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/rules_tick",
                move || fingerprint_path(&path),
            )
            .await
            .map_err(|_| RulesTickError::File)?
        })
    }
}

fn fingerprint_path(path: &str) -> Result<Option<RuleFileFingerprint>, RulesTickError> {
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

#[derive(Default, Debug)]
pub struct TickReport {
    /// Continue from this row on the next batch; None means the scan is complete.
    pub next_rowid: Option<i64>,
    pub changed: usize,
    pub launched: usize,
}

/// A timer invokes batches until next_rowid is None, then starts its next scan at 0.
/// Sampling is optimistic: an edit or owner-permission change discards its old sample.
pub async fn tick_batch(
    store: &RuntimeStore,
    after_rowid: i64,
    now: f64,
    files: &impl RuleFiles,
    monitor: &(impl RuleMonitor + ?Sized),
) -> Result<TickReport, RulesTickError> {
    let rows = store
        .read_async("runtime-services/rules_tick", move |tx| {
            RulesTickRepository::page(tx, after_rowid).map_err(RulesTickError::from)
        })
        .await?;
    let mut report = TickReport {
        next_rowid: (rows.len() == RULE_BATCH)
            .then(|| rows.last().map(|row| row.rowid))
            .flatten(),
        ..TickReport::default()
    };
    for row in rows {
        let rule = row.rule;
        // Most stored rules are paused or completed. Skip them before the owner
        // record, which can hold hundreds of kilobytes, is read and decoded.
        if !tick_can_change(rule.value())? {
            continue;
        }
        let sampled = store
            .read_async("runtime-services/rules_tick", move |tx| {
                load_snapshot(tx, rule)
            })
            .await?;
        let fingerprint = if let Some(path) = sampled.fingerprint_path(now) {
            files.fingerprint(path).await?
        } else {
            None
        };
        let available = monitor.available();
        // A rule with nothing to change is decided on a reader; only a real change
        // takes the writer lane, which re-decides inside its own transaction.
        let probe = sampled.clone();
        let busy = store
            .read_async("runtime-services/rules_tick", move |tx| {
                Ok::<_, RulesTickError>(
                    decide(tx, &probe, now, fingerprint, available)?
                        .is_some_and(|(_, decision)| decision.record.is_some()),
                )
            })
            .await?;
        if !busy {
            continue;
        }
        let Some((changed, check)) = store
            .transaction_with::<_, RulesTickError>(move |transaction| {
                let Some((owner, decision)) =
                    decide(transaction, &sampled, now, fingerprint, available)?
                else {
                    return Ok(None);
                };
                let changed = apply(transaction, &owner, &decision, now)?;
                Ok(Some((changed, decision.check)))
            })
            .await?
        else {
            continue;
        };
        report.changed += usize::from(changed);
        if let Some(check) = check
            && submit_check(store, &check, now, monitor).await?
        {
            report.launched += 1;
        }
    }
    Ok(report)
}

/// Run before enabling ticks. Reconcile durable intents outside the writer, then
/// revalidate their exact reservation inside it. Legacy checks remain unknown.
pub async fn recover_batch(
    store: &RuntimeStore,
    after_rowid: i64,
    now: f64,
    monitor: &(impl RuleMonitor + ?Sized),
) -> Result<TickReport, RulesTickError> {
    let rows = store
        .read_async("runtime-services/rules_tick", move |tx| {
            RulesTickRepository::page(tx, after_rowid).map_err(RulesTickError::from)
        })
        .await?;
    let mut report = TickReport {
        next_rowid: (rows.len() == RULE_BATCH)
            .then(|| rows.last().map(|row| row.rowid))
            .flatten(),
        ..TickReport::default()
    };
    for row in rows {
        let rule = row.rule;
        let sampled = store
            .read_async("runtime-services/rules_tick", move |tx| {
                load_snapshot(tx, rule)
            })
            .await?;
        let intent = launch_intent(&sampled)?;
        let state = match &intent {
            Some(check) if sampled.launch_allowed(check) && monitor.available() => {
                monitor.reconcile(check).await
            }
            _ => CheckReconciliation::Unknown,
        };
        let intent_for_tx = intent.clone();
        let Some((changed, check)) = store
            .transaction_with::<_, RulesTickError>(move |tx| {
                let Some(rule) = RulesRepository::get(tx, sampled.id())? else {
                    return Ok(None);
                };
                let owner = owner(tx, sampled.owner_id())?;
                if !sampled.current(rule.value(), &owner.value()) {
                    return Ok(None);
                }
                if let Some(check) = &intent_for_tx
                    && sampled.launch_allowed(check)
                {
                    match state {
                        CheckReconciliation::NotSubmitted => {
                            return Ok(Some((false, Some(check.clone()))));
                        }
                        CheckReconciliation::Submitted => return Ok(None),
                        CheckReconciliation::Unknown => {}
                    }
                }
                let changed = apply(tx, &owner, &sampled.recover(now)?, now)?;
                Ok(Some((changed, None)))
            })
            .await?
        else {
            continue;
        };
        report.changed += usize::from(changed);
        if let Some(check) = check
            && submit_check(store, &check, now, monitor).await?
        {
            report.launched += 1;
        }
    }
    Ok(report)
}

/// Timer, recovery and event callers use this only AFTER their reservation commits.
/// The monitor port closes the final ownership race and deduplicates by monitor ID.
pub async fn submit_check(
    store: &RuntimeStore,
    check: &RuleCheck,
    now: f64,
    monitor: &(impl RuleMonitor + ?Sized),
) -> Result<bool, RulesTickError> {
    if !monitor.available() {
        return Err(RulesTickError::MonitorUnavailable);
    }
    let check_for_tx = check.clone();
    let current = store
        .transaction_with::<_, RulesTickError>(move |tx| {
            let Some(rule) = RulesRepository::get(tx, &check_for_tx.rule_id)? else {
                return Ok(false);
            };
            let snapshot = load_snapshot(tx, rule)?;
            Ok(snapshot.launch_allowed(&check_for_tx)
                && launch_intent(&snapshot)?.as_ref() == Some(&check_for_tx))
        })
        .await?;
    if !current {
        return Ok(false);
    }
    let submission = monitor.launch(check).await;
    settle_submission(store, check, &submission, now).await?;
    Ok(true)
}

/// Record the outcome of one launch, once. A replay after a lost commit
/// response changes nothing: the first commit cleared the check's launch.
pub async fn settle_submission(
    store: &RuntimeStore,
    check: &RuleCheck,
    submission: &CheckSubmission,
    now: f64,
) -> Result<(), RulesTickError> {
    match submission {
        CheckSubmission::Accepted | CheckSubmission::Detached => {}
        CheckSubmission::NotSubmitted { error } => {
            finish(
                store,
                check,
                CheckResult {
                    code: None,
                    error: Some(error),
                    cancelled: false,
                    output: "",
                },
                now,
            )
            .await?;
        }
        CheckSubmission::Unknown => {
            let check_for_tx = check.clone();
            store
                .transaction_with::<_, RulesTickError>(move |tx| {
                    let Some(rule) = RulesRepository::get(tx, &check_for_tx.rule_id)? else {
                        return Ok(());
                    };
                    let snapshot = load_snapshot(tx, rule)?;
                    if snapshot.launch_allowed(&check_for_tx) {
                        let mut decision = snapshot.recover(now)?;
                        if let Some(record) = &mut decision.record {
                            record["error"] = serde_json::json!(
                                "Monitor submission outcome unknown; check was not repeated."
                            );
                        }
                        apply(tx, &owner(tx, snapshot.owner_id())?, &decision, now)?;
                    }
                    Ok(())
                })
                .await?;
        }
    }
    Ok(())
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

pub async fn finish(
    store: &RuntimeStore,
    check: &RuleCheck,
    result: CheckResult<'_>,
    now: f64,
) -> Result<bool, RulesTickError> {
    let error = result.error.map(str::to_owned);
    let output = result.output.to_owned();
    let code = result.code;
    let cancelled = result.cancelled;
    let check = check.clone();
    store
        .transaction_with(move |transaction| {
            finish_in_transaction(
                transaction,
                &check,
                CheckResult {
                    code,
                    error: error.as_deref(),
                    cancelled,
                    output: &output,
                },
                now,
            )
        })
        .await
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
/// Decide one sampled rule against current rows: `None` when the sample is stale.
fn decide(
    connection: &studio_runtime_store::Connection,
    sampled: &RuleSnapshot,
    now: f64,
    fingerprint: Option<RuleFileFingerprint>,
    available: bool,
) -> Result<Option<(AgentRecord, RuleDecision)>, RulesTickError> {
    let Some(rule) = RulesRepository::get(connection, sampled.id())? else {
        return Ok(None);
    };
    let owner = owner(connection, sampled.owner_id())?;
    if !sampled.current(rule.value(), &owner.value()) {
        return Ok(None);
    }
    let activity = if sampled.is_low_workers() {
        let raw = RulesTickRepository::activity(connection, sampled.owner_id())?;
        Some(active_workers(&raw.agents, &raw.monitors)?)
    } else {
        None
    };
    let decision = sampled.tick(
        now,
        TickObservation {
            fingerprint: Some(fingerprint),
            active_workers: activity,
        },
    )?;
    if decision.check.is_some() && !available {
        return Err(RulesTickError::MonitorUnavailable);
    }
    Ok(Some((owner, decision)))
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

/// Complete the rule in the same transaction as the monitor's definitive result.
/// The stored check is the authority; a foreign/stale monitor cannot finish it.
pub struct MonitorRuleFinish;
impl crate::monitor_runtime::RuleFinish for MonitorRuleFinish {
    fn finish(
        &self,
        tx: &Transaction<'_>,
        before: &serde_json::Value,
        after: &serde_json::Value,
        now: f64,
    ) -> Result<(), crate::monitor_policy::MonitorError> {
        use crate::monitor_policy::MonitorError;
        use serde_json::Value;
        // Replayed terminal receipts have already committed the rule alongside it.
        if matches!(
            before["status"].as_str(),
            Some("completed" | "failed" | "cancelled")
        ) {
            return Ok(());
        }
        let rule_id = before["ruleId"]
            .as_str()
            .ok_or(MonitorError::Refused("Missing monitor rule identity"))?;
        // Python's rule_finished is a no-op after the rule row has been deleted.
        // Commit the monitor so its durable result receipt can be acknowledged.
        let Some(rule) = RulesRepository::get(tx, rule_id)? else {
            return Ok(());
        };
        let snapshot = load_snapshot(tx, rule)?;
        let check = launch_intent(&snapshot)?.unwrap_or_else(|| snapshot.check());
        if before["id"].as_str() != Some(&check.monitor_id)
            || before["agent"].as_str() != Some(&check.agent_id)
            || before["epoch"].as_u64() != Some(check.epoch)
            || before["command"].as_str() != Some(&check.command)
            || before
                .pointer("/operation/accountKey")
                .and_then(Value::as_str)
                .is_some_and(|account| account != check.account_key.as_deref().unwrap_or("default"))
        {
            return Err(MonitorError::Refused("Monitor rule identity changed"));
        }
        if !finish_in_transaction(
            tx,
            &check,
            CheckResult {
                code: after["exitCode"].as_i64(),
                error: if after["status"] == "cancelled" {
                    Some("Monitor cancelled")
                } else {
                    after["error"].as_str()
                },
                cancelled: after["status"] == "cancelled",
                output: after["tail"].as_str().unwrap_or_default(),
            },
            now,
        )? {
            return Err(MonitorError::Refused("Monitor rule check is stale"));
        }
        Ok(())
    }
}
