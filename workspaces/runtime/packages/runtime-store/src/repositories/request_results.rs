//! Immutable exact replies. Call only after authorization, inside the owner's writer transaction.
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use studio_runtime_schema::persistence::{
    REQUEST_RESULT_TTL_MS, RequestIdentity, RequestIdentityError,
};

/// Maximum rows removed by one GC transaction. Unit: request-result rows.
pub const REQUEST_RESULT_GC_BATCH_ROWS: usize = 500;
pub(crate) const TABLE: &str = "request_results";

#[derive(Debug, thiserror::Error)]
pub enum RequestResultError {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredResult {
    pub id: String,
    pub signature: String,
    /// Exact stored JSON bytes, never reconstructed from mutable owner state.
    pub result: String,
    pub created: i64,
}

/// Persist this with the admitted owner/outbox for asynchronous settlement.
/// Deserialization is for trusted owner state, never unauthenticated ingress.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmittedRequest {
    id: String,
    signature: String,
    created: i64,
}
impl AdmittedRequest {
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
        if let Some(saved) = Self::stored(tx, &admitted.id, &admitted.signature)? {
            return Ok(saved);
        }
        let result = serde_json::to_string(result)?;
        tx.execute(
            "INSERT INTO request_results(id,signature,result,created) VALUES(?1,?2,?3,?4)",
            params![admitted.id, admitted.signature, result, admitted.created],
        )?;
        Ok(StoredResult {
            id: admitted.id.clone(),
            signature: admitted.signature.clone(),
            result,
            created: admitted.created,
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
            "INSERT INTO request_results VALUES('opaque-id','original','{\"reply\":7}',?1)",
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
                "INSERT INTO request_results VALUES(?1,'sig','{}',?2)",
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
        let now = REQUEST_RESULT_TTL_MS * 2;
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
