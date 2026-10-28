//! Persistence primitives shared by the remote federation and multi-server
//! transports. This module only uses tables already owned by the Python schema.

use super::multi_server_delivery::{admit_remote, remote_result_key, replay_remote, settle_remote};
use crate::request_results::{Admission, AdmittedRequest};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use studio_runtime_schema::persistence::AccessOutboundState;

#[derive(Serialize, Deserialize)]
pub struct AccessReply {
    pub outcome: AccessOutboundState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Box<serde_json::value::RawValue>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingAccessRequest {
    outcome: AccessOutboundState,
    created: f64,
    admission: AdmittedRequest,
}

fn request_result_error(error: crate::request_results::RequestResultError) -> StoreError {
    error.into()
}

use crate::schema::{SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};

const ACCESS_CLIENTS: &[TableRequirement] = &[TableRequirement {
    name: "runtime_access_clients",
    columns: &["id", "record"],
}];
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
const FEDERATION_RATE_LIMIT: i64 = 240;
const FEDERATION_RATE_WINDOW_SECONDS: i64 = 60;
const NONCE_RETENTION_SECONDS: i64 = 600;
const MULTI_SERVER_MAX_RETAINED_NONCES: i64 = 10_000;
const MULTI_SERVER_MAX_UNPAIRED_NONCES: i64 = 1_280;

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    conditional_tables: &[],
    tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[
        ACCESS_CLIENTS[0],
        ACCESS_NONCES[0],
        ACCESS_OUTBOUND[0],
        FEDERATION_IDENTITY[0],
        FEDERATION_NONCES[0],
    ],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "remote_transport",
        requirements: &REQUIREMENTS,
    }
}

/// Outcome of reserving a stable multi-server request identity.
pub enum OutboundReservation {
    New,
    Unknown,
    Complete(Value),
    Redacted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessNonceOutcome {
    Accepted,
    Replay,
    ClientLimit,
    PairingLimit,
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
        signed_timestamp: i64,
        now: i64,
    ) -> StoreResult<bool> {
        let stale_before = now.saturating_sub(NONCE_RETENTION_SECONDS);
        transaction.execute(
            "DELETE FROM runtime_federation_nonces WHERE created < ?1",
            [stale_before],
        )?;
        let recent: i64 = transaction.query_row(
            "SELECT count(*) FROM runtime_federation_nonces WHERE peer_id=?1 AND created>=?2",
            params![peer_id, now.saturating_sub(FEDERATION_RATE_WINDOW_SECONDS)],
            |row| row.get(0),
        )?;
        if recent >= FEDERATION_RATE_LIMIT {
            return Ok(false);
        }
        Ok(transaction.execute(
            "INSERT OR IGNORE INTO runtime_federation_nonces(peer_id,nonce,created) VALUES(?1,?2,?3)",
            params![peer_id, nonce, signed_timestamp],
        )? == 1)
    }

    /// Compatibility wrapper preserving all existing callers' bool outcome.
    pub fn consume_access_nonce(
        transaction: &Transaction<'_>,
        client_id: &str,
        nonce: &str,
        now: i64,
        unpaired_attempt: bool,
    ) -> StoreResult<bool> {
        Ok(Self::consume_access_nonce_outcome(
            transaction,
            client_id,
            nonce,
            now,
            unpaired_attempt,
        )? == AccessNonceOutcome::Accepted)
    }

    /// Consume a multi-server nonce as part of the caller's immediate transaction.
    pub fn consume_access_nonce_outcome(
        transaction: &Transaction<'_>,
        client_id: &str,
        nonce: &str,
        now: i64,
        unpaired_attempt: bool,
    ) -> StoreResult<AccessNonceOutcome> {
        let stale_before = now.saturating_sub(NONCE_RETENTION_SECONDS);
        transaction.execute(
            "DELETE FROM runtime_access_nonces WHERE created < ?1",
            [stale_before],
        )?;
        if unpaired_attempt {
            let unpaired: i64 = transaction.query_row(
                "SELECT count(*) FROM runtime_access_nonces n \
                 WHERE n.created>=?1 AND NOT EXISTS \
                 (SELECT 1 FROM runtime_access_clients c WHERE c.id=n.client)",
                [stale_before],
                |row| row.get(0),
            )?;
            if unpaired >= MULTI_SERVER_MAX_UNPAIRED_NONCES {
                return Ok(AccessNonceOutcome::PairingLimit);
            }
        }
        let retained: i64 = transaction.query_row(
            "SELECT count(*) FROM runtime_access_nonces WHERE client=?1",
            [client_id],
            |row| row.get(0),
        )?;
        if retained >= MULTI_SERVER_MAX_RETAINED_NONCES {
            return Ok(AccessNonceOutcome::ClientLimit);
        }
        Ok(if transaction.execute(
            "INSERT OR IGNORE INTO runtime_access_nonces(client,nonce,created) VALUES(?1,?2,?3)",
            params![client_id, nonce, now],
        )? == 1 { AccessNonceOutcome::Accepted } else { AccessNonceOutcome::Replay })
    }

