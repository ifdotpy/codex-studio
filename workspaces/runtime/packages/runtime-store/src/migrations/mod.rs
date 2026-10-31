//! Server-owned format transitions. Each database target commits once; a failed
//! target rolls back completely. Earlier completed targets stay committed, and
//! restart continues from their existing format markers. No committed downgrade
//! or cross-file atomicity is promised. The caller holds its existing state lock.

pub(crate) mod m2026_10_20_agents;
pub(crate) mod m2026_10_22_analytics;
pub(crate) mod m2026_10_26_canvas;
mod m2026_10_26_provider_identity;
mod m2026_10_26_registry;
pub(crate) mod m2026_10_27_cost_index;
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
mod m2026_10_31_project_record_kind;
mod m2026_10_31_retired_start_indexes;
mod m2026_10_31_transfer_history_pages;

#[cfg(any(test, feature = "test-support"))]
pub use m2026_10_20_agents::migrate_2026_10_20;
#[cfg(any(test, feature = "test-support"))]
pub use m2026_10_20_agents::prepare_columns;
pub use m2026_10_26_provider_identity::TITLE_FINGERPRINT_PROVIDER_2026_10_26;
#[cfg(any(test, feature = "test-support"))]
pub use m2026_10_26_provider_identity::migrate_canvas_2026_10_26;
pub(crate) use m2026_10_26_provider_identity::{
    ANALYTICS_PROVIDER_FORMAT_KEY, PROVIDER_IDENTITY_FORMAT,
};
#[cfg(any(test, feature = "test-support"))]
pub use m2026_10_30_branch_phase::apply as migrate_branch_phase;
#[cfg(any(test, feature = "test-support"))]
pub use m2026_10_30_project_teams::apply as migrate_project_teams;
#[cfg(any(test, feature = "test-support"))]
pub use m2026_10_30_radio_room::apply as migrate_radio_room;
#[cfg(any(test, feature = "test-support"))]
pub use m2026_10_31_project_record_kind::apply as migrate_project_record_kind;

use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior};
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
/// Stable migration identifier; includes the introduction date.
const ANALYTICS_PROJECTION_ID: &str = "2026-10-22-analytics-projection";
const ANALYTICS_PROVIDER_ID: &str = "2026-10-26-analytics-provider";
const CANVAS_PROVIDER_ID: &str = "2026-10-26-canvas-provider";
/// Stable migration identifier; includes the introduction date.
const AGENT_STORAGE_ID: &str = "2026-10-20-agent-storage";
/// Stable migration identifier; includes the introduction date.
const WORKSPACE_LIMITS_ID: &str = "2026-10-26-workspace-limits";
/// Stable migration identifier; includes the introduction date.
const RETIRED_WAITS_ID: &str = "2026-10-26-retired-waits";
/// Stable migration identifier; includes the introduction date.
const SUPERVISOR_ID: &str = "2026-10-26-supervisor-journal";
const SUPERVISOR_WRITTEN_ID: &str = "2026-10-29-supervisor-written";
/// Stable migration identifier; includes the introduction date.
const REMOVE_AFTER_SOURCE_RETIRED: &str = "Supported installations and restorable backups no longer admit the source format; pending historical effects are converted or terminal";

struct SqlContext<'a, 'db> {
    transaction: &'a Transaction<'db>,
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
    id: &'static str,
}

