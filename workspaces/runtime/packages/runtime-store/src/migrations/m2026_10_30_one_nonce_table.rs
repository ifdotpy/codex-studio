//! Move federation nonces into the one nonce table of the unified signature scheme.
//! Each `(peer_id, nonce, created)` row becomes `('federation:' || peer_id, nonce, created)`
//! in `runtime_access_nonces` (created by the earlier canvas feature migration), so a nonce seen before the update stays a replay after it.
use rusqlite::Transaction;

use crate::{StoreResult, repositories::remote_transport::FEDERATION_NONCE_CLIENT_PREFIX};

pub(super) fn apply(transaction: &Transaction<'_>) -> StoreResult<()> {
    if !crate::schema::table_exists(transaction, "runtime_federation_nonces")? {
        return Ok(());
    }
    transaction.execute(
        "INSERT OR IGNORE INTO runtime_access_nonces(client,nonce,created) \
         SELECT ?1||peer_id,nonce,created FROM runtime_federation_nonces",
        [FEDERATION_NONCE_CLIENT_PREFIX],
    )?;
    transaction.execute_batch("DROP TABLE runtime_federation_nonces")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    #[test]
    fn federation_nonces_move_without_loss_and_the_old_table_goes() {
        let mut db = Connection::open_in_memory().expect("database opens");
        db.execute_batch(
            "CREATE TABLE runtime_federation_nonces(peer_id TEXT NOT NULL, nonce TEXT NOT NULL, created INTEGER NOT NULL, PRIMARY KEY(peer_id,nonce));
             CREATE TABLE runtime_access_nonces(client TEXT NOT NULL, nonce TEXT NOT NULL, created INTEGER NOT NULL, PRIMARY KEY(client,nonce));
             INSERT INTO runtime_federation_nonces VALUES('peer-a','n1',100),('peer-a','n2',200),('peer-b','n1',300);",
        )
        .expect("old table fixture");
        let tx = db.transaction().expect("transaction starts");
        super::apply(&tx).expect("migration applies");
        super::apply(&tx).expect("migration is idempotent");
        tx.commit().expect("commit");
        let rows: Vec<(String, String, i64)> = db
            .prepare("SELECT client,nonce,created FROM runtime_access_nonces ORDER BY client,nonce")
            .expect("prepare")
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert_eq!(
            rows,
            [
                ("federation:peer-a".into(), "n1".into(), 100),
                ("federation:peer-a".into(), "n2".into(), 200),
                ("federation:peer-b".into(), "n1".into(), 300),
            ]
        );
        let old: bool =
            crate::schema::table_exists(&db, "runtime_federation_nonces").expect("exists check");
        assert!(!old);
    }
}