    /// Reserve or recover an outbound request without changing its identity.
    pub fn reserve_multi_server_request(
        transaction: &Transaction<'_>,
        server_id: &str,
        request_id: &str,
        fingerprint: &str,
        created_at: f64,
    ) -> Result<OutboundReservation, StoreError> {
        let id = remote_result_key("access-outbound", &[server_id], request_id);
        if let Some(saved) = replay_remote::<AccessReply>(
            transaction,
            &id,
            fingerprint,
            (created_at * 1000.0) as i64,
        )
        .map_err(request_result_error)?
        {
            return access_reservation(saved.reply);
        }
        let existing = transaction
            .query_row(
                "SELECT hash,record FROM runtime_access_outbound WHERE server=?1 AND request_id=?2",
                params![server_id, request_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((saved_fingerprint, record)) = existing {
            if saved_fingerprint != fingerprint {
                return Err(crate::request_results::RequestResultError::Conflict {
                    id: id.clone(),
                }
                .into());
            }
            let record: PendingAccessRequest = serde_json::from_str(&record)
                .map_err(|_| invalid_outbound(server_id, request_id))?;
            if record.outcome != AccessOutboundState::Unknown
                || record.admission.signature() != fingerprint
            {
                return Err(invalid_outbound(server_id, request_id));
            }
            return Ok(OutboundReservation::Unknown);
        }
        let admission = match admit_remote(
            transaction,
            "access-outbound",
            &[server_id],
            request_id,
            fingerprint,
            (created_at * 1000.0) as i64,
        )
        .map_err(request_result_error)?
        {
            Admission::Fresh(admitted) => admitted,
            Admission::Replay(saved) => {
                let saved: crate::request_results::ResultEnvelope<AccessReply> =
                    serde_json::from_str(&saved.result)?;
                return access_reservation(saved.reply);
            }
        };
        let record = PendingAccessRequest {
            outcome: AccessOutboundState::Unknown,
            created: created_at,
            admission,
        };
        transaction.execute(
            "INSERT INTO runtime_access_outbound(server,request_id,hash,record) VALUES(?1,?2,?3,?4)",
            params![server_id, request_id, fingerprint, serde_json::to_string(&record)?],
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
        _completed_at: f64,
    ) -> Result<(), StoreError> {
        if !value.is_object()
            || matches!(
                value.get("outcome").and_then(Value::as_str),
                Some("pending" | "unknown")
            )
        {
            return Err(StoreError::SchemaMismatch(format!(
                "invalid completion for runtime_access_outbound/{server_id}/{request_id}"
            )));
        }
        finish_access(
            transaction,
            server_id,
            request_id,
            fingerprint,
            AccessReply {
                outcome: AccessOutboundState::Complete,
                value: Some(serde_json::value::to_raw_value(value)?),
            },
        )
    }

    pub fn redact_multi_server_request(
        transaction: &Transaction<'_>,
        server_id: &str,
        request_id: &str,
        fingerprint: &str,
        _completed_at: f64,
    ) -> Result<(), StoreError> {
        finish_access(
            transaction,
            server_id,
            request_id,
            fingerprint,
            AccessReply {
                outcome: AccessOutboundState::Redacted,
                value: None,
            },
        )
    }
}

fn access_reservation(reply: AccessReply) -> Result<OutboundReservation, StoreError> {
    match reply.outcome {
        AccessOutboundState::Complete => reply
            .value
            .map(|value| {
                serde_json::from_str(value.get())
                    .map(OutboundReservation::Complete)
                    .map_err(Into::into)
            })
            .ok_or(StoreError::RecordContract("completed access response"))?,
        AccessOutboundState::Redacted => Ok(OutboundReservation::Redacted),
        AccessOutboundState::Unknown => {
            Err(StoreError::RecordContract("unsettled access response"))
        }
    }
}

fn finish_access(
    tx: &Transaction<'_>,
    server: &str,
    request: &str,
    signature: &str,
    reply: AccessReply,
) -> Result<(), StoreError> {
    let raw: String = tx.query_row(
        "SELECT record FROM runtime_access_outbound WHERE server=?1 AND request_id=?2 AND hash=?3",
        params![server, request, signature],
        |row| row.get(0),
    )?;
    let pending: PendingAccessRequest =
        serde_json::from_str(&raw).map_err(|_| invalid_outbound(server, request))?;
    if pending.outcome != AccessOutboundState::Unknown || pending.admission.signature() != signature
    {
        return Err(invalid_outbound(server, request));
    }
    settle_remote(tx, &pending.admission, None, &reply).map_err(request_result_error)?;
    tx.execute(
        "DELETE FROM runtime_access_outbound WHERE server=?1 AND request_id=?2 AND hash=?3",
        params![server, request, signature],
    )?;
    Ok(())
}

fn invalid_outbound(server_id: &str, request_id: &str) -> StoreError {
    StoreError::InvalidStoredObject {
        table: "runtime_access_outbound",
        id: format!("{server_id}/{request_id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MULTI_SERVER_MAX_RETAINED_NONCES, MULTI_SERVER_MAX_UNPAIRED_NONCES, OutboundReservation,
        RemoteTransportRepository,
    };
    use rusqlite::Connection;
    use serde_json::json;

    fn nonce_db() -> Connection {
        let db = Connection::open_in_memory().expect("in-memory SQLite opens");
        db.execute_batch(
            "CREATE TABLE runtime_federation_nonces(peer_id TEXT NOT NULL, nonce TEXT NOT NULL, created INTEGER NOT NULL, PRIMARY KEY(peer_id,nonce));
             CREATE TABLE runtime_access_nonces(client TEXT NOT NULL, nonce TEXT NOT NULL, created INTEGER NOT NULL, PRIMARY KEY(client,nonce));
             CREATE TABLE runtime_access_clients(id TEXT NOT NULL, record TEXT NOT NULL);
             CREATE TABLE runtime_access_outbound(server TEXT NOT NULL, request_id TEXT NOT NULL, hash TEXT NOT NULL, record TEXT NOT NULL, PRIMARY KEY(server,request_id));",
        ).expect("fixture tables create");
        super::super::super::receipt_schema::create(&db).expect("target receipt schema");
        db.execute_batch(
            "CREATE TABLE runtime_orchestration_settings(id TEXT PRIMARY KEY,record TEXT NOT NULL)",
        )
        .expect("settings");
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
                1_700_000_000,
                1_700_000_000
            )
            .expect("first nonce inserts")
        );
        assert!(
            !RemoteTransportRepository::consume_federation_nonce(
                &tx,
                "peer-a",
                "abcdefghijklmnopqrstuvwxyz012345",
                1_700_000_000,
                1_700_000_000
            )
            .expect("replayed nonce does not insert")
        );
        tx.commit().expect("nonce transaction commits");
    }

    #[test]
    fn federation_rate_limit_uses_signed_created_timestamp() {
        let mut db = nonce_db();
        let tx = db.transaction().expect("transaction starts");
        let now = 1_700_000_000;
        for index in 0..240 {
            let nonce = format!("{index:032}");
            assert!(
                RemoteTransportRepository::consume_federation_nonce(
                    &tx,
                    "peer-a",
                    &nonce,
                    now - 61,
                    now,
                )
                .expect("nonce inserts within the rate limit")
            );
        }
        assert!(
            RemoteTransportRepository::consume_federation_nonce(
                &tx,
                "peer-a",
                "new-federation-nonce",
                now - 61,
                now,
            )
            .expect("old signed timestamps are outside the rate window")
        );
        let created: i64 = tx
            .query_row(
                "SELECT created FROM runtime_federation_nonces WHERE nonce='new-federation-nonce'",
                [],
                |row| row.get(0),
            )
            .expect("signed time is stored");
        assert_eq!(created, now - 61);
        tx.commit().expect("nonce transaction commits");
    }

    #[test]
    fn multi_server_uses_retained_nonce_limits_not_federation_rate_limit() {
        let mut db = nonce_db();
        let tx = db.transaction().expect("transaction starts");
        let now = 1_700_000_000;
        for index in 0..240 {
            assert!(
                RemoteTransportRepository::consume_access_nonce(
                    &tx,
                    "paired-client",
                    &format!("{index:032}"),
                    now,
                    false,
                )
                .expect("multi-server nonce within retained cap")
            );
        }
        assert!(
            RemoteTransportRepository::consume_access_nonce(
                &tx,
                "paired-client",
                "99999999999999999999999999999999",
                now,
                false,
            )
            .expect("241st retained multi-server nonce is allowed")
        );
        tx.commit().expect("nonce transaction commits");

        let mut db = nonce_db();
        db.execute(
            "WITH RECURSIVE n(i) AS (VALUES(0) UNION ALL SELECT i+1 FROM n WHERE i+1<?1)
             INSERT INTO runtime_access_nonces SELECT 'paired-client',printf('%032d',i),?2 FROM n",
            rusqlite::params![MULTI_SERVER_MAX_RETAINED_NONCES, now - 61],
        )
        .expect("retained nonce fixture inserts");
        let tx = db.transaction().expect("limit transaction starts");
        assert!(
            !RemoteTransportRepository::consume_access_nonce(
                &tx,
                "paired-client",
                "fresh-nonce",
                now,
                false,
            )
            .expect("retained cap refuses next nonce")
        );
        tx.rollback().expect("refusal rolls back transaction");
    }

    #[test]
    fn unpaired_nonce_cap_is_limited_to_pairing_context() {
        let mut db = nonce_db();
        let tx = db.transaction().expect("transaction starts");
        tx.execute(
            "WITH RECURSIVE n(i) AS (VALUES(0) UNION ALL SELECT i+1 FROM n WHERE i+1<?1)
             INSERT INTO runtime_access_nonces SELECT 'client-'||i,printf('%032d',i),100 FROM n",
            [MULTI_SERVER_MAX_UNPAIRED_NONCES],
        )
        .expect("unpaired nonce fixture inserts");
        assert!(
            !RemoteTransportRepository::consume_access_nonce(
                &tx,
                "new-client",
                "new-nonce",
                200,
                true,
            )
            .expect("pairing context is capped")
        );
        assert!(
            RemoteTransportRepository::consume_access_nonce(
                &tx,
                "paired-client",
                "paired-nonce",
                200,
                false,
            )
            .expect("paired context does not use unpaired cap")
        );
        tx.rollback().expect("test transaction rolls back");
    }

    #[test]
    fn refusal_transaction_rolls_back_stale_nonce_cleanup() {
        let mut db = nonce_db();
        db.execute(
            "INSERT INTO runtime_federation_nonces(peer_id,nonce,created) VALUES('peer-a','stale',1)",
            [],
        ).expect("stale row inserts");
        for index in 0..240 {
            db.execute(
                "INSERT INTO runtime_federation_nonces(peer_id,nonce,created) VALUES('peer-a',?1,1000)",
                [format!("recent-{index}")],
            ).expect("rate row inserts");
        }
        let tx = db.transaction().expect("refusal transaction begins");
        assert!(
            !RemoteTransportRepository::consume_federation_nonce(
                &tx,
                "peer-a",
                "rate-limited",
                1000,
                1000,
            )
            .expect("rate refusal returned to transaction owner")
        );
        drop(tx);
        let stale_count: i64 = db
            .query_row(
                "SELECT count(*) FROM runtime_federation_nonces WHERE nonce='stale'",
                [],
                |row| row.get(0),
            )
            .expect("stale-row count reads");
        assert_eq!(stale_count, 1);
    }

    #[test]
    fn lost_outcome_keeps_the_same_request_identity() {
        let mut db = nonce_db();
        let tx = db.transaction().expect("transaction starts");
        assert!(matches!(
            RemoteTransportRepository::reserve_multi_server_request(
                &tx,
                "server-a",
                "00000000-03e8-7000-8000-000000000001",
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
                "00000000-03e8-7000-8000-000000000001",
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
            "00000000-03e8-7000-8000-000000000001",
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
                "00000000-03e8-7000-8000-000000000001",
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
                "00000000-03e8-7000-8000-000000000001",
                "fingerprint-a",
                16.0
            )
            .expect("completed identity recovers receipt"),
            OutboundReservation::Complete(_)
        ));
    }

    #[test]
    fn unknown_reservation_is_not_sent_again_after_database_restart() {
        let directory = tempfile::tempdir().expect("owned test directory");
        let path = directory.path().join("snapshot.sqlite3");
        let mut sent = 0;
        {
            let mut db = Connection::open(&path).expect("journal database opens");
            db.execute_batch("CREATE TABLE runtime_access_outbound(server TEXT,request_id TEXT,hash TEXT,record TEXT,PRIMARY KEY(server,request_id));")
                .expect("journal schema creates");
            crate::receipt_schema::create(&db).expect("target receipt schema");
            db.execute_batch("CREATE TABLE runtime_orchestration_settings(id TEXT PRIMARY KEY,record TEXT NOT NULL)").expect("settings");
            let tx = db.transaction().expect("first reservation starts");
            let reservation = RemoteTransportRepository::reserve_multi_server_request(
                &tx,
                "server-a",
                "00000000-03e8-7000-8000-000000000001",
                "fingerprint-a",
                1.0,
            )
            .expect("first request reserves");
            tx.commit().expect("reservation persists");
            if matches!(&reservation, OutboundReservation::New) {
                sent += 1;
            }
        }
        {
            let mut db = Connection::open(&path).expect("journal reopens after restart");
            let tx = db.transaction().expect("recovery transaction starts");
            let reservation = RemoteTransportRepository::reserve_multi_server_request(
                &tx,
                "server-a",
                "00000000-03e8-7000-8000-000000000001",
                "fingerprint-a",
                2.0,
            )
            .expect("recovery sees unknown result");
            assert!(matches!(reservation, OutboundReservation::Unknown));
            tx.commit().expect("recovery does not change reservation");
            if matches!(&reservation, OutboundReservation::New) {
                sent += 1;
            }
        }

        assert_eq!(sent, 1);
    }

    #[test]
    fn malformed_receipt_reports_the_table_and_stored_key() {
        let db = nonce_db();
        db.execute(
            "INSERT INTO runtime_access_outbound(server,request_id,hash,record) VALUES('server-a','00000000-03e8-7000-8000-000000000001','fingerprint-a','{}')",
            [],
        ).expect("invalid persisted receipt fixture inserts");
        let tx = db.unchecked_transaction().expect("read transaction begins");
        let error = RemoteTransportRepository::reserve_multi_server_request(
            &tx,
            "server-a",
            "00000000-03e8-7000-8000-000000000001",
            "fingerprint-a",
            1.0,
        )
        .err()
        .expect("receipt without outcome is rejected");
        assert!(
            error
                .to_string()
                .contains("runtime_access_outbound/server-a/00000000-03e8-7000-8000-000000000001")
        );
        tx.rollback().expect("read transaction ends");
    }

    #[test]
    fn invitation_receipt_is_redacted_without_storing_the_response() {
        let mut db = nonce_db();
        let tx = db.transaction().expect("reservation begins");
        RemoteTransportRepository::reserve_multi_server_request(
            &tx,
            "server-a",
            "00000000-03e8-7000-8000-000000000002",
            "fingerprint-a",
            1.0,
        )
        .expect("invitation reserves");
        RemoteTransportRepository::redact_multi_server_request(
            &tx,
            "server-a",
            "00000000-03e8-7000-8000-000000000002",
            "fingerprint-a",
            2.0,
        )
        .expect("invitation response is marked redacted");
        tx.commit().expect("redacted receipt commits");
        let stored: String = db
            .query_row(
                "SELECT result FROM request_results WHERE id=?1",
                [super::remote_result_key(
                    "access-outbound",
                    &["server-a"],
                    "00000000-03e8-7000-8000-000000000002",
                )],
                |row| row.get(0),
            )
            .expect("redacted receipt reads");
        let result: serde_json::Value = serde_json::from_str(&stored).expect("receipt JSON");
        assert_eq!(result["reply"]["outcome"], "redacted");
        assert!(result["reply"].get("value").is_none());
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_access_outbound", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("pending count"),
            0
        );
        let tx = db
            .unchecked_transaction()
            .expect("replay transaction begins");
        assert!(matches!(
            RemoteTransportRepository::reserve_multi_server_request(
                &tx,
                "server-a",
                "00000000-03e8-7000-8000-000000000002",
                "fingerprint-a",
                3.0
            )
            .expect("redacted state validates"),
            OutboundReservation::Redacted
        ));
        tx.rollback().expect("replay transaction ends");
    }
}
