//! Persistence primitives shared by the remote federation and multi-server
//! transports. This module only uses tables already owned by the Python schema.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;

use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};

const ACCESS_NONCES: &[TableRequirement] = &[TableRequirement {
    name: "runtime_access_nonces",
    columns: &["client", "nonce", "created"],
}];
const ACCESS_OUTBOUND: &[TableRequirement] = &[TableRequirement {
    name: "runtime_access_outbound",
    columns: &["server", "request_id", "hash", "record"],
}];
const FEDERATION_IDENTITY: &[TableRequirement] = &[TableRequirement {
    name: "runtime_federation_identity",
    columns: &["id", "record"],
}];
const FEDERATION_NONCES: &[TableRequirement] = &[TableRequirement {
    name: "runtime_federation_nonces",
    columns: &["peer_id", "nonce", "created"],
}];
const FEDERATION_OUTBOX: &[TableRequirement] = &[TableRequirement {
    name: "runtime_federation_outbox",
    columns: &["id", "peer_id", "record"],
}];

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    conditional_tables: &[
        ConditionalTableRequirement {
            table_exists: "runtime_access_nonces",
            tables: ACCESS_NONCES,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_access_outbound",
            tables: ACCESS_OUTBOUND,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_federation_identity",
            tables: FEDERATION_IDENTITY,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_federation_nonces",
            tables: FEDERATION_NONCES,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_federation_outbox",
            tables: FEDERATION_OUTBOX,
        },
    ],
    tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

/// Outcome of reserving a stable multi-server request identity.
pub enum OutboundReservation {
    New,
    Unknown,
    Complete(Value),
}

pub struct RemoteTransportRepository;

