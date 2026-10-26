//! Ephemeral native voice display. Voice transcript bytes never enter SQLite.
use crate::delivery::StoreSender;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{Mutex, OnceLock},
};

/// Maximum records retained in one ephemeral native transcript display.
const VOICE_VIEW_MAX_RECORDS: usize = 2048;
/// Maximum JSON record bytes retained in one ephemeral display. Unit: bytes.
const VOICE_VIEW_MAX_BYTES: usize = 2 * 1024 * 1024;
/// Maximum chat displays retained across currently open native Store owners.
const VOICE_VIEW_MAX_OWNERS: usize = 64;
/// Maximum remembered source epochs before this owner must be reopened.
const VOICE_VIEW_MAX_RETIRED_EPOCHS: usize = 64;
struct View {
    store: tokio::sync::mpsc::WeakSender<crate::delivery::StoreCommand>,
    agent: String,
    epoch: String,
    retired: BTreeSet<String>,
    cursor: u64,
    gap: bool,
    trimmed: bool,
    bytes: usize,
    records: Vec<Value>,
    session: Value,
}
fn views() -> &'static Mutex<Vec<View>> {
    static VIEWS: OnceLock<Mutex<Vec<View>>> = OnceLock::new();
    VIEWS.get_or_init(Mutex::default)
}

