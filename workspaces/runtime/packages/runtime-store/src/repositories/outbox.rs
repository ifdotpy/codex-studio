//! Family-typed outstanding obligations. Unknown is never a send selector.
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
        let changed = tx.execute("INSERT INTO runtime_outbox(id,kind,owner_kind,owner_id,request_id,signature,status,account,stream_id,generation,ordinal,next_at,payload,created) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14) ON CONFLICT(id) DO UPDATE SET status=excluded.status,next_at=excluded.next_at,payload=excluded.payload WHERE kind=excluded.kind AND owner_kind=excluded.owner_kind AND owner_id=excluded.owner_id AND request_id IS excluded.request_id AND signature=excluded.signature AND account IS excluded.account AND stream_id IS excluded.stream_id AND generation IS excluded.generation AND ordinal IS excluded.ordinal AND created=excluded.created", params![row.id,row.kind,row.owner_kind,row.owner_id,row.request_id,row.signature,row.status,row.account,row.stream_id,row.generation,row.ordinal,row.next_at,serde_json::to_string(&row.payload)?,row.created])?;
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
            .map_err(|error| match error {
                rusqlite::Error::FromSqlConversionFailure(_, _, source)
                    if source.is::<serde_json::Error>() => OutboxError::Json(
                        *source.downcast::<serde_json::Error>().expect("checked error type"),
                    ),
                error => OutboxError::Sql(error),
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
