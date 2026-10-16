mod bootstrap;
mod feature_schema;
mod fresh_state;
mod multi_server_state_check;
mod path;
mod records;
mod repositories;
mod schema;
mod sqlite_actor;
pub mod supervisor_journal;
pub use schema::{
    python_float_json, python_json, python_json_compact, python_json_utf8,
    python_json_with_separators, sha256_hex, truthy,
};
mod state_check;
pub use state_check::{StateCheckFailure, StateCheckReport, StateCheckTable, check_state_readonly};

pub use repositories::account_state;
pub use repositories::account_transfer::{
    AccountTransferRecord, AccountTransferRepository, MAX_TRANSFER_RECORD_BYTES,
};
pub use repositories::agent_management;
pub use repositories::analytics::{
    self, AnalyticsError, AnalyticsOptions, AnalyticsRepository, AnalyticsStoreError,
    SessionCostError,
};
pub use repositories::analytics_capture;
pub use repositories::budget;
pub use repositories::capability_reads::CapabilityReadsRepository;
pub use repositories::checkpoints::{
    CaptureKind, CheckpointsRepository, StoredCheckpointObject, WorkspaceSnapshot,
};
pub use repositories::claude_controls::ClaudeControlRepository;
pub use repositories::cost_usage::{CostUsageRepositoryError, CostUsageRow, read_cost_usage};
pub use repositories::remote_transport::{
    AccessNonceOutcome, OutboundReservation, RemoteTransportRepository,
};
pub use repositories::system_reads;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::BufReader;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::trace::{TraceEvent, TraceEventCodes};
pub use rusqlite::{Connection, Error as SqliteError, ErrorCode, Transaction};
use rusqlite::{OpenFlags, TransactionBehavior};
use thiserror::Error;
use tokio::sync::OwnedSemaphorePermit;

type BoxedTransactionOperation<T, E> =
    Box<dyn for<'transaction> FnOnce(&Transaction<'transaction>) -> Result<T, E> + Send + 'static>;

use sqlite_actor::{ActorResponseDisposition, SqliteActor};

pub use path::resolve_state_dir;
pub use records::{
    AgentId, AgentRecord, AgentRepository, AgentStatus, AnnotationRecord, ChatMessage,
    ChatMessageId, CheckpointRecord, ComplaintRecord, EventMetadataRecord, ExecutionRunRecord,
    GraphAgentRecord, JsonRecord, MessageRepository, MonitorRecord, PlanRecord, QueueEvent,
    QueueEventId, QueueEventRepository, RecordKeyMode, RecordOrder, RecordPredicate,
    RecordQueryShape, RequestRecord, RuleRecord, Table, TaskId, TaskRecord, TaskRepository,
    TerminalRecord, TranscriptAssetId, TranscriptAssetRecord, TranscriptItemId,
    TranscriptItemRecord, UsageResumeRecord, WorkItemRecord, WorkStatus, WorkspaceOperationRecord,
};
pub use repositories::agent_move::{AgentMoveRepository, MoveRecord};
pub use repositories::claude_retry::{ClaudeRetryRecords, ClaudeRetryRepository};
pub use repositories::context_repair::ContextRepairRepository;
pub use repositories::continuation::{ContinuationRepository, ContinuationTable};
pub use repositories::coordination::{CanvasMessage, ChatGroup, CoordinationRepository};
pub use repositories::event_write::{
    EventWriteReceipt, EventWriteRepository, INVALID_AGENT_HOLD_RECEIPT_PREFIX,
    INVALID_AGENT_HOLD_RECEIPT_SIGNATURE,
};
pub use repositories::execution_history::ExecutionHistoryRepository;
pub use repositories::federation::FederationRepository;
pub use repositories::federation_credentials::FederationCredentialsRepository;
pub use repositories::federation_messaging;
pub use repositories::monitor_runtime;
pub use repositories::monitors::{MonitorsRepository, StoredMonitor};
pub use repositories::multi_server::MultiServerRepository;
pub use repositories::notification_dispatch::NotificationDispatchRepository;
pub use repositories::profiles::{ProfileRecord, ProfileRepository, ProfileRole, ReasoningEffort};
pub use repositories::project_locations::{ProjectLocationRecord, ProjectLocationsRepository};
pub use repositories::projects::ProjectsRepository;
pub use repositories::questions_reads::QuestionsReadsRepository;
pub use repositories::questions_write::QuestionsWriteRepository;
pub use repositories::queue_write::{PendingEvent, QueueWriteRepository};
pub use repositories::reviewer_archive::ReviewerArchiveRepository;
pub use repositories::rules::RulesRepository;
pub use repositories::rules_tick;
pub use repositories::scheduler::{
    AccountWakePage, ClaimedEvents, SchedulerRepository, TurnAttemptIdentity,
};
pub use repositories::sync_documents::{
    EntityPage, EntityRow, SyncDocument, SyncDocumentsRepository,
};
pub use repositories::terminals::{TerminalRow, TerminalsRepository};
pub use repositories::transcript::{
    TranscriptItem, TranscriptRepository, TranscriptSearchKind, TranscriptStoreError,
    TranscriptStoreResult,
};
pub use repositories::usage::{
    UsageRepositoryError, cancel_capacity_retry, cancel_usage_resume, disable_usage_resume,
};
pub use repositories::voice::{VoiceDelivery, VoiceRecord, VoiceRepository};
pub use repositories::work_items::{StoredQueueEvent, StoredRecord, WorkItemsRepository};
pub use repositories::workspace_files::{StoredWorkspaceAsset, WorkspaceFilesRepository};
pub use repositories::workspace_operations;
pub use repositories::workspace_ready;
pub use repositories::workspace_views::{WorkspaceTaskRow, WorkspaceViewsRepository};

const DATABASE_BUSY_TIMEOUT_MS: u32 = 15_000;
/// A Rust repository-owned index whose SQL is shared by bootstrap and the
/// fresh-state cross-language schema parity check.
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedRuntimeIndexDefinition {
    pub name: &'static str,
    pub table: &'static str,
    pub create_sql: String,
}

/// Maximum interval between maintenance checkpoints; the unit is elapsed seconds.
pub const WAL_CHECKPOINT_CADENCE: Duration = Duration::from_secs(60);
/// WAL size that permits an early maintenance checkpoint; the unit is bytes.
const WAL_CHECKPOINT_THRESHOLD_BYTES: u64 = 4 * 1024 * 1024;
/// Maximum total Canvas reader connections, including the dedicated version reader.
const CANVAS_READER_CONNECTION_CAPACITY: usize = 4;
/// Reader connections reserved for the dedicated `PRAGMA data_version` query.
const VERSION_READER_CONNECTION_RESERVATION: usize = 1;
/// Maximum ordinary checked-out Canvas read connections after the version-reader reservation.
pub const READ_CONNECTION_POOL_CAPACITY: usize =
    CANVAS_READER_CONNECTION_CAPACITY - VERSION_READER_CONNECTION_RESERVATION;

/// Canvas tables with process-local observers for committed row changes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ObservedTable {
    AccessClients,
    UsageResumes,
}

impl ObservedTable {
    fn name(self) -> &'static str {
        match self {
            Self::AccessClients => "runtime_access_clients",
            Self::UsageResumes => "runtime_usage_resumes",
        }
    }

    fn change_bit(self) -> u8 {
        match self {
            Self::AccessClients => 1 << 0,
            Self::UsageResumes => 1 << 1,
        }
    }
}

const OBSERVED_TABLES: [ObservedTable; 2] =
    [ObservedTable::AccessClients, ObservedTable::UsageResumes];

/// Value-free observations emitted by the optional diagnostics callback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreDiagnosticsEvent {
    WriterAdmission {
        wait: Duration,
        waited: bool,
        waiting_threads: usize,
        peak_waiters: usize,
    },
    WriterWaiters {
        waiting_threads: usize,
        peak_waiters: usize,
    },
    ReadAdmission {
        wait: Duration,
        waited: bool,
        waiting_threads: usize,
        peak_waiters: usize,
        opened_connections: usize,
    },
    ReadWaiters {
        waiting_threads: usize,
        peak_waiters: usize,
        opened_connections: usize,
    },
    WriterTransaction {
        wait: Duration,
        hold: Duration,
    },
    SqlProfile {
        database: StoreDatabaseKind,
        elapsed: Duration,
    },
    CommitDuration {
        database: StoreDatabaseKind,
        elapsed: Duration,
    },
    WalCheckpoint {
        database: StoreDatabaseKind,
        wal_size_bytes: u64,
        busy: i64,
        log_frames: i64,
        checkpointed_frames: i64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreDatabaseKind {
    Canvas,
    Analytics,
    Other,
}

/// The callback must be nonblocking; diagnostics collectors should use a
/// try-lock and discard the sample if contended.
pub type StoreDiagnosticsCallback = fn(StoreDiagnosticsEvent);

static DIAGNOSTICS_CALLBACK: OnceLock<Mutex<Option<StoreDiagnosticsCallback>>> = OnceLock::new();
static DIAGNOSTICS_ENABLED: AtomicBool = AtomicBool::new(false);

fn diagnostics_callback() -> &'static Mutex<Option<StoreDiagnosticsCallback>> {
    DIAGNOSTICS_CALLBACK.get_or_init(|| Mutex::new(None))
}

fn emit_diagnostic(event: StoreDiagnosticsEvent) {
    let callback = match diagnostics_callback().try_lock() {
        Ok(callback) => *callback,
        Err(std::sync::TryLockError::Poisoned(error)) => *error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => None,
    };
    if let Some(callback) = callback {
        callback(event);
    }
}

fn fire_commit_observers(notifiers: &Mutex<CommitNotifiers>) {
    let (notifier, observers) = {
        let notifiers = notifiers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (notifiers.primary.clone(), notifiers.observers.clone())
    };
    if let Some(notifier) = notifier {
        notifier();
    }
    for observer in observers {
        observer();
    }
}

fn fire_table_change_observers(
    observers: &Mutex<HashMap<ObservedTable, Vec<Arc<dyn Fn() + Send + Sync>>>>,
    changed_tables: u8,
) {
    for table in OBSERVED_TABLES {
        if changed_tables & table.change_bit() == 0 {
            continue;
        }
        let callbacks = observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&table)
            .cloned()
            .unwrap_or_default();
        for callback in callbacks {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback()));
        }
    }
}

fn install_table_change_hooks(
    connection: &Connection,
    table_change_flags: Arc<AtomicU8>,
) -> rusqlite::Result<()> {
    connection.update_hook(Some(
        move |_action: rusqlite::hooks::Action,
              database_name: &str,
              table_name: &str,
              _rowid: i64| {
            if database_name != "main" {
                return;
            }
            let Some(bit) = OBSERVED_TABLES
                .iter()
                .find(|observed| observed.name() == table_name)
                .map(|observed| observed.change_bit())
            else {
                return;
            };
            table_change_flags.fetch_or(bit, Ordering::Relaxed);
        },
    ))?;
    connection.authorizer(Some(|context: AuthContext<'_>| match context.action {
        AuthAction::Delete { table_name }
            if context.database_name == Some("main")
                && OBSERVED_TABLES
                    .iter()
                    .any(|observed| observed.name() == table_name) =>
        {
            // SQLite documents IGNORE here as retaining DELETE semantics while
            // disabling the truncate optimization, so update_hook sees rows.
            Authorization::Ignore
        }
        _ => Authorization::Allow,
    }))
}

fn verify_observed_tables_are_rowid(connection: &Connection) -> StoreResult<()> {
    for table in OBSERVED_TABLES {
        let without_rowid = connection.query_row(
            "SELECT wr FROM pragma_table_list WHERE schema = 'main' AND name = ?1",
            [table.name()],
            |row| row.get::<_, i64>(0),
        )?;
        if without_rowid != 0 {
            return Err(StoreError::SchemaMismatch(format!(
                "Canvas table {} must use rowid storage for change notifications",
                table.name()
            )));
        }
    }
    Ok(())
}

