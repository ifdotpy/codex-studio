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
pub(crate) mod m2026_10_27_cost_index;
mod m2026_10_27_retired_sqlite;
mod m2026_10_27_scheduler_indexes;
mod m2026_10_28_agent_account;
mod m2026_10_28_agent_fields;
mod m2026_10_28_group_revision;
mod m2026_10_28_receipts_account_worker;
mod m2026_10_28_receipts_actions;
mod m2026_10_28_receipts_checks;
mod m2026_10_28_receipts_context_repair;
mod m2026_10_28_receipts_drop_old_tables;
mod m2026_10_28_receipts_execution;
mod m2026_10_28_receipts_incidents;
mod m2026_10_28_receipts_owner_state;
mod m2026_10_28_receipts_provider_callbacks;
mod m2026_10_28_receipts_provider_delivery;
mod m2026_10_28_receipts_remote;
mod m2026_10_28_receipts_requests;
mod m2026_10_28_receipts_schema;
mod m2026_10_28_receipts_title;
mod m2026_10_28_receipts_tools;
mod m2026_10_28_receipts_vm_provider;
mod m2026_10_28_receipts_voice;
mod m2026_10_28_receipts_work;
mod m2026_10_28_usage_revision;
mod m2026_10_29_agent_authorizations;
mod m2026_10_29_model_choices;
mod m2026_10_29_request_results_v2;
mod m2026_10_30_agent_compactions;
mod m2026_10_30_agent_volatile;
mod m2026_10_30_branch_phase;
mod m2026_10_30_capture_errors;
mod m2026_10_30_command_path;
mod m2026_10_30_complaint_record;
mod m2026_10_30_domain_model;
pub(crate) mod m2026_10_30_monitor_errors;
mod m2026_10_30_monitor_generated;
pub(crate) mod m2026_10_30_monitor_queue;
pub(crate) mod m2026_10_30_monitor_status;
mod m2026_10_30_one_nonce_table;
mod m2026_10_30_one_outbox;
mod m2026_10_30_project_teams;
mod m2026_10_30_radio_room;
mod m2026_10_30_request_record;
mod m2026_10_30_retire_graph_agents;
mod m2026_10_30_rule_monitor_records;
mod m2026_10_30_start_wait;
mod m2026_10_30_task_records;
mod m2026_10_30_text_blobs;
mod m2026_10_30_unused_agent_columns;
mod m2026_10_30_unused_start_action;
mod m2026_10_30_vm_stream_owners;
mod m2026_10_31_native_status_error;
mod m2026_10_31_transfer_history_pages;

#[cfg(any(test, feature = "test-support"))]
pub use m2026_10_20_agents::migrate_2026_10_20;
#[cfg(any(test, feature = "test-support"))]
pub use m2026_10_20_agents::prepare_columns;
pub use m2026_10_26_provider_identity::TITLE_FINGERPRINT_PROVIDER_2026_10_26;
#[cfg(any(test, feature = "test-support"))]
pub use m2026_10_26_provider_identity::migrate_canvas_2026_10_26;
#[cfg(any(test, feature = "test-support"))]
pub use m2026_10_30_branch_phase::apply as migrate_branch_phase;
#[cfg(any(test, feature = "test-support"))]
pub use m2026_10_30_project_teams::apply as migrate_project_teams;
#[cfg(any(test, feature = "test-support"))]
pub use m2026_10_30_radio_room::apply as migrate_radio_room;

use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior};
use studio_runtime_schema::studio_errors::{
    MigrationPhase, MigrationRollbackOutcome, MigrationTarget,
};

use crate::receipt_migration_contract::{self, MigrationCheckpoint, ReceiptFamilyMigration};
use crate::{StoreError, StoreResult};
use m2026_10_26_provider_identity::Accounts;
use studio_runtime_schema::persistence::{MigrationStatus, ReceiptMigrationPhase};

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
const OCTOBER_27: MigrationDate = MigrationDate {
    year: 2026,
    month: 10,
    day: 27,
};
const OCTOBER_28: MigrationDate = MigrationDate {
    year: 2026,
    month: 10,
    day: 28,
};
const OCTOBER_29: MigrationDate = MigrationDate {
    year: 2026,
    month: 10,
    day: 29,
};
const OCTOBER_31: MigrationDate = MigrationDate {
    year: 2026,
    month: 10,
    day: 31,
};
const OCTOBER_30: MigrationDate = MigrationDate {
    year: 2026,
    month: 10,
    day: 30,
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
const SUPERVISOR_WRITTEN_ID: &str = "2026-10-29-supervisor-written";
/// Stable migration identifier; includes the introduction date.
const TERMINAL_ERRORS_ID: &str = "2026-10-26-terminal-errors";
const RETIRED_CANVAS_ID: &str = "2026-10-27-retired-canvas";
const RETIRED_ANALYTICS_ID: &str = "2026-10-27-retired-analytics";
const SCHEDULER_INDEXES_ID: &str = "2026-10-27-scheduler-indexes";
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
    fn receipt_family(&self) -> Option<&ReceiptMigration> {
        None
    }
}

/// Catalog adapter for independent receipt families. Only the runner owns commits.
struct ReceiptMigration {
    family: &'static (dyn ReceiptFamilyMigration + Sync),
    sources: &'static [&'static str],
}

