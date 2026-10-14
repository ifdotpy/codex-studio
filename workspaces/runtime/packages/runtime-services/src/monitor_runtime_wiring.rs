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

use std::sync::{Arc, Mutex};
use std::time::Duration;

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
use crate::{event_write, monitor_runtime, rules_tick};

const BATCH: usize = 64;
const TICK_INTERVAL: Duration = Duration::from_secs(1);

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
    rule: i64,
    recovering_rules: bool,
}

pub struct MonitorRuntime {
    store: Arc<RuntimeStore>,
    port: Arc<dyn MonitorPort>,
    output: MonitorOutput,
    scan: Mutex<Scan>,
    dispatch_wake: Arc<Notify>,
    wake: Arc<Notify>,
    last_tick_error: Mutex<Option<String>>,
}

impl MonitorRuntime {
    pub fn new(store: Arc<RuntimeStore>, port: Arc<dyn MonitorPort>, wake: Arc<Notify>) -> Self {
        Self {
            store,
            port,
            wake,
            output: MonitorOutput::default(),
            dispatch_wake: Arc::new(Notify::new()),
            scan: Mutex::new(Scan {
                recovering_rules: true,
                ..Scan::default()
            }),
            last_tick_error: Mutex::new(None),
        }
    }

    /// Handle used by committed lifecycle adapters to wake cancellation dispatch.
    pub fn dispatch_wake_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.dispatch_wake)
    }

    /// Last timer failure, cleared after a successful tick. The host can inspect
    /// this while the consumer retries transient transport and storage failures.
    pub fn last_tick_error(&self) -> Option<String> {
        self.last_tick_error
            .lock()
            .expect("monitor tick diagnostic")
            .clone()
    }

    fn record_runtime_error(&self, error: &MonitorError) {
        *self
            .last_tick_error
            .lock()
            .expect("monitor tick diagnostic") = Some(error.to_string());
    }

    /// Caller supplies wall time once per batch; tests can drive time deterministically.
    pub fn tick(&self, now: f64) -> Result<(), MonitorError> {
        let result = self.tick_inner(now);
        *self
            .last_tick_error
            .lock()
            .expect("monitor tick diagnostic") = result.as_ref().err().map(ToString::to_string);
        result
    }

    fn tick_inner(&self, now: f64) -> Result<(), MonitorError> {
        let mut scan = self
            .scan
            .try_lock()
            .map_err(|_| MonitorError::Refused("Monitor tick already running"))?;
        // If recovery fills its batch, continue other work but defer stall decisions.
        let result_batch_full = monitor_receipts::recover_pending(&self.store, BATCH)? == BATCH;
        self.attach_batch(&mut scan, now)?;
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
            if let Err(error) = self.deliver(&delivery, now) {
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
        match monitor_driver::pending_start_ids(&self.store) {
            Ok(ids) => {
                for id in ids {
                    if let Err(error) = self.port.start_monitor(&id, now) {
                        record_tick_failure(&mut failure, error);
                    }
                }
            }
            Err(error) => record_tick_failure(&mut failure, error),
        }
        match monitor_driver::pending_cancel_ids(&self.store, now) {
            Ok(ids) => {
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
        match rules {
            Ok(rules) => {
                scan.rule = rules.next_rowid.unwrap_or(0);
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
        if drained && !result_batch_full && failure.is_none() {
            match monitor_runtime::tick(&self.store, now) {
                Ok(effects) => {
                    for (id, effect) in effects {
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
        self.wake.notify_one();
        failure.map_or(Ok(()), Err)
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

    fn attach_batch(&self, scan: &mut Scan, now: f64) -> Result<(), MonitorError> {
        let ids = {
            let connection = self.store.read()?;
            let mut query = connection.prepare("SELECT id FROM runtime_monitors WHERE id>?1 AND json_extract(record,'$.status')='lost' AND json_extract(record,'$.reattachRecovery.status')='running' ORDER BY id LIMIT ?2").map_err(studio_runtime_store::StoreError::from)?;
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
        Ok(())
    }

    /// The retained session may call this directly before committing its own cursor.
    pub fn deliver(&self, delivery: &Delivery, now: f64) -> Result<(), MonitorError> {
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
        self.wake.notify_one();
        Ok(())
    }

    pub fn cancel(&self, id: &str, owner: Option<&str>, now: f64) -> Result<Value, MonitorError> {
        let result = monitor_driver::cancel(&self.store, self.port.as_ref(), id, owner, now)?;
        self.wake.notify_one();
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

/// P01 owns this task and the retained transport. Signal shutdown and await the
/// returned handle before closing the store; do not abort a commit in progress.
pub fn start(
    runtime: Arc<MonitorRuntime>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<Result<(), MonitorError>> {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(TICK_INTERVAL);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return Ok(()); }
                }
                _ = runtime.dispatch_wake.notified() => {
                    let tick_runtime = runtime.clone();
                    match studio_runtime_process::blocking_pool::spawn_blocking("runtime-services/monitor_runtime_wiring", move || tick_runtime.tick(event_write::unix_time()?)).await {
                        Ok(Err(error)) if error.store_failure_for_task() => return Err(error),
                        Ok(Err(_)) => {},
                        Ok(Ok(())) => {},
                        Err(_) => runtime.record_runtime_error(&MonitorError::TaskFailed),
                    }
                }
                _ = timer.tick() => {
                    let tick_runtime = runtime.clone();
                    match studio_runtime_process::blocking_pool::spawn_blocking("runtime-services/monitor_runtime_wiring", move || tick_runtime.tick(event_write::unix_time()?)).await {
                        Ok(Err(error)) if error.store_failure_for_task() => return Err(error),
                        Ok(Err(_)) => {},
                        Ok(Ok(())) => {},
                        Err(_) => runtime.record_runtime_error(&MonitorError::TaskFailed),
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::record_tick_failure;
    use crate::monitor_policy::MonitorError;

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
}
