//! Outgoing requests to other servers live in the shared `runtime_outbox` table.
//! The kind is `remote_delivery`, `target` names the server, and the typed payload
//! says which request family owns the row. One id and one signature never change:
//! a retry sends the same request id and the exact stored body.
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use studio_runtime_schema::persistence::{OutboxKind, OutboxStatus, OwnerKind};

use super::outbox::{Outbox, OutboxError, OutboxRow};

/// The request families that share the remote outbox.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteEffect {
    /// Reservation of a paired-server HTTP request (`runtime_access_outbound`).
    AccessRequest,
    /// Reservation of a local Git fetch from a paired server (`runtime_server_fetch`).
    ServerFetch,
    /// A signed request to a paired server (`runtime_server_outbox`). The body keeps its
    /// action, so the restart rule depends on the action (see `multi_server_orchestration`).
    ServerRequest,
    /// A federation chat or room message (`runtime_federation_outbox`).
    FederationMessage,
}

impl RemoteEffect {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AccessRequest => "access_request",
            Self::ServerFetch => "server_fetch",
            Self::ServerRequest => "server_request",
            Self::FederationMessage => "federation_message",
        }
    }

    /// What the worker does with an entry that was in flight at a restart.
    pub const fn restart_rule(self) -> RestartRule {
        match self {
            Self::AccessRequest | Self::ServerFetch => RestartRule::None,
            // The receiver dedupes by request id. Server requests whose action is not
            // replay safe are held by `hold_unclean_server_requests`.
            Self::ServerRequest | Self::FederationMessage => RestartRule::TargetDedupes,
        }
    }
}

/// Restart rule of an effect kind (target architecture, "Command execution").
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartRule {
    /// The target dedupes by id: send again with the same id.
    TargetDedupes,
    /// Ask the target. Found: done. Proven absent: queue again. Else: unknown.
    AskTarget,
    /// The outcome is unknown. Only the user resolves it.
    None,
}

/// The typed payload of every remote entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemotePayload<T> {
    pub effect: RemoteEffect,
    pub record: T,
}

/// One decoded remote entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteEntry<T> {
    pub id: String,
    pub target: String,
    pub request_id: String,
    pub signature: String,
    pub status: OutboxStatus,
    /// Unix milliseconds; `None` for unknown entries.
    pub next_at: Option<i64>,
    pub record: T,
}

pub struct RemoteOutbox;

impl RemoteOutbox {
    pub fn id(effect: RemoteEffect, target: &str, request_id: &str) -> String {
        Outbox::effect_id(
            request_id,
            OutboxKind::RemoteDelivery,
            OwnerKind::Server,
            target,
            &[effect.as_str()],
        )
    }

    /// Insert the entry, or change its state when the identity and signature match.
    /// Another signature for the same id is a conflict.
    #[allow(clippy::too_many_arguments)]
    pub fn put<T: Serialize>(
        tx: &Transaction<'_>,
        effect: RemoteEffect,
        target: &str,
        request_id: &str,
        signature: &str,
        status: OutboxStatus,
        next_at: Option<i64>,
        record: T,
        created: i64,
    ) -> Result<(), OutboxError> {
        let id = Self::id(effect, target, request_id);
        Outbox::put(
            tx,
            &OutboxRow {
                id: id.clone(),
                kind: OutboxKind::RemoteDelivery,
                owner_kind: OwnerKind::Server,
                owner_id: target.to_owned(),
                request_id: Some(request_id.to_owned()),
                signature: signature.to_owned(),
                status,
                account: None,
                stream_id: None,
                generation: None,
                ordinal: None,
                next_at,
                payload: RemotePayload { effect, record },
                created,
            },
        )?;
        tx.execute(
            "UPDATE runtime_outbox SET target=?2 WHERE id=?1",
            params![id, target],
        )?;
        Ok(())
    }

    /// Change the state and record of an existing entry. `false` when the entry or its
    /// signature does not exist.
    #[allow(clippy::too_many_arguments)]
    pub fn update<T: Serialize>(
        tx: &Transaction<'_>,
        effect: RemoteEffect,
        target: &str,
        request_id: &str,
        signature: &str,
        status: OutboxStatus,
        next_at: Option<i64>,
        record: T,
    ) -> Result<bool, OutboxError> {
        let payload = serde_json::to_string(&RemotePayload { effect, record })?;
        Ok(tx.execute(
            "UPDATE runtime_outbox SET status=?3,next_at=?4,payload=?5 WHERE id=?1 AND signature=?2",
            params![
                Self::id(effect, target, request_id),
                signature,
                status,
                next_at,
                payload
            ],
        )? != 0)
    }

    /// The entry of one family for one target and request id.
    pub fn get<T: DeserializeOwned>(
        db: &Connection,
        effect: RemoteEffect,
        target: &str,
        request_id: &str,
    ) -> Result<Option<RemoteEntry<T>>, OutboxError> {
        Self::query(
            db,
            "SELECT id,target,request_id,signature,status,next_at,payload FROM runtime_outbox WHERE id=?1",
            params![Self::id(effect, target, request_id)],
        )
    }

