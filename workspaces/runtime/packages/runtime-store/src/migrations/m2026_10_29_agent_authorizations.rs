use crate::StoreResult;
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    for (table, column, bases) in [
        (
            "runtime_agents",
            "record",
            &[
                "$",
                "$.nativeRelease.authorization.lead",
                "$.nativeRelease.authorization.target",
            ][..],
        ),
        (
            "runtime_outbox",
            "payload",
            &["$.authorization.lead", "$.authorization.target"][..],
        ),
    ] {
        if !crate::schema::table_exists(tx, table)? {
            continue;
        }
        for base in bases {
            let account = format!("{base}.accountKey");
            let sql = format!(
                "UPDATE {table} SET {column}=json_set({column},?1,?2) WHERE json_valid({column}) AND json_type({column},?3)='object' AND (json_type({column},?1) IS NULL OR json_type({column},?1)='null')"
            );
            tx.execute(
                &sql,
                rusqlite::params![account, crate::cost_aggregate::DEFAULT_ACCOUNT, base],
            )?;
            for field in ["threadId", "turnId"] {
                let path = format!("{base}.{field}");
                let sql = format!(
                    "UPDATE {table} SET {column}=json_remove({column},?1) WHERE json_valid({column}) AND json_type({column},?1)='null'"
                );
                tx.execute(&sql, [&path])?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    #[test]
    fn old_pending_authorizations_are_normalized() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL); CREATE TABLE runtime_outbox(id TEXT PRIMARY KEY, payload TEXT NOT NULL);").unwrap();
        let snapshot =
            r#"{"id":"a","rootId":"a","epoch":1,"status":"idle","threadId":null,"turnId":null}"#;
        let payload = format!(
            r#"{{"authorization":{{"lead":{snapshot},"target":{snapshot}}},"reply":{{"status":"unconfirmed"}}}}"#
        );
        let agent = format!(
            r#"{{"id":"a","rootId":"a","epoch":1,"status":"idle","threadId":null,"turnId":null,"nativeRelease":{{"authorization":{{"lead":{snapshot},"target":{snapshot}}}}}}}"#
        );
        db.execute("INSERT INTO runtime_agents VALUES('a',?1)", [&agent])
            .unwrap();
        db.execute(
            "INSERT INTO runtime_outbox VALUES('pending',?1)",
            [&payload],
        )
        .unwrap();
        let tx = db.transaction().unwrap();
        super::apply(&tx).unwrap();
        for (table, column, base) in [
            ("runtime_agents", "record", "$.nativeRelease.authorization"),
            ("runtime_outbox", "payload", "$.authorization"),
        ] {
            for who in ["lead", "target"] {
                let path = format!("{base}.{who}");
                let query = format!(
                    "SELECT json_extract({column},?1), json_type({column},?2), json_type({column},?3) FROM {table}"
                );
                let (account, thread, turn): (Option<String>, Option<String>, Option<String>) = tx
                    .query_row(
                        &query,
                        [
                            format!("{path}.accountKey"),
                            format!("{path}.threadId"),
                            format!("{path}.turnId"),
                        ],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .unwrap();
                assert_eq!(account.as_deref(), Some("default"));
                assert_eq!((thread, turn), (None, None));
            }
        }
    }
}
