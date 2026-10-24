//! Durable per-account model catalog entities.

use serde_json::{Value, json};
use studio_runtime_services::model_catalog::ChangeCallback;
use studio_runtime_store::{RuntimeStore, StoreError, SyncDocumentsRepository, Transaction};

const MODEL_COLLECTION: &str = "model";
const MODEL_ID_SEPARATOR: &str = ":";

pub(crate) fn change_callback(store: Option<std::sync::Arc<RuntimeStore>>) -> ChangeCallback {
    std::sync::Arc::new(move |account, _home, provider, catalog| {
        let Some(store) = store.clone() else { return };
        let account = account.to_owned();
        let provider = provider.to_owned();
        let publication_account = account.clone();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            eprintln!("Could not publish model entities for {account}: no async runtime");
            return;
        };
        runtime.spawn(async move {
            let result = store
                .transaction_with(move |transaction| {
                    publish_catalog(
                        transaction,
                        &publication_account,
                        &provider,
                        catalog.as_ref(),
                    )
                })
                .await;
            match result {
                // The committed entity rows publish through RuntimeStore's
                // entity-only change observer; no resource invalidation is needed.
                Ok(true) => {}
                Ok(false) => {}
                Err(error) => {
                    eprintln!("Could not publish model entities for {account}: {error}");
                }
            }
        });
    })
}

fn publish_catalog(
    transaction: &Transaction<'_>,
    account: &str,
    provider: &str,
    catalog: Option<&Value>,
) -> Result<bool, StoreError> {
    let prefix = format!("{account}{MODEL_ID_SEPARATOR}");
    let mut next_ids = std::collections::HashSet::new();
    let mut changed = false;
    if let Some(rows) = catalog
        .and_then(|value| value.get("data"))
        .and_then(Value::as_array)
    {
        for (position, row) in rows.iter().enumerate() {
            let Some(model) = row
                .get("model")
                .and_then(Value::as_str)
                .filter(|model| !model.is_empty())
            else {
                continue;
            };
            let id = format!("{prefix}{model}");
            let mut value = row.clone();
            let Some(value) = value.as_object_mut() else {
                continue;
            };
            value.insert("accountKey".into(), Value::String(account.to_owned()));
            value.insert("provider".into(), Value::String(provider.to_owned()));
            value.insert("catalogPosition".into(), json!(position));
            changed |= SyncDocumentsRepository::put_entity(
                transaction,
                MODEL_COLLECTION,
                &id,
                Value::Object(value.clone()),
                false,
            )?;
            next_ids.insert(id);
        }
    }

    let mut statement = transaction
        .prepare_cached("SELECT id FROM sync_entities WHERE collection=?1 AND deleted=0")?;
    let existing = statement
        .query_map([MODEL_COLLECTION], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for id in existing
        .into_iter()
        .filter(|id| id.starts_with(&prefix) && !next_ids.contains(id))
    {
        changed |= SyncDocumentsRepository::put_entity(
            transaction,
            MODEL_COLLECTION,
            &id,
            Value::Null,
            true,
        )?;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use studio_runtime_store::RuntimeStore;

    #[tokio::test]
    async fn publishes_full_per_account_rows_and_tombstones_removed_models() {
        let directory = tempfile::tempdir().expect("isolated runtime state");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("runtime store opens");
        let initial = json!({"data":[
            {"model":"alpha","hidden":false,"description":"Alpha"},
            {"model":"hidden","hidden":true}
        ]});
        assert!(
            store
                .transaction_with(move |tx| publish_catalog(tx, "one", "codex", Some(&initial)))
                .await
                .expect("catalog publishes")
        );
        let second_account = json!({"data":[{"model":"alpha","hidden":false}]});
        store
            .transaction_with(move |tx| publish_catalog(tx, "two", "claude", Some(&second_account)))
            .await
            .expect("second account publishes");

        let replacement = json!({"data":[{"model":"alpha","hidden":true}]});
        store
            .transaction_with(move |tx| publish_catalog(tx, "one", "codex", Some(&replacement)))
            .await
            .expect("catalog replacement publishes");

        let rows = store
            .test_read_transaction(|tx| {
                let mut statement = tx.prepare(
                    "SELECT id,deleted,payload FROM sync_entities WHERE collection='model' ORDER BY id",
                )?;
                let rows = statement
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, bool>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .expect("model entities read");
        assert_eq!(rows.len(), 3);
        let one_alpha = rows
            .iter()
            .find(|(id, _, _)| id == "one:alpha")
            .expect("updated model retained");
        assert!(!one_alpha.1);
        let one_value: Value = serde_json::from_str(&one_alpha.2).expect("entity payload JSON");
        assert_eq!(one_value["value"]["accountKey"], "one");
        assert_eq!(one_value["value"]["provider"], "codex");
        assert_eq!(one_value["value"]["hidden"], true);
        assert_eq!(one_value["value"]["catalogPosition"], 0);
        assert!(
            rows.iter()
                .any(|(id, deleted, _)| id == "one:hidden" && *deleted)
        );
        assert!(
            rows.iter()
                .any(|(id, deleted, _)| id == "two:alpha" && !deleted)
        );
    }
}
