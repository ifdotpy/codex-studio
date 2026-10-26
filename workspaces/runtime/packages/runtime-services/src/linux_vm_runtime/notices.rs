//! Chat notices about an automatic VM restart (Python `drop_providers` and
//! `post_notices` of `codex_linux_vm_watchdog.py`): each VM chat with a live
//! turn in the dead boot shows how the restart ended.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use studio_runtime_store::RuntimeStore;

/// The restart outcomes that a chat notice reports.
pub(super) const NOTICED_OUTCOMES: [&str; 3] = ["restarted", "cancelled", "failed"];

/// The chats of a VM and their restart notices.
pub trait VmChatNotices: Send + Sync {
    /// The VM chats with a live turn, before an automatic restart stops the VM.
    fn chats(&self) -> BoxFuture<'_, Result<Vec<String>, String>>;
    /// Show one finished automatic restart in each of its chats.
    fn post(&self, record: Map<String, Value>) -> BoxFuture<'_, Result<(), String>>;
}

/// The text of the notice of a finished restart record.
pub(super) fn notice_text(record: &Map<String, Value>) -> String {
    let text = |field: &str| record.get(field).and_then(Value::as_str);
    let at = record
        .get("finishedAt")
        .or_else(|| record.get("at"))
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let when = chrono::DateTime::from_timestamp(at as i64, 0)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%H:%M")
                .to_string()
        })
        .unwrap_or_default();
    let lead = format!(
        "The VM stopped working ({}). ",
        text("reason").unwrap_or("no reason")
    );
    match text("outcome") {
        Some("restarted") => format!(
            "{lead}Studio restarted it at {when}. Your next message starts this chat again. \
             If a turn was running, its result is unknown: review it before you continue."
        ),
        Some("cancelled") => {
            format!("{lead}It was stopped on purpose, so Studio did not start it again.")
        }
        _ => format!(
            "{lead}Studio could not restart it: {}. Start it with \
             `studio-runtime-server linux-vm start`, then send your next message.",
            text("error").unwrap_or("no reason")
        ),
    }
}

/// The chats and notices of the runtime store.
pub struct StoreChatNotices {
    store: Arc<RuntimeStore>,
}

impl StoreChatNotices {
    pub fn new(store: Arc<RuntimeStore>) -> Self {
        Self { store }
    }
}

impl VmChatNotices for StoreChatNotices {
    fn chats(&self) -> BoxFuture<'_, Result<Vec<String>, String>> {
        Box::pin(async move {
            self.store
                .read_async("runtime-services/linux_vm_notices", |tx| {
                    let mut statement = tx.prepare(
                        "SELECT id FROM runtime_agents WHERE deleted_at IS NULL \
                         AND json_extract(record,'$.executionMode')='vm' \
                         AND (status IN ('running','starting','approval') OR in_flight=1) \
                         ORDER BY id",
                    )?;
                    let ids = statement
                        .query_map([], |row| row.get::<_, String>(0))?
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok::<_, studio_runtime_store::StoreError>(ids)
                })
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn post(&self, record: Map<String, Value>) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let chats: Vec<String> = record
                .get("chats")
                .and_then(Value::as_array)
                .map(|chats| {
                    chats
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let id = record
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let text = notice_text(&record);
            let at = record
                .get("finishedAt")
                .or_else(|| record.get("at"))
                .and_then(Value::as_f64)
                .unwrap_or_default();
            self.store
                .transaction_with(move |tx| {
                    for agent in &chats {
                        let deleted: Option<bool> = tx
                            .query_row(
                                "SELECT deleted_at IS NOT NULL FROM runtime_agents WHERE id=?1",
                                [agent],
                                |row| row.get(0),
                            )
                            .ok();
                        if deleted != Some(false) {
                            continue;
                        }
                        let item = format!("{agent}:linux-vm-restart:{id}");
                        let notice = json!({"id": item, "role": "system", "title": "VM",
                            "text": text, "truncated": false, "at": at, "nativeNotice": "error"});
                        studio_runtime_store::NotificationDispatchRepository::put_transcript_item(
                            tx, &item, agent, &notice, at, &text, true,
                        )?;
                    }
                    Ok::<_, studio_runtime_store::StoreError>(())
                })
                .await
                .map_err(|error| error.to_string())
        })
    }
}
