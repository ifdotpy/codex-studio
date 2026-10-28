//! Transactional location outbox and tool-receipt persistence. No delivery occurs here.
use crate::request_results::{AdmittedRequest, RequestResults, ResultEnvelope};
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[ConditionalTableRequirement {
        table_exists: "runtime_server_outbox",
        tables: &[TableRequirement {
            name: "runtime_server_outbox",
            columns: &[
                "id",
                "server",
                "body",
                "state",
                "result",
                "error",
                "completed_at",
            ],
        }],
    }],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[
        TableRequirement {
            name: "runtime_tool_requests",
            columns: &["id", "agent", "record"],
        },
        TableRequirement {
            name: "request_results",
            columns: &["id", "signature", "result", "created"],
        },
    ],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

pub struct ProjectLocationsRepository;
impl ProjectLocationsRepository {
    pub fn outbox(db: &Connection, id: &str) -> StoreResult<Option<Value>> {
        let raw: Option<String> = db.query_row(
            "SELECT json_object('id',id,'server',server,'body',body,'state',state,'result',result,'fingerprint',fingerprint,'attempts',attempts,'created_at',created_at,'completed_at',completed_at) FROM runtime_server_outbox WHERE id=?1",
            [id], |row| row.get(0),
        ).optional()?;
        raw.map(|raw| {
            // Validate the complete shared delivery row before the location
            // consumer decodes its action-specific body and receipt.
            let mut row = crate::system_reads::decode("runtime_server_outbox", id, &raw)?;
            for field in ["body", "result"] {
                if let Some(encoded) = row[field].as_str() {
                    row[field] = decode("runtime_server_outbox", id, encoded)?;
                }
            }
            Ok(row)
        })
        .transpose()
    }

    pub fn tool(db: &Connection, id: &str) -> StoreResult<Option<Value>> {
        let raw: Option<String> = db
            .query_row(
                "SELECT record FROM runtime_tool_requests WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        raw.map(|s| decode("runtime_tool_requests", id, &s))
            .transpose()
    }
    pub fn save_tool(
        tx: &Transaction<'_>,
        id: &str,
        record: &Value,
        result: &Value,
    ) -> StoreResult<()> {
        let raw: String = tx.query_row(
            "SELECT json_extract(record,'$.admitted') FROM runtime_tool_requests WHERE id=?1",
            [id],
            |row| row.get(0),
        )?;
        let admitted: AdmittedRequest = serde_json::from_str(&raw)?;
        RequestResults::settle(
            tx,
            &admitted,
            &ResultEnvelope {
                actor: record["agent"].as_str().map(str::to_owned),
                reply: result,
                references: Vec::new(),
            },
        )?;
        let mut record = record.clone();
        if let Some(fields) = record.as_object_mut() {
            fields.remove("result");
            fields.remove("_payloadBlobs");
        }
        tx.execute(
            "UPDATE runtime_tool_requests SET record=?1 WHERE id=?2",
            params![record.to_string(), id],
        )?;
        Ok(())
    }
    pub fn alias_requests(db: &Connection, actor: &str, alias: &str) -> StoreResult<Vec<String>> {
        let mut statement = db.prepare_cached("SELECT id FROM runtime_tool_requests WHERE agent=?1 AND EXISTS(SELECT 1 FROM json_each(record,'$.aliases') WHERE value=?2) LIMIT 2")?;
        Ok(statement
            .query_map(params![actor, alias], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }
}
fn decode(table: &'static str, id: &str, raw: &str) -> StoreResult<Value> {
    serde_json::from_str(raw).map_err(|_| StoreError::InvalidStoredRecord {
        object: table,
        id: id.to_owned(),
    })
}

inventory::submit! { crate::repositories::SchemaRegistration { repository: "project_locations", requirements: &REQUIREMENTS } }

/// Open project record; only the schema-defined location fields are consumed here.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ProjectLocationRecord {
    id: String,
    #[serde(flatten)]
    fields: serde_json::Map<String, Value>,
}
impl crate::JsonRecord for ProjectLocationRecord {
    type Id = String;
    const TABLE: &'static str = "runtime_projects";
    const SCHEMA: &'static str = "PersistedProjectRecord";
    const KEY_MODE: crate::RecordKeyMode = crate::RecordKeyMode::PayloadId;
    const QUERY_SHAPE: crate::RecordQueryShape = crate::RecordQueryShape::None;
    fn record_key(&self) -> Option<&String> {
        Some(&self.id)
    }
}
