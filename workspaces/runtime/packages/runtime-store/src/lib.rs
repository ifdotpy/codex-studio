mod bootstrap;
mod feature_schema;
mod fresh_state;
mod multi_server_state_check;
mod path;
mod records;
mod repositories;
mod schema;
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
use std::io::BufReader;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::trace::{TraceEvent, TraceEventCodes};
pub use rusqlite::{Connection, Error as SqliteError, ErrorCode, Transaction};
use rusqlite::{OpenFlags, TransactionBehavior};
use thiserror::Error;

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
pub use repositories::event_write::{EventWriteReceipt, EventWriteRepository};
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
pub use repositories::rules::RulesRepository;
pub use repositories::rules_tick;
pub use repositories::scheduler::{ClaimedEvents, SchedulerRepository, TurnAttemptIdentity};
pub use repositories::sync_documents::{
    EntityPage, EntityRow, SyncDocument, SyncDocumentsRepository,
};
pub use repositories::terminals::{TerminalRow, TerminalsRepository};
pub use repositories::transcript::{
    TranscriptItem, TranscriptRepository, TranscriptStoreError, TranscriptStoreResult,
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
/// Maximum interval between maintenance checkpoints; the unit is elapsed seconds.
pub const WAL_CHECKPOINT_CADENCE: Duration = Duration::from_secs(60);
/// WAL size that permits an early maintenance checkpoint; the unit is bytes.
const WAL_CHECKPOINT_THRESHOLD_BYTES: u64 = 4 * 1024 * 1024;
/// Maximum concurrently checked-out read-only canvas connections per store.
const READ_CONNECTION_POOL_CAPACITY: usize = 4;

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
    #[error("nested read checkout from the same runtime store is not supported")]
    NestedRead,
}