    /// The entry of one family for a request id, whatever its target.
    pub fn find<T: DeserializeOwned>(
        db: &Connection,
        effect: RemoteEffect,
        request_id: &str,
    ) -> Result<Option<RemoteEntry<T>>, OutboxError> {
        Self::query(
            db,
            "SELECT id,target,request_id,signature,status,next_at,payload FROM runtime_outbox \
             WHERE kind=?1 AND request_id=?2 AND json_extract(payload,'$.effect')=?3",
            params![OutboxKind::RemoteDelivery, request_id, effect.as_str()],
        )
    }

    fn query<T: DeserializeOwned>(
        db: &Connection,
        sql: &str,
        args: impl rusqlite::Params,
    ) -> Result<Option<RemoteEntry<T>>, OutboxError> {
        let row = db.query_row(sql, args, Self::raw).optional()?;
        row.map(Self::decode).transpose()
    }

    /// Every entry of one family, optionally of one target, in insertion order.
    pub fn entries<T: DeserializeOwned>(
        db: &Connection,
        effect: RemoteEffect,
        target: Option<&str>,
    ) -> Result<Vec<RemoteEntry<T>>, OutboxError> {
        let mut query = db.prepare_cached(
            "SELECT id,target,request_id,signature,status,next_at,payload FROM runtime_outbox \
             WHERE kind=?1 AND json_extract(payload,'$.effect')=?2 AND (?3 IS NULL OR target=?3) \
             ORDER BY rowid",
        )?;
        let rows = query
            .query_map(
                params![OutboxKind::RemoteDelivery, effect.as_str(), target],
                Self::raw,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(Self::decode).collect()
    }

    #[allow(clippy::type_complexity)]
    fn raw(
        row: &rusqlite::Row<'_>,
    ) -> rusqlite::Result<(
        String,
        Option<String>,
        Option<String>,
        String,
        OutboxStatus,
        Option<i64>,
        String,
    )> {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
        ))
    }

    #[allow(clippy::type_complexity)]
    fn decode<T: DeserializeOwned>(
        row: (
            String,
            Option<String>,
            Option<String>,
            String,
            OutboxStatus,
            Option<i64>,
            String,
        ),
    ) -> Result<RemoteEntry<T>, OutboxError> {
        let (id, target, request_id, signature, status, next_at, payload) = row;
        let payload: RemotePayload<T> = serde_json::from_str(&payload)?;
        Ok(RemoteEntry {
            target: target.ok_or_else(|| OutboxError::Conflict(id.clone()))?,
            request_id: request_id.ok_or_else(|| OutboxError::Conflict(id.clone()))?,
            id,
            signature,
            status,
            next_at,
            record: payload.record,
        })
    }

    /// Change an existing entry, or insert it when it does not exist.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert<T: Serialize>(
        tx: &Transaction<'_>,
        effect: RemoteEffect,
        target: &str,
        request_id: &str,
        signature: &str,
        status: OutboxStatus,
        next_at: Option<i64>,
        record: T,
        created: i64,
    ) -> Result<(), OutboxError> {
        let record = serde_json::to_value(record)?;
        let changed = Self::update(
            tx, effect, target, request_id, signature, status, next_at, &record,
        )?;
        if !changed {
            Self::put(
                tx, effect, target, request_id, signature, status, next_at, record, created,
            )?;
        }
        Ok(())
    }

    /// Remove the entry in the transaction that saves its result.
    pub fn delete(
        tx: &Transaction<'_>,
        effect: RemoteEffect,
        target: &str,
        request_id: &str,
        signature: &str,
    ) -> Result<bool, OutboxError> {
        Outbox::delete_settled(tx, &Self::id(effect, target, request_id), signature)
    }

    /// A restored connection to `target` makes its waiting entries due at once.
    pub fn release_target(
        tx: &Transaction<'_>,
        target: &str,
        now_ms: i64,
    ) -> rusqlite::Result<usize> {
        tx.execute(
            "UPDATE runtime_outbox SET next_at=?3 WHERE kind=?1 AND target=?2 AND status='pending' AND next_at>?3",
            params![OutboxKind::RemoteDelivery, target, now_ms],
        )
    }

    /// After an unclean stop the commit that marked an entry as sent can be lost, so
    /// every pending or submitted entry follows the rule of its family. `TargetDedupes`
    /// entries stay due. Others become unknown and wait for the user.
    pub fn hold_unclean(tx: &Transaction<'_>) -> Result<usize, OutboxError> {
        let rows = {
            let mut query = tx.prepare(
                "SELECT id,json_extract(payload,'$.effect') FROM runtime_outbox \
                 WHERE kind=?1 AND status IN ('pending','submitted')",
            )?;
            query
                .query_map(params![OutboxKind::RemoteDelivery], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut held = 0;
        for (id, effect) in rows {
            // A row of another remote family has no `effect` and keeps its own rules.
            let Some(effect) = effect else {
                continue;
            };
            let effect: RemoteEffect = serde_json::from_value(serde_json::Value::String(effect))?;
            if effect.restart_rule() == RestartRule::TargetDedupes {
                continue;
            }
            held += tx.execute(
                "UPDATE runtime_outbox SET status=?2,next_at=NULL WHERE id=?1",
                params![id, OutboxStatus::Unknown],
            )?;
        }
        Ok(held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn database() -> Connection {
        let db = Connection::open_in_memory().expect("database");
        crate::receipt_schema::create(&db).expect("schema");
        db
    }

    #[test]
    fn same_signature_updates_state_and_another_signature_conflicts() {
        let mut db = database();
        let tx = db.transaction().unwrap();
        let put = |signature: &str, status: OutboxStatus| {
            RemoteOutbox::put(
                &tx,
                RemoteEffect::ServerFetch,
                "peer",
                "request",
                signature,
                status,
                None,
                json!({"state": "preparing"}),
                1,
            )
        };
        put("sig", OutboxStatus::Pending).unwrap();
        put("sig", OutboxStatus::Submitted).unwrap();
        assert!(matches!(
            put("other", OutboxStatus::Pending),
            Err(OutboxError::Conflict(_))
        ));
        let entry = RemoteOutbox::get::<serde_json::Value>(
            &tx,
            RemoteEffect::ServerFetch,
            "peer",
            "request",
        )
        .unwrap()
        .expect("entry");
        assert_eq!(entry.status, OutboxStatus::Submitted);
        assert_eq!(entry.target, "peer");
        let found =
            RemoteOutbox::find::<serde_json::Value>(&tx, RemoteEffect::ServerFetch, "request")
                .unwrap()
                .expect("found without target");
        assert_eq!(found.id, entry.id);
        assert!(
            RemoteOutbox::find::<serde_json::Value>(&tx, RemoteEffect::AccessRequest, "request")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn restored_connection_makes_only_that_target_due() {
        let mut db = database();
        let tx = db.transaction().unwrap();
        for target in ["a", "b"] {
            RemoteOutbox::put(
                &tx,
                RemoteEffect::AccessRequest,
                target,
                "request",
                "sig",
                OutboxStatus::Pending,
                Some(9_000),
                json!({}),
                1,
            )
            .unwrap();
        }
        assert_eq!(RemoteOutbox::release_target(&tx, "a", 100).unwrap(), 1);
        let due = |target: &str| {
            RemoteOutbox::get::<serde_json::Value>(
                &tx,
                RemoteEffect::AccessRequest,
                target,
                "request",
            )
            .unwrap()
            .unwrap()
            .next_at
        };
        assert_eq!(due("a"), Some(100));
        assert_eq!(due("b"), Some(9_000));
    }

    #[test]
    fn unclean_stop_holds_entries_without_a_target_dedupe_rule() {
        let mut db = database();
        let tx = db.transaction().unwrap();
        RemoteOutbox::put(
            &tx,
            RemoteEffect::ServerFetch,
            "peer",
            "request",
            "sig",
            OutboxStatus::Pending,
            Some(5),
            json!({}),
            1,
        )
        .unwrap();
        assert_eq!(RemoteOutbox::hold_unclean(&tx).unwrap(), 1);
        let entry = RemoteOutbox::get::<serde_json::Value>(
            &tx,
            RemoteEffect::ServerFetch,
            "peer",
            "request",
        )
        .unwrap()
        .unwrap();
        assert_eq!(entry.status, OutboxStatus::Unknown);
        assert_eq!(entry.next_at, None);
    }

    #[test]
    fn upsert_inserts_then_changes_and_entries_list_one_family_in_order() {
        let mut db = database();
        let tx = db.transaction().unwrap();
        for (request, text) in [("a", "first"), ("b", "second")] {
            RemoteOutbox::upsert(
                &tx,
                RemoteEffect::FederationMessage,
                "peer",
                request,
                "sig",
                OutboxStatus::Pending,
                Some(1),
                json!({"text": text}),
                1,
            )
            .unwrap();
        }
        // Another upsert changes the record and keeps the identity.
        RemoteOutbox::upsert(
            &tx,
            RemoteEffect::FederationMessage,
            "peer",
            "a",
            "sig",
            OutboxStatus::Committed,
            None,
            json!({"text": "changed"}),
            1,
        )
        .unwrap();
        RemoteOutbox::upsert(
            &tx,
            RemoteEffect::ServerFetch,
            "other",
            "c",
            "sig",
            OutboxStatus::Pending,
            None,
            json!({}),
            1,
        )
        .unwrap();
        let entries = RemoteOutbox::entries::<serde_json::Value>(
            &tx,
            RemoteEffect::FederationMessage,
            Some("peer"),
        )
        .unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|e| e.request_id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(entries[0].record["text"], "changed");
        assert_eq!(entries[0].status, OutboxStatus::Committed);
        assert_eq!(
            RemoteOutbox::entries::<serde_json::Value>(&tx, RemoteEffect::ServerFetch, None)
                .unwrap()
                .len(),
            1
        );
    }
}
