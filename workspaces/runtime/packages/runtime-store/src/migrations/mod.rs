//! Server-owned format transitions. Each database target commits once; a failed
//! target rolls back completely. Earlier completed targets stay committed, and
//! restart continues from their existing format markers. No committed downgrade
//! or cross-file atomicity is promised. The caller holds its existing state lock.

pub(crate) mod m2026_10_20_agents;
pub(crate) mod m2026_10_22_analytics;
pub(crate) mod m2026_10_26_canvas;
mod m2026_10_26_provider_identity;
mod m2026_10_26_registry;
mod m2026_10_26_terminal_errors;

#[cfg(any(test, feature = "test-support"))]
pub use m2026_10_20_agents::migrate_2026_10_20;
#[cfg(any(test, feature = "test-support"))]
pub use m2026_10_20_agents::prepare_columns;
pub use m2026_10_26_provider_identity::TITLE_FINGERPRINT_PROVIDER_2026_10_26;
#[cfg(any(test, feature = "test-support"))]
pub use m2026_10_26_provider_identity::migrate_canvas_2026_10_26;

use std::path::Path;

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};
use studio_runtime_schema::studio_errors::{
    MigrationPhase, MigrationRollbackOutcome, MigrationTarget,
};

use crate::{StoreError, StoreResult};
use m2026_10_26_provider_identity::Accounts;

/// Calendar date of the format transition, never an automatic expiry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MigrationDate {
    year: u16,
    month: u8,
    day: u8,
}

/// The catalog owns dates, supported source descriptions and removal conditions.
#[derive(Clone, Copy, Debug)]
pub struct MigrationDefinition {
    pub id: &'static str,
    pub introduced: MigrationDate,
    pub target: MigrationTarget,
    pub supported_source: &'static str,
    pub remove_when: &'static str,
}

const OCTOBER_26: MigrationDate = MigrationDate {
    year: 2026,
    month: 10,
    day: 26,
};
const OCTOBER_22: MigrationDate = MigrationDate {
    year: 2026,
    month: 10,
    day: 22,
};
const OCTOBER_20: MigrationDate = MigrationDate {
    year: 2026,
    month: 10,
    day: 20,
};
/// Stable migration identifier; includes the introduction date.
const ANALYTICS_PROVIDER_ID: &str = "2026-10-26-analytics-provider";
/// Stable migration identifier; includes the introduction date.
const ANALYTICS_PROJECTION_ID: &str = "2026-10-22-analytics-projection";
/// Stable migration identifier; includes the introduction date.
const CANVAS_PROVIDER_ID: &str = "2026-10-26-canvas-provider";
/// Stable migration identifier; includes the introduction date.
const AGENT_STORAGE_ID: &str = "2026-10-20-agent-storage";
/// Stable migration identifier; includes the introduction date.
const CANVAS_COLUMNS_ID: &str = "2026-10-26-canvas-columns";
/// Stable migration identifier; includes the introduction date.
const WORKSPACE_LIMITS_ID: &str = "2026-10-26-workspace-limits";
/// Stable migration identifier; includes the introduction date.
const RETIRED_WAITS_ID: &str = "2026-10-26-retired-waits";
/// Stable migration identifier; includes the introduction date.
const SUPERVISOR_ID: &str = "2026-10-26-supervisor-journal";
/// Stable migration identifier; includes the introduction date.
const TERMINAL_ERRORS_ID: &str = "2026-10-26-terminal-errors";
const REMOVE_AFTER_SOURCE_RETIRED: &str = "Supported installations and restorable backups no longer admit the source format; pending historical effects are converted or terminal";

#[derive(Clone, Copy)]
enum SourceDatabase<'a> {
    Canvas(&'a Connection),
    Independent,
}

struct SqlContext<'a, 'db> {
    transaction: &'a Transaction<'db>,
    accounts: &'a Accounts,
    source: SourceDatabase<'a>,
}
impl SqlContext<'_, '_> {
    fn canvas(&self) -> StoreResult<&Connection> {
        match self.source {
            SourceDatabase::Canvas(db) => Ok(db),
            SourceDatabase::Independent => Err(StoreError::RecordContract(
                "migration requires the Canvas source database",
            )),
        }
    }
}