pub async fn apply_records(
    store: &StoreSender,
    agent: &str,
    epoch: &str,
    batch: &Value,
) -> Result<Value, String> {
    if agent.is_empty() || agent.len() > 256 || epoch.is_empty() || epoch.len() > 128 {
        return Err("Invalid native voice display owner.".into());
    }
    let after = batch["after"]
        .as_u64()
        .ok_or("Invalid voice display cursor.")?;
    let cursor = batch["cursor"]
        .as_u64()
        .filter(|n| *n >= after && *n <= studio_client_domain::MAX_SAFE_INTEGER as u64)
        .ok_or("Invalid voice display cursor.")?;
    let records = batch["records"]
        .as_array()
        .filter(|rows| {
            rows.len()
                <= studio_runtime_schema::shared_values::SYNC_VOICE_STREAM_MAX_RECORDS as usize
        })
        .ok_or("The voice batch exceeds its source bound.")?;
    for record in records {
        if record["id"]
            .as_str()
            .is_none_or(|id| id.is_empty() || id.chars().count() > studio_runtime_schema::shared_values::MAX_VOICE_IDENTITY_CHARS)
            || record["agent"] != agent
        {
            return Err("The voice record belongs to another display owner.".into());
        }
        if record["seq"].as_u64().is_none_or(|seq| seq > cursor) {
            return Err("The voice record cursor is invalid.".into());
        }
    }
    let mut views = views()
        .lock()
        .map_err(|_| "The native voice display stopped.")?;
    views.retain(|view| view.store.strong_count() > 0);
    let index = views.iter().position(|view| {
        view.agent == agent
            && view
                .store
                .upgrade()
                .is_some_and(|owner| owner.same_channel(store))
    });
    let mut view = if let Some(index) = index {
        views.remove(index)
    } else {
        if views.len() >= VOICE_VIEW_MAX_OWNERS {
            views.remove(0);
        }
        View {
            store: store.downgrade(),
            agent: agent.into(),
            epoch: epoch.into(),
            retired: BTreeSet::new(),
            cursor: 0,
            gap: false,
            trimmed: false,
            bytes: 0,
            records: vec![],
            session: Value::Null,
        }
    };
    if view.epoch != epoch {
        if view.retired.contains(epoch) || view.retired.len() >= VOICE_VIEW_MAX_RETIRED_EPOCHS {
            views.push(view);
            return Err(
                "The voice display received a retired source epoch. Reopen this server connection."
                    .into(),
            );
        }
        view.retired.insert(view.epoch.clone());
        view.epoch = epoch.into();
        view.cursor = 0;
        view.gap = false;
        view.trimmed = false;
        view.bytes = 0;
        view.records.clear();
        view.session = Value::Null;
    }
    if cursor >= view.cursor {
        if after > view.cursor {
            view.gap = true;
        }
        for record in records {
            if let Some(index) = view
                .records
                .iter()
                .position(|saved| saved["id"] == record["id"])
            {
                if record["seq"].as_u64() < view.records[index]["seq"].as_u64() {
                    continue;
                }
                view.bytes -= view.records[index].to_string().len();
                view.records[index] = record.clone();
            } else {
                view.records.push(record.clone());
            }
            view.bytes += record.to_string().len();
        }
        while view.records.len() > VOICE_VIEW_MAX_RECORDS || view.bytes > VOICE_VIEW_MAX_BYTES {
            view.bytes -= view.records.remove(0).to_string().len();
            view.trimmed = true;
        }
        view.cursor = cursor;
        view.session = batch["session"].clone();
    }
    let result = json!({"after":0,"cursor":view.cursor,"records":view.records,"session":view.session,"view":{
        "maxRecords":VOICE_VIEW_MAX_RECORDS,"maxBytes":VOICE_VIEW_MAX_BYTES,"trimmed":view.trimmed,"gap":view.gap,"partial":true}});
    views.push(view);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn native_voice_view_uses_the_server_identity_character_boundary() {
        let limit = studio_runtime_schema::shared_values::MAX_VOICE_IDENTITY_CHARS;
        for character in ['a', '🦀'] {
            let (store, _receive) = tokio::sync::mpsc::channel(1);
            let id = character.to_string().repeat(limit);
            assert_eq!(id.chars().count(), limit);
            let batch = json!({"after":0,"cursor":1,"records":[{
                "id":id,"agent":"chat","seq":1,"kind":"user","text":"saved"
            }],"session":null});
            let accepted = apply_records(&store, "chat", "epoch", &batch).await
                .expect("the native view must accept every server-valid record identity");
            assert_eq!(accepted["records"][0]["id"], batch["records"][0]["id"]);
            let mut too_long = batch.clone();
            too_long["records"][0]["id"] = json!(character.to_string().repeat(limit + 1));
            assert!(apply_records(&store, "chat", "epoch", &too_long).await.is_err());
            let mut empty = batch.clone();
            empty["records"][0]["id"] = json!("");
            assert!(apply_records(&store, "chat", "epoch", &empty).await.is_err());
        }
    }

    #[tokio::test]
    async fn native_voice_view_keeps_overlap_and_marks_gaps_without_crossing_owner_or_epoch() {
        let (store, _receive) = tokio::sync::mpsc::channel(1);
        let (sibling, _other) = tokio::sync::mpsc::channel(1);
        let first = json!({"after":0,"cursor":1,"records":[{"id":"a","agent":"chat","seq":1,"kind":"user","text":"original"}],"session":null});
        apply_records(&store, "chat", "first", &first)
            .await
            .unwrap();
        let second = json!({"after":1,"cursor":2,"records":[{"id":"b","agent":"chat","seq":2,"kind":"assistant","text":"answer"}],"session":null});
        let merged = apply_records(&store, "chat", "first", &second)
            .await
            .unwrap();
        assert_eq!(merged["records"].as_array().unwrap().len(), 2);
        assert_eq!(merged["view"]["gap"], false);
        let gap = json!({"after":4,"cursor":5,"records":[{"id":"c","agent":"chat","seq":5,"kind":"user","text":"later"}],"session":null});
        assert_eq!(
            apply_records(&store, "chat", "first", &gap).await.unwrap()["view"]["gap"],
            true
        );
        assert_eq!(
            apply_records(&sibling, "chat", "first", &second)
                .await
                .unwrap()["records"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(
            apply_records(&store, "other-chat", "first", &second)
                .await
                .is_err()
        );
        let reset = apply_records(&store, "chat", "new-epoch", &first)
            .await
            .unwrap();
        assert_eq!(reset["records"].as_array().unwrap().len(), 1);
        assert!(
            apply_records(&store, "chat", "first", &second)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn native_voice_view_reports_its_native_retention_bound() {
        let (store, _receive) = tokio::sync::mpsc::channel(1);
        let mut result = Value::Null;
        let count = VOICE_VIEW_MAX_RECORDS + 1;
        let mut after = 0usize;
        for chunk in (0..count)
            .collect::<Vec<_>>()
            .chunks(studio_runtime_schema::shared_values::SYNC_VOICE_STREAM_MAX_RECORDS as usize)
        {
            let records=chunk.iter().map(|i|json!({"id":format!("r-{i}"),"agent":"chat","seq":i+1,"kind":"user","text":"saved"})).collect::<Vec<_>>();
            let cursor = chunk.last().unwrap() + 1;
            result = apply_records(
                &store,
                "chat",
                "epoch",
                &json!({"after":after,"cursor":cursor,"records":records,"session":null}),
            )
            .await
            .unwrap();
            after = cursor;
        }
        assert_eq!(
            result["records"].as_array().unwrap().len(),
            VOICE_VIEW_MAX_RECORDS
        );
        assert_eq!(result["view"]["maxRecords"], VOICE_VIEW_MAX_RECORDS);
        assert_eq!(result["view"]["maxBytes"], VOICE_VIEW_MAX_BYTES);
        assert_eq!(result["view"]["trimmed"], true);
        assert_eq!(result["view"]["partial"], true);
    }
}