fn profile_statement(database: StoreDatabaseKind, event: TraceEvent<'_>) {
    if let TraceEvent::Profile(_statement, elapsed) = event {
        emit_diagnostic(StoreDiagnosticsEvent::SqlProfile { database, elapsed });
    }
}

fn profile_canvas(event: TraceEvent<'_>) {
    profile_statement(StoreDatabaseKind::Canvas, event);
}

fn profile_analytics(event: TraceEvent<'_>) {
    profile_statement(StoreDatabaseKind::Analytics, event);
}

fn profile_other(event: TraceEvent<'_>) {
    profile_statement(StoreDatabaseKind::Other, event);
}

fn install_profile(connection: &mut Connection, database: StoreDatabaseKind) {
    let enabled = diagnostics_callback()
        .try_lock()
        .map(|callback| callback.is_some())
        .unwrap_or(false);
    connection.trace_v2(
        if enabled {
            TraceEventCodes::SQLITE_TRACE_PROFILE
        } else {
            TraceEventCodes::empty()
        },
        enabled.then_some(match database {
            StoreDatabaseKind::Canvas => profile_canvas,
            StoreDatabaseKind::Analytics => profile_analytics,
            StoreDatabaseKind::Other => profile_other,
        }),
    );
}

struct IdentityEntry {
    server_id: Option<String>,
    stores: usize,
}

/// Keeps the process-local identity registered until every queued writer
/// callback that can consult it has finished, even if its RuntimeStore is gone.
struct IdentityRegistryLease {
    path: PathBuf,
}

impl Drop for IdentityRegistryLease {
    fn drop(&mut self) {
        let Ok(mut registry) = identity_registry().lock() else {
            return;
        };
        if let Some(entry) = registry.get_mut(&self.path) {
            entry.stores = entry.stores.saturating_sub(1);
            if entry.stores == 0 {
                registry.remove(&self.path);
            }
        }
    }
}

fn identity_registry() -> &'static Mutex<HashMap<PathBuf, IdentityEntry>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, IdentityEntry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("state directory is unavailable: {0}")]
    StateDirectory(#[from] std::io::Error),
    #[error("another runtime owns the state directory")]
    AlreadyOwned,
    #[error("state database is missing: {0}")]
    MissingDatabase(PathBuf),
    #[error("state directory cannot be inside .claude")]
    UnsafeStateDirectory,
    #[error("database schema or migration marker is incompatible: {0}")]
    SchemaMismatch(String),
    #[error("invalid stored sync object {scope}/{id}")]
    InvalidSyncObject { scope: String, id: String },
    #[error("invalid stored sync object {scope}/{id} at {pointer}: {reason}")]
    InvalidSyncObjectField {
        scope: String,
        id: String,
        pointer: String,
        reason: String,
    },
    #[error("invalid stored object {object}/{id}")]
    InvalidStoredRecord { object: &'static str, id: String },
    #[error("database object {object} in {database} is incompatible: {reason}")]
    SchemaObjectMismatch {
        database: String,
        object: String,
        reason: String,
    },
    #[error("SQLite operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("SQLite commit outcome is unknown: {0}")]
    CommitOutcomeUnknown(rusqlite::Error),
    #[error("stored JSON record is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid stored object {table}/{id}")]
    InvalidStoredObject { table: &'static str, id: String },
    #[error("persisted JSON schema validation failed: {0}")]
    SchemaValidation(String),
    #[error("invalid JSON-record table contract: {0}")]
    RecordContract(&'static str),
    #[error("runtime store writer mutex was poisoned")]
    WriterPoisoned,
    #[error("Canvas SQLite writer actor failed to start: {0}")]
    WriterActorInitialization(tokio_rusqlite::Error),
    #[error("Canvas SQLite writer actor control operation failed: {0}")]
    WriterActorControl(tokio_rusqlite::Error),
    #[error("Canvas SQLite writer response was lost; transaction outcome is unknown: {0}")]
    CommitActorOutcomeUnknown(tokio_rusqlite::Error),
    #[error("nested read checkout from the same runtime store is not supported")]
    NestedRead,
}

pub type StoreResult<T> = Result<T, StoreError>;

/// Convert the actor's transport channel error to the store's typed unknown
/// outcome. `call_raw` only reports a closed channel today; retain a typed close
/// error if a future control path returns one. Application errors are carried
/// inside the response value and must not pass through this conversion.
pub(crate) fn actor_transport_outcome_unknown<E>(error: tokio_rusqlite::Error<E>) -> StoreError {
    let transport = match error {
        tokio_rusqlite::Error::ConnectionClosed => tokio_rusqlite::Error::ConnectionClosed,
        tokio_rusqlite::Error::Close((connection, error)) => {
            tokio_rusqlite::Error::Close((connection, error))
        }
        tokio_rusqlite::Error::Error(_) => tokio_rusqlite::Error::ConnectionClosed,
        _ => tokio_rusqlite::Error::ConnectionClosed,
    };
    StoreError::CommitActorOutcomeUnknown(transport)
}

/// Exclusive process lease compatible with Python's runtime lock on Unix.
pub struct RuntimeLock {
    _file: File,
}

impl RuntimeLock {
    pub fn acquire(state_dir: &Path) -> StoreResult<Self> {
        let lock_path = state_dir.join("runtime.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(StoreError::AlreadyOwned),
            Err(std::fs::TryLockError::Error(error)) => Err(StoreError::StateDirectory(error)),
        }
    }
}

/// Owns the runtime process lock and serializes all Rust writes.
pub struct RuntimeStore {
    root: PathBuf,
    writer: SqliteActor,
    analytics: Arc<Mutex<Connection>>,
    version_reader: Mutex<Connection>,
    read_pool: Arc<ReadConnectionPool>,
    writer_waiters: AtomicUsize,
    writer_waiter_peak: AtomicUsize,
    identity_registry_path: PathBuf,
    identity_registry_lease: Arc<IdentityRegistryLease>,
    commit_notifiers: Arc<Mutex<CommitNotifiers>>,
    table_commit_observers: Arc<Mutex<HashMap<ObservedTable, Vec<Arc<dyn Fn() + Send + Sync>>>>>,
    table_change_flags: Arc<AtomicU8>,
    #[cfg(test)]
    simulate_unknown_commit_after_success: Arc<AtomicBool>,
    /// Count of committed transactions that changed rows; owned and advanced by the writer.
    committed_change_generation: Arc<AtomicU64>,
    last_entity_projection_state: Mutex<Option<(u64, String)>>,
    entity_projection_maintenance_lock: tokio::sync::Mutex<()>,
    wal_checkpoint_requested: Arc<AtomicBool>,
    last_wal_checkpoint: Mutex<Instant>,
}

struct ReadConnectionPool {
    path: PathBuf,
    state: Mutex<ReadConnectionPoolState>,
    available: Condvar,
    waiters: AtomicUsize,
    waiter_peak: AtomicUsize,
}

struct ReadConnectionPoolState {
    idle: Vec<Connection>,
    opened: usize,
}

thread_local! {
    static ACTIVE_READ_POOLS: RefCell<HashSet<usize>> = RefCell::new(HashSet::new());
}

tokio::task_local! {
    static ACCOUNT_WORKER_OPERATION_LEASE: Arc<OwnedSemaphorePermit>;
}

/// Run an account-worker operation under its bridge serialization permit.
/// Canvas writes admitted within this scope retain a clone through their SQL
/// callback, even when the worker stops waiting for the transaction result.
/// A write still waiting for actor admission does not retain a clone and remains
/// cancellable with its caller. Tokio task-local values do not cross
/// `tokio::spawn`; spawned writer work must establish its own scope.
pub fn scope_account_worker_operation<T, O>(
    permit: OwnedSemaphorePermit,
    operation: O,
) -> impl Future<Output = T> + Send
where
    T: Send,
    O: Future<Output = T> + Send,
{
    ACCOUNT_WORKER_OPERATION_LEASE.scope(Arc::new(permit), Box::pin(operation))
}

/// A checked-out read-only canvas connection returned to its bounded pool on drop.
pub struct ReadConnection {
    connection: Option<Connection>,
    pool: Arc<ReadConnectionPool>,
    pool_id: usize,
    _thread_bound: PhantomData<Rc<()>>,
}

impl Deref for ReadConnection {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.connection
            .as_ref()
            .expect("checked-out read connection")
    }
}

impl DerefMut for ReadConnection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.connection
            .as_mut()
            .expect("checked-out read connection")
    }
}

impl Drop for ReadConnection {
    fn drop(&mut self) {
        ACTIVE_READ_POOLS.with(|active| {
            active.borrow_mut().remove(&self.pool_id);
        });
        let Some(connection) = self.connection.take() else {
            return;
        };
        if let Ok(mut state) = self.pool.state.lock() {
            state.idle.push(connection);
            self.pool.available.notify_one();
        }
    }
}

impl ReadConnectionPool {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            state: Mutex::new(ReadConnectionPoolState {
                idle: Vec::new(),
                opened: 0,
            }),
            available: Condvar::new(),
            waiters: AtomicUsize::new(0),
            waiter_peak: AtomicUsize::new(0),
        }
    }

    fn checkout(self: &Arc<Self>) -> StoreResult<ReadConnection> {
        let pool_id = Arc::as_ptr(self) as usize;
        if ACTIVE_READ_POOLS.with(|active| active.borrow().contains(&pool_id)) {
            return Err(StoreError::NestedRead);
        }
        let started = Instant::now();
        let mut waiting = false;
        let mut state = self.state.lock().map_err(|_| StoreError::WriterPoisoned)?;
        loop {
            if let Some(connection) = state.idle.pop() {
                if waiting {
                    let waiting_threads = self.waiters.fetch_sub(1, Ordering::Relaxed) - 1;
                    emit_diagnostic(StoreDiagnosticsEvent::ReadWaiters {
                        waiting_threads,
                        peak_waiters: self.waiter_peak.load(Ordering::Relaxed),
                        opened_connections: state.opened,
                    });
                }
                emit_diagnostic(StoreDiagnosticsEvent::ReadAdmission {
                    wait: started.elapsed(),
                    waited: waiting,
                    waiting_threads: self.waiters.load(Ordering::Relaxed),
                    peak_waiters: self.waiter_peak.load(Ordering::Relaxed),
                    opened_connections: state.opened,
                });
                ACTIVE_READ_POOLS.with(|active| {
                    active.borrow_mut().insert(pool_id);
                });
                return Ok(ReadConnection {
                    connection: Some(connection),
                    pool: Arc::clone(self),
                    pool_id,
                    _thread_bound: PhantomData,
                });
            }
            if state.opened < READ_CONNECTION_POOL_CAPACITY {
                state.opened += 1;
                drop(state);
                let result = open_database(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY).and_then(
                    |connection| {
                        connection.pragma_update(None, "query_only", "ON")?;
                        Ok(connection)
                    },
                );
                if result.is_err()
                    && let Ok(mut state) = self.state.lock()
                {
                    state.opened = state.opened.saturating_sub(1);
                    self.available.notify_one();
                }
                if waiting {
                    let waiting_threads = self.waiters.fetch_sub(1, Ordering::Relaxed) - 1;
                    emit_diagnostic(StoreDiagnosticsEvent::ReadWaiters {
                        waiting_threads,
                        peak_waiters: self.waiter_peak.load(Ordering::Relaxed),
                        opened_connections: self
                            .state
                            .lock()
                            .map(|state| state.opened)
                            .unwrap_or_default(),
                    });
                }
                let opened_connections = self
                    .state
                    .lock()
                    .map(|state| state.opened)
                    .unwrap_or_default();
                emit_diagnostic(StoreDiagnosticsEvent::ReadAdmission {
                    wait: started.elapsed(),
                    waited: waiting,
                    waiting_threads: self.waiters.load(Ordering::Relaxed),
                    peak_waiters: self.waiter_peak.load(Ordering::Relaxed),
                    opened_connections,
                });
                if result.is_ok() {
                    ACTIVE_READ_POOLS.with(|active| {
                        active.borrow_mut().insert(pool_id);
                    });
                }
                return result.map(|connection| ReadConnection {
                    connection: Some(connection),
                    pool: Arc::clone(self),
                    pool_id,
                    _thread_bound: PhantomData,
                });
            }
            if !waiting {
                waiting = true;
                let active = self.waiters.fetch_add(1, Ordering::Relaxed) + 1;
                let peak_waiters = self
                    .waiter_peak
                    .fetch_max(active, Ordering::Relaxed)
                    .max(active);
                emit_diagnostic(StoreDiagnosticsEvent::ReadWaiters {
                    waiting_threads: active,
                    peak_waiters,
                    opened_connections: state.opened,
                });
            }
            state = match self.available.wait(state) {
                Ok(state) => state,
                Err(error) => {
                    if waiting {
                        let waiting_threads = self.waiters.fetch_sub(1, Ordering::Relaxed) - 1;
                        emit_diagnostic(StoreDiagnosticsEvent::ReadWaiters {
                            waiting_threads,
                            peak_waiters: self.waiter_peak.load(Ordering::Relaxed),
                            opened_connections: error.get_ref().opened,
                        });
                        emit_diagnostic(StoreDiagnosticsEvent::ReadAdmission {
                            wait: started.elapsed(),
                            waited: true,
                            waiting_threads: self.waiters.load(Ordering::Relaxed),
                            peak_waiters: self.waiter_peak.load(Ordering::Relaxed),
                            opened_connections: error.get_ref().opened,
                        });
                    }
                    return Err(StoreError::WriterPoisoned);
                }
            };
        }
    }

    #[cfg(test)]
    fn opened_count(&self) -> usize {
        self.state.lock().expect("read pool lock").opened
    }
}