/// The sole typed migration contract. The runner owns transaction rollback;
/// a committed migration has no generic inverse.
enum Context<'a, 'db> {
    Sql(SqlContext<'a, 'db>),
    Registry(&'a Option<m2026_10_26_registry::PreparedRegistry>),
}

trait Migration: Sync {
    fn definition(&self) -> MigrationDefinition;
    fn apply(&self, context: &Context<'_, '_>) -> StoreResult<()>;
}

struct SqlMigration {
    definition: MigrationDefinition,
    apply: fn(&SqlContext<'_, '_>) -> StoreResult<()>,
}

impl Migration for SqlMigration {
    fn definition(&self) -> MigrationDefinition {
        self.definition
    }
    fn apply(&self, context: &Context<'_, '_>) -> StoreResult<()> {
        match context {
            Context::Sql(sql) => (self.apply)(sql),
            Context::Registry(_) => Err(StoreError::RecordContract(
                "SQL migration requires a transaction",
            )),
        }
    }
}

struct RegistryMigration;
impl Migration for RegistryMigration {
    fn definition(&self) -> MigrationDefinition {
        m2026_10_26_registry::DEFINITION
    }
    fn apply(&self, context: &Context<'_, '_>) -> StoreResult<()> {
        match context {
            Context::Registry(prepared) => m2026_10_26_registry::apply(prepared.as_ref()),
            Context::Sql(_) => Err(StoreError::RecordContract(
                "Registry migration requires a prepared file",
            )),
        }
    }
}

// Explicit dependency order: provider leaves precede strict generated columns;
// columns precede indexes; volatile data moves before its new triggers exist.
static CATALOG: &[&dyn Migration] = &[
    &SqlMigration {
        definition: MigrationDefinition {
            id: ANALYTICS_PROVIDER_ID,
            introduced: OCTOBER_26,
            target: MigrationTarget::Analytics,
            supported_source: "Analytics without providerIdentityFormat; known providers only",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| {
            m2026_10_26_provider_identity::migrate_analytics_2026_10_26(c.transaction, c.accounts)
        },
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: ANALYTICS_PROJECTION_ID,
            introduced: OCTOBER_22,
            target: MigrationTarget::Analytics,
            supported_source: "Retired Python cost projection and pre-current aggregate columns",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| {
            m2026_10_22_analytics::migrate_2026_10_22_drop_python_cost_projection(c.transaction)?;
            m2026_10_22_analytics::aggregate_columns(c.transaction)?;
            crate::schema::ensure_analytics_limits_index(c.transaction)?;
            crate::repositories::cost_aggregate::install(c.transaction)
        },
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: CANVAS_PROVIDER_ID,
            introduced: OCTOBER_26,
            target: MigrationTarget::Canvas,
            supported_source: "Canvas without an explicit provider column; known historical identities",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| {
            m2026_10_26_provider_identity::migrate_canvas_2026_10_26(c.transaction, c.accounts)
        },
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: AGENT_STORAGE_ID,
            introduced: OCTOBER_20,
            target: MigrationTarget::Canvas,
            supported_source: "JSON agent columns and pre-2026-10-21 capacity column",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| {
            m2026_10_20_agents::prepare_columns(c.transaction)?;
            crate::bootstrap::prepare_sync_schema(c.transaction)?;
            crate::bootstrap::prepare_runtime_event_indexes(c.transaction)?;
            crate::AgentRepository::prepare_change_journal(c.transaction)?;
            if !crate::schema::table_exists(
                c.transaction,
                crate::agent_volatile::AGENT_VOLATILE_TABLE,
            )? {
                c.transaction
                    .execute_batch(&crate::agent_volatile::TABLE_SQL)?;
                m2026_10_20_agents::migrate_2026_10_20(c.transaction)?;
            }
            crate::agent_volatile::prepare(c.transaction)
        },
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: CANVAS_COLUMNS_ID,
            introduced: OCTOBER_26,
            target: MigrationTarget::Canvas,
            supported_source: "Terminal output without generation and budget receipts without category/details columns",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| {
            crate::feature_schema::prepare(c.transaction)?;
            m2026_10_26_canvas::feature_columns(c.transaction)?;
            crate::StartupRecoveryRepository::prepare_indexes(c.transaction)?;
            crate::MonitorsRepository::prepare_indexes(c.transaction)?;
            crate::AgentIndexedReads::prepare_index(c.transaction)
        },
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: WORKSPACE_LIMITS_ID,
            introduced: OCTOBER_26,
            target: MigrationTarget::Canvas,
            supported_source: "Workspace rateLimits/rateLimitsByAccount before per-account entities",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_26_canvas::migrate_workspace_limits(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: RETIRED_WAITS_ID,
            introduced: OCTOBER_26,
            target: MigrationTarget::Canvas,
            supported_source: "Retired auth_probe_wait and provably unsubmitted capacity attempts; prepared callback cache",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| {
            m2026_10_26_canvas::migrate_auth_probe_waits(c.transaction)?;
            m2026_10_26_canvas::release_capacity_start_attempts(
                c.transaction,
                m2026_10_26_canvas::unix_now(),
            )?;
            m2026_10_26_canvas::delete_prepared_callback_cache(c.transaction)
        },
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: SUPERVISOR_ID,
            introduced: OCTOBER_26,
            target: MigrationTarget::Supervisor,
            supported_source: "Existing supervisor journal schema; original identities and receipts retained",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| crate::supervisor_journal::prepare_schema(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: TERMINAL_ERRORS_ID,
            introduced: OCTOBER_26,
            target: MigrationTarget::Analytics,
            supported_source: "Incomplete terminalErrorRepairV1 checkpoint and exact persisted native error notices",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_26_terminal_errors::apply(c.transaction, c.canvas()?),
    },
    &RegistryMigration,
];

/// Operator-facing catalog; implementation and metadata have one owner.
pub fn catalog() -> impl Iterator<Item = MigrationDefinition> {
    CATALOG.iter().map(|step| step.definition())
}

fn failure(
    id: &'static str,
    target: MigrationTarget,
    phase: MigrationPhase,
    rollback: MigrationRollbackOutcome,
    source: StoreError,
) -> StoreError {
    StoreError::Migration {
        id,
        target,
        phase,
        rollback,
        source: Box::new(source),
        rollback_error: None,
    }
}

fn preflight_sql_sources(canvas: &Path, analytics: &Path) -> StoreResult<()> {
    let canvas = crate::open_database(canvas, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let analytics = crate::open_database(analytics, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    m2026_10_20_agents::preflight(&canvas).map_err(|e| {
        failure(
            AGENT_STORAGE_ID,
            MigrationTarget::Canvas,
            MigrationPhase::Validate,
            MigrationRollbackOutcome::NotRequired,
            e,
        )
    })?;
    m2026_10_26_canvas::preflight_workspace(&canvas).map_err(|e| {
        failure(
            WORKSPACE_LIMITS_ID,
            MigrationTarget::Canvas,
            MigrationPhase::Validate,
            MigrationRollbackOutcome::NotRequired,
            e,
        )
    })?;
    m2026_10_26_canvas::preflight_retired(&canvas).map_err(|error| {
        failure(
            RETIRED_WAITS_ID,
            MigrationTarget::Canvas,
            MigrationPhase::Validate,
            MigrationRollbackOutcome::NotRequired,
            error,
        )
    })?;
    m2026_10_22_analytics::preflight(&analytics).map_err(|e| {
        failure(
            ANALYTICS_PROJECTION_ID,
            MigrationTarget::Analytics,
            MigrationPhase::Validate,
            MigrationRollbackOutcome::NotRequired,
            e,
        )
    })?;
    m2026_10_26_terminal_errors::preflight(&analytics, &canvas).map_err(|error| {
        failure(
            TERMINAL_ERRORS_ID,
            MigrationTarget::Analytics,
            MigrationPhase::Validate,
            MigrationRollbackOutcome::NotRequired,
            error,
        )
    })
}

fn rolled_back(
    tx: Transaction<'_>,
    id: &'static str,
    target: MigrationTarget,
    phase: MigrationPhase,
    source: StoreError,
) -> StoreError {
    match tx.rollback() {
        Ok(()) => failure(
            id,
            target,
            phase,
            MigrationRollbackOutcome::RolledBack,
            source,
        ),
        Err(error) => StoreError::Migration {
            id,
            target,
            phase: MigrationPhase::Rollback,
            rollback: MigrationRollbackOutcome::Unknown,
            source: Box::new(failure(
                id,
                target,
                phase,
                MigrationRollbackOutcome::Unknown,
                source,
            )),
            rollback_error: Some(error),
        },
    }
}

fn apply_target(
    db: &mut Connection,
    target: MigrationTarget,
    accounts: &Accounts,
    source_database: SourceDatabase<'_>,
) -> StoreResult<()> {
    let first = CATALOG
        .iter()
        .find(|step| step.definition().target == target)
        .ok_or(StoreError::RecordContract(
            "migration target has no catalog entry",
        ))?;
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| {
            failure(
                first.definition().id,
                target,
                MigrationPhase::Apply,
                MigrationRollbackOutcome::NotRequired,
                error.into(),
            )
        })?;
    let mut last = first.definition().id;
    for step in CATALOG
        .iter()
        .filter(|step| step.definition().target == target)
    {
        last = step.definition().id;
        if let Err(source) = step.apply(&Context::Sql(SqlContext {
            transaction: &tx,
            accounts,
            source: source_database,
        })) {
            return Err(rolled_back(tx, last, target, MigrationPhase::Apply, source));
        }
    }
    let verified = match target {
        MigrationTarget::Canvas => crate::schema::verify_bootstrap(&tx)
            .and_then(|()| crate::verify_observed_tables_are_rowid(&tx)),
        MigrationTarget::Supervisor => crate::supervisor_journal::validate_schema(
            &tx,
            crate::supervisor_journal::SchemaPresence::Complete,
        ),
        MigrationTarget::Analytics => Ok(()),
        MigrationTarget::AccountRegistry => Err(StoreError::RecordContract(
            "registry is not a SQL migration target",
        )),
    };
    if let Err(source) = verified {
        return Err(rolled_back(
            tx,
            last,
            target,
            MigrationPhase::Validate,
            source,
        ));
    }
    // Keep ownership after COMMIT failure: consuming Transaction::commit would
    // discard a failed rollback from Drop and lose its diagnostic evidence.
    match tx.execute_batch("COMMIT") {
        Ok(()) => Ok(()),
        Err(error) if tx.is_autocommit() => Err(failure(
            last,
            target,
            MigrationPhase::Commit,
            MigrationRollbackOutcome::Unknown,
            error.into(),
        )),
        Err(error) => Err(rolled_back(
            tx,
            last,
            target,
            MigrationPhase::Commit,
            error.into(),
        )),
    }
}

/// Run under runtime.lock, before ordinary recovery, workers, readers or readiness.
pub(crate) fn run(root: &Path) -> StoreResult<()> {
    let canvas_path = root.join("canvas.sqlite3");
    let analytics_path = root.join("analytics.sqlite3");
    crate::schema::verify_core(&canvas_path, &analytics_path)?;
    let registry = m2026_10_26_registry::preflight(root)?;
    let accounts = registry
        .as_ref()
        .map(|prepared| prepared.accounts.clone())
        .unwrap_or_default();
    m2026_10_26_provider_identity::validate_2026_10_26(root, &accounts)?;
    preflight_sql_sources(&canvas_path, &analytics_path)?;
    let mut analytics = crate::open_database(&analytics_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let mut canvas = crate::open_database(&canvas_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    crate::schema::register_python_trigger_functions(&canvas)?;
    crate::sqlite_durability::configure_writer(&analytics)?;
    crate::sqlite_durability::configure_writer(&canvas)?;
    let source_canvas = crate::open_database(&canvas_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    apply_target(
        &mut analytics,
        MigrationTarget::Analytics,
        &accounts,
        SourceDatabase::Canvas(&source_canvas),
    )?;
    apply_target(
        &mut canvas,
        MigrationTarget::Canvas,
        &accounts,
        SourceDatabase::Canvas(&source_canvas),
    )?;
    RegistryMigration.apply(&Context::Registry(&registry))
}

/// The supervisor calls this under supervisor.lock, before recovery and listening.
pub(crate) fn supervisor(db: &mut Connection) -> StoreResult<()> {
    let definition = catalog()
        .find(|entry| entry.target == MigrationTarget::Supervisor)
        .ok_or(StoreError::RecordContract(
            "supervisor missing from catalog",
        ))?;
    crate::supervisor_journal::validate_schema(
        db,
        crate::supervisor_journal::SchemaPresence::Existing,
    )
    .map_err(|source| {
        failure(
            definition.id,
            definition.target,
            MigrationPhase::Validate,
            MigrationRollbackOutcome::NotRequired,
            source,
        )
    })?;
    apply_target(
        db,
        MigrationTarget::Supervisor,
        &Accounts::new(),
        SourceDatabase::Independent,
    )
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn prepare_canvas_fixture(transaction: &Transaction<'_>) -> StoreResult<()> {
    let accounts = Accounts::new();
    for step in CATALOG
        .iter()
        .filter(|step| step.definition().target == MigrationTarget::Canvas)
    {
        step.apply(&Context::Sql(SqlContext {
            transaction,
            accounts: &accounts,
            source: SourceDatabase::Independent,
        }))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    #[test]
    fn migration_catalog_has_real_unique_dates_sources_and_removal_rules() {
        let mut ids = std::collections::BTreeSet::new();
        for entry in catalog() {
            assert!(ids.insert(entry.id), "duplicate migration ID");
            let date = entry.introduced;
            assert!((1..=12).contains(&date.month));
            let leap = date.year.is_multiple_of(4)
                && (!date.year.is_multiple_of(100) || date.year.is_multiple_of(400));
            let days = [
                31,
                if leap { 29 } else { 28 },
                31,
                30,
                31,
                30,
                31,
                31,
                30,
                31,
                30,
                31,
            ];
            assert!((1..=days[usize::from(date.month - 1)]).contains(&date.day));
            assert!(entry.id.starts_with(&format!(
                "{:04}-{:02}-{:02}-",
                date.year, date.month, date.day
            )));
            assert!(!entry.supported_source.is_empty());
            assert!(!entry.remove_when.is_empty());
        }
    }

    #[tokio::test]
    async fn migration_analytics_sql_fault_rolls_back_provider_and_schema_together() {
        let root = tempfile::tempdir().expect("state");
        drop(
            crate::RuntimeStore::open(Some(root.path()))
                .await
                .expect("fresh"),
        );
        let mut db = Connection::open(root.path().join("analytics.sqlite3")).expect("analytics");
        db.execute_batch("DELETE FROM analytics_meta WHERE key='providerIdentityFormat'; DROP TABLE analytics_cost_rows; DROP TABLE analytics_cost_groups;").expect("source");
        db.authorizer(Some(|action: AuthContext<'_>| {
            if matches!(
                action.action,
                AuthAction::CreateTable {
                    table_name: "analytics_cost_rows"
                }
            ) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))
        .expect("install SQL fault");
        let error = apply_target(
            &mut db,
            MigrationTarget::Analytics,
            &Accounts::new(),
            SourceDatabase::Canvas(
                &Connection::open(root.path().join("canvas.sqlite3")).expect("Canvas source"),
            ),
        )
        .expect_err("fault after provider transition");
        assert!(matches!(
            error,
            StoreError::Migration {
                phase: MigrationPhase::Apply,
                rollback: MigrationRollbackOutcome::RolledBack,
                ..
            }
        ));
        db.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .expect("remove SQL fault");
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM analytics_meta WHERE key='providerIdentityFormat'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .expect("no partial marker"),
            0
        );
        assert!(
            !crate::schema::table_exists(&db, "analytics_cost_rows").expect("no partial table")
        );
        apply_target(
            &mut db,
            MigrationTarget::Analytics,
            &Accounts::new(),
            SourceDatabase::Canvas(
                &Connection::open(root.path().join("canvas.sqlite3")).expect("Canvas source"),
            ),
        )
        .expect("continue after fault removed");
    }

    #[tokio::test]
    async fn migration_commit_and_rollback_failures_keep_both_typed_causes() {
        use rusqlite::hooks::TransactionOperation;
        let root = tempfile::tempdir().expect("state");
        drop(
            crate::RuntimeStore::open(Some(root.path()))
                .await
                .expect("fresh"),
        );
        let mut db = Connection::open(root.path().join("analytics.sqlite3")).expect("analytics");
        // rusqlite 0.40.2 exposes COMMIT as Unknown; allow only BEGIN here.
        db.authorizer(Some(|context: AuthContext<'_>| {
            if matches!(context.action, AuthAction::Transaction { operation } if !matches!(operation, TransactionOperation::Begin)) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))
        .expect("transaction faults");
        let error = apply_target(
            &mut db,
            MigrationTarget::Analytics,
            &Accounts::new(),
            SourceDatabase::Canvas(
                &Connection::open(root.path().join("canvas.sqlite3")).expect("Canvas source"),
            ),
        )
        .expect_err("commit and rollback denied");
        assert!(
            matches!(&error,StoreError::Migration{phase:MigrationPhase::Rollback,rollback:MigrationRollbackOutcome::Unknown,source,rollback_error:Some(_),..} if matches!(**source,StoreError::Migration{phase:MigrationPhase::Commit,..}))
        );
        db.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .expect("remove faults");
        if !db.is_autocommit() {
            db.execute_batch("ROLLBACK").expect("fixture cleanup");
        }
    }
}
