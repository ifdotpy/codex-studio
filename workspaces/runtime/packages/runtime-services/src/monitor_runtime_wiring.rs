//! Retained monitor consumer. The account/session owner supplies the transport;
//! this task owns no provider connection and never repeats an uncertain command.
//! Result recovery precedes stall decisions. Publication and transport ACK follow commit.
//!
//! Registered HTTP gap: "monitor input has no request identity; pending owner decision".
//! Python has the same acknowledged-response gap: codex_rules.py:727 refuses only while
//! `inputAttempt` exists; codex_rules.py:781-782 clears it on settlement and saves
//! `lastInputAttempt` without comparing it on the next call. Therefore an identical HTTP retry
//! after an acknowledged response may be a new input. Each provider write still has a durable
//! ticketed intent; an unknown provider outcome remains latched and is never resent. Managed-tool
//! input uses its callback request identity for durable replay instead.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::Value;
use studio_runtime_store::RuntimeStore;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

use crate::monitor_driver::{self, InputRequest, MonitorNative, Submission};
use crate::monitor_output::MonitorOutput;
use crate::monitor_policy::{
    Input, InputOutcome, MonitorError, MonitorSnapshot, Operation, ReattachProof,
};
use crate::monitor_receipts::{self, ResultReceipt};
use crate::rules_engine::RuleCheck;
use crate::{event_write, monitor_runtime, rules_tick};

const BATCH: usize = 64;
const ERROR_RETRY_MIN: Duration = Duration::from_millis(250);
const ERROR_RETRY_MAX: Duration = Duration::from_secs(30);

/// The retained session must replay the same delivery until acknowledge succeeds.
/// Its cursor belongs to that session, not to a second supervisor journal reader.
#[derive(Clone)]
pub struct Delivery {
    pub id: String,
    pub operation: Operation,
    pub cursor: String,
    pub event: MonitorEvent,
}
#[derive(Clone)]
pub enum MonitorEvent {
    Output {
        offset: u64,
        bytes: Vec<u8>,
    },
    Exit {
        code: Option<i64>,
        error: Option<String>,
        finished: f64,
    },
    Input {
        ticket: String,
        outcome: InputOutcome,
    },
    Probe {
        generation: i64,
        result: Value,
    },
}

pub enum InputReply {
    Acknowledged(Value),
    Rejected(String),
    Unknown(String),
}

/// Timer methods are nonblocking: read the retained account owner's bounded queue
/// and cached supervisor proofs. Schedule native work on that owner, never wait
/// for transport here. `reattach` never starts a command. Only `input_reply` may
/// wait, within the native request deadline. All calls run outside transactions.
/// `command/exec` runs inside the retained provider app-server. Its child priority
/// is selected by the provider launch environment; this port never spawns it locally.
pub trait MonitorPort: MonitorNative + rules_tick::RuleMonitor + Send + Sync {
    /// Prepare and submit one persisted managed start. Implementations must return
    /// after a durable intent and retain Unknown without another command/exec.
    fn start_monitor(&self, _id: &str, _now: f64) -> Result<(), MonitorError> {
        Err(MonitorError::Unavailable("provider-session/monitor-start"))
    }
    fn next(&self) -> Result<Option<Delivery>, MonitorError>;
    fn acknowledge(&self, delivery: &Delivery) -> Result<(), MonitorError>;
    fn reattach(&self, snapshot: &MonitorSnapshot) -> Result<Option<ReattachProof>, MonitorError>;
    /// Reserve through the account owner, using liveness:{id}:{generation}.
    /// Return promptly; completion is delivered as MonitorEvent::Probe.
    fn probe(&self, id: &str, generation: i64, command: &str) -> Result<(), MonitorError>;
    /// Wait for this exact submitted ticket, bounded by the native request deadline.
    fn input_reply(&self, operation: &Operation, ticket: &str) -> InputReply;
}

#[derive(Default)]
struct Scan {
    monitor: String,
    stall_monitor: Option<String>,
    rule: i64,
    recovering_rules: bool,
    recovering_receipts: bool,
}

pub struct MonitorRuntime {
    store: Arc<RuntimeStore>,
    port: Arc<dyn MonitorPort>,
    output: MonitorOutput,
    scan: Mutex<Scan>,
    dispatch_wake: Arc<Notify>,
    last_tick_error: Mutex<Option<String>>,
    watcher_error: Mutex<Option<String>>,
    #[cfg(test)]
    watcher_failures: AtomicUsize,
    #[cfg(test)]
    loop_activity: Arc<LoopActivity>,
    #[cfg(test)]
    watched_directories: Mutex<BTreeSet<PathBuf>>,
}

#[cfg(test)]
#[derive(Default)]
struct LoopActivity {
    ticks: AtomicUsize,
    completed_ticks: AtomicUsize,
    deadline_reads: AtomicUsize,
    wait_cycles: AtomicUsize,
    watch_events: AtomicUsize,
    waited: Notify,
    tick_completed: Notify,
    watch_event: Notify,
    watch_updated: Notify,
}

impl MonitorRuntime {
    pub fn new(
        store: Arc<RuntimeStore>,
        port: Arc<dyn MonitorPort>,
        _event_wake: Arc<Notify>,
    ) -> Self {
        let dispatch_wake = Arc::new(Notify::new());
        let commit_wake = Arc::clone(&dispatch_wake);
        // A Notify is a single-consumer signal. The account registry also listens
        // to event_wake, so monitor correctness needs its own post-commit observer.
        store.add_commit_observer(move || commit_wake.notify_one());
        Self {
            store,
            port,
            output: MonitorOutput::default(),
            dispatch_wake,
            scan: Mutex::new(Scan {
                recovering_rules: true,
                recovering_receipts: true,
                ..Scan::default()
            }),
            last_tick_error: Mutex::new(None),
            watcher_error: Mutex::new(None),
            #[cfg(test)]
            watcher_failures: AtomicUsize::new(0),
            #[cfg(test)]
            loop_activity: Arc::new(LoopActivity::default()),
            #[cfg(test)]
            watched_directories: Mutex::new(BTreeSet::new()),
        }
    }