#[derive(Default)]
struct CommitNotifiers {
    primary: Option<Arc<dyn Fn() + Send + Sync>>,
    observers: Vec<Arc<dyn Fn() + Send + Sync>>,
}

enum WriterTransactionOutcome<T, E> {
    Known {
        result: Result<T, E>,
        generation: u64,
        changed: bool,
        changed_tables: u8,
    },
    CommitOutcomeUnknown {
        error: StoreError,
        possible_table_changes: u8,
    },
}

struct WriterWaiterRegistration<'a> {
    waiting: &'a AtomicUsize,
    peak: &'a AtomicUsize,
}

impl<'a> WriterWaiterRegistration<'a> {
    fn begin(waiting: &'a AtomicUsize, peak: &'a AtomicUsize) -> Self {
        let waiting_threads = waiting.fetch_add(1, Ordering::Relaxed) + 1;
        peak.fetch_max(waiting_threads, Ordering::Relaxed);
        emit_diagnostic(StoreDiagnosticsEvent::WriterWaiters {
            waiting_threads,
            peak_waiters: peak.load(Ordering::Relaxed),
        });
        Self { waiting, peak }
    }
}

impl Drop for WriterWaiterRegistration<'_> {
    fn drop(&mut self) {
        let waiting_threads = self.waiting.fetch_sub(1, Ordering::Relaxed) - 1;
        emit_diagnostic(StoreDiagnosticsEvent::WriterWaiters {
            waiting_threads,
            peak_waiters: self.peak.load(Ordering::Relaxed),
        });
    }
}

struct WalCheckpointClaim<'a> {
    requested: &'a AtomicBool,
    completed: bool,
}

impl Drop for WalCheckpointClaim<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.requested.store(true, Ordering::Release);
        }
    }
}

impl RuntimeStore {
    /// Canonical state root for adapters that must resolve content-addressed receipts.
    pub fn state_dir(&self) -> &Path {
        &self.root
    }

    /// Open an existing state directory, bootstrapping the sync family on first open.
    pub async fn open(state_dir: Option<&Path>) -> StoreResult<Self> {
        let root = match state_dir {
            Some(path) => path::resolve_explicit_state_dir(path)?,
            None => resolve_state_dir()?,
        };
        fresh_state::validate(&root)?;
        let canvas_path = root.join("canvas.sqlite3");
        let analytics_path = root.join("analytics.sqlite3");
        let runtime_lock = Arc::new(RuntimeLock::acquire(&root)?);
        fresh_state::prepare(&root)?;
        let local_server_id = local_server_id(&root)?;

        for database in [&canvas_path, &analytics_path] {
            if !database.is_file() {
                return Err(StoreError::MissingDatabase(database.clone()));
            }
        }
        // Existing paired databases make fresh_state::prepare a no-op; newly
        // created databases are verified here after their catalog is prepared.
        schema::verify_core(&canvas_path, &analytics_path)?;
        // Keep Rust opens current even when Python has not refreshed this index yet.
        schema::ensure_analytics_limits_index(&analytics_path)?;

        let table_change_flags = Arc::new(AtomicU8::new(0));
        let writer = SqliteActor::open(&canvas_path, Arc::clone(&runtime_lock)).await?;
        let hook_table_change_flags = Arc::clone(&table_change_flags);
        writer
            .call(move |connection| {
                connection
                    .busy_timeout(Duration::from_millis(u64::from(DATABASE_BUSY_TIMEOUT_MS)))
                    .map_err(StoreError::from)?;
                install_profile(connection, StoreDatabaseKind::Canvas);
                connection
                    .pragma_update(None, "synchronous", "FULL")
                    .map_err(StoreError::from)?;
                schema::register_python_trigger_functions(connection)?;
                install_table_change_hooks(connection, hook_table_change_flags)
                    .map_err(StoreError::from)?;
                Ok::<(), StoreError>(())
            })
            .await
            .map_err(|error| match error {
                tokio_rusqlite::Error::Error(error) => error,
                tokio_rusqlite::Error::ConnectionClosed => {
                    StoreError::WriterActorInitialization(tokio_rusqlite::Error::ConnectionClosed)
                }
                tokio_rusqlite::Error::Close((_, error)) => StoreError::Sqlite(error),
                _ => StoreError::WriterActorInitialization(tokio_rusqlite::Error::ConnectionClosed),
            })?;
        let analytics = open_database(&analytics_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        analytics.pragma_update(None, "journal_mode", "WAL")?;
        analytics.pragma_update(None, "synchronous", "FULL")?;
        let version_reader = open_database(&canvas_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        version_reader.pragma_update(None, "query_only", "ON")?;
        let identity_registry_path = canvas_path.clone();
        {
            let mut registry = identity_registry()
                .lock()
                .map_err(|_| StoreError::WriterPoisoned)?;
            let entry = registry
                .entry(identity_registry_path.clone())
                .or_insert_with(|| IdentityEntry {
                    server_id: local_server_id.clone(),
                    stores: 0,
                });
            entry.stores += 1;
        }
        let store = Self {
            root,
            writer,
            analytics: Arc::new(Mutex::new(analytics)),
            version_reader: Mutex::new(version_reader),
            read_pool: Arc::new(ReadConnectionPool::new(canvas_path.clone())),
            writer_waiters: AtomicUsize::new(0),
            writer_waiter_peak: AtomicUsize::new(0),
            identity_registry_path,
            identity_registry_lease: Arc::new(IdentityRegistryLease {
                path: canvas_path.clone(),
            }),
            commit_notifiers: Arc::new(Mutex::new(CommitNotifiers::default())),
            table_commit_observers: Arc::new(Mutex::new(HashMap::new())),
            table_change_flags,
            #[cfg(test)]
            simulate_unknown_commit_after_success: Arc::new(AtomicBool::new(false)),
            committed_change_generation: Arc::new(AtomicU64::new(0)),
            last_entity_projection_state: Mutex::new(None),
            entity_projection_maintenance_lock: tokio::sync::Mutex::new(()),
            wal_checkpoint_requested: Arc::new(AtomicBool::new(false)),
            last_wal_checkpoint: Mutex::new(Instant::now()),
        };
        store
            .transaction(move |transaction| {
                bootstrap::prepare_sync_schema(transaction)?;
                bootstrap::prepare_runtime_event_indexes(transaction)?;
                feature_schema::prepare(transaction)?;
                schema::verify_bootstrap(transaction)?;
                verify_observed_tables_are_rowid(transaction)
            })
            .await?;
        let recovery_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs_f64())
            .unwrap_or_default();
        store
            .transaction(move |transaction| {
                CheckpointsRepository::recover_captures(transaction, recovery_time)
            })
            .await?;
        // Schema preparation and recovery are private startup work. The store
        // is not observable until open returns, so publish a zero generation
        // baseline after those commits and count only caller-visible writes.
        store
            .committed_change_generation
            .store(0, Ordering::Release);
        store.checkpoint_wal_passive().await?;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Update the process-local server identity before requests begin.
    pub fn set_local_server_id(&self, server_id: Option<String>) -> StoreResult<()> {
        if server_id.as_deref().is_some_and(str::is_empty) {
            return Err(StoreError::InvalidSyncObject {
                scope: "multi-server".to_owned(),
                id: "identity.json".to_owned(),
            });
        }
        let mut registry = identity_registry()
            .lock()
            .map_err(|_| StoreError::WriterPoisoned)?;
        let entry =
            registry
                .get_mut(&self.identity_registry_path)
                .ok_or(StoreError::RecordContract(
                    "runtime identity registry entry missing",
                ))?;
        entry.server_id = server_id;
        Ok(())
    }

    pub fn local_server_id(&self) -> StoreResult<Option<String>> {
        identity_registry()
            .lock()
            .map_err(|_| StoreError::WriterPoisoned)?
            .get(&self.identity_registry_path)
            .map(|entry| entry.server_id.clone())
            .ok_or(StoreError::RecordContract(
                "runtime identity registry entry missing",
            ))
    }

    pub fn transaction_local_server_id(
        transaction: &Transaction<'_>,
    ) -> StoreResult<Option<String>> {
        let Some(path) = transaction.path() else {
            return Ok(None);
        };
        Ok(identity_registry()
            .lock()
            .map_err(|_| StoreError::WriterPoisoned)?
            .get(Path::new(path))
            .and_then(|entry| entry.server_id.clone()))
    }

    /// Register a change invalidation hint for a Canvas table.
    /// Register before taking the snapshot that the observer will maintain.
    /// A hint fires after a known successful commit or an ambiguous commit that
    /// may have landed; consumers must reread authoritative state. Callbacks run
    /// outside the transaction and observer lock. After caller cancellation
    /// they may run on the actor thread, so keep them short and free of database
    /// or provider I/O.
    pub fn add_table_change_observer(
        &self,
        table: ObservedTable,
        observer: impl Fn() + Send + Sync + 'static,
    ) -> StoreResult<()> {
        self.table_commit_observers
            .lock()
            .map_err(|_| StoreError::WriterPoisoned)?
            .entry(table)
            .or_default()
            .push(Arc::new(observer));
        Ok(())
    }

    /// Install the process-local notification fired after each successful write commit.
    pub fn set_commit_notifier(&self, notifier: impl Fn() + Send + Sync + 'static) {
        let mut notifiers = self
            .commit_notifiers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        notifiers.primary = Some(Arc::new(notifier));
    }

    /// Add an independent observer fired after each successful write commit.
    /// Unlike the primary notifier, observers are retained when an owner replaces it.
    pub fn add_commit_observer(&self, observer: impl Fn() + Send + Sync + 'static) {
        self.commit_notifiers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .observers
            .push(Arc::new(observer));
    }

    /// Install or clear the optional value-free diagnostics observer.
    pub async fn set_diagnostics_callback(
        &self,
        callback: Option<StoreDiagnosticsCallback>,
    ) -> StoreResult<()> {
        let mut current = diagnostics_callback()
            .lock()
            .map_err(|_| StoreError::WriterPoisoned)?;
        DIAGNOSTICS_ENABLED.store(callback.is_some(), Ordering::Release);
        *current = callback;
        drop(current);
        self.writer
            .call(|writer| {
                install_profile(writer, StoreDatabaseKind::Canvas);
                Ok::<(), rusqlite::Error>(())
            })
            .await
            .map_err(StoreError::WriterActorControl)?;
        let mut analytics = self
            .analytics
            .lock()
            .map_err(|_| StoreError::WriterPoisoned)?;
        install_profile(&mut analytics, StoreDatabaseKind::Analytics);
        let mut reader = self
            .version_reader
            .lock()
            .map_err(|_| StoreError::WriterPoisoned)?;
        install_profile(&mut reader, StoreDatabaseKind::Canvas);
        let mut pool = self
            .read_pool
            .state
            .lock()
            .map_err(|_| StoreError::WriterPoisoned)?;
        for connection in &mut pool.idle {
            install_profile(connection, StoreDatabaseKind::Canvas);
        }
        Ok(())
    }

    /// Checkpoint both WAL files without waiting on active readers or truncating them.
    pub async fn checkpoint_wal_passive(&self) -> StoreResult<()> {
        let canvas_wal_path = self.root.join("canvas.sqlite3-wal");
        self.writer
            .call(move |writer| {
                Self::checkpoint_connection(writer, StoreDatabaseKind::Canvas, &canvas_wal_path)
            })
            .await
            .map_err(StoreError::WriterActorControl)?;

        let analytics = self
            .analytics
            .lock()
            .map_err(|_| StoreError::WriterPoisoned)?;
        Self::checkpoint_connection(
            &analytics,
            StoreDatabaseKind::Analytics,
            &self.root.join("analytics.sqlite3-wal"),
        )?;
        Ok(())
    }

    /// Coalesce write requests and apply WAL threshold/cadence policy from the maintenance owner.
    /// This async method performs filesystem checks and submits its SQLite work to the actor.
    pub async fn checkpoint_wal_if_due(&self) -> StoreResult<bool> {
        if !self.wal_checkpoint_requested.load(Ordering::Acquire) {
            return Ok(false);
        }
        let canvas_size = std::fs::metadata(self.root.join("canvas.sqlite3-wal"))
            .map_or(0, |metadata| metadata.len());
        let analytics_size = std::fs::metadata(self.root.join("analytics.sqlite3-wal"))
            .map_or(0, |metadata| metadata.len());
        let cadence_due = self
            .last_wal_checkpoint
            .lock()
            .map_err(|_| StoreError::WriterPoisoned)?
            .elapsed()
            >= WAL_CHECKPOINT_CADENCE;
        if canvas_size < WAL_CHECKPOINT_THRESHOLD_BYTES
            && analytics_size < WAL_CHECKPOINT_THRESHOLD_BYTES
            && !cadence_due
        {
            return Ok(false);
        }
        if !self.wal_checkpoint_requested.swap(false, Ordering::AcqRel) {
            return Ok(false);
        }
        let mut checkpoint_claim = WalCheckpointClaim {
            requested: &self.wal_checkpoint_requested,
            completed: false,
        };
        self.checkpoint_wal_passive().await?;
        *self
            .last_wal_checkpoint
            .lock()
            .map_err(|_| StoreError::WriterPoisoned)? = Instant::now();
        checkpoint_claim.completed = true;
        Ok(true)
    }

    /// Return the remaining cadence before a requested checkpoint can run.
    /// The server uses this only after an event-triggered checkpoint was
    /// deferred, so no periodic WAL polling is needed.
    pub fn wal_checkpoint_retry_after(&self) -> StoreResult<Option<Duration>> {
        if !self.wal_checkpoint_requested.load(Ordering::Acquire) {
            return Ok(None);
        }
        let elapsed = self
            .last_wal_checkpoint
            .lock()
            .map_err(|_| StoreError::WriterPoisoned)?
            .elapsed();
        Ok(Some(WAL_CHECKPOINT_CADENCE.saturating_sub(elapsed)))
    }

    pub(crate) fn request_wal_checkpoint(&self) {
        self.wal_checkpoint_requested.store(true, Ordering::Release);
    }

    fn checkpoint_connection(
        connection: &Connection,
        database: StoreDatabaseKind,
        wal_path: &Path,
    ) -> rusqlite::Result<()> {
        let (busy, log_frames, checkpointed_frames) =
            connection.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
        let wal_size_bytes = std::fs::metadata(wal_path).map_or(0, |metadata| metadata.len());
        emit_diagnostic(StoreDiagnosticsEvent::WalCheckpoint {
            database,
            wal_size_bytes,
            busy,
            log_frames,
            checkpointed_frames,
        });
        Ok(())
    }

    fn notify_commit(&self) {
        self.request_wal_checkpoint();
        fire_commit_observers(&self.commit_notifiers);
    }

    /// Run one durable transaction while holding the sole serialized writer.
    /// Admission waits cancellably when the named actor capacity is exhausted. A
    /// canceled wait submits no SQL; cancellation after submission cannot retract
    /// the job, so callers must resolve a lost response through their receipt path.
    pub fn transaction<T>(
        &self,
        operation: impl for<'transaction> FnOnce(&Transaction<'transaction>) -> StoreResult<T>
        + Send
        + 'static,
    ) -> impl Future<Output = StoreResult<T>> + Send + '_
    where
        T: Send + 'static,
    {
        self.transaction_with(operation)
    }

    /// Run one durable transaction with a domain error type that preserves
    /// operation refusals while still reporting store and commit failures.
    pub fn transaction_with<T, E>(
        &self,
        operation: impl for<'transaction> FnOnce(&Transaction<'transaction>) -> Result<T, E>
        + Send
        + 'static,
    ) -> impl Future<Output = Result<T, E>> + Send + '_
    where
        T: Send + 'static,
        E: From<StoreError> + Send + 'static,
    {
        let operation: BoxedTransactionOperation<T, E> = Box::new(operation);
        async move {
            self.transaction_with_post_commit_boxed(operation, |_| {})
                .await
                .map(|(value, _)| value)
        }
    }