pub type StoreResult<T> = Result<T, StoreError>;

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
    _runtime_lock: RuntimeLock,
    writer: Mutex<Connection>,
    analytics: Mutex<Connection>,
    version_reader: Mutex<Connection>,
    read_pool: Arc<ReadConnectionPool>,
    writer_waiters: AtomicUsize,
    writer_waiter_peak: AtomicUsize,
    identity_registry_path: PathBuf,
    commit_notifiers: Mutex<CommitNotifiers>,
    /// Count of committed transactions that changed rows; owned and advanced by the writer.
    committed_change_generation: AtomicU64,
    last_entity_projection_state: Mutex<Option<(u64, String)>>,
    entity_projection_maintenance_lock: Mutex<()>,
    wal_checkpoint_requested: AtomicBool,
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
                if result.is_err() {
                    if let Ok(mut state) = self.state.lock() {
                        state.opened = state.opened.saturating_sub(1);
                        self.available.notify_one();
                    }
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

impl RuntimeStore {
    /// Canonical state root for adapters that must resolve content-addressed receipts.
    pub fn state_dir(&self) -> &Path {
        &self.root
    }

    /// Open an existing state directory, bootstrapping the sync family on first open.
    pub fn open(state_dir: Option<&Path>) -> StoreResult<Self> {
        let root = match state_dir {
            Some(path) => path::resolve_explicit_state_dir(path)?,
            None => resolve_state_dir()?,
        };
        fresh_state::validate(&root)?;
        let canvas_path = root.join("canvas.sqlite3");
        let analytics_path = root.join("analytics.sqlite3");
        let runtime_lock = RuntimeLock::acquire(&root)?;
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

        let writer = open_database(&canvas_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        writer.pragma_update(None, "synchronous", "FULL")?;
        schema::register_python_trigger_functions(&writer)?;
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
            _runtime_lock: runtime_lock,
            writer: Mutex::new(writer),
            analytics: Mutex::new(analytics),
            version_reader: Mutex::new(version_reader),
            read_pool: Arc::new(ReadConnectionPool::new(canvas_path.clone())),
            writer_waiters: AtomicUsize::new(0),
            writer_waiter_peak: AtomicUsize::new(0),
            identity_registry_path,
            commit_notifiers: Mutex::new(CommitNotifiers::default()),
            committed_change_generation: AtomicU64::new(0),
            last_entity_projection_state: Mutex::new(None),
            entity_projection_maintenance_lock: Mutex::new(()),
            wal_checkpoint_requested: AtomicBool::new(false),
            last_wal_checkpoint: Mutex::new(Instant::now()),
        };
        store.transaction(|transaction| {
            bootstrap::prepare_sync_schema(transaction)?;
            bootstrap::prepare_runtime_event_indexes(transaction)?;
            feature_schema::prepare(transaction)?;
            schema::verify_bootstrap(transaction)
        })?;
        let recovery_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs_f64())
            .unwrap_or_default();
        store.transaction(|transaction| {
            CheckpointsRepository::recover_captures(transaction, recovery_time)
        })?;
        store.checkpoint_wal_passive()?;
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
    pub fn set_diagnostics_callback(&self, callback: Option<StoreDiagnosticsCallback>) {
        if let Ok(mut current) = diagnostics_callback().lock() {
            DIAGNOSTICS_ENABLED.store(callback.is_some(), Ordering::Release);
            *current = callback;
        }
        if let Ok(mut writer) = self.writer.lock() {
            install_profile(&mut writer, StoreDatabaseKind::Canvas);
        }
        if let Ok(mut analytics) = self.analytics.lock() {
            install_profile(&mut analytics, StoreDatabaseKind::Analytics);
        }
        if let Ok(mut reader) = self.version_reader.lock() {
            install_profile(&mut reader, StoreDatabaseKind::Canvas);
        }
        if let Ok(mut pool) = self.read_pool.state.lock() {
            for connection in &mut pool.idle {
                install_profile(connection, StoreDatabaseKind::Canvas);
            }
        }
    }

    /// Checkpoint both WAL files without waiting on active readers or truncating them.
    pub fn checkpoint_wal_passive(&self) -> StoreResult<()> {
        let writer = self.writer.lock().map_err(|_| StoreError::WriterPoisoned)?;
        self.checkpoint_connection(
            &writer,
            StoreDatabaseKind::Canvas,
            &self.root.join("canvas.sqlite3-wal"),
        )?;
        drop(writer);

        let analytics = self
            .analytics
            .lock()
            .map_err(|_| StoreError::WriterPoisoned)?;
        self.checkpoint_connection(
            &analytics,
            StoreDatabaseKind::Analytics,
            &self.root.join("analytics.sqlite3-wal"),
        )?;
        Ok(())
    }

    /// Coalesce write requests and apply WAL threshold/cadence policy from the maintenance owner.
    /// This method performs filesystem and SQLite work; call it only in a bounded blocking stage.
    pub fn checkpoint_wal_if_due(&self) -> StoreResult<bool> {
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
        if let Err(error) = self.checkpoint_wal_passive() {
            self.wal_checkpoint_requested.store(true, Ordering::Release);
            return Err(error);
        }
        *self
            .last_wal_checkpoint
            .lock()
            .map_err(|_| StoreError::WriterPoisoned)? = Instant::now();
        Ok(true)
    }

    pub(crate) fn request_wal_checkpoint(&self) {
        self.wal_checkpoint_requested.store(true, Ordering::Release);
    }

    fn checkpoint_connection(
        &self,
        connection: &Connection,
        database: StoreDatabaseKind,
        wal_path: &Path,
    ) -> StoreResult<()> {
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
        let (notifier, observers) = {
            let notifiers = self
                .commit_notifiers
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

    /// Run one durable transaction while holding the sole serialized writer.
    pub fn transaction<T>(
        &self,
        operation: impl FnOnce(&Transaction<'_>) -> StoreResult<T>,
    ) -> StoreResult<T> {
        self.transaction_with(operation)
    }

    /// Run one durable transaction with a domain error type that preserves
    /// operation refusals while still reporting store and commit failures.
    pub fn transaction_with<T, E>(
        &self,
        operation: impl FnOnce(&Transaction<'_>) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<StoreError>,
    {
        self.transaction_with_commit_generation(operation)
            .map(|(value, _)| value)
    }

    /// Run a write transaction and return its committed change generation.
    /// The generation unit is one committed transaction with row changes.
    pub fn transaction_with_commit_generation<T, E>(
        &self,
        operation: impl FnOnce(&Transaction<'_>) -> Result<T, E>,
    ) -> Result<(T, u64), E>
    where
        E: From<StoreError>,
    {
        let diagnostics_enabled = DIAGNOSTICS_ENABLED.load(Ordering::Acquire);
        let waiting = diagnostics_enabled.then(Instant::now);
        let mut did_wait = false;
        let (mut writer, waiting_threads) = match self.writer.try_lock() {
            Ok(writer) => (writer, self.writer_waiters.load(Ordering::Relaxed)),
            Err(std::sync::TryLockError::WouldBlock) => {
                did_wait = true;
                let active_waiters = self.writer_waiters.fetch_add(1, Ordering::Relaxed) + 1;
                let peak_waiters = self
                    .writer_waiter_peak
                    .fetch_max(active_waiters, Ordering::Relaxed)
                    .max(active_waiters);
                emit_diagnostic(StoreDiagnosticsEvent::WriterWaiters {
                    waiting_threads: active_waiters,
                    peak_waiters,
                });
                let writer = match self.writer.lock() {
                    Ok(writer) => writer,
                    Err(_) => {
                        let waiting_threads =
                            self.writer_waiters.fetch_sub(1, Ordering::Relaxed) - 1;
                        if diagnostics_enabled {
                            emit_diagnostic(StoreDiagnosticsEvent::WriterWaiters {
                                waiting_threads,
                                peak_waiters: self.writer_waiter_peak.load(Ordering::Relaxed),
                            });
                            emit_diagnostic(StoreDiagnosticsEvent::WriterAdmission {
                                wait: waiting.map_or(Duration::ZERO, |started| started.elapsed()),
                                waited: true,
                                waiting_threads,
                                peak_waiters: self.writer_waiter_peak.load(Ordering::Relaxed),
                            });
                        }
                        return Err(StoreError::WriterPoisoned.into());
                    }
                };
                let waiting_threads = self.writer_waiters.fetch_sub(1, Ordering::Relaxed) - 1;
                emit_diagnostic(StoreDiagnosticsEvent::WriterWaiters {
                    waiting_threads,
                    peak_waiters: self.writer_waiter_peak.load(Ordering::Relaxed),
                });
                (writer, waiting_threads)
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(StoreError::WriterPoisoned.into());
            }
        };
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
        let outcome = (|| {
            let transaction = writer
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(StoreError::from)
                .map_err(E::from)?;
            let initial_changes = transaction.total_changes();
            match operation(&transaction) {
                Ok(value) => {
                    let changed = transaction.total_changes() != initial_changes;
                    let commit_started = diagnostics_enabled.then(Instant::now);
                    transaction
                        .commit()
                        .map_err(StoreError::CommitOutcomeUnknown)?;
                    if let Some(started) = commit_started {
                        emit_diagnostic(StoreDiagnosticsEvent::CommitDuration {
                            database: StoreDatabaseKind::Canvas,
                            elapsed: started.elapsed(),
                        });
                    }
                    if changed {
                        self.committed_change_generation
                            .fetch_add(1, Ordering::Release);
                    }
                    let generation = self.committed_change_generation();
                    Ok((value, generation, changed))
                }
                Err(error) => {
                    // Dropping rolls back best-effort. Preserve the operation's
                    // error if SQLite also reports a rollback failure.
                    drop(transaction);
                    Err(error)
                }
            }
        })();
        let hold = holding.map_or(Duration::ZERO, |started| started.elapsed());
        drop(writer);
        let outcome = outcome.map(|(value, generation, changed)| {
            if changed {
                self.notify_commit();
            }
            (value, generation)
        });
        if diagnostics_enabled {
            emit_diagnostic(StoreDiagnosticsEvent::WriterTransaction { wait, hold });
        }
        outcome
    }

    pub fn transaction_with_error<T, E>(
        &self,
        operation: impl FnOnce(&Transaction<'_>) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<StoreError>,
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
    pub fn lock_entity_projection_maintenance(&self) -> StoreResult<MutexGuard<'_, ()>> {
        self.entity_projection_maintenance_lock
            .lock()
            .map_err(|_| StoreError::WriterPoisoned)
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

impl Drop for RuntimeStore {
    fn drop(&mut self) {
        let Ok(mut registry) = identity_registry().lock() else {
            return;
        };
        if let Some(entry) = registry.get_mut(&self.identity_registry_path) {
            entry.stores = entry.stores.saturating_sub(1);
            if entry.stores == 0 {
                registry.remove(&self.identity_registry_path);
            }
        }
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

    #[test]
    fn sequential_reads_reuse_a_bounded_connection() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let canvas_path = directory.path().join("canvas.sqlite3");
        Connection::open(&canvas_path).expect("canvas database");
        let store = RuntimeStore {
            root: directory.path().to_path_buf(),
            _runtime_lock: RuntimeLock::acquire(directory.path()).expect("runtime lock"),
            writer: Mutex::new(Connection::open_in_memory().expect("writer connection")),
            analytics: Mutex::new(Connection::open_in_memory().expect("analytics connection")),
            version_reader: Mutex::new(Connection::open_in_memory().expect("version reader")),
            read_pool: Arc::new(ReadConnectionPool::new(canvas_path)),
            writer_waiters: AtomicUsize::new(0),
            writer_waiter_peak: AtomicUsize::new(0),
            identity_registry_path: directory.path().join("canvas.sqlite3"),
            commit_notifiers: Mutex::new(CommitNotifiers::default()),
            committed_change_generation: AtomicU64::new(0),
            last_entity_projection_state: Mutex::new(None),
            entity_projection_maintenance_lock: Mutex::new(()),
            wal_checkpoint_requested: AtomicBool::new(false),
            last_wal_checkpoint: Mutex::new(Instant::now()),
        };

        for _ in 0..16 {
            let reader = store.read().expect("read checkout");
            assert_eq!(
                reader
                    .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                1
            );
        }

        assert_eq!(store.read_pool.opened_count(), 1);
        assert!(READ_CONNECTION_POOL_CAPACITY > 1);
    }

    #[test]
    fn nested_read_fails_fast_when_all_pooled_connections_are_checked_out() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let canvas_path = directory.path().join("canvas.sqlite3");
        Connection::open(&canvas_path).expect("canvas database");
        let store = Arc::new(RuntimeStore {
            root: directory.path().to_path_buf(),
            _runtime_lock: RuntimeLock::acquire(directory.path()).expect("runtime lock"),
            writer: Mutex::new(Connection::open_in_memory().expect("writer connection")),
            analytics: Mutex::new(Connection::open_in_memory().expect("analytics connection")),
            version_reader: Mutex::new(Connection::open_in_memory().expect("version reader")),
            read_pool: Arc::new(ReadConnectionPool::new(canvas_path)),
            writer_waiters: AtomicUsize::new(0),
            writer_waiter_peak: AtomicUsize::new(0),
            identity_registry_path: directory.path().join("canvas.sqlite3"),
            commit_notifiers: Mutex::new(CommitNotifiers::default()),
            committed_change_generation: AtomicU64::new(0),
            last_entity_projection_state: Mutex::new(None),
            entity_projection_maintenance_lock: Mutex::new(()),
            wal_checkpoint_requested: AtomicBool::new(false),
            last_wal_checkpoint: Mutex::new(Instant::now()),
        });
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

    #[test]
    fn optional_hook_observes_writer_and_sql_timing_and_can_be_cleared() {
        let _test_guard = TEST_LOCK.lock().expect("serialize diagnostics hook tests");
        let directory = tempfile::tempdir().expect("temporary state directory");
        let canvas_path = directory.path().join("canvas.sqlite3");
        Connection::open(&canvas_path)
            .expect("create read pool database")
            .close()
            .expect("close read pool database");
        let writer = Connection::open_in_memory().expect("writer connection");
        let reader = Connection::open_in_memory().expect("reader connection");
        let store = RuntimeStore {
            root: directory.path().to_path_buf(),
            _runtime_lock: RuntimeLock::acquire(directory.path()).expect("runtime lock"),
            writer: Mutex::new(writer),
            analytics: Mutex::new(Connection::open_in_memory().expect("analytics connection")),
            version_reader: Mutex::new(reader),
            read_pool: Arc::new(ReadConnectionPool::new(canvas_path)),
            writer_waiters: AtomicUsize::new(0),
            writer_waiter_peak: AtomicUsize::new(0),
            identity_registry_path: directory.path().join("canvas.sqlite3"),
            commit_notifiers: Mutex::new(CommitNotifiers::default()),
            committed_change_generation: AtomicU64::new(0),
            last_entity_projection_state: Mutex::new(None),
            entity_projection_maintenance_lock: Mutex::new(()),
            wal_checkpoint_requested: AtomicBool::new(false),
            last_wal_checkpoint: Mutex::new(Instant::now()),
        };
        EVENTS.lock().expect("diagnostic events lock").clear();
        store.set_diagnostics_callback(Some(record_event));
        store
            .transaction(|transaction| {
                transaction.execute_batch("CREATE TABLE measured(value INTEGER);")?;
                transaction.execute("INSERT INTO measured(value) VALUES (1)", [])?;
                Ok(())
            })
            .expect("transaction succeeds");
        assert!(EVENTS.lock().expect("diagnostic events lock").len() >= 3);

        EVENTS.lock().expect("diagnostic events lock").clear();
        let writer_guard = store.writer.lock().expect("hold writer");
        let writer_wait_observed = std::thread::scope(|scope| {
            let waiter = scope.spawn(|| store.transaction(|_| Ok::<(), StoreError>(())));
            let observed = wait_for_event(|event| {
                matches!(
                    event,
                    StoreDiagnosticsEvent::WriterWaiters {
                        waiting_threads: 1,
                        ..
                    }
                )
            });
            drop(writer_guard);
            waiter
                .join()
                .expect("writer waiter thread")
                .expect("writer wait completes");
            observed
        });
        assert!(
            writer_wait_observed,
            "writer queue occupancy was not published"
        );
        assert_eq!(
            EVENTS
                .lock()
                .expect("diagnostic events lock")
                .iter()
                .filter(|event| matches!(
                    event,
                    StoreDiagnosticsEvent::WriterAdmission { waited: true, .. }
                ))
                .count(),
            1,
            "one completed wait must produce one admission latency sample"
        );

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

        store.set_diagnostics_callback(None);
        let previous = EVENTS.lock().expect("diagnostic events lock").len();
        store
            .transaction(|transaction| {
                transaction.execute("INSERT INTO measured(value) VALUES (2)", [])?;
                Ok(())
            })
            .expect("transaction succeeds with no observer");
        assert_eq!(
            EVENTS.lock().expect("diagnostic events lock").len(),
            previous
        );
    }

    #[test]
    fn analytics_writer_publishes_queue_occupancy_before_blocking() {
        let _test_guard = TEST_LOCK.lock().expect("serialize diagnostics hook tests");
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store =
            Arc::new(RuntimeStore::open(Some(directory.path())).expect("open runtime store"));
        EVENTS.lock().expect("diagnostic events lock").clear();
        store.set_diagnostics_callback(Some(record_event));
        let writer_guard = store.writer.lock().expect("hold canvas writer");

        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                store
                    .analytics_transaction(|_, _| Ok::<(), StoreError>(()))
                    .expect("analytics transaction succeeds after writer release");
            });
            assert!(
                wait_for_event(|event| {
                    matches!(
                        event,
                        StoreDiagnosticsEvent::WriterWaiters {
                            waiting_threads: 1,
                            ..
                        }
                    )
                }),
                "analytics writer must publish occupancy before waiting on the writer"
            );
            assert_eq!(
                EVENTS
                    .lock()
                    .expect("diagnostic events lock")
                    .iter()
                    .filter(|event| matches!(
                        event,
                        StoreDiagnosticsEvent::WriterAdmission { waited: true, .. }
                    ))
                    .count(),
                0,
                "a blocked analytics admission has no completed latency sample yet"
            );
            drop(writer_guard);
            waiter.join().expect("analytics waiter thread");
        });

        let events = EVENTS.lock().expect("diagnostic events lock");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    StoreDiagnosticsEvent::WriterAdmission { waited: true, .. }
                ))
                .count(),
            1,
            "completed analytics wait contributes exactly one admission sample"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                StoreDiagnosticsEvent::WriterWaiters {
                    waiting_threads: 0,
                    ..
                }
            )),
            "analytics waiter departure must publish the new occupancy"
        );
        store.set_diagnostics_callback(None);
    }

    #[test]
    fn sync_pull_completion_after_a_commit_does_not_checkpoint_the_wal() {
        let _test_guard = TEST_LOCK.lock().expect("serialize diagnostics hook tests");
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path())).expect("open runtime store");
        EVENTS.lock().expect("diagnostic events lock").clear();
        store.set_diagnostics_callback(Some(record_event));
        store
            .transaction(|transaction| {
                transaction.execute_batch("CREATE TABLE checkpoint_witness(value INTEGER);")?;
                transaction.execute("INSERT INTO checkpoint_witness VALUES (1)", [])?;
                Ok(())
            })
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
        store.set_diagnostics_callback(None);
    }
}

