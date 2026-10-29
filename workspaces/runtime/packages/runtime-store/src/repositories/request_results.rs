//! Immutable exact replies. Call only after authorization, inside the owner's writer transaction.
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use studio_runtime_schema::persistence::{
    REQUEST_RESULT_TTL_MS, RequestIdentity, RequestIdentityError, RequestReferenceKind,
};

/// Maximum rows removed by one GC transaction. Unit: request-result rows.
pub const REQUEST_RESULT_GC_BATCH_ROWS: usize = 500;
pub(crate) const TABLE: &str = "request_results";

#[derive(Debug, thiserror::Error)]
pub enum RequestResultError {
    #[error("Request id expired")]
    Expired,
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Identity(#[from] RequestIdentityError),
    #[error("request id {id} already has a different signature")]
    Conflict { id: String },
}

pub type Result<T> = std::result::Result<T, RequestResultError>;

/// The caller's typed original response, actor and durable evidence references.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultEnvelope<R> {
    pub actor: Option<String>,
    pub reply: R,
    pub references: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AbandonedResult {
    Abandoned { outcome: AbandonedOutcome },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AbandonedOutcome {
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredResult {
    pub id: String,
    pub signature: String,
    /// Exact stored JSON bytes, never reconstructed from mutable owner state.
    pub result: String,
    pub created: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredReply<R> {
    pub signature: String,
    pub reply: R,
}

/// The stable entity and revision recorded when a command was applied.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestReference {
    pub kind: RequestReferenceKind,
    pub id: String,
    pub revision: i64,
}

/// Persist this with the admitted owner/outbox for asynchronous settlement.
/// Deserialization is for trusted owner state, never unauthenticated ingress.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AdmittedRequest {
    id: String,
    signature: String,
    created: i64,
}
impl AdmittedRequest {
    /// Restore a token from a previously validated persisted owner record.
    pub fn from_persisted_fields(id: String, signature: String, created: i64) -> Self {
        Self {
            id,
            signature,
            created,
        }
    }
    /// A result suboperation of this persisted admitted owner token. This is
    /// completion, never ingress: preserve issuance and do not refresh its TTL.
    pub fn derive_result(&self, namespace: &str, operation: &[&str], signature: &str) -> Self {
        // A tuple of strings always serializes. The owner token remains the scope fence.
        let id = serde_json::to_string(&("owner-result", namespace, &self.id, operation))
            .expect("owner result identity serializes");
        Self {
            id,
            signature: signature.into(),
            created: self.created,
        }
    }

    /// Used only by classified cutover migrations, never to admit a runtime caller.
    pub(crate) fn from_migration(id: &str, signature: &str, cutover_ms: i64) -> Self {
        Self {
            id: id.into(),
            signature: signature.into(),
            created: cutover_ms,
        }
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn signature(&self) -> &str {
        &self.signature
    }
    pub fn created(&self) -> i64 {
        self.created
    }
}

#[derive(Debug)]
pub enum Admission {
    Fresh(AdmittedRequest),
    Replay(StoredResult),
}

pub struct RequestResults;
impl RequestResults {
    /// Look up a reference without admitting an operation. A body-backed result
    /// remains on the existing body replay path.
    pub fn replay_reference(
        conn: &Connection,
        id: &str,
        signature: &str,
        now_ms: i64,
    ) -> Result<Option<RequestReference>> {
        type SavedReference = (
            String,
            Option<RequestReferenceKind>,
            Option<String>,
            Option<i64>,
            i64,
        );
        let saved: Option<SavedReference> = conn
            .query_row(
                "SELECT signature,ref_kind,ref_id,ref_rev,created FROM request_results WHERE id=?1",
                [id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((stored_signature, kind, ref_id, revision, created)) = saved else {
            return Ok(None);
        };
        if stored_signature != signature {
            return Err(RequestResultError::Conflict { id: id.into() });
        }
        let floor = now_ms.saturating_sub(REQUEST_RESULT_TTL_MS);
        if created <= floor {
            return Err(RequestIdentityError::Expired {
                issued_ms: created,
                expired_before_ms: floor,
            }
            .into());
        }
        match (kind, ref_id, revision) {
            (Some(kind), Some(id), Some(revision)) => {
                Ok(Some(RequestReference { kind, id, revision }))
            }
            (None, None, None) => Ok(None),
            _ => Err(rusqlite::Error::InvalidQuery.into()),
        }
    }

    /// Settle an entity reference in the same transaction as its mutation.
    pub fn settle_reference(
        tx: &Transaction<'_>,
        admitted: &AdmittedRequest,
        reference: &RequestReference,
    ) -> Result<RequestReference> {
        let existing: Option<String> = tx
            .query_row(
                "SELECT signature FROM request_results WHERE id=?1",
                [admitted.id()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(signature) = existing {
            if signature != admitted.signature() {
                return Err(RequestResultError::Conflict {
                    id: admitted.id().into(),
                });
            }
            return Self::replay_reference(
                tx,
                admitted.id(),
                admitted.signature(),
                admitted.created(),
            )?
            .ok_or_else(|| RequestResultError::Conflict {
                id: admitted.id().into(),
            });
        }
        tx.execute(
            "INSERT INTO request_results(id,signature,status,ref_kind,ref_id,ref_rev,created) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![admitted.id(), admitted.signature(), studio_runtime_schema::persistence::RequestResultStatus::Applied, reference.kind, reference.id, reference.revision, admitted.created()],
        )?;
        Ok(reference.clone())
    }
    /// Settle an explicitly authorized abandonment and remove its exact outbox
    /// in the same owner transaction. The effect may have happened.
    pub fn settle_abandoned<P>(
        tx: &Transaction<'_>,
        row: &crate::outbox::OutboxRow<P>,
    ) -> crate::StoreResult<()> {
        let result = ResultEnvelope {
            actor: (row.owner_kind == studio_runtime_schema::persistence::OwnerKind::Agent)
                .then(|| row.owner_id.clone()),
            reply: AbandonedResult::Abandoned {
                outcome: AbandonedOutcome::Unknown,
            },
            references: vec![],
        };
        Self::settle_exact(tx, &row.id, &row.signature, row.created, &result)
            .map_err(crate::StoreError::from)?;
        crate::outbox::Outbox::delete_settled(tx, &row.id, &row.signature)
            .map_err(crate::StoreError::from)?;
        Ok(())
    }
    /// Authorized status recovery only: exact imported PK first, then canonical
    /// scope. A miss admits nothing; expired cache rows are ordinary misses.
    #[allow(clippy::too_many_arguments)]
    pub fn read<R: DeserializeOwned>(
        conn: &Connection,
        raw_id: &str,
        namespace: &str,
        authority: &[&str],
        root: &str,
        operation: &[&str],
        now_ms: i64,
    ) -> crate::StoreResult<Option<StoredReply<R>>> {
        let lookup = |id: &str| -> rusqlite::Result<Option<(String, String)>> {
            conn.query_row(
                "SELECT signature,result FROM request_results WHERE id=?1 AND created>?2",
                params![id, now_ms.saturating_sub(REQUEST_RESULT_TTL_MS)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
        };
        let saved = match lookup(raw_id)? {
            Some(saved) => Some(saved),
            None => match RequestIdentity::new(namespace, authority, root, operation) {
                Ok(identity) => lookup(identity.id())?,
                Err(_) => None,
            },
        };
        saved
            .map(|(signature, result)| {
                let envelope: ResultEnvelope<R> = serde_json::from_str(&result)?;
                Ok(StoredReply {
                    signature,
                    reply: envelope.reply,
                })
            })
            .transpose()
    }

    /// Completion-only lookup using the persisted owner token. Age does not
    /// invalidate already admitted work; a miss grants no new effect/admission.
    pub fn admitted_result(
        tx: &Connection,
        admitted: &AdmittedRequest,
    ) -> Result<Option<StoredResult>> {
        Self::stored(tx, admitted.id(), admitted.signature())
    }
    pub fn expired_before_ms(tx: &Connection) -> Result<i64> {
        Ok(tx.query_row("SELECT coalesce(json_extract(record,'$.expired_before_ms'),0) FROM runtime_orchestration_settings WHERE id='global'", [], |row| row.get(0)).optional()?.unwrap_or(0))
    }

    /// Ordinary ID lookup includes migrated rows. A miss grants no admission.
    pub fn replay(
        tx: &Connection,
        id: &str,
        signature: &str,
        now_ms: i64,
    ) -> Result<Option<StoredResult>> {
        let saved = Self::stored(tx, id, signature)?;
        if let Some(row) = &saved {
            let floor = now_ms.saturating_sub(REQUEST_RESULT_TTL_MS);
            if row.created <= floor {
                return Err(RequestIdentityError::Expired {
                    issued_ms: row.created,
                    expired_before_ms: floor,
                }
                .into());
            }
        }
        Ok(saved)
    }

    fn stored(tx: &Connection, id: &str, signature: &str) -> Result<Option<StoredResult>> {
        let saved = tx
            .query_row(
                "SELECT id,signature,result,created FROM request_results WHERE id=?1",
                [id],
                |row| {
                    Ok(StoredResult {
                        id: row.get(0)?,
                        signature: row.get(1)?,
                        result: row.get(2)?,
                        created: row.get(3)?,
                    })
                },
            )
            .optional()?;
        if let Some(row) = &saved
            && row.signature != signature
        {
            return Err(RequestResultError::Conflict { id: id.into() });
        }
        Ok(saved)
    }

    /// The caller must use an IMMEDIATE transaction (RuntimeStore's writer does).
    /// Fresh admission is made durable with the owner's mutation, never a placeholder reply.
    pub fn admit(
        tx: &Transaction<'_>,
        identity: &RequestIdentity,
        signature: &str,
        now_ms: i64,
    ) -> Result<Admission> {
        if let Some(row) = Self::replay(tx, identity.id(), signature, now_ms)? {
            return Ok(Admission::Replay(row));
        }
        identity.validate(now_ms, Self::expired_before_ms(tx)?)?;
        Ok(Admission::Fresh(AdmittedRequest {
            id: identity.id().into(),
            signature: signature.into(),
            created: identity.issued_ms(),
        }))
    }

    /// Completion remains allowed after expiry; it never authorizes another external attempt.
    /// Mutate the owner and call this in the same caller-owned transaction.
    pub fn settle<R: Serialize>(
        tx: &Transaction<'_>,
        admitted: &AdmittedRequest,
        result: &ResultEnvelope<R>,
    ) -> Result<StoredResult> {
        Self::settle_exact(
            tx,
            &admitted.id,
            &admitted.signature,
            admitted.created,
            result,
        )
    }

    fn settle_exact<R: Serialize>(
        tx: &Transaction<'_>,
        id: &str,
        signature: &str,
        created: i64,
        result: &ResultEnvelope<R>,
    ) -> Result<StoredResult> {
        if let Some(saved) = Self::stored(tx, id, signature)? {
            return Ok(saved);
        }
        let result = serde_json::to_string(result)?;
        tx.execute(
            "INSERT INTO request_results(id,signature,result,created) VALUES(?1,?2,?3,?4)",
            params![id, signature, result, created],
        )?;
        Ok(StoredResult {
            id: id.into(),
            signature: signature.into(),
            result,
            created,
        })
    }

    /// Only the created index is visited. The floor advances with deletion in this transaction.
    pub fn gc(tx: &Transaction<'_>, now_ms: i64) -> Result<usize> {
        let cutoff = now_ms.saturating_sub(REQUEST_RESULT_TTL_MS);
        let floor = cutoff.max(Self::expired_before_ms(tx)?);
        let deleted = tx.execute("DELETE FROM request_results WHERE id IN (SELECT id FROM request_results INDEXED BY request_results_created WHERE created<=?1 ORDER BY created,id LIMIT ?2)", params![cutoff,REQUEST_RESULT_GC_BATCH_ROWS as i64])?;
        tx.execute("INSERT INTO runtime_orchestration_settings(id,record) VALUES('global',json_object('expired_before_ms',?1)) ON CONFLICT(id) DO UPDATE SET record=json_set(record,'$.expired_before_ms',?1)", [floor])?;
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Connection, RuntimeStore, StoreError};
    use std::sync::Arc;
    use studio_runtime_schema::persistence::REQUEST_ID_FUTURE_SKEW_MS;

    #[test]
    fn abandoned_result_and_exact_outbox_are_settled_together() {
        use studio_runtime_schema::persistence::{OutboxKind, OutboxStatus, OwnerKind};
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        tx.execute("INSERT INTO runtime_outbox(id,kind,owner_kind,owner_id,signature,status,payload,created) VALUES('effect',?1,?2,'owner','sig',?3,'{}',1)", params![OutboxKind::ProviderCall, OwnerKind::Agent, OutboxStatus::Unknown]).expect("outbox");
        let token = AdmittedRequest::from_migration("effect", "sig", 1);
        let effect: crate::outbox::OutboxRow<serde_json::Value> =
            crate::outbox::Outbox::get(&tx, "effect")
                .expect("row")
                .expect("exists");
        tx.execute(
            "INSERT INTO request_results(id,signature,result,created) VALUES('effect','different','{}',1)",
            [],
        )
        .expect("conflict");
        assert!(RequestResults::settle_abandoned(&tx, &effect).is_err());
        assert!(
            crate::outbox::Outbox::get::<serde_json::Value>(&tx, "effect")
                .expect("row")
                .is_some()
        );
        tx.execute("DELETE FROM request_results WHERE id='effect'", [])
            .expect("clear conflict");
        RequestResults::settle_abandoned(&tx, &effect).expect("settle");
        let saved = RequestResults::admitted_result(&tx, &token)
            .expect("read")
            .expect("saved");
        let result: ResultEnvelope<AbandonedResult> =
            serde_json::from_str(&saved.result).expect("typed result");
        assert_eq!(
            result.reply,
            AbandonedResult::Abandoned {
                outcome: AbandonedOutcome::Unknown
            }
        );
        assert_eq!(
            tx.query_row("SELECT count(*) FROM runtime_outbox", [], |row| row
                .get::<_, i64>(0))
                .expect("count"),
            0
        );
        tx.rollback().expect("rollback");
        assert_eq!(
            db.query_row("SELECT count(*) FROM request_results", [], |row| row
                .get::<_, i64>(0))
                .expect("count"),
            0
        );
    }

    #[test]
    fn receipt_result_expired_stays_typed_without_synthetic_metadata() {
        let error: StoreError = RequestResultError::Expired.into();
        assert!(matches!(
            error,
            StoreError::RequestResult(RequestResultError::Expired)
        ));
        assert_eq!(error.to_string(), "Request id expired");
    }

    #[test]
    fn receipt_result_typed_read_exact_canonical_ttl_and_decode() {
        let db = database();
        let now = REQUEST_RESULT_TTL_MS * 2;
        let root = uuid::Uuid::from_u128(((now as u128) << 80) | (7 << 76) | (2 << 62)).to_string();
        let canonical = RequestIdentity::new("read", &["actor"], &root, &["op"]).unwrap();
        let envelope = serde_json::to_string(&ResultEnvelope {
            actor: None,
            reply: "exact reply",
            references: vec![],
        })
        .unwrap();
        for id in ["raw pk", canonical.id()] {
            db.execute(
                "INSERT INTO request_results(id,signature,result,created) VALUES(?1,'sig',?2,?3)",
                params![id, envelope, now],
            )
            .unwrap();
        }
        let read = |raw, at| {
            RequestResults::read::<String>(&db, raw, "read", &["actor"], &root, &["op"], at)
        };
        let exact = read("raw pk", now).unwrap().unwrap();
        assert_eq!(
            (exact.signature.as_str(), exact.reply.as_str()),
            ("sig", "exact reply")
        );
        assert_eq!(read("missing", now).unwrap().unwrap().reply, "exact reply");
        assert!(
            read("raw pk", now + REQUEST_RESULT_TTL_MS)
                .unwrap()
                .is_none()
        );
        db.execute(
            "UPDATE request_results SET result=json_object('actor',NULL,'reply',5,'references',json_array()) WHERE id='raw pk'",
            [],
        )
        .unwrap();
        assert!(matches!(read("raw pk", now), Err(StoreError::Json(_))));
        assert_eq!(
            db.query_row("SELECT count(*) FROM request_results", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2,
            "reads admit nothing"
        );
    }

    #[test]
    fn receipt_result_admitted_schema_matches_store_owner() {
        let owner = schemars::schema_for!(AdmittedRequest).to_value();
        let consumer = studio_runtime_schema::contracts::admitted_request_schema();
        for field in ["properties", "required", "additionalProperties"] {
            assert_eq!(
                owner[field], consumer[field],
                "AdmittedRequest schemas must match runtime-store/src/repositories/request_results.rs owner: {field}"
            );
        }
    }

    #[test]
    fn receipt_result_cutover_floor_preserves_imported_reply_ttl() {
        let mut db = database();
        let cutover = REQUEST_RESULT_TTL_MS * 2;
        let tx = db.transaction().expect("tx");
        tx.execute("INSERT INTO runtime_orchestration_settings VALUES('global',json_object('expired_before_ms',?1))",[cutover]).expect("seed floor");
        let token = crate::receipt_migration_contract::admitted_owner_token(
            "ordinary imported pk",
            "sig",
            cutover,
        );
        RequestResults::settle(
            &tx,
            &token,
            &ResultEnvelope {
                actor: None,
                reply: "exact old reply",
                references: vec![],
            },
        )
        .expect("import");
        assert!(
            RequestResults::replay(&tx, token.id(), "sig", cutover + 1)
                .expect("ordinary imported cache grace")
                .is_some()
        );
        assert_eq!(
            RequestResults::gc(&tx, cutover + 1).expect("not TTL-expired"),
            0
        );
        assert!(matches!(
            RequestResults::admit(
                &tx,
                &identity(cutover, "discarded old operation"),
                "sig",
                cutover + 1
            ),
            Err(RequestResultError::Identity(
                RequestIdentityError::Expired { .. }
            ))
        ));
        assert!(matches!(
            RequestResults::replay(&tx, token.id(), "sig", cutover + REQUEST_RESULT_TTL_MS),
            Err(RequestResultError::Identity(
                RequestIdentityError::Expired { .. }
            ))
        ));
    }

    #[test]
    fn receipt_result_derived_owner_tokens_preserve_issuance_after_expiry() {
        let mut db = database();
        let issued = REQUEST_RESULT_TTL_MS * 2;
        let tx = db.transaction().expect("tx");
        let Admission::Fresh(root) =
            RequestResults::admit(&tx, &identity(issued, "root"), "root", issued)
                .expect("root admission")
        else {
            panic!("fresh root");
        };
        let persisted: AdmittedRequest =
            serde_json::from_str(&serde_json::to_string(&root).expect("persist token"))
                .expect("restore token");
        let final_result =
            persisted.derive_result("execution-attempt", &["terminal"], "exact callback input");
        assert_eq!(final_result.created(), root.created());
        assert_eq!(
            final_result,
            root.derive_result("execution-attempt", &["terminal"], "exact callback input")
        );
        assert_ne!(
            final_result.id(),
            root.derive_result("another-owner", &["terminal"], "exact callback input")
                .id()
        );
        RequestResults::gc(&tx, issued + REQUEST_RESULT_TTL_MS + 1).expect("expire ingress");
        RequestResults::settle(
            &tx,
            &final_result,
            &ResultEnvelope {
                actor: None,
                reply: "original reply",
                references: vec![],
            },
        )
        .expect("late completion remains allowed");
        assert!(
            RequestResults::admitted_result(&tx, &final_result)
                .expect("owner completion sees retained result")
                .is_some()
        );
        assert!(matches!(
            RequestResults::replay(
                &tx,
                final_result.id(),
                final_result.signature(),
                issued + REQUEST_RESULT_TTL_MS + 1
            ),
            Err(RequestResultError::Identity(
                RequestIdentityError::Expired { .. }
            ))
        ));
        RequestResults::gc(&tx, issued + REQUEST_RESULT_TTL_MS + 1)
            .expect("normal expired cache GC");
        assert!(
            RequestResults::admitted_result(&tx, &final_result)
                .expect("no resurrection after GC")
                .is_none()
        );
        let migrated = crate::receipt_migration_contract::admitted_owner_token(
            "opaque migrated pk",
            "original signature",
            issued,
        );
        assert_eq!(migrated.id(), "opaque migrated pk");
        assert_eq!(migrated.created(), issued);
        assert_eq!(
            migrated
                .derive_result("execution-attempt", &["terminal"], "callback")
                .created(),
            issued
        );
    }

    fn identity(ms: i64, discriminator: &str) -> RequestIdentity {
        let root = uuid::Uuid::from_u128(((ms as u128) << 80) | (7 << 76) | (2 << 62));
        RequestIdentity::new(
            "disposable_counter",
            &["actor"],
            &root.to_string(),
            &[discriminator],
        )
        .expect("v7")
    }
    fn database() -> Connection {
        let db = Connection::open_in_memory().expect("database");
        crate::receipt_schema::create(&db).expect("target schema");
        db.execute_batch("CREATE TABLE runtime_orchestration_settings(id TEXT PRIMARY KEY,record TEXT NOT NULL); CREATE TABLE counter(n INTEGER NOT NULL); INSERT INTO counter VALUES(0)").expect("owner");
        db
    }
    /// Disposable owner caller: admission, mutation and exact settlement share the writer tx.
    fn increment(
        tx: &Transaction<'_>,
        identity: &RequestIdentity,
        signature: &str,
        now: i64,
    ) -> Result<StoredResult> {
        match RequestResults::admit(tx, identity, signature, now)? {
            Admission::Replay(saved) => Ok(saved),
            Admission::Fresh(admitted) => {
                let reply: i64 =
                    tx.query_row("UPDATE counter SET n=n+1 RETURNING n", [], |row| row.get(0))?;
                RequestResults::settle(
                    tx,
                    &admitted,
                    &ResultEnvelope {
                        actor: Some("actor".into()),
                        reply,
                        references: vec![],
                    },
                )
            }
        }
    }
    #[test]
    fn receipt_result_exact_replay_conflict_and_atomic_rollback() {
        let mut db = database();
        let now = REQUEST_RESULT_TTL_MS * 2;
        let id = identity(now, "one");
        let original = {
            let tx = db.transaction().expect("tx");
            let reply = increment(&tx, &id, "bytes", now).expect("first");
            tx.commit().expect("commit");
            reply
        };
        let tx = db.transaction().expect("tx");
        assert_eq!(
            increment(&tx, &id, "bytes", now + 1).expect("replay"),
            original
        );
        assert!(matches!(
            increment(&tx, &id, "changed", now),
            Err(RequestResultError::Conflict { .. })
        ));
        assert_eq!(
            tx.query_row("SELECT n FROM counter", [], |row| row.get::<_, i64>(0))
                .expect("owner"),
            1
        );
        let second = identity(now, "two");
        increment(&tx, &second, "bytes", now).expect("uncommitted");
        tx.rollback().expect("rollback");
        let tx = db.transaction().expect("tx");
        assert!(
            RequestResults::replay(&tx, second.id(), "bytes", now)
                .expect("lookup")
                .is_none()
        );
        assert_eq!(
            tx.query_row("SELECT n FROM counter", [], |row| row.get::<_, i64>(0))
                .expect("owner"),
            1
        );
    }
    #[test]
    fn receipt_result_migrated_ids_are_ordinary_and_expiry_does_not_slide() {
        let mut db = database();
        let cutover = REQUEST_RESULT_TTL_MS * 2;
        db.execute(
            "INSERT INTO request_results(id,signature,result,created) VALUES('opaque-id','original','{\"reply\":7}',?1)",
            [cutover],
        )
        .expect("migration import");
        let tx = db.transaction().expect("tx");
        let row = RequestResults::replay(
            &tx,
            "opaque-id",
            "original",
            cutover + REQUEST_RESULT_TTL_MS - 1,
        )
        .expect("replay")
        .expect("retained");
        assert_eq!(row.result, "{\"reply\":7}");
        assert_eq!(row.created, cutover);
        assert!(
            RequestResults::replay(&tx, "unknown-id", "original", cutover)
                .expect("ordinary miss")
                .is_none()
        );
        assert!(matches!(
            RequestResults::replay(
                &tx,
                "opaque-id",
                "original",
                cutover + REQUEST_RESULT_TTL_MS
            ),
            Err(RequestResultError::Identity(
                RequestIdentityError::Expired { .. }
            ))
        ));
        assert!(RequestIdentity::new("namespace", &[], "unknown-id", &[]).is_err());
    }
    #[test]
    fn receipt_result_gc_is_bounded_and_rollback_clock_cannot_resurrect() {
        let mut db = database();
        let issued = REQUEST_RESULT_TTL_MS * 2;
        let id = identity(issued, "old");
        {
            let tx = db.transaction().expect("tx");
            increment(&tx, &id, "same", issued).expect("reply");
            tx.commit().expect("commit");
        }
        let tx = db.transaction().expect("tx");
        assert_eq!(
            RequestResults::gc(&tx, issued + REQUEST_RESULT_TTL_MS).expect("gc"),
            1
        );
        tx.rollback().expect("rollback GC");
        {
            let tx = db.transaction().expect("tx");
            assert_eq!(RequestResults::expired_before_ms(&tx).expect("floor"), 0);
            tx.commit().expect("commit");
        }
        {
            let tx = db.transaction().expect("tx");
            RequestResults::gc(&tx, issued + REQUEST_RESULT_TTL_MS).expect("gc");
            tx.commit().expect("commit");
        }
        let tx = db.transaction().expect("tx");
        assert!(matches!(
            RequestResults::admit(&tx, &id, "same", issued),
            Err(RequestResultError::Identity(
                RequestIdentityError::Expired { .. }
            ))
        ));
        for index in 0..REQUEST_RESULT_GC_BATCH_ROWS + 1 {
            tx.execute(
                "INSERT INTO request_results(id,signature,result,created) VALUES(?1,'sig','{}',?2)",
                params![index.to_string(), issued],
            )
            .expect("expired row");
        }
        assert_eq!(
            RequestResults::gc(&tx, issued).expect("rollback clock preserves cache TTL"),
            0
        );
        assert_eq!(
            RequestResults::gc(&tx, issued + REQUEST_RESULT_TTL_MS)
                .expect("bounded normal-clock GC"),
            REQUEST_RESULT_GC_BATCH_ROWS
        );
        assert_eq!(
            tx.query_row("SELECT count(*) FROM request_results", [], |row| row
                .get::<_, i64>(0))
                .expect("bounded remainder"),
            1
        );
    }
    #[test]
    fn receipt_result_future_issuance_is_not_pruned_early() {
        let mut db = database();
        let now = REQUEST_RESULT_TTL_MS * 2;
        let id = identity(now + REQUEST_ID_FUTURE_SKEW_MS, "future");
        let tx = db.transaction().expect("tx");
        let original = increment(&tx, &id, "same", now).expect("accepted skew");
        assert_eq!(original.created, now + REQUEST_ID_FUTURE_SKEW_MS);
        assert_eq!(
            RequestResults::gc(&tx, now + REQUEST_RESULT_TTL_MS).expect("gc"),
            0
        );
        assert!(
            RequestResults::replay(&tx, id.id(), "same", now + REQUEST_RESULT_TTL_MS)
                .expect("not expired")
                .is_some()
        );
    }

    #[derive(Debug, thiserror::Error)]
    enum CallerError {
        #[error(transparent)]
        Store(#[from] StoreError),
        #[error(transparent)]
        Request(#[from] RequestResultError),
    }
    #[tokio::test]
    async fn receipt_result_disposable_writer_concurrent_admission_serializes() {
        let directory = tempfile::tempdir().expect("disposable state");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("store"),
        );
        store
            .transaction(|tx| {
                crate::receipt_schema::create(tx)?;
                tx.execute_batch(
                    "CREATE TABLE counter(n INTEGER NOT NULL); INSERT INTO counter VALUES(0)",
                )?;
                Ok(())
            })
            .await
            .expect("caller schema");
        let now = RequestResults::expired_before_ms(&store.test_reader().expect("reader"))
            .expect("migration watermark")
            + 1;
        let id = identity(now, "concurrent");
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let store = Arc::clone(&store);
            let id = id.clone();
            tasks.push(tokio::spawn(async move {
                store
                    .transaction_with(move |tx| {
                        increment(tx, &id, "sig", now).map_err(CallerError::from)
                    })
                    .await
            }));
        }
        let first = tasks.remove(0).await.expect("caller").expect("result");
        let second = tasks.remove(0).await.expect("caller").expect("result");
        assert_eq!(first, second);
        assert_eq!(
            store
                .transaction(|tx| Ok(
                    tx.query_row("SELECT n FROM counter", [], |row| row.get::<_, i64>(0))?
                ))
                .await
                .expect("owner"),
            1
        );
    }
}