    /// Handle used by committed lifecycle adapters to wake cancellation dispatch.
    pub fn dispatch_wake_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.dispatch_wake)
    }

    /// Submit rule checks reserved by a committed event transaction. A lost or
    /// uncertain native submission is left to the monitor port's receipt logic.
    pub fn submit_event_checks(
        &self,
        checks: &[RuleCheck],
        now: f64,
    ) -> Result<usize, MonitorError> {
        let mut submitted = 0;
        for check in checks {
            match rules_tick::submit_check(&self.store, check, now, self.port.as_ref()) {
                Ok(true) => submitted += 1,
                Ok(false) => {}
                Err(error) => {
                    // The event transaction already committed the exact launch intent.
                    // Restart recovery from the first row so this saved identity is
                    // reconciled on the next tick; it will never invent a fresh command.
                    let mut scan = self
                        .scan
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    scan.rule = 0;
                    scan.recovering_rules = true;
                    drop(scan);
                    self.dispatch_wake.notify_one();
                    return Err(error.into());
                }
            }
        }
        Ok(submitted)
    }

    /// Wake the monitor tick after a managed creation transaction commits.
    pub fn wake_tick(&self) {
        self.dispatch_wake.notify_one();
    }

    fn wake_receipt_recovery(&self) {
        self.request_receipt_recovery();
        self.dispatch_wake.notify_one();
    }

    fn request_receipt_recovery(&self) {
        let mut scan = self
            .scan
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        scan.recovering_receipts = true;
    }

    /// Last timer failure, cleared after a successful tick. The host can inspect
    /// this while the consumer retries transient transport and storage failures.
    pub fn last_tick_error(&self) -> Option<String> {
        self.last_tick_error
            .lock()
            .expect("monitor tick diagnostic")
            .clone()
            .or_else(|| {
                self.watcher_error
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            })
    }

    fn record_watcher_error(&self, error: &MonitorError) {
        *self
            .watcher_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error.to_string());
    }

    fn clear_watcher_error(&self) {
        *self
            .watcher_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    fn rule_watch_directories(&self) -> Result<BTreeSet<PathBuf>, MonitorError> {
        let connection = self.store.read()?;
        let mut statement = connection
            .prepare("SELECT id,record FROM runtime_rules ORDER BY rowid")
            .map_err(studio_runtime_store::StoreError::from)?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(studio_runtime_store::StoreError::from)?;
        let mut directories = BTreeSet::new();
        let receipt_directory = self.store.root().join("monitor-results");
        for row in rows {
            let (id, encoded) = row.map_err(studio_runtime_store::StoreError::from)?;
            let rule: Value = serde_json::from_str(&encoded).map_err(|_| {
                MonitorError::Store(studio_runtime_store::StoreError::InvalidStoredRecord {
                    object: "rules",
                    id,
                })
            })?;
            if rule["kind"] == "file"
                && rule["status"] == "active"
                && let Some(path) = rule["path"].as_str()
                && let Some(parent) = Path::new(path).parent()
                && let Some(directory) = nearest_existing_directory(parent)?
                && directory != receipt_directory
            {
                directories.insert(directory);
            }
        }
        Ok(directories)
    }

    /// Read only the earliest persisted work deadline. This query runs after a
    /// real event or a completed batch, then the task sleeps until that deadline.
    fn next_deadline(&self) -> Result<Option<f64>, MonitorError> {
        #[cfg(test)]
        self.loop_activity
            .deadline_reads
            .fetch_add(1, Ordering::SeqCst);
        let connection = self.store.read()?;
        let monitor_deadline: Option<f64> = connection
            .query_row(
                "SELECT MIN(deadline) FROM (
                    SELECT CASE
                        WHEN COALESCE(json_extract(m.record,'$.stallProbeGeneration'),-1)=json_extract(m.record,'$.activityGeneration')
                            THEN COALESCE(json_extract(m.record,'$.stallProbeStarted'),json_extract(m.record,'$.activityAt'),json_extract(m.record,'$.created'))+?1
                        ELSE COALESCE(json_extract(m.record,'$.activityAt'),json_extract(m.record,'$.created'))+COALESCE(json_extract(m.record,'$.stallTimeoutSeconds'),?2)
                    END AS deadline
                    FROM runtime_monitors m JOIN runtime_agents a ON a.id=json_extract(m.record,'$.agent')
                    WHERE json_extract(m.record,'$.status')='running'
                      AND json_extract(a.record,'$.autoWake')=1
                      AND COALESCE(json_extract(a.record,'$.deletedAt'),0)=0
                      AND json_extract(a.record,'$.epoch')=COALESCE(json_extract(m.record,'$.epoch'),json_extract(m.record,'$.operation.epoch'))
                      AND COALESCE(json_extract(m.record,'$.stallTimeoutSeconds'),?2)>0
                      AND COALESCE(json_extract(m.record,'$.activityGeneration'),0)>COALESCE(json_extract(m.record,'$.stallWakeGeneration'),-1)
                      AND (COALESCE(json_extract(m.record,'$.stallProbeGeneration'),-1)
                           != json_extract(m.record,'$.activityGeneration')
                           OR json_type(m.record,'$.stallProbeStarted') IN ('integer','real'))
                    UNION ALL
                    SELECT COALESCE(json_extract(record,'$.cancelDispatch.nextAttemptAt'),0)
                    FROM runtime_monitors
                    WHERE json_extract(record,'$.status')='running'
                      AND json_extract(record,'$.cancelRequested')=1
                      AND COALESCE(json_extract(record,'$.cancelDispatch.status'),'pending')='pending'
                )",
                rusqlite::params![
                    crate::monitor_policy::PROBE_LEASE_SECONDS,
                    crate::monitor_policy::DEFAULT_MONITOR_STALL_SECONDS as i64,
                ],
                |row| row.get(0),
            )
            .map_err(studio_runtime_store::StoreError::from)?;
        let rule_deadline: Option<f64> = connection
            .query_row(
                "SELECT MIN(deadline) FROM (
                    SELECT CASE
                        WHEN json_extract(r.record,'$.kind')='file'
                             AND COALESCE(json_extract(r.record,'$.stallTimeoutSeconds'),?1)>0
                             AND COALESCE(json_extract(r.record,'$.fileGeneration'),0)>COALESCE(json_extract(r.record,'$.stallWakeGeneration'),-1)
                        THEN MIN(json_extract(r.record,'$.nextAt'),COALESCE(json_extract(r.record,'$.fileActivityAt'),json_extract(r.record,'$.created'))+COALESCE(json_extract(r.record,'$.stallTimeoutSeconds'),?1))
                        WHEN json_extract(r.record,'$.kind')='file'
                        THEN json_extract(r.record,'$.nextAt')
                        WHEN json_extract(r.record,'$.kind')='low_workers'
                             AND COALESCE(json_extract(r.record,'$.alerted'),0)=0
                             AND json_type(r.record,'$.lowSince') IN ('integer','real')
                        THEN json_extract(r.record,'$.lowSince')
                             + json_extract(r.record,'$.durationMinutes')*60.0 + ?2
                        WHEN json_extract(r.record,'$.kind') IN ('interval','once')
                        THEN json_extract(r.record,'$.nextAt')
                    END AS deadline
                    FROM runtime_rules r JOIN runtime_agents a ON a.id=json_extract(r.record,'$.agent')
                    WHERE json_extract(r.record,'$.status')='active'
                      AND COALESCE(json_extract(r.record,'$.inFlight'),0)=0
                      AND json_extract(a.record,'$.autoWake')=1
                      AND COALESCE(json_extract(a.record,'$.deletedAt'),0)=0
                      AND json_extract(a.record,'$.epoch')=json_extract(r.record,'$.epoch')
                      AND COALESCE((
                        (json_extract(a.record,'$.restartRecovery.stage')='pending'
                         AND json_extract(a.record,'$.restartRecovery.autoWake')=1
                         AND json_extract(a.record,'$.restartRecovery.epoch')=json_extract(a.record,'$.epoch')
                         AND json_extract(a.record,'$.restartRecovery.accountKey') IS json_extract(a.record,'$.accountKey')
                         AND json_extract(a.record,'$.restartRecovery.threadId') IS json_extract(a.record,'$.threadId'))
                        OR
                        ((CASE json_type(a.record,'$.nativeFailureHold')
                            WHEN 'true' THEN 1
                            WHEN 'integer' THEN json_extract(a.record,'$.nativeFailureHold')!=0
                            WHEN 'real' THEN json_extract(a.record,'$.nativeFailureHold')!=0
                            WHEN 'text' THEN length(json_extract(a.record,'$.nativeFailureHold'))>0
                            WHEN 'array' THEN json_array_length(a.record,'$.nativeFailureHold')>0
                            WHEN 'object' THEN json_extract(a.record,'$.nativeFailureHold')<>'{}'
                            ELSE 0 END)=1
                         AND json_extract(a.record,'$.disconnectRecovery.autoWake')=1
                         AND json_extract(a.record,'$.disconnectRecovery.epoch')=json_extract(a.record,'$.epoch')
                         AND json_extract(a.record,'$.disconnectRecovery.accountKey') IS json_extract(a.record,'$.accountKey')
                         AND json_extract(a.record,'$.disconnectRecovery.threadId') IS json_extract(a.record,'$.threadId'))
                      ),0)=0
                )",
                rusqlite::params![
                    crate::rules_engine::DEFAULT_RULE_STALL_SECONDS as i64,
                    crate::rules_engine::LOW_WORKER_STRICT_DEADLINE_MARGIN_SECONDS,
                ],
                |row| row.get(0),
            )
            .map_err(studio_runtime_store::StoreError::from)?;
        Ok(earliest_deadline([monitor_deadline, rule_deadline]))
    }

    fn record_runtime_error(&self, error: &MonitorError) {
        *self
            .last_tick_error
            .lock()
            .expect("monitor tick diagnostic") = Some(error.to_string());
    }

    /// Caller supplies wall time once per batch; tests can drive time deterministically.
    pub fn tick(&self, now: f64) -> Result<(), MonitorError> {
        self.tick_with_progress(now).map(|_| ())
    }

    fn tick_with_progress(&self, now: f64) -> Result<bool, MonitorError> {
        #[cfg(test)]
        self.loop_activity.ticks.fetch_add(1, Ordering::SeqCst);
        let result = self.tick_inner(now);
        *self
            .last_tick_error
            .lock()
            .expect("monitor tick diagnostic") = result.as_ref().err().map(ToString::to_string);
        #[cfg(test)]
        {
            self.loop_activity
                .completed_ticks
                .fetch_add(1, Ordering::SeqCst);
            self.loop_activity.tick_completed.notify_one();
        }
        result
    }

    fn tick_inner(&self, now: f64) -> Result<bool, MonitorError> {
        let mut scan = self
            .scan
            .try_lock()
            .map_err(|_| MonitorError::Refused("Monitor tick already running"))?;
        // Receipt files are scanned only at startup or on a native filesystem event.
        // Full batches remain scheduled immediately so recovery is bounded per pass.
        let result_batch_full = if scan.recovering_receipts {
            let full = monitor_receipts::recover_pending(&self.store, BATCH)? == BATCH;
            scan.recovering_receipts = full;
            full
        } else {
            false
        };
        let attach_more = self.attach_batch(&mut scan, now)?;
        let mut drained = false;
        let mut failure = None;
        for _ in 0..BATCH {
            let delivery = match self.port.next() {
                Ok(Some(delivery)) => delivery,
                Ok(None) => {
                    drained = true;
                    break;
                }
                Err(error @ MonitorError::Unavailable(_)) => return Err(error),
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            };
            if let Err(error) = self.deliver_inner(&delivery, now, false) {
                if error.is_bad_delivery() {
                    if let Err(quarantine_error) = self.quarantine_delivery(&delivery, &error, now)
                    {
                        failure = Some(quarantine_error);
                        break;
                    }
                    record_tick_failure(&mut failure, error);
                    if let Err(ack_error) = self.port.acknowledge(&delivery) {
                        record_tick_failure(&mut failure, ack_error);
                        break;
                    }
                    continue;
                }
                failure = Some(error);
                break;
            }
            // A failed ACK leaves an exact replay, never an instruction to resend.
            if let Err(error) = self.port.acknowledge(&delivery) {
                failure = Some(error);
                break;
            }
        }

        // Cancellation receipts and rule pages progress even under sustained stream
        // output. Their exact identities and replay receipts make this safe.
        let mut start_more = false;
        match monitor_driver::pending_start_ids(&self.store) {
            Ok(ids) => {
                start_more = ids.len() == BATCH;
                for id in ids {
                    if let Err(error) = self.port.start_monitor(&id, now) {
                        record_tick_failure(&mut failure, error);
                    }
                }
            }
            Err(error) => record_tick_failure(&mut failure, error),
        }
        let mut cancel_more = false;
        match monitor_driver::pending_cancel_ids(&self.store, now) {
            Ok(ids) => {
                cancel_more = ids.len() == BATCH;
                for id in ids {
                    if let Err(error) =
                        monitor_driver::cancel(&self.store, self.port.as_ref(), &id, None, now)
                    {
                        record_tick_failure(&mut failure, error);
                    }
                }
            }
            Err(error) => {
                record_tick_failure(&mut failure, error);
            }
        }
        let rules = if scan.recovering_rules {
            rules_tick::recover_batch(&self.store, scan.rule, now, self.port.as_ref())
        } else {
            rules_tick::tick_batch(
                &self.store,
                scan.rule,
                now,
                &rules_tick::LocalRuleFiles,
                self.port.as_ref(),
            )
        };
        let mut rules_more = false;
        match rules {
            Ok(rules) => {
                scan.rule = rules.next_rowid.unwrap_or(0);
                rules_more = rules.next_rowid.is_some();
                if rules.next_rowid.is_none() {
                    scan.recovering_rules = false;
                }
            }
            Err(error) => {
                // A failed batch may already have committed a launch intent.
                // Reconcile it from the same cursor before ordinary rule ticks resume.
                scan.recovering_rules = true;
                record_tick_failure(&mut failure, error.into());
            }
        }

        // Output-age stall decisions require a drained result queue and recovered
        // result receipts. Cancellation and rule work above does not.
        let mut stall_more = false;
        if drained && !result_batch_full && failure.is_none() {
            match monitor_runtime::tick_page(&self.store, now, scan.stall_monitor.as_deref()) {
                Ok(page) => {
                    stall_more = page.next_after.is_some();
                    scan.stall_monitor = page.next_after;
                    for (id, effect) in page.effects {
                        if let crate::monitor_policy::Effect::Probe {
                            generation,
                            command,
                        } = effect
                            && let Err(error) = self.port.probe(&id, generation, &command)
                        {
                            record_tick_failure(&mut failure, error);
                        }
                    }
                }
                Err(error) => {
                    record_tick_failure(&mut failure, error);
                }
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(result_batch_full
                || attach_more
                || !drained
                || stall_more
                || start_more
                || cancel_more
                || rules_more),
        }
    }

    fn quarantine_delivery(
        &self,
        delivery: &Delivery,
        error: &MonitorError,
        now: f64,
    ) -> Result<(), MonitorError> {
        let located_error = format!("{error} at delivery cursor {}", delivery.cursor);
        self.store.transaction_with(|tx| {
            let (snapshot, actor) = monitor_runtime::snapshot(tx, &delivery.id)?;
            let mut decision = crate::monitor_policy::Decision {
                record: snapshot.record().clone(),
                effects: vec![],
                response: Value::Null,
            };
            decision.record["status"] = serde_json::json!("failed");
            decision.record["error"] = serde_json::json!(located_error);
            decision.record["finished"] = serde_json::json!(now);
            decision.record["configurationPending"] = serde_json::json!(false);
            decision.record["deliveryFailure"] = serde_json::json!({
                "cursor": delivery.cursor,
                "error": error.to_string(),
            });
            decision.effects.push(
                if snapshot
                    .record()
                    .get("ruleId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| !id.is_empty())
                {
                    crate::monitor_policy::Effect::RuleFinished
                } else {
                    crate::monitor_policy::Effect::ExitEvent { wake: true }
                },
            );
            monitor_runtime::apply(tx, &snapshot, &actor, &decision, now)
        })
    }

    fn attach_batch(&self, scan: &mut Scan, now: f64) -> Result<bool, MonitorError> {
        let ids = {
            let connection = self.store.read()?;
            let mut query = connection.prepare_cached("SELECT id FROM runtime_monitors WHERE id>?1 AND json_extract(record,'$.status')='lost' AND json_extract(record,'$.reattachRecovery.status')='running' ORDER BY id LIMIT ?2").map_err(studio_runtime_store::StoreError::from)?;
            query
                .query_map((&scan.monitor, BATCH as i64), |row| row.get::<_, String>(0))
                .map_err(studio_runtime_store::StoreError::from)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(studio_runtime_store::StoreError::from)?
        };
        for id in &ids {
            let (snapshot, _) = self
                .store
                .transaction_with(|tx| monitor_runtime::snapshot(tx, id))?;
            if let Some(proof) = self.port.reattach(&snapshot)? {
                monitor_runtime::reattach(&self.store, id, &proof, now)?;
            }
            scan.monitor.clone_from(id);
        }
        if ids.len() < BATCH {
            scan.monitor.clear();
        }
        Ok(ids.len() == BATCH)
    }

    /// The retained session may call this directly before committing its own cursor.
    pub fn deliver(&self, delivery: &Delivery, now: f64) -> Result<(), MonitorError> {
        self.deliver_inner(delivery, now, true)
    }

    fn deliver_inner(
        &self,
        delivery: &Delivery,
        now: f64,
        wake_dispatch: bool,
    ) -> Result<(), MonitorError> {
        match &delivery.event {
            MonitorEvent::Output { offset, bytes } => self.output.append(
                &self.store,
                &delivery.id,
                &delivery.operation,
                *offset,
                bytes,
                now,
            )?,
            MonitorEvent::Exit {
                code,
                error,
                finished,
            } => {
                // Reject a foreign stream before it can occupy the first-result file.
                // On a read failure the retained delivery remains unacknowledged.
                let (snapshot, _) = self
                    .store
                    .transaction_with(|tx| monitor_runtime::snapshot(tx, &delivery.id))?;
                if snapshot.operation() != Some(&delivery.operation) {
                    return Err(MonitorError::InvalidDeliveryIdentity(
                        "result operation changed",
                    ));
                }
                monitor_receipts::finish(
                    &self.store,
                    &ResultReceipt {
                        version: 1,
                        key: delivery.id.clone(),
                        code: *code,
                        error: error.clone(),
                        operation: Some(delivery.operation.clone()),
                        finished: *finished,
                    },
                )?;
            }
            MonitorEvent::Input { ticket, outcome } => monitor_runtime::settle_input(
                &self.store,
                &delivery.id,
                &delivery.operation,
                ticket,
                outcome.clone(),
                now,
            )
            .map_err(|error| match error {
                MonitorError::Refused("Monitor operation identity changed") => {
                    MonitorError::InvalidDeliveryIdentity("input operation changed")
                }
                MonitorError::Refused("Monitor input receipt identity changed") => {
                    MonitorError::InvalidDeliveryIdentity("input receipt changed")
                }
                error => error,
            })?,
            MonitorEvent::Probe { generation, result } => {
                self.store.transaction_with(|tx| {
                    let (snapshot, actor) = monitor_runtime::snapshot(tx, &delivery.id)?;
                    if snapshot.operation() != Some(&delivery.operation) {
                        return Err(MonitorError::InvalidDeliveryIdentity(
                            "probe operation changed",
                        ));
                    }
                    let decision = crate::monitor_policy::probe_result(
                        &snapshot,
                        *generation,
                        result.clone(),
                        now,
                    )?;
                    monitor_runtime::apply(tx, &snapshot, &actor, &decision, now)
                })?;
            }
        }
        if wake_dispatch {
            self.dispatch_wake.notify_one();
        }
        Ok(())
    }

    pub fn cancel(&self, id: &str, owner: Option<&str>, now: f64) -> Result<Value, MonitorError> {
        let result = monitor_driver::cancel(&self.store, self.port.as_ref(), id, owner, now)?;
        self.dispatch_wake.notify_one();
        Ok(result)
    }

    /// Submit HTTP input with a fresh server ticket. The HTTP schema currently has
    /// no caller request identity, so a lost HTTP response cannot be distinguished
    /// from a new identical request; that retry gap is registered in this module's
    /// documentation. Provider writes still commit an intent first, and an
    /// uncertain provider outcome is retained and never resent.
    pub fn input_http(&self, id: &str, input: &Input, now: f64) -> Result<Value, MonitorError> {
        let ticket = http_input_ticket()?;
        self.input(id, &ticket, input, now)
    }

    pub fn input(
        &self,
        id: &str,
        ticket: &str,
        input: &Input,
        now: f64,
    ) -> Result<Value, MonitorError> {
        let (snapshot, _) = self
            .store
            .transaction_with(|tx| monitor_runtime::snapshot(tx, id))?;
        let operation = snapshot.operation().ok_or(MonitorError::Refused(
            "This interactive monitor is not active",
        ))?;
        let submitted = monitor_driver::input(
            &self.store,
            self.port.as_ref(),
            InputRequest {
                id,
                operation,
                ticket,
                input,
            },
            now,
        )?;
        if !matches!(submitted, Submission::Written) {
            return Err(MonitorError::Refused(
                "Monitor input was not acknowledged; inspect its receipt",
            ));
        }
        let (outcome, response) = match self.port.input_reply(operation, ticket) {
            InputReply::Acknowledged(value) => (InputOutcome::Acknowledged, Ok(value)),
            InputReply::Rejected(error) => (
                InputOutcome::Rejected(error),
                Err(MonitorError::Refused("Monitor input rejected")),
            ),
            InputReply::Unknown(error) => (
                InputOutcome::Unknown(error),
                Err(MonitorError::Refused(
                    "Monitor input outcome unknown; do not repeat it",
                )),
            ),
        };
        monitor_runtime::settle_input(&self.store, id, operation, ticket, outcome, now)?;
        response
    }

    /// Submit managed input only when the authenticated caller still owns the
    /// active monitor at the same actor epoch.
    pub fn input_for_owner(
        &self,
        caller_id: &str,
        caller_epoch: i64,
        id: &str,
        ticket: &str,
        input: &Input,
        now: f64,
    ) -> Result<Value, MonitorError> {
        self.store.read_transaction_with_error(|connection| {
            let caller =
                studio_runtime_store::system_reads::get(connection, "runtime_agents", caller_id)?
                    .ok_or(studio_runtime_store::StoreError::RecordContract(
                    "Monitor input caller is no longer active",
                ))?;
            if caller.get("epoch").and_then(Value::as_i64) != Some(caller_epoch)
                || caller.get("autoWake") != Some(&Value::Bool(true))
                || studio_runtime_store::truthy(caller.get("deletedAt"))
            {
                return Err(studio_runtime_store::StoreError::RecordContract(
                    "Monitor input caller is no longer active",
                ));
            }
            let (snapshot, owner) =
                monitor_runtime::snapshot(connection, id).map_err(|error| match error {
                    MonitorError::Store(error) => error,
                    _ => studio_runtime_store::StoreError::RecordContract(
                        "Unknown monitor in this caller's scope",
                    ),
                })?;
            if snapshot.agent() != caller_id
                || snapshot.record().get("epoch").and_then(Value::as_i64) != Some(caller_epoch)
                || owner.epoch != caller_epoch
            {
                return Err(studio_runtime_store::StoreError::RecordContract(
                    "You can input only to your own active monitor",
                ));
            }
            Ok(())
        })?;
        self.input(id, ticket, input, now)
    }
}

fn http_input_ticket() -> Result<String, MonitorError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|_| MonitorError::Refused("Could not allocate monitor input ticket"))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(uuid::Uuid::from_bytes(bytes).to_string())
}

