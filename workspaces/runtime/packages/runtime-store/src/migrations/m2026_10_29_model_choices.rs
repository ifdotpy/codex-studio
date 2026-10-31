//! Drop the retired exclusive model allowlist; every model starts untouched.
use rusqlite::Transaction;
use serde_json::Value;
use studio_runtime_schema::shared_values::RETIRED_MODEL_WHITELIST_KEY;

use crate::{StoreResult, SyncDocumentsRepository};

pub(super) fn apply(transaction: &Transaction<'_>) -> StoreResult<()> {
    let Some(payload) = SyncDocumentsRepository::preference_payload(transaction, "server")? else {
        return Ok(());
    };
    let mut envelope: Value = serde_json::from_str(&payload)?;
    let name = serde_json::to_string(&[RETIRED_MODEL_WHITELIST_KEY])?;
    let removed = envelope
        .pointer_mut("/value/fields")
        .and_then(Value::as_object_mut)
        .and_then(|fields| fields.remove(&name));
    if removed.is_some() {
        SyncDocumentsRepository::put_preferences(
            transaction,
            "server",
            &serde_json::to_string(&envelope)?,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::OpenFlags;

    use crate::{RuntimeStore, SyncDocumentsRepository};

    #[tokio::test]
    async fn retired_model_whitelist_is_removed_and_other_fields_stay() {
        let root = tempfile::tempdir().expect("state");
        drop(RuntimeStore::open(Some(root.path())).await.expect("fresh"));
        let canvas = root.path().join("canvas.sqlite3");
        let mut db =
            crate::open_database(&canvas, OpenFlags::SQLITE_OPEN_READ_WRITE).expect("canvas opens");
        crate::schema::register_python_trigger_functions(&db).expect("trigger functions");
        let field = r#"{"value":true,"timestamp":1,"writer":"test"}"#;
        let transaction = db.transaction().expect("transaction");
        SyncDocumentsRepository::put_preferences(
            &transaction,
            "server",
            &format!(
                r#"{{"collection":"uiPreferences","id":"server","value":{{"fields":{{"[\"codex-worker-disclosures\"]":{field},"[\"studio-model-whitelist-v1\"]":{field}}}}}}}"#
            ),
        )
        .expect("legacy preferences write");
        super::apply(&transaction).expect("remove retired preference");
        super::apply(&transaction).expect("repeat migration");
        transaction.commit().expect("commit");
        drop(db);
        let db = crate::open_database(&canvas, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("reader");
        let payload: serde_json::Value = serde_json::from_str(
            &SyncDocumentsRepository::preference_payload(&db, "server")
                .expect("read")
                .expect("preferences"),
        )
        .expect("json");
        let fields = payload["value"]["fields"].as_object().expect("fields");
        assert_eq!(
            fields.keys().collect::<Vec<_>>(),
            [r#"["codex-worker-disclosures"]"#]
        );
    }
}
