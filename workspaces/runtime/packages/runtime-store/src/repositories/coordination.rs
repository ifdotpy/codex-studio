//! Durable Canvas chat, edge and message rows.

use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::json;

use crate::StoreResult;
use crate::repositories::sync_documents::SyncDocumentsRepository;
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};

const GROUPS: &[TableRequirement] = &[TableRequirement {
    name: "groups",
    columns: &["id", "name", "members"],
}];
const EDGES: &[TableRequirement] = &[TableRequirement {
    name: "graph_edges",
    columns: &["id", "source", "target", "kind"],
}];
const MESSAGES: &[TableRequirement] = &[TableRequirement {
    name: "messages",
    columns: &["id", "room", "author", "text", "at", "deliveries"],
}];

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[
        ConditionalTableRequirement {
            table_exists: "groups",
            tables: GROUPS,
        },
        ConditionalTableRequirement {
            table_exists: "graph_edges",
            tables: EDGES,
        },
        ConditionalTableRequirement {
            table_exists: "messages",
            tables: MESSAGES,
        },
    ],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatGroup {
    pub id: String,
    pub name: String,
    pub members: Vec<String>,
    pub original_members: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CanvasMessage {
    pub id: String,
    pub room: String,
    pub author: String,
    pub text: String,
    pub at: f64,
    pub deliveries: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoricalParticipant {
    pub id: String,
    pub author: String,
    pub deliveries: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CoordinationRepository;

impl CoordinationRepository {
    pub fn edge_id(source: &str, target: &str) -> String {
        let encoded = format!(
            "[\"chat\", {}, {}]",
            python_string(source),
            python_string(target)
        );
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(encoded.as_bytes());
        data_encoding::HEXLOWER.encode(&digest[..12])
    }

    pub fn group(connection: &rusqlite::Connection, id: &str) -> StoreResult<Option<ChatGroup>> {
        let row = connection
            .query_row(
                "SELECT id,name,members FROM groups WHERE id=?1",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(id, name, original_members)| {
            Ok(ChatGroup {
                members: members(connection, &id)?,
                id,
                name,
                original_members,
            })
        })
        .transpose()
    }

    pub fn agents(connection: &rusqlite::Connection) -> StoreResult<Vec<(String, String, bool)>> {
        let mut result = Vec::new();
        let mut statement = connection.prepare("SELECT id,record FROM runtime_agents")?;
        for item in statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, false))
        })? {
            result.push(item?);
        }
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='graph_agents')",
            [],
            |row| row.get(0),
        )?;
        if exists {
            let mut statement = connection.prepare("SELECT id,record FROM graph_agents")?;
            for item in statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, true))
            })? {
                result.push(item?);
            }
        }
        Ok(result)
    }

    pub fn agent_exists(connection: &rusqlite::Connection, id: &str) -> StoreResult<bool> {
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_agents WHERE id=?1)",
            [id],
            |row| row.get(0),
        )?)
    }

    pub fn federated_room_exists(connection: &rusqlite::Connection, id: &str) -> StoreResult<bool> {
        if !id.starts_with("federated:") {
            return Ok(false);
        }
        let table: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_federation_rooms')",
            [],
            |row| row.get(0),
        )?;
        if !table {
            return Ok(false);
        }
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_federation_rooms WHERE id=?1)",
            [id],
            |row| row.get(0),
        )?)
    }

    pub fn messages(
        connection: &rusqlite::Connection,
        room: &str,
    ) -> StoreResult<Vec<CanvasMessage>> {
        let mut statement = connection.prepare("SELECT id,room,author,text,at,deliveries FROM messages WHERE room=?1 ORDER BY at DESC LIMIT 200")?;
        let rows = statement.query_map([room], |row| {
            Ok(CanvasMessage {
                id: row.get(0)?,
                room: row.get(1)?,
                author: row.get(2)?,
                text: row.get(3)?,
                at: row.get(4)?,
                deliveries: row.get(5)?,
            })
        })?;
        let mut values = rows.collect::<Result<Vec<_>, _>>()?;
        values.reverse();
        Ok(values)
    }

    pub fn participants(
        connection: &rusqlite::Connection,
        room: &str,
    ) -> StoreResult<Vec<HistoricalParticipant>> {
        let mut statement =
            connection.prepare("SELECT id,author,deliveries FROM messages WHERE room=?1")?;
        let rows = statement.query_map([room], |row| {
            Ok(HistoricalParticipant {
                id: row.get(0)?,
                author: row.get(1)?,
                deliveries: row.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn message(
        connection: &rusqlite::Connection,
        id: &str,
    ) -> StoreResult<Option<CanvasMessage>> {
        Ok(connection
            .query_row(
                "SELECT id,room,author,text,at,deliveries FROM messages WHERE id=?1",
                [id],
                |row| {
                    Ok(CanvasMessage {
                        id: row.get(0)?,
                        room: row.get(1)?,
                        author: row.get(2)?,
                        text: row.get(3)?,
                        at: row.get(4)?,
                        deliveries: row.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn put_group(transaction: &Transaction<'_>, group: &ChatGroup) -> StoreResult<()> {
        transaction.execute(
            "INSERT INTO groups(id,name,members) VALUES(?1,?2,?3)",
            params![group.id, group.name, group.original_members],
        )?;
        Ok(())
    }

    pub fn put_edge(
        transaction: &Transaction<'_>,
        id: &str,
        source: &str,
        target: &str,
    ) -> StoreResult<()> {
        transaction.execute(
            "INSERT OR IGNORE INTO graph_edges(id,source,target,kind) VALUES(?1,?2,?3,'chat')",
            params![id, source, target],
        )?;
        Ok(())
    }

    pub fn remove_edge(transaction: &Transaction<'_>, id: &str) -> StoreResult<()> {
        transaction.execute("DELETE FROM graph_edges WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn put_message(transaction: &Transaction<'_>, message: &CanvasMessage) -> StoreResult<()> {
        transaction.execute(
            "INSERT INTO messages(id,room,author,text,at,deliveries) VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                message.id,
                message.room,
                message.author,
                message.text,
                message.at,
                message.deliveries
            ],
        )?;
        Ok(())
    }

    pub fn chat_projection(
        transaction: &Transaction<'_>,
        id: &str,
        deleted: bool,
    ) -> StoreResult<()> {
        let value = if deleted {
            json!({})
        } else {
            let group = Self::group(transaction, id)?.ok_or_else(|| {
                crate::StoreError::SchemaMismatch(
                    "chat group disappeared during projection".to_owned(),
                )
            })?;
            let members = members(transaction, id)?;
            let last = transaction
                .query_row(
                    "SELECT text,at FROM messages WHERE room=?1 ORDER BY at DESC LIMIT 1",
                    [id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)),
                )
                .optional()?;
            let count: i64 = transaction.query_row(
                "SELECT count(*) FROM messages WHERE room=?1",
                [id],
                |row| row.get(0),
            )?;
            let tail = last
                .as_ref()
                .map(|item| item.0.chars().take(2_000).collect::<String>())
                .unwrap_or_default();
            json!({"id":id,"name":group.name,"members":members,"kind":"chat","messageCount":count,"tail":tail,"lastMessageAt":last.map(|item| item.1)})
        };
        SyncDocumentsRepository::put_entity(transaction, "chat", id, value, deleted)?;
        Ok(())
    }

    pub fn edge_projection(
        transaction: &Transaction<'_>,
        id: &str,
        source: &str,
        target: &str,
        deleted: bool,
    ) -> StoreResult<()> {
        let value = json!({"id":id,"source":source,"target":target,"kind":"chat"});
        SyncDocumentsRepository::put_entity(transaction, "edge", id, value, deleted)?;
        Ok(())
    }
}

fn members(connection: &rusqlite::Connection, id: &str) -> StoreResult<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT source FROM graph_edges WHERE target=?1 AND kind='chat' ORDER BY source",
    )?;
    let rows = statement.query_map([id], |row| row.get(0))?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn python_string(value: &str) -> String {
    let mut result = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\u{8}' => result.push_str("\\b"),
            '\u{c}' => result.push_str("\\f"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            c if c < ' ' => result.push_str(&format!("\\u{:04x}", c as u32)),
            c => result.push(c),
        }
    }
    result.push('"');
    result
}