fn record_tick_failure(current: &mut Option<MonitorError>, incoming: MonitorError) {
    if current
        .as_ref()
        .is_none_or(|error| !error.fatal_for_tick() && incoming.fatal_for_tick())
    {
        *current = Some(incoming);
    }
}

fn earliest_deadline<const N: usize>(deadlines: [Option<f64>; N]) -> Option<f64> {
    deadlines
        .into_iter()
        .flatten()
        .filter(|deadline| deadline.is_finite() && *deadline >= 0.0)
        .min_by(f64::total_cmp)
}

fn nearest_existing_directory(path: &Path) -> Result<Option<PathBuf>, MonitorError> {
    let mut candidate = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    loop {
        if candidate.is_dir() {
            return Ok(Some(candidate));
        }
        if !candidate.pop() {
            return Ok(None);
        }
    }
}

fn filesystem_event_needs_rescan(event: &notify::Event) -> bool {
    event.need_rescan() || matches!(&event.kind, EventKind::Other)
}

/// P01 owns this task and the retained transport. Signal shutdown and await the
/// returned handle before closing the store; do not abort a commit in progress.
pub fn start(
    runtime: Arc<MonitorRuntime>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<Result<(), MonitorError>> {
    tokio::spawn(async move {
        let (receipt_tx, mut receipt_events) = watch::channel(false);
        let (rule_tx, mut rule_events) = watch::channel(false);
        let (watch_reset_tx, mut watch_reset_events) = watch::channel(());
        let mut watcher = None;
        let mut watched_rule_directories = BTreeSet::new();
        let mut immediate = true;
        let mut retry_at: Option<tokio::time::Instant> = None;
        let mut retry_delay = ERROR_RETRY_MIN;
        let mut watcher_retry_at: Option<tokio::time::Instant> = None;
        let mut watcher_retry_delay = ERROR_RETRY_MIN;
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            let now = tokio::time::Instant::now();
            if watcher.is_none() && watcher_retry_at.is_none_or(|deadline| now >= deadline) {
                match install_file_watcher(
                    &runtime,
                    receipt_tx.clone(),
                    rule_tx.clone(),
                    watch_reset_tx.clone(),
                ) {
                    Ok((installed, directories)) => {
                        watcher = Some(installed);
                        watched_rule_directories = directories;
                        #[cfg(test)]
                        {
                            *runtime
                                .watched_directories
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                watched_rule_directories.clone();
                            runtime.loop_activity.watch_updated.notify_one();
                        }
                        watcher_retry_at = None;
                        watcher_retry_delay = ERROR_RETRY_MIN;
                        runtime.clear_watcher_error();
                        // Registration precedes this baseline scan, closing the
                        // gap for receipts written while watching was unavailable.
                        runtime.request_receipt_recovery();
                        immediate = true;
                    }
                    Err(error) if error.store_failure_for_task() => return Err(error),
                    Err(error) => {
                        eprintln!("monitor filesystem watcher unavailable: {error}");
                        runtime.record_watcher_error(&error);
                        watcher_retry_at = Some(now + watcher_retry_delay);
                        watcher_retry_delay = (watcher_retry_delay * 2).min(ERROR_RETRY_MAX);
                    }
                }
            }

            if immediate && retry_at.is_none_or(|deadline| now >= deadline) {
                let wall_now = match event_write::unix_time() {
                    Ok(value) => value,
                    Err(error) => {
                        let error = MonitorError::Event(error);
                        runtime.record_runtime_error(&error);
                        retry_at = Some(now + retry_delay);
                        retry_delay = (retry_delay * 2).min(ERROR_RETRY_MAX);
                        continue;
                    }
                };
                let tick_runtime = Arc::clone(&runtime);
                let tick_result = studio_runtime_process::blocking_pool::spawn_blocking(
                    "runtime-services/monitor_runtime_wiring",
                    move || tick_runtime.tick_with_progress(wall_now),
                )
                .await
                .map_err(|_| MonitorError::TaskFailed);
                match tick_result {
                    Ok(Ok(more)) => {
                        immediate = more;
                        retry_at = None;
                        retry_delay = ERROR_RETRY_MIN;
                        let update_result = if let Some(active_watcher) = watcher.as_mut() {
                            runtime.rule_watch_directories().and_then(|directories| {
                                register_rule_directories(
                                    active_watcher,
                                    &mut watched_rule_directories,
                                    &directories,
                                )
                            })
                        } else {
                            Ok(())
                        };
                        #[cfg(test)]
                        if update_result.is_ok() {
                            *runtime
                                .watched_directories
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                watched_rule_directories.clone();
                            runtime.loop_activity.watch_updated.notify_one();
                        }
                        if let Err(error) = update_result {
                            if error.store_failure_for_task() {
                                return Err(error);
                            }
                            eprintln!("monitor rule filesystem watcher update failed: {error}");
                            runtime.record_watcher_error(&error);
                            watcher = None;
                            watched_rule_directories.clear();
                            watcher_retry_at =
                                Some(tokio::time::Instant::now() + watcher_retry_delay);
                            watcher_retry_delay = (watcher_retry_delay * 2).min(ERROR_RETRY_MAX);
                        }
                    }
                    Ok(Err(error)) if error.store_failure_for_task() => return Err(error),
                    Err(error) => return Err(error),
                    Ok(Err(error)) => {
                        runtime.record_runtime_error(&error);
                        retry_at = Some(tokio::time::Instant::now() + retry_delay);
                        retry_delay = (retry_delay * 2).min(ERROR_RETRY_MAX);
                        immediate = false;
                    }
                }
                if immediate {
                    continue;
                }
            }

            let durable_deadline = match runtime.next_deadline() {
                Ok(Some(deadline)) => match event_write::unix_time() {
                    Ok(now) => {
                        let seconds = (deadline - now).max(0.0);
                        Some(tokio::time::Instant::now() + Duration::from_secs_f64(seconds))
                    }
                    Err(error) => {
                        let error = MonitorError::Event(error);
                        runtime.record_runtime_error(&error);
                        let deadline = tokio::time::Instant::now() + retry_delay;
                        retry_at = Some(retry_at.map_or(deadline, |old| old.min(deadline)));
                        retry_delay = (retry_delay * 2).min(ERROR_RETRY_MAX);
                        None
                    }
                },
                Ok(None) => None,
                Err(error) if error.store_failure_for_task() => return Err(error),
                Err(error) => {
                    runtime.record_runtime_error(&error);
                    let deadline = tokio::time::Instant::now() + retry_delay;
                    retry_at = Some(retry_at.map_or(deadline, |old| old.min(deadline)));
                    retry_delay = (retry_delay * 2).min(ERROR_RETRY_MAX);
                    None
                }
            };
            let dispatch_deadline = retry_at.or(durable_deadline);
            let wait_until = [dispatch_deadline, watcher_retry_at]
                .into_iter()
                .flatten()
                .min();
            #[cfg(test)]
            {
                runtime
                    .loop_activity
                    .wait_cycles
                    .fetch_add(1, Ordering::SeqCst);
                runtime.loop_activity.waited.notify_one();
            }
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return Ok(()); }
                }
                _ = runtime.dispatch_wake.notified() => {
                    immediate = true;
                }
                changed = receipt_events.changed() => {
                    if changed.is_err() { return Err(MonitorError::TaskFailed); }
                    if *receipt_events.borrow_and_update() {
                        runtime.wake_receipt_recovery();
                        immediate = true;
                    }
                }
                changed = rule_events.changed() => {
                    if changed.is_err() { return Err(MonitorError::TaskFailed); }
                    if *rule_events.borrow_and_update() {
                        immediate = true;
                    }
                }
                changed = watch_reset_events.changed() => {
                    if changed.is_err() { return Err(MonitorError::TaskFailed); }
                    watch_reset_events.borrow_and_update();
                    runtime.request_receipt_recovery();
                    watcher = None;
                    watched_rule_directories.clear();
                    watcher_retry_at = None;
                    immediate = true;
                }
                _ = async {
                    match wait_until {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    let now = tokio::time::Instant::now();
                    if retry_at.is_none_or(|deadline| now >= deadline) {
                        retry_at = None;
                        immediate = true;
                    }
                    if watcher_retry_at.is_none_or(|deadline| now >= deadline) {
                        immediate = true;
                    }
                }
            }
        }
    })
}

