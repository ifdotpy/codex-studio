//! Retained monitor consumer. The account/session owner supplies the transport;
//! this task owns no provider connection and never repeats an uncertain command.
//! Result recovery precedes stall decisions. Publication and transport ACK follow commit.

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
    wake: Arc<Notify>,
}

impl MonitorRuntime {
    pub fn new(store: Arc<RuntimeStore>, port: Arc<dyn MonitorPort>, wake: Arc<Notify>) -> Self {
        Self {
            store,
            port,
            wake,
            output: MonitorOutput::default(),
            scan: Mutex::new(Scan {
                recovering_rules: true,
                ..Scan::default()
            }),
        }
    }

    /// Caller supplies wall time once per batch; tests can drive time deterministically.
    pub fn tick(&self, now: f64) -> Result<(), MonitorError> {
        let mut scan = self
            .scan
            .try_lock()
            .map_err(|_| MonitorError::Refused("Monitor tick already running"))?;
        // If recovery fails, do not describe a possibly finished command as stalled.
        if monitor_receipts::recover_pending(&self.store, BATCH)? == BATCH {
            self.wake.notify_one();
            return Ok(());
        }
        self.attach_batch(&mut scan, now)?;
        for _ in 0..BATCH {
            let Some(delivery) = self.port.next()? else {
                // Results were drained above. Recover cancellation intents before
                // rule evaluation so an unavailable rule adapter cannot starve them.
                for id in monitor_driver::pending_cancel_ids(&self.store)? {
                    let _response =
                        monitor_driver::cancel(&self.store, self.port.as_ref(), &id, None, now)?;
                }
                let rules = if scan.recovering_rules {
                    rules_tick::recover_batch(&self.store, scan.rule, now, self.port.as_ref())?
                } else {
                    rules_tick::tick_batch(
                        &self.store,
                        scan.rule,
                        now,
                        &rules_tick::LocalRuleFiles,
                        self.port.as_ref(),
                    )?
                };
                scan.rule = rules.next_rowid.unwrap_or(0);
                if rules.next_rowid.is_none() {
                    scan.recovering_rules = false;
                }
                for (id, effect) in monitor_runtime::tick(&self.store, now)? {
                    if let crate::monitor_policy::Effect::Probe {
                        generation,
                        command,
                    } = effect
                    {
                        self.port.probe(&id, generation, &command)?;
                    }
                }
                self.wake.notify_one();
                return Ok(());
            };
            self.deliver(&delivery, now)?;
            // A failed ACK leaves an exact replay, never an instruction to resend.
            self.port.acknowledge(&delivery)?;
        }
        // Drain retained output/results before evaluating output-age stalls.
        Ok(())
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
                    return Err(MonitorError::Refused("Monitor result identity changed"));
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
            )?,
            MonitorEvent::Probe { generation, result } => {
                self.store.transaction_with(|tx| {
                    let (snapshot, actor) = monitor_runtime::snapshot(tx, &delivery.id)?;
                    if snapshot.operation() != Some(&delivery.operation) {
                        return Err(MonitorError::Refused("Monitor probe identity changed"));
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

    /// MonitorInput lacks a caller identity, so fail closed until its schema carries
    /// a stable request_id that can key a saved replay receipt.
    pub fn input_http(&self, _id: &str, _input: &Input, _now: f64) -> Result<Value, MonitorError> {
        Err(MonitorError::Unavailable("monitor-input-request-identity"))
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
                _ = timer.tick() => {
                    let runtime = runtime.clone();
                    tokio::task::spawn_blocking(move || runtime.tick(event_write::unix_time()?))
                        .await.map_err(|_| MonitorError::Refused("Monitor runtime task failed"))??;
                }
            }
        }
    })
}
