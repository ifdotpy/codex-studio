//! Persisted checkpoint records and their capture reservations.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};

use crate::records::{decode_json_record, get_json_record};
use crate::schema::{SchemaRequirements, TableRequirement};
use crate::{EventWriteRepository, StoreError, StoreResult};

const CHECKPOINTS: &[TableRequirement] = &[TableRequirement {
    name: "runtime_checkpoints",
    columns: &["id", "record"],
}];
const OPERATIONS: &[TableRequirement] = &[TableRequirement {
    name: "runtime_workspace_operations",
    columns: &["id", "record"],
}];
const ITEMS: &[TableRequirement] = &[TableRequirement {
    name: "runtime_items",
    columns: &["id", "agent", "record"],
}];
const MONITORS: &[TableRequirement] = &[TableRequirement {
    name: "runtime_monitors",
    columns: &["id", "record"],
}];
const TASKS: &[TableRequirement] = &[TableRequirement {
    name: "runtime_tasks",
    columns: &["id", "record"],
}];

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[TableRequirement {
        name: "runtime_agents",
        columns: &["id", "record"],
    }],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[
        CHECKPOINTS[0],
        OPERATIONS[0],
        ITEMS[0],
        MONITORS[0],
        TASKS[0],
    ],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "checkpoints",
        requirements: &REQUIREMENTS,
    }
}

const REQUIRED_TABLES: &[&str] = &[
    "runtime_checkpoints",
    "runtime_workspace_operations",
    "runtime_items",
    "runtime_monitors",
    "runtime_tasks",
];
const ACTIVE_OPERATION_PHASES: &[&str] = &[
    "capture_pending",
    "capture_running",
    "local_mutation",
    "provider_pending",
    "provider_ready",
    "recovery_required",
];
const ACTIVE_MONITOR_STATUSES: &[&str] = &["running", "starting", "approval"];

