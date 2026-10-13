//! Transactional location outbox and tool-receipt persistence. No delivery occurs here.
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[
        ConditionalTableRequirement {
            table_exists: "runtime_server_outbox",
            tables: &[
                TableRequirement {
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
                },
                TableRequirement {
                    name: "runtime_server_retired",
                    columns: &["id", "direction", "server"],
                },
            ],
        },
        ConditionalTableRequirement {
            table_exists: "runtime_tool_requests",
            tables: &[
                TableRequirement {
                    name: "runtime_tool_requests",
                    columns: &["id", "record"],
                },
                TableRequirement {
                    name: "runtime_tool_results",
                    columns: &["id", "result"],
                },
                TableRequirement {
                    name: "runtime_tool_request_aliases",
                    columns: &["agent", "alias", "request"],
                },
            ],
        },
    ],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

pub struct ProjectLocationsRepository;
impl ProjectLocationsRepository {
    pub fn outbox(db: &Connection, id: &str) -> StoreResult<Option<Value>> {
        let row = db
            .query_row(
                "SELECT server,body,state,result FROM runtime_server_outbox WHERE id=?1",
                [id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(server,body,state,result)| Ok(json!({"id":id,"server":server,"body":decode("runtime_server_outbox",id,&body)?,"state":state,"result":result.map(|s|decode("runtime_server_outbox",id,&s)).transpose()?}))).transpose()
    }
    pub fn complete(tx: &Transaction<'_>, id: &str, receipt: &Value, now: f64) -> StoreResult<()> {
        tx.execute("UPDATE runtime_server_outbox SET state='complete',result=?1,error=NULL,completed_at=?2 WHERE id=?3 AND state='queued'",params![receipt.to_string(),now,id])?;
        Ok(())
    }
    pub fn registration_receipt(
        tx: &Transaction<'_>,
        project: &str,
        request: &str,
        receipt: &Value,
    ) -> StoreResult<()> {
        let mut result = receipt.clone();
        if let Some(object) = result.as_object_mut() {
            object.insert("projectId".into(), json!(project));
        }
        tx.execute("UPDATE runtime_operation_receipts SET result=?1 WHERE id=?2 AND json_extract(result,'$.requestId')=?3",params![result.to_string(),format!("project-registration:{project}"),request])?;
        Ok(())
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
    pub fn tool_result(db: &Connection, id: &str) -> StoreResult<Option<Value>> {
        let raw: Option<String> = db
            .query_row(
                "SELECT result FROM runtime_tool_results WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        raw.map(|s| decode("runtime_tool_results", id, &s))
            .transpose()
    }
    pub fn save_tool(
        tx: &Transaction<'_>,
        id: &str,
        record: &Value,
        result: &Value,
    ) -> StoreResult<()> {
        tx.execute(
            "UPDATE runtime_tool_results SET result=?1 WHERE id=?2",
            params![result.to_string(), id],
        )?;
        tx.execute(
            "UPDATE runtime_tool_requests SET record=?1 WHERE id=?2",
            params![record.to_string(), id],
        )?;
        Ok(())
    }
    pub fn alias_requests(db: &Connection, actor: &str, alias: &str) -> StoreResult<Vec<String>> {
        let mut statement = db.prepare("SELECT request FROM runtime_tool_request_aliases WHERE agent=?1 AND alias=?2 LIMIT 1001")?;
        Ok(statement
            .query_map(params![actor, alias], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }
    pub fn retired(db: &Connection, id: &str, server: &str) -> StoreResult<bool> {
        Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM runtime_server_retired WHERE id=?1 AND direction='out' AND server=?2)",params![id,server],|r|r.get(0))?)
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
    const SCHEMA: &'static str = "relocation/RelocationProject";
    const KEY_MODE: crate::RecordKeyMode = crate::RecordKeyMode::PayloadId;
    const QUERY_SHAPE: crate::RecordQueryShape = crate::RecordQueryShape::None;
    fn record_key(&self) -> Option<&String> {
        Some(&self.id)
    }
}

pub(crate) fn check_outbox(db: &Connection) -> StoreResult<crate::StateCheckTable> {
    let mut report = crate::StateCheckTable {
        table: "runtime_server_outbox",
        row_count: 0,
        rejected_count: 0,
        failures: vec![],
    };
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='runtime_server_outbox')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(report);
    }
    let mut failures = std::collections::BTreeMap::<(String, String), u64>::new();
    let mut statement = db.prepare("SELECT id,body FROM runtime_server_outbox")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        report.row_count += 1;
        let id: String = row.get(0)?;
        let body: Result<Value, _> = serde_json::from_str(&row.get::<_, String>(1)?);
        let failure = match body {
            Err(_) => Some(("/body".to_owned(), "json".to_owned())),
            Ok(body)
                if matches!(
                    body["action"].as_str(),
                    Some("add_location" | "remove_location")
                ) =>
            {
                match ProjectLocationsRepository::outbox(db, &id)? {
                    Some(value) => {
                        studio_runtime_schema::first_error("relocation/LocationOutbox", &value)
                            .map_err(StoreError::SchemaValidation)?
                    }
                    None => Some(("/id".to_owned(), "rowKey".to_owned())),
                }
            }
            Ok(_) => None,
        };
        if let Some(key) = failure {
            report.rejected_count += 1;
            *failures.entry(key).or_default() += 1;
        }
    }
    report.failures = failures
        .into_iter()
        .map(|((pointer, keyword), count)| crate::StateCheckFailure {
            pointer,
            keyword,
            count,
        })
        .collect();
    Ok(report)
}