#[cfg(test)]
mod commit_notification_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn commit_notifies_after_releasing_writer_for_changed_rows() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(RuntimeStore {
            root: directory.path().to_path_buf(),
            _runtime_lock: RuntimeLock::acquire(directory.path()).expect("runtime lock"),
            writer: Mutex::new(Connection::open_in_memory().expect("writer connection")),
            analytics: Mutex::new(Connection::open_in_memory().expect("analytics connection")),
            version_reader: Mutex::new(Connection::open_in_memory().expect("reader connection")),
            read_pool: Arc::new(ReadConnectionPool::new(
                directory.path().join("canvas.sqlite3"),
            )),
            writer_waiters: AtomicUsize::new(0),
            writer_waiter_peak: AtomicUsize::new(0),
            identity_registry_path: directory.path().join("canvas.sqlite3"),
            commit_notifiers: Mutex::new(CommitNotifiers::default()),
            committed_change_generation: AtomicU64::new(0),
            last_entity_projection_state: Mutex::new(None),
            entity_projection_maintenance_lock: Mutex::new(()),
            wal_checkpoint_requested: AtomicBool::new(false),
            last_wal_checkpoint: Mutex::new(Instant::now()),
        });
        let notifications = Arc::new(AtomicUsize::new(0));
        let notified = Arc::clone(&notifications);
        let weak_store = Arc::downgrade(&store);
        store.set_commit_notifier(move || {
            assert!(
                weak_store
                    .upgrade()
                    .expect("store remains alive")
                    .writer
                    .try_lock()
                    .is_ok()
            );
            notified.fetch_add(1, Ordering::Relaxed);
        });
        let observer_notifications = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&observer_notifications);
        store.add_commit_observer(move || {
            observed.fetch_add(1, Ordering::Relaxed);
        });
        store
            .writer
            .lock()
            .expect("writer mutex")
            .execute_batch("CREATE TABLE values_table(value INTEGER);")
            .expect("test table is created");

        store
            .transaction(|_| Ok(()))
            .expect("no-op transaction succeeds");
        assert_eq!(notifications.load(Ordering::Relaxed), 0);
        assert_eq!(store.committed_change_generation(), 0);

        store
            .transaction(|transaction| {
                transaction.execute("INSERT INTO values_table(value) VALUES (1)", [])?;
                Ok(())
            })
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
            .expect("write after notifier replacement succeeds");
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
        assert_eq!(replacement_notifications.load(Ordering::Relaxed), 1);
        assert_eq!(observer_notifications.load(Ordering::Relaxed), 2);
    }
}