    /// Run a write transaction and return its committed change generation.
    /// The generation unit is one committed transaction with row changes.
    pub fn transaction_with_commit_generation<T, E>(
        &self,
        operation: impl for<'transaction> FnOnce(&Transaction<'transaction>) -> Result<T, E>
        + Send
        + 'static,
    ) -> impl Future<Output = Result<(T, u64), E>> + Send + '_
    where
        T: Send + 'static,
        E: From<StoreError> + Send + 'static,
    {
        let operation: BoxedTransactionOperation<T, E> = Box::new(operation);
        self.transaction_with_post_commit_boxed(operation, |_| {})
    }

    /// Run a write transaction and invoke `post_commit` exactly once after a
    /// successful SQLite commit, even if the submitted response is abandoned.
    /// The callback runs synchronously after the writer callback releases its
    /// transaction and admission permit; keep it bounded and free of database
    /// or provider I/O. Errors, rollback, and cancellation before submission
    /// do not invoke it.
    pub fn transaction_with_post_commit<T, E>(
        &self,
        operation: impl for<'transaction> FnOnce(&Transaction<'transaction>) -> Result<T, E>
        + Send
        + 'static,
        post_commit: impl FnOnce(&T) + Send + 'static,
    ) -> impl Future<Output = Result<(T, u64), E>> + Send + '_
    where
        T: Send + 'static,
        E: From<StoreError> + Send + 'static,
    {
        let operation: BoxedTransactionOperation<T, E> = Box::new(operation);
        self.transaction_with_post_commit_boxed(operation, post_commit)
    }

    async fn transaction_with_post_commit_boxed<T, E>(
        &self,
        operation: BoxedTransactionOperation<T, E>,
        post_commit: impl FnOnce(&T) + Send + 'static,
    ) -> Result<(T, u64), E>
    where
        T: Send + 'static,
        E: From<StoreError> + Send + 'static,
    {
        let diagnostics_enabled = DIAGNOSTICS_ENABLED.load(Ordering::Acquire);
        let waiting = diagnostics_enabled.then(Instant::now);
        let did_wait = self.writer.available_admissions() == 0;
        let waiter_registration = did_wait.then(|| {
            WriterWaiterRegistration::begin(&self.writer_waiters, &self.writer_waiter_peak)
        });
        let permit = self
            .writer
            .acquire()
            .await
            .map_err(actor_transport_outcome_unknown)
            .map_err(E::from)?;
        let account_worker_operation_lease =
            ACCOUNT_WORKER_OPERATION_LEASE.try_with(Arc::clone).ok();
        drop(waiter_registration);
        let waiting_threads = self.writer_waiters.load(Ordering::Relaxed);
        let wait = waiting.map_or(Duration::ZERO, |started| started.elapsed());
        if diagnostics_enabled {
            emit_diagnostic(StoreDiagnosticsEvent::WriterAdmission {
                wait,
                waited: did_wait,
                waiting_threads,
                peak_waiters: self.writer_waiter_peak.load(Ordering::Relaxed),
            });
        }
        let holding = diagnostics_enabled.then(Instant::now);
        let generation = Arc::clone(&self.committed_change_generation);
        let identity_registry_lease = Arc::clone(&self.identity_registry_lease);
        let table_change_flags = Arc::clone(&self.table_change_flags);
        #[cfg(test)]
        let simulate_unknown_commit_after_success =
            Arc::clone(&self.simulate_unknown_commit_after_success);
        let operation =
            move |writer: &mut Connection| -> Result<WriterTransactionOutcome<T, E>, StoreError> {
                let _account_worker_operation_lease = account_worker_operation_lease;
                let _identity_registry_lease = identity_registry_lease;
                table_change_flags.store(0, Ordering::Release);
                let transaction = writer
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(StoreError::from)?;
                let initial_changes = transaction.total_changes();
                match operation(&transaction) {
                    Ok(value) => {
                        let changed = transaction.total_changes() != initial_changes;
                        let commit_started = diagnostics_enabled.then(Instant::now);
                        if let Err(error) = transaction.commit() {
                            return Ok(WriterTransactionOutcome::CommitOutcomeUnknown {
                                error: StoreError::CommitOutcomeUnknown(error),
                                possible_table_changes: table_change_flags
                                    .swap(0, Ordering::AcqRel),
                            });
                        }
                        #[cfg(test)]
                        if simulate_unknown_commit_after_success.swap(false, Ordering::AcqRel) {
                            return Ok(WriterTransactionOutcome::CommitOutcomeUnknown {
                                error: StoreError::CommitOutcomeUnknown(
                                    rusqlite::Error::InvalidQuery,
                                ),
                                possible_table_changes: table_change_flags
                                    .swap(0, Ordering::AcqRel),
                            });
                        }
                        if let Some(started) = commit_started {
                            emit_diagnostic(StoreDiagnosticsEvent::CommitDuration {
                                database: StoreDatabaseKind::Canvas,
                                elapsed: started.elapsed(),
                            });
                        }
                        let commit_generation = if changed {
                            generation.fetch_add(1, Ordering::Release) + 1
                        } else {
                            generation.load(Ordering::Acquire)
                        };
                        let changed_tables = table_change_flags.swap(0, Ordering::AcqRel);
                        Ok(WriterTransactionOutcome::Known {
                            result: Ok(value),
                            generation: commit_generation,
                            changed,
                            changed_tables,
                        })
                    }
                    Err(error) => {
                        // Dropping rolls back best-effort; preserve the operation error.
                        drop(transaction);
                        table_change_flags.store(0, Ordering::Release);
                        Ok(WriterTransactionOutcome::Known {
                            result: Err(error),
                            generation: generation.load(Ordering::Acquire),
                            changed: false,
                            changed_tables: 0,
                        })
                    }
                }
            };
        let abandoned_notifiers = Arc::clone(&self.commit_notifiers);
        let abandoned_checkpoint = Arc::clone(&self.wal_checkpoint_requested);
        let table_commit_observers = Arc::clone(&self.table_commit_observers);
        let actor_outcome = self
            .writer
            .call_admitted_with_completion(permit, operation, move |result, disposition| {
                if let Ok(outcome) = result {
                    let (changed_tables, value) = match outcome {
                        WriterTransactionOutcome::Known {
                            result: Ok(value),
                            changed,
                            changed_tables,
                            ..
                        } => {
                            if *changed && disposition == ActorResponseDisposition::Abandoned {
                                abandoned_checkpoint.store(true, Ordering::Release);
                                // Fire the synchronous wake directly; no Runtime handle or
                                // detached task is required for this committed transaction.
                                let _ =
                                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                        fire_commit_observers(&abandoned_notifiers);
                                    }));
                            }
                            (*changed_tables, Some(value))
                        }
                        WriterTransactionOutcome::CommitOutcomeUnknown {
                            possible_table_changes,
                            ..
                        } => (*possible_table_changes, None),
                        WriterTransactionOutcome::Known { result: Err(_), .. } => (0, None),
                    };
                    let table_observer_result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            fire_table_change_observers(&table_commit_observers, changed_tables);
                        }));
                    let post_commit_result = value.map(|value| {
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            post_commit(value)
                        }))
                    });
                    if let Err(panic) = table_observer_result {
                        std::panic::resume_unwind(panic);
                    }
                    if let Some(Err(panic)) = post_commit_result {
                        std::panic::resume_unwind(panic);
                    }
                }
            })
            .await;
        let hold = holding.map_or(Duration::ZERO, |started| started.elapsed());
        if diagnostics_enabled {
            emit_diagnostic(StoreDiagnosticsEvent::WriterTransaction { wait, hold });
        }
        let actor_response = actor_outcome
            .map_err(actor_transport_outcome_unknown)
            .map_err(E::from)?;
        let outcome = actor_response
            .resolve_with(|result| {
                if matches!(
                    result,
                    Ok(WriterTransactionOutcome::Known { changed: true, .. })
                ) {
                    self.notify_commit();
                }
            })
            .map_err(E::from)?;
        match outcome {
            WriterTransactionOutcome::Known {
                result, generation, ..
            } => result.map(|value| (value, generation)),
            WriterTransactionOutcome::CommitOutcomeUnknown { error, .. } => Err(E::from(error)),
        }
    }

    pub fn transaction_with_error<T, E>(
        &self,
        operation: impl for<'transaction> FnOnce(&Transaction<'transaction>) -> Result<T, E>
        + Send
        + 'static,
    ) -> impl Future<Output = Result<T, E>> + Send + '_
    where
        T: Send + 'static,
        E: From<StoreError> + Send + 'static,
    {
        self.transaction_with(operation)
    }

    /// Open an independent read-only, query-only connection to the main DB.
    pub fn read(&self) -> StoreResult<ReadConnection> {
        self.read_pool.checkout()
    }

    /// Run related reads in one SQLite snapshot.
    pub fn read_transaction<T>(
        &self,
        operation: impl FnOnce(&Transaction<'_>) -> StoreResult<T>,
    ) -> StoreResult<T> {
        self.read_transaction_with_error(operation)
    }

    /// Run a read-only snapshot with a domain error type that preserves
    /// operation refusals while still reporting store and commit failures.
    pub fn read_transaction_with_error<T, E>(
        &self,
        operation: impl FnOnce(&Transaction<'_>) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<StoreError>,
    {
        let mut connection = self.read()?;
        let transaction = connection.transaction().map_err(StoreError::from)?;
        let value = operation(&transaction)?;
        transaction
            .commit()
            .map_err(StoreError::from)
            .map_err(E::from)?;
        Ok(value)
    }

    /// Return SQLite's connection-local generation, advanced by commits made
    /// through the serialized writer or another connection.
    pub fn generation(&self) -> StoreResult<i64> {
        let reader = self
            .version_reader
            .lock()
            .map_err(|_| StoreError::WriterPoisoned)?;
        Ok(reader.query_row("PRAGMA data_version", [], |row| row.get(0))?)
    }

    /// Return the number of committed writer transactions that changed rows.
    /// This is process-local and intentionally distinct from sync-document generation.
    pub fn committed_change_generation(&self) -> u64 {
        self.committed_change_generation.load(Ordering::Acquire)
    }

    /// Whether projection maintenance has not observed the current commit generation and vault signature.
    pub fn entity_projections_need_maintenance(&self, vault_signature: &str) -> StoreResult<bool> {
        let last = self
            .last_entity_projection_state
            .lock()
            .map_err(|_| StoreError::WriterPoisoned)?;
        let current_generation = self.committed_change_generation();
        Ok(match last.as_ref() {
            Some((generation, signature)) => {
                *generation != current_generation || signature != vault_signature
            }
            None => true,
        })
    }

    /// Serialize the cold entity projection maintenance path for this process store.
    pub async fn lock_entity_projection_maintenance(
        &self,
    ) -> StoreResult<tokio::sync::MutexGuard<'_, ()>> {
        Ok(self.entity_projection_maintenance_lock.lock().await)
    }

    /// Record the committed store generation and vault input signature after maintenance commits.
    pub fn mark_entity_projections_maintained(
        &self,
        generation: u64,
        vault_signature: String,
    ) -> StoreResult<()> {
        if generation > self.committed_change_generation() {
            return Err(StoreError::RecordContract(
                "entity projection generation is newer than committed store state",
            ));
        }
        *self
            .last_entity_projection_state
            .lock()
            .map_err(|_| StoreError::WriterPoisoned)? = Some((generation, vault_signature));
        Ok(())
    }

    pub fn verify_schema(&self) -> StoreResult<()> {
        schema::verify(
            &self.root.join("canvas.sqlite3"),
            &self.root.join("analytics.sqlite3"),
        )
    }
}

/// Read the public routing ID from the multi-server identity file before startup.
/// Missing identity remains a read-only `None`; malformed, unsafe, or unusable
/// identity data is refused, matching the Python runtime's credential gate.
pub fn local_server_id(root: &Path) -> StoreResult<Option<String>> {
    let directory = root.join("multi-server");
    let directory_metadata = match std::fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(invalid_local_identity(
                "cannot inspect credential directory",
            ));
        }
    };
    if !directory_metadata.is_dir() || directory_metadata.file_type().is_symlink() {
        return Err(invalid_local_identity(
            "credential directory is not a directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if directory_metadata.permissions().mode() & 0o077 != 0 {
            return Err(invalid_local_identity(
                "unsafe credential directory permissions",
            ));
        }
    }
    let path = directory.join("identity.json");
    let path_metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(invalid_local_identity("cannot inspect identity.json")),
    };
    if !path_metadata.is_file()
        || path_metadata.file_type().is_symlink()
        || path_metadata.len() > 16_384
    {
        return Err(invalid_local_identity(
            "identity.json is not a bounded regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path_metadata.permissions().mode() & 0o077 != 0 {
            return Err(invalid_local_identity("unsafe identity.json permissions"));
        }
    }
    let file =
        File::open(&path).map_err(|_| invalid_local_identity("cannot read identity.json"))?;
    if !file
        .metadata()
        .map_err(|_| invalid_local_identity("cannot inspect identity.json"))?
        .is_file()
    {
        return Err(invalid_local_identity(
            "identity.json is not a regular file",
        ));
    }
    let identity: serde_json::Value = match serde_json::from_reader(BufReader::new(file)) {
        Ok(identity) => identity,
        Err(_) => return Err(invalid_local_identity("malformed JSON in identity.json")),
    };
    let Some(server_id) = identity.get("serverId").and_then(serde_json::Value::as_str) else {
        return Err(invalid_local_identity("missing serverId"));
    };
    if !valid_server_id(server_id) {
        return Err(invalid_local_identity("invalid serverId"));
    }
    Ok(Some(server_id.to_owned()))
}