fn install_file_watcher(
    runtime: &MonitorRuntime,
    receipt_events: watch::Sender<bool>,
    rule_events: watch::Sender<bool>,
    watch_reset: watch::Sender<()>,
) -> Result<(RecommendedWatcher, BTreeSet<PathBuf>), MonitorError> {
    #[cfg(test)]
    if runtime
        .watcher_failures
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
            if remaining > 0 {
                Some(remaining - 1)
            } else {
                None
            }
        })
        .is_ok()
    {
        return Err(MonitorError::Refused("test watcher unavailable"));
    }
    let receipt_directory = monitor_receipts::prepare_watch_directory(runtime.store.root())?;
    let rule_directories = runtime.rule_watch_directories()?;
    let receipt_directory_for_callback = receipt_directory.clone();
    #[cfg(test)]
    let callback_activity = Arc::clone(&runtime.loop_activity);
    let mut watcher =
        notify::recommended_watcher(move |result: notify::Result<notify::Event>| match result {
            Ok(event) if filesystem_event_needs_rescan(&event) => {
                #[cfg(test)]
                {
                    callback_activity
                        .watch_events
                        .fetch_add(1, Ordering::SeqCst);
                    callback_activity.watch_event.notify_one();
                }
                receipt_events.send_replace(true);
                rule_events.send_replace(true);
                watch_reset.send_replace(());
            }
            Ok(event)
                if matches!(
                    &event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                ) =>
            {
                #[cfg(test)]
                {
                    callback_activity
                        .watch_events
                        .fetch_add(1, Ordering::SeqCst);
                    callback_activity.watch_event.notify_one();
                }
                if event
                    .paths
                    .iter()
                    .any(|path| path.starts_with(&receipt_directory_for_callback))
                {
                    receipt_events.send_replace(true);
                    rule_events.send_replace(true);
                } else {
                    rule_events.send_replace(true);
                }
            }
            Ok(_) => {}
            Err(error) => {
                #[cfg(test)]
                {
                    callback_activity
                        .watch_events
                        .fetch_add(1, Ordering::SeqCst);
                    callback_activity.watch_event.notify_one();
                }
                eprintln!("monitor filesystem watcher requested a rescan: {error}");
                receipt_events.send_replace(true);
                rule_events.send_replace(true);
                watch_reset.send_replace(());
            }
        })
        .map_err(|_| MonitorError::Refused("Monitor filesystem watcher could not start"))?;
    watcher
        .watch(&receipt_directory, RecursiveMode::NonRecursive)
        .map_err(|_| MonitorError::Refused("Monitor receipt watcher could not start"))?;
    let mut watched = BTreeSet::new();
    register_rule_directories(&mut watcher, &mut watched, &rule_directories)?;
    Ok((watcher, watched))
}