impl RemoteTransportRepository {
    pub fn federation_identity(connection: &Connection) -> StoreResult<Option<String>> {
        Ok(connection
            .query_row(
                "SELECT record FROM runtime_federation_identity WHERE id='local'",
                [],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Consume a federation nonce as part of the caller's immediate transaction.
    pub fn consume_federation_nonce(
        transaction: &Transaction<'_>,
        peer_id: &str,
        nonce: &str,
        now: i64,
    ) -> StoreResult<bool> {
        consume_nonce(
            transaction,
            "runtime_federation_nonces",
            "peer_id",
            peer_id,
            nonce,
            now,
        )
    }

    /// Consume a multi-server nonce as part of the caller's immediate transaction.
    pub fn consume_access_nonce(
        transaction: &Transaction<'_>,
        client_id: &str,
        nonce: &str,
        now: i64,
    ) -> StoreResult<bool> {
        consume_nonce(
            transaction,
            "runtime_access_nonces",
            "client",
            client_id,
            nonce,
            now,
        )
    }

    /// Reserve or recover an outbound request without changing its identity.
    pub fn reserve_multi_server_request(
        transaction: &Transaction<'_>,
        server_id: &str,
        request_id: &str,
        fingerprint: &str,
        created_at: f64,
    ) -> Result<OutboundReservation, StoreError> {
        let existing = transaction
            .query_row(
                "SELECT hash,record FROM runtime_access_outbound WHERE server=?1 AND request_id=?2",
                params![server_id, request_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((saved_fingerprint, record)) = existing {
            if saved_fingerprint != fingerprint {
                return Err(StoreError::SchemaMismatch(
                    "remote request identity conflicts with stored content".to_owned(),
                ));
            }
            let record: Value = serde_json::from_str(&record)?;
            return if record.get("outcome").and_then(Value::as_str) == Some("complete") {
                let value = record.get("value").cloned().ok_or_else(|| {
                    StoreError::SchemaMismatch("remote receipt is incomplete".to_owned())
                })?;
                Ok(OutboundReservation::Complete(value))
            } else {
                Ok(OutboundReservation::Unknown)
            };
        }
        let record = serde_json::json!({"outcome":"unknown","created":created_at});
        transaction.execute(
            "INSERT INTO runtime_access_outbound(server,request_id,hash,record) VALUES(?1,?2,?3,?4)",
            params![server_id, request_id, fingerprint, record.to_string()],
        )?;
        Ok(OutboundReservation::New)
    }

    /// Complete a previously reserved request, retaining its original key.
    pub fn complete_multi_server_request(
        transaction: &Transaction<'_>,
        server_id: &str,
        request_id: &str,
        fingerprint: &str,
        value: &Value,
        completed_at: f64,
    ) -> Result<(), StoreError> {
        let record = serde_json::json!({
            "outcome":"complete",
            "value":value,
            "completed":completed_at
        });
        let changed = transaction.execute(
            "UPDATE runtime_access_outbound SET record=?1 \
             WHERE server=?2 AND request_id=?3 AND hash=?4 \
             AND json_extract(record,'$.outcome')='unknown'",
            params![record.to_string(), server_id, request_id, fingerprint],
        )?;
        if changed != 1 {
            return Err(StoreError::SchemaMismatch(
                "remote request receipt has no matching reservation".to_owned(),
            ));
        }
        Ok(())
    }
}

fn consume_nonce(
    transaction: &Transaction<'_>,
    table: &str,
    principal_column: &str,
    principal: &str,
    nonce: &str,
    now: i64,
) -> StoreResult<bool> {
    let stale_before = now.saturating_sub(600);
    let cleanup = format!("DELETE FROM {table} WHERE created < ?1");
    transaction.execute(&cleanup, [stale_before])?;
    let query = format!("SELECT count(*) FROM {table} WHERE {principal_column}=?1 AND created>=?2");
    let recent: i64 =
        transaction.query_row(&query, params![principal, now.saturating_sub(60)], |row| {
            row.get(0)
        })?;
    if recent >= 240 {
        return Ok(false);
    }
    let insert =
        format!("INSERT OR IGNORE INTO {table}({principal_column},nonce,created) VALUES(?1,?2,?3)");
    Ok(transaction.execute(&insert, params![principal, nonce, now])? == 1)
}

#[cfg(test)]
mod tests {
    use super::{OutboundReservation, RemoteTransportRepository};
    use rusqlite::Connection;
    use serde_json::json;

    fn nonce_db() -> Connection {
        let db = Connection::open_in_memory().expect("in-memory SQLite opens");
        db.execute_batch(
            "CREATE TABLE runtime_federation_nonces(peer_id TEXT NOT NULL, nonce TEXT NOT NULL, created INTEGER NOT NULL, PRIMARY KEY(peer_id,nonce));
             CREATE TABLE runtime_access_nonces(client TEXT NOT NULL, nonce TEXT NOT NULL, created INTEGER NOT NULL, PRIMARY KEY(client,nonce));
             CREATE TABLE runtime_access_outbound(server TEXT NOT NULL, request_id TEXT NOT NULL, hash TEXT NOT NULL, record TEXT NOT NULL, PRIMARY KEY(server,request_id));",
        ).expect("fixture tables create");
        db
    }

    #[test]
    fn nonce_replay_is_rejected_within_the_transaction() {
        let mut db = nonce_db();
        let tx = db.transaction().expect("transaction starts");
        assert!(
            RemoteTransportRepository::consume_federation_nonce(
                &tx,
                "peer-a",
                "abcdefghijklmnopqrstuvwxyz012345",
                1_700_000_000
            )
            .expect("first nonce inserts")
        );
        assert!(
            !RemoteTransportRepository::consume_federation_nonce(
                &tx,
                "peer-a",
                "abcdefghijklmnopqrstuvwxyz012345",
                1_700_000_000
            )
            .expect("replayed nonce does not insert")
        );
        tx.commit().expect("nonce transaction commits");
    }

    #[test]
    fn nonce_rate_limit_counts_using_receipt_time_not_signed_time() {
        let mut db = nonce_db();
        let tx = db.transaction().expect("transaction starts");
        let now = 1_700_000_000;
        for index in 0..240 {
            let nonce = format!("{index:032}");
            assert!(
                RemoteTransportRepository::consume_access_nonce(&tx, "client-a", &nonce, now,)
                    .expect("nonce inserts within the rate limit")
            );
        }
        assert!(
            !RemoteTransportRepository::consume_access_nonce(
                &tx,
                "client-a",
                "99999999999999999999999999999999",
                now,
            )
            .expect("the next nonce is rate limited")
        );
        tx.commit().expect("rate limit transaction commits");
    }

    #[test]
    fn lost_outcome_keeps_the_same_request_identity() {
        let mut db = nonce_db();
        let tx = db.transaction().expect("transaction starts");
        assert!(matches!(
            RemoteTransportRepository::reserve_multi_server_request(
                &tx,
                "server-a",
                "request-a",
                "fingerprint-a",
                12.0
            )
            .expect("request is reserved"),
            OutboundReservation::New
        ));
        tx.commit().expect("reservation commits");

        let tx = db.transaction().expect("recovery transaction starts");
        assert!(matches!(
            RemoteTransportRepository::reserve_multi_server_request(
                &tx,
                "server-a",
                "request-a",
                "fingerprint-a",
                13.0
            )
            .expect("same identity recovers unknown state"),
            OutboundReservation::Unknown
        ));
        tx.commit().expect("recovery transaction commits");

        let tx = db.transaction().expect("receipt transaction starts");
        RemoteTransportRepository::complete_multi_server_request(
            &tx,
            "server-a",
            "request-a",
            "fingerprint-a",
            &json!({"ok":true}),
            14.0,
        )
        .expect("receipt completes matching identity");
        tx.commit().expect("receipt commits");

        let tx = db
            .transaction()
            .expect("duplicate completion transaction starts");
        assert!(
            RemoteTransportRepository::complete_multi_server_request(
                &tx,
                "server-a",
                "request-a",
                "fingerprint-a",
                &json!({"ok":false}),
                15.0,
            )
            .is_err()
        );
        tx.rollback().expect("duplicate completion rolls back");

        let tx = db.transaction().expect("read transaction starts");
        assert!(matches!(
            RemoteTransportRepository::reserve_multi_server_request(
                &tx,
                "server-a",
                "request-a",
                "fingerprint-a",
                16.0
            )
            .expect("completed identity recovers receipt"),
            OutboundReservation::Complete(_)
        ));
    }
}
