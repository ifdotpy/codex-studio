//! Persisted project metadata and its renderer sync projection.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};

use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult, SyncDocumentsRepository};

const PROJECT_TABLE: &[TableRequirement] = &[TableRequirement {
    name: "runtime_projects",
    columns: &["id", "record"],
}];
const SIDEBAR_TABLE: &[TableRequirement] = &[TableRequirement {
    name: "runtime_sidebar_order",
    columns: &["id", "record"],
}];
const RECEIPT_TABLE: &[TableRequirement] = &[TableRequirement {
    name: "runtime_operation_receipts",
    columns: &["id", "signature", "result"],
}];
const AGENT_TABLE: &[TableRequirement] = &[TableRequirement {
    name: "runtime_agents",
    columns: &["id", "record"],
}];
const ROOM_TABLE: &[TableRequirement] = &[TableRequirement {
    name: "runtime_rooms",
    columns: &["id", "record"],
}];

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[
        ConditionalTableRequirement {
            table_exists: "runtime_agents",
            tables: AGENT_TABLE,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_rooms",
            tables: ROOM_TABLE,
        },
    ],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[PROJECT_TABLE[0], SIDEBAR_TABLE[0], RECEIPT_TABLE[0]],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Copy, Debug, Default)]
pub struct ProjectsRepository;