impl Migration for ReceiptMigration {
    fn definition(&self) -> MigrationDefinition {
        MigrationDefinition {
            id: self.family.id(),
            introduced: OCTOBER_28,
            target: MigrationTarget::Canvas,
            supported_source: "Classified pre-cutover receipt family, bounded by rowid and input bytes",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        }
    }
    fn apply(&self, _context: &Context<'_, '_>) -> StoreResult<()> {
        Err(StoreError::RecordContract(
            "receipt families require the bounded startup runner",
        ))
    }
    fn receipt_family(&self) -> Option<&ReceiptMigration> {
        Some(self)
    }
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
            id: "2026-10-30-agent-compactions",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Agent records with missing compaction counts",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_agent_compactions::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-start-wait",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Agent records with legacy start waits",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_start_wait::apply(c.transaction),
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
            id: "2026-10-30-unused-agent-columns",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Agent records with unread stored generated columns",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_unused_agent_columns::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-retire-graph-agents",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Legacy registered graph agent rows",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_retire_graph_agents::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-unused-start-action",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Agent records with an unread stored start action",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_unused_start_action::apply(c.transaction),
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
            id: "2026-10-30-monitor-queue-column",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Monitor rows with JSON queue index",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_monitor_queue::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-vm-stream-owner-index",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Agents before the VM stream owner index",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_vm_stream_owners::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-monitor-status-column",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Monitor rows with JSON status indexes",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_monitor_status::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-monitor-error-columns",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Monitor rows with JSON Studio-error index",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_monitor_errors::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-monitor-generated-columns",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Monitor records with writer-maintained index columns",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_monitor_generated::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: CANVAS_COLUMNS_ID,
            introduced: OCTOBER_26,
            target: MigrationTarget::Canvas,
            supported_source: "Terminal output without generation",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| {
            crate::feature_schema::prepare(c.transaction)?;
            m2026_10_26_canvas::feature_columns(c.transaction)?;
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
            id: SCHEDULER_INDEXES_ID,
            introduced: OCTOBER_27,
            target: MigrationTarget::Canvas,
            supported_source: "Canvas without scheduler indexes",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_27_scheduler_indexes::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-28-agent-fields",
            introduced: OCTOBER_28,
            target: MigrationTarget::Canvas,
            supported_source: "Agent execution fields before typed decoding",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_28_agent_fields::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-28-agent-account",
            introduced: OCTOBER_28,
            target: MigrationTarget::Canvas,
            supported_source: "Agent records before account ownership was materialized",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_28_agent_account::apply(c.transaction),
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
            )
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
            id: SUPERVISOR_WRITTEN_ID,
            introduced: OCTOBER_29,
            target: MigrationTarget::Supervisor,
            supported_source: "Supervisor receipts before written evidence",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| crate::supervisor_journal::prepare_written_schema(c.transaction),
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
    &SqlMigration {
        definition: MigrationDefinition {
            id: RETIRED_ANALYTICS_ID,
            introduced: OCTOBER_27,
            target: MigrationTarget::Analytics,
            supported_source: "Retired unused SQLite families",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_27_retired_sqlite::analytics(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: RETIRED_CANVAS_ID,
            introduced: OCTOBER_27,
            target: MigrationTarget::Canvas,
            supported_source: "Retired unused SQLite families",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_27_retired_sqlite::canvas(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-27-cost-group-cover",
            introduced: OCTOBER_27,
            target: MigrationTarget::Analytics,
            supported_source: "Cost groups with the non-covering lookup index",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_27_cost_index::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-28-usage-revision",
            introduced: OCTOBER_28,
            target: MigrationTarget::Analytics,
            supported_source: "Analytics usage without receipt revisions",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_28_usage_revision::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-28-group-revision",
            introduced: OCTOBER_28,
            target: MigrationTarget::Analytics,
            supported_source: "Cost groups without revisions",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_28_group_revision::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-28-receipts-schema",
            introduced: OCTOBER_28,
            target: MigrationTarget::Canvas,
            supported_source: "Receipt tables before per-family cutover",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_28_receipts_schema::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-dismissed-incidents-index",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Receipt incidents without a dismissal index",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| {
            Ok(crate::receipt_schema::install_dismissed_index(
                c.transaction,
            )?)
        },
    },
    &ReceiptMigration {
        family: &m2026_10_28_receipts_execution::ExecutionMigration,
        sources: m2026_10_28_receipts_execution::SOURCES,
    },
    &ReceiptMigration {
        family: &m2026_10_28_receipts_actions::ActionsMigration,
        sources: m2026_10_28_receipts_actions::SOURCES,
    },
    &ReceiptMigration {
        family: &m2026_10_28_receipts_context_repair::ContextRepairMigration,
        sources: m2026_10_28_receipts_context_repair::SOURCES,
    },
    &ReceiptMigration {
        family: &m2026_10_28_receipts_voice::VoiceMigration,
        sources: m2026_10_28_receipts_voice::SOURCES,
    },
    &ReceiptMigration {
        family: &m2026_10_28_receipts_work::Family,
        sources: m2026_10_28_receipts_work::SOURCES,
    },
    &ReceiptMigration {
        family: &m2026_10_28_receipts_title::Family,
        sources: m2026_10_28_receipts_title::SOURCES,
    },
    &ReceiptMigration {
        family: &m2026_10_28_receipts_owner_state::Family,
        sources: m2026_10_28_receipts_owner_state::SOURCES,
    },
    // Tool lineage proves managed location identities before public replay classification.
    &ReceiptMigration {
        family: &m2026_10_28_receipts_tools::Family,
        sources: m2026_10_28_receipts_tools::SOURCES,
    },
    &ReceiptMigration {
        family: &m2026_10_28_receipts_requests::Family,
        sources: m2026_10_28_receipts_requests::SOURCES,
    },
    &ReceiptMigration {
        family: &m2026_10_28_receipts_remote::Family,
        sources: m2026_10_28_receipts_remote::SOURCES,
    },
    &ReceiptMigration {
        family: &m2026_10_28_receipts_provider_delivery::Family,
        sources: m2026_10_28_receipts_provider_delivery::SOURCES,
    },
    &ReceiptMigration {
        family: &m2026_10_28_receipts_provider_callbacks::Family,
        sources: m2026_10_28_receipts_provider_callbacks::SOURCES,
    },
    &ReceiptMigration {
        family: &m2026_10_28_receipts_vm_provider::Family,
        sources: m2026_10_28_receipts_vm_provider::SOURCES,
    },
    &ReceiptMigration {
        family: &m2026_10_28_receipts_account_worker::Family,
        sources: m2026_10_28_receipts_account_worker::SOURCES,
    },
    &ReceiptMigration {
        family: &m2026_10_28_receipts_incidents::Family,
        sources: &["runtime_operation_receipts"],
    },
    &ReceiptMigration {
        family: &m2026_10_28_receipts_checks::Family,
        sources: m2026_10_28_receipts_checks::SOURCES,
    },
    &ReceiptMigration {
        family: &m2026_10_28_receipts_drop_old_tables::Family,
        sources: &[],
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-29-agent-authorizations",
            introduced: OCTOBER_29,
            target: MigrationTarget::Canvas,
            supported_source: "Agent and pending recovery authorization snapshots before typed identities",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_29_agent_authorizations::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-agent-volatile-complete",
            introduced: MigrationDate {
                year: 2026,
                month: 10,
                day: 30,
            },
            target: MigrationTarget::Canvas,
            supported_source: "Agent volatile fields before complete typed writes",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_agent_volatile::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-retired-auth-probe-receipts",
            introduced: MigrationDate {
                year: 2026,
                month: 10,
                day: 30,
            },
            target: MigrationTarget::Canvas,
            supported_source: "Retired Claude auth probe receipts in any status",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_26_canvas::remove_auth_probe_receipts(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-29-model-choices",
            introduced: OCTOBER_29,
            target: MigrationTarget::Canvas,
            supported_source: "Server preferences with the retired exclusive model whitelist",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_29_model_choices::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-29-request-results-v2",
            introduced: OCTOBER_29,
            target: MigrationTarget::Canvas,
            supported_source: "Request results with exact response bodies",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_29_request_results_v2::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-29-annotation-revision",
            introduced: OCTOBER_29,
            target: MigrationTarget::Canvas,
            supported_source: "Annotation rows without a revision",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_29_request_results_v2::annotation_revision(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-29-chat-message-revision",
            introduced: OCTOBER_29,
            target: MigrationTarget::Canvas,
            supported_source: "Chat message rows without a revision",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_29_request_results_v2::message_revision(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-command-path",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Databases without command watermarks and event revisions",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_command_path::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-durable-seq",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Main tables without a change number and tombstones",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| crate::durable_seq::install(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-one-outbox",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Pending rows of the four transport outboxes",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_one_outbox::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-rule-monitor-records",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Rule and monitor records before strict typed state",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_rule_monitor_records::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-unused-monitor-stall-index",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Monitor rows with the unused stall deadline expression index",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| {
            c.transaction
                .execute_batch("DROP INDEX IF EXISTS runtime_monitor_stall_due")?;
            Ok(())
        },
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-capture-errors",
            introduced: OCTOBER_30,
            target: MigrationTarget::Analytics,
            supported_source: "Capture error timestamps stored as decimal strings",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_capture_errors::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-complaint-record",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Complaint records before strict typed state",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_complaint_record::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-request-record",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Request records before strict typed state",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_request_record::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-transcript-read",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Transcript copies in sync_entities and items without a change number entry",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| {
            crate::durable_seq::install(c.transaction)?;
            // Old items are numbered by the online backfill (transcript_seq_backfill),
            // which also runs the copy check and ends the copies. Without items there is
            // nothing to number, so the check runs here. The old rows stay either way.
            let items: bool = c.transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_items)",
                [],
                |row| row.get(0),
            )?;
            if !items {
                crate::durable_seq::retire_transcript_copies(c.transaction)?;
            }
            Ok(())
        },
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-text-blobs",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "State without runtime_item_blobs",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_text_blobs::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-one-nonce-table",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Federation nonces in runtime_federation_nonces",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_one_nonce_table::apply(c.transaction),
    },
    &RegistryMigration,
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-task-records",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Task records before strict typed validation",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_task_records::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-radio-room",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Radio rooms with missing or null direct and missing active fields",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_radio_room::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-project-teams",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Project records without peerTeams",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_project_teams::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-branch-phase",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Branch operations with missing or unclassifiable phases",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_branch_phase::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-31-native-status-error",
            introduced: OCTOBER_31,
            target: MigrationTarget::Canvas,
            supported_source: "Agent native status errors stored as objects",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_31_native_status_error::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-31-transfer-history-pages",
            introduced: OCTOBER_31,
            target: MigrationTarget::Canvas,
            supported_source: "Account transfers with provider read replies stored inline",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_31_transfer_history_pages::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-31-restore-monitor-active-index",
            introduced: OCTOBER_31,
            target: MigrationTarget::Canvas,
            supported_source: "Monitor status migration without the active monitor index",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| crate::StartupRecoveryRepository::prepare_monitor_index(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-30-domain-model",
            introduced: OCTOBER_30,
            target: MigrationTarget::Canvas,
            supported_source: "Agent records that hold chat, run and scheduling state, and the completed-turn table",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_30_domain_model::apply(c.transaction),
    },
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
    apply_target_catalog(db, target, accounts, source_database, CATALOG)
}

fn apply_target_catalog(
    db: &mut Connection,
    target: MigrationTarget,
    accounts: &Accounts,
    source_database: SourceDatabase<'_>,
    catalog: &[&dyn Migration],
) -> StoreResult<()> {
    let marker_table = match target {
        MigrationTarget::Canvas => Some("sync_entity_meta"),
        MigrationTarget::Analytics => Some("analytics_meta"),
        MigrationTarget::Supervisor | MigrationTarget::AccountRegistry => None,
    };
    let fingerprint = crate::sha256_hex(
        catalog
            .iter()
            .filter(|step| step.definition().target == target && step.receipt_family().is_none())
            .map(|step| step.definition().id)
            .collect::<Vec<_>>()
            .join("\0")
            .as_bytes(),
    );
    if let Some(table) = marker_table
        && crate::schema::table_exists(db, table)?
        && db
            .query_row(
                &format!("SELECT value FROM {table} WHERE key='migration_catalog'"),
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .as_deref()
            == Some(catalog_marker(db, &fingerprint)?.as_str())
    {
        return Ok(());
    }
    let first = catalog
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
    for step in catalog
        .iter()
        .filter(|step| step.definition().target == target && step.receipt_family().is_none())
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
        // Receipt families finish the target query columns before final bootstrap validation.
        MigrationTarget::Canvas => Ok(()),
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
    if let Some(table) = marker_table {
        let write_marker = || -> rusqlite::Result<()> {
            let marker = catalog_marker(&tx, &fingerprint)?;
            tx.execute(
                &format!("INSERT INTO {table}(key,value) VALUES('migration_catalog',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value"),
                [&marker],
            )?;
            Ok(())
        };
        if let Err(error) = write_marker() {
            return Err(rolled_back(
                tx,
                last,
                target,
                MigrationPhase::Apply,
                error.into(),
            ));
        }
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

fn catalog_marker(db: &Connection, fingerprint: &str) -> rusqlite::Result<String> {
    let version: i64 = db.pragma_query_value(None, "schema_version", |row| row.get(0))?;
    Ok(format!("{fingerprint}:{version}"))
}

/// Position in the currently migrating source, measured in SQLite rowids.
#[derive(Clone, Debug, serde::Serialize)]
pub struct MigrationProgress {
    pub id: String,
    pub done: i64,
    pub total: i64,
}

/// Run under runtime.lock, before ordinary recovery, workers, readers or readiness.
pub(crate) fn run(
    root: &Path,
    progress: Option<&(dyn Fn(MigrationProgress) + Send + Sync)>,
) -> StoreResult<()> {
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
    run_receipt_migrations(&mut canvas, progress)?;
    RegistryMigration.apply(&Context::Registry(&registry))
}

/// Startup owns the state lock throughout all batches. Recovery and serving
/// connections are created only after this function and final validation return.
fn run_receipt_migrations(
    db: &mut Connection,
    progress: Option<&(dyn Fn(MigrationProgress) + Send + Sync)>,
) -> StoreResult<()> {
    for migration in CATALOG.iter().filter_map(|entry| entry.receipt_family()) {
        run_receipt_family_progress(db, migration, &mut |_, _, _| Ok(()), progress)?;
    }
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    m2026_10_29_request_results_v2::convert_annotations(&tx)?;
    tx.commit()?;
    // The receipt families keep the pending transport rows in their old tables. Move them.
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    m2026_10_30_one_outbox::apply(&tx)?;
    tx.commit()?;
    m2026_10_28_receipts_drop_old_tables::verify_absent(db)
        .map_err(|error| receipt_error(&error))?;
    crate::schema::verify_bootstrap(db)?;
    crate::verify_observed_tables_are_rowid(db)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommitBoundary {
    Before,
    After,
}

fn receipt_error(error: &receipt_migration_contract::ReceiptMigrationError) -> StoreError {
    StoreError::SchemaMismatch(error.to_string())
}

#[cfg(test)]
fn run_receipt_family(
    db: &mut Connection,
    migration: &ReceiptMigration,
    fault: &mut impl FnMut(ReceiptMigrationPhase, CommitBoundary, i64) -> StoreResult<()>,
) -> StoreResult<()> {
    run_receipt_family_progress(db, migration, fault, None)
}

fn run_receipt_family_progress(
    db: &mut Connection,
    migration: &ReceiptMigration,
    fault: &mut impl FnMut(ReceiptMigrationPhase, CommitBoundary, i64) -> StoreResult<()>,
    progress: Option<&(dyn Fn(MigrationProgress) + Send + Sync)>,
) -> StoreResult<()> {
    let id = migration.family.id();
    let now = (m2026_10_26_canvas::unix_now() * 1000.0) as i64;
    let prepare = receipt_migration_contract::phase_id(id, ReceiptMigrationPhase::Prepare);
    let finish = receipt_migration_contract::phase_id(id, ReceiptMigrationPhase::Finish);
    {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let completed = MigrationCheckpoint::load(&tx, &finish, "")
            .map_err(|error| receipt_error(&error))?
            .is_some_and(|checkpoint| checkpoint.status == MigrationStatus::Complete);
        if completed {
            return Ok(());
        }
        let mut checkpoint = MigrationCheckpoint::begin(&tx, &prepare, "", now)
            .map_err(|error| receipt_error(&error))?;
        if checkpoint.status != MigrationStatus::Complete {
            migration
                .family
                .prepare(&tx, checkpoint.cutover_ms)
                .map_err(|error| receipt_error(&error))?;
            checkpoint.status = MigrationStatus::Complete;
            checkpoint
                .save(&tx, &prepare, "", now)
                .map_err(|error| receipt_error(&error))?;
            commit_receipt_phase(
                tx,
                id,
                ReceiptMigrationPhase::Prepare,
                checkpoint.cursor,
                fault,
            )?;
        }
    }
    receipt_checkpoint(db)?;
    for source in migration.sources {
        let total = if crate::schema::table_exists(db, source)? {
            db.query_row(
                &format!("SELECT coalesce(max(rowid),0) FROM \"{source}\""),
                [],
                |row| row.get::<_, i64>(0),
            )?
        } else {
            0
        };
        loop {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let checkpoint = MigrationCheckpoint::begin(&tx, id, source, now)
                .map_err(|error| receipt_error(&error))?;
            if let Some(progress) = progress {
                progress(MigrationProgress {
                    id: format!("{id}:{source}"),
                    done: checkpoint.cursor,
                    total: total.max(checkpoint.cursor),
                });
            }
            if checkpoint.status == MigrationStatus::Complete {
                break;
            }
            let next = match migration.family.move_batch(&tx, source, &checkpoint) {
                Ok(next) => next,
                Err(error) => {
                    return Err(rolled_back(
                        tx,
                        id,
                        MigrationTarget::Canvas,
                        MigrationPhase::Apply,
                        receipt_error(&error),
                    ));
                }
            };
            if next.cutover_ms != checkpoint.cutover_ms
                || next.cursor < checkpoint.cursor
                || (next.cursor == checkpoint.cursor && next.status != MigrationStatus::Complete)
            {
                return Err(rolled_back(
                    tx,
                    id,
                    MigrationTarget::Canvas,
                    MigrationPhase::Validate,
                    StoreError::RecordContract("receipt batch did not advance its checkpoint"),
                ));
            }
            next.save(&tx, id, source, now)
                .map_err(|error| receipt_error(&error))?;
            commit_receipt_phase(tx, id, ReceiptMigrationPhase::Move, next.cursor, fault)?;
            receipt_checkpoint(db)?;
            if next.status == MigrationStatus::Complete {
                break;
            }
        }
    }
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for source in migration.sources {
        if !MigrationCheckpoint::load(&tx, id, source)
            .map_err(|error| receipt_error(&error))?
            .is_some_and(|checkpoint| checkpoint.status == MigrationStatus::Complete)
        {
            return Err(rolled_back(
                tx,
                id,
                MigrationTarget::Canvas,
                MigrationPhase::Validate,
                StoreError::SchemaMismatch(format!(
                    "incomplete receipt migration {id} source {source}"
                )),
            ));
        }
    }
    migration
        .family
        .finish(&tx)
        .map_err(|error| receipt_error(&error))?;
    let mut checkpoint =
        MigrationCheckpoint::begin(&tx, &finish, "", now).map_err(|error| receipt_error(&error))?;
    checkpoint.status = MigrationStatus::Complete;
    checkpoint
        .save(&tx, &finish, "", now)
        .map_err(|error| receipt_error(&error))?;
    commit_receipt_phase(
        tx,
        id,
        ReceiptMigrationPhase::Finish,
        checkpoint.cursor,
        fault,
    )?;
    receipt_checkpoint(db)
}

fn commit_receipt_phase(
    tx: Transaction<'_>,
    id: &'static str,
    phase: ReceiptMigrationPhase,
    cursor: i64,
    fault: &mut impl FnMut(ReceiptMigrationPhase, CommitBoundary, i64) -> StoreResult<()>,
) -> StoreResult<()> {
    if let Err(error) = fault(phase, CommitBoundary::Before, cursor) {
        return Err(rolled_back(
            tx,
            id,
            MigrationTarget::Canvas,
            MigrationPhase::Commit,
            error,
        ));
    }
    if let Err(error) = tx.execute_batch("COMMIT") {
        return Err(if tx.is_autocommit() {
            failure(
                id,
                MigrationTarget::Canvas,
                MigrationPhase::Commit,
                MigrationRollbackOutcome::Unknown,
                error.into(),
            )
        } else {
            rolled_back(
                tx,
                id,
                MigrationTarget::Canvas,
                MigrationPhase::Commit,
                error.into(),
            )
        });
    }
    fault(phase, CommitBoundary::After, cursor)
}

fn receipt_checkpoint(db: &Connection) -> StoreResult<()> {
    let result = crate::sqlite_durability::checkpoint(
        db,
        crate::sqlite_durability::CheckpointMode::Truncate,
    )?;
    if !result.is_complete() {
        return Err(StoreError::SchemaMismatch(
            "receipt migration WAL checkpoint is blocked; startup has not completed".into(),
        ));
    }
    Ok(())
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

#[cfg(test)]
pub(crate) fn m2026_10_30_domain_model_for_tests(tx: &Transaction<'_>) -> StoreResult<()> {
    crate::domain::backfill(tx)
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn prepare_canvas_fixture(transaction: &Transaction<'_>) -> StoreResult<()> {
    let accounts = Accounts::new();
    for step in CATALOG.iter().filter(|step| {
        step.definition().target == MigrationTarget::Canvas && step.receipt_family().is_none()
    }) {
        step.apply(&Context::Sql(SqlContext {
            transaction,
            accounts: &accounts,
            source: SourceDatabase::Independent,
        }))?;
    }
    crate::schema::prepare_receipt_owners(transaction)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn completed_target_records_catalog_on_first_open() {
        let root = tempfile::tempdir().expect("state");
        let store = crate::RuntimeStore::open(Some(root.path()))
            .await
            .expect("first open");
        let db = store.test_reader().expect("reader");
        let count: i64 = db
            .query_row(
                "SELECT count(*) FROM sync_entity_meta WHERE key='migration_catalog'",
                [],
                |row| row.get(0),
            )
            .expect("catalog marker");
        assert_eq!(count, 1);
        drop(db);
        drop(store);
        drop(
            crate::RuntimeStore::open(Some(root.path()))
                .await
                .expect("reopen"),
        );
    }

    #[test]
    fn catalog_change_runs_new_body_and_failure_rolls_back_marker() {
        fn first(c: &SqlContext<'_, '_>) -> StoreResult<()> {
            c.transaction
                .execute("UPDATE counts SET first=first+1", [])?;
            Ok(())
        }
        fn second(c: &SqlContext<'_, '_>) -> StoreResult<()> {
            c.transaction
                .execute("UPDATE counts SET second=second+1", [])?;
            Ok(())
        }
        fn fail(c: &SqlContext<'_, '_>) -> StoreResult<()> {
            c.transaction
                .execute("UPDATE counts SET second=second+1", [])?;
            Err(StoreError::RecordContract("injected failure"))
        }
        let definition = |id| MigrationDefinition {
            id,
            introduced: OCTOBER_30,
            target: MigrationTarget::Analytics,
            supported_source: "test",
            remove_when: "test",
        };
        let one = SqlMigration {
            definition: definition("one"),
            apply: first,
        };
        let two = SqlMigration {
            definition: definition("two"),
            apply: second,
        };
        let bad = SqlMigration {
            definition: definition("bad"),
            apply: fail,
        };
        let mut db = Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE analytics_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL); CREATE TABLE counts(first INTEGER,second INTEGER); INSERT INTO counts VALUES(0,0)").expect("fixture");
        let accounts = Accounts::new();
        let run = |db: &mut Connection, steps: &[&dyn Migration]| {
            apply_target_catalog(
                db,
                MigrationTarget::Analytics,
                &accounts,
                SourceDatabase::Independent,
                steps,
            )
        };
        run(&mut db, &[&one]).expect("first run");
        run(&mut db, &[&one]).expect("reopen skips bodies");
        assert_eq!(
            db.query_row("SELECT first FROM counts", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        run(&mut db, &[&one, &two]).expect("appended migration");
        run(&mut db, &[&one, &two]).expect("appended migration skips on reopen");
        assert_eq!(
            db.query_row("SELECT first,second FROM counts", [], |row| Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?
            )))
            .unwrap(),
            (2, 1)
        );
        assert!(run(&mut db, &[&one, &two, &bad]).is_err());
        assert_eq!(
            db.query_row("SELECT first,second FROM counts", [], |row| Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?
            )))
            .unwrap(),
            (2, 1)
        );
        run(&mut db, &[&one, &two]).expect("old marker survived rollback");
        assert_eq!(
            db.query_row("SELECT first,second FROM counts", [], |row| Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?
            )))
            .unwrap(),
            (2, 1)
        );
        db.execute_batch("CREATE TABLE schema_drift(id INTEGER)")
            .expect("changed schema");
        run(&mut db, &[&one, &two]).expect("schema change reruns target");
        run(&mut db, &[&one, &two]).expect("unchanged schema skips target");
        assert_eq!(
            db.query_row("SELECT first,second FROM counts", [], |row| Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?
            )))
            .unwrap(),
            (3, 2)
        );
    }

    #[test]
    fn vm_stream_owner_scan_uses_partial_index() {
        let db = crate::fresh_state::test_canvas();
        let detail: String = db.query_row(
            "EXPLAIN QUERY PLAN SELECT id FROM runtime_agents INDEXED BY runtime_agent_vm_stream_owner WHERE account_key=?1 AND execution_mode='vm' AND hold_status='clear'",
            ["default"], |row| row.get(3),
        ).unwrap();
        assert!(detail.contains("runtime_agent_vm_stream_owner"), "{detail}");
        for (id, mode, account) in [
            ("vm", "vm", "default"),
            ("host", "host", "default"),
            ("other", "vm", "other"),
        ] {
            let record = serde_json::json!({"id":id,"rootId":id,"provider":"codex","epoch":1,"status":"idle","accountKey":account,"executionMode":mode});
            db.execute(
                "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                rusqlite::params![id, record.to_string()],
            )
            .unwrap();
        }
        let owners = || {
            db.prepare("SELECT id FROM runtime_agents WHERE account_key=?1 AND execution_mode='vm' AND hold_status='clear' ORDER BY id").unwrap().query_map(["default"], |row| row.get::<_,String>(0)).unwrap().collect::<Result<Vec<_>,_>>().unwrap()
        };
        assert_eq!(owners(), ["vm"]);
        db.execute(
            "UPDATE runtime_agents SET hold_status='invalid' WHERE id='vm'",
            [],
        )
        .unwrap();
        assert!(owners().is_empty());
        db.execute(
            "UPDATE runtime_agents SET hold_status='clear' WHERE id='vm'",
            [],
        )
        .unwrap();
        assert_eq!(owners(), ["vm"]);
    }
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    #[test]
    fn dismissal_index_migration_preserves_incidents() {
        let mut db = Connection::open_in_memory().expect("database");
        crate::receipt_schema::create(&db).expect("schema");
        db.execute_batch("DROP INDEX runtime_error_incidents_dismissed; INSERT INTO runtime_error_incidents(id,kind,owner_kind,owner_id,created,last_seen,dismissed_at,payload) VALUES('dismissed','http_failure','request','owner',1,1,2,'null'),('open','http_failure','request','owner',1,1,NULL,'null');").expect("old incidents");
        let tx = db.transaction().expect("transaction");
        let accounts = Accounts::new();
        let step = CATALOG
            .iter()
            .find(|step| step.definition().id == "2026-10-30-dismissed-incidents-index")
            .expect("registered migration");
        let context = Context::Sql(SqlContext {
            transaction: &tx,
            accounts: &accounts,
            source: SourceDatabase::Independent,
        });
        step.apply(&context).expect("migration");
        step.apply(&context).expect("repeat migration");
        let rows: i64 = tx
            .query_row("SELECT count(*) FROM runtime_error_incidents", [], |row| {
                row.get(0)
            })
            .expect("rows");
        assert_eq!(rows, 2);
        let plan: String = tx.query_row("EXPLAIN QUERY PLAN SELECT id,dismissed_at FROM runtime_error_incidents WHERE dismissed_at IS NOT NULL", [], |row| row.get(3)).expect("plan");
        assert!(plan.contains("runtime_error_incidents_dismissed"), "{plan}");
        tx.commit().expect("commit");
    }

    #[test]
    fn migration_removes_unused_monitor_stall_index() {
        let mut db = Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL); CREATE INDEX runtime_monitor_stall_due ON runtime_monitors(json_extract(record,'$.activityAt')) WHERE json_extract(record,'$.status')='running'; INSERT INTO runtime_monitors(id,record) VALUES('m','{}');").expect("legacy monitor");
        let tx = db.transaction().expect("transaction");
        let accounts = Accounts::new();
        let step = CATALOG
            .iter()
            .find(|step| step.definition().id == "2026-10-30-unused-monitor-stall-index")
            .expect("registered migration");
        step.apply(&Context::Sql(SqlContext {
            transaction: &tx,
            accounts: &accounts,
            source: SourceDatabase::Independent,
        }))
        .expect("migration");
        assert!(
            !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='runtime_monitor_stall_due')",
                [],
                |row| row.get::<_, bool>(0)
            )
            .expect("index")
        );
        assert_eq!(
            tx.query_row(
                "SELECT record FROM runtime_monitors WHERE id='m'",
                [],
                |row| row.get::<_, String>(0)
            )
            .expect("row"),
            "{}"
        );
    }

    struct BatchWitness;
    impl ReceiptFamilyMigration for BatchWitness {
        fn id(&self) -> &'static str {
            "2026-10-28-receipts-batch-witness"
        }
        fn prepare(&self, _: &Transaction<'_>, _: i64) -> receipt_migration_contract::Result<()> {
            Ok(())
        }
        fn move_batch(
            &self,
            tx: &Transaction<'_>,
            source: &str,
            checkpoint: &MigrationCheckpoint,
        ) -> receipt_migration_contract::Result<MigrationCheckpoint> {
            #[derive(serde::Deserialize, serde::Serialize)]
            struct Reply {
                index: i64,
                #[serde(default, skip_serializing_if = "Option::is_none")]
                padding: Option<String>,
            }
            let rows = receipt_migration_contract::read_receipt_batch(tx, source, checkpoint, "1")?;
            let mut next = checkpoint.clone();
            next.status = if rows.is_empty() {
                MigrationStatus::Complete
            } else {
                MigrationStatus::Running
            };
            for row in rows {
                if row.signature != "known" {
                    return Err(receipt_migration_contract::unclassifiable(
                        source,
                        &row.id,
                        &row.signature,
                    ));
                }
                let reply: Reply = row.json(source)?;
                receipt_migration_contract::import_result(
                    tx,
                    &row.id,
                    &row.signature,
                    &crate::request_results::ResultEnvelope {
                        actor: None,
                        reply,
                        references: vec![],
                    },
                    next.cutover_ms,
                )?;
                receipt_migration_contract::delete_source(tx, source, row.rowid)?;
                next.cursor = row.rowid;
            }
            Ok(next)
        }
        fn finish(&self, _: &Transaction<'_>) -> receipt_migration_contract::Result<()> {
            Ok(())
        }
    }

