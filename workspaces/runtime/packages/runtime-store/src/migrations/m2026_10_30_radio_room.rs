use crate::StoreResult;
use rusqlite::Transaction;

pub fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    if !crate::schema::table_exists(tx, "runtime_rooms")? {
        return Ok(());
    }
    tx.execute("UPDATE runtime_rooms SET record=json_set(record,'$.radio.direct',json('false')) WHERE json_valid(record) AND json_type(record,'$.radio')='object' AND (json_type(record,'$.radio.direct') IS NULL OR json_type(record,'$.radio.direct')='null')", [])?;
    tx.execute("UPDATE runtime_rooms SET record=json_set(record,'$.radio.active',NULL) WHERE json_valid(record) AND json_type(record,'$.radio')='object' AND json_type(record,'$.radio.active') IS NULL", [])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn canonicalizes_only_missing_radio_fields() {
        let mut db = Connection::open_in_memory().expect("radio migration fixture");
        db.execute_batch("CREATE TABLE runtime_rooms(id TEXT PRIMARY KEY,record TEXT NOT NULL); INSERT INTO runtime_rooms VALUES('radio:a','{\"id\":\"radio:a\",\"radio\":{\"teamId\":\"a\",\"opaque\":1.0}}'),('radio:b','{\"id\":\"radio:b\",\"radio\":{\"direct\":true,\"active\":null}}'),('other','{\"id\":\"other\"}');").expect("radio migration fixture");
        let tx = db.transaction().expect("radio migration fixture");
        apply(&tx).expect("radio migration fixture");
        apply(&tx).expect("radio migration fixture");
        let a: String = tx
            .query_row(
                "SELECT record FROM runtime_rooms WHERE id='radio:a'",
                [],
                |row| row.get(0),
            )
            .expect("radio migration fixture");
        assert_eq!(
            a,
            r#"{"id":"radio:a","radio":{"teamId":"a","opaque":1.0,"direct":false,"active":null}}"#
        );
        let b: String = tx
            .query_row(
                "SELECT record FROM runtime_rooms WHERE id='radio:b'",
                [],
                |row| row.get(0),
            )
            .expect("radio migration fixture");
        assert_eq!(
            b,
            r#"{"id":"radio:b","radio":{"direct":true,"active":null}}"#
        );
        let other: String = tx
            .query_row(
                "SELECT record FROM runtime_rooms WHERE id='other'",
                [],
                |row| row.get(0),
            )
            .expect("radio migration fixture");
        assert_eq!(other, r#"{"id":"other"}"#);
    }
}
