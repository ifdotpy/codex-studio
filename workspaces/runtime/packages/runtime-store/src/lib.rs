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

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
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
pub use repositories::usage::{UsageRepositoryError, cancel_capacity_retry, disable_usage_resume};
pub use repositories::voice::{VoiceDelivery, VoiceRecord, VoiceRepository};
pub use repositories::work_items::{StoredQueueEvent, StoredRecord, WorkItemsRepository};
pub use repositories::workspace_files::{StoredWorkspaceAsset, WorkspaceFilesRepository};
pub use repositories::workspace_operations;
pub use repositories::workspace_ready;
pub use repositories::workspace_views::{WorkspaceTaskRow, WorkspaceViewsRepository};

const DATABASE_BUSY_TIMEOUT_MS: u32 = 15_000;

/// Value-free observations emitted by the optional diagnostics callback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreDiagnosticsEvent {
    WriterTransaction {
        wait: Duration,
        hold: Duration,
    },
    SqlProfile {
        database: StoreDatabaseKind,
        elapsed: Duration,
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

fn profile_statement(event: TraceEvent<'_>) {
    if let TraceEvent::Profile(_statement, elapsed) = event {
        emit_diagnostic(StoreDiagnosticsEvent::SqlProfile {
            database: StoreDatabaseKind::Canvas,
            elapsed,
        });
    }
}

fn install_profile(connection: &mut Connection) {
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
        enabled.then_some(profile_statement),
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
    identity_registry_path: PathBuf,
    commit_notifiers: Mutex<CommitNotifiers>,
    /// Count of committed transactions that changed rows; owned and advanced by the writer.
    committed_change_generation: AtomicU64,
    last_entity_projection_state: Mutex<Option<(u64, String)>>,
    entity_projection_maintenance_lock: Mutex<()>,
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
        let mut analytics = open_database(&analytics_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
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
            identity_registry_path,
            commit_notifiers: Mutex::new(CommitNotifiers::default()),
            committed_change_generation: AtomicU64::new(0),
            last_entity_projection_state: Mutex::new(None),
            entity_projection_maintenance_lock: Mutex::new(()),
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
            install_profile(&mut writer);
        }
        if let Ok(mut reader) = self.version_reader.lock() {
            install_profile(&mut reader);
        }
    }

    fn notify_commit(&self) {
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
        let mut writer = self.writer.lock().map_err(|_| StoreError::WriterPoisoned)?;
        let wait = waiting.map_or(Duration::ZERO, |started| started.elapsed());
        let holding = diagnostics_enabled.then(Instant::now);
        let outcome = (|| {
            repositories::sync_documents::reset_sequence_high_water(&writer)
                .map_err(StoreError::from)
                .map_err(E::from)?;
            let transaction = writer
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(StoreError::from)
                .map_err(E::from)?;
            let initial_changes = transaction.total_changes();
            match operation(&transaction) {
                Ok(value) => {
                    let changed = transaction.total_changes() != initial_changes;
                    transaction
                        .commit()
                        .map_err(StoreError::CommitOutcomeUnknown)?;
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
    pub fn read(&self) -> StoreResult<Connection> {
        let mut connection = open_database(
            &self.root.join("canvas.sqlite3"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        install_profile(&mut connection);
        connection.pragma_update(None, "query_only", "ON")?;
        Ok(connection)
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
    install_profile(&mut connection);
    connection.busy_timeout(std::time::Duration::from_millis(u64::from(
        DATABASE_BUSY_TIMEOUT_MS,
    )))?;
    Ok(connection)
}

#[cfg(test)]
mod diagnostics_hook_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static EVENTS: AtomicUsize = AtomicUsize::new(0);

    fn count_event(_event: StoreDiagnosticsEvent) {
        EVENTS.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn optional_hook_observes_writer_and_sql_timing_and_can_be_cleared() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let writer = Connection::open_in_memory().expect("writer connection");
        let reader = Connection::open_in_memory().expect("reader connection");
        let store = RuntimeStore {
            root: directory.path().to_path_buf(),
            _runtime_lock: RuntimeLock::acquire(directory.path()).expect("runtime lock"),
            writer: Mutex::new(writer),
            analytics: Mutex::new(Connection::open_in_memory().expect("analytics connection")),
            version_reader: Mutex::new(reader),
            identity_registry_path: directory.path().join("canvas.sqlite3"),
            commit_notifiers: Mutex::new(CommitNotifiers::default()),
            committed_change_generation: AtomicU64::new(0),
            last_entity_projection_state: Mutex::new(None),
            entity_projection_maintenance_lock: Mutex::new(()),
        };
        EVENTS.store(0, Ordering::Relaxed);
        store.set_diagnostics_callback(Some(count_event));
        store
            .transaction(|transaction| {
                transaction.execute_batch("CREATE TABLE measured(value INTEGER);")?;
                transaction.execute("INSERT INTO measured(value) VALUES (1)", [])?;
                Ok(())
            })
            .expect("transaction succeeds");
        assert!(EVENTS.load(Ordering::Relaxed) >= 3);

        store.set_diagnostics_callback(None);
        let previous = EVENTS.load(Ordering::Relaxed);
        store
            .transaction(|transaction| {
                transaction.execute("INSERT INTO measured(value) VALUES (2)", [])?;
                Ok(())
            })
            .expect("transaction succeeds with no observer");
        assert_eq!(EVENTS.load(Ordering::Relaxed), previous);
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
            identity_registry_path: directory.path().join("canvas.sqlite3"),
            commit_notifiers: Mutex::new(CommitNotifiers::default()),
            committed_change_generation: AtomicU64::new(0),
            last_entity_projection_state: Mutex::new(None),
            entity_projection_maintenance_lock: Mutex::new(()),
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
