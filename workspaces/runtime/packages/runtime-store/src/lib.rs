pub mod agent_columns;
pub mod agent_volatile;
pub mod blocking_pool;
mod bootstrap;
mod cached_sql;
pub use cached_sql::{CachedSql, STATEMENT_CACHE_CAPACITY};
mod feature_schema;
mod fresh_state;
mod multi_server_state_check;
mod path;
pub mod path_identity;
mod records;
mod repositories;
mod schema;
mod sqlite_actor;
pub mod state_owner;
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
pub use repositories::cost_aggregate;
pub use repositories::remote_transport::{
    AccessNonceOutcome, OutboundReservation, RemoteTransportRepository,
};
pub use repositories::system_reads;

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::trace::{TraceEvent, TraceEventCodes};
pub use rusqlite::{Connection, Error as SqliteError, ErrorCode, Transaction};
use rusqlite::{OpenFlags, TransactionBehavior};
use thiserror::Error;

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
    decode_agent_value,
};
pub use repositories::agent_indexed_reads::{AgentIndexedReads, RESERVED_TURN_INPUT};
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
pub use repositories::orchestration_settings::{
    CHILD_RESULT_COALESCE_DEFAULT, CHILD_RESULT_COALESCE_MAX, CHILD_RESULT_COALESCE_MIN,
    CHILD_RESULT_COALESCE_SETTING, IDLE_SESSION_UNLOAD_SETTING, OrchestrationSettingsRepository,
    Setting as OrchestrationSetting,
};
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
    AccountWakePage, AcknowledgedSteer, BACKEND_RESTARTED_TURN_ERROR, ClaimedEvents, ClaimedSteer,
    NATIVE_ACTION_ATTEMPT_KINDS, PROVIDER_DISCONNECTED_TURN_ERROR, START_RETURN_LIMIT,
    STEER_UNCONFIRMED_ERROR, SchedulerRepository, TurnAttemptIdentity,
    VM_PROVIDER_DISCONNECTED_TURN_ERROR,
};
pub use repositories::startup_recovery::{self, StartupRecoveryRepository};
pub use repositories::sync_documents::{
    ENTITY_TOMBSTONE_LIMIT, ENTITY_TOMBSTONE_PRUNE_BATCH, EntityPage, EntityRow, SyncDocument,
    SyncDocumentsRepository,
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
/// WAL readers never block the writer or each other, so the pool is sized for the
/// concurrent read routes of 512 working agents (UI polls, sync pulls, per-turn
/// reads) rather than for the three that fit a single user; an idle connection
/// costs only its page cache, and readers are opened on first use.
const CANVAS_READER_CONNECTION_CAPACITY: usize = 16;
/// Reader connections reserved for the dedicated `PRAGMA data_version` query.
const VERSION_READER_CONNECTION_RESERVATION: usize = 1;
/// Maximum ordinary checked-out Canvas read connections after the version-reader reservation.
pub const READ_CONNECTION_POOL_CAPACITY: usize =
    CANVAS_READER_CONNECTION_CAPACITY - VERSION_READER_CONNECTION_RESERVATION;

/// `PRAGMA synchronous` of the Canvas database: every commit is fsynced, because
/// Canvas holds the analytics projection log that analytics.sqlite3 is rebuilt from.
const CANVAS_SYNCHRONOUS: &str = "FULL";
/// `PRAGMA synchronous` of the analytics database (WAL) for ordinary writes. A commit is
/// durable against an application crash but is not fsynced. After an OS or power crash
/// the last analytics commits can be lost; the projection cursor is lost with them, so
/// the projector replays those Canvas log rows (WAL with NORMAL never corrupts).
const ANALYTICS_SYNCHRONOUS: &str = "NORMAL";
/// `PRAGMA synchronous` of one analytics projection commit: it fsyncs the WAL, so every
/// analytics commit up to it is durable before Canvas may prune the projected log rows.
const ANALYTICS_PROJECTION_SYNCHRONOUS: &str = "FULL";

/// Canvas tables with process-local observers for committed row changes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ObservedTable {
    AccessClients,
    UsageResumes,
    Agents,
    Events,
    EventMeta,
    Rooms,
    OperationReceipts,
    Work,
    Monitors,
    Rules,
    ServerOutbox,
    ServerLinks,
    FederationSettings,
    FederationIdentity,
    FederationPeers,
    FederationInvites,
    FederationRooms,
    FederationOutbox,
    NativeActionReceipts,
    AccessReceipts,
    Budget,
    BudgetUsage,
    AccountTransfers,
    OrchestrationSettings,
    RuntimeRequests,
    RuntimeToolRequests,
    AnalyticsProjection,
}

/// Maximum rowid occurrences recorded per observed table for one writer transaction.
/// Unit: occurrences, so a row written twice counts twice. Overflow drops that
/// table's row precision for the commit and reports [`TableChange::Full`].
const CHANGED_ROW_HINT_LIMIT: usize = 1024;

/// What one known commit changed in an observed table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TableChange<'a> {
    /// Physical rowids written by the commit, in write order. A row written more
    /// than once appears more than once; a deleted row is reported by its rowid.
    Rows(&'a [i64]),
    /// Row identity is unknown; readers must reread the whole table.
    Full,
}

/// Committed resource scope proven by the writer's exact SQLite update counts.
/// Unknown and mixed writes always retain the broad refresh.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceChange {
    Entities,
    All,
}

// Entity projection tables whose rows are delivered by entity-backed sync refs.
const ENTITY_RESOURCE_TABLES: [&str; 2] = ["sync_entities", "sync_entity_meta"];

type ResourceChangeObserver = Arc<dyn Fn(ResourceChange) + Send + Sync>;
type TableChangeObserver = Arc<dyn for<'a> Fn(TableChange<'a>) + Send + Sync>;
type TableChangeObserverRegistry = Mutex<HashMap<ObservedTable, Vec<TableChangeObserver>>>;

/// Rowids written by the current writer transaction, grouped by observed table bit.
#[derive(Default)]
struct ChangedRowHints {
    /// Tables whose rowids are recorded for the whole transaction.
    complete: u32,
    rows: [Vec<i64>; OBSERVED_TABLES.len()],
}

impl ChangedRowHints {
    fn record(&mut self, table: ObservedTable, row_id: i64) {
        let bit = table.change_bit();
        if self.complete & bit == 0 {
            return;
        }
        let rows = &mut self.rows[bit.trailing_zeros() as usize];
        if rows.len() < CHANGED_ROW_HINT_LIMIT {
            rows.push(row_id);
        } else {
            self.complete &= !bit;
            rows.clear();
        }
    }
}

impl ObservedTable {
    const fn name(self) -> &'static str {
        match self {
            Self::AccessClients => "runtime_access_clients",
            Self::UsageResumes => "runtime_usage_resumes",
            Self::Agents => "runtime_agents",
            Self::Events => "runtime_events",
            Self::EventMeta => "runtime_event_meta",
            Self::Rooms => "runtime_rooms",
            Self::OperationReceipts => "runtime_operation_receipts",
            Self::Work => "runtime_work",
            Self::Monitors => "runtime_monitors",
            Self::Rules => "runtime_rules",
            Self::ServerOutbox => "runtime_server_outbox",
            Self::ServerLinks => "runtime_server_links",
            Self::FederationSettings => "runtime_federation_settings",
            Self::FederationIdentity => "runtime_federation_identity",
            Self::FederationPeers => "runtime_federation_peers",
            Self::FederationInvites => "runtime_federation_invites",
            Self::FederationRooms => "runtime_federation_rooms",
            Self::FederationOutbox => "runtime_federation_outbox",
            Self::NativeActionReceipts => "runtime_native_action_receipts",
            Self::AccessReceipts => "runtime_access_receipts",
            Self::Budget => "runtime_budget",
            Self::BudgetUsage => "runtime_budget_usage",
            Self::AccountTransfers => "runtime_account_transfers",
            Self::OrchestrationSettings => "runtime_orchestration_settings",
            Self::RuntimeRequests => "runtime_requests",
            Self::RuntimeToolRequests => "runtime_tool_requests",
            Self::AnalyticsProjection => "runtime_analytics_projection",
        }
    }

    const fn change_bit(self) -> u32 {
        match self {
            Self::AccessClients => 1 << 0,
            Self::UsageResumes => 1 << 1,
            Self::Agents => 1 << 2,
            Self::Events => 1 << 3,
            Self::EventMeta => 1 << 19,
            Self::Rooms => 1 << 4,
            Self::OperationReceipts => 1 << 5,
            Self::Work => 1 << 6,
            Self::Monitors => 1 << 7,
            Self::Rules => 1 << 8,
            Self::ServerOutbox => 1 << 9,
            Self::ServerLinks => 1 << 10,
            Self::FederationSettings => 1 << 11,
            Self::FederationIdentity => 1 << 12,
            Self::FederationPeers => 1 << 13,
            Self::FederationInvites => 1 << 14,
            Self::FederationRooms => 1 << 15,
            Self::FederationOutbox => 1 << 16,
            Self::NativeActionReceipts => 1 << 17,
            Self::AccessReceipts => 1 << 18,
            Self::Budget => 1 << 20,
            Self::BudgetUsage => 1 << 21,
            Self::AccountTransfers => 1 << 22,
            Self::OrchestrationSettings => 1 << 23,
            Self::RuntimeRequests => 1 << 24,
            Self::RuntimeToolRequests => 1 << 25,
            Self::AnalyticsProjection => 1 << 26,
        }
    }

    const fn suppressible_resource_receipt(self) -> bool {
        matches!(
            self,
            Self::OperationReceipts | Self::NativeActionReceipts | Self::AccessReceipts
        )
    }
}

const OBSERVED_TABLES: [ObservedTable; 27] = [
    ObservedTable::AccessClients,
    ObservedTable::UsageResumes,
    ObservedTable::Agents,
    ObservedTable::Events,
    ObservedTable::EventMeta,
    ObservedTable::Rooms,
    ObservedTable::OperationReceipts,
    ObservedTable::Work,
    ObservedTable::Monitors,
    ObservedTable::Rules,
    ObservedTable::ServerOutbox,
    ObservedTable::ServerLinks,
    ObservedTable::FederationSettings,
    ObservedTable::FederationIdentity,
    ObservedTable::FederationPeers,
    ObservedTable::FederationInvites,
    ObservedTable::FederationRooms,
    ObservedTable::FederationOutbox,
    ObservedTable::NativeActionReceipts,
    ObservedTable::AccessReceipts,
    ObservedTable::Budget,
    ObservedTable::BudgetUsage,
    ObservedTable::AccountTransfers,
    ObservedTable::OrchestrationSettings,
    ObservedTable::RuntimeRequests,
    ObservedTable::RuntimeToolRequests,
    ObservedTable::AnalyticsProjection,
];

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
    /// One serialized writer job. `wait` covers admission plus the actor
    /// queue; `hold` covers only the job's SQLite transaction on the actor
    /// thread. `origin` is the caller that submitted the job.
    WriterTransaction {
        wait: Duration,
        hold: Duration,
        origin: &'static std::panic::Location<'static>,
    },
    SqlProfile {
        database: StoreDatabaseKind,
        elapsed: Duration,
    },
    CommitDuration {
        database: StoreDatabaseKind,
        elapsed: Duration,
    },
    /// Snapshot after one terminal SQLite busy or locked error crossed a typed store boundary.
    SqliteLockError { total: u64 },
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
static SQLITE_LOCK_ERROR_COUNT: AtomicU64 = AtomicU64::new(0);

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
    observers: &TableChangeObserverRegistry,
    changed_tables: u32,
    changed_rows: Option<&ChangedRowHints>,
) {
    for table in OBSERVED_TABLES {
        let bit = table.change_bit();
        if changed_tables & bit == 0 {
            continue;
        }
        let change = match changed_rows {
            Some(rows) if rows.complete & bit != 0 => {
                TableChange::Rows(rows.rows[bit.trailing_zeros() as usize].as_slice())
            }
            _ => TableChange::Full,
        };
        let callbacks = observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&table)
            .cloned()
            .unwrap_or_default();
        for callback in callbacks {
            if let Err(_panic_payload) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(change)))
            {
                // A table observer panic must not interrupt other invalidations.
            }
        }
    }
}