fn valid_server_id(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && value.len() <= 128
        && bytes
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':' | b'-'))
}

fn invalid_local_identity(reason: &str) -> StoreError {
    StoreError::InvalidSyncObject {
        scope: "multi-server".to_owned(),
        id: format!("identity.json: {reason}"),
    }
}

fn open_database(path: &Path, flags: OpenFlags) -> StoreResult<Connection> {
    let mut connection = Connection::open_with_flags(path, flags)?;
    let database = match path.file_name().and_then(|name| name.to_str()) {
        Some("analytics.sqlite3") => StoreDatabaseKind::Analytics,
        Some("canvas.sqlite3") => StoreDatabaseKind::Canvas,
        _ => StoreDatabaseKind::Other,
    };
    install_profile(&mut connection, database);
    connection.busy_timeout(std::time::Duration::from_millis(u64::from(
        DATABASE_BUSY_TIMEOUT_MS,
    )))?;
    Ok(connection)
}

#[cfg(test)]
mod read_pool_tests {
    use super::*;
    use std::sync::mpsc;

    #[tokio::test]
    async fn sequential_reads_reuse_a_bounded_connection() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open runtime store");

        for _ in 0..16 {
            let reader = store.read().expect("read checkout");
            assert_eq!(
                reader
                    .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                    .expect("checked out reader remains queryable"),
                1
            );
        }

        assert_eq!(store.read_pool.opened_count(), 1);
        const {
            assert!(READ_CONNECTION_POOL_CAPACITY > 1);
            assert!(
                READ_CONNECTION_POOL_CAPACITY + VERSION_READER_CONNECTION_RESERVATION
                    == CANVAS_READER_CONNECTION_CAPACITY
            );
        }
    }

    #[tokio::test]
    async fn nested_read_fails_fast_when_all_pooled_connections_are_checked_out() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open runtime store"),
        );
        let (ready_tx, ready_rx) = mpsc::channel();
        let (start_nested_tx, start_nested_rx) = mpsc::channel();
        let mut start_nested_rx = Some(start_nested_rx);
        let (nested_result_tx, nested_result_rx) = mpsc::sync_channel(1);
        let mut release_senders = Vec::with_capacity(READ_CONNECTION_POOL_CAPACITY);

        std::thread::scope(|scope| {
            for index in 0..READ_CONNECTION_POOL_CAPACITY {
                let store = Arc::clone(&store);
                let ready_tx = ready_tx.clone();
                let start_nested_rx = if index == 0 {
                    start_nested_rx.take()
                } else {
                    None
                };
                let nested_result_tx = nested_result_tx.clone();
                let (release_tx, release_rx) = mpsc::channel();
                release_senders.push(release_tx);
                scope.spawn(move || {
                    let _reader = store.read().expect("acquire pool connection");
                    ready_tx.send(()).expect("report read connection ready");
                    if let Some(start_nested_rx) = start_nested_rx {
                        start_nested_rx.recv().expect("start nested checkout");
                        let nested_is_refused = matches!(store.read(), Err(StoreError::NestedRead));
                        nested_result_tx
                            .send(nested_is_refused)
                            .expect("report nested checkout result");
                    }
                    release_rx.recv().expect("release read connection");
                });
            }

            for _ in 0..READ_CONNECTION_POOL_CAPACITY {
                ready_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("all pool connections are checked out");
            }
            assert_eq!(
                store.read_pool.opened_count(),
                READ_CONNECTION_POOL_CAPACITY
            );
            start_nested_tx.send(()).expect("request nested checkout");
            let immediate_result = nested_result_rx.recv_timeout(Duration::from_secs(2));

            // If nested checkout regresses to waiting, release the other leases
            // so the test can report the wrong result instead of hanging.
            for release in release_senders.iter().skip(1) {
                release.send(()).expect("release other read connection");
            }
            let nested_result = match immediate_result {
                Ok(result) => Some(result),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    nested_result_rx.recv_timeout(Duration::from_secs(5)).ok()
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => None,
            };
            release_senders[0]
                .send(())
                .expect("release nested-read worker");
            assert!(
                immediate_result.is_ok() && nested_result == Some(true),
                "nested read must be refused before waiting for another pool connection"
            );
        });
    }
}

