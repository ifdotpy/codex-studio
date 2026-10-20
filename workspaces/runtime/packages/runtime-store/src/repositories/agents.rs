use crate::CachedSql;
use rusqlite::Connection;

use crate::StoreResult;
use crate::records::{AgentId, AgentRecord, AgentRepository, Table};
use crate::schema::{
    ConditionalTriggerRequirement, SchemaRequirements, TableRequirement, TriggerRequirement,
};

const TABLES: &[TableRequirement] = &[
    TableRequirement {
        name: "runtime_agents",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_agent_record_generation",
        columns: &["id", "value"],
    },
    TableRequirement {
        name: "runtime_scheduler_agent_change_state",
        columns: &["id", "entries", "total", "generation"],
    },
    TableRequirement {
        name: "runtime_scheduler_agent_changes",
        columns: &["id", "generation"],
    },
];
const TRIGGERS: &[TriggerRequirement] = &[
    TriggerRequirement {
        name: "runtime_agent_record_generation_insert",
        table: "runtime_agents",
    },
    TriggerRequirement {
        name: "runtime_agent_record_generation_update",
        table: "runtime_agents",
    },
    TriggerRequirement {
        name: "runtime_agent_record_generation_delete",
        table: "runtime_agents",
    },
];
const TRANSCRIPT_TRIGGERS: &[TriggerRequirement] = &[
    TriggerRequirement {
        name: "sync_transcript_revision_runtime_agents_insert",
        table: "runtime_agents",
    },
    TriggerRequirement {
        name: "sync_transcript_revision_runtime_agents_update",
        table: "runtime_agents",
    },
    TriggerRequirement {
        name: "sync_transcript_revision_runtime_agents_delete",
        table: "runtime_agents",
    },
];

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: TABLES,
    conditional_tables: &[],
    triggers: TRIGGERS,
    conditional_triggers: &[ConditionalTriggerRequirement {
        table_exists: "runtime_transcript_revisions",
        triggers: TRANSCRIPT_TRIGGERS,
    }],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

impl AgentRepository {
    /// Seed the singleton rows the change-journal triggers update when a
    /// schema-only state lacks them; without them no change is journaled.
    pub(crate) fn prepare_change_journal(
        transaction: &rusqlite::Transaction<'_>,
    ) -> StoreResult<()> {
        if crate::schema::table_exists(transaction, "runtime_agent_record_generation")?
            && crate::schema::table_exists(transaction, "runtime_scheduler_agent_change_state")?
        {
            transaction.execute_batch(
                "INSERT OR IGNORE INTO runtime_agent_record_generation(id,value) VALUES(1,0);
                 INSERT OR IGNORE INTO runtime_scheduler_agent_change_state(id,floor,generation,entries,total) VALUES(1,0,0,0,0);",
            )?;
        }
        Ok(())
    }

    pub fn get_raw(connection: &Connection, id: &AgentId) -> StoreResult<Option<String>> {
        Ok(Table::<AgentRecord>::new(connection)
            .get_with_raw(id)?
            .map(|(_, raw)| raw))
    }

    pub fn get(connection: &Connection, id: &AgentId) -> StoreResult<Option<AgentRecord>> {
        Table::<AgentRecord>::new(connection).get(id)
    }

    pub fn put(connection: &Connection, record: &AgentRecord) -> StoreResult<()> {
        Table::<AgentRecord>::new(connection).put(record)?;
        let has_receipts: bool = connection.query_row_cached(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_operation_receipts')",
            [],
            |row| row.get(0),
        )?;
        if has_receipts {
            connection.execute_cached(
                "DELETE FROM runtime_operation_receipts WHERE id=?1 AND signature=?2",
                rusqlite::params![
                    format!(
                        "{}{}",
                        crate::INVALID_AGENT_HOLD_RECEIPT_PREFIX,
                        record.id.0
                    ),
                    crate::INVALID_AGENT_HOLD_RECEIPT_SIGNATURE
                ],
            )?;
        }
        Ok(())
    }
}
inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "agents",
        requirements: &REQUIREMENTS,
    }
}

#[cfg(test)]
mod invalid_agent_repair_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn valid_agent_repair_clears_the_invalid_record_hold() {
        let directory = tempfile::tempdir().expect("temporary runtime");
        let store = crate::RuntimeStore::open(Some(directory.path()))
            .await
            .expect("runtime opens");
        let agent = AgentRecord::from_value(json!({
            "id":"agent", "rootId":"agent", "epoch":1, "status":"idle"
        }))
        .expect("valid repair record");
        let seed_agent = agent.clone();
        store
            .transaction(move |tx| {
                AgentRepository::put(tx, &seed_agent)?;
                tx.execute(
                    "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES (?1,?2,'{}')",
                    rusqlite::params![
                        format!("{}agent", crate::INVALID_AGENT_HOLD_RECEIPT_PREFIX),
                        crate::INVALID_AGENT_HOLD_RECEIPT_SIGNATURE
                    ],
                )?;
                Ok(())
            })
            .await
            .expect("seed agent and hold");

        store
            .transaction(move |tx| AgentRepository::put(tx, &agent))
            .await
            .expect("typed repair saves and clears hold atomically");

        let connection = store.read().expect("reader opens");
        let remaining: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_operation_receipts WHERE id=?1",
                [format!("{}agent", crate::INVALID_AGENT_HOLD_RECEIPT_PREFIX)],
                |row| row.get(0),
            )
            .expect("hold receipt count reads");
        assert_eq!(remaining, 0);
    }
}
