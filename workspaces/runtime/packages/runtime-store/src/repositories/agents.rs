use crate::CachedSql;
use rusqlite::Connection;
use std::collections::BTreeSet;
use studio_runtime_schema::persistence::HoldStatus;

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
    bootstrap_tables: &[crate::agent_volatile::TABLE_REQUIREMENT],
    bootstrap_triggers: &crate::agent_volatile::TRIGGER_REQUIREMENTS,
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
        Self::hold(connection, &record.id.0, HoldStatus::Clear, None)?;
        Ok(())
    }

    /// Quarantine is independent of record decoding, including malformed JSON.
    pub fn hold(
        connection: &Connection,
        id: &str,
        status: HoldStatus,
        incident: Option<&str>,
    ) -> StoreResult<()> {
        connection.execute_cached(
            "UPDATE runtime_agents SET hold_status=?2,hold_incident=?3 WHERE id=?1",
            rusqlite::params![id, status, incident],
        )?;
        Ok(())
    }

    pub fn event_fired(connection: &Connection, root: &str, event: &str) -> StoreResult<bool> {
        let agent = Self::get(connection, &AgentId::from(root))?
            .ok_or(crate::StoreError::RecordContract("missing event root"))?;
        let names: BTreeSet<String> = agent
            .extra
            .get("firedEvents")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        Ok(names.contains(event))
    }

    /// The root owns every fired name for its lifetime; first insertion wins.
    pub fn fire_event(
        tx: &rusqlite::Transaction<'_>,
        root: &str,
        event: &str,
    ) -> StoreResult<bool> {
        let mut agent = Self::get(tx, &AgentId::from(root))?
            .ok_or(crate::StoreError::RecordContract("missing event root"))?;
        let mut names: BTreeSet<String> = agent
            .extra
            .get("firedEvents")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        if !names.insert(event.to_owned()) {
            return Ok(false);
        }
        agent
            .extra
            .insert("firedEvents".into(), serde_json::to_value(names)?);
        Self::put(tx, &agent)?;
        Ok(true)
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

    #[test]
    fn receipt_domain_quarantine_does_not_decode_malformed_agent() {
        let db = rusqlite::Connection::open_in_memory().expect("database");
        db.execute_batch(&format!(
            "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT,hold_status TEXT {},hold_incident TEXT); INSERT INTO runtime_agents(id,record,hold_status) VALUES('broken','malformed','clear')",
            studio_runtime_schema::persistence::enum_check::<HoldStatus>("hold_status")
        )).expect("owner shape");
        AgentRepository::hold(&db, "broken", HoldStatus::Invalid, Some("incident"))
            .expect("independent quarantine");
        let state: (HoldStatus, String, String) = db
            .query_row(
                "SELECT hold_status,hold_incident,record FROM runtime_agents WHERE id='broken'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("scalar authority");
        assert_eq!(
            state,
            (HoldStatus::Invalid, "incident".into(), "malformed".into())
        );
    }

    #[test]
    fn valid_agent_repair_clears_the_invalid_record_hold() {
        let mut connection = crate::fresh_state::test_canvas();
        let agent = AgentRecord::from_value(json!({
            "id":"agent", "provider":"codex", "rootId":"agent", "epoch":1, "status":"idle"
        }))
        .expect("valid repair record");
        AgentRepository::put(&connection, &agent).expect("seed agent");
        AgentRepository::hold(&connection, "agent", HoldStatus::Invalid, Some("incident"))
            .expect("seed hold");
        let tx = connection.transaction().expect("repair transaction");
        AgentRepository::put(&tx, &agent).expect("typed repair saves and clears hold");
        tx.commit().expect("repair commits atomically");
        let status: HoldStatus = connection
            .query_row(
                "SELECT hold_status FROM runtime_agents WHERE id='agent'",
                [],
                |row| row.get(0),
            )
            .expect("hold");
        assert_eq!(status, HoldStatus::Clear);
    }
    #[test]
    fn receipt_domain_fired_event_set_retains_all_names_and_rolls_back() {
        let mut db = crate::fresh_state::test_canvas();
        let root = AgentRecord::from_value(
            json!({"id":"root","rootId":"root","provider":"codex","epoch":1,"status":"idle"}),
        )
        .expect("root");
        AgentRepository::put(&db, &root).expect("save");
        let tx = db.transaction().expect("tx");
        assert!(AgentRepository::fire_event(&tx, "root", "one").expect("first"));
        assert!(AgentRepository::fire_event(&tx, "root", "two").expect("second"));
        assert!(!AgentRepository::fire_event(&tx, "root", "one").expect("replay"));
        tx.commit().expect("commit");
        assert!(AgentRepository::event_fired(&db, "root", "one").expect("one"));
        let tx = db.transaction().expect("tx");
        assert!(AgentRepository::fire_event(&tx, "root", "three").expect("three"));
        tx.rollback().expect("rollback");
        assert!(!AgentRepository::event_fired(&db, "root", "three").expect("not fired"));
    }
}
