//! Persisted project metadata and its renderer sync projection.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_schema::persistence::ProjectRemovalReason;

use crate::request_results::AdmittedRequest;

use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult, SyncDocumentsRepository};

/// Durable destructive-removal fence. Sequence belongs to the project sync tombstone;
/// deleted_at is Unix milliseconds, independent of transport reply retention.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRemoval {
    pub reason: ProjectRemovalReason,
    pub sequence: i64,
    pub request_id: String,
    pub deleted_at: i64,
}

const PROJECT_TABLE: &[TableRequirement] = &[TableRequirement {
    name: "runtime_projects",
    columns: &["id", "record"],
}];
const SIDEBAR_TABLE: &[TableRequirement] = &[TableRequirement {
    name: "runtime_sidebar_order",
    columns: &["id", "record"],
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
    bootstrap_tables: &[PROJECT_TABLE[0], SIDEBAR_TABLE[0]],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Copy, Debug, Default)]
pub struct ProjectsRepository;

impl ProjectsRepository {
    /// Durable Unix timestamp in seconds marking permanent project-owned room retirement.
    pub const ROOM_RETIRED_FIELD: &'static str = "projectDeletedAt";

    pub fn room_retired(room: &Value) -> bool {
        room.get(Self::ROOM_RETIRED_FIELD)
            .is_some_and(|value| !value.is_null())
    }

