use rusqlite::Connection;

use crate::StoreResult;
use crate::records::{
    AgentId, AgentRecord, AgentRepository, get_json_record, get_raw_record, put_json_record,
};
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
    pub fn get_raw(connection: &Connection, id: &AgentId) -> StoreResult<Option<String>> {
        get_raw_record(connection, "runtime_agents", &id.0)
    }

    pub fn get(connection: &Connection, id: &AgentId) -> StoreResult<Option<AgentRecord>> {
        get_json_record(connection, "runtime_agents", &id.0)
    }

    pub fn put(connection: &Connection, record: &AgentRecord) -> StoreResult<()> {
        put_json_record(connection, "runtime_agents", &record.id.0, record)
    }
}
inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "agents",
        requirements: &REQUIREMENTS,
    }
}
