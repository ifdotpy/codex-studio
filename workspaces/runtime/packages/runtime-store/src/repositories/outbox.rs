//! Family-typed outstanding obligations. Unknown is never a send selector.
use crate::CachedSql;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::{Serialize, de::DeserializeOwned};
use studio_runtime_schema::persistence::{OutboxKind, OutboxStatus, OwnerKind};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxRow<P> {
    pub id: String,
    pub kind: OutboxKind,
    pub owner_kind: OwnerKind,
    pub owner_id: String,
    pub request_id: Option<String>,
    pub signature: String,
    pub status: OutboxStatus,
    pub account: Option<String>,
    pub stream_id: Option<String>,
    pub generation: Option<i64>,
    pub ordinal: Option<i64>,
    /// Unix milliseconds; Unknown and Unrecognized always have None.
    pub next_at: Option<i64>,
    pub payload: P,
    /// Unix milliseconds. Unresolved obligations have no response TTL.
    pub created: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum OutboxError {
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("outbox id {0} has another signature or owner")]
    Conflict(String),
}
type Result<T> = std::result::Result<T, OutboxError>;

pub struct Outbox;
impl Outbox {
    /// Include the exact effect discriminator: one request can owe multiple effects.
    pub fn effect_id(
        request_id: &str,
        kind: OutboxKind,
        owner: OwnerKind,
        owner_id: &str,
        discriminator: &[&str],
    ) -> String {
        // Strings and closed enums always serialize to a canonical tuple.
        serde_json::to_string(&(request_id, kind, owner, owner_id, discriminator))
            .expect("effect identity serializes")
    }

    pub fn put<P: Serialize>(tx: &Transaction<'_>, row: &OutboxRow<P>) -> Result<()> {
        let changed = tx.execute_cached("INSERT INTO runtime_outbox(id,kind,owner_kind,owner_id,request_id,signature,status,account,stream_id,generation,ordinal,next_at,payload,created) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14) ON CONFLICT(id) DO UPDATE SET status=excluded.status,next_at=excluded.next_at,payload=excluded.payload WHERE kind=excluded.kind AND owner_kind=excluded.owner_kind AND owner_id=excluded.owner_id AND request_id IS excluded.request_id AND signature=excluded.signature AND account IS excluded.account AND stream_id IS excluded.stream_id AND generation IS excluded.generation AND ordinal IS excluded.ordinal AND created=excluded.created", params![row.id,row.kind,row.owner_kind,row.owner_id,row.request_id,row.signature,row.status,row.account,row.stream_id,row.generation,row.ordinal,row.next_at,serde_json::to_string(&row.payload)?,row.created])?;
        if changed == 0 {
            return Err(OutboxError::Conflict(row.id.clone()));
        }
        Ok(())
    }

    fn decode<P: DeserializeOwned>(row: &Row<'_>) -> rusqlite::Result<OutboxRow<P>> {
        let payload: String = row.get(12)?;
        let payload = serde_json::from_str(&payload).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                12,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        Ok(OutboxRow {
            id: row.get(0)?,
            kind: row.get(1)?,
            owner_kind: row.get(2)?,
            owner_id: row.get(3)?,
            request_id: row.get(4)?,
            signature: row.get(5)?,
            status: row.get(6)?,
            account: row.get(7)?,
            stream_id: row.get(8)?,
            generation: row.get(9)?,
            ordinal: row.get(10)?,
            next_at: row.get(11)?,
            payload,
            created: row.get(13)?,
        })
    }

    pub fn get<P: DeserializeOwned>(tx: &Connection, id: &str) -> Result<Option<OutboxRow<P>>> {
        Ok(tx.query_row("SELECT id,kind,owner_kind,owner_id,request_id,signature,status,account,stream_id,generation,ordinal,next_at,payload,created FROM runtime_outbox WHERE id=?1", [id], Self::decode).optional()?)
    }