#[cfg(test)]
mod diagnostics_hook_tests {
    use super::*;
    use std::sync::{Condvar, Mutex, mpsc};

    static EVENTS: Mutex<Vec<StoreDiagnosticsEvent>> = Mutex::new(Vec::new());
    static EVENTS_READY: Condvar = Condvar::new();
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn record_event(event: StoreDiagnosticsEvent) {
        EVENTS.lock().expect("diagnostic events lock").push(event);
        EVENTS_READY.notify_all();
    }

    fn wait_for_event(predicate: impl Fn(&StoreDiagnosticsEvent) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut events = EVENTS.lock().expect("diagnostic events lock");
        loop {
            if events.iter().any(&predicate) {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, timeout) = EVENTS_READY
                .wait_timeout(events, remaining)
                .expect("diagnostics event wait");
            events = next;
            if timeout.timed_out() && !events.iter().any(&predicate) {
                return false;
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn optional_hook_observes_writer_and_sql_timing_and_can_be_cleared() {
        let _test_guard = TEST_LOCK.lock().expect("serialize diagnostics hook tests");
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open runtime store"),
        );
        EVENTS.lock().expect("diagnostic events lock").clear();
        store
            .set_diagnostics_callback(Some(record_event))
            .await
            .expect("install diagnostics callback");
        store
            .transaction(|transaction| {
                transaction.execute_batch("CREATE TABLE measured(value INTEGER);")?;
                transaction.execute("INSERT INTO measured(value) VALUES (1)", [])?;
                Ok(())
            })
            .await
            .expect("transaction succeeds");
        assert!(EVENTS.lock().expect("diagnostic events lock").len() >= 3);

        EVENTS.lock().expect("diagnostic events lock").clear();
        let read_wait_observed = std::thread::scope(|scope| {
            let (ready_tx, ready_rx) = mpsc::channel();
            let mut release = Vec::with_capacity(READ_CONNECTION_POOL_CAPACITY);
            for _ in 0..READ_CONNECTION_POOL_CAPACITY {
                let (release_tx, release_rx) = mpsc::channel();
                release.push(release_tx);
                let ready_tx = ready_tx.clone();
                let store_ref = &store;
                scope.spawn(move || {
                    let reader = store_ref.read().expect("checkout held reader");
                    ready_tx.send(()).expect("report held reader");
                    release_rx.recv().expect("release held reader");
                    drop(reader);
                });
            }
            drop(ready_tx);
            let mut all_readers_held = true;
            for _ in 0..READ_CONNECTION_POOL_CAPACITY {
                if ready_rx.recv_timeout(Duration::from_secs(2)).is_err() {
                    all_readers_held = false;
                    break;
                }
            }
            if !all_readers_held {
                for release in &release {
                    let _result = release.send(());
                }
                return false;
            }
            let (done_tx, done_rx) = mpsc::channel();
            let store_ref = &store;
            let waiter = scope.spawn(move || {
                let reader = store_ref
                    .read()
                    .expect("read waiter acquires released slot");
                done_tx.send(()).expect("report acquired reader");
                drop(reader);
            });
            let observed = wait_for_event(|event| {
                matches!(
                    event,
                    StoreDiagnosticsEvent::ReadWaiters {
                        waiting_threads: 1,
                        peak_waiters: 1,
                        ..
                    }
                )
            });
            release[0].send(()).expect("release first read slot");
            let acquired = done_rx.recv_timeout(Duration::from_secs(2)).is_ok();
            for release in release.iter().skip(1) {
                release.send(()).expect("release read slot");
            }
            waiter.join().expect("read waiter thread");
            observed && acquired
        });
        assert!(
            read_wait_observed,
            "read pool queue occupancy was not published"
        );
        assert_eq!(
            EVENTS
                .lock()
                .expect("diagnostic events lock")
                .iter()
                .filter(|event| matches!(
                    event,
                    StoreDiagnosticsEvent::ReadAdmission { waited: true, .. }
                ))
                .count(),
            1,
            "one completed read wait must produce one admission latency sample"
        );

        store
            .set_diagnostics_callback(None)
            .await
            .expect("clear diagnostics callback");
        let previous = EVENTS.lock().expect("diagnostic events lock").len();
        store
            .transaction(|transaction| {
                transaction.execute("INSERT INTO measured(value) VALUES (2)", [])?;
                Ok(())
            })
            .await
            .expect("transaction succeeds with no observer");
        assert_eq!(
            EVENTS.lock().expect("diagnostic events lock").len(),
            previous
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn analytics_writer_waits_for_canvas_actor_while_raw_reads_remain_independent() {
        let _test_guard = TEST_LOCK.lock().expect("serialize diagnostics hook tests");
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open runtime store"),
        );
        EVENTS.lock().expect("diagnostic events lock").clear();
        store
            .set_diagnostics_callback(Some(record_event))
            .await
            .expect("install diagnostics callback");
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let entered_job = Arc::clone(&entered);
        let release_job = Arc::clone(&release);
        let canvas_job = {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                store
                    .transaction(move |_| {
                        entered_job.wait();
                        release_job.wait();
                        Ok::<(), StoreError>(())
                    })
                    .await
            })
        };
        entered.wait();
        let (completed_tx, mut completed_rx) = tokio::sync::oneshot::channel();
        let analytics_job = {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                let result = store
                    .analytics_transaction(|_, _| Ok::<(), StoreError>(()))
                    .await;
                let _sent = completed_tx.send(result.is_ok());
                result
            })
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            while store.writer.available_admissions() != sqlite_actor::MAX_PENDING_WRITES - 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("analytics transaction is queued behind the held Canvas callback");
        assert!(matches!(
            completed_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        release.wait();
        canvas_job
            .await
            .expect("Canvas writer task")
            .expect("Canvas transaction");
        analytics_job
            .await
            .expect("analytics writer task")
            .expect("analytics transaction");
        store
            .set_diagnostics_callback(None)
            .await
            .expect("clear diagnostics callback");
    }

    #[tokio::test]
    async fn sync_pull_completion_after_a_commit_does_not_checkpoint_the_wal() {
        let _test_guard = TEST_LOCK.lock().expect("serialize diagnostics hook tests");
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open runtime store");
        EVENTS.lock().expect("diagnostic events lock").clear();
        store
            .set_diagnostics_callback(Some(record_event))
            .await
            .expect("install diagnostics callback");
        store
            .transaction(|transaction| {
                transaction.execute_batch("CREATE TABLE checkpoint_witness(value INTEGER);")?;
                transaction.execute("INSERT INTO checkpoint_witness VALUES (1)", [])?;
                Ok::<(), StoreError>(())
            })
            .await
            .expect("commit before pull maintenance completion");
        assert!(store.wal_checkpoint_requested.load(Ordering::Acquire));
        store
            .mark_entity_projections_maintained(
                store.committed_change_generation(),
                "pull-witness".to_owned(),
            )
            .expect("record completed pull maintenance");
        assert!(
            !EVENTS
                .lock()
                .expect("diagnostic events lock")
                .iter()
                .any(|event| matches!(event, StoreDiagnosticsEvent::WalCheckpoint { .. })),
            "sync-pull maintenance completion must not checkpoint after its commit"
        );
        assert!(store.wal_checkpoint_requested.load(Ordering::Acquire));
        store
            .set_diagnostics_callback(None)
            .await
            .expect("clear diagnostics callback");
    }

    #[tokio::test]
    async fn requested_checkpoint_exposes_its_remaining_cadence_without_polling() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open runtime store");
        // Opening the migrated store commits schema setup. Establish a completed
        // checkpoint baseline so this test measures the fixture commit below.
        store
            .checkpoint_wal_passive()
            .await
            .expect("baseline checkpoint after schema setup");
        store
            .wal_checkpoint_requested
            .store(false, Ordering::Release);
        assert_eq!(
            store.wal_checkpoint_retry_after().expect("initial retry"),
            None
        );
        store
            .transaction(|transaction| {
                transaction
                    .execute_batch("CREATE TABLE checkpoint_deadline_witness(value INTEGER);")?;
                transaction.execute("INSERT INTO checkpoint_deadline_witness VALUES (1)", [])?;
                Ok(())
            })
            .await
            .expect("commit requests checkpoint");
        let retry_after = store
            .wal_checkpoint_retry_after()
            .expect("requested checkpoint cadence");
        assert!(retry_after.is_some_and(|delay| delay <= WAL_CHECKPOINT_CADENCE));
    }
}

#[cfg(test)]
mod commit_notification_tests {
    use super::*;
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn transaction_entry_futures_do_not_store_operation_capture() {
        const SMALL_CAPTURE_BYTES: usize = 8;
        const LARGE_CAPTURE_BYTES: usize = 65_536;

        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open runtime store");
        let callback_calls = Arc::new(AtomicUsize::new(0));

        macro_rules! capture_operation {
            ($payload:ident) => {{
                let callback_calls = Arc::clone(&callback_calls);
                move |_| {
                    std::hint::black_box($payload);
                    callback_calls.fetch_add(1, Ordering::Relaxed);
                    Ok::<(), StoreError>(())
                }
            }};
        }

        macro_rules! make_future {
            (transaction, $operation:expr) => {
                store.transaction($operation)
            };
            (transaction_with, $operation:expr) => {
                store.transaction_with($operation)
            };
            (commit_generation, $operation:expr) => {
                store.transaction_with_commit_generation($operation)
            };
            (post_commit, $operation:expr) => {
                store.transaction_with_post_commit($operation, |_| {})
            };
            (transaction_with_error, $operation:expr) => {
                store.transaction_with_error($operation)
            };
        }

        macro_rules! assert_capture_independent_future_size {
            ($entry:ident) => {{
                let small_payload = [0_u8; SMALL_CAPTURE_BYTES];
                let large_payload = [0_u8; LARGE_CAPTURE_BYTES];
                let small_future = make_future!($entry, capture_operation!(small_payload));
                let large_future = make_future!($entry, capture_operation!(large_payload));
                assert_eq!(
                    std::mem::size_of_val(&small_future),
                    std::mem::size_of_val(&large_future),
                    "{} future size must not include its SQL callback payload",
                    stringify!($entry)
                );
                drop((small_future, large_future));
            }};
        }

        assert_capture_independent_future_size!(transaction);
        assert_capture_independent_future_size!(transaction_with);
        assert_capture_independent_future_size!(commit_generation);
        assert_capture_independent_future_size!(post_commit);
        assert_capture_independent_future_size!(transaction_with_error);
        assert_eq!(callback_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn tracked_observer_tables_must_keep_rowid_storage() {
        let database = Connection::open_in_memory().expect("in-memory database");
        database
            .execute_batch(
                "CREATE TABLE runtime_access_clients(id TEXT PRIMARY KEY) WITHOUT ROWID;
                 CREATE TABLE runtime_usage_resumes(id TEXT PRIMARY KEY);",
            )
            .expect("create test tables");
        assert!(matches!(
            verify_observed_tables_are_rowid(&database),
            Err(StoreError::SchemaMismatch(_))
        ));

        database
            .execute_batch("DROP TABLE runtime_access_clients;")
            .expect("drop without-rowid table");
        database
            .execute_batch("CREATE TABLE runtime_access_clients(id TEXT PRIMARY KEY);")
            .expect("create rowid table");
        verify_observed_tables_are_rowid(&database).expect("both tables use rowid storage");
    }

    #[tokio::test]
    async fn table_commit_observer_tracks_only_successful_target_changes() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open runtime store");
        let access_notifications = Arc::new(AtomicUsize::new(0));
        let access_observed = Arc::clone(&access_notifications);
        store
            .add_table_change_observer(ObservedTable::AccessClients, move || {
                access_observed.fetch_add(1, Ordering::AcqRel);
            })
            .expect("register access-client observer");
        let usage_notifications = Arc::new(AtomicUsize::new(0));
        let usage_observed = Arc::clone(&usage_notifications);
        store
            .add_table_change_observer(ObservedTable::UsageResumes, move || {
                usage_observed.fetch_add(1, Ordering::AcqRel);
            })
            .expect("register usage-resume observer");

        store
            .transaction(|transaction| {
                transaction.execute_batch("CREATE TABLE observer_unrelated(value INTEGER);")?;
                transaction.execute("INSERT INTO observer_unrelated VALUES (1)", [])?;
                Ok(())
            })
            .await
            .expect("unrelated write succeeds");
        assert_eq!(access_notifications.load(Ordering::Acquire), 0);
        assert_eq!(usage_notifications.load(Ordering::Acquire), 0);

        store
            .transaction(|transaction| {
                transaction.execute(
                    "INSERT INTO runtime_access_clients(id, record) VALUES ('client-a', '{}')",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("targeted access-client write succeeds");
        assert_eq!(access_notifications.load(Ordering::Acquire), 1);
        assert_eq!(usage_notifications.load(Ordering::Acquire), 0);

        let rolled_back = store
            .transaction_with(|transaction| {
                transaction.execute(
                    "INSERT INTO runtime_access_clients(id, record) VALUES ('client-rollback', '{}')",
                    [],
                )?;
                Err::<(), StoreError>(StoreError::RecordContract("test rollback"))
            })
            .await;
        assert!(rolled_back.is_err());
        assert_eq!(access_notifications.load(Ordering::Acquire), 1);

        store
            .transaction(|transaction| {
                transaction.execute(
                    "INSERT INTO runtime_usage_resumes(id, agent, record) VALUES ('resume-a', 'agent-a', '{}')",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("seed usage-resume row");
        assert_eq!(usage_notifications.load(Ordering::Acquire), 1);
        store
            .transaction(|transaction| {
                transaction.execute("DELETE FROM runtime_usage_resumes", [])?;
                Ok(())
            })
            .await
            .expect("bulk delete usage-resume rows");
        assert_eq!(usage_notifications.load(Ordering::Acquire), 2);

        store
            .transaction(|transaction| {
                transaction.execute(
                    "INSERT OR REPLACE INTO runtime_access_clients(id, record) VALUES ('client-a', '{\"updated\":true}')",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("replacement access-client write succeeds");
        assert_eq!(access_notifications.load(Ordering::Acquire), 2);
    }

    #[tokio::test]
    async fn table_observer_callbacks_run_without_holding_registration_lock() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open runtime store"),
        );
        let first_calls = Arc::new(AtomicUsize::new(0));
        let second_calls = Arc::new(AtomicUsize::new(0));
        let weak_store = Arc::downgrade(&store);
        let registered_second = Arc::new(AtomicBool::new(false));
        let registered_second_callback = Arc::clone(&registered_second);
        let first_count = Arc::clone(&first_calls);
        let second_count = Arc::clone(&second_calls);
        store
            .add_table_change_observer(ObservedTable::UsageResumes, move || {
                first_count.fetch_add(1, Ordering::AcqRel);
                if !registered_second_callback.swap(true, Ordering::AcqRel) {
                    let second_count = Arc::clone(&second_count);
                    weak_store
                        .upgrade()
                        .expect("store remains alive")
                        .add_table_change_observer(ObservedTable::UsageResumes, move || {
                            second_count.fetch_add(1, Ordering::AcqRel);
                        })
                        .expect("register callback from observer");
                }
            })
            .expect("register initial usage observer");

        store
            .transaction(|transaction| {
                transaction.execute(
                    "INSERT INTO runtime_usage_resumes(id, agent, record) VALUES ('resume-one', 'agent-a', '{}')",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("first usage write succeeds");
        assert_eq!(first_calls.load(Ordering::Acquire), 1);
        assert_eq!(second_calls.load(Ordering::Acquire), 0);

        store
            .transaction(|transaction| {
                transaction.execute(
                    "INSERT INTO runtime_usage_resumes(id, agent, record) VALUES ('resume-two', 'agent-a', '{}')",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("second usage write succeeds");
        assert_eq!(first_calls.load(Ordering::Acquire), 2);
        assert_eq!(second_calls.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn ambiguous_commit_wakes_table_observer_and_returns_unknown() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open runtime store");
        let notifications = Arc::new(AtomicUsize::new(0));
        let notified = Arc::clone(&notifications);
        store
            .add_table_change_observer(ObservedTable::AccessClients, move || {
                notified.fetch_add(1, Ordering::AcqRel);
            })
            .expect("register access-client observer");
        store
            .simulate_unknown_commit_after_success
            .store(true, Ordering::Release);
        let post_commit_calls = Arc::new(AtomicUsize::new(0));
        let post_commit_called = Arc::clone(&post_commit_calls);

        let result = store
            .transaction_with_post_commit(
                |transaction| {
                    transaction.execute(
                        "INSERT INTO runtime_access_clients(id, record) VALUES ('unknown-client', '{}')",
                        [],
                    )?;
                    Ok::<(), StoreError>(())
                },
                move |_| {
                    post_commit_called.fetch_add(1, Ordering::AcqRel);
                },
            )
            .await;

        assert!(matches!(result, Err(StoreError::CommitOutcomeUnknown(_))));
        assert_eq!(notifications.load(Ordering::Acquire), 1);
        assert_eq!(post_commit_calls.load(Ordering::Acquire), 0);
        let reader = store.read().expect("read committed access client");
        let count = reader
            .query_row(
                "SELECT COUNT(*) FROM runtime_access_clients WHERE id = 'unknown-client'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("query committed access client");
        assert_eq!(count, 1);
        assert_eq!(store.committed_change_generation(), 0);
    }

    #[tokio::test]
    async fn open_publishes_generation_baseline_before_runtime_writes() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open runtime store");

        assert_eq!(
            store.committed_change_generation(),
            0,
            "private schema and recovery commits establish the baseline before open returns"
        );
        store
            .transaction(|transaction| {
                transaction.execute_batch("CREATE TABLE generation_witness(value INTEGER);")?;
                transaction.execute("INSERT INTO generation_witness VALUES (1)", [])?;
                Ok(())
            })
            .await
            .expect("runtime write succeeds");
        assert_eq!(
            store.committed_change_generation(),
            1,
            "the first caller-visible changed commit advances from the published baseline"
        );
    }

    #[tokio::test]
    async fn post_commit_callback_runs_for_success_without_row_changes() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open runtime store");
        let callbacks = Arc::new(AtomicUsize::new(0));
        let callback_count = Arc::clone(&callbacks);
        let (value, generation) = store
            .transaction_with_post_commit(
                |_| Ok::<_, StoreError>(17),
                move |value| {
                    assert_eq!(*value, 17);
                    callback_count.fetch_add(1, Ordering::AcqRel);
                },
            )
            .await
            .expect("read-only transaction commits");

        assert_eq!(value, 17);
        assert_eq!(generation, 0);
        assert_eq!(callbacks.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn post_commit_callback_runs_even_when_commit_observer_panics() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open runtime store"),
        );
        store
            .transaction(|transaction| {
                transaction.execute_batch("CREATE TABLE post_commit_panic(value INTEGER);")?;
                Ok(())
            })
            .await
            .expect("create witness table");
        store.set_commit_notifier(|| panic!("expected commit observer panic"));
        let callbacks = Arc::new(AtomicUsize::new(0));
        let callback_count = Arc::clone(&callbacks);
        let task = {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                store
                    .transaction_with_post_commit(
                        |transaction| {
                            transaction.execute("INSERT INTO post_commit_panic VALUES (1)", [])?;
                            Ok::<_, StoreError>(())
                        },
                        move |_| {
                            callback_count.fetch_add(1, Ordering::AcqRel);
                        },
                    )
                    .await
            })
        };

        assert!(
            task.await
                .expect_err("observer panic propagates")
                .is_panic()
        );
        assert_eq!(callbacks.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn post_commit_callback_skips_operation_error_and_pre_enqueue_cancel() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open runtime store");
        let callbacks = Arc::new(AtomicUsize::new(0));
        let callback_count = Arc::clone(&callbacks);
        let refused = store
            .transaction_with_post_commit(
                |_| Err::<(), _>(StoreError::RecordContract("test rollback")),
                move |_| {
                    callback_count.fetch_add(1, Ordering::AcqRel);
                },
            )
            .await;
        assert!(refused.is_err());
        assert_eq!(callbacks.load(Ordering::Acquire), 0);

        let mut permits = Vec::with_capacity(sqlite_actor::MAX_PENDING_WRITES);
        for _ in 0..sqlite_actor::MAX_PENDING_WRITES {
            permits.push(store.writer.acquire().await.expect("fill actor admission"));
        }
        let callback_count = Arc::clone(&callbacks);
        let mut canceled = Box::pin(store.transaction_with_post_commit(
            |_| Ok::<_, StoreError>(()),
            move |_| {
                callback_count.fetch_add(1, Ordering::AcqRel);
            },
        ));
        let first_poll =
            std::future::poll_fn(|context| std::task::Poll::Ready(canceled.as_mut().poll(context)))
                .await;
        assert!(
            first_poll.is_pending(),
            "saturated admission waits before enqueue"
        );
        drop(canceled);
        drop(permits);
        tokio::task::yield_now().await;
        assert_eq!(callbacks.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn commit_notifies_after_releasing_writer_for_changed_rows() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open runtime store"),
        );
        let notifications = Arc::new(AtomicUsize::new(0));
        let notified = Arc::clone(&notifications);
        let weak_store = Arc::downgrade(&store);
        store.set_commit_notifier(move || {
            assert!(
                weak_store
                    .upgrade()
                    .expect("store remains alive")
                    .writer
                    .available_admissions()
                    == sqlite_actor::MAX_PENDING_WRITES
            );
            notified.fetch_add(1, Ordering::Relaxed);
        });
        let observer_notifications = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&observer_notifications);
        store.add_commit_observer(move || {
            observed.fetch_add(1, Ordering::Relaxed);
        });
        store
            .transaction(|transaction| {
                transaction.execute_batch("CREATE TABLE values_table(value INTEGER);")?;
                Ok(())
            })
            .await
            .expect("test table is created");

        store
            .transaction(|_| Ok(()))
            .await
            .expect("no-op transaction succeeds");
        assert_eq!(notifications.load(Ordering::Relaxed), 0);
        assert_eq!(store.committed_change_generation(), 0);

        store
            .transaction(|transaction| {
                transaction.execute("INSERT INTO values_table(value) VALUES (1)", [])?;
                Ok(())
            })
            .await
            .expect("write transaction succeeds");
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
        assert_eq!(store.committed_change_generation(), 1);

        let replacement_notifications = Arc::new(AtomicUsize::new(0));
        let replacement = Arc::clone(&replacement_notifications);
        store.set_commit_notifier(move || {
            replacement.fetch_add(1, Ordering::Relaxed);
        });
        store
            .transaction(|transaction| {
                transaction.execute("INSERT INTO values_table(value) VALUES (2)", [])?;
                Ok(())
            })
            .await
            .expect("write after notifier replacement succeeds");
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
        assert_eq!(replacement_notifications.load(Ordering::Relaxed), 1);
        assert_eq!(observer_notifications.load(Ordering::Relaxed), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn canceled_write_response_keeps_commit_notification() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open runtime store"),
        );
        store
            .transaction(|transaction| {
                transaction.execute_batch("CREATE TABLE canceled_write(value INTEGER);")?;
                Ok(())
            })
            .await
            .expect("create test table");
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&notifications);
        store.add_commit_observer(move || {
            observed.fetch_add(1, Ordering::Release);
        });
        let table_notifications = Arc::new(AtomicUsize::new(0));
        let table_observed = Arc::clone(&table_notifications);
        store
            .add_table_change_observer(ObservedTable::AccessClients, move || {
                table_observed.fetch_add(1, Ordering::Release);
            })
            .expect("register table observer");
        let post_commit_callbacks = Arc::new(AtomicUsize::new(0));

        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let entered_job = Arc::clone(&entered);
        let release_job = Arc::clone(&release);
        let first = {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                store
                    .transaction(move |_| {
                        entered_job.wait();
                        release_job.wait();
                        Ok::<(), StoreError>(())
                    })
                    .await
            })
        };
        entered.wait();

        let callback_count = Arc::clone(&post_commit_callbacks);
        let mut canceled = Box::pin(store.transaction_with_post_commit(
            |transaction| {
                transaction.execute("INSERT INTO canceled_write VALUES (1)", [])?;
                transaction.execute(
                    "INSERT INTO runtime_access_clients(id, record) VALUES ('canceled-client', '{}')",
                    [],
                )?;
                Ok::<(), StoreError>(())
            },
            move |_| {
                callback_count.fetch_add(1, Ordering::Release);
            },
        ));
        let first_poll =
            std::future::poll_fn(|context| std::task::Poll::Ready(canceled.as_mut().poll(context)))
                .await;
        assert!(
            first_poll.is_pending(),
            "canceled transaction has submitted its actor callback"
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while store.writer.available_admissions() != sqlite_actor::MAX_PENDING_WRITES - 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("canceled write is admitted to the actor queue");
        drop(canceled);
        release.wait();
        first
            .await
            .expect("first writer task")
            .expect("first transaction");
        tokio::time::timeout(Duration::from_secs(2), async {
            while notifications.load(Ordering::Acquire) != 1
                || table_notifications.load(Ordering::Acquire) != 1
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("abandoned committed write still notifies observers");
        assert_eq!(post_commit_callbacks.load(Ordering::Acquire), 1);
        assert_eq!(table_notifications.load(Ordering::Acquire), 1);

        let reader = store.read().expect("read committed row");
        let count = reader
            .query_row("SELECT COUNT(*) FROM canceled_write", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("count committed row");
        assert_eq!(count, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn queued_transaction_retains_local_identity_after_store_drop() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open runtime store"),
        );
        store
            .set_local_server_id(Some("lease-test-server".to_owned()))
            .expect("configure local server identity");

        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let entered_job = Arc::clone(&entered);
        let release_job = Arc::clone(&release);
        let (identity_sender, identity_receiver) = std::sync::mpsc::channel();
        let transaction_task = {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                store
                    .transaction(move |transaction| {
                        entered_job.wait();
                        release_job.wait();
                        let server_id = RuntimeStore::transaction_local_server_id(transaction)?;
                        identity_sender
                            .send(server_id)
                            .expect("test still receives callback identity");
                        Ok(())
                    })
                    .await
            })
        };
        entered.wait();
        transaction_task.abort();
        let _ = transaction_task.await;
        drop(store);

        release.wait();
        let server_id = tokio::task::spawn_blocking(move || {
            identity_receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("submitted callback completes")
        })
        .await
        .expect("identity assertion task");
        assert_eq!(server_id.as_deref(), Some("lease-test-server"));
    }
}

#[cfg(test)]
mod sync_pull_admission_tests {
    use super::*;
    use crate::SyncDocumentsRepository;
    use std::sync::Barrier;

    #[tokio::test(flavor = "multi_thread")]
    async fn read_only_pull_completes_while_analytics_transaction_is_held() {
        let directory = tempfile::tempdir().expect("isolated store directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("runtime store"),
        );
        let analytics_entered = Arc::new(Barrier::new(2));
        let analytics_release = Arc::new(Barrier::new(2));
        let analytics_store = Arc::clone(&store);
        let entered = Arc::clone(&analytics_entered);
        let release = Arc::clone(&analytics_release);
        let analytics = tokio::spawn(async move {
            analytics_store
                .analytics_transaction(move |_, _| {
                    entered.wait();
                    release.wait();
                    Ok::<_, StoreError>(())
                })
                .await
                .expect("held analytics transaction");
        });

        analytics_entered.wait();
        let connection = store.read().expect("read-only pull connection");
        let page = SyncDocumentsRepository::pull_drafts(&connection, 0, 100)
            .expect("read-only pull page completes while analytics is held");
        assert!(page.is_empty());
        drop(connection);
        analytics_release.wait();
        analytics.await.expect("analytics transaction task");
    }
}

#[cfg(test)]
mod projection_generation_tests {
    use super::*;

    #[tokio::test]
    async fn projection_watermark_tracks_changes_and_restarts_unset() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("store opens");
        assert!(
            store
                .entity_projections_need_maintenance("vault-a")
                .expect("watermark is readable")
        );

        let initial = store.committed_change_generation();
        store
            .mark_entity_projections_maintained(initial, "vault-a".to_owned())
            .expect("initial projection generation is recorded");
        assert!(
            !store
                .entity_projections_need_maintenance("vault-a")
                .expect("watermark is readable")
        );
        assert!(
            store
                .entity_projections_need_maintenance("vault-b")
                .expect("changed vault signature invalidates watermark")
        );
        store
            .transaction(|_| Ok(()))
            .await
            .expect("no-op transaction succeeds");
        assert!(
            !store
                .entity_projections_need_maintenance("vault-a")
                .expect("no-op preserves watermark")
        );

        store
            .transaction(|transaction| {
                transaction.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES (?1,?2)",
                    [
                        "generation-agent",
                        r#"{"id":"generation-agent","rootId":"generation-agent","epoch":0,"status":"idle"}"#,
                    ],
                )?;
                Ok(())
            })
            .await
            .expect("source record write succeeds");
        assert!(
            store
                .entity_projections_need_maintenance("vault-a")
                .expect("source write advances generation")
        );
        drop(store);

        let restarted = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("store reopens");
        assert!(
            restarted
                .entity_projections_need_maintenance("vault-a")
                .expect("restart begins without a process-local watermark")
        );
    }
}

#[cfg(test)]
mod account_worker_operation_lease_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use tokio::sync::Semaphore;

    #[test]
    fn account_worker_scope_future_does_not_store_operation_capture() {
        const SMALL_CAPTURE_BYTES: usize = 8;
        const LARGE_CAPTURE_BYTES: usize = 65_536;

        let account_slot = Arc::new(Semaphore::new(2));
        macro_rules! make_future {
            ($payload:ident) => {{
                let permit = Arc::clone(&account_slot)
                    .try_acquire_owned()
                    .expect("account operation slot is available");
                scope_account_worker_operation(permit, async move {
                    std::hint::black_box($payload);
                })
            }};
        }

        let small_payload = [0_u8; SMALL_CAPTURE_BYTES];
        let large_payload = [0_u8; LARGE_CAPTURE_BYTES];
        let small = make_future!(small_payload);
        let large = make_future!(large_payload);
        assert_eq!(
            std::mem::size_of_val(&small),
            std::mem::size_of_val(&large),
            "task-local scope future keeps the operation boxed"
        );
    }

    #[tokio::test]
    async fn admitted_write_keeps_account_operation_lease_after_caller_cancellation() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("runtime store opens"),
        );
        let (blocked_tx, blocked_rx) = std::sync::mpsc::channel();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let blocker_store = Arc::clone(&store);
        let blocker = tokio::spawn(async move {
            blocker_store
                .transaction(move |_| {
                    entered_tx
                        .send(())
                        .expect("blocked writer entry is observed");
                    blocked_rx.recv().expect("blocked writer is released");
                    Ok::<(), StoreError>(())
                })
                .await
        });
        entered_rx
            .await
            .expect("first actor callback owns the writer");

        let account_slot = Arc::new(Semaphore::new(1));
        let account_permit = Arc::clone(&account_slot)
            .acquire_owned()
            .await
            .expect("account operation slot is open");
        let mut queued = Box::pin(scope_account_worker_operation(
            account_permit,
            store.transaction(|_| Ok::<(), StoreError>(())),
        ));
        let first_poll =
            std::future::poll_fn(|context| std::task::Poll::Ready(queued.as_mut().poll(context)))
                .await;
        assert!(
            first_poll.is_pending(),
            "queued callback awaits its response"
        );
        assert_eq!(
            store.writer.available_admissions(),
            sqlite_actor::MAX_PENDING_WRITES - 2,
            "the blocked and queued callbacks both own writer admission"
        );
        drop(queued);
        assert_eq!(
            account_slot.available_permits(),
            0,
            "the admitted SQLite callback retains the bridge operation slot"
        );

        blocked_tx
            .send(())
            .expect("release the first actor callback");
        blocker
            .await
            .expect("blocked transaction task completes")
            .expect("blocked transaction commits");
        store
            .transaction(|_| Ok::<(), StoreError>(()))
            .await
            .expect("FIFO barrier follows the cancelled queued callback");
        assert_eq!(
            account_slot.available_permits(),
            1,
            "the bridge operation slot releases after the admitted callback"
        );
    }

    #[tokio::test]
    async fn cancelled_writer_admission_does_not_retain_account_operation_lease() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("runtime store opens"),
        );
        let mut actor_permits = Vec::with_capacity(sqlite_actor::MAX_PENDING_WRITES);
        for _ in 0..sqlite_actor::MAX_PENDING_WRITES {
            actor_permits.push(
                store
                    .writer
                    .acquire()
                    .await
                    .expect("fill writer admission capacity"),
            );
        }

        let account_slot = Arc::new(Semaphore::new(1));
        let account_permit = Arc::clone(&account_slot)
            .acquire_owned()
            .await
            .expect("account operation slot is open");
        let operation_ran = Arc::new(AtomicBool::new(false));
        let operation_ran_in_job = Arc::clone(&operation_ran);
        let mut waiting = Box::pin(scope_account_worker_operation(
            account_permit,
            store.transaction(move |_| {
                operation_ran_in_job.store(true, Ordering::Release);
                Ok::<(), StoreError>(())
            }),
        ));
        let first_poll =
            std::future::poll_fn(|context| std::task::Poll::Ready(waiting.as_mut().poll(context)))
                .await;
        assert!(first_poll.is_pending(), "writer admission is saturated");
        assert_eq!(store.writer_waiters.load(Ordering::Acquire), 1);
        drop(waiting);
        assert_eq!(
            account_slot.available_permits(),
            1,
            "pre-admission cancellation releases the bridge operation slot"
        );
        drop(actor_permits);
        store
            .transaction(|_| Ok::<(), StoreError>(()))
            .await
            .expect("barrier succeeds after admission permits are restored");
        assert!(
            !operation_ran.load(Ordering::Acquire),
            "cancelled pre-admission operation is never submitted"
        );
    }
}
