use rusqlite::{Connection, OptionalExtension, params};

use crate::StoreResult;
use crate::records::{ChatMessage, ChatMessageId, MessageRepository};
use crate::schema::{SchemaRequirements, TableRequirement};

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[TableRequirement {
        name: "runtime_chat_messages",
        columns: &[
            "seq",
            "id",
            "room",
            "sender",
            "text",
            "created",
            "deliveries",
        ],
    }],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
};

impl MessageRepository {
    pub fn get(connection: &Connection, id: &ChatMessageId) -> StoreResult<Option<ChatMessage>> {
        Ok(connection.query_row(
            "SELECT seq, id, room, sender, text, created, deliveries FROM runtime_chat_messages WHERE id = ?1",
            [&id.0],
            |row| {
                let deliveries: String = row.get(6)?;
                let parsed = serde_json::from_str(&deliveries).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(error))
                })?;
                Ok(ChatMessage {
                    sequence: row.get(0)?, id: ChatMessageId(row.get(1)?), room: row.get(2)?,
                    sender: row.get(3)?, text: row.get(4)?, created: row.get(5)?, deliveries: parsed,
                })
            },
        ).optional()?)
    }

    pub fn put(connection: &Connection, message: &ChatMessage) -> StoreResult<()> {
        connection.execute(
            "INSERT INTO runtime_chat_messages(id, room, sender, text, created, deliveries) VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(id) DO UPDATE SET room=excluded.room, sender=excluded.sender, text=excluded.text, created=excluded.created, deliveries=excluded.deliveries",
            params![message.id.0, message.room, message.sender, message.text, message.created, serde_json::to_string(&message.deliveries)?],
        )?;
        Ok(())
    }
}
