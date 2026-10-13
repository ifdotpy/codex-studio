//! Narrow storage effects for federation; the caller owns the transaction.
use crate::schema::{SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;

const MAX_RECORD_BYTES: usize = 512 * 1024;

#[derive(Clone, Copy, Debug)]
pub enum FederationTable {
    Settings,
    Peer,
    Room,
    Outbox,
    Inbox,
}
impl FederationTable {
    pub fn name(self) -> &'static str {
        match self {
            Self::Settings => "runtime_federation_settings",
            Self::Peer => "runtime_federation_peers",
            Self::Room => "runtime_federation_rooms",
            Self::Outbox => "runtime_federation_outbox",
            Self::Inbox => "runtime_federation_inbox",
        }
    }
    pub fn schema(self) -> &'static str {
        match self {
            Self::Settings => "PersistedFederationSettings",
            Self::Peer => "PersistedFederationPeer",
            Self::Room => "PersistedFederationRoom",
            Self::Outbox => "PersistedFederationOutbox",
            Self::Inbox => "PersistedFederationInbox",
        }
    }
    pub fn validate(self, id: &str, value: &Value) -> StoreResult<()> {
        let valid = studio_runtime_schema::validate(self.schema(), value)
            .map_err(StoreError::SchemaValidation)?;
        let identity = match self {
            Self::Peer => Some("stateId"),
            Self::Room | Self::Outbox => Some("id"),
            _ => None,
        };
        if !valid
            || identity.is_some_and(|field| value.get(field).and_then(Value::as_str) != Some(id))
        {
            return Err(StoreError::InvalidStoredObject {
                table: self.name(),
                id: id.to_owned(),
            });
        }
        Ok(())
    }
}