fn register_rule_directories(
    watcher: &mut RecommendedWatcher,
    watched: &mut BTreeSet<PathBuf>,
    directories: &BTreeSet<PathBuf>,
) -> Result<(), MonitorError> {
    let stale = watched.difference(directories).cloned().collect::<Vec<_>>();
    for directory in stale {
        // The path may already have disappeared; dropping it from our set is
        // still required so a recreated directory is watched on the next scan.
        let _unwatch_result = watcher.unwatch(&directory);
        watched.remove(&directory);
    }
    let new_directories = directories.difference(watched).cloned().collect::<Vec<_>>();
    for directory in new_directories {
        watcher
            .watch(&directory, RecursiveMode::NonRecursive)
            .map_err(|_| MonitorError::Refused("Monitor rule file watcher could not start"))?;
        watched.insert(directory);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ERROR_RETRY_MIN, MonitorRuntime, record_tick_failure};
    use crate::monitor_driver::{MonitorNative, Submission};
    use crate::monitor_policy::{Effect, MonitorError, MonitorSnapshot, Operation, ReattachProof};
    use crate::monitor_runtime_wiring::{Delivery, InputReply};
    use crate::rules_engine::RuleCheck;
    use crate::rules_tick::{CheckReconciliation, CheckSubmission, RuleMonitor, RulesTickError};
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use studio_runtime_store::RuntimeStore;
    use tokio::sync::{Notify, watch};

    #[derive(Default)]
    struct EventPort {
        next_calls: AtomicUsize,
        fail_next: AtomicUsize,
        queued_results: AtomicUsize,
        drained_results: AtomicUsize,
        probes: AtomicUsize,
        called: Notify,
        probe_called: Notify,
    }

    impl MonitorNative for EventPort {
        fn submit(&self, _operation: &Operation, _effect: &Effect) -> Submission {
            Submission::Unknown
        }
    }

    impl RuleMonitor for EventPort {
        fn available(&self) -> bool {
            false
        }
        fn launch(&self, _check: &RuleCheck) -> CheckSubmission {
            CheckSubmission::Unknown
        }
        fn cancel(&self, _monitor_id: &str) -> Result<(), RulesTickError> {
            Ok(())
        }
        fn reconcile(&self, _check: &RuleCheck) -> CheckReconciliation {
            CheckReconciliation::Unknown
        }
    }

    impl super::MonitorPort for EventPort {
        fn next(&self) -> Result<Option<Delivery>, MonitorError> {
            self.next_calls.fetch_add(1, Ordering::SeqCst);
            self.called.notify_one();
            let queued = self.queued_results.swap(0, Ordering::SeqCst);
            self.drained_results.fetch_add(queued, Ordering::SeqCst);
            if self
                .fail_next
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                    if count > 0 { Some(count - 1) } else { None }
                })
                .is_ok()
            {
                return Err(MonitorError::Refused("test transient"));
            }
            Ok(None)
        }
        fn acknowledge(&self, _delivery: &Delivery) -> Result<(), MonitorError> {
            Ok(())
        }
        fn reattach(
            &self,
            _snapshot: &MonitorSnapshot,
        ) -> Result<Option<ReattachProof>, MonitorError> {
            Ok(None)
        }
        fn probe(&self, _id: &str, _generation: i64, _command: &str) -> Result<(), MonitorError> {
            self.probes.fetch_add(1, Ordering::SeqCst);
            self.probe_called.notify_one();
            Ok(())
        }
        fn input_reply(&self, _operation: &Operation, _ticket: &str) -> InputReply {
            InputReply::Unknown("unused".into())
        }
    }

    async fn wait_for_calls(port: &EventPort, expected: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let called = port.called.notified();
                if port.next_calls.load(Ordering::SeqCst) >= expected {
                    return;
                }
                called.await;
            }
        })
        .await
        .expect("monitor port call arrives");
    }

    async fn wait_for_probes(port: &EventPort, expected: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let probed = port.probe_called.notified();
                if port.probes.load(Ordering::SeqCst) >= expected {
                    return;
                }
                probed.await;
            }
        })
        .await
        .expect("monitor probe arrives");
    }

    async fn wait_for_cycles(runtime: &MonitorRuntime, expected: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let waited = runtime.loop_activity.waited.notified();
                if runtime.loop_activity.wait_cycles.load(Ordering::SeqCst) >= expected {
                    return;
                }
                waited.await;
            }
        })
        .await
        .expect("monitor enters its event wait");
    }

    async fn wait_for_completed_ticks(runtime: &MonitorRuntime, expected: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let completed = runtime.loop_activity.tick_completed.notified();
                if runtime.loop_activity.completed_ticks.load(Ordering::SeqCst) >= expected {
                    return;
                }
                completed.await;
            }
        })
        .await
        .expect("monitor completes the expected tick");
    }

    async fn wait_for_watch_events(runtime: &MonitorRuntime, expected: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let changed = runtime.loop_activity.watch_event.notified();
                if runtime.loop_activity.watch_events.load(Ordering::SeqCst) >= expected {
                    return;
                }
                changed.await;
            }
        })
        .await
        .expect("native filesystem watcher reports event");
    }

    async fn wait_for_watch_directory(runtime: &MonitorRuntime, directory: &Path) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let updated = runtime.loop_activity.watch_updated.notified();
                if runtime
                    .watched_directories
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains(directory)
                {
                    return;
                }
                updated.await;
            }
        })
        .await
        .expect("monitor watcher reconciles watched directory");
    }

    async fn stop_task(
        shutdown: &watch::Sender<bool>,
        task: tokio::task::JoinHandle<Result<(), MonitorError>>,
    ) {
        shutdown
            .send(true)
            .expect("monitor shutdown receiver exists");
        task.await
            .expect("monitor task joins")
            .expect("monitor task stops cleanly");
    }

    fn fixture() -> (tempfile::TempDir, Arc<RuntimeStore>, Arc<EventPort>) {
        let root = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).expect("store opens"));
        let port = Arc::new(EventPort::default());
        (root, store, port)
    }

    fn insert_active_owner(store: &RuntimeStore) {
        store
            .transaction_with(|tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES ('owner',?1)",
                    [serde_json::json!({
                        "id":"owner","rootId":"owner","epoch":0,"status":"ready",
                        "autoWake":true,"accountKey":"default","threadId":"thread"
                    })
                    .to_string()],
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("persist active owner fixture");
    }

    #[test]
    fn fatal_phase_error_replaces_an_earlier_retryable_error() {
        let mut failure = Some(MonitorError::Refused("transport unavailable"));
        record_tick_failure(&mut failure, MonitorError::InvalidResultReceipt("json"));
        assert!(matches!(
            failure.as_ref(),
            Some(MonitorError::InvalidResultReceipt("json"))
        ));

        record_tick_failure(&mut failure, MonitorError::Refused("transient"));
        assert!(matches!(
            failure.as_ref(),
            Some(MonitorError::InvalidResultReceipt("json"))
        ));

        let malformed_rule = studio_runtime_store::RuleRecord::from_value(serde_json::json!({
            "agent": "agent-without-id"
        }))
        .expect_err("persisted rule without string id");
        let rule_error = MonitorError::Rules(crate::rules_tick::RulesTickError::Store(
            malformed_rule.into(),
        ));
        assert!(rule_error.fatal_for_tick());
    }

    #[tokio::test(start_paused = true)]
    async fn monitor_waits_idle_and_a_pre_wait_wake_is_not_lost() {
        let (_root, store, port) = fixture();
        let runtime = Arc::new(MonitorRuntime::new(
            store,
            port.clone(),
            Arc::new(Notify::new()),
        ));
        runtime.wake_tick();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = super::start(runtime.clone(), shutdown_rx);
        wait_for_calls(&port, 2).await;
        wait_for_cycles(&runtime, 1).await;
        let ticks = runtime.loop_activity.ticks.load(Ordering::SeqCst);
        let reads = runtime.loop_activity.deadline_reads.load(Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(3600)).await;
        tokio::task::yield_now().await;
        assert_eq!(runtime.loop_activity.ticks.load(Ordering::SeqCst), ticks);
        assert_eq!(
            runtime.loop_activity.deadline_reads.load(Ordering::SeqCst),
            reads
        );
        stop_task(&shutdown_tx, task).await;
    }

    #[tokio::test(start_paused = true)]
    async fn queued_output_and_exit_results_wake_a_sleeping_consumer() {
        let (_root, store, port) = fixture();
        let runtime = Arc::new(MonitorRuntime::new(
            store,
            port.clone(),
            Arc::new(Notify::new()),
        ));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = super::start(runtime.clone(), shutdown_rx);
        wait_for_calls(&port, 1).await;
        wait_for_cycles(&runtime, 1).await;
        port.queued_results.store(2, Ordering::SeqCst);
        runtime.wake_tick();
        wait_for_calls(&port, 2).await;
        assert_eq!(port.drained_results.load(Ordering::SeqCst), 2);
        stop_task(&shutdown_tx, task).await;
    }

    #[tokio::test(start_paused = true)]
    async fn watcher_failure_does_not_gate_committed_dispatch_wakes() {
        let (_root, store, port) = fixture();
        let runtime = Arc::new(MonitorRuntime::new(
            store,
            port.clone(),
            Arc::new(Notify::new()),
        ));
        runtime.watcher_failures.store(1, Ordering::SeqCst);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = super::start(runtime.clone(), shutdown_rx);
        wait_for_calls(&port, 1).await;
        assert_eq!(
            runtime.last_tick_error().as_deref(),
            Some("test watcher unavailable")
        );

        port.queued_results.store(2, Ordering::SeqCst);
        runtime.wake_tick();
        wait_for_calls(&port, 2).await;
        assert_eq!(port.drained_results.load(Ordering::SeqCst), 2);
        stop_task(&shutdown_tx, task).await;
    }

    #[tokio::test]
    async fn file_rule_with_missing_parent_uses_nearest_existing_watch_ancestor() {
        let root = tempfile::tempdir().expect("temporary watch root");
        let state_directory = root.path().join("state");
        std::fs::create_dir(&state_directory).expect("create isolated state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(&state_directory)).expect("open isolated state store"),
        );
        let port = Arc::new(EventPort::default());
        insert_active_owner(&store);
        let missing_parent = root.path().join("new-directory/deeper");
        store
            .transaction_with(|tx| {
                tx.execute(
                    "INSERT INTO runtime_rules(id,record) VALUES ('file-rule',?1)",
                    [serde_json::json!({
                        "id":"file-rule","agent":"owner","epoch":0,"name":"watch",
                        "kind":"file","status":"active","intervalSeconds":30,
                        "nextAt":9_000_000_000_000_000.0,"created":1.0,"command":"true",
                        "text":"","path":missing_parent.join("watched.txt").to_string_lossy(),
                        "inFlight":false
                    })
                    .to_string()],
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("persist file rule fixture");
        let runtime = Arc::new(MonitorRuntime::new(
            store,
            port.clone(),
            Arc::new(Notify::new()),
        ));
        assert_eq!(
            runtime
                .rule_watch_directories()
                .expect("watch ancestors")
                .into_iter()
                .collect::<Vec<_>>(),
            vec![root.path().to_owned()]
        );

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = super::start(runtime.clone(), shutdown_rx);
        wait_for_calls(&port, 1).await;
        let first_watch_event = runtime.loop_activity.watch_events.load(Ordering::SeqCst) + 1;
        let new_ancestor = root.path().join("new-directory");
        std::fs::create_dir(&new_ancestor).expect("create watched ancestor");
        wait_for_watch_events(&runtime, first_watch_event).await;
        wait_for_watch_directory(&runtime, &new_ancestor).await;
        stop_task(&shutdown_tx, task).await;
    }

    #[test]
    fn stall_monitor_scan_pages_past_the_first_sixty_four_ids() {
        let (_root, store, _port) = fixture();
        insert_active_owner(&store);
        store
            .transaction_with(|tx| {
                for index in 0..65 {
                    let id = format!("monitor-{index:03}");
                    let activity_at = if index == 64 { 0.0 } else { 100.0 };
                    tx.execute(
                        "INSERT INTO runtime_monitors(id,record) VALUES (?1,?2)",
                        (
                            &id,
                            serde_json::json!({
                                "id":id,"agent":"owner","epoch":0,"status":"running",
                                "created":activity_at,"activityAt":activity_at,
                                "activityGeneration":1,"stallWakeGeneration":0,
                                "stallTimeoutSeconds":50,"livenessCommand":"true"
                            })
                            .to_string(),
                        ),
                    )?;
                }
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("persist 65 running monitor fixtures");

        let first = crate::monitor_runtime::tick_page(&store, 100.0, None)
            .expect("process first monitor page");
        assert_eq!(first.next_after.as_deref(), Some("monitor-063"));
        assert!(first.effects.is_empty());
        let second = crate::monitor_runtime::tick_page(&store, 100.0, first.next_after.as_deref())
            .expect("process second monitor page");
        assert_eq!(second.next_after, None);
        assert_eq!(second.effects.len(), 1);
        assert_eq!(second.effects[0].0, "monitor-064");
        assert!(matches!(&second.effects[0].1, Effect::Probe { .. }));
    }

    #[tokio::test(start_paused = true)]
    async fn monitor_loop_reaches_a_due_sixty_fifth_monitor_then_sleeps() {
        let (_root, store, port) = fixture();
        insert_active_owner(&store);
        let monitor_count = crate::monitor_runtime::MONITOR_TICK_BATCH + 1;
        let page_count = monitor_count.div_ceil(crate::monitor_runtime::MONITOR_TICK_BATCH);
        // The probe commit resets the cursor, so the follow-up scan traverses the
        // same pages once more before the runtime can enter its idle wait.
        let expected_ticks = page_count * 2;
        let active_at = crate::event_write::unix_time().expect("read fixture time");
        store
            .transaction_with(|tx| {
                for index in 0..monitor_count {
                    let id = format!("monitor-{index:03}");
                    let activity_at = if index == 64 {
                        active_at - 100.0
                    } else {
                        active_at
                    };
                    tx.execute(
                        "INSERT INTO runtime_monitors(id,record) VALUES (?1,?2)",
                        (
                            &id,
                            serde_json::json!({
                                "id":id,"agent":"owner","epoch":0,"status":"running",
                                "created":activity_at,"activityAt":activity_at,
                                "activityGeneration":1,"stallWakeGeneration":0,
                                "stallTimeoutSeconds":50,"livenessCommand":"true"
                            })
                            .to_string(),
                        ),
                    )?;
                }
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("persist running monitor fixtures");
        let runtime = Arc::new(MonitorRuntime::new(
            store,
            port.clone(),
            Arc::new(Notify::new()),
        ));
        {
            let mut scan = runtime
                .scan
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            scan.recovering_rules = false;
        }
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = super::start(runtime.clone(), shutdown_rx);

        wait_for_probes(&port, 1).await;
        wait_for_calls(&port, expected_ticks).await;
        wait_for_completed_ticks(&runtime, expected_ticks).await;
        wait_for_cycles(&runtime, page_count).await;
        assert!(
            runtime
                .scan
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .stall_monitor
                .is_none(),
            "both cursor traversals must finish before claiming the loop is idle"
        );
        assert_eq!(port.probes.load(Ordering::SeqCst), 1);
        assert_eq!(
            runtime.loop_activity.ticks.load(Ordering::SeqCst),
            expected_ticks
        );
        let ticks = runtime.loop_activity.ticks.load(Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert_eq!(runtime.loop_activity.ticks.load(Ordering::SeqCst), ticks);
        stop_task(&shutdown_tx, task).await;
    }

    #[tokio::test(start_paused = true)]
    async fn transient_port_error_retries_after_backoff_and_restarts_from_durable_state() {
        let (root, store, port) = fixture();
        port.fail_next.store(1, Ordering::SeqCst);
        let runtime = Arc::new(MonitorRuntime::new(
            store,
            port.clone(),
            Arc::new(Notify::new()),
        ));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = super::start(runtime.clone(), shutdown_rx);
        wait_for_calls(&port, 1).await;
        wait_for_cycles(&runtime, 1).await;
        assert_eq!(runtime.last_tick_error().as_deref(), Some("test transient"));
        tokio::time::advance(ERROR_RETRY_MIN).await;
        wait_for_calls(&port, 2).await;
        wait_for_cycles(&runtime, 2).await;
        assert_eq!(runtime.last_tick_error(), None);
        stop_task(&shutdown_tx, task).await;

        drop(runtime);
        crate::monitor_receipts::persist(
            root.path(),
            &crate::monitor_receipts::ResultReceipt {
                version: 1,
                key: "unknown-monitor-result".into(),
                code: Some(0),
                error: None,
                operation: None,
                finished: 1.0,
            },
        )
        .expect("persist an unresolved receipt before restart");
        let reopened = Arc::new(RuntimeStore::open(Some(root.path())).expect("store reopens"));
        let restarted_port = Arc::new(EventPort::default());
        let restarted = Arc::new(MonitorRuntime::new(
            reopened,
            restarted_port.clone(),
            Arc::new(Notify::new()),
        ));
        let (restart_shutdown, restart_rx) = watch::channel(false);
        let restart_task = super::start(restarted, restart_rx);
        wait_for_calls(&restarted_port, 1).await;
        let receipts = std::fs::read_dir(root.path().join("monitor-results"))
            .expect("receipt directory remains readable")
            .count();
        assert_eq!(
            receipts, 1,
            "unknown result identity stays available after restart"
        );
        stop_task(&restart_shutdown, restart_task).await;
    }

    #[test]
    fn earliest_deadline_uses_only_finite_durable_times() {
        assert_eq!(
            super::earliest_deadline([Some(12.0), Some(7.0), None, Some(f64::INFINITY)]),
            Some(7.0)
        );
        assert_eq!(super::earliest_deadline([None, Some(f64::NAN)]), None);
    }

    #[test]
    fn durable_stall_and_rule_deadlines_choose_the_earliest_timestamp() {
        let (_root, store, port) = fixture();
        let runtime = MonitorRuntime::new(store.clone(), port, Arc::new(Notify::new()));
        store
            .transaction_with(|tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES ('owner',?1)",
                    [serde_json::json!({"id":"owner","epoch":1,"autoWake":true}).to_string()],
                )?;
                tx.execute(
                    "INSERT INTO runtime_monitors(id,record) VALUES ('watch',?1)",
                    [serde_json::json!({
                        "id":"watch","agent":"owner","epoch":1,"status":"running",
                        "created":90.0,"activityAt":100.0,"activityGeneration":2,
                        "stallWakeGeneration":0,"stallTimeoutSeconds":50
                    })
                    .to_string()],
                )?;
                tx.execute(
                    "INSERT INTO runtime_rules(id,record) VALUES ('rule',?1)",
                    [serde_json::json!({
                        "id":"rule","agent":"owner","epoch":1,"status":"active",
                        "kind":"interval","inFlight":false,"nextAt":130.0
                    })
                    .to_string()],
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("persist deadline fixtures");
        assert_eq!(
            runtime.next_deadline().expect("read deadlines"),
            Some(130.0)
        );
    }

    #[test]
    fn recovery_pending_rule_does_not_leave_a_past_deadline_hot() {
        let (_root, store, port) = fixture();
        store
            .transaction_with(|tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES ('owner',?1)",
                    [serde_json::json!({
                        "id":"owner","rootId":"owner","epoch":1,"status":"interrupted",
                        "autoWake":true,"accountKey":"default","threadId":"thread",
                        "nativeFailureHold":{"reason":"pending"},
                        "disconnectRecovery":{"epoch":1,"autoWake":true,
                            "accountKey":"default","threadId":"thread"}
                    })
                    .to_string()],
                )?;
                tx.execute(
                    "INSERT INTO runtime_rules(id,record) VALUES ('rule',?1)",
                    [serde_json::json!({
                        "id":"rule","agent":"owner","epoch":1,"status":"active",
                        "kind":"interval","inFlight":false,"nextAt":1.0
                    })
                    .to_string()],
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("persist recovery-pending rule fixture");
        let runtime = MonitorRuntime::new(store, port, Arc::new(Notify::new()));
        assert_eq!(
            runtime.next_deadline().expect("read deadlines"),
            None,
            "owner recovery keeps the rule ineligible until a committed wake"
        );
    }

    #[test]
    fn empty_native_failure_hold_matches_store_truthiness() {
        let (_root, store, port) = fixture();
        store
            .transaction_with(|tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES ('owner',?1)",
                    [serde_json::json!({
                        "id":"owner","rootId":"owner","epoch":1,"status":"ready",
                        "autoWake":true,"accountKey":"default","threadId":"thread",
                        "nativeFailureHold":{},
                        "disconnectRecovery":{"epoch":1,"autoWake":true,
                            "accountKey":"default","threadId":"thread"}
                    })
                    .to_string()],
                )?;
                tx.execute(
                    "INSERT INTO runtime_rules(id,record) VALUES ('rule',?1)",
                    [serde_json::json!({
                        "id":"rule","agent":"owner","epoch":1,"status":"active",
                        "kind":"interval","inFlight":false,"nextAt":1.0
                    })
                    .to_string()],
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("persist empty-hold fixture");
        let runtime = MonitorRuntime::new(store, port, Arc::new(Notify::new()));
        assert_eq!(
            runtime.next_deadline().expect("read deadlines"),
            Some(1.0),
            "empty objects are false under the store owner contract"
        );
    }

    #[test]
    fn omitted_monitor_stall_timeout_uses_the_policy_default_deadline() {
        let (_root, store, port) = fixture();
        insert_active_owner(&store);
        store
            .transaction_with(|tx| {
                tx.execute(
                    "INSERT INTO runtime_monitors(id,record) VALUES ('legacy-monitor',?1)",
                    [serde_json::json!({
                        "id":"legacy-monitor","agent":"owner","epoch":0,
                        "status":"running","created":100.0,"activityAt":100.0,
                        "activityGeneration":1,"stallWakeGeneration":0
                    })
                    .to_string()],
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("persist legacy monitor fixture");
        let runtime = MonitorRuntime::new(store, port, Arc::new(Notify::new()));
        assert_eq!(
            runtime.next_deadline().expect("read deadlines"),
            Some(100.0 + crate::monitor_policy::DEFAULT_MONITOR_STALL_SECONDS as f64)
        );
    }

    #[test]
    fn unstarted_stall_probe_state_does_not_leave_a_past_deadline_hot() {
        let (_root, store, port) = fixture();
        insert_active_owner(&store);
        store
            .transaction_with(|tx| {
                tx.execute(
                    "INSERT INTO runtime_monitors(id,record) VALUES ('legacy-probe',?1)",
                    [serde_json::json!({
                        "id":"legacy-probe","agent":"owner","epoch":0,
                        "status":"running","created":100.0,"activityAt":100.0,
                        "activityGeneration":1,"stallWakeGeneration":0,
                        "stallProbeGeneration":1,"stallTimeoutSeconds":50,
                        "livenessCommand":"true"
                    })
                    .to_string()],
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("persist legacy unstarted-probe state");
        let runtime = MonitorRuntime::new(store, port, Arc::new(Notify::new()));
        assert_eq!(
            runtime.next_deadline().expect("read legacy probe deadline"),
            None,
            "the policy treats a missing probe start as an unknown in-flight outcome"
        );
    }

    #[test]
    fn omitted_file_rule_stall_timeout_uses_the_policy_default_deadline() {
        let (_root, store, port) = fixture();
        insert_active_owner(&store);
        store
            .transaction_with(|tx| {
                tx.execute(
                    "INSERT INTO runtime_rules(id,record) VALUES ('legacy-file-rule',?1)",
                    [serde_json::json!({
                        "id":"legacy-file-rule","agent":"owner","epoch":0,
                        "status":"active","kind":"file","inFlight":false,
                        "created":100.0,"nextAt":5000.0,"fileGeneration":1,
                        "stallWakeGeneration":0
                    })
                    .to_string()],
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("persist legacy file-rule fixture");
        let runtime = MonitorRuntime::new(store, port, Arc::new(Notify::new()));
        assert_eq!(
            runtime.next_deadline().expect("read deadlines"),
            Some(100.0 + crate::rules_engine::DEFAULT_RULE_STALL_SECONDS as f64)
        );
    }

    #[test]
    fn low_worker_deadline_uses_low_since_and_strict_duration_boundary() {
        let (_root, store, port) = fixture();
        insert_active_owner(&store);
        store
            .transaction_with(|tx| {
                tx.execute(
                    "INSERT INTO runtime_rules(id,record) VALUES ('low-rule',?1)",
                    [serde_json::json!({
                        "id":"low-rule","agent":"owner","epoch":0,"status":"active",
                        "kind":"low_workers","inFlight":false,"nextAt":1.0,
                        "minimumWorkers":1,"durationMinutes":30,"lowSince":100.0,
                        "alerted":false
                    })
                    .to_string()],
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("persist low-worker deadline fixture");
        let runtime = MonitorRuntime::new(store, port, Arc::new(Notify::new()));
        assert_eq!(
            runtime.next_deadline().expect("read low-worker deadline"),
            Some(
                100.0
                    + 30.0 * 60.0
                    + crate::rules_engine::LOW_WORKER_STRICT_DEADLINE_MARGIN_SECONDS
            )
        );
    }

    #[tokio::test(start_paused = true)]
    async fn low_worker_commit_wakes_monitor_without_stealing_registry_wake() {
        let (_root, store, port) = fixture();
        insert_active_owner(&store);
        store
            .transaction_with(|tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES ('worker',?1)",
                    [serde_json::json!({
                        "id":"worker","rootId":"owner","epoch":0,
                        "status":"running","isLead":false
                    })
                    .to_string()],
                )?;
                tx.execute(
                    "INSERT INTO runtime_rules(id,record) VALUES ('low-rule',?1)",
                    [serde_json::json!({
                        "id":"low-rule","agent":"owner","epoch":0,"name":"workers",
                        "status":"active","kind":"low_workers","intervalSeconds":60,
                        "nextAt":1.0,"created":1.0,"command":"","text":"worker alert",
                        "inFlight":false,
                        "minimumWorkers":1,"durationMinutes":1,"lowSince":null,
                        "alerted":false
                    })
                    .to_string()],
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("persist low-worker owner and rule");
        let registry_wake = Arc::new(Notify::new());
        let registry_commit_wake = Arc::clone(&registry_wake);
        store.add_commit_observer(move || registry_commit_wake.notify_one());
        let runtime = Arc::new(MonitorRuntime::new(
            store.clone(),
            port.clone(),
            Arc::clone(&registry_wake),
        ));
        {
            let mut scan = runtime
                .scan
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            scan.recovering_rules = false;
        }
        let registry_initial_waiter = tokio::spawn({
            let registry_wake = Arc::clone(&registry_wake);
            async move { registry_wake.notified().await }
        });
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = super::start(runtime.clone(), shutdown_rx);
        wait_for_calls(&port, 2).await;
        wait_for_completed_ticks(&runtime, 2).await;
        wait_for_cycles(&runtime, 1).await;
        tokio::time::timeout(Duration::from_secs(10), registry_initial_waiter)
            .await
            .expect("registry receives monitor-owned commit wake")
            .expect("initial registry wake task completes");
        assert_eq!(
            runtime.next_deadline().expect("no low-worker deadline"),
            None
        );

        let registry_waiter = tokio::spawn({
            let registry_wake = Arc::clone(&registry_wake);
            async move { registry_wake.notified().await }
        });
        let calls = port.next_calls.load(Ordering::SeqCst);
        let completed_ticks = runtime.loop_activity.completed_ticks.load(Ordering::SeqCst);
        store
            .transaction_with(|tx| {
                tx.execute(
                    "UPDATE runtime_agents SET record=?1 WHERE id='worker'",
                    [serde_json::json!({
                        "id":"worker","rootId":"owner","epoch":0,
                        "status":"completed","isLead":false
                    })
                    .to_string()],
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("commit worker completion");
        wait_for_calls(&port, calls + 1).await;
        wait_for_completed_ticks(&runtime, completed_ticks + 1).await;
        tokio::time::timeout(Duration::from_secs(10), registry_waiter)
            .await
            .expect("registry receives its own commit wake")
            .expect("registry wake task completes");
        let low_since: f64 = store
            .read()
            .expect("read worker rule")
            .query_row(
                "SELECT json_extract(record,'$.lowSince') FROM runtime_rules WHERE id='low-rule'",
                [],
                |row| row.get(0),
            )
            .expect("low-worker observation persists");
        assert_eq!(
            runtime
                .next_deadline()
                .expect("read new low-worker deadline"),
            Some(low_since + 60.0 + crate::rules_engine::LOW_WORKER_STRICT_DEADLINE_MARGIN_SECONDS)
        );
        stop_task(&shutdown_tx, task).await;
    }

    #[tokio::test(start_paused = true)]
    async fn due_low_worker_alert_settles_then_monitor_waits_without_past_next_at_spin() {
        let (_root, store, port) = fixture();
        insert_active_owner(&store);
        let now = crate::event_write::unix_time().expect("read fixture time");
        store
            .transaction_with(|tx| {
                tx.execute(
                    "INSERT INTO runtime_rules(id,record) VALUES ('low-rule',?1)",
                    [serde_json::json!({
                        "id":"low-rule","agent":"owner","epoch":0,"name":"workers",
                        "status":"active","kind":"low_workers","intervalSeconds":60,
                        "nextAt":1.0,"created":1.0,"command":"","text":"worker alert",
                        "inFlight":false,
                        "minimumWorkers":1,"durationMinutes":1,"lowSince":now-120.0,
                        "alerted":false
                    })
                    .to_string()],
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("persist already-due low-worker rule");
        let runtime = Arc::new(MonitorRuntime::new(
            store.clone(),
            port.clone(),
            Arc::new(Notify::new()),
        ));
        {
            let mut scan = runtime
                .scan
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            scan.recovering_rules = false;
        }
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = super::start(runtime.clone(), shutdown_rx);
        wait_for_calls(&port, 2).await;
        wait_for_cycles(&runtime, 1).await;
        let alerted: bool = store
            .read()
            .expect("read settled rule")
            .query_row(
                "SELECT json_extract(record,'$.alerted') FROM runtime_rules WHERE id='low-rule'",
                [],
                |row| row.get(0),
            )
            .expect("alert settles once");
        assert!(alerted);
        assert_eq!(
            runtime.next_deadline().expect("alerted rule has no timer"),
            None
        );
        let ticks = runtime.loop_activity.ticks.load(Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(3600)).await;
        tokio::task::yield_now().await;
        assert_eq!(runtime.loop_activity.ticks.load(Ordering::SeqCst), ticks);
        stop_task(&shutdown_tx, task).await;
    }

    #[test]
    fn missing_cancel_retry_deadline_is_immediately_due() {
        let (_root, store, port) = fixture();
        store
            .transaction_with(|tx| {
                tx.execute(
                    "INSERT INTO runtime_monitors(id,record) VALUES ('cancel',?1)",
                    [serde_json::json!({
                        "id":"cancel","status":"running","cancelRequested":true,
                        "cancelDispatch":{"status":"pending"}
                    })
                    .to_string()],
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("persist pending cancel fixture");
        let runtime = MonitorRuntime::new(store, port, Arc::new(Notify::new()));
        assert_eq!(runtime.next_deadline().expect("read deadlines"), Some(0.0));
    }

    #[test]
    fn filesystem_overflow_requests_a_full_rescan() {
        let event = notify::Event::new(notify::EventKind::Other);
        assert!(super::filesystem_event_needs_rescan(&event));
    }
}
