//! Recovery adapters used by startup reconciliation and the owning account worker.
//! Reads and payload resolution finish outside the lifecycle transaction. The
//! transaction rechecks stored scope and the worker's cached observation; publication
//! is committed with the rows. No recovery port can submit native input.
use crate::{
    recovery::{RecoveryError, RecoveryOutcome, recover_connection, recover_turn},
    recovery_absence::RecoveryObservation,
    recovery_history::{
        ConnectionScope, HistoryError, HistoryMethod, HistoryReceipt, RecoveryReader,
    },
    recovery_policy::RecoverySnapshot,
    recovery_store::{CompletionWriter, RecoveryStore},
    task_recovery::{TaskRecoverySnapshot, recover_task},
    task_recovery_store::{TaskCompletionWriter, TaskRecoveryStore},
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use studio_runtime_store::RuntimeStore;

/// The account actor drives this read-only port while continuing to drain its
/// sole retained session. Both calls must use a timer for this absolute deadline
/// and cancellation, including queue admission. A barrier completes only after
/// preceding notifications and their store commits. Neither call opens a session.
pub trait RetainedHistory {
    fn request(
        &mut self,
        scope: &ConnectionScope,
        method: HistoryMethod,
        params: Value,
        deadline: Instant,
    ) -> Result<HistoryReceipt, HistoryError>;
    fn barrier(&mut self, scope: &ConnectionScope, deadline: Instant) -> Result<(), HistoryError>;
}

/// Worker-owned cached observation. Must not wait, read files, or query a provider;
/// it is also called under the writer transaction. A stopped/replaced connection
/// returns None. The worker serializes stop/generation changes with commits.
pub type ObservationSource<'a> = dyn Fn(&ConnectionScope) -> Option<RecoveryObservation> + 'a;

pub struct RetainedRecoveryReader<'a> {
    scope: ConnectionScope,
    thread: String,
    port: &'a mut dyn RetainedHistory,
    observation: &'a ObservationSource<'a>,
}
impl<'a> RetainedRecoveryReader<'a> {
    pub fn new(
        scope: ConnectionScope,
        thread: String,
        port: &'a mut dyn RetainedHistory,
        observation: &'a ObservationSource<'a>,
    ) -> Result<Self, HistoryError> {
        if scope.account.is_empty()
            || scope.connection.is_empty()
            || scope.generation == 0
            || thread.is_empty()
        {
            return Err(HistoryError::Identity);
        }
        Ok(Self {
            scope,
            thread,
            port,
            observation,
        })
    }
    fn current(&self, scope: &ConnectionScope) -> Result<RecoveryObservation, HistoryError> {
        if *scope != self.scope {
            return Err(HistoryError::Identity);
        }
        (self.observation)(scope)
            .filter(|o| o.valid(scope))
            .ok_or(HistoryError::Identity)
    }
    fn ready(&self, scope: &ConnectionScope, deadline: Instant) -> Result<(), HistoryError> {
        if Instant::now() >= deadline {
            return Err(HistoryError::Deadline);
        }
        self.current(scope).map(|_| ())
    }
}
impl RecoveryReader for RetainedRecoveryReader<'_> {
    fn observation(
        &mut self,
        scope: &ConnectionScope,
    ) -> Result<RecoveryObservation, HistoryError> {
        self.current(scope)
    }
    fn read(
        &mut self,
        scope: &ConnectionScope,
        method: HistoryMethod,
        params: Value,
        deadline: Instant,
    ) -> Result<HistoryReceipt, HistoryError> {
        self.ready(scope, deadline)?;
        if params["threadId"] != self.thread
            || (method == HistoryMethod::Thread && params["includeTurns"] != false)
        {
            return Err(HistoryError::Identity);
        }
        let receipt = self.port.request(scope, method, params, deadline)?;
        self.ready(scope, deadline)?;
        if receipt.body.to_string().len() > crate::recovery_history::MAX_RESPONSE_BYTES {
            return Err(HistoryError::Capacity);
        }
        if receipt.scope != *scope {
            return Err(HistoryError::Identity);
        }
        Ok(receipt)
    }
    fn barrier(&mut self, scope: &ConnectionScope, deadline: Instant) -> Result<(), HistoryError> {
        self.ready(scope, deadline)?;
        self.port.barrier(scope, deadline)?;
        self.ready(scope, deadline)?;
        if !self.current(scope)?.journal_drained {
            return Err(HistoryError::Unavailable);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub enum RecoveryJob {
    Turn(RecoverySnapshot),
    Connection {
        snapshot: RecoverySnapshot,
        automatic: bool,
    },
    Task(TaskRecoverySnapshot),
}
impl RecoveryJob {
    fn agent(&self) -> &RecoverySnapshot {
        match self {
            Self::Turn(s) | Self::Connection { snapshot: s, .. } => s,
            Self::Task(t) => t.agent(),
        }
    }
    fn key(&self) -> String {
        let kind = match self {
            Self::Turn(_) => "turn",
            Self::Connection { .. } => "connection",
            Self::Task(_) => "task",
        };
        let a = self.agent().record();
        let task = match self {
            Self::Task(t) => Some(t.task()["id"].clone()),
            _ => None,
        };
        // Diagnostic timestamps change after each probe; they must not reset
        // pacing. Only durable operation identity creates a new check.
        studio_runtime_store::sha256_hex(
            serde_json::json!([
                kind,
                a["id"],
                a["epoch"],
                a["accountKey"],
                a["threadId"],
                a["turnId"],
                a["startAttempt"]["id"],
                a["disconnectRecovery"]["connectionId"],
                a["disconnectRecovery"]["at"],
                a["restartRecovery"]["at"],
                task
            ])
            .to_string()
            .as_bytes(),
        )
    }
}

/// Cursor alternates agent and task pages, so neither a large account nor a
/// permanently held task can starve later rows. It is disposable pacing state.
#[derive(Default)]
pub struct RecoveryCursor {
    agents: String,
    tasks: String,
    task_page: bool,
}
pub fn load_recovery_jobs(
    store: &RuntimeStore,
    account: &str,
    cursor: &mut RecoveryCursor,
) -> Result<Vec<RecoveryJob>, RecoveryError> {
    let db = store.read()?;
    let mut jobs = Vec::new();
    if cursor.task_page {
        let mut stmt=db.prepare("SELECT t.id,t.record,a.record FROM runtime_tasks t JOIN runtime_agents a ON a.id=json_extract(t.record,'$.agent') WHERE COALESCE(json_extract(a.record,'$.accountKey'),'default')=?1 AND t.id>?2 AND json_extract(t.record,'$.status')='running' AND json_extract(t.record,'$.type') IN ('commandExecution','fileChange','dynamicToolCall','mcpToolCall') ORDER BY t.id LIMIT ?3")?;
        let rows = stmt
            .query_map(
                rusqlite::params![account, cursor.tasks, MAX_SCAN_JOBS as i64],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let full = rows.len() == MAX_SCAN_JOBS;
        for (id, task, agent) in rows {
            let task: Value =
                serde_json::from_str(&task).map_err(|_| invalid_record("runtime_tasks", &id))?;
            let agent: Value =
                serde_json::from_str(&agent).map_err(|_| invalid_record("runtime_agents", &id))?;
            let (aliases, receipts) =
                crate::task_recovery_store::receipt_state(&db, &agent, &task)?;
            let snapshot = TaskRecoverySnapshot::from_records(agent, task, aliases, receipts)?;
            cursor.tasks = id;
            if snapshot.agent().eligibility().task_owner {
                jobs.push(RecoveryJob::Task(snapshot));
            }
        }
        if !full {
            cursor.tasks.clear();
        }
    } else {
        let mut stmt=db.prepare("SELECT id,record FROM runtime_agents WHERE COALESCE(json_extract(record,'$.accountKey'),'default')=?1 AND id>?2 ORDER BY id LIMIT ?3")?;
        let rows = stmt
            .query_map(
                rusqlite::params![account, cursor.agents, MAX_SCAN_JOBS as i64],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let full = rows.len() == MAX_SCAN_JOBS;
        for (id, raw) in rows {
            let snapshot = RecoverySnapshot::from_record(
                serde_json::from_str(&raw).map_err(|_| invalid_record("runtime_agents", &id))?,
            )?;
            let e = snapshot.eligibility();
            cursor.agents = id;
            if e.connection {
                jobs.push(RecoveryJob::Connection {
                    snapshot,
                    automatic: true,
                });
            } else if e.unconfirmed_start || e.orphan_busy || snapshot.record()["inFlight"] == true
            {
                jobs.push(RecoveryJob::Turn(snapshot));
            }
        }
        if !full {
            cursor.agents.clear();
        }
    }
    cursor.task_page = !cursor.task_page;
    Ok(jobs)
}

/// The startup caller and periodic account caller use the same entry. The owner
/// runs it on its blocking boundary, never while holding the SQLite transaction.
/// Recovery is not an ACK: the owner publishes/wakes only after this returns.
pub struct RecoveryRuntime<'a> {
    pub store: &'a RuntimeStore,
    pub observation: &'a ObservationSource<'a>,
    pub completion: &'a mut dyn CompletionWriter,
    pub task_completion: &'a mut dyn TaskCompletionWriter,
}
impl RecoveryRuntime<'_> {
    pub fn reconcile(
        &mut self,
        job: &RecoveryJob,
        scope: ConnectionScope,
        reader: &mut dyn RecoveryReader,
    ) -> Result<RecoveryOutcome, RecoveryError> {
        let Some(observation) = (self.observation)(&scope).filter(|o| o.valid(&scope)) else {
            return Ok(RecoveryOutcome::Superseded);
        };
        let current = |s: &ConnectionScope| (self.observation)(s).is_some_and(|o| o.valid(s));
        match job {
            RecoveryJob::Task(snapshot) => {
                let changed = recover_task(
                    snapshot,
                    scope,
                    reader,
                    &mut TaskRecoveryStore {
                        store: self.store,
                        observation_current: self.observation,
                        completion: self.task_completion,
                    },
                )?;
                Ok(if changed {
                    RecoveryOutcome::Applied(serde_json::json!({"status":"checked"}))
                } else {
                    RecoveryOutcome::Unconfirmed
                })
            }
            _ => {
                let mut effects = RecoveryStore {
                    store: self.store,
                    connection_current: &current,
                    completion: self.completion,
                    at: observation.at,
                    observation_current: self.observation,
                };
                match job {
                    RecoveryJob::Turn(snapshot) => {
                        recover_turn(snapshot, scope, reader, &mut effects)
                    }
                    RecoveryJob::Connection {
                        snapshot,
                        automatic,
                    } => recover_connection(snapshot, scope, reader, &mut effects, *automatic),
                    RecoveryJob::Task(_) => unreachable!("task handled above"),
                }
            }
        }
    }
}

pub const TASK_SCAN_INTERVAL: Duration = Duration::from_secs(30);
pub const MAX_SCAN_JOBS: usize = 32;
const MAX_CHECKS: usize = 256;
const UNKNOWN_START_QUIET_SECONDS: f64 = 10.0;
const TURN_QUIET_SECONDS: f64 = 120.0;
const TURN_CHECK_INTERVAL: Duration = Duration::from_secs(60);
const CONNECTION_RETRY_BASE: u64 = 15;
const CONNECTION_RETRY_MAX: u64 = 300;

struct Check {
    at: Instant,
    next: Instant,
    failures: u32,
}
/// One instance per account worker. The owner keeps at most one recovery job in
/// flight, calls finish on every outcome, and schedules its next timer from next_at.
/// Restart drops pacing only; durable rows remain the recovery authority.
#[derive(Default)]
pub struct RecoveryScan {
    checks: BTreeMap<String, Check>,
    active: Option<String>,
}
impl RecoveryScan {
    pub fn take(
        &mut self,
        jobs: impl IntoIterator<Item = RecoveryJob>,
        now: Instant,
        at: f64,
    ) -> Option<RecoveryJob> {
        if self.active.is_some() || !at.is_finite() || at < 0.0 {
            return None;
        }
        let job = jobs
            .into_iter()
            .take(MAX_SCAN_JOBS)
            .filter(|j| due(j, at))
            .filter(|j| self.checks.get(&j.key()).is_none_or(|c| c.next <= now))
            .min_by_key(|j| {
                (
                    !j.agent().eligibility().unconfirmed_start,
                    self.checks.get(&j.key()).map(|c| c.at),
                )
            })?;
        self.active = Some(job.key());
        Some(job)
    }
    pub fn finish(
        &mut self,
        job: &RecoveryJob,
        now: Instant,
        outcome: &Result<RecoveryOutcome, RecoveryError>,
    ) -> Result<(), RecoveryError> {
        let key = job.key();
        if self.active.as_ref() != Some(&key) {
            return Err(RecoveryError::CommitFailed);
        }
        let restored = matches!(outcome, Ok(RecoveryOutcome::Applied(v)) if matches!(v["status"].as_str(), Some("adopted"|"reconciled"|"input_restored")));
        let failures = if restored {
            0
        } else {
            self.checks
                .get(&key)
                .map_or(1, |c| c.failures.saturating_add(1).min(6))
        };
        let delay = match job {
            RecoveryJob::Connection { .. } if restored => Duration::ZERO,
            RecoveryJob::Connection { .. } => Duration::from_secs(
                (CONNECTION_RETRY_BASE * (1 << failures.saturating_sub(1)))
                    .min(CONNECTION_RETRY_MAX),
            ),
            RecoveryJob::Task(_) => TASK_SCAN_INTERVAL,
            RecoveryJob::Turn(_) => TURN_CHECK_INTERVAL,
        };
        self.checks.insert(
            key,
            Check {
                at: now,
                next: now + delay,
                failures,
            },
        );
        self.active = None;
        if self.checks.len() > MAX_CHECKS
            && let Some(oldest) = self
                .checks
                .iter()
                .min_by_key(|(_, c)| c.at)
                .map(|(k, _)| k.clone())
        {
            self.checks.remove(&oldest);
        }
        Ok(())
    }
    pub fn next_at(&self, now: Instant) -> Option<Instant> {
        self.checks
            .values()
            .map(|c| c.next)
            .filter(|next| *next > now)
            .min()
    }
}

fn due(job: &RecoveryJob, at: f64) -> bool {
    let a = job.agent().record();
    let e = job.agent().eligibility();
    match job {
        RecoveryJob::Connection { .. } => e.connection,
        RecoveryJob::Task(t) => e.task_owner && t.task()["status"] == "running",
        RecoveryJob::Turn(_) => {
            if !e.unconfirmed_start && (a["inFlight"] != true || a["deletedAt"].is_string()) {
                return false;
            }
            let mut activity = a["activity"]["at"]
                .as_f64()
                .filter(|v| *v != 0.)
                .or_else(|| a["created"].as_f64())
                .unwrap_or(at);
            if let Some(last) = a["lastEvent"]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            {
                activity = activity.max(last.timestamp_millis() as f64 / 1000.);
            }
            if e.unconfirmed_start {
                activity = a["startAttempt"]["created"].as_f64().unwrap_or(activity);
            }
            at - activity
                >= if e.unconfirmed_start {
                    UNKNOWN_START_QUIET_SECONDS
                } else {
                    TURN_QUIET_SECONDS
                }
        }
    }
}

fn invalid_record(table: &'static str, id: &str) -> RecoveryError {
    studio_runtime_store::StoreError::InvalidStoredObject {
        table,
        id: id.into(),
    }
    .into()
}
