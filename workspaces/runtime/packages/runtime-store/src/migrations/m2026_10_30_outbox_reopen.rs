//! A card's answer reply kept the whole unanswered card in its outbox payload
//! (`unanswered`) to restore it when the provider connection refused the reply.
//! The payload keeps a typed `reopen` marker now: the approval returns to
//! pending with its expiry timer. Rows written before the timer existed have
//! none.
use rusqlite::{Transaction, params};
use studio_runtime_schema::persistence::OutboxKind;

use crate::StoreResult;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    if !crate::schema::table_exists(tx, "runtime_outbox")? {
        return Ok(());
    }
    tx.execute(
        "UPDATE runtime_outbox SET payload=CASE
             WHEN json_type(payload,'$.unanswered')='object'
             THEN json_set(json_remove(payload,'$.unanswered'),'$.reopen',json('{\"fire_at\":null}'))
             ELSE json_remove(payload,'$.unanswered') END
         WHERE kind=?1 AND json_type(payload,'$.unanswered') IS NOT NULL",
        params![OutboxKind::ProviderCallback],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn a_pending_answer_keeps_a_reopen_marker_instead_of_the_whole_card() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_outbox(id TEXT PRIMARY KEY, kind TEXT NOT NULL, payload TEXT NOT NULL)")
            .unwrap();
        for (id, payload) in [
            (
                "card",
                r#"{"card":"c","unanswered":{"id":"c","status":"pending"},"body":{"result":{}}}"#,
            ),
            (
                "plain",
                r#"{"card":null,"unanswered":null,"body":{"result":{}}}"#,
            ),
            ("fresh", r#"{"card":"d","reopen":{"fire_at":5.0}}"#),
        ] {
            db.execute(
                "INSERT INTO runtime_outbox VALUES (?1,?2,?3)",
                params![id, OutboxKind::ProviderCallback, payload],
            )
            .unwrap();
        }
        let tx = db.transaction().unwrap();
        apply(&tx).unwrap();
        apply(&tx).unwrap();
        let get = |id: &str| -> serde_json::Value {
            let text: String = tx
                .query_row(
                    "SELECT payload FROM runtime_outbox WHERE id=?1",
                    [id],
                    |r| r.get(0),
                )
                .unwrap();
            serde_json::from_str(&text).unwrap()
        };
        assert_eq!(get("card")["reopen"], serde_json::json!({"fire_at":null}));
        assert!(get("card").get("unanswered").is_none());
        assert!(get("plain").get("unanswered").is_none());
        assert!(get("plain").get("reopen").is_none());
        assert_eq!(get("fresh")["reopen"]["fire_at"], serde_json::json!(5.0));
    }
}