impl Migration for ReceiptMigration {
    fn definition(&self) -> MigrationDefinition {
        MigrationDefinition {
            id: self.id,
            introduced: OCTOBER_28,
            target: MigrationTarget::Canvas,
            supported_source: "Completed historical receipt family",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        }
    }
    fn apply(&self, _context: &Context<'_, '_>) -> StoreResult<()> {
        Ok(())
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
            id: "2026-10-31-retired-start-indexes",
            introduced: OCTOBER_31,
            target: MigrationTarget::Canvas,
            supported_source: "Agent indexes referencing retired start-attempt columns",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_31_retired_start_indexes::apply(c.transaction),
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
            id: "2026-10-28-receipts-schema",
            introduced: OCTOBER_28,
            target: MigrationTarget::Canvas,
            supported_source: "Receipt tables before per-family cutover",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| {
            crate::receipt_migration_contract::prepare_schema(c.transaction)
                .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
            crate::schema::prepare_receipt_holds(c.transaction)
        },
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
        id: "2026-10-28-receipts-execution",
    },
    &ReceiptMigration {
        id: "2026-10-28-receipts-actions",
    },
    &ReceiptMigration {
        id: "2026-10-28-receipts-context-repair",
    },
    &ReceiptMigration {
        id: "2026-10-28-receipts-voice",
    },
    &ReceiptMigration {
        id: "2026-10-28-receipts-work",
    },
    &ReceiptMigration {
        id: "2026-10-28-receipts-title",
    },
    &ReceiptMigration {
        id: "2026-10-28-receipts-owner-state",
    },
    &ReceiptMigration {
        id: "2026-10-28-receipts-tools",
    },
    &ReceiptMigration {
        id: "2026-10-28-receipts-requests",
    },
    &ReceiptMigration {
        id: "2026-10-28-receipts-remote",
    },
    &ReceiptMigration {
        id: "2026-10-28-receipts-provider-delivery",
    },
    &ReceiptMigration {
        id: "2026-10-28-receipts-provider-callbacks",
    },
    &ReceiptMigration {
        id: "2026-10-28-receipts-vm-provider",
    },
    &ReceiptMigration {
        id: "2026-10-28-receipts-account-worker",
    },
    &ReceiptMigration {
        id: "2026-10-28-receipts-incidents",
    },
    &ReceiptMigration {
        id: "2026-10-28-receipts-checks",
    },
    &ReceiptMigration {
        id: "2026-10-28-receipts-drop-old-tables",
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
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-31-restore-startup-agent-indexes",
            introduced: OCTOBER_31,
            target: MigrationTarget::Canvas,
            supported_source: "Retired agent indexes with completed receipt migration families",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| crate::StartupRecoveryRepository::prepare_agent_indexes(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-31-project-record-kind",
            introduced: OCTOBER_31,
            target: MigrationTarget::Canvas,
            supported_source: "Project live records and removal tombstones without a discriminator",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| m2026_10_31_project_record_kind::apply(c.transaction),
    },
    &SqlMigration {
        definition: MigrationDefinition {
            id: "2026-10-31-analytics-revision-schema",
            introduced: OCTOBER_31,
            target: MigrationTarget::Analytics,
            supported_source: "Completed stores without analytics revision tracking",
            remove_when: REMOVE_AFTER_SOURCE_RETIRED,
        },
        apply: |c| crate::fresh_state::prepare_analytics_revision_schema(c.transaction),
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
    Ok(())
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

fn apply_target(db: &mut Connection, target: MigrationTarget) -> StoreResult<()> {
    apply_target_catalog(db, target, CATALOG)
}

fn apply_target_catalog(
    db: &mut Connection,
    target: MigrationTarget,
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
    if target == MigrationTarget::Canvas
        && let Err(source) = crate::bootstrap::prepare_sync_schema(&tx)
    {
        return Err(rolled_back(tx, last, target, MigrationPhase::Apply, source));
    }
    for step in catalog
        .iter()
        .filter(|step| step.definition().target == target && step.receipt_family().is_none())
    {
        last = step.definition().id;
        if let Err(source) = step.apply(&Context::Sql(SqlContext { transaction: &tx })) {
            return Err(rolled_back(tx, last, target, MigrationPhase::Apply, source));
        }
    }
    let current_schema = match target {
        MigrationTarget::Canvas => (|| {
            crate::bootstrap::prepare_runtime_event_indexes(&tx)?;
            crate::AgentRepository::prepare_change_journal(&tx)?;
            crate::feature_schema::prepare(&tx)?;
            crate::MonitorsRepository::prepare_indexes(&tx)?;
            crate::AgentIndexedReads::prepare_index(&tx)
        })(),
        MigrationTarget::Analytics => (|| {
            crate::schema::ensure_analytics_limits_index(&tx)?;
            crate::repositories::cost_aggregate::install(&tx)
        })(),
        MigrationTarget::Supervisor | MigrationTarget::AccountRegistry => Ok(()),
    };
    if let Err(source) = current_schema {
        return Err(rolled_back(tx, last, target, MigrationPhase::Apply, source));
    }
    if target == MigrationTarget::Canvas {
        let seeded: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_payload_migrations WHERE name=?1)",
            [receipt_finish_name("2026-10-28-receipts-execution")],
            |row| row.get(0),
        )?;
        if !seeded {
            seed_receipt_finish(&tx)?;
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

fn receipt_finish_name(id: &str) -> String {
    let finish = format!("{id}-finish");
    format!("receipts:{}:{finish}:", finish.len())
}

pub(crate) fn verify_receipt_finish(path: &Path, analytics_path: &Path) -> StoreResult<()> {
    use rusqlite::OptionalExtension;
    let db = crate::open_database(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let has_checkpoints: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='runtime_payload_migrations')", [], |row| row.get(0))?;
    if !has_checkpoints {
        for table in [
            "runtime_operation_receipts",
            "runtime_native_action_receipts",
            "runtime_context_terminal_receipts",
            "user_terminal_receipts",
            "runtime_access_receipts",
            "runtime_tool_results",
            "runtime_tool_request_aliases",
        ] {
            let exists: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
                [table],
                |row| row.get(0),
            )?;
            if exists {
                return Err(StoreError::IntermediateReleaseRequired);
            }
        }
        for table in [
            "runtime_agents",
            "runtime_items",
            "messages",
            "runtime_chat_messages",
        ] {
            let exists: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
                [table],
                |row| row.get(0),
            )?;
            if exists
                && db.query_row(
                    &format!("SELECT EXISTS(SELECT 1 FROM {table} LIMIT 1)"),
                    [],
                    |row| row.get::<_, bool>(0),
                )?
            {
                return Err(StoreError::IntermediateReleaseRequired);
            }
        }
        let analytics = crate::open_database(analytics_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut stmt = analytics.prepare(
            "SELECT name FROM sqlite_schema WHERE type='table' AND name LIKE 'analytics_%' AND name!='analytics_meta'",
        )?;
        let tables = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for table in tables {
            let quoted = table.replace('"', "\"\"");
            if analytics.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM \"{quoted}\" LIMIT 1)"),
                [],
                |row| row.get::<_, bool>(0),
            )? {
                return Err(StoreError::IntermediateReleaseRequired);
            }
        }
        return Ok(());
    }
    for family in CATALOG.iter().filter_map(|entry| entry.receipt_family()) {
        let status: Option<String> = db
            .query_row(
                "SELECT status FROM runtime_payload_migrations WHERE name=?1",
                [receipt_finish_name(family.id)],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| StoreError::IntermediateReleaseRequired)?;
        if status.as_deref() != Some("complete") {
            return Err(StoreError::IntermediateReleaseRequired);
        }
    }
    Ok(())
}

fn seed_receipt_finish(tx: &Transaction<'_>) -> StoreResult<()> {
    crate::receipt_migration_contract::prepare_schema(tx)
        .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
    crate::schema::prepare_receipt_owners(tx)?;
    tx.execute_batch("ALTER TABLE runtime_server_inbox ADD COLUMN admission TEXT; ALTER TABLE runtime_server_outbox ADD COLUMN admission TEXT; ALTER TABLE runtime_server_fetch ADD COLUMN admission TEXT; ALTER TABLE voice_sessions ADD COLUMN provider_scope TEXT;")?;
    use studio_runtime_schema::persistence::{
        AccessOutboundState, EventKind, EventStatus, ExecutionEffectKind, FederationAckState,
        FederationDeliveryState, FederationMessageKind, MigrationStatus, ServerDeliveryState,
        ServerFetchState, ServerOperationKind, enum_check,
    };
    for (table, checks) in [
        (
            "runtime_events",
            format!(
                "{} {}",
                enum_check::<EventKind>("kind"),
                enum_check::<EventStatus>("status")
            ),
        ),
        (
            "runtime_execution_effects",
            enum_check::<ExecutionEffectKind>("kind"),
        ),
        (
            "runtime_payload_migrations",
            enum_check::<MigrationStatus>("status"),
        ),
    ] {
        tx.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN receipt_enum_guard INTEGER GENERATED ALWAYS AS (NULL) VIRTUAL {checks}"))?;
    }
    for (table, name, expression, check, nullable) in [
        (
            "runtime_server_inbox",
            "receipt_state",
            "state",
            enum_check::<ServerDeliveryState>("receipt_state"),
            false,
        ),
        (
            "runtime_server_outbox",
            "receipt_state",
            "state",
            enum_check::<ServerDeliveryState>("receipt_state"),
            false,
        ),
        (
            "runtime_server_fetch",
            "receipt_state",
            "state",
            enum_check::<ServerFetchState>("receipt_state"),
            false,
        ),
        (
            "runtime_server_outbox",
            "receipt_kind",
            "json_extract(body,'$.action')",
            enum_check::<ServerOperationKind>("receipt_kind"),
            false,
        ),
        (
            "runtime_access_outbound",
            "receipt_state",
            "json_extract(record,'$.outcome')",
            enum_check::<AccessOutboundState>("receipt_state"),
            false,
        ),
        (
            "runtime_federation_inbox",
            "receipt_ack",
            "json_extract(record,'$.ack')",
            enum_check::<FederationAckState>("receipt_ack"),
            false,
        ),
        (
            "runtime_federation_inbox",
            "receipt_kind",
            "json_extract(record,'$.kind')",
            enum_check::<FederationMessageKind>("receipt_kind"),
            true,
        ),
        (
            "runtime_federation_outbox",
            "receipt_state",
            "json_extract(record,'$.status')",
            enum_check::<FederationDeliveryState>("receipt_state"),
            false,
        ),
        (
            "runtime_federation_outbox",
            "receipt_kind",
            "json_extract(record,'$.kind')",
            enum_check::<FederationMessageKind>("receipt_kind"),
            false,
        ),
    ] {
        let required = if nullable { "" } else { "NOT NULL" };
        tx.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {name} TEXT GENERATED ALWAYS AS ({expression}) VIRTUAL {required} {check}"))?;
    }
    seed_receipt_finish_markers(tx)
}

pub(crate) fn seed_receipt_finish_markers(tx: &Transaction<'_>) -> StoreResult<()> {
    for family in CATALOG.iter().filter_map(|entry| entry.receipt_family()) {
        tx.execute("INSERT INTO runtime_payload_migrations(name,cursor,complete,status,updated,cutover_ms) VALUES(?1,0,1,'complete',0,0)", [receipt_finish_name(family.id)])?;
    }
    Ok(())
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
    apply_target(&mut analytics, MigrationTarget::Analytics)?;
    apply_target(&mut canvas, MigrationTarget::Canvas)?;
    run_receipt_migrations(&mut canvas, progress)?;
    RegistryMigration.apply(&Context::Registry(&registry))
}

/// Startup owns the state lock throughout all batches. Recovery and serving
/// connections are created only after this function and final validation return.
fn run_receipt_migrations(
    db: &mut Connection,
    progress: Option<&(dyn Fn(MigrationProgress) + Send + Sync)>,
) -> StoreResult<()> {
    let _ = progress;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    crate::schema::prepare_receipt_owners(&tx)?;
    tx.commit()?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    m2026_10_29_request_results_v2::convert_annotations(&tx)?;
    tx.commit()?;
    // The receipt families keep the pending transport rows in their old tables. Move them.
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    m2026_10_30_one_outbox::apply(&tx)?;
    tx.commit()?;
    crate::schema::verify_bootstrap(db)?;
    crate::verify_observed_tables_are_rowid(db)
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
    apply_target(db, MigrationTarget::Supervisor)
}

#[cfg(test)]
pub(crate) fn m2026_10_30_domain_model_for_tests(tx: &Transaction<'_>) -> StoreResult<()> {
    crate::domain::backfill(tx)
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn prepare_canvas_fixture(transaction: &Transaction<'_>) -> StoreResult<()> {
    for step in CATALOG.iter().filter(|step| {
        step.definition().target == MigrationTarget::Canvas && step.receipt_family().is_none()
    }) {
        step.apply(&Context::Sql(SqlContext { transaction }))?;
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

    #[tokio::test]
    async fn catalog_change_runs_new_body_and_failure_rolls_back_marker() {
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
        let root = tempfile::tempdir().expect("state");
        drop(
            crate::RuntimeStore::open(Some(root.path()))
                .await
                .expect("fresh state"),
        );
        let mut db = Connection::open(root.path().join("analytics.sqlite3")).expect("database");
        db.execute_batch(
            "CREATE TABLE counts(first INTEGER,second INTEGER); INSERT INTO counts VALUES(0,0)",
        )
        .expect("fixture");

        let run = |db: &mut Connection, steps: &[&dyn Migration]| {
            apply_target_catalog(db, MigrationTarget::Analytics, steps)
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
            "EXPLAIN QUERY PLAN SELECT r.id FROM chats ch INDEXED BY chats_vm CROSS JOIN runtime_agents r ON r.id=ch.id WHERE ch.execution_mode='vm' AND r.account_key=?1 AND r.hold_status='clear'",
            ["default"], |row| row.get(3),
        ).unwrap();
        assert!(detail.contains("chats_vm"), "{detail}");
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
            db.execute(
                "INSERT INTO chats(id,state,status,execution_mode) VALUES(?1,'active','idle',?2)",
                rusqlite::params![id, mode],
            )
            .unwrap();
        }
        let owners = || {
            db.prepare("SELECT r.id FROM chats ch JOIN runtime_agents r ON r.id=ch.id WHERE r.account_key=?1 AND ch.execution_mode='vm' AND r.hold_status='clear' ORDER BY r.id").unwrap().query_map(["default"], |row| row.get::<_,String>(0)).unwrap().collect::<Result<Vec<_>,_>>().unwrap()
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

        let step = CATALOG
            .iter()
            .find(|step| step.definition().id == "2026-10-30-dismissed-incidents-index")
            .expect("registered migration");
        let context = Context::Sql(SqlContext { transaction: &tx });
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

        let step = CATALOG
            .iter()
            .find(|step| step.definition().id == "2026-10-30-unused-monitor-stall-index")
            .expect("registered migration");
        step.apply(&Context::Sql(SqlContext { transaction: &tx }))
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
        assert!(
            position("2026-10-31-retired-start-indexes")
                < position("2026-10-30-unused-agent-columns")
        );
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
        let error = apply_target(&mut db, MigrationTarget::Analytics)
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

        let step = CATALOG
            .iter()
            .find(|step| step.definition().id == "2026-10-31-restore-monitor-active-index")
            .expect("migration");
        let context = Context::Sql(SqlContext { transaction: &tx });
        step.apply(&context).expect("restore index");
        step.apply(&context).expect("repeat migration");
        let plan: String = tx.query_row("EXPLAIN QUERY PLAN SELECT id FROM runtime_monitors WHERE monitor_status IN ('running','starting','approval')", [], |row| row.get(3)).expect("plan");
        assert!(plan.contains("runtime_monitor_startup_active"), "{plan}");
    }
}