impl ProjectsRepository {
    pub fn list(connection: &Connection) -> StoreResult<Vec<(String, Value)>> {
        let mut statement = connection.prepare(
            "SELECT id,record FROM runtime_projects ORDER BY json_extract(record,'$.created'),id",
        )?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (id, raw) = row?;
                let value =
                    serde_json::from_str(&raw).map_err(|_| StoreError::InvalidStoredRecord {
                        object: "runtime_projects",
                        id: id.clone(),
                    })?;
                Ok((id, value))
            })
            .collect()
    }

    pub fn get(connection: &Connection, id: &str) -> StoreResult<Option<(String, Value)>> {
        let raw = connection
            .query_row(
                "SELECT record FROM runtime_projects WHERE id=?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        raw.map(|raw| {
            Ok((
                id.to_owned(),
                serde_json::from_str(&raw).map_err(|_| StoreError::InvalidStoredRecord {
                    object: "runtime_projects",
                    id: id.to_owned(),
                })?,
            ))
        })
        .transpose()
    }

    pub fn receipt(connection: &Connection, id: &str) -> StoreResult<Option<(String, Value)>> {
        let row = connection
            .query_row(
                "SELECT signature,result FROM runtime_operation_receipts WHERE id=?1",
                [id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        row.map(|(signature, raw)| {
            Ok((
                signature,
                serde_json::from_str(&raw).map_err(|_| StoreError::InvalidStoredRecord {
                    object: "runtime_operation_receipts",
                    id: id.to_owned(),
                })?,
            ))
        })
        .transpose()
    }

    pub fn save_receipt(
        transaction: &Transaction<'_>,
        id: &str,
        signature: &str,
        result: &Value,
    ) -> StoreResult<()> {
        transaction.execute(
            "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES (?1,?2,?3)",
            params![id, signature, serde_json::to_string(result)?],
        )?;
        Ok(())
    }

    pub fn agents(connection: &Connection) -> StoreResult<Vec<(String, Value)>> {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_agents')",
            [], |row| row.get(0),
        )?;
        if !exists {
            return Ok(Vec::new());
        }
        let mut statement = connection.prepare("SELECT id,record FROM runtime_agents")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (id, raw) = row?;
                let value =
                    serde_json::from_str(&raw).map_err(|_| StoreError::InvalidStoredRecord {
                        object: "runtime_agents",
                        id: id.clone(),
                    })?;
                Ok((id, value))
            })
            .collect()
    }

    pub fn put_project(transaction: &Transaction<'_>, project: &Value) -> StoreResult<()> {
        let id = project
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| StoreError::SchemaMismatch("project id is invalid".to_owned()))?;
        let record = serde_json::to_string(project)?;
        transaction.execute(
            "INSERT INTO runtime_projects(id,record) VALUES (?1,?2) \
             ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            params![id, record],
        )?;
        SyncDocumentsRepository::put_entity(transaction, "project", id, project.clone(), false)?;
        Ok(())
    }

    pub fn remove(transaction: &Transaction<'_>, id: &str) -> StoreResult<bool> {
        let existing = Self::get(transaction, id)?;
        if existing.is_none() {
            return Ok(false);
        }
        let removed = transaction.execute("DELETE FROM runtime_projects WHERE id=?1", [id])? > 0;
        if removed {
            SyncDocumentsRepository::put_entity(transaction, "project", id, json!({}), true)?;
            let mut statement = transaction.prepare(
                "SELECT id FROM sync_entities WHERE collection='peerTeam' AND deleted=0 \
                 AND json_extract(payload,'$.value.projectPath')=?1",
            )?;
            let ids = statement
                .query_map([id], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(statement);
            for team_id in ids {
                SyncDocumentsRepository::put_entity(
                    transaction,
                    "peerTeam",
                    &team_id,
                    json!({}),
                    true,
                )?;
            }
            let room_object: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_rooms')",
                [], |row| row.get(0),
            )?;
            if !room_table {
                return Ok(removed);
            }
            let mut rooms = transaction.prepare(
                "SELECT id FROM runtime_rooms WHERE json_extract(record,'$.kind')='private' \
                 AND json_extract(record,'$.projectPath')=?1",
            )?;
            let room_ids = rooms
                .query_map([id], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(rooms);
            for room_id in room_ids {
                Self::refresh_private_room(transaction, &room_id)?;
            }
        }
        Ok(removed)
    }

    pub fn refresh_private_room(transaction: &Transaction<'_>, id: &str) -> StoreResult<()> {
        let room = transaction
            .query_row(
                "SELECT record FROM runtime_rooms WHERE id=?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(raw) = room else {
            return Ok(());
        };
        let mut room: Value =
            serde_json::from_str(&raw).map_err(|_| StoreError::InvalidStoredRecord {
                object: "runtime_rooms",
                id: id.to_owned(),
            })?;
        let members = room
            .get("members")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut live = Vec::new();
        for member in &members {
            let Some(member_id) = member.as_str() else {
                continue;
            };
            if let Some((key, agent)) = Self::agent(transaction, member_id)? {
                if agent.get("id").and_then(Value::as_str) != Some(key.as_str()) {
                    return Err(StoreError::InvalidStoredRecord {
                        object: "runtime_agents",
                        id: key,
                    });
                }
                if agent.get("deletedAt").is_none_or(Value::is_null) {
                    live.push(member.clone());
                }
            }
        }
        if live.is_empty() || live.len() != members.len() {
            SyncDocumentsRepository::put_entity(transaction, "room", id, json!({}), true)?;
            return Ok(());
        }
        room["members"] = Value::Array(live.clone());
        let project_path = room.get("projectPath").and_then(Value::as_str);
        let teams = if let Some(path) = project_path {
            Self::get(transaction, path)?
                .and_then(|(_, project)| {
                    project.get("peerTeams").and_then(Value::as_array).cloned()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if let Some(team) = teams.iter().find(|team| {
            let team_members = team.get("members").and_then(Value::as_array);
            team_members.is_some_and(|members| live.iter().all(|member| members.contains(member)))
        }) {
            room["peerTeamId"] = team.get("id").cloned().unwrap_or(Value::Null);
            room["peerTeamName"] = team.get("name").cloned().unwrap_or(Value::Null);
        } else {
            if let Some(record) = room.as_object_mut() {
                record.remove("peerTeamId");
                record.remove("peerTeamName");
            }
        }
        SyncDocumentsRepository::put_entity(transaction, "room", id, room, false)?;
        Ok(())
    }

    pub fn agent(connection: &Connection, id: &str) -> StoreResult<Option<(String, Value)>> {
        let raw = connection
            .query_row(
                "SELECT record FROM runtime_agents WHERE id=?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        raw.map(|raw| {
            serde_json::from_str(&raw)
                .map(|record| (id.to_owned(), record))
                .map_err(|_| StoreError::InvalidStoredRecord {
                    object: "runtime_agents",
                    id: id.to_owned(),
                })
        })
        .transpose()
    }

    pub fn sidebar_order(connection: &Connection) -> StoreResult<Value> {
        connection
            .query_row(
                "SELECT record FROM runtime_sidebar_order WHERE id='current'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|raw| {
                serde_json::from_str(&raw).map_err(|_| StoreError::InvalidStoredRecord {
                    object: "runtime_sidebar_order",
                    id: "current".to_owned(),
                })
            })
            .transpose()
            .map(|value| value.unwrap_or_else(|| json!({"revision": 0, "groups": null})))
    }

    pub fn put_sidebar_order(transaction: &Transaction<'_>, value: &Value) -> StoreResult<()> {
        transaction.execute(
            "INSERT INTO runtime_sidebar_order(id,record) VALUES ('current',?1) \
             ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            [serde_json::to_string(value)?],
        )?;
        SyncDocumentsRepository::patch_entity(
            transaction,
            "workspace",
            "current",
            json!({"sidebarOrder":value}),
        )?;
        Ok(())
    }
}

inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "projects",
        requirements: &REQUIREMENTS,
    }
}