#[cfg(test)]
mod sync_pull_admission_tests {
    use super::*;
    use crate::SyncDocumentsRepository;
    use std::sync::Barrier;

    #[test]
    fn read_only_pull_completes_while_analytics_transaction_is_held() {
        let directory = tempfile::tempdir().expect("isolated store directory");
        let store = Arc::new(RuntimeStore::open(Some(directory.path())).expect("runtime store"));
        let analytics_entered = Arc::new(Barrier::new(2));
        let analytics_release = Arc::new(Barrier::new(2));
        let analytics_store = Arc::clone(&store);
        let entered = Arc::clone(&analytics_entered);
        let release = Arc::clone(&analytics_release);
        let analytics = std::thread::spawn(move || {
            analytics_store
                .analytics_transaction(|_, _| {
                    entered.wait();
                    release.wait();
                    Ok::<_, StoreError>(())
                })
                .expect("held analytics transaction");
        });

        analytics_entered.wait();
        let connection = store.read().expect("read-only pull connection");
        let page = SyncDocumentsRepository::pull_drafts(&connection, 0, 100)
            .expect("read-only pull page completes while analytics is held");
        assert!(page.is_empty());
        drop(connection);
        analytics_release.wait();
        analytics.join().expect("analytics transaction thread");
    }
}

#[cfg(test)]
mod projection_generation_tests {
    use super::*;

    #[test]
    fn projection_watermark_tracks_changes_and_restarts_unset() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path())).expect("store opens");
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
            .expect("source record write succeeds");
        assert!(
            store
                .entity_projections_need_maintenance("vault-a")
                .expect("source write advances generation")
        );
        drop(store);

        let restarted = RuntimeStore::open(Some(directory.path())).expect("store reopens");
        assert!(
            restarted
                .entity_projections_need_maintenance("vault-a")
                .expect("restart begins without a process-local watermark")
        );
    }
}