    pub fn pending<P: DeserializeOwned>(
        tx: &Connection,
        kind: OutboxKind,
        now_ms: i64,
        limit: u32,
    ) -> Result<Vec<OutboxRow<P>>> {
        let mut query = tx.prepare_cached("SELECT id,kind,owner_kind,owner_id,request_id,signature,status,account,stream_id,generation,ordinal,next_at,payload,created FROM runtime_outbox WHERE kind=?1 AND status='pending' AND next_at<=?2 ORDER BY next_at,id LIMIT ?3")?;
        Ok(query
            .query_map(params![kind, now_ms, limit], Self::decode)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn for_owner<P: DeserializeOwned>(
        tx: &Connection,
        owner: OwnerKind,
        id: &str,
        kind: OutboxKind,
    ) -> Result<Vec<OutboxRow<P>>> {
        let mut query = tx.prepare_cached("SELECT id,kind,owner_kind,owner_id,request_id,signature,status,account,stream_id,generation,ordinal,next_at,payload,created FROM runtime_outbox WHERE owner_kind=?1 AND owner_id=?2 AND kind=?3 ORDER BY id")?;
        query
            .query_map(params![owner, id, kind], Self::decode)?
            .collect::<rusqlite::Result<_>>()
            .map_err(|error| {
                if let rusqlite::Error::FromSqlConversionFailure(index, ty, source) = error {
                    match source.downcast::<serde_json::Error>() {
                        Ok(error) => OutboxError::Json(*error),
                        Err(source) => OutboxError::Sql(rusqlite::Error::FromSqlConversionFailure(
                            index, ty, source,
                        )),
                    }
                } else {
                    OutboxError::Sql(error)
                }
            })
    }

    pub fn for_account<P: DeserializeOwned>(
        tx: &Connection,
        account: &str,
        kind: OutboxKind,
        status: OutboxStatus,
        limit: u32,
    ) -> Result<Vec<OutboxRow<P>>> {
        let mut query=tx.prepare_cached("SELECT id,kind,owner_kind,owner_id,request_id,signature,status,account,stream_id,generation,ordinal,next_at,payload,created FROM runtime_outbox WHERE account=?1 AND kind=?2 AND status=?3 ORDER BY ordinal,id LIMIT ?4")?;
        Ok(query
            .query_map(params![account, kind, status, limit], Self::decode)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn for_stream<P: DeserializeOwned>(
        tx: &Connection,
        stream: &str,
        generation: i64,
        limit: u32,
    ) -> Result<Vec<OutboxRow<P>>> {
        let mut query=tx.prepare_cached("SELECT id,kind,owner_kind,owner_id,request_id,signature,status,account,stream_id,generation,ordinal,next_at,payload,created FROM runtime_outbox WHERE stream_id=?1 AND generation=?2 ORDER BY ordinal,id LIMIT ?3")?;
        Ok(query
            .query_map(params![stream, generation, limit], Self::decode)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn for_request<P: DeserializeOwned>(
        tx: &Connection,
        request: &str,
    ) -> Result<Vec<OutboxRow<P>>> {
        let mut query=tx.prepare_cached("SELECT id,kind,owner_kind,owner_id,request_id,signature,status,account,stream_id,generation,ordinal,next_at,payload,created FROM runtime_outbox WHERE request_id=?1 ORDER BY id")?;
        Ok(query
            .query_map([request], Self::decode)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Filter heterogeneous request effects before decoding the family's payload.
    pub fn for_request_kind<P: DeserializeOwned>(
        tx: &Connection,
        request: &str,
        kind: OutboxKind,
    ) -> Result<Vec<OutboxRow<P>>> {
        let mut query=tx.prepare_cached("SELECT id,kind,owner_kind,owner_id,request_id,signature,status,account,stream_id,generation,ordinal,next_at,payload,created FROM runtime_outbox WHERE request_id=?1 AND kind=?2 ORDER BY id")?;
        Ok(query
            .query_map(params![request, kind], Self::decode)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Call only in the transaction committing delivery/settlement evidence to the owner.
    pub fn delete_settled(tx: &Transaction<'_>, id: &str, signature: &str) -> Result<bool> {
        Ok(tx.execute(
            "DELETE FROM runtime_outbox WHERE id=?1 AND signature=?2",
            params![id, signature],
        )? != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    struct Input {
        text: String,
    }
    #[test]
    fn receipt_outbox_typed_selectors_hold_unknown_and_distinguish_effects() {
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        crate::receipt_schema::create(&db).expect("schema");
        let tx = db.transaction().expect("tx");
        let mut row = OutboxRow {
            id: Outbox::effect_id(
                "request",
                OutboxKind::MonitorInput,
                OwnerKind::Monitor,
                "monitor",
                &["stdin", "1"],
            ),
            kind: OutboxKind::MonitorInput,
            owner_kind: OwnerKind::Monitor,
            owner_id: "monitor".into(),
            request_id: Some("request".into()),
            signature: "signature".into(),
            status: OutboxStatus::Pending,
            account: Some("account".into()),
            stream_id: Some("stream".into()),
            generation: Some(1),
            ordinal: Some(1),
            next_at: Some(10),
            payload: Input {
                text: "exact input".into(),
            },
            created: 1,
        };
        Outbox::put(&tx, &row).expect("admit");
        let cached_statement = || {
            // SAFETY: the transaction holds the connection and the statement
            // is inspected while owned by SQLite's connection cache.
            #[allow(unsafe_code)]
            unsafe {
                let db = tx.handle();
                let mut stmt = rusqlite::ffi::sqlite3_next_stmt(db, std::ptr::null_mut());
                while !stmt.is_null() {
                    let sql = std::ffi::CStr::from_ptr(rusqlite::ffi::sqlite3_sql(stmt));
                    if sql.to_bytes().starts_with(b"INSERT INTO runtime_outbox(") {
                        return Some(stmt as usize);
                    }
                    stmt = rusqlite::ffi::sqlite3_next_stmt(db, stmt);
                }
                None
            }
        };
        let first = cached_statement().expect("outbox writer retains its prepared statement");
        Outbox::put(&tx, &row).expect("repeat exact write");
        assert_eq!(cached_statement(), Some(first), "same statement is reused");
        assert_eq!(
            Outbox::get::<Input>(&tx, &row.id).expect("get"),
            Some(row.clone())
        );
        assert_eq!(
            Outbox::pending::<Input>(&tx, row.kind, 10, 10).expect("due"),
            vec![row.clone()]
        );
        assert_eq!(
            Outbox::for_owner::<Input>(&tx, row.owner_kind, &row.owner_id, row.kind)
                .expect("owner"),
            vec![row.clone()]
        );
        assert_eq!(
            Outbox::for_account::<Input>(&tx, "account", row.kind, row.status, 10)
                .expect("account"),
            vec![row.clone()]
        );
        assert_eq!(
            Outbox::for_stream::<Input>(&tx, "stream", 1, 10).expect("stream"),
            vec![row.clone()]
        );
        assert_eq!(
            Outbox::for_request::<Input>(&tx, "request").expect("request"),
            vec![row.clone()]
        );
        tx.execute("INSERT INTO runtime_outbox SELECT id||'-other',?1,owner_kind,owner_id,request_id,signature,status,account,stream_id,generation,ordinal,next_at,'5',created FROM runtime_outbox WHERE id=?2",params![OutboxKind::WorkVm,row.id]).expect("heterogeneous sibling effect");
        assert!(Outbox::for_request::<Input>(&tx, "request").is_err());
        assert_eq!(
            Outbox::for_request_kind::<Input>(&tx, "request", row.kind)
                .expect("filter before family decode"),
            vec![row.clone()]
        );
        let second = Outbox::effect_id(
            "request",
            row.kind,
            row.owner_kind,
            &row.owner_id,
            &["stdin", "2"],
        );
        assert_ne!(row.id, second);
        row.signature = "changed".into();
        assert!(matches!(
            Outbox::put(&tx, &row),
            Err(OutboxError::Conflict(_))
        ));
        row.signature = "signature".into();
        row.status = OutboxStatus::Unknown;
        assert!(Outbox::put(&tx, &row).is_err());
        row.next_at = None;
        Outbox::put(&tx, &row).expect("hold unknown");
        assert!(
            Outbox::pending::<Input>(&tx, row.kind, i64::MAX, 10)
                .expect("never resend unknown")
                .is_empty()
        );
        assert!(!Outbox::delete_settled(&tx, &row.id, "changed").expect("exact identity"));
        assert!(Outbox::delete_settled(&tx, &row.id, "signature").expect("settled"));
    }
}
#[derive(Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PublicationClass {
    #[serde(rename = "token-rate")]
    TokenRate,
    #[serde(rename = "invalidation")]
    Invalidation,
    #[default]
    #[serde(rename = "pending")]
    Pending,
}
impl PublicationClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TokenRate => "token-rate",
            Self::Invalidation => "invalidation",
            Self::Pending => "pending",
        }
    }
}

#[derive(PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(
    rename_all = "camelCase",
    bound(deserialize = "T: serde::Deserialize<'de>")
)]
pub struct Publication<T = Box<serde_json::value::RawValue>> {
    pub account: String,
    pub sequence: u64,
    pub index: usize,
    #[serde(default)]
    pub class: PublicationClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<T>,
    #[serde(default)]
    pub coalesced_effects: Vec<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analytics_overflow: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication_overflow: Option<u64>,
    #[serde(default)]
    pub state_invalidation: bool,
    #[serde(default)]
    pub state_invalidation_generation: u64,
    #[serde(default)]
    pub token_rate_coalesced: bool,
}

impl<T> Default for Publication<T> {
    fn default() -> Self {
        Self {
            account: String::new(),
            sequence: 0,
            index: 0,
            class: PublicationClass::default(),
            effect: None,
            coalesced_effects: Vec::new(),
            analytics_overflow: Some(0),
            publication_overflow: Some(0),
            state_invalidation: false,
            state_invalidation_generation: 0,
            token_rate_coalesced: false,
        }
    }
}
