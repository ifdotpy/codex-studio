mod bootstrap;
mod feature_schema;
mod path;
mod records;
mod repositories;
mod schema;
pub use schema::{python_json, python_json_compact, sha256_hex, truthy};
mod state_check;
pub use state_check::{StateCheckFailure, StateCheckReport, StateCheckTable, check_state_readonly};

pub use repositories::account_state;
pub use repositories::analytics::{
    self, AnalyticsError, AnalyticsOptions, AnalyticsRepository, AnalyticsStoreError,
    SessionCostError,
};
pub use repositories::checkpoints::{
    CheckpointsRepository, StoredCheckpointObject, WorkspaceSnapshot,
};
pub use repositories::cost_usage::{CostUsageRepositoryError, CostUsageRow, read_cost_usage};
pub use repositories::remote_transport::{OutboundReservation, RemoteTransportRepository};

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub use rusqlite::{Connection, Transaction};
use rusqlite::{OpenFlags, TransactionBehavior};
use thiserror::Error;

pub use path::resolve_state_dir;
pub use records::{
    AgentId, AgentRecord, AgentRepository, AgentStatus, AnnotationRecord, ChatMessage,
    ChatMessageId, CheckpointRecord, ComplaintRecord, EventMetadataRecord, GraphAgentRecord,
    JsonRecord, MessageRepository, MonitorRecord, PlanRecord, QueueEvent, QueueEventId,
    QueueEventRepository, RecordKeyMode, RecordOrder, RecordPredicate, RecordQueryShape,
    RequestRecord, RuleRecord, Table, TaskId, TaskRecord, TaskRepository, TerminalRecord,
    TranscriptAssetId, TranscriptAssetRecord, TranscriptItemId, TranscriptItemRecord,
    UsageResumeRecord, WorkItemRecord, WorkStatus,
};
pub use repositories::coordination::{CanvasMessage, ChatGroup, CoordinationRepository};
pub use repositories::event_write::{EventWriteReceipt, EventWriteRepository};
pub use repositories::federation::FederationRepository;
pub use repositories::monitors::{MonitorsRepository, StoredMonitor};
pub use repositories::multi_server::MultiServerRepository;
pub use repositories::profiles::{ProfileRecord, ProfileRepository, ProfileRole, ReasoningEffort};
pub use repositories::projects::ProjectsRepository;
pub use repositories::questions_reads::QuestionsReadsRepository;
pub use repositories::questions_write::QuestionsWriteRepository;
pub use repositories::queue_write::{PendingEvent, QueueWriteRepository};
pub use repositories::rules::RulesRepository;
pub use repositories::scheduler::{ClaimedEvents, SchedulerRepository};
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
pub use repositories::workspace_views::{WorkspaceTaskRow, WorkspaceViewsRepository};

const DATABASE_BUSY_TIMEOUT_MS: u32 = 15_000;

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
    version_reader: Mutex<Connection>,
    commit_notifier: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl RuntimeStore {
    /// Open an existing state directory, bootstrapping the sync family on first open.
    pub fn open(state_dir: Option<&Path>) -> StoreResult<Self> {
        let root = match state_dir {
            Some(path) => path::resolve_explicit_state_dir(path)?,
            None => resolve_state_dir()?,
        };
        let runtime_lock = RuntimeLock::acquire(&root)?;

        let canvas_path = root.join("canvas.sqlite3");
        let analytics_path = root.join("analytics.sqlite3");
        for database in [&canvas_path, &analytics_path] {
            if !database.is_file() {
                return Err(StoreError::MissingDatabase(database.clone()));
            }
        }
        schema::verify_core(&canvas_path, &analytics_path)?;

        let writer = open_database(&canvas_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        writer.pragma_update(None, "synchronous", "FULL")?;
        schema::register_python_trigger_functions(&writer)?;
        let version_reader = open_database(&canvas_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        version_reader.pragma_update(None, "query_only", "ON")?;
        let store = Self {
            root,
            _runtime_lock: runtime_lock,
            writer: Mutex::new(writer),
            version_reader: Mutex::new(version_reader),
            commit_notifier: Mutex::new(None),
        };
        store.transaction(|transaction| {
            bootstrap::prepare_sync_schema(transaction)?;
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

    /// Install the process-local notification fired after each successful write commit.
    pub fn set_commit_notifier(&self, notifier: impl Fn() + Send + Sync + 'static) {
        let mut slot = self
            .commit_notifier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some(Arc::new(notifier));
    }

    fn notify_commit(&self) {
        let notifier = self
            .commit_notifier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .cloned();
        if let Some(notifier) = notifier {
            notifier();
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
        let mut writer = self.writer.lock().map_err(|_| StoreError::WriterPoisoned)?;
        let transaction = writer
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StoreError::from)
            .map_err(E::from)?;
        match operation(&transaction) {
            Ok(value) => {
                transaction
                    .commit()
                    .map_err(StoreError::CommitOutcomeUnknown)?;
                self.notify_commit();
                Ok(value)
            }
            Err(error) => {
                // Dropping rolls back best-effort. Preserve the operation's
                // error if SQLite also reports a rollback failure.
                drop(transaction);
                Err(error)
            }
        }
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
        let connection = open_database(
            &self.root.join("canvas.sqlite3"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        connection.pragma_update(None, "query_only", "ON")?;
        Ok(connection)
    }

    /// Run related reads in one SQLite snapshot.
    pub fn read_transaction<T>(
        &self,
        operation: impl FnOnce(&Transaction<'_>) -> StoreResult<T>,
    ) -> StoreResult<T> {
        let mut connection = self.read()?;
        let transaction = connection.transaction()?;
        let value = operation(&transaction)?;
        transaction.commit()?;
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

    pub fn verify_schema(&self) -> StoreResult<()> {
        schema::verify(
            &self.root.join("canvas.sqlite3"),
            &self.root.join("analytics.sqlite3"),
        )
    }
}

fn open_database(path: &Path, flags: OpenFlags) -> StoreResult<Connection> {
    let connection = Connection::open_with_flags(path, flags)?;
    connection.busy_timeout(std::time::Duration::from_millis(u64::from(
        DATABASE_BUSY_TIMEOUT_MS,
    )))?;
    Ok(connection)
}