    fn receipt_batch_fixture(path: &Path, count: i64) -> Connection {
        let mut db = Connection::open(path).expect("database");
        crate::sqlite_durability::configure_writer(&db).expect("WAL");
        let tx = db.transaction().expect("fixture transaction");
        receipt_migration_contract::prepare_schema(&tx).expect("targets");
        tx.execute_batch("CREATE TABLE IF NOT EXISTS runtime_orchestration_settings(id TEXT PRIMARY KEY,record TEXT NOT NULL); CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT,result TEXT)").expect("source");
        for index in 1..=count {
            tx.execute(
                "INSERT INTO runtime_operation_receipts VALUES(?1,'known',?2)",
                rusqlite::params![
                    format!("request-{index}"),
                    serde_json::json!({"index":index}).to_string()
                ],
            )
            .expect("old row");
        }
        tx.commit().expect("fixture committed");
        db
    }

    #[test]
    fn receipt_cutover_batch_crashes_preserve_evidence_and_resume_exactly() {
        let family = ReceiptMigration {
            family: &BatchWitness,
            sources: &["runtime_operation_receipts"],
        };
        // Prepare, four move commits (500/500/1/empty), finish: before and after each.
        for crash in 0..12 {
            let root = tempfile::tempdir().expect("fixture");
            let path = root.path().join("canvas.sqlite3");
            let mut db = receipt_batch_fixture(&path, 1001);
            let mut boundary = 0;
            let result = run_receipt_family(&mut db, &family, &mut |_, _, _| {
                let at = boundary;
                boundary += 1;
                if at == crash {
                    Err(StoreError::RecordContract("injected process interruption"))
                } else {
                    Ok(())
                }
            });
            assert!(result.is_err(), "boundary {crash}");
            let evidence:i64=db.query_row("SELECT (SELECT count(*) FROM runtime_operation_receipts)+(SELECT count(*) FROM request_results)",[],|row|row.get(0)).expect("evidence");
            assert_eq!(
                evidence, 1001,
                "source deletion never outruns destination at {crash}"
            );
            drop(db);
            let mut db = Connection::open(&path).expect("restart");
            crate::sqlite_durability::configure_writer(&db).expect("WAL");
            run_receipt_family(&mut db, &family, &mut |_, _, _| Ok(())).expect("resume");
            let results: Vec<(String, String, String, i64)> = db
                .prepare("SELECT id,signature,result,created FROM request_results ORDER BY id")
                .expect("results")
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .expect("rows")
                .collect::<rusqlite::Result<_>>()
                .expect("read");
            assert_eq!(results.len(), 1001);
            assert!(results.iter().all(|row| row.3 == results[0].3));
            let drop_migration = ReceiptMigration {
                family: &m2026_10_28_receipts_drop_old_tables::Family,
                sources: &[],
            };
            run_receipt_family(&mut db, &drop_migration, &mut |_, _, _| Ok(()))
                .expect("drop old sources");
            drop(db);
            let mut db = Connection::open(&path).expect("second open");
            run_receipt_family(&mut db, &family, &mut |_, _, _| {
                panic!("completed family must not execute again")
            })
            .expect("completed");
            let after: Vec<(String, String, String, i64)> = db
                .prepare("SELECT id,signature,result,created FROM request_results ORDER BY id")
                .expect("results")
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .expect("rows")
                .collect::<rusqlite::Result<_>>()
                .expect("read");
            assert_eq!(after, results);
            assert!(
                !crate::schema::table_exists(&db, "runtime_operation_receipts").expect("schema")
            );
        }
    }