    /// Distinguish a destructive project lifetime fence from sidebar-only removal.
    /// The recorded sequence is the exact project tombstone's durable sequence.
    pub fn record_destructive_removal(
        tx: &Transaction<'_>,
        id: &str,
        request_id: &str,
        deleted_at: i64,
    ) -> StoreResult<()> {
        let seq: i64 = tx.query_row(
            "SELECT seq FROM sync_entities WHERE collection='project' AND id=?1 AND deleted=1",
            [id],
            |row| row.get(0),
        )?;
        let removal = ProjectRemoval {
            reason: ProjectRemovalReason::Delete,
            sequence: seq,
            request_id: request_id.to_owned(),
            deleted_at,
        };
        tx.execute("INSERT INTO runtime_projects(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            params![id, json!({"id":id,"removal":removal}).to_string()])?;
        Ok(())
    }
    pub fn is_destructive_removal(
        connection: &Connection,
        id: &str,
        seq: i64,
    ) -> StoreResult<bool> {
        let raw: Option<String> = connection
            .query_row(
                "SELECT json_extract(record,'$.removal') FROM runtime_projects WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let removal: Option<ProjectRemoval> =
            raw.map(|raw| serde_json::from_str(&raw)).transpose()?;
        Ok(removal.is_some_and(|removal| {
            removal.reason == ProjectRemovalReason::Delete && removal.sequence == seq
        }))
    }

    pub fn list(connection: &Connection) -> StoreResult<Vec<(String, Value)>> {
        Ok(Self::owned(connection)?
            .into_iter()
            .filter(|(_, project)| {
                project.get("sidebarHidden").and_then(Value::as_bool) != Some(true)
            })
            .collect())
    }

    /// Active project owners, including projects hidden from navigation.
    pub fn owned(connection: &Connection) -> StoreResult<Vec<(String, Value)>> {
        let mut statement = connection.prepare_cached(
            "SELECT id,record FROM runtime_projects WHERE json_extract(record,'$.removal') IS NULL ORDER BY json_extract(record,'$.created'),id",
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
                "SELECT record FROM runtime_projects WHERE id=?1 AND json_extract(record,'$.removal') IS NULL",
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

    pub fn location_request(
        connection: &Connection,
        project: &str,
        operation: &str,
    ) -> StoreResult<Option<AdmittedRequest>> {
        Self::get(connection, project)?
            .and_then(|(_, value)| {
                value
                    .get("locationIntents")
                    .and_then(|intents| intents.get(operation))
                    .and_then(|intent| intent.get("request"))
                    .cloned()
            })
            .map(serde_json::from_value)
            .transpose()
            .map_err(Into::into)
    }

    pub fn put_location_request(
        tx: &Transaction<'_>,
        project: &str,
        operation: &str,
        request: &AdmittedRequest,
    ) -> StoreResult<()> {
        let (_, mut value) =
            Self::get(tx, project)?.ok_or_else(|| StoreError::InvalidStoredRecord {
                object: "runtime_projects",
                id: project.to_owned(),
            })?;
        if let Some(previous) = Self::location_request(tx, project, operation)?
            && previous != *request
        {
            return Err(StoreError::SchemaMismatch(
                "location request identity has different content".to_owned(),
            ));
        }
        if value.get("locationIntents").is_none() {
            value["locationIntents"] = json!({});
        }
        value["locationIntents"][operation] = json!({"request":request});
        tx.execute(
            "UPDATE runtime_projects SET record=?2 WHERE id=?1",
            params![project, value.to_string()],
        )?;
        Ok(())
    }

    pub fn remove_location_request(
        tx: &Transaction<'_>,
        project: &str,
        operation: &str,
    ) -> StoreResult<()> {
        let Some((_, mut value)) = Self::get(tx, project)? else {
            return Ok(());
        };
        if let Some(intents) = value
            .get_mut("locationIntents")
            .and_then(Value::as_object_mut)
        {
            intents.remove(operation);
        }
        tx.execute(
            "UPDATE runtime_projects SET record=?2 WHERE id=?1",
            params![project, value.to_string()],
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
        let mut statement = connection.prepare_cached("SELECT id,record FROM runtime_agents")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (id, raw) = row?;
                let mut value =
                    serde_json::from_str(&raw).map_err(|_| StoreError::InvalidStoredRecord {
                        object: "runtime_agents",
                        id: id.clone(),
                    })?;
                crate::agent_volatile::merge(connection, &id, &mut value)?;
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
        SyncDocumentsRepository::put_entity(
            transaction,
            "project",
            id,
            project.clone(),
            project.get("sidebarHidden").and_then(Value::as_bool) == Some(true),
        )?;
        Ok(())
    }

    pub fn remove(transaction: &Transaction<'_>, id: &str) -> StoreResult<bool> {
        let Some((_, mut existing)) = Self::get(transaction, id)? else {
            return Ok(false);
        };
        if existing.get("sidebarHidden").and_then(Value::as_bool) == Some(true) {
            return Ok(false);
        }
        existing["sidebarHidden"] = json!(true);
        let removed = transaction.execute(
            "UPDATE runtime_projects SET record=?2 WHERE id=?1",
            params![id, existing.to_string()],
        )? > 0;
        if removed {
            SyncDocumentsRepository::put_entity(transaction, "project", id, json!({}), true)?;
            let mut statement = transaction.prepare_cached(
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
            let room_table: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_rooms')",
                [], |row| row.get(0),
            )?;
            if !room_table {
                return Ok(removed);
            }
            let mut rooms = transaction.prepare_cached(
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
        if Self::room_retired(&room) {
            SyncDocumentsRepository::put_entity(transaction, "room", id, json!({}), true)?;
            return Ok(());
        }
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
                .filter(|(_, project)| {
                    project.get("sidebarHidden").and_then(Value::as_bool) != Some(true)
                })
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
            let mut record =
                serde_json::from_str(&raw).map_err(|_| StoreError::InvalidStoredRecord {
                    object: "runtime_agents",
                    id: id.to_owned(),
                })?;
            crate::agent_volatile::merge(connection, id, &mut record)?;
            Ok((id.to_owned(), record))
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
        let workspace_exists = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sync_entities \
             WHERE collection='workspace' AND id='current' AND deleted=0)",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if workspace_exists {
            SyncDocumentsRepository::patch_entity(
                transaction,
                "workspace",
                "current",
                json!({"sidebarOrder":value}),
            )?;
        }
        Ok(())
    }
}

inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "projects",
        requirements: &REQUIREMENTS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_hide_retains_pending_location_owner() {
        use crate::request_results::{Admission, RequestResults};
        use studio_runtime_schema::persistence::RequestIdentity;
        let mut db = Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,PRIMARY KEY(collection,id));
            CREATE TABLE sync_documents(seq INTEGER);
            CREATE TABLE sync_versions(seq INTEGER);
            CREATE TABLE runtime_orchestration_settings(id TEXT,record TEXT);").expect("schema");
        crate::receipt_schema::create(&db).expect("request schema");
        let tx = db.transaction().expect("transaction");
        ProjectsRepository::put_project(&tx, &json!({"id":"p","locations":[]})).expect("project");
        let root = RequestIdentity::issue_root();
        let identity =
            RequestIdentity::new("project-location", &["server", "ui"], &root, &["location"])
                .expect("identity");
        let now = identity.issued_ms();
        let Admission::Fresh(admitted) =
            RequestResults::admit(&tx, &identity, "signature", now).expect("admit")
        else {
            panic!("fresh request")
        };
        ProjectsRepository::put_location_request(&tx, "p", "operation", &admitted).expect("intent");
        assert!(ProjectsRepository::remove(&tx, "p").expect("hide"));
        assert_eq!(
            ProjectsRepository::location_request(&tx, "p", "operation").expect("intent"),
            Some(admitted)
        );
        assert!(ProjectsRepository::list(&tx).expect("sidebar").is_empty());
        assert_eq!(
            ProjectsRepository::get(&tx, "p")
                .expect("owner")
                .expect("retained")
                .1["sidebarHidden"],
            true
        );
        assert!(!ProjectsRepository::remove(&tx, "p").expect("second hide"));
    }

    #[test]
    fn destructive_fence_survives_sidebar_removal_on_project_owner() {
        let mut db = Connection::open_in_memory().expect("database");
        db.execute_batch(
            "CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,deleted INTEGER);
            CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT,result TEXT);
            INSERT INTO sync_entities VALUES('project','p',17,1);",
        )
        .expect("schema");
        let tx = db.transaction().expect("transaction");
        ProjectsRepository::record_destructive_removal(&tx, "p", "request", 1000).expect("fence");
        assert!(ProjectsRepository::is_destructive_removal(&tx, "p", 17).expect("fence read"));
        assert!(!ProjectsRepository::remove(&tx, "p").expect("sidebar hide"));
        let record: String = tx
            .query_row(
                "SELECT record FROM runtime_projects WHERE id='p'",
                [],
                |row| row.get(0),
            )
            .expect("durable project tombstone");
        let record: Value = serde_json::from_str(&record).expect("project");
        assert_eq!(record["removal"]["reason"], "delete");
        assert_eq!(record["removal"]["sequence"], 17);
        assert!(!ProjectsRepository::is_destructive_removal(&tx, "p", 18).expect("other lifetime"));
        assert!(
            ProjectsRepository::list(&tx)
                .expect("visible projects")
                .is_empty()
        );
    }
}
