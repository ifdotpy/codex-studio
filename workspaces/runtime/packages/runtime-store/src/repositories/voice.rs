//! Persisted native voice records, sessions, deliveries and approval inputs.

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;

use crate::schema::{SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};

const BOOTSTRAP_TABLES: &[TableRequirement] = &[
    TableRequirement {
        name: "voice_approvals",
        columns: &["id", "agent", "speech", "transcript", "status", "result"],
    },
    TableRequirement {
        name: "voice_deliveries",
        columns: &["id", "agent", "records", "text", "edited_text"],
    },
    TableRequirement {
        name: "voice_records",
        columns: &[
            "seq",
            "id",
            "agent",
            "session",
            "kind",
            "text",
            "item_id",
            "previous_item_id",
            "payload",
            "created",
        ],
    },
    TableRequirement {
        name: "voice_sessions",
        columns: &[
            "id", "agent", "created", "ended", "answer", "state", "error",
        ],
    },
    TableRequirement {
        name: "voice_speech_requests",
        columns: &["id", "session", "record_id", "state", "error"],
    },
];
pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: BOOTSTRAP_TABLES,
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct VoiceRecord {
    #[serde(rename = "seq")]
    pub sequence: i64,
    pub id: String,
    pub agent: String,
    pub session: String,
    pub kind: String,
    pub text: String,
    pub item_id: String,
    pub previous_item_id: String,
    pub payload: String,
    pub created: f64,
}

impl VoiceRecord {
    pub fn value(&self) -> StoreResult<Value> {
        Ok(serde_json::to_value(self)?)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct VoiceDelivery {
    pub id: String,
    pub agent: String,
    pub records: String,
    pub text: String,
    pub edited_text: Option<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct VoiceRepository;

impl VoiceRepository {
    pub fn record(connection: &Connection, id: &str) -> StoreResult<Option<VoiceRecord>> {
        Ok(connection
            .query_row(
                "SELECT seq,id,agent,session,kind,text,item_id,previous_item_id,payload,created \
                 FROM voice_records WHERE id=?1",
                [id],
                record_from_row,
            )
            .optional()?)
    }

    pub fn records(
        connection: &Connection,
        agent: &str,
        after: i64,
    ) -> StoreResult<Vec<VoiceRecord>> {
        let mut statement = connection.prepare(
            "SELECT seq,id,agent,session,kind,text,item_id,previous_item_id,payload,created \
             FROM voice_records WHERE agent=?1 AND seq>?2 ORDER BY seq",
        )?;
        let rows = statement.query_map(params![agent, after], record_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn latest_session(connection: &Connection, agent: &str) -> StoreResult<Option<Value>> {
        connection
            .query_row(
                "SELECT id,state,answer,error,ended FROM voice_sessions \
                 WHERE agent=?1 AND state IS NOT NULL ORDER BY created DESC LIMIT 1",
                [agent],
                |row| {
                    Ok(serde_json::json!({
                        "session_id": row.get::<_, String>(0)?,
                        "state": row.get::<_, Option<String>>(1)?,
                        "sdp": row.get::<_, Option<String>>(2)?,
                        "error": row.get::<_, Option<String>>(3)?,
                        "ended": row.get::<_, Option<f64>>(4)?,
                    }))
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn session(connection: &Connection, id: &str, agent: &str) -> StoreResult<Option<Value>> {
        connection
            .query_row(
                "SELECT id,state,answer,error,ended FROM voice_sessions WHERE id=?1 AND agent=?2",
                params![id, agent],
                |row| {
                    Ok(serde_json::json!({
                        "session_id": row.get::<_, String>(0)?,
                        "state": row.get::<_, Option<String>>(1)?,
                        "sdp": row.get::<_, Option<String>>(2)?,
                        "error": row.get::<_, Option<String>>(3)?,
                        "ended": row.get::<_, Option<f64>>(4)?,
                    }))
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn speech_receipt(connection: &Connection, id: &str) -> StoreResult<Option<Value>> {
        connection
            .query_row(
                "SELECT id,session,record_id,state,error FROM voice_speech_requests WHERE id=?1",
                [id],
                |row| {
                    Ok(serde_json::json!({
                        "id": row.get::<_, String>(0)?,
                        "session": row.get::<_, String>(1)?,
                        "record_id": row.get::<_, String>(2)?,
                        "state": row.get::<_, String>(3)?,
                        "error": row.get::<_, Option<String>>(4)?,
                    }))
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn delivered(connection: &Connection, agent: &str) -> StoreResult<Vec<(String, String)>> {
        let mut statement = connection.prepare(
            "SELECT id,records FROM voice_deliveries WHERE agent=?1 \
             AND id IN (SELECT id FROM runtime_events)",
        )?;
        let rows = statement.query_map([agent], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn insert_record(connection: &Connection, record: &VoiceRecord) -> StoreResult<()> {
        connection.execute(
            "INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                record.id,
                record.agent,
                record.session,
                record.kind,
                record.text,
                record.item_id,
                record.previous_item_id,
                record.payload,
                record.created,
            ],
        )?;
        Ok(())
    }

    pub fn session_exists(
        connection: &Connection,
        agent: &str,
        session: &str,
    ) -> StoreResult<bool> {
        connection
            .query_row(
                "SELECT 1 FROM voice_sessions WHERE id=?1 AND agent=?2",
                params![session, agent],
                |_| Ok(()),
            )
            .optional()
            .map(|value| value.is_some())
            .map_err(StoreError::from)
    }

    pub fn delivery(connection: &Connection, id: &str) -> StoreResult<Option<VoiceDelivery>> {
        connection
            .query_row(
                "SELECT id,agent,records,text,edited_text FROM voice_deliveries WHERE id=?1",
                [id],
                |row| {
                    Ok(VoiceDelivery {
                        id: row.get(0)?,
                        agent: row.get(1)?,
                        records: row.get(2)?,
                        text: row.get(3)?,
                        edited_text: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn deliveries(connection: &Connection, agent: &str) -> StoreResult<Vec<(String, String)>> {
        let mut statement =
            connection.prepare("SELECT id,records FROM voice_deliveries WHERE agent=?1")?;
        let rows = statement.query_map([agent], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn insert_delivery(
        connection: &Connection,
        id: &str,
        agent: &str,
        records: &str,
        text: &str,
        edited_text: Option<&str>,
    ) -> StoreResult<()> {
        connection.execute(
            "INSERT INTO voice_deliveries(id,agent,records,text,edited_text) VALUES(?1,?2,?3,?4,?5)",
            params![id, agent, records, text, edited_text],
        )?;
        Ok(())
    }
}

fn record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<VoiceRecord> {
    Ok(VoiceRecord {
        sequence: row.get(0)?,
        id: row.get(1)?,
        agent: row.get(2)?,
        session: row.get(3)?,
        kind: row.get(4)?,
        text: row.get(5)?,
        item_id: row.get(6)?,
        previous_item_id: row.get(7)?,
        payload: row.get(8)?,
        created: row.get(9)?,
    })
}

inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "voice",
        requirements: &REQUIREMENTS,
    }
}