    #[test]
    fn receipt_progress_reports_committed_cursor() {
        let root = tempfile::tempdir().expect("fixture");
        let mut db = receipt_batch_fixture(&root.path().join("canvas.sqlite3"), 1001);
        let family = ReceiptMigration {
            family: &BatchWitness,
            sources: &["runtime_operation_receipts"],
        };
        let positions = std::sync::Mutex::new(Vec::new());
        let result = run_receipt_family_progress(
            &mut db,
            &family,
            &mut |phase, boundary, cursor| {
                if phase == ReceiptMigrationPhase::Move
                    && boundary == CommitBoundary::Before
                    && cursor > 500
                {
                    return Err(StoreError::RecordContract("interrupt second batch"));
                }
                Ok(())
            },
            Some(&|progress| positions.lock().expect("positions").push(progress)),
        );
        assert!(result.is_err());
        let positions = positions.lock().expect("positions");
        assert_eq!(positions.len(), 2);
        assert_eq!(positions[0].done, 0);
        assert_eq!(positions[1].done, 500);
        assert_eq!(positions[1].total, 1001);
        assert_eq!(
            positions[1].id,
            "2026-10-28-receipts-batch-witness:runtime_operation_receipts"
        );
    }

    #[test]
    fn receipt_cutover_drop_crashes_are_atomic_and_never_accept_leftover_rows() {
        for boundary in [CommitBoundary::Before, CommitBoundary::After] {
            let root = tempfile::tempdir().expect("fixture");
            let path = root.path().join("canvas.sqlite3");
            let mut db = receipt_batch_fixture(&path, 0);
            let migration = ReceiptMigration {
                family: &m2026_10_28_receipts_drop_old_tables::Family,
                sources: &[],
            };
            assert!(
                run_receipt_family(&mut db, &migration, &mut |phase, at, _| {
                    if phase == ReceiptMigrationPhase::Finish && at == boundary {
                        Err(StoreError::RecordContract("drop interrupted"))
                    } else {
                        Ok(())
                    }
                })
                .is_err()
            );
            assert_eq!(
                crate::schema::table_exists(&db, "runtime_operation_receipts").expect("schema"),
                boundary == CommitBoundary::Before
            );
            drop(db);
            let mut db = Connection::open(&path).expect("restart");
            run_receipt_family(&mut db, &migration, &mut |_, _, _| Ok(())).expect("finish drop");
            assert!(
                !crate::schema::table_exists(&db, "runtime_operation_receipts").expect("schema")
            );
        }
        let root = tempfile::tempdir().expect("fixture");
        let mut db = receipt_batch_fixture(&root.path().join("canvas.sqlite3"), 1);
        let migration = ReceiptMigration {
            family: &m2026_10_28_receipts_drop_old_tables::Family,
            sources: &[],
        };
        let error = run_receipt_family(&mut db, &migration, &mut |_, _, _| Ok(()))
            .expect_err("leftover refuses drop");
        let error = error.to_string();
        assert!(
            error.contains("runtime_operation_receipts")
                && error.contains("request-1")
                && error.contains("known"),
            "{error}"
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM runtime_operation_receipts",
                [],
                |row| row.get::<_, i64>(0)
            )
            .expect("source"),
            1
        );
    }

    #[test]
    fn receipt_cutover_low_space_preserves_oversized_source_and_resumes() {
        let root = tempfile::tempdir().expect("fixture");
        let mut db = receipt_batch_fixture(&root.path().join("canvas.sqlite3"), 1);
        let oversized=serde_json::json!({"index":1,"padding":"x".repeat(receipt_migration_contract::MIGRATION_BATCH_BYTES+1)}).to_string();
        db.execute(
            "UPDATE runtime_operation_receipts SET result=?1",
            [&oversized],
        )
        .expect("oversized source");
        let pages: i64 = db
            .pragma_query_value(None, "page_count", |row| row.get(0))
            .expect("pages");
        db.pragma_update(None, "max_page_count", pages)
            .expect("finite disk budget");
        let family = ReceiptMigration {
            family: &BatchWitness,
            sources: &["runtime_operation_receipts"],
        };
        let error = run_receipt_family(&mut db, &family, &mut |_, _, _| Ok(()))
            .expect_err("destination cannot fit");
        assert!(format!("{error:?}").contains("full"), "{error:?}");
        let source: String = db
            .query_row("SELECT result FROM runtime_operation_receipts", [], |row| {
                row.get(0)
            })
            .expect("source survives");
        assert_eq!(source, oversized);
        assert_eq!(
            db.query_row("SELECT count(*) FROM request_results", [], |row| row
                .get::<_, i64>(0))
                .expect("results"),
            0
        );
        db.pragma_update(None, "max_page_count", pages * 4)
            .expect("space restored");
        run_receipt_family(&mut db, &family, &mut |_, _, _| Ok(()))
            .expect("one oversized row resumes");
        let result: String = db
            .query_row("SELECT result FROM request_results", [], |row| row.get(0))
            .expect("exact result");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&result).expect("envelope")["reply"],
            serde_json::from_str::<serde_json::Value>(&oversized).expect("source")
        );
    }

    #[test]
    fn receipt_cutover_startup_error_names_unclassifiable_row_in_display() {
        let error = failure(
            "2026-10-28-receipts-batch-witness",
            MigrationTarget::Canvas,
            MigrationPhase::Apply,
            MigrationRollbackOutcome::RolledBack,
            receipt_error(&receipt_migration_contract::unclassifiable(
                "source-table",
                "source-key",
                "source-signature",
            )),
        );
        let shown = error.to_string();
        for expected in ["source-table", "source-key", "source-signature"] {
            assert!(
                shown.contains(expected),
                "operator startup error must name {expected}: {shown}"
            );
        }
    }

    #[tokio::test]
    async fn retired_sqlite_drop_fault_rolls_back_each_target() {
        let root = tempfile::tempdir().expect("state");
        drop(
            crate::RuntimeStore::open(Some(root.path()))
                .await
                .expect("fresh"),
        );
        for (file, target, setup, denied, restored) in [
            (
                "canvas.sqlite3",
                MigrationTarget::Canvas,
                "CREATE TABLE runtime_turn_item_links_v1(id TEXT); CREATE TABLE runtime_budget_usage(id TEXT); INSERT INTO runtime_turn_item_links_v1 VALUES('old'); CREATE TRIGGER runtime_turn_item_link_delete_v1 AFTER DELETE ON runtime_items BEGIN DELETE FROM runtime_turn_item_links_v1; END;",
                "runtime_budget_usage",
                "runtime_turn_item_links_v1",
            ),
            (
                "analytics.sqlite3",
                MigrationTarget::Analytics,
                "CREATE TABLE analytics_cost_changes_v1(id TEXT); CREATE TABLE analytics_cost_roots_v1(id TEXT); INSERT INTO analytics_cost_changes_v1 VALUES('old');",
                "analytics_cost_roots_v1",
                "analytics_cost_changes_v1",
            ),
        ] {
            let mut db = Connection::open(root.path().join(file)).expect("database");
            db.execute_batch(setup).expect("old objects");
            let marker_table = if target == MigrationTarget::Canvas {
                "sync_entity_meta"
            } else {
                "analytics_meta"
            };
            db.execute(
                &format!("DELETE FROM {marker_table} WHERE key='migration_catalog'"),
                [],
            )
            .expect("old catalog");
            db.authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(context.action, AuthAction::DropTable { table_name } if table_name == denied) {
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            })).expect("fault");
            let source =
                Connection::open(root.path().join("canvas.sqlite3")).expect("canvas source");
            let error = apply_target(
                &mut db,
                target,
                &Accounts::new(),
                SourceDatabase::Canvas(&source),
            )
            .expect_err("drop fails");
            assert!(matches!(
                error,
                StoreError::Migration {
                    phase: MigrationPhase::Apply,
                    rollback: MigrationRollbackOutcome::RolledBack,
                    ..
                }
            ));
            db.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                .expect("remove fault");
            let count: i64 = db
                .query_row(&format!("SELECT count(*) FROM {restored}"), [], |row| {
                    row.get(0)
                })
                .expect("retired rows restored");
            assert_eq!(count, 1);
            if target == MigrationTarget::Canvas {
                let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='runtime_turn_item_link_delete_v1')", [], |row| row.get(0)).expect("trigger restored");
                assert!(exists);
            }
        }
    }

    #[test]
    fn migration_catalog_has_real_unique_dates_sources_and_removal_rules() {
        let order = catalog().map(|entry| entry.id).collect::<Vec<_>>();
        let position = |id| {
            order
                .iter()
                .position(|candidate| *candidate == id)
                .expect("registered family")
        };
        assert!(position("2026-10-28-receipts-tools") < position("2026-10-28-receipts-requests"));
        assert!(position("2026-10-28-receipts-requests") < position("2026-10-28-receipts-remote"));
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
        db.execute_batch("DELETE FROM analytics_meta WHERE key IN ('providerIdentityFormat','migration_catalog'); DROP TABLE analytics_cost_rows; DROP TABLE analytics_cost_groups;").expect("source");
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
        db.execute(
            "DELETE FROM analytics_meta WHERE key='migration_catalog'",
            [],
        )
        .expect("old catalog");
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

#[cfg(test)]
mod monitor_active_index_regression {
    use super::*;

    #[test]
    fn migration_restores_missing_active_monitor_index() {
        let mut db = Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT,monitor_status TEXT); INSERT INTO runtime_monitors VALUES('m','{}','running');").expect("old schema");
        let tx = db.transaction().expect("transaction");
        let accounts = Accounts::new();
        let step = CATALOG
            .iter()
            .find(|step| step.definition().id == "2026-10-31-restore-monitor-active-index")
            .expect("migration");
        let context = Context::Sql(SqlContext {
            transaction: &tx,
            accounts: &accounts,
            source: SourceDatabase::Independent,
        });
        step.apply(&context).expect("restore index");
        step.apply(&context).expect("repeat migration");
        let plan: String = tx.query_row("EXPLAIN QUERY PLAN SELECT id FROM runtime_monitors WHERE monitor_status IN ('running','starting','approval')", [], |row| row.get(3)).expect("plan");
        assert!(plan.contains("runtime_monitor_startup_active"), "{plan}");
    }
}