pub struct FederationMessagingRepository;
impl FederationMessagingRepository {
    pub fn record(
        db: &Connection,
        table: FederationTable,
        id: &str,
        peer: &str,
    ) -> StoreResult<Option<Value>> {
        let sql = match table {
            FederationTable::Inbox => {
                "SELECT record FROM runtime_federation_inbox WHERE message_id=?1 AND peer_id=?2"
                    .to_owned()
            }
            FederationTable::Outbox => {
                "SELECT record FROM runtime_federation_outbox WHERE id=?1 AND peer_id=?2".to_owned()
            }
            _ => format!("SELECT record FROM {} WHERE id=?1 AND ?2=?2", table.name()),
        };
        let sql=sql.replacen("SELECT record", &format!("SELECT CASE WHEN length(CAST(record AS BLOB))<={MAX_RECORD_BYTES} THEN record ELSE NULL END"),1);
        let raw: Option<Option<String>> = db
            .query_row(&sql, params![id, peer], |r| r.get(0))
            .optional()?;
        raw.map(|raw| {
            Self::parse(
                table,
                id,
                &raw.ok_or_else(|| StoreError::InvalidStoredObject {
                    table: table.name(),
                    id: id.into(),
                })?,
            )
        })
        .transpose()
    }
    pub fn parse(table: FederationTable, id: &str, raw: &str) -> StoreResult<Value> {
        if raw.len() > MAX_RECORD_BYTES {
            return Err(StoreError::InvalidStoredObject {
                table: table.name(),
                id: id.into(),
            });
        }
        let value = serde_json::from_str(raw).map_err(|_| StoreError::InvalidStoredObject {
            table: table.name(),
            id: id.into(),
        })?;
        table.validate(id, &value)?;
        Ok(value)
    }
    pub fn put(
        tx: &Transaction<'_>,
        table: FederationTable,
        id: &str,
        peer: &str,
        value: &Value,
    ) -> StoreResult<()> {
        table.validate(id, value)?;
        let raw = crate::python_json_compact(value);
        if raw.len() > MAX_RECORD_BYTES {
            return Err(StoreError::InvalidStoredObject {
                table: table.name(),
                id: id.into(),
            });
        }
        match table {
            FederationTable::Inbox => {
                tx.execute("INSERT INTO runtime_federation_inbox(peer_id,message_id,record) VALUES(?1,?2,?3) ON CONFLICT(peer_id,message_id) DO UPDATE SET record=excluded.record",params![peer,id,raw])?;
            }
            FederationTable::Outbox => {
                tx.execute("INSERT INTO runtime_federation_outbox(id,peer_id,record) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET record=excluded.record WHERE peer_id=excluded.peer_id",params![id,peer,raw])?;
            }
            _ => {
                tx.execute(&format!("INSERT INTO {}(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",table.name()),params![id,raw])?;
            }
        }
        Ok(())
    }
    pub fn batch(
        db: &Connection,
        table: FederationTable,
        peer: &str,
        now: f64,
    ) -> StoreResult<Vec<(String, Value)>> {
        let sql = match table {
            FederationTable::Outbox => {
                "SELECT id,CASE WHEN length(CAST(record AS BLOB))<=?3 THEN record END FROM runtime_federation_outbox WHERE peer_id=?1 AND json_extract(record,'$.status')='queued' AND json_extract(record,'$.nextAt')<=?2 ORDER BY json_extract(record,'$.created'),id LIMIT 50"
            }
            FederationTable::Inbox => {
                "SELECT message_id,CASE WHEN length(CAST(record AS BLOB))<=?3 THEN record END FROM runtime_federation_inbox WHERE peer_id=?1 AND json_extract(record,'$.ack') IN ('pending','sent') AND ?2=?2 ORDER BY json_extract(record,'$.created'),message_id LIMIT 50"
            }
            _ => return Err(StoreError::RecordContract("federation batch table")),
        };
        let mut stmt = db.prepare(sql)?;
        let rows = stmt
            .query_map(params![peer, now, MAX_RECORD_BYTES as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(id, raw)| {
                let raw = raw.ok_or_else(|| StoreError::InvalidStoredObject {
                    table: table.name(),
                    id: id.clone(),
                })?;
                Ok((id.clone(), Self::parse(table, &id, &raw)?))
            })
            .collect()
    }
    pub fn room_rate(db: &Connection, peer: &str, room: &str, now: f64) -> StoreResult<u64> {
        Ok(db.query_row("SELECT count(*) FROM runtime_federation_inbox WHERE peer_id=?1 AND json_extract(record,'$.room')=?2 AND json_extract(record,'$.kind')='chat-message' AND json_extract(record,'$.created')>=?3",params![peer,room,now-60.0],|r|r.get::<_,u32>(0))? as u64)
    }
    pub fn queue_size(db: &Connection, peer: &str) -> StoreResult<(u64, u64)> {
        Ok(db.query_row("SELECT count(*),COALESCE(sum(length(record)),0) FROM runtime_federation_outbox WHERE peer_id=?1 AND json_extract(record,'$.status')='queued'",[peer],|r| Ok((u64::from(r.get::<_,u32>(0)?),u64::from(r.get::<_,u32>(1)?))))?)
    }
}

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[TableRequirement {
        name: "runtime_federation_inbox",
        columns: &["peer_id", "message_id", "record"],
    }],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};
inventory::submit! { crate::repositories::SchemaRegistration { repository:"federation_messaging",requirements:&REQUIREMENTS } }

/// Read-only coverage for every federation record consumed by this subsystem.
pub fn check_state(db: &Connection) -> StoreResult<Vec<crate::StateCheckTable>> {
    let mut result = Vec::new();
    for table in [
        FederationTable::Settings,
        FederationTable::Peer,
        FederationTable::Room,
        FederationTable::Outbox,
        FederationTable::Inbox,
    ] {
        let exists: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table.name()],
            |r| r.get(0),
        )?;
        let mut row_count = 0;
        let mut rejected_count = 0;
        if exists {
            let key = if matches!(table, FederationTable::Inbox) {
                "message_id"
            } else {
                "id"
            };
            let mut stmt = db.prepare(&format!("SELECT {key},record FROM {}", table.name()))?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                row_count += 1;
                let id: String = row.get(0)?;
                let raw: String = row.get(1)?;
                if FederationMessagingRepository::parse(table, &id, &raw).is_err() {
                    rejected_count += 1;
                }
            }
        }
        result.push(crate::StateCheckTable {
            table: table.name(),
            row_count,
            rejected_count,
            failures: if rejected_count == 0 {
                vec![]
            } else {
                vec![crate::StateCheckFailure {
                    pointer: "/".into(),
                    keyword: "schema".into(),
                    count: rejected_count,
                }]
            },
        });
    }
    Ok(result)
}