fn fire_resource_change_observers(
    observers: &Mutex<Vec<ResourceChangeObserver>>,
    change: ResourceChange,
) {
    let callbacks = observers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    for callback in callbacks {
        if let Err(_panic_payload) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(change)))
        {
            // Resource observers are hints and cannot report panics to their caller.
        }
    }
}

fn saturating_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn transaction_resource_change(
    total_changes: Option<u64>,
    receipt_updates: u64,
    entity_updates: u64,
) -> Option<ResourceChange> {
    let Some(total_changes) = total_changes else {
        return Some(ResourceChange::All);
    };
    if total_changes == receipt_updates && entity_updates == 0 {
        None
    } else if entity_updates > 0
        && receipt_updates.checked_add(entity_updates) == Some(total_changes)
    {
        Some(ResourceChange::Entities)
    } else {
        Some(ResourceChange::All)
    }
}

#[cfg(test)]
mod resource_change_classifier_tests {
    use super::{ResourceChange, transaction_resource_change};

    #[test]
    fn only_exact_receipt_and_entity_counts_narrow_the_refresh() {
        assert_eq!(transaction_resource_change(Some(0), 0, 0), None);
        assert_eq!(transaction_resource_change(Some(3), 3, 0), None);
        assert_eq!(
            transaction_resource_change(Some(3), 0, 3),
            Some(ResourceChange::Entities)
        );
        assert_eq!(
            transaction_resource_change(Some(3), 1, 2),
            Some(ResourceChange::Entities)
        );
        for (total, receipts, entities) in [
            (Some(3), 2, 0),
            (Some(3), 0, 2),
            (Some(3), 0, 0),
            (None, 0, 0),
            (Some(0), 0, 1),
            (Some(u64::MAX), u64::MAX, 1),
        ] {
            assert_eq!(
                transaction_resource_change(total, receipts, entities),
                Some(ResourceChange::All)
            );
        }
    }
}

fn install_table_change_hooks(
    connection: &Connection,
    table_change_flags: Arc<AtomicU32>,
    resource_receipt_update_count: Arc<AtomicU64>,
    resource_entity_update_count: Arc<AtomicU64>,
    row_interest: Arc<AtomicU32>,
    changed_rows: Arc<Mutex<ChangedRowHints>>,
) -> rusqlite::Result<()> {
    connection.update_hook(Some(
        move |_action: rusqlite::hooks::Action,
              database_name: &str,
              table_name: &str,
              row_id: i64| {
            if database_name != "main" {
                return;
            }
            if ENTITY_RESOURCE_TABLES.contains(&table_name) {
                resource_entity_update_count.fetch_add(1, Ordering::Relaxed);
            }
            let Some(observed) = OBSERVED_TABLES
                .iter()
                .find(|observed| observed.name() == table_name)
            else {
                return;
            };
            table_change_flags.fetch_or(observed.change_bit(), Ordering::Relaxed);
            if observed.suppressible_resource_receipt() {
                resource_receipt_update_count.fetch_add(1, Ordering::Relaxed);
            }
            // SQLite reports the row being written. A raw UPDATE that changes a
            // rowid is not tracked, so no repository writer relies on that path.
            if row_interest.load(Ordering::Relaxed) & observed.change_bit() != 0 {
                changed_rows
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .record(*observed, row_id);
            }
        },
    ))?;
    connection.authorizer(Some(|context: AuthContext<'_>| match context.action {
        AuthAction::Delete { table_name }
            if context.database_name == Some("main")
                && (OBSERVED_TABLES
                    .iter()
                    .any(|observed| observed.name() == table_name)
                    || ENTITY_RESOURCE_TABLES.contains(&table_name)) =>
        {
            // SQLite documents IGNORE here as retaining DELETE semantics while
            // disabling the truncate optimization, so update_hook sees rows.
            Authorization::Ignore
        }
        _ => Authorization::Allow,
    }))
}