#[derive(Clone, Debug, PartialEq)]
pub struct StoredCheckpointObject {
    pub id: String,
    pub record: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceSnapshot {
    pub agent: StoredCheckpointObject,
    pub parent_checkpoint: Option<StoredCheckpointObject>,
    pub agents: Vec<StoredCheckpointObject>,
    pub operations: Vec<StoredCheckpointObject>,
    pub monitors: Vec<StoredCheckpointObject>,
    pub tasks: Vec<StoredCheckpointObject>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CheckpointsRepository;

impl CheckpointsRepository {
    pub fn tables_available(connection: &Connection) -> StoreResult<bool> {
        for table in REQUIRED_TABLES {
            if !table_exists(connection, table)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn checkpoint_table_available(connection: &Connection) -> StoreResult<bool> {
        table_exists(connection, "runtime_checkpoints")
    }

    pub fn agent(connection: &Connection, id: &str) -> StoreResult<Option<StoredCheckpointObject>> {
        read_object(connection, "runtime_agents", id)
    }

    pub fn checkpoint(
        connection: &Connection,
        id: &str,
    ) -> StoreResult<Option<StoredCheckpointObject>> {
        if !table_exists(connection, "runtime_checkpoints")? {
            return Ok(None);
        }
        read_object(connection, "runtime_checkpoints", id)
    }

    pub fn restore_expected_tree(
        connection: &Connection,
        agent_id: &str,
        checkpoint_id: &str,
    ) -> StoreResult<Option<String>> {
        let operations = read_active(
            connection,
            "runtime_workspace_operations",
            "phase",
            ACTIVE_OPERATION_PHASES,
        )?;
        Ok(operations.into_iter().find_map(|operation| {
            (operation.record.get("kind").and_then(Value::as_str) == Some("restore")
                && operation.record.get("agent").and_then(Value::as_str) == Some(agent_id)
                && operation.record.get("checkpoint").and_then(Value::as_str)
                    == Some(checkpoint_id))
            .then(|| {
                operation
                    .record
                    .get("expectedTree")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .flatten()
        }))
    }

    pub fn workspace_snapshot(
        connection: &Connection,
        agent_id: &str,
    ) -> StoreResult<Option<WorkspaceSnapshot>> {
        let Some(agent) = Self::agent(connection, agent_id)? else {
            return Ok(None);
        };
        let parent_checkpoint = agent
            .record
            .get("checkpointHistoryHead")
            .and_then(Value::as_str)
            .map(|id| read_object(connection, "runtime_checkpoints", id))
            .transpose()?
            .flatten();
        Ok(Some(WorkspaceSnapshot {
            agent,
            parent_checkpoint,
            agents: read_all(connection, "runtime_agents")?,
            operations: read_active(
                connection,
                "runtime_workspace_operations",
                "phase",
                ACTIVE_OPERATION_PHASES,
            )?,
            monitors: read_active(
                connection,
                "runtime_monitors",
                "status",
                ACTIVE_MONITOR_STATUSES,
            )?,
            tasks: read_active(connection, "runtime_tasks", "status", &["running"])?,
        }))
    }

    pub fn reserve_capture(
        transaction: &Transaction<'_>,
        expected: &WorkspaceSnapshot,
        operation_id: &str,
        now: f64,
    ) -> StoreResult<bool> {
        let Some(current) = Self::workspace_snapshot(transaction, &expected.agent.id)? else {
            return Ok(false);
        };
        if !same_workspace_activity(&current, expected) {
            return Ok(false);
        }
        let agent =
            current.agent.record.as_object().ok_or_else(|| {
                StoreError::SchemaMismatch("invalid stored agents record".to_owned())
            })?;
        let source = json!({
            "accountKey": agent.get("accountKey").and_then(Value::as_str).unwrap_or("default"),
            "threadId": agent.get("threadId"),
            "cwd": agent.get("cwd"),
        });
        let operation = json!({
            "id": operation_id,
            "kind": "capture",
            "agent": expected.agent.id,
            "cwd": agent.get("cwd"),
            "epoch": agent.get("epoch"),
            "turnId": Value::Null,
            "source": source,
            "phase": "capture_pending",
            "created": now,
        });
        transaction.execute(
            "INSERT INTO runtime_workspace_operations(id,record) VALUES(?1,?2)",
            params![operation_id, serde_json::to_string(&operation)?],
        )?;
        let mut record = current.agent.record;
        let object = record
            .as_object_mut()
            .ok_or_else(|| StoreError::SchemaMismatch("invalid stored agents record".to_owned()))?;
        object.insert(
            "workspaceOperation".to_owned(),
            Value::String("capture".to_owned()),
        );
        object.insert(
            "workspaceReservationId".to_owned(),
            Value::String(operation_id.to_owned()),
        );
        update_agent(
            transaction,
            &expected.agent.id,
            &record,
            &[
                ("workspaceOperation", json!("capture")),
                ("workspaceReservationId", json!(operation_id)),
            ],
        )?;
        Ok(true)
    }

    pub fn set_capture_running(
        transaction: &Transaction<'_>,
        expected: &WorkspaceSnapshot,
        operation_id: &str,
        now: f64,
    ) -> StoreResult<bool> {
        let Some(current) = Self::workspace_snapshot(transaction, &expected.agent.id)? else {
            return Ok(false);
        };
        if !same_workspace_activity(&current, expected) {
            return Ok(false);
        }
        let Some(agent) = current.agent.record.as_object() else {
            return Err(StoreError::SchemaMismatch(
                "invalid stored agent".to_owned(),
            ));
        };
        if agent.get("workspaceReservationId").and_then(Value::as_str) != Some(operation_id)
            || agent.get("workspaceOperation").and_then(Value::as_str) != Some("capture")
        {
            return Ok(false);
        }
        let Some(mut operation) =
            read_object(transaction, "runtime_workspace_operations", operation_id)?
        else {
            return Ok(false);
        };
        let object = operation.record.as_object_mut().ok_or_else(|| {
            StoreError::SchemaMismatch("invalid stored workspace operation".to_owned())
        })?;
        if object.get("phase").and_then(Value::as_str) != Some("capture_pending")
            || object.get("kind").and_then(Value::as_str) != Some("capture")
            || object.get("agent").and_then(Value::as_str) != Some(expected.agent.id.as_str())
            || object.get("cwd") != agent.get("cwd")
            || object.get("epoch") != agent.get("epoch")
        {
            return Ok(false);
        }
        let source = json!({
            "accountKey": agent.get("accountKey").and_then(Value::as_str).unwrap_or("default"),
            "threadId": agent.get("threadId"),
            "cwd": agent.get("cwd"),
        });
        if object.get("source") != Some(&source) {
            return Ok(false);
        }
        object.insert(
            "phase".to_owned(),
            Value::String("capture_running".to_owned()),
        );
        object.insert("updated".to_owned(), json!(now));
        update_object(
            transaction,
            "runtime_workspace_operations",
            operation_id,
            &operation.record,
        )?;
        Ok(true)
    }

    pub fn complete_capture(
        transaction: &Transaction<'_>,
        agent_id: &str,
        operation_id: &str,
        expected_parent_id: Option<&str>,
        checkpoint: &Value,
        now: f64,
    ) -> StoreResult<bool> {
        let id = checkpoint
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| StoreError::SchemaMismatch("checkpoint has no id".to_owned()))?;
        let Some(mut agent) = read_object(transaction, "runtime_agents", agent_id)? else {
            return Ok(false);
        };
        let Some(mut operation) =
            read_object(transaction, "runtime_workspace_operations", operation_id)?
        else {
            return Ok(false);
        };
        let agent_record = agent
            .record
            .as_object()
            .ok_or_else(|| StoreError::SchemaMismatch("invalid stored agent".to_owned()))?;
        let operation_record = operation.record.as_object().ok_or_else(|| {
            StoreError::SchemaMismatch("invalid stored workspace operation".to_owned())
        })?;
        if agent_record.get("id").and_then(Value::as_str) != Some(agent_id)
            || operation_record.get("id").and_then(Value::as_str) != Some(operation_id)
            || operation_id.strip_prefix("capture:") != Some(id)
            || checkpoint.get("historyParent").and_then(Value::as_str) != expected_parent_id
            || agent_record
                .get("workspaceReservationId")
                .and_then(Value::as_str)
                != Some(operation_id)
            || agent_record
                .get("workspaceOperation")
                .and_then(Value::as_str)
                != Some("capture")
            || agent_record
                .get("checkpointHistoryHead")
                .and_then(Value::as_str)
                != expected_parent_id
            || operation_record.get("kind").and_then(Value::as_str) != Some("capture")
            || operation_record.get("phase").and_then(Value::as_str) != Some("capture_running")
            || operation_record.get("agent").and_then(Value::as_str) != Some(agent_id)
            || checkpoint.get("agent").and_then(Value::as_str) != Some(agent_id)
        {
            return Ok(false);
        }
        insert_object(transaction, "runtime_checkpoints", id, checkpoint)?;
        let object = agent
            .record
            .as_object_mut()
            .ok_or_else(|| StoreError::SchemaMismatch("invalid stored agents record".to_owned()))?;
        object.insert(
            "checkpointHistoryHead".to_owned(),
            Value::String(id.to_owned()),
        );
        object.insert("workspaceOperation".to_owned(), Value::Null);
        object.insert("checkpointError".to_owned(), Value::Null);
        object.remove("workspaceReservationId");
        update_agent(
            transaction,
            agent_id,
            &agent.record,
            &[
                ("checkpointHistoryHead", json!(id)),
                ("workspaceOperation", Value::Null),
                ("workspaceReservationId", Value::Null),
                ("checkpointError", Value::Null),
            ],
        )?;
        let object = operation.record.as_object_mut().ok_or_else(|| {
            StoreError::SchemaMismatch("invalid stored workspace operation".to_owned())
        })?;
        object.insert("phase".to_owned(), Value::String("completed".to_owned()));
        object.insert("updated".to_owned(), json!(now));
        object.insert("error".to_owned(), Value::Null);
        update_object(
            transaction,
            "runtime_workspace_operations",
            operation_id,
            &operation.record,
        )?;
        Ok(true)
    }

    pub fn history_delta(
        connection: &Connection,
        agent_id: &str,
        parent_boundary: Option<i64>,
    ) -> StoreResult<(Vec<String>, Option<i64>)> {
        let boundary = connection
            .query_row(
                "SELECT rowid FROM runtime_items WHERE agent=?1 \
                 AND json_extract(record,'$.afterRestore') IS NULL \
                 ORDER BY rowid DESC LIMIT 1",
                [agent_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let mut statement = connection.prepare(
            "SELECT id FROM runtime_items WHERE agent=?1 \
             AND json_extract(record,'$.afterRestore') IS NULL \
             AND (?2 IS NULL OR rowid>?2) ORDER BY rowid",
        )?;
        let ids = statement
            .query_map(params![agent_id, parent_boundary], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok((ids, boundary))
    }

    pub fn settle_capture(
        transaction: &Transaction<'_>,
        operation_id: &str,
        error: Option<&str>,
        now: f64,
    ) -> StoreResult<()> {
        let Some(mut operation) =
            read_object(transaction, "runtime_workspace_operations", operation_id)?
        else {
            return Ok(());
        };
        let object = operation.record.as_object_mut().ok_or_else(|| {
            StoreError::SchemaMismatch("invalid stored workspace operation".to_owned())
        })?;
        if !matches!(
            object.get("phase").and_then(Value::as_str),
            Some("capture_pending" | "capture_running")
        ) {
            return Ok(());
        }
        object.insert(
            "phase".to_owned(),
            Value::String(
                if error.is_some() {
                    "failed"
                } else {
                    "completed"
                }
                .to_owned(),
            ),
        );
        object.insert("updated".to_owned(), json!(now));
        object.insert(
            "error".to_owned(),
            error.map_or(Value::Null, |message| Value::String(message.to_owned())),
        );
        update_object(
            transaction,
            "runtime_workspace_operations",
            operation_id,
            &operation.record,
        )?;
        let agent_id = operation
            .record
            .get("agent")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Some(mut agent) = read_object(transaction, "runtime_agents", agent_id)?
            && agent
                .record
                .get("workspaceReservationId")
                .and_then(Value::as_str)
                == Some(operation_id)
            && agent
                .record
                .get("workspaceOperation")
                .and_then(Value::as_str)
                == Some("capture")
        {
            let object = agent
                .record
                .as_object_mut()
                .ok_or_else(|| StoreError::SchemaMismatch("invalid stored agent".to_owned()))?;
            object.insert("workspaceOperation".to_owned(), Value::Null);
            object.insert(
                "checkpointError".to_owned(),
                error.map_or(Value::Null, |message| Value::String(message.to_owned())),
            );
            object.remove("workspaceReservationId");
            let checkpoint_error = error.map_or(Value::Null, |message| json!(message));
            update_agent(
                transaction,
                agent_id,
                &agent.record,
                &[
                    ("workspaceOperation", Value::Null),
                    ("workspaceReservationId", Value::Null),
                    ("checkpointError", checkpoint_error),
                ],
            )?;
        }
        Ok(())
    }

    pub fn recover_captures(transaction: &Transaction<'_>, now: f64) -> StoreResult<()> {
        let active_phases = [
            "provider_pending",
            "provider_ready",
            "local_mutation",
            "recovery_required",
            "capture_pending",
            "capture_running",
        ];
        let mut operations = read_all(transaction, "runtime_workspace_operations")?;
        for operation in &operations {
            let kind = operation.record.get("kind").and_then(Value::as_str);
            if let Some(kind @ ("capture" | "checkpoint")) = kind {
                validate_active_record(
                    "runtime_workspace_operations",
                    operation,
                    &["kind", "agent", "phase"],
                )?;
                if !operation.id.starts_with(&format!("{kind}:")) {
                    return Err(invalid_record(
                        "runtime_workspace_operations",
                        &operation.id,
                    ));
                }
            }
            let Some(phase) = operation.record.get("phase").and_then(Value::as_str) else {
                if matches!(kind, Some("capture" | "checkpoint")) {
                    return Err(invalid_record(
                        "runtime_workspace_operations",
                        &operation.id,
                    ));
                }
                continue;
            };
            if active_phases.contains(&phase) {
                validate_active_record(
                    "runtime_workspace_operations",
                    operation,
                    &["kind", "agent", "phase"],
                )?;
            }
        }
        operations.retain(|operation| {
            operation
                .record
                .get("phase")
                .and_then(Value::as_str)
                .is_some_and(|phase| active_phases.contains(&phase))
                && matches!(
                    operation.record.get("kind").and_then(Value::as_str),
                    Some("capture" | "checkpoint")
                )
        });
        for mut operation in operations {
            let operation_id = operation.id.clone();
            let Some(agent_id) = operation
                .record
                .get("agent")
                .and_then(Value::as_str)
                .map(str::to_owned)
            else {
                return Err(invalid_record(
                    "runtime_workspace_operations",
                    &operation_id,
                ));
            };
            let object = operation
                .record
                .as_object_mut()
                .ok_or_else(|| invalid_record("runtime_workspace_operations", &operation_id))?;
            object.insert("phase".to_owned(), Value::String("failed".to_owned()));
            object.insert("updated".to_owned(), json!(now));
            object.insert(
                "error".to_owned(),
                Value::String("Server restarted before checkpoint capture finished".to_owned()),
            );
            update_object(
                transaction,
                "runtime_workspace_operations",
                &operation_id,
                &operation.record,
            )?;
            let Some(mut agent) = read_object(transaction, "runtime_agents", &agent_id)? else {
                return Err(invalid_record(
                    "runtime_workspace_operations",
                    &operation_id,
                ));
            };
            let expected_kind = operation.record["kind"].as_str().unwrap_or_default();
            if agent
                .record
                .get("workspaceReservationId")
                .and_then(Value::as_str)
                != Some(operation_id.as_str())
                || agent
                    .record
                    .get("workspaceOperation")
                    .and_then(Value::as_str)
                    != Some(expected_kind)
            {
                return Err(invalid_record("runtime_agents", &agent_id));
            }
            let object = agent
                .record
                .as_object_mut()
                .ok_or_else(|| invalid_record("runtime_agents", &agent_id))?;
            object.insert("workspaceOperation".to_owned(), Value::Null);
            object.remove("workspaceReservationId");
            object.insert(
                "checkpointError".to_owned(),
                json!("Server restarted before checkpoint capture finished"),
            );
            update_agent(
                transaction,
                &agent_id,
                &agent.record,
                &[
                    ("workspaceOperation", Value::Null),
                    ("workspaceReservationId", Value::Null),
                    (
                        "checkpointError",
                        json!("Server restarted before checkpoint capture finished"),
                    ),
                ],
            )?;
        }
        for mut agent in read_all(transaction, "runtime_agents")? {
            let kind = agent
                .record
                .get("workspaceOperation")
                .and_then(Value::as_str);
            if !matches!(kind, Some("capture" | "checkpoint")) {
                continue;
            }
            let Some(_operation_id) = agent
                .record
                .get("workspaceReservationId")
                .and_then(Value::as_str)
            else {
                return Err(invalid_record("runtime_agents", &agent.id));
            };
            let object = agent
                .record
                .as_object_mut()
                .ok_or_else(|| invalid_record("runtime_agents", &agent.id))?;
            object.insert("workspaceOperation".to_owned(), Value::Null);
            object.remove("workspaceReservationId");
            object.insert(
                "checkpointError".to_owned(),
                json!("Server restarted before checkpoint capture finished"),
            );
            update_agent(
                transaction,
                &agent.id,
                &agent.record,
                &[
                    ("workspaceOperation", Value::Null),
                    ("workspaceReservationId", Value::Null),
                    (
                        "checkpointError",
                        json!("Server restarted before checkpoint capture finished"),
                    ),
                ],
            )?;
        }
        Ok(())
    }
}

fn same_workspace_activity(current: &WorkspaceSnapshot, expected: &WorkspaceSnapshot) -> bool {
    const AGENT_FIELDS: &[&str] = &[
        "id",
        "cwd",
        "epoch",
        "accountKey",
        "threadId",
        "rootId",
        "lastCompletedTurn",
        "inFlight",
        "workspaceOperation",
        "workspaceReservationId",
        "deletedAt",
        "imageWorkspace",
        "executionMode",
        "workspaceMode",
        "checkpointHistoryHead",
    ];
    current.agent.id == expected.agent.id
        && same_record_fields(&current.agent.record, &expected.agent.record, AGENT_FIELDS)
        && same_parent_boundary(
            current.parent_checkpoint.as_ref(),
            expected.parent_checkpoint.as_ref(),
        )
        && same_record_lists(
            &current.agents,
            &expected.agents,
            &[
                "id",
                "cwd",
                "inFlight",
                "workspaceOperation",
                "workspaceReservationId",
            ],
        )
        && same_record_lists(
            &current.operations,
            &expected.operations,
            &["id", "agent", "phase"],
        )
        && same_record_lists(
            &current.monitors,
            &expected.monitors,
            &["id", "cwd", "status"],
        )
        && same_record_lists(&current.tasks, &expected.tasks, &["id", "agent", "status"])
}

fn same_parent_boundary(
    current: Option<&StoredCheckpointObject>,
    expected: Option<&StoredCheckpointObject>,
) -> bool {
    match (current, expected) {
        (None, None) => true,
        (Some(current), Some(expected)) => {
            current.id == expected.id
                && current.record.get("historyBoundary") == expected.record.get("historyBoundary")
        }
        _ => false,
    }
}

fn same_record_lists(
    current: &[StoredCheckpointObject],
    expected: &[StoredCheckpointObject],
    fields: &[&str],
) -> bool {
    current.len() == expected.len()
        && current.iter().zip(expected).all(|(current, expected)| {
            current.id == expected.id
                && same_record_fields(&current.record, &expected.record, fields)
        })
}

fn same_record_fields(current: &Value, expected: &Value, fields: &[&str]) -> bool {
    fields
        .iter()
        .all(|field| current.get(*field) == expected.get(*field))
}

fn read_all(connection: &Connection, table: &str) -> StoreResult<Vec<StoredCheckpointObject>> {
    if !table_exists(connection, table)? {
        return Ok(Vec::new());
    }
    let mut statement =
        connection.prepare(&format!("SELECT id,record FROM {table} ORDER BY rowid"))?;
    let rows = statement.query_map([], read_stored_row)?;
    collect_records(rows, table)
}

fn read_active(
    connection: &Connection,
    table: &str,
    field: &str,
    values: &[&str],
) -> StoreResult<Vec<StoredCheckpointObject>> {
    if !table_exists(connection, table)? || values.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = std::iter::repeat_n("?", values.len())
        .collect::<Vec<_>>()
        .join(",");
    let mut statement = connection.prepare(&format!(
        "SELECT id,record FROM {table} WHERE json_extract(record,'$.{field}') IN ({placeholders}) ORDER BY rowid"
    ))?;
    let rows = statement.query_map(rusqlite::params_from_iter(values.iter()), read_stored_row)?;
    let records = collect_records(rows, table)?;
    for record in &records {
        match table {
            "runtime_workspace_operations" => {
                validate_active_record(table, record, &["kind", "agent", "phase"])?
            }
            "runtime_tasks" => validate_active_record(table, record, &["agent", "status"])?,
            "runtime_monitors" => validate_active_record(table, record, &["status", "cwd"])?,
            _ => {}
        }
    }
    Ok(records)
}

fn read_object(
    connection: &Connection,
    table: &str,
    id: &str,
) -> StoreResult<Option<StoredCheckpointObject>> {
    let record: Option<Value> = get_json_record(connection, table, id)?;
    record
        .map(|record| stored_object(table, id.to_owned(), record))
        .transpose()
}

fn read_stored_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredCheckpointObject> {
    Ok(StoredCheckpointObject {
        id: row.get(0)?,
        record: decode_json_record(row, 1)?,
    })
}

fn collect_records(
    rows: impl Iterator<Item = rusqlite::Result<StoredCheckpointObject>>,
    table: &str,
) -> StoreResult<Vec<StoredCheckpointObject>> {
    rows.map(|row| {
        let record = row?;
        stored_object(table, record.id, record.record)
    })
    .collect()
}

fn stored_object(table: &str, id: String, record: Value) -> StoreResult<StoredCheckpointObject> {
    if record.get("id").and_then(Value::as_str) != Some(id.as_str()) {
        return Err(invalid_record(table, &id));
    }
    Ok(StoredCheckpointObject { id, record })
}

fn validate_active_record(
    table: &str,
    record: &StoredCheckpointObject,
    fields: &[&str],
) -> StoreResult<()> {
    if fields.iter().any(|field| {
        record
            .record
            .get(*field)
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    }) {
        return Err(invalid_record(table, &record.id));
    }
    Ok(())
}

fn invalid_record(table: &str, id: &str) -> StoreError {
    StoreError::SchemaMismatch(format!("Invalid stored object {table}/{id}"))
}

fn update_object(
    transaction: &Transaction<'_>,
    table: &str,
    id: &str,
    record: &Value,
) -> StoreResult<()> {
    if record.get("id").and_then(Value::as_str) != Some(id) {
        return Err(invalid_record(table, id));
    }
    transaction.execute(
        &format!("UPDATE {table} SET record=?1 WHERE id=?2"),
        params![serde_json::to_string(record)?, id],
    )?;
    Ok(())
}

fn insert_object(
    transaction: &Transaction<'_>,
    table: &str,
    id: &str,
    record: &Value,
) -> StoreResult<()> {
    if record.get("id").and_then(Value::as_str) != Some(id) {
        return Err(invalid_record(table, id));
    }
    transaction.execute(
        &format!("INSERT INTO {table}(id,record) VALUES(?1,?2)"),
        params![id, serde_json::to_string(record)?],
    )?;
    Ok(())
}

fn update_agent(
    transaction: &Transaction<'_>,
    id: &str,
    record: &Value,
    sync_fields: &[(&str, Value)],
) -> StoreResult<()> {
    update_object(transaction, "runtime_agents", id, record)?;
    EventWriteRepository::set_agent_sync_values(transaction, id, sync_fields)
}

fn table_exists(connection: &Connection, table: &str) -> StoreResult<bool> {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |_| Ok(()),
        )
        .optional()
        .map(|value| value.is_some())
        .map_err(StoreError::from)
}

#[cfg(test)]
mod tests {
    use super::{
        CheckpointsRepository, StoredCheckpointObject, WorkspaceSnapshot, same_workspace_activity,
    };
    use rusqlite::Connection;
    use serde_json::json;

    #[test]
    fn workspace_activity_ignores_unrelated_record_metadata() {
        let agent = StoredCheckpointObject {
            id: "agent".to_owned(),
            record: json!({
                "id": "agent",
                "cwd": "/workspace",
                "epoch": 0,
                "accountKey": "default",
                "threadId": "thread",
                "rootId": "root",
                "lastCompletedTurn": "turn",
                "inFlight": false,
                "workspaceOperation": null,
                "workspaceReservationId": null,
                "name": "before",
            }),
        };
        let other_agent = StoredCheckpointObject {
            id: "other".to_owned(),
            record: json!({
                "id": "other",
                "cwd": "/another-workspace",
                "inFlight": false,
                "workspaceOperation": null,
                "workspaceReservationId": null,
                "name": "before",
            }),
        };
        let snapshot = WorkspaceSnapshot {
            agent: agent.clone(),
            parent_checkpoint: None,
            agents: vec![agent, other_agent],
            operations: Vec::new(),
            monitors: Vec::new(),
            tasks: Vec::new(),
        };

        let mut metadata_update = snapshot.clone();
        metadata_update.agent.record["name"] = json!("renamed target agent");
        metadata_update.agents[1].record["name"] = json!("renamed unrelated agent");
        assert!(same_workspace_activity(&metadata_update, &snapshot));

        metadata_update.agent.record["epoch"] = json!(1);
        assert!(!same_workspace_activity(&metadata_update, &snapshot));
    }

    #[test]
    fn recovery_refuses_malformed_active_records_with_table_and_row_id() {
        let mut connection = Connection::open_in_memory().expect("memory database opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_workspace_operations(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 INSERT INTO runtime_workspace_operations VALUES
                 ('bad-owner','{\"id\":\"bad-owner\",\"kind\":\"capture\",\"phase\":\"capture_running\",\"agent\":17}');",
            )
            .expect("malformed active row inserted");
        let transaction = connection
            .transaction()
            .expect("recovery transaction begins");
        let error = CheckpointsRepository::recover_captures(&transaction, 1.0)
            .expect_err("invalid active recovery record is refused");
        assert!(
            error
                .to_string()
                .contains("runtime_workspace_operations/bad-owner")
        );
        assert!(transaction
            .query_row(
                "SELECT json_extract(record,'$.phase') FROM runtime_workspace_operations WHERE id='bad-owner'",
                [],
                |row| row.get::<_, String>(0),
            )
            .is_ok_and(|phase| phase == "capture_running"));
    }
}