fn verify_observed_tables_are_rowid(connection: &Connection) -> StoreResult<()> {
    for table in OBSERVED_TABLES
        .iter()
        .map(|table| table.name())
        .chain(ENTITY_RESOURCE_TABLES)
    {
        let without_rowid = connection.query_row(
            "SELECT wr FROM pragma_table_list WHERE schema = 'main' AND name = ?1",
            [table],
            |row| row.get::<_, i64>(0),
        )?;
        if without_rowid != 0 {
            return Err(StoreError::SchemaMismatch(format!(
                "Canvas table {table} must use rowid storage for change notifications"
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
    Sqlite(#[source] rusqlite::Error),
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
    #[error("asynchronous store read task did not complete")]
    ReadTaskFailed,
}

pub type StoreResult<T> = Result<T, StoreError>;

/// Number of terminal SQLite busy/locked errors observed at typed store boundaries.
/// Zero means none were observed at this boundary, not that all-process count is zero:
/// service-specific conversions and raw errors handled inside `read_async` closures
/// are not included.
pub fn sqlite_lock_error_count() -> u64 {
    SQLITE_LOCK_ERROR_COUNT.load(Ordering::Acquire)
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        observe_sqlite_lock_error(&error);
        Self::Sqlite(error)
    }
}

fn is_sqlite_lock_error(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

fn observe_sqlite_lock_error(error: &rusqlite::Error) {
    if is_sqlite_lock_error(error) {
        let mut total = SQLITE_LOCK_ERROR_COUNT.load(Ordering::Relaxed);
        while total < u64::MAX {
            match SQLITE_LOCK_ERROR_COUNT.compare_exchange_weak(
                total,
                total + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    total += 1;
                    break;
                }
                Err(observed) => total = observed,
            }
        }
        emit_diagnostic(StoreDiagnosticsEvent::SqliteLockError { total });
    }
}

impl StoreError {
    fn writer_actor_control(error: tokio_rusqlite::Error) -> Self {
        match &error {
            tokio_rusqlite::Error::Error(error) | tokio_rusqlite::Error::Close((_, error)) => {
                observe_sqlite_lock_error(error)
            }
            tokio_rusqlite::Error::ConnectionClosed => {}
            _ => {}
        }
        Self::WriterActorControl(error)
    }

    pub(crate) fn commit_outcome_unknown(error: rusqlite::Error) -> Self {
        observe_sqlite_lock_error(&error);
        Self::CommitOutcomeUnknown(error)
    }
}

/// Convert the actor's transport channel error to the store's typed unknown
/// outcome. `call_raw` only reports a closed channel today; retain a typed close
/// error if a future control path returns one. Application errors are carried
/// inside the response value and must not pass through this conversion.
pub(crate) fn actor_transport_outcome_unknown<E>(error: tokio_rusqlite::Error<E>) -> StoreError {
    if let tokio_rusqlite::Error::Close((_, error)) = &error {
        observe_sqlite_lock_error(error);
    }
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
    table_commit_observers: Arc<TableChangeObserverRegistry>,
    resource_change_observers: Arc<Mutex<Vec<ResourceChangeObserver>>>,
    table_change_flags: Arc<AtomicU32>,
    resource_receipt_update_count: Arc<AtomicU64>,
    resource_entity_update_count: Arc<AtomicU64>,
    /// Observed tables with a row-aware observer; set before the observer is added.
    table_row_interest: Arc<AtomicU32>,
    changed_rows: Arc<Mutex<ChangedRowHints>>,
    #[cfg(test)]
    simulate_unknown_commit_after_success: Arc<AtomicBool>,
    /// Count of committed transactions that changed rows; owned and advanced by the writer.
    committed_change_generation: Arc<AtomicU64>,
    /// Count of jobs admitted to this store's serialized writer lane since open.
    /// Unit: jobs. Process-local; read by tests that require no writer use.
    writer_admissions: AtomicU64,
    last_entity_projection_state: Mutex<Option<(u64, String)>>,
    entity_projection_maintenance_lock: tokio::sync::Mutex<()>,
    wal_checkpoint_requested: Arc<AtomicBool>,
    last_wal_checkpoint: Mutex<Instant>,
}

struct ReadConnectionPool {
    path: PathBuf,
    /// One permit per pooled reader. [`RuntimeStore::read_async`] holds one
    /// for its whole checkout, so readers queue in one FIFO as futures and a
    /// holder always finds a connection.
    admission: Arc<tokio::sync::Semaphore>,
    state: Mutex<ReadConnectionPoolState>,
    /// Reads waiting for a permit.
    waiters: AtomicUsize,
    waiter_peak: AtomicUsize,
}

struct ReadConnectionPoolState {
    idle: Vec<Connection>,
    opened: usize,
}

/// A checked-out read-only canvas connection returned to its bounded pool on drop.
struct ReadConnection {
    connection: Option<Connection>,
    pool: Arc<ReadConnectionPool>,
    /// Released after the connection is back in the idle list.
    _admission: tokio::sync::OwnedSemaphorePermit,
}

impl Drop for ReadConnection {
    fn drop(&mut self) {
        let Some(connection) = self.connection.take() else {
            return;
        };
        if let Ok(mut state) = self.pool.state.lock() {
            state.idle.push(connection);
        }
    }
}

/// Counts one read waiting for a permit for as long as it lives.
struct ReadWaiterRegistration<'a>(&'a ReadConnectionPool);

impl<'a> ReadWaiterRegistration<'a> {
    fn begin(pool: &'a ReadConnectionPool) -> Self {
        let active = pool.waiters.fetch_add(1, Ordering::Relaxed) + 1;
        let peak_waiters = pool
            .waiter_peak
            .fetch_max(active, Ordering::Relaxed)
            .max(active);
        emit_diagnostic(StoreDiagnosticsEvent::ReadWaiters {
            waiting_threads: active,
            peak_waiters,
            opened_connections: pool.opened(),
        });
        Self(pool)
    }
}

impl Drop for ReadWaiterRegistration<'_> {
    fn drop(&mut self) {
        let waiting_threads = self.0.waiters.fetch_sub(1, Ordering::Relaxed) - 1;
        emit_diagnostic(StoreDiagnosticsEvent::ReadWaiters {
            waiting_threads,
            peak_waiters: self.0.waiter_peak.load(Ordering::Relaxed),
            opened_connections: self.0.opened(),
        });
    }
}

impl ReadConnectionPool {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            admission: Arc::new(tokio::sync::Semaphore::new(READ_CONNECTION_POOL_CAPACITY)),
            state: Mutex::new(ReadConnectionPoolState {
                idle: Vec::new(),
                opened: 0,
            }),
            waiters: AtomicUsize::new(0),
            waiter_peak: AtomicUsize::new(0),
        }
    }

    /// Wait as a future for one of the [`READ_CONNECTION_POOL_CAPACITY`] permits.
    async fn admit(&self) -> StoreResult<(tokio::sync::OwnedSemaphorePermit, bool)> {
        match Arc::clone(&self.admission).try_acquire_owned() {
            Ok(permit) => Ok((permit, false)),
            Err(_) => {
                let _waiting = ReadWaiterRegistration::begin(self);
                let permit = Arc::clone(&self.admission)
                    .acquire_owned()
                    .await
                    .map_err(|_| StoreError::ReadTaskFailed)?;
                Ok((permit, true))
            }
        }
    }

    /// Check out a reader for a held permit; with the permit one is idle or may be opened.
    fn checkout_admitted(
        self: &Arc<Self>,
        permit: tokio::sync::OwnedSemaphorePermit,
        started: Instant,
        waited: bool,
    ) -> StoreResult<ReadConnection> {
        let idle = self
            .state
            .lock()
            .map_err(|_| StoreError::WriterPoisoned)?
            .idle
            .pop();
        let connection = match idle {
            Some(connection) => connection,
            None => {
                self.state
                    .lock()
                    .map_err(|_| StoreError::WriterPoisoned)?
                    .opened += 1;
                let opened = open_database(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY).and_then(
                    |connection| {
                        connection.pragma_update(None, "query_only", "ON")?;
                        Ok(connection)
                    },
                );
                if opened.is_err()
                    && let Ok(mut state) = self.state.lock()
                {
                    state.opened = state.opened.saturating_sub(1);
                }
                opened?
            }
        };
        emit_diagnostic(StoreDiagnosticsEvent::ReadAdmission {
            wait: started.elapsed(),
            waited,
            waiting_threads: self.waiters.load(Ordering::Relaxed),
            peak_waiters: self.waiter_peak.load(Ordering::Relaxed),
            opened_connections: self.opened(),
        });
        Ok(ReadConnection {
            connection: Some(connection),
            pool: Arc::clone(self),
            _admission: permit,
        })
    }

    fn opened(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.opened)
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn opened_count(&self) -> usize {
        self.state.lock().expect("read pool lock").opened
    }
}

/// Assert that a runtime filesystem or process owner is outside a SQL callback.
/// The thread-local state and check are compiled out of release builds.
#[inline]
pub fn debug_assert_outside_sql() {
    #[cfg(debug_assertions)]
    SQL_CALLBACK_ACTIVE.with(|active| {
        debug_assert!(
            !active.get(),
            "filesystem/process work inside a SQL closure"
        );
    });
}

#[cfg(debug_assertions)]
thread_local! {
    static SQL_CALLBACK_ACTIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct SqlCallbackGuard {
    #[cfg(debug_assertions)]
    previous: bool,
}

impl SqlCallbackGuard {
    fn enter() -> Self {
        Self {
            #[cfg(debug_assertions)]
            previous: SQL_CALLBACK_ACTIVE.with(|active| active.replace(true)),
        }
    }
}

impl Drop for SqlCallbackGuard {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        SQL_CALLBACK_ACTIVE.with(|active| active.set(self.previous));
    }
}

fn sql_callback<T>(operation: impl FnOnce() -> T) -> T {
    let _guard = SqlCallbackGuard::enter();
    operation()
}

#[cfg(all(test, debug_assertions))]
mod sql_guard_tests {
    #[test]
    fn guard_restores_nested_and_panicking_callbacks() {
        super::debug_assert_outside_sql();
        let panic = std::panic::catch_unwind(|| {
            super::sql_callback(|| {
                super::sql_callback(|| ());
                super::debug_assert_outside_sql();
            });
        });
        assert!(panic.is_err(), "nested callbacks retain their outer guard");
        super::debug_assert_outside_sql();
    }
}

fn snapshot_on<T, E>(
    mut connection: ReadConnection,
    operation: impl FnOnce(&Transaction<'_>) -> Result<T, E>,
) -> Result<T, E>
where
    E: From<StoreError>,
{
    let connection = connection
        .connection
        .as_mut()
        .expect("checked-out read connection");
    let transaction = connection.transaction().map_err(StoreError::from)?;
    let value = sql_callback(|| operation(&transaction))?;
    transaction
        .commit()
        .map_err(StoreError::from)
        .map_err(E::from)?;
    Ok(value)
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
        changed_tables: u32,
        changed_rows: Box<ChangedRowHints>,
        resource_change: Option<ResourceChange>,
    },
    CommitOutcomeUnknown {
        error: StoreError,
        possible_table_changes: u32,
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
        Self::open_inner(state_dir, None).await
    }

    /// Open the state as its serving owner: after `runtime.lock` is held and before the first
    /// write, record the Rust owner marker that fences Python (`state_owner.rs`).
    pub async fn open_claiming(
        state_dir: Option<&Path>,
        owner: &state_owner::StateOwnerClaim,
    ) -> StoreResult<Self> {
        Self::open_inner(state_dir, Some(owner)).await
    }

    async fn open_inner(
        state_dir: Option<&Path>,
        owner: Option<&state_owner::StateOwnerClaim>,
    ) -> StoreResult<Self> {
        let root = match state_dir {
            Some(path) => path::resolve_explicit_state_dir(path)?,
            None => resolve_state_dir()?,
        }
        .into_path_buf();
        fresh_state::validate(&root)?;
        let canvas_path = root.join("canvas.sqlite3");
        let analytics_path = root.join("analytics.sqlite3");
        let runtime_lock = Arc::new(RuntimeLock::acquire(&root)?);
        if let Some(owner) = owner {
            state_owner::claim(&root, owner)?;
        }
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
        repositories::cost_aggregate::ensure_cost_aggregate(&analytics_path)?;

        let table_change_flags = Arc::new(AtomicU32::new(0));
        let resource_receipt_update_count = Arc::new(AtomicU64::new(0));
        let resource_entity_update_count = Arc::new(AtomicU64::new(0));
        let writer = SqliteActor::open(&canvas_path, Arc::clone(&runtime_lock)).await?;
        let hook_table_change_flags = Arc::clone(&table_change_flags);
        let hook_resource_receipt_update_count = Arc::clone(&resource_receipt_update_count);
        let hook_resource_entity_update_count = Arc::clone(&resource_entity_update_count);
        let table_row_interest = Arc::new(AtomicU32::new(0));
        let changed_rows = Arc::new(Mutex::new(ChangedRowHints::default()));
        let hook_table_row_interest = Arc::clone(&table_row_interest);
        let hook_changed_rows = Arc::clone(&changed_rows);
        writer
            .call(move |connection| {
                connection
                    .busy_timeout(Duration::from_millis(u64::from(DATABASE_BUSY_TIMEOUT_MS)))
                    .map_err(StoreError::from)?;
                install_profile(connection, StoreDatabaseKind::Canvas);
                connection.set_prepared_statement_cache_capacity(STATEMENT_CACHE_CAPACITY);
                connection
                    .pragma_update(None, "synchronous", CANVAS_SYNCHRONOUS)
                    .map_err(StoreError::from)?;
                schema::register_python_trigger_functions(connection)?;
                install_table_change_hooks(
                    connection,
                    hook_table_change_flags,
                    hook_resource_receipt_update_count,
                    hook_resource_entity_update_count,
                    hook_table_row_interest,
                    hook_changed_rows,
                )
                .map_err(StoreError::from)?;
                Ok::<(), StoreError>(())
            })
            .await
            .map_err(|error| match error {
                tokio_rusqlite::Error::Error(error) => error,
                tokio_rusqlite::Error::ConnectionClosed => {
                    StoreError::WriterActorInitialization(tokio_rusqlite::Error::ConnectionClosed)
                }
                tokio_rusqlite::Error::Close((_, error)) => StoreError::from(error),
                _ => StoreError::WriterActorInitialization(tokio_rusqlite::Error::ConnectionClosed),
            })?;
        let analytics = open_database(&analytics_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        analytics.pragma_update(None, "journal_mode", "WAL")?;
        analytics.pragma_update(None, "synchronous", ANALYTICS_SYNCHRONOUS)?;
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
            resource_change_observers: Arc::new(Mutex::new(Vec::new())),
            table_change_flags,
            resource_receipt_update_count,
            resource_entity_update_count,
            table_row_interest,
            changed_rows,
            #[cfg(test)]
            simulate_unknown_commit_after_success: Arc::new(AtomicBool::new(false)),
            committed_change_generation: Arc::new(AtomicU64::new(0)),
            writer_admissions: AtomicU64::new(0),
            last_entity_projection_state: Mutex::new(None),
            entity_projection_maintenance_lock: tokio::sync::Mutex::new(()),
            wal_checkpoint_requested: Arc::new(AtomicBool::new(false)),
            last_wal_checkpoint: Mutex::new(Instant::now()),
        };
        store
            .transaction(move |transaction| {
                agent_columns::prepare(transaction)?;
                bootstrap::prepare_sync_schema(transaction)?;
                bootstrap::prepare_runtime_event_indexes(transaction)?;
                AgentRepository::prepare_change_journal(transaction)?;
                agent_volatile::prepare(transaction)?;
                feature_schema::prepare(transaction)?;
                StartupRecoveryRepository::prepare_indexes(transaction)?;
                MonitorsRepository::prepare_indexes(transaction)?;
                AgentIndexedReads::prepare_index(transaction)?;
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
    ) {
        self.add_table_observer(table, move |_change| observer());
    }

    /// Like [`Self::add_table_change_observer`], but a known commit passes the
    /// rowids it wrote. Overflow or an unknown outcome passes [`TableChange::Full`].
    /// Rowids are recorded from the next writer transaction onward, so register
    /// before taking the snapshot that the observer will maintain.
    pub fn add_table_row_change_observer(
        &self,
        table: ObservedTable,
        observer: impl for<'a> Fn(TableChange<'a>) + Send + Sync + 'static,
    ) {
        self.table_row_interest
            .fetch_or(table.change_bit(), Ordering::Release);
        self.add_table_observer(table, observer);
    }

    fn add_table_observer(
        &self,
        table: ObservedTable,
        observer: impl for<'a> Fn(TableChange<'a>) + Send + Sync + 'static,
    ) {
        self.table_commit_observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(table)
            .or_default()
            .push(Arc::new(observer));
    }

    /// Register a notification for durable changes visible to HTTP resource
    /// subscribers. Writes exclusively to the three named idempotency receipt
    /// tables are suppressed only when their update-hook count exactly matches
    /// SQLite's transaction change count. Exact entity-projection-only writes
    /// carry `Entities`; unknown, mixed, and uncertain writes carry `All`.
    /// A known commit and an ambiguous commit that may have landed both notify,
    /// so consumers must reread authoritative state. On known commits, callbacks
    /// run after the transaction's post-commit callback. Register before taking
    /// the subscriber's initial snapshot. Callbacks run outside the transaction
    /// and registry lock. After caller cancellation they may run on the actor
    /// thread, so keep them short and free of database or provider I/O.
    pub fn add_resource_change_observer(
        &self,
        observer: impl Fn(ResourceChange) + Send + Sync + 'static,
    ) {
        self.resource_change_observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Arc::new(observer));
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
        {
            let mut current = diagnostics_callback()
                .lock()
                .map_err(|_| StoreError::WriterPoisoned)?;
            DIAGNOSTICS_ENABLED.store(callback.is_some(), Ordering::Release);
            *current = callback;
        }
        self.writer
            .call(|writer| {
                install_profile(writer, StoreDatabaseKind::Canvas);
                Ok::<(), rusqlite::Error>(())
            })
            .await
            .map_err(StoreError::writer_actor_control)?;
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
            .map_err(StoreError::writer_actor_control)?;

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
    #[track_caller]
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
    #[track_caller]
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
        let origin = std::panic::Location::caller();
        async move {
            self.transaction_with_post_commit_boxed(operation, |_| {}, origin)
                .await
                .map(|(value, _)| value)
        }
    }

    /// Run a write transaction and return its committed change generation.
    /// The generation unit is one committed transaction with row changes.
    #[track_caller]
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
        self.transaction_with_post_commit_boxed(operation, |_| {}, std::panic::Location::caller())
    }

    /// Run a write transaction and invoke `post_commit` exactly once after a
    /// successful SQLite commit, even if the submitted response is abandoned.
    /// The callback runs synchronously after the writer callback releases its
    /// transaction and admission permit; keep it bounded and free of database
    /// or provider I/O. Errors, rollback, and cancellation before submission
    /// do not invoke it.
    #[track_caller]
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
        self.transaction_with_post_commit_boxed(
            operation,
            post_commit,
            std::panic::Location::caller(),
        )
    }

    async fn transaction_with_post_commit_boxed<T, E>(
        &self,
        operation: BoxedTransactionOperation<T, E>,
        post_commit: impl FnOnce(&T) + Send + 'static,
        origin: &'static std::panic::Location<'static>,
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
        drop(waiter_registration);
        self.writer_admissions.fetch_add(1, Ordering::Relaxed);
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
        // Measured on the actor thread so queued jobs are not reported as
        // transaction hold time; unit: nanoseconds.
        let submitted = diagnostics_enabled.then(Instant::now);
        let actor_timing = Arc::new([AtomicU64::new(0), AtomicU64::new(0)]);
        let job_timing = Arc::clone(&actor_timing);
        let generation = Arc::clone(&self.committed_change_generation);
        let identity_registry_lease = Arc::clone(&self.identity_registry_lease);
        let table_change_flags = Arc::clone(&self.table_change_flags);
        let resource_receipt_update_count = Arc::clone(&self.resource_receipt_update_count);
        let resource_entity_update_count = Arc::clone(&self.resource_entity_update_count);
        let table_row_interest = Arc::clone(&self.table_row_interest);
        let changed_rows = Arc::clone(&self.changed_rows);
        #[cfg(test)]
        let simulate_unknown_commit_after_success =
            Arc::clone(&self.simulate_unknown_commit_after_success);
        let transaction_job =
            move |writer: &mut Connection| -> Result<WriterTransactionOutcome<T, E>, StoreError> {
                let _identity_registry_lease = identity_registry_lease;
                table_change_flags.store(0, Ordering::Release);
                resource_receipt_update_count.store(0, Ordering::Release);
                resource_entity_update_count.store(0, Ordering::Release);
                // Row precision is kept only for tables observed when this transaction starts.
                *changed_rows
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = ChangedRowHints {
                    complete: table_row_interest.load(Ordering::Acquire),
                    ..ChangedRowHints::default()
                };
                let transaction = writer
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(StoreError::from)?;
                let initial_changes = transaction.total_changes();
                match sql_callback(|| operation(&transaction)) {
                    Ok(value) => {
                        let final_changes = transaction.total_changes();
                        let changed = final_changes != initial_changes;
                        let total_changes = final_changes.checked_sub(initial_changes);
                        let receipt_updates = resource_receipt_update_count.load(Ordering::Acquire);
                        let resource_change = transaction_resource_change(
                            total_changes,
                            receipt_updates,
                            resource_entity_update_count.load(Ordering::Acquire),
                        );
                        let transaction_rows = std::mem::take(
                            &mut *changed_rows
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner),
                        );
                        let commit_started = diagnostics_enabled.then(Instant::now);
                        if let Err(error) = transaction.commit() {
                            return Ok(WriterTransactionOutcome::CommitOutcomeUnknown {
                                error: StoreError::commit_outcome_unknown(error),
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
                            changed_rows: Box::new(transaction_rows),
                            resource_change,
                        })
                    }
                    Err(error) => {
                        // Dropping rolls back best-effort; preserve the operation error.
                        drop(transaction);
                        table_change_flags.store(0, Ordering::Release);
                        resource_receipt_update_count.store(0, Ordering::Release);
                        resource_entity_update_count.store(0, Ordering::Release);
                        Ok(WriterTransactionOutcome::Known {
                            result: Err(error),
                            generation: generation.load(Ordering::Acquire),
                            changed: false,
                            changed_tables: 0,
                            changed_rows: Box::default(),
                            resource_change: None,
                        })
                    }
                }
            };
        let operation = move |writer: &mut Connection| {
            let started = submitted.map(|submitted| (submitted.elapsed(), Instant::now()));
            let outcome = transaction_job(writer);
            if let Some((queued, started)) = started {
                job_timing[0].store(saturating_nanos(queued), Ordering::Relaxed);
                job_timing[1].store(saturating_nanos(started.elapsed()), Ordering::Relaxed);
            }
            outcome
        };
        let abandoned_notifiers = Arc::clone(&self.commit_notifiers);
        let abandoned_checkpoint = Arc::clone(&self.wal_checkpoint_requested);
        let table_commit_observers = Arc::clone(&self.table_commit_observers);
        let resource_change_observers = Arc::clone(&self.resource_change_observers);
        let actor_outcome = self
            .writer
            .call_admitted_with_completion(permit, operation, move |result, disposition| {
                if let Ok(outcome) = result {
                    let (changed_tables, changed_rows, resource_change, value) = match outcome {
                        WriterTransactionOutcome::Known {
                            result: Ok(value),
                            changed,
                            changed_tables,
                            changed_rows,
                            resource_change,
                            ..
                        } => {
                            if *changed && disposition == ActorResponseDisposition::Abandoned {
                                abandoned_checkpoint.store(true, Ordering::Release);
                                // Fire the synchronous wake directly; no Runtime handle or
                                // detached task is required for this committed transaction.
                                if let Err(_panic_payload) =
                                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                        fire_commit_observers(&abandoned_notifiers);
                                    }))
                                {
                                    // A dropped response cannot report observer panics.
                                }
                            }
                            (
                                *changed_tables,
                                Some(changed_rows),
                                *resource_change,
                                Some(value),
                            )
                        }
                        WriterTransactionOutcome::CommitOutcomeUnknown {
                            possible_table_changes,
                            ..
                        } => (
                            *possible_table_changes,
                            None,
                            Some(ResourceChange::All),
                            None,
                        ),
                        WriterTransactionOutcome::Known { result: Err(_), .. } => {
                            (0, None, None, None)
                        }
                    };
                    let table_observer_result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            fire_table_change_observers(
                                &table_commit_observers,
                                changed_tables,
                                changed_rows.map(|rows| &**rows),
                            );
                        }));
                    let post_commit_result = value.map(|value| {
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            post_commit(value)
                        }))
                    });
                    let resource_observer_result = resource_change.map(|change| {
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            fire_resource_change_observers(&resource_change_observers, change);
                        }))
                    });
                    if let Err(panic) = table_observer_result {
                        std::panic::resume_unwind(panic);
                    }
                    if let Some(Err(panic)) = post_commit_result {
                        std::panic::resume_unwind(panic);
                    }
                    if let Some(Err(panic)) = resource_observer_result {
                        std::panic::resume_unwind(panic);
                    }
                }
            })
            .await;
        if diagnostics_enabled {
            let queued = Duration::from_nanos(actor_timing[0].load(Ordering::Relaxed));
            let hold = Duration::from_nanos(actor_timing[1].load(Ordering::Relaxed));
            emit_diagnostic(StoreDiagnosticsEvent::WriterTransaction {
                wait: wait.saturating_add(queued),
                hold,
                origin,
            });
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

    #[track_caller]
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

    /// Open an independent read-only, query-only canvas connection outside
    /// the reader pool, for test assertions only.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_reader(&self) -> StoreResult<Connection> {
        let connection = open_database(&self.read_pool.path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.pragma_update(None, "query_only", "ON")?;
        Ok(connection)
    }

    /// Run related test assertions in one snapshot of a [`Self::test_reader`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_read_transaction<T>(
        &self,
        operation: impl FnOnce(&Transaction<'_>) -> StoreResult<T>,
    ) -> StoreResult<T> {
        let mut connection = self.test_reader()?;
        let transaction = connection.transaction()?;
        let value = sql_callback(|| operation(&transaction))?;
        transaction.commit()?;
        Ok(value)
    }

    /// Run `operation` in one read-only SQLite snapshot on the blocking pool.
    ///
    /// This is the only way to read the canvas through the reader pool.
    /// Callers wait for a reader as futures: a blocking-pool thread is taken
    /// only after one of the [`READ_CONNECTION_POOL_CAPACITY`] permits is held,
    /// so a read burst never parks more threads than there are readers and a
    /// permit holder never waits for a thread held by a permit waiter. Keep
    /// `operation` to SQL; do file, process and network work after it returns.
    /// Runtime filesystem and process owners call [`debug_assert_outside_sql`]
    /// to enforce this boundary in debug builds.
    /// `label` names the call site in the blocking-pool diagnostics.
    pub async fn read_async<T, E>(
        &self,
        label: &'static str,
        operation: impl FnOnce(&Transaction<'_>) -> Result<T, E> + Send + 'static,
    ) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<StoreError> + Send + 'static,
    {
        let started = Instant::now();
        let pool = Arc::clone(&self.read_pool);
        let (permit, waited) = pool.admit().await?;
        blocking_pool::spawn_blocking(label, move || {
            snapshot_on(pool.checkout_admitted(permit, started, waited)?, operation)
        })
        .await
        .map_err(|_| StoreError::ReadTaskFailed)?
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

    /// Return the number of jobs this store admitted to its serialized writer
    /// lane, including jobs that changed no rows. Unlike the process-global
    /// diagnostics callback, it counts only this store.
    pub fn writer_admissions(&self) -> u64 {
        self.writer_admissions.load(Ordering::Relaxed)
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
    connection.set_prepared_statement_cache_capacity(STATEMENT_CACHE_CAPACITY);
    connection.busy_timeout(std::time::Duration::from_millis(u64::from(
        DATABASE_BUSY_TIMEOUT_MS,
    )))?;
    Ok(connection)
}

#[cfg(test)]
mod read_pool_tests {
    use super::*;
    use std::sync::mpsc;

    const READ_POOL_TEST_WATCHDOG: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn sequential_reads_reuse_a_bounded_connection() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open runtime store");

        for _ in 0..16 {
            let value = store
                .read_async("runtime-store/tests/sequential_reads", |tx| {
                    tx.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                        .map_err(StoreError::from)
                })
                .await
                .expect("read");
            assert_eq!(value, 1);
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
    async fn canvas_commits_are_fsynced_and_analytics_commits_are_not() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open runtime store");
        // SQLite reports FULL as 2 and NORMAL as 1.
        let canvas: i64 = store
            .transaction_with(|tx| -> StoreResult<i64> {
                Ok(tx.query_row("PRAGMA synchronous", [], |row| row.get(0))?)
            })
            .await
            .expect("canvas pragma");
        assert_eq!(canvas, 2, "{CANVAS_SYNCHRONOUS}");
        let analytics: i64 = store
            .analytics
            .lock()
            .expect("analytics connection")
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .expect("analytics pragma");
        assert_eq!(analytics, 1, "{ANALYTICS_SYNCHRONOUS}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_reads_take_blocking_threads_only_up_to_reader_capacity() {
        const WAITING_READS_PER_READER: usize = 4;
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open runtime store"),
        );
        let reads = READ_CONNECTION_POOL_CAPACITY * WAITING_READS_PER_READER;
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let tasks = (0..reads)
            .map(|_| {
                let store = Arc::clone(&store);
                let (running, peak) = (Arc::clone(&running), Arc::clone(&peak));
                let entered_tx = entered_tx.clone();
                let release_rx = Arc::clone(&release_rx);
                tokio::spawn(async move {
                    store
                        .read_async("runtime-store/tests/read_async", move |tx| {
                            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                            peak.fetch_max(now, Ordering::SeqCst);
                            entered_tx.send(()).expect("report entered read");
                            release_rx
                                .lock()
                                .expect("release gate")
                                .recv()
                                .expect("release read");
                            running.fetch_sub(1, Ordering::SeqCst);
                            tx.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                                .map_err(StoreError::from)
                        })
                        .await
                })
            })
            .collect::<Vec<_>>();
        for _ in 0..READ_CONNECTION_POOL_CAPACITY {
            entered_rx.recv().await.expect("a reader enters");
        }
        for _ in 0..reads {
            tokio::task::yield_now().await;
        }
        // Every permit is held by a gated closure; the other reads wait as
        // futures and have not been submitted to the blocking pool.
        assert_eq!(store.read_pool.admission.available_permits(), 0);
        assert_eq!(
            running.load(Ordering::SeqCst),
            READ_CONNECTION_POOL_CAPACITY
        );
        assert!(entered_rx.try_recv().is_err());
        for _ in 0..reads {
            release_tx.send(()).expect("gate is open");
        }
        for task in tasks {
            assert_eq!(task.await.expect("read task joins").expect("read"), 1);
        }
        assert_eq!(peak.load(Ordering::SeqCst), READ_CONNECTION_POOL_CAPACITY);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_read_cancellation_before_admission_submits_no_work() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open runtime store"),
        );
        let held = Arc::clone(&store.read_pool.admission)
            .acquire_many_owned(READ_CONNECTION_POOL_CAPACITY as u32)
            .await
            .expect("hold all read admission permits");
        let ran = Arc::new(AtomicBool::new(false));
        let task_ran = Arc::clone(&ran);
        let mut read = Box::pin(store.read_async(
            "runtime-store/tests/read_async_cancel_before_admission",
            move |_| {
                task_ran.store(true, Ordering::Release);
                Ok::<_, StoreError>(())
            },
        ));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(
            matches!(
                std::future::Future::poll(read.as_mut(), &mut context),
                std::task::Poll::Pending
            ),
            "read waits for admission before submitting blocking work"
        );
        drop(read);
        assert!(!ran.load(Ordering::Acquire));
        assert_eq!(store.read_pool.admission.available_permits(), 0);

        drop(held);
        assert_eq!(
            store.read_pool.admission.available_permits(),
            READ_CONNECTION_POOL_CAPACITY
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_read_cancellation_after_submission_retains_admission_until_done() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open runtime store"),
        );
        let mut held = Vec::with_capacity(READ_CONNECTION_POOL_CAPACITY - 1);
        for _ in 1..READ_CONNECTION_POOL_CAPACITY {
            held.push(
                Arc::clone(&store.read_pool.admission)
                    .acquire_owned()
                    .await
                    .expect("hold a read admission permit"),
            );
        }

        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let task_store = Arc::clone(&store);
        let task = tokio::spawn(async move {
            task_store
                .read_async(
                    "runtime-store/tests/read_async_cancel_after_submission",
                    move |tx| {
                        entered_tx.send(()).expect("report submitted read started");
                        release_rx.recv().expect("release submitted read");
                        tx.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                            .map_err(StoreError::from)
                    },
                )
                .await
        });
        let read_started = tokio::time::timeout(READ_POOL_TEST_WATCHDOG, entered_rx.recv()).await;
        if !matches!(read_started, Ok(Some(()))) {
            release_tx
                .send(())
                .expect("release a possibly submitted read");
            task.abort();
            let canceled = tokio::time::timeout(READ_POOL_TEST_WATCHDOG, task).await;
            assert!(matches!(canceled, Ok(Err(error)) if error.is_cancelled()));
            panic!("blocking read did not start within the test watchdog");
        }
        assert_eq!(store.read_pool.admission.available_permits(), 0);

        task.abort();
        assert!(
            matches!(
                tokio::time::timeout(READ_POOL_TEST_WATCHDOG, task).await,
                Ok(Err(error)) if error.is_cancelled()
            ),
            "read caller cancellation completes within the watchdog"
        );
        let (waiting_entered_tx, mut waiting_entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let (waiting_started_tx, waiting_started_rx) = tokio::sync::oneshot::channel();
        let waiting_store = Arc::clone(&store);
        let mut waiting = tokio::spawn(async move {
            let mut read = Box::pin(waiting_store.read_async(
                "runtime-store/tests/read_async_after_cancel_waiter",
                move |tx| {
                    waiting_entered_tx
                        .send(())
                        .expect("report waiting read started");
                    tx.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                        .map_err(StoreError::from)
                },
            ));
            let mut waiting_started_tx = Some(waiting_started_tx);
            std::future::poll_fn(|context| {
                let result = std::future::Future::poll(read.as_mut(), context);
                if result.is_pending()
                    && let Some(sender) = waiting_started_tx.take()
                {
                    sender
                        .send(())
                        .expect("report waiting read reached admission");
                }
                result
            })
            .await
        });
        let waiting_started =
            tokio::time::timeout(READ_POOL_TEST_WATCHDOG, waiting_started_rx).await;
        if !matches!(waiting_started, Ok(Ok(()))) {
            release_tx
                .send(())
                .expect("release the submitted read before failing");
            waiting.abort();
            let canceled = tokio::time::timeout(READ_POOL_TEST_WATCHDOG, waiting).await;
            assert!(matches!(canceled, Ok(Err(error)) if error.is_cancelled()));
            panic!("waiting read did not reach admission within the test watchdog");
        }
        assert!(waiting_entered_rx.try_recv().is_err());
        assert_eq!(store.read_pool.admission.available_permits(), 0);

        release_tx.send(()).expect("submitted read is gated");
        let waiting_admitted =
            tokio::time::timeout(READ_POOL_TEST_WATCHDOG, waiting_entered_rx.recv()).await;
        if !matches!(waiting_admitted, Ok(Some(()))) {
            waiting.abort();
            let canceled = tokio::time::timeout(READ_POOL_TEST_WATCHDOG, waiting).await;
            assert!(matches!(canceled, Ok(Err(error)) if error.is_cancelled()));
            panic!("released read admission did not make progress within the watchdog");
        }
        let waiting_result = tokio::time::timeout(READ_POOL_TEST_WATCHDOG, &mut waiting).await;
        if waiting_result.is_err() {
            waiting.abort();
            let canceled = tokio::time::timeout(READ_POOL_TEST_WATCHDOG, waiting).await;
            assert!(matches!(canceled, Ok(Err(error)) if error.is_cancelled()));
            panic!("waiting read did not complete within the test watchdog");
        }
        assert_eq!(
            waiting_result
                .expect("checked above")
                .expect("waiting task joins")
                .expect("read"),
            1
        );
        drop(held);
        assert_eq!(
            store.read_pool.admission.available_permits(),
            READ_CONNECTION_POOL_CAPACITY
        );
    }

    /// Wait until `count` reads queue for a permit, failing at the watchdog.
    async fn wait_for_read_waiters(store: &RuntimeStore, count: usize) {
        tokio::time::timeout(READ_POOL_TEST_WATCHDOG, async {
            while store.read_pool.waiters.load(Ordering::SeqCst) < count {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reads queue for a permit within the watchdog");
    }

    /// Regression for the read admission wait cycle. With a synchronous read
    /// path, blocking-pool work took a thread and then waited for a permit,
    /// while `read_async` held a permit and waited for a thread: once every
    /// blocking thread waited for a permit queued behind async reads, nothing
    /// progressed. Blocking work now takes its read through `read_async`
    /// before it takes a thread, so the same arrangement completes.
    #[test]
    fn blocking_work_and_async_reads_do_not_form_a_wait_cycle() {
        const BLOCKING_THREADS: usize = 2;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(BLOCKING_THREADS)
            .enable_all()
            .build()
            .expect("runtime with a bounded blocking pool builds");
        runtime.block_on(async {
            let directory = tempfile::tempdir().expect("temporary state directory");
            let store = Arc::new(
                RuntimeStore::open(Some(directory.path()))
                    .await
                    .expect("open runtime store"),
            );
            let held = Arc::clone(&store.read_pool.admission)
                .acquire_many_owned(READ_CONNECTION_POOL_CAPACITY as u32)
                .await
                .expect("hold every read permit");
            let select_one = |tx: &Transaction<'_>| {
                tx.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                    .map_err(StoreError::from)
            };
            // Async reads queue first, so they take the permits on release.
            let mut tasks = (0..READ_CONNECTION_POOL_CAPACITY)
                .map(|_| {
                    let store = Arc::clone(&store);
                    tokio::spawn(async move {
                        store
                            .read_async("runtime-store/tests/wait_cycle_async", select_one)
                            .await
                    })
                })
                .collect::<Vec<_>>();
            wait_for_read_waiters(&store, READ_CONNECTION_POOL_CAPACITY).await;
            // One read-then-compute job per blocking thread queues behind them.
            tasks.extend((0..BLOCKING_THREADS).map(|_| {
                let store = Arc::clone(&store);
                tokio::spawn(async move {
                    let value = store
                        .read_async("runtime-store/tests/wait_cycle_job", select_one)
                        .await?;
                    crate::blocking_pool::spawn_blocking(
                        "runtime-store/tests/wait_cycle_job_work",
                        move || value,
                    )
                    .await
                    .map_err(|_| StoreError::ReadTaskFailed)
                })
            }));
            wait_for_read_waiters(&store, READ_CONNECTION_POOL_CAPACITY + BLOCKING_THREADS).await;
            drop(held);
            let completed = tokio::time::timeout(READ_POOL_TEST_WATCHDOG, async {
                for task in &mut tasks {
                    assert_eq!(task.await.expect("read task joins").expect("read"), 1);
                }
            })
            .await;
            if completed.is_err() {
                for task in &tasks {
                    task.abort();
                }
            }
            assert!(completed.is_ok(), "reads completed within the watchdog");
        });
    }

    #[test]
    fn read_admission_preserves_progress_with_server_blocking_pool_limit() {
        // One extra future exposes a missing admission bound while keeping this
        // a small, gate-controlled regression test rather than a load run.
        const READS_TO_EXCEED_BLOCKING_POOL: usize = crate::blocking_pool::MAX_THREADS + 1;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(crate::blocking_pool::MAX_THREADS)
            .enable_all()
            .build()
            .expect("runtime with the server blocking-pool limit builds");
        runtime.block_on(async {
            let directory = tempfile::tempdir().expect("temporary state directory");
            let store = Arc::new(
                RuntimeStore::open(Some(directory.path()))
                    .await
                    .expect("open runtime store"),
            );
            let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
            let (polled_tx, mut polled_rx) = tokio::sync::mpsc::unbounded_channel();
            let (release_tx, release_rx) = mpsc::channel::<()>();
            let release_rx = Arc::new(Mutex::new(release_rx));
            let mut reads = (0..READS_TO_EXCEED_BLOCKING_POOL)
                .map(|_| {
                    let store = Arc::clone(&store);
                    let entered_tx = entered_tx.clone();
                    let polled_tx = polled_tx.clone();
                    let release_rx = Arc::clone(&release_rx);
                    tokio::spawn(async move {
                        let mut read = Box::pin(store.read_async(
                            "runtime-store/tests/read_async_server_pool_progress",
                            move |tx| {
                                entered_tx.send(()).expect("report admitted read");
                                release_rx
                                    .lock()
                                    .expect("release gate")
                                    .recv()
                                    .expect("release admitted read");
                                tx.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                                    .map_err(StoreError::from)
                            },
                        ));
                        let mut reported_pending = false;
                        std::future::poll_fn(|context| {
                            let result = std::future::Future::poll(read.as_mut(), context);
                            if result.is_pending() && !reported_pending {
                                reported_pending = true;
                                polled_tx.send(()).expect("report read admission wait");
                            }
                            result
                        })
                        .await
                    })
                })
                .collect::<Vec<_>>();

            let all_reads_polled = tokio::time::timeout(READ_POOL_TEST_WATCHDOG, async {
                for _ in 0..READS_TO_EXCEED_BLOCKING_POOL {
                    if polled_rx.recv().await.is_none() {
                        return false;
                    }
                }
                true
            })
            .await;
            let readers_entered = tokio::time::timeout(READ_POOL_TEST_WATCHDOG, async {
                for _ in 0..READ_CONNECTION_POOL_CAPACITY {
                    if entered_rx.recv().await.is_none() {
                        return false;
                    }
                }
                true
            })
            .await;

            let (progress_tx, progress_rx) = tokio::sync::oneshot::channel();
            let mut progress = crate::blocking_pool::spawn_blocking(
                "runtime-store/tests/read_async_server_pool_progress_witness",
                move || {
                    let _receiver_open = progress_tx.send(()).is_ok();
                },
            );
            let made_progress = tokio::time::timeout(READ_POOL_TEST_WATCHDOG, progress_rx).await;
            for _ in 0..READS_TO_EXCEED_BLOCKING_POOL {
                release_tx.send(()).expect("read gate is open");
            }
            let reads_completed = tokio::time::timeout(READ_POOL_TEST_WATCHDOG, async {
                for read in &mut reads {
                    if !matches!(read.await, Ok(Ok(1))) {
                        return false;
                    }
                }
                true
            })
            .await;
            if !matches!(reads_completed, Ok(true)) {
                for read in &reads {
                    read.abort();
                }
            }
            let progress_completed =
                tokio::time::timeout(READ_POOL_TEST_WATCHDOG, &mut progress).await;
            if progress_completed.is_err() {
                progress.abort();
            }
            assert!(matches!(all_reads_polled, Ok(true)));
            assert!(matches!(readers_entered, Ok(true)));
            assert!(matches!(made_progress, Ok(Ok(()))));
            assert!(matches!(reads_completed, Ok(true)));
            assert!(matches!(progress_completed, Ok(Ok(()))));
        });
    }
}

#[cfg(test)]
mod diagnostics_hook_tests {
    use super::*;
    use std::sync::{Condvar, Mutex, mpsc};

    static EVENTS: Mutex<Vec<StoreDiagnosticsEvent>> = Mutex::new(Vec::new());
    static EVENTS_READY: Condvar = Condvar::new();
    static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
        let _test_guard = TEST_LOCK.lock().await;
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
        let held = Arc::clone(&store.read_pool.admission)
            .acquire_many_owned(READ_CONNECTION_POOL_CAPACITY as u32)
            .await
            .expect("hold every read permit");
        let waiter_store = Arc::clone(&store);
        let waiter = tokio::spawn(async move {
            waiter_store
                .read_async("runtime-store/tests/read_waiter", |_| {
                    Ok::<_, StoreError>(())
                })
                .await
        });
        let observed =
            crate::blocking_pool::spawn_blocking("runtime-store/tests/read_waiter_event", || {
                wait_for_event(|event| {
                    matches!(
                        event,
                        StoreDiagnosticsEvent::ReadWaiters {
                            waiting_threads: 1,
                            peak_waiters: 1,
                            ..
                        }
                    )
                })
            })
            .await
            .expect("diagnostic wait joins");
        drop(held);
        let acquired = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .is_ok_and(|joined| matches!(joined, Ok(Ok(()))));
        let read_wait_observed = observed && acquired;
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
    async fn queued_writer_job_reports_queue_as_wait_and_its_caller_origin() {
        const BLOCKED: Duration = Duration::from_millis(150);
        let _test_guard = TEST_LOCK.lock().await;
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
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let slow_store = Arc::clone(&store);
        let slow = tokio::spawn(async move {
            slow_store
                .transaction(move |_| {
                    entered_tx.send(()).expect("report entered writer");
                    release_rx.recv().expect("release blocked writer");
                    Ok(())
                })
                .await
        });
        crate::blocking_pool::spawn_blocking("runtime-store/lib", move || {
            entered_rx.recv().expect("slow job entered")
        })
        .await
        .expect("entered wait");
        let queued_store = Arc::clone(&store);
        let queued_line = line!() + 3;
        let queued = tokio::spawn(async move {
            queued_store
                .transaction(|transaction| Ok(transaction.execute("SELECT 1", []).is_err()))
                .await
        });
        while store.writer.available_admissions() > sqlite_actor::MAX_PENDING_WRITES - 2 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(BLOCKED).await;
        release_tx.send(()).expect("release slow job");
        slow.await.expect("slow task").expect("slow job");
        queued.await.expect("queued task").expect("queued job");
        store
            .set_diagnostics_callback(None)
            .await
            .expect("clear diagnostics callback");
        let events = EVENTS.lock().expect("diagnostic events lock");
        let queued_event = events
            .iter()
            .find_map(|event| match event {
                StoreDiagnosticsEvent::WriterTransaction { wait, hold, origin }
                    if origin.line() == queued_line =>
                {
                    Some((*wait, *hold, origin.file()))
                }
                _ => None,
            })
            .expect("queued writer job event");
        assert!(queued_event.0 >= BLOCKED, "actor queue time is wait time");
        assert!(queued_event.1 < BLOCKED, "queued time is not hold time");
        assert!(queued_event.2.ends_with("runtime-store/src/lib.rs"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn analytics_writer_waits_for_canvas_actor_while_raw_reads_remain_independent() {
        let _test_guard = TEST_LOCK.lock().await;
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
        let _test_guard = TEST_LOCK.lock().await;
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

    fn sqlite_failure(code: i32) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None)
    }

    #[test]
    fn sqlite_lock_error_classifier_includes_primary_and_extended_codes_only() {
        for code in [
            rusqlite::ffi::SQLITE_BUSY,
            rusqlite::ffi::SQLITE_BUSY_RECOVERY,
            rusqlite::ffi::SQLITE_LOCKED,
            rusqlite::ffi::SQLITE_LOCKED_SHAREDCACHE,
        ] {
            assert!(is_sqlite_lock_error(&sqlite_failure(code)));
        }
        assert!(!is_sqlite_lock_error(&sqlite_failure(
            rusqlite::ffi::SQLITE_CONSTRAINT
        )));
        assert!(!is_sqlite_lock_error(&rusqlite::Error::InvalidQuery));
    }

    #[test]
    fn sqlite_store_error_preserves_rusqlite_source() {
        let error = StoreError::from(sqlite_failure(rusqlite::ffi::SQLITE_BUSY));
        let source = std::error::Error::source(&error).expect("SQLite error is the source");
        assert!(source.downcast_ref::<rusqlite::Error>().is_some());
    }

    #[tokio::test]
    async fn sqlite_lock_errors_emit_once_at_each_typed_error_boundary() {
        let _test_guard = TEST_LOCK.lock().await;
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open runtime store");
        EVENTS.lock().expect("diagnostic events lock").clear();
        store
            .set_diagnostics_callback(Some(record_event))
            .await
            .expect("install diagnostics callback");

        let before = sqlite_lock_error_count();
        let _store_error = StoreError::from(sqlite_failure(rusqlite::ffi::SQLITE_BUSY));
        let _unknown_commit =
            StoreError::commit_outcome_unknown(sqlite_failure(rusqlite::ffi::SQLITE_LOCKED));
        let _control_error = StoreError::writer_actor_control(tokio_rusqlite::Error::Error(
            sqlite_failure(rusqlite::ffi::SQLITE_BUSY_RECOVERY),
        ));
        let close_connection = tokio_rusqlite::Connection::open_in_memory()
            .await
            .expect("open connection for typed actor-close error");
        let close_error =
            actor_transport_outcome_unknown::<StoreError>(tokio_rusqlite::Error::Close((
                close_connection,
                sqlite_failure(rusqlite::ffi::SQLITE_LOCKED_SHAREDCACHE),
            )));
        assert!(matches!(
            close_error,
            StoreError::CommitActorOutcomeUnknown(tokio_rusqlite::Error::Close((_, _)))
        ));
        let _closed_actor =
            StoreError::writer_actor_control(tokio_rusqlite::Error::ConnectionClosed);
        let _non_lock_error = StoreError::from(rusqlite::Error::InvalidQuery);

        let totals: Vec<_> = {
            let events = EVENTS.lock().expect("diagnostic events lock");
            events
                .iter()
                .filter_map(|event| match event {
                    StoreDiagnosticsEvent::SqliteLockError { total } => Some(*total),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(totals, vec![before + 1, before + 2, before + 3, before + 4]);
        store
            .set_diagnostics_callback(None)
            .await
            .expect("clear diagnostics callback");
    }

    #[tokio::test]
    async fn sqlite_lock_count_survives_disabled_and_contended_observer() {
        let _test_guard = TEST_LOCK.lock().await;
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open runtime store");
        store
            .set_diagnostics_callback(None)
            .await
            .expect("disable diagnostics callback");

        let before_disabled = sqlite_lock_error_count();
        observe_sqlite_lock_error(&sqlite_failure(rusqlite::ffi::SQLITE_BUSY));
        assert_eq!(sqlite_lock_error_count(), before_disabled + 1);

        EVENTS.lock().expect("diagnostic events lock").clear();
        store
            .set_diagnostics_callback(Some(record_event))
            .await
            .expect("install diagnostics callback");
        let before_contended = sqlite_lock_error_count();
        let callback_guard = diagnostics_callback()
            .lock()
            .expect("hold diagnostics callback lock");
        observe_sqlite_lock_error(&sqlite_failure(rusqlite::ffi::SQLITE_LOCKED));
        assert_eq!(sqlite_lock_error_count(), before_contended + 1);
        drop(callback_guard);
        assert!(
            EVENTS.lock().expect("diagnostic events lock").is_empty(),
            "the observer intentionally drops an event while contended"
        );
        store
            .set_diagnostics_callback(None)
            .await
            .expect("clear diagnostics callback");
    }

    #[tokio::test]
    async fn terminal_writer_busy_error_is_observed_once() {
        let _test_guard = TEST_LOCK.lock().await;
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open runtime store");
        EVENTS.lock().expect("diagnostic events lock").clear();
        store
            .set_diagnostics_callback(Some(record_event))
            .await
            .expect("install diagnostics callback");

        let path = store.root.join("canvas.sqlite3");
        let blocker = Connection::open(path).expect("open competing test connection");
        blocker
            .execute_batch("BEGIN IMMEDIATE")
            .expect("hold one test-only writer lock");
        store
            .writer
            .call(|writer| {
                writer.busy_timeout(Duration::ZERO)?;
                Ok::<(), rusqlite::Error>(())
            })
            .await
            .expect("set test-only zero busy timeout");

        let result = store.transaction(|_| Ok::<(), StoreError>(())).await;
        assert!(matches!(
            result,
            Err(StoreError::Sqlite(error))
                if error.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy)
        ));
        blocker
            .execute_batch("ROLLBACK")
            .expect("release test-only writer lock");

        assert_eq!(
            EVENTS
                .lock()
                .expect("diagnostic events lock")
                .iter()
                .filter(|event| matches!(event, StoreDiagnosticsEvent::SqliteLockError { .. }))
                .count(),
            1
        );
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
        for table in OBSERVED_TABLES
            .iter()
            .map(|table| table.name())
            .chain(ENTITY_RESOURCE_TABLES)
        {
            database
                .execute_batch(&format!("CREATE TABLE {}(id TEXT PRIMARY KEY);", table))
                .expect("create test table");
        }
        verify_observed_tables_are_rowid(&database).expect("all observed tables use rowid storage");

        for table in [
            ObservedTable::AccessClients.name(),
            ObservedTable::EventMeta.name(),
            ObservedTable::OperationReceipts.name(),
            ObservedTable::NativeActionReceipts.name(),
            ObservedTable::AccessReceipts.name(),
            ENTITY_RESOURCE_TABLES[0],
            ENTITY_RESOURCE_TABLES[1],
        ] {
            database
                .execute_batch(&format!("DROP TABLE {};", table))
                .expect("drop rowid test table");
            database
                .execute_batch(&format!(
                    "CREATE TABLE {}(id TEXT PRIMARY KEY) WITHOUT ROWID;",
                    table
                ))
                .expect("create without-rowid test table");
            assert!(matches!(
                verify_observed_tables_are_rowid(&database),
                Err(StoreError::SchemaMismatch(_))
            ));

            database
                .execute_batch(&format!("DROP TABLE {};", table))
                .expect("drop without-rowid table");
            database
                .execute_batch(&format!("CREATE TABLE {}(id TEXT PRIMARY KEY);", table))
                .expect("restore rowid test table");
        }
        verify_observed_tables_are_rowid(&database).expect("all observed tables use rowid storage");
    }

    #[test]
    fn table_change_mask_covers_each_named_observer_table() {
        let database = Connection::open_in_memory().expect("in-memory database");
        for table in OBSERVED_TABLES {
            database
                .execute_batch(&format!(
                    "CREATE TABLE {}(id INTEGER PRIMARY KEY);",
                    table.name()
                ))
                .expect("create observer table");
        }
        let table_change_flags = Arc::new(AtomicU32::new(0));
        let receipt_update_count = Arc::new(AtomicU64::new(0));
        install_table_change_hooks(
            &database,
            Arc::clone(&table_change_flags),
            Arc::clone(&receipt_update_count),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU32::new(0)),
            Arc::new(Mutex::new(ChangedRowHints::default())),
        )
        .expect("install update and delete hooks");

        for (index, table) in OBSERVED_TABLES.into_iter().enumerate() {
            let insert = format!("INSERT INTO {}(id) VALUES (?1)", table.name());
            database
                .execute(&insert, [index as i64])
                .expect("insert observed row");
            assert_eq!(
                table_change_flags.swap(0, Ordering::AcqRel),
                table.change_bit()
            );
            assert_eq!(
                receipt_update_count.swap(0, Ordering::AcqRel),
                if table.suppressible_resource_receipt() {
                    1
                } else {
                    0
                },
                "only named internal receipts count toward the resource filter"
            );
        }
    }

    #[test]
    fn observed_table_names_and_change_bits_are_unique_and_fit_the_mask() {
        let mut names = std::collections::BTreeSet::new();
        let mut bits = std::collections::BTreeSet::new();
        assert!(OBSERVED_TABLES.len() <= u32::BITS as usize);
        for table in OBSERVED_TABLES {
            assert!(names.insert(table.name()), "duplicate observer table name");
            let bit = table.change_bit();
            assert_eq!(bit.count_ones(), 1, "observer table must own one mask bit");
            assert!(bits.insert(bit), "duplicate observer table mask bit");
        }
        assert_eq!(
            [
                (
                    ObservedTable::AccountTransfers.name(),
                    ObservedTable::AccountTransfers.change_bit()
                ),
                (
                    ObservedTable::OrchestrationSettings.name(),
                    ObservedTable::OrchestrationSettings.change_bit()
                ),
                (
                    ObservedTable::RuntimeRequests.name(),
                    ObservedTable::RuntimeRequests.change_bit()
                ),
                (
                    ObservedTable::RuntimeToolRequests.name(),
                    ObservedTable::RuntimeToolRequests.change_bit()
                ),
            ],
            [
                ("runtime_account_transfers", 1 << 22),
                ("runtime_orchestration_settings", 1 << 23),
                ("runtime_requests", 1 << 24),
                ("runtime_tool_requests", 1 << 25),
            ],
            "idle-unload observer owners keep their exact table and change-mask contracts"
        );
    }

    #[tokio::test]
    async fn table_commit_observer_tracks_only_successful_target_changes() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open runtime store");
        let access_notifications = Arc::new(AtomicUsize::new(0));
        let access_observed = Arc::clone(&access_notifications);
        store.add_table_change_observer(ObservedTable::AccessClients, move || {
            access_observed.fetch_add(1, Ordering::AcqRel);
        });
        let usage_notifications = Arc::new(AtomicUsize::new(0));
        let usage_observed = Arc::clone(&usage_notifications);
        store.add_table_change_observer(ObservedTable::UsageResumes, move || {
            usage_observed.fetch_add(1, Ordering::AcqRel);
        });

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
    async fn entity_resource_hints_cover_tombstones_deletes_and_unknown_commits() {
        let directory = tempfile::tempdir().expect("isolated state");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("store");
        let hints = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&hints);
        store.add_resource_change_observer(move |change| {
            observed.lock().expect("hints").push(change);
        });
        store
            .transaction(|tx| {
                SyncDocumentsRepository::put_entity(
                    tx,
                    "agent",
                    "agent",
                    serde_json::json!({"id":"agent"}),
                    false,
                )?;
                Ok(())
            })
            .await
            .expect("entity insert");
        store
            .transaction(|tx| {
                SyncDocumentsRepository::put_entity(
                    tx,
                    "agent",
                    "agent",
                    serde_json::json!({}),
                    true,
                )?;
                Ok(())
            })
            .await
            .expect("tombstone and metadata update");
        store
            .transaction(|tx| {
                tx.execute("DELETE FROM sync_entities", [])?;
                Ok(())
            })
            .await
            .expect("delete hook includes every entity and metadata row");
        assert_eq!(
            *hints.lock().expect("hints"),
            vec![ResourceChange::Entities; 3]
        );
        store
            .simulate_unknown_commit_after_success
            .store(true, Ordering::Release);
        let result = store
            .transaction(|tx| {
                SyncDocumentsRepository::put_entity(
                    tx,
                    "agent",
                    "agent",
                    serde_json::json!({"id":"agent"}),
                    false,
                )?;
                Ok(())
            })
            .await;
        assert!(matches!(result, Err(StoreError::CommitOutcomeUnknown(_))));
        assert_eq!(
            *hints.lock().expect("hints"),
            vec![
                ResourceChange::Entities,
                ResourceChange::Entities,
                ResourceChange::Entities,
                ResourceChange::All,
            ]
        );
    }

    #[tokio::test]
    async fn resource_observer_suppresses_only_proven_receipt_only_writes() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open runtime store");
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&notifications);
        store.add_resource_change_observer(move |_| {
            observed.fetch_add(1, Ordering::AcqRel);
        });

        let unpolled = store.transaction(|transaction| {
            transaction.execute(
                "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES ('unpolled','agent','message','text','queued',1.0,1)",
                [],
            )?;
            Ok(())
        });
        drop(unpolled);
        assert_eq!(notifications.load(Ordering::Acquire), 0);

        let receipt_inserts = [
            "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES ('op-only','sig','{}')",
            "INSERT INTO runtime_native_action_receipts(id,signature,receipt,outcome) VALUES ('native-only','sig','{}','{}')",
            "INSERT INTO runtime_access_receipts(client,request_id,hash,session,status,record) VALUES ('client-only','request-only','hash','session','pending','{}')",
        ];
        for sql in receipt_inserts {
            store
                .transaction(move |transaction| {
                    transaction.execute_batch(sql)?;
                    Ok(())
                })
                .await
                .expect("receipt-only write commits");
        }
        assert_eq!(notifications.load(Ordering::Acquire), 0);

        let mixed_receipt_inserts = [
            "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES ('op-mixed','sig','{}')",
            "INSERT INTO runtime_native_action_receipts(id,signature,receipt,outcome) VALUES ('native-mixed','sig','{}','{}')",
            "INSERT INTO runtime_access_receipts(client,request_id,hash,session,status,record) VALUES ('client-mixed','request-mixed','hash','session','pending','{}')",
        ];
        for (index, sql) in mixed_receipt_inserts.into_iter().enumerate() {
            store
                .transaction(move |transaction| {
                    transaction.execute_batch(sql)?;
                    transaction.execute(
                        "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES (?1,'agent','message','text','queued',1.0,1)",
                        [format!("resource-event-{index}")],
                    )?;
                    Ok(())
                })
                .await
                .expect("mixed receipt and resource write commits");
            assert_eq!(
                notifications.load(Ordering::Acquire),
                index + 1,
                "each internal receipt mixed with a resource row must wake"
            );
        }

        store
            .transaction(|transaction| {
                transaction.execute_batch(
                    "CREATE TABLE resource_receipt_trigger_side_effect(value INTEGER);
                     CREATE TRIGGER resource_receipt_trigger AFTER INSERT ON runtime_access_receipts
                     BEGIN INSERT INTO resource_receipt_trigger_side_effect VALUES (1); END;",
                )?;
                transaction.execute_batch(
                    "INSERT INTO runtime_access_receipts(client,request_id,hash,session,status,record)
                     VALUES ('trigger-client','trigger-request','hash','session','pending','{}')",
                )?;
                Ok(())
            })
            .await
            .expect("receipt trigger side-effect commits");
        assert_eq!(notifications.load(Ordering::Acquire), 4);

        store
            .transaction(|transaction| {
                let selected_value =
                    transaction.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))?;
                assert_eq!(selected_value, 1);
                Ok(())
            })
            .await
            .expect("no-op transaction succeeds");
        let rolled_back = store
            .transaction_with(|transaction| {
                transaction.execute_batch(
                    "INSERT INTO runtime_operation_receipts(id,signature,result)
                     VALUES ('op-rollback','sig','{}')",
                )?;
                Err::<(), StoreError>(StoreError::RecordContract("test rollback"))
            })
            .await;
        assert!(rolled_back.is_err());
        assert_eq!(notifications.load(Ordering::Acquire), 4);

        store
            .simulate_unknown_commit_after_success
            .store(true, Ordering::Release);
        let receipt_unknown = store
            .transaction(|transaction| {
                transaction.execute_batch(
                    "INSERT INTO runtime_operation_receipts(id,signature,result)
                     VALUES ('op-unknown-receipt','sig','{}')",
                )?;
                Ok(())
            })
            .await;
        assert!(matches!(
            receipt_unknown,
            Err(StoreError::CommitOutcomeUnknown(_))
        ));
        assert_eq!(notifications.load(Ordering::Acquire), 5);

        store
            .simulate_unknown_commit_after_success
            .store(true, Ordering::Release);
        let resource_unknown = store
            .transaction(|transaction| {
                transaction.execute(
                    "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES ('unknown-resource','agent','message','text','queued',1.0,1)",
                    [],
                )?;
                Ok(())
            })
            .await;
        assert!(matches!(
            resource_unknown,
            Err(StoreError::CommitOutcomeUnknown(_))
        ));
        assert_eq!(notifications.load(Ordering::Acquire), 6);

        store
            .writer
            .call(|connection| {
                connection.update_hook(
                    None::<
                        for<'database, 'table> fn(
                            rusqlite::hooks::Action,
                            &'database str,
                            &'table str,
                            i64,
                        ),
                    >,
                )?;
                Ok::<(), StoreError>(())
            })
            .await
            .expect("temporarily remove update hook");
        store
            .transaction(|transaction| {
                transaction.execute_batch(
                    "INSERT INTO runtime_operation_receipts(id,signature,result)
                     VALUES ('op-missing-hook','sig','{}')",
                )?;
                Ok(())
            })
            .await
            .expect("receipt write without update hook commits");
        assert_eq!(notifications.load(Ordering::Acquire), 7);

        let table_change_flags = Arc::clone(&store.table_change_flags);
        let receipt_update_count = Arc::clone(&store.resource_receipt_update_count);
        let entity_update_count = Arc::clone(&store.resource_entity_update_count);
        let row_interest = Arc::clone(&store.table_row_interest);
        let changed_rows = Arc::clone(&store.changed_rows);
        store
            .writer
            .call(move |connection| {
                install_table_change_hooks(
                    connection,
                    table_change_flags,
                    receipt_update_count,
                    entity_update_count,
                    row_interest,
                    changed_rows,
                )
                .map_err(StoreError::from)?;
                connection
                    .authorizer(None::<for<'context> fn(AuthContext<'context>) -> Authorization>)
                    .map_err(StoreError::from)?;
                Ok::<(), StoreError>(())
            })
            .await
            .expect("restore update hook and remove delete authorizer");
        store
            .transaction(|transaction| {
                transaction.execute("DELETE FROM runtime_operation_receipts", [])?;
                Ok(())
            })
            .await
            .expect("receipt bulk delete commits");
        assert_eq!(notifications.load(Ordering::Acquire), 8);
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
        store.add_table_change_observer(ObservedTable::UsageResumes, move || {
            first_count.fetch_add(1, Ordering::AcqRel);
            if !registered_second_callback.swap(true, Ordering::AcqRel) {
                let second_count = Arc::clone(&second_count);
                weak_store
                    .upgrade()
                    .expect("store remains alive")
                    .add_table_change_observer(ObservedTable::UsageResumes, move || {
                        second_count.fetch_add(1, Ordering::AcqRel);
                    });
            }
        });

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
        store.add_table_change_observer(ObservedTable::AccessClients, move || {
            notified.fetch_add(1, Ordering::AcqRel);
        });
        let resource_notifications = Arc::new(AtomicUsize::new(0));
        let resource_notified = Arc::clone(&resource_notifications);
        store.add_resource_change_observer(move |_| {
            resource_notified.fetch_add(1, Ordering::AcqRel);
        });
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
        assert_eq!(resource_notifications.load(Ordering::Acquire), 1);
        assert_eq!(post_commit_calls.load(Ordering::Acquire), 0);
        let reader = store.test_reader().expect("read committed access client");
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

    /// Hang guard for writer witnesses: a stuck commit fails the test instead of blocking the suite.
    const WRITER_WITNESS_HANG_GUARD: std::time::Duration = std::time::Duration::from_secs(10);

    /// Each entry is one observed change: `Some(rowids)` for row hints, `None` for a full reread.
    type RecordedChanges = Arc<Mutex<Vec<Option<Vec<i64>>>>>;

    fn recording_observer(
        recorded: &RecordedChanges,
    ) -> impl for<'a> Fn(TableChange<'a>) + Send + Sync + 'static {
        let recorded = Arc::clone(recorded);
        move |change| {
            recorded
                .lock()
                .expect("recorded table changes")
                .push(match change {
                    TableChange::Rows(row_ids) => Some(row_ids.to_vec()),
                    TableChange::Full => None,
                });
        }
    }

    fn record_changes(store: &RuntimeStore, table: ObservedTable) -> RecordedChanges {
        let recorded: RecordedChanges = Arc::new(Mutex::new(Vec::new()));
        store.add_table_row_change_observer(table, recording_observer(&recorded));
        recorded
    }

    fn access_client_row_ids(store: &RuntimeStore) -> Vec<i64> {
        let db = store.test_reader().expect("read connection");
        let mut statement = db
            .prepare("SELECT rowid FROM runtime_access_clients ORDER BY rowid")
            .expect("prepare rowid query");
        statement
            .query_map([], |row| row.get::<_, i64>(0))
            .expect("query rowids")
            .collect::<Result<Vec<_>, _>>()
            .expect("read rowids")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn row_observer_receives_committed_rowids_in_write_order() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open runtime store"),
        );
        let recorded = record_changes(&store, ObservedTable::AccessClients);
        let before = access_client_row_ids(&store);
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_access_clients(id, record) VALUES ('client-a', '{}')",
                    [],
                )?;
                tx.execute(
                    "INSERT INTO runtime_access_clients(id, record) VALUES ('client-b', '{}')",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("commit two client rows");
        let written = access_client_row_ids(&store)
            .into_iter()
            .filter(|row_id| !before.contains(row_id))
            .collect::<Vec<_>>();
        assert_eq!(written.len(), 2);
        assert_eq!(*recorded.lock().expect("recorded"), vec![Some(written)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rolled_back_transaction_reports_no_table_change() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open runtime store"),
        );
        let recorded = record_changes(&store, ObservedTable::AccessClients);
        let result = store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_access_clients(id, record) VALUES ('client-rolled-back', '{}')",
                    [],
                )?;
                Err::<(), StoreError>(StoreError::RecordContract("test rollback"))
            })
            .await;
        assert!(result.is_err());
        assert!(recorded.lock().expect("recorded").is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_commit_outcome_reports_full_reread() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open runtime store"),
        );
        let recorded = record_changes(&store, ObservedTable::AccessClients);
        store
            .simulate_unknown_commit_after_success
            .store(true, Ordering::Release);
        let result = store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_access_clients(id, record) VALUES ('client-unknown', '{}')",
                    [],
                )?;
                Ok(())
            })
            .await;
        assert!(result.is_err());
        assert_eq!(*recorded.lock().expect("recorded"), vec![None]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn row_hint_cap_keeps_rows_at_limit_and_reports_full_past_it() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open runtime store"),
        );
        let recorded = record_changes(&store, ObservedTable::AccessClients);
        for (label, count) in [
            ("at-limit", CHANGED_ROW_HINT_LIMIT),
            ("over-limit", CHANGED_ROW_HINT_LIMIT + 1),
        ] {
            store
                .transaction(move |tx| {
                    for index in 0..count {
                        tx.execute(
                            "INSERT INTO runtime_access_clients(id, record) VALUES (?1, '{}')",
                            [format!("client-{label}-{index}")],
                        )?;
                    }
                    Ok(())
                })
                .await
                .expect("commit bounded client rows");
        }
        let changes = recorded.lock().expect("recorded");
        assert_eq!(changes.len(), 2);
        assert_eq!(
            changes[0].as_ref().map(Vec::len),
            Some(CHANGED_ROW_HINT_LIMIT)
        );
        assert_eq!(changes[1], None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn observer_registered_mid_transaction_gets_full_for_that_commit() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open runtime store"),
        );
        let recorded: RecordedChanges = Arc::new(Mutex::new(Vec::new()));
        let observer = recording_observer(&recorded);
        let weak_store = Arc::downgrade(&store);
        store
            .transaction(move |tx| {
                // This transaction started before the row interest existed, so its
                // rowids are incomplete and the observer must receive a full reread.
                weak_store
                    .upgrade()
                    .expect("store remains alive")
                    .add_table_row_change_observer(ObservedTable::AccessClients, observer);
                tx.execute(
                    "INSERT INTO runtime_access_clients(id, record) VALUES ('client-late', '{}')",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("commit with late observer");
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_access_clients(id, record) VALUES ('client-next', '{}')",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("commit with registered observer");
        let changes = recorded.lock().expect("recorded");
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0], None);
        assert_eq!(changes[1].as_ref().map(Vec::len), Some(1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn observer_registered_inside_callback_does_not_block_later_commits() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open runtime store"),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let call_count = Arc::clone(&calls);
        let weak_store = Arc::downgrade(&store);
        let nested_registered = Arc::new(AtomicBool::new(false));
        store.add_table_row_change_observer(ObservedTable::AccessClients, move |_change| {
            call_count.fetch_add(1, Ordering::Relaxed);
            if !nested_registered.swap(true, Ordering::Relaxed) {
                weak_store
                    .upgrade()
                    .expect("store remains alive")
                    .add_table_row_change_observer(ObservedTable::AccessClients, |_change| {});
            }
        });
        for client in ["client-callback-a", "client-callback-b"] {
            tokio::time::timeout(
                WRITER_WITNESS_HANG_GUARD,
                store.transaction(move |tx| {
                    tx.execute(
                        "INSERT INTO runtime_access_clients(id, record) VALUES (?1, '{}')",
                        [client],
                    )?;
                    Ok(())
                }),
            )
            .await
            .expect("writer does not hang after callback registration")
            .expect("commit client row");
        }
        assert_eq!(calls.load(Ordering::Relaxed), 2);
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
        store.add_table_change_observer(ObservedTable::AccessClients, move || {
            table_observed.fetch_add(1, Ordering::Release);
        });
        let resource_notifications = Arc::new(AtomicUsize::new(0));
        let resource_observed = Arc::clone(&resource_notifications);
        let published_cache_marker = Arc::new(AtomicUsize::new(0));
        let observer_seen_cache_marker = Arc::new(AtomicUsize::new(0));
        let published_marker_for_observer = Arc::clone(&published_cache_marker);
        let seen_marker = Arc::clone(&observer_seen_cache_marker);
        store.add_resource_change_observer(move |_| {
            seen_marker.store(
                published_marker_for_observer.load(Ordering::Acquire),
                Ordering::Release,
            );
            resource_observed.fetch_add(1, Ordering::Release);
        });
        let post_commit_callbacks = Arc::new(AtomicUsize::new(0));

        let normal_callback_count = Arc::clone(&post_commit_callbacks);
        let normal_published_marker = Arc::clone(&published_cache_marker);
        store
            .transaction_with_post_commit(
                |transaction| {
                    transaction.execute(
                        "INSERT INTO runtime_access_clients(id, record) VALUES ('normal-client', '{}')",
                        [],
                    )?;
                    Ok::<(), StoreError>(())
                },
                move |_| {
                    normal_callback_count.fetch_add(1, Ordering::Release);
                    normal_published_marker.store(1, Ordering::Release);
                },
            )
            .await
            .expect("normal committed write publishes before resource invalidation");
        assert_eq!(observer_seen_cache_marker.load(Ordering::Acquire), 1);
        assert_eq!(post_commit_callbacks.load(Ordering::Acquire), 1);
        assert_eq!(table_notifications.load(Ordering::Acquire), 1);
        assert_eq!(resource_notifications.load(Ordering::Acquire), 1);
        assert_eq!(notifications.load(Ordering::Acquire), 1);

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
        let canceled_published_marker = Arc::clone(&published_cache_marker);
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
                canceled_published_marker.store(2, Ordering::Release);
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
            while notifications.load(Ordering::Acquire) != 2
                || table_notifications.load(Ordering::Acquire) != 2
                || resource_notifications.load(Ordering::Acquire) != 2
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("abandoned committed write still notifies observers");
        assert_eq!(post_commit_callbacks.load(Ordering::Acquire), 2);
        assert_eq!(table_notifications.load(Ordering::Acquire), 2);
        assert_eq!(resource_notifications.load(Ordering::Acquire), 2);
        assert_eq!(observer_seen_cache_marker.load(Ordering::Acquire), 2);

        let reader = store.test_reader().expect("read committed row");
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
        assert!(
            transaction_task
                .await
                .is_err_and(|error| error.is_cancelled()),
            "aborted writer caller must report task cancellation"
        );
        drop(store);

        release.wait();
        let server_id = crate::blocking_pool::spawn_blocking("runtime-store/lib", move || {
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
        let connection = store.test_reader().expect("read-only pull connection");
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
