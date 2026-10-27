//! Callback identity survives cached reply expiry until its generation is fenced.
use super::ToolRequestError;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use studio_runtime_schema::persistence::{ToolRequestKind, ToolRequestStatus};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallbackTask<T> {
    pub id: String,
    #[serde(rename = "originKey")]
    pub origin_key: String,
    pub status: ToolRequestStatus,
    pub updated: i64,
    pub payload: T,
}

pub fn get_callback<T: DeserializeOwned>(
    db: &Connection,
    origin_key: &str,
) -> Result<Option<CallbackTask<T>>, ToolRequestError> {
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM runtime_tool_requests WHERE origin_key=?1 AND kind=?2",
            params![origin_key, ToolRequestKind::ProviderCallback],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|raw| {
        serde_json::from_str(&raw)
            .map_err(studio_runtime_store::StoreError::from)
            .map_err(Into::into)
    })
    .transpose()
}

pub fn put_callback<T: Serialize>(
    tx: &Transaction<'_>,
    task: &CallbackTask<T>,
) -> Result<(), ToolRequestError> {
    let mut record = serde_json::to_value(task).map_err(studio_runtime_store::StoreError::from)?;
    record["kind"] = serde_json::to_value(ToolRequestKind::ProviderCallback)
        .map_err(studio_runtime_store::StoreError::from)?;
    // Both keys are immutable: a reconnect cannot allocate another business operation.
    let count = tx.execute(
        "INSERT INTO runtime_tool_requests(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record WHERE runtime_tool_requests.origin_key=?3 AND runtime_tool_requests.kind=?4",
        params![task.id, record.to_string(),task.origin_key,ToolRequestKind::ProviderCallback],
    )?;
    if count != 1 {
        return Err(ToolRequestError::InvalidRequest(
            "Callback identity already belongs to another origin",
        ));
    }
    Ok(())
}

/// Call only after the provider generation has been conclusively fenced.
pub fn delete_callback(tx: &Transaction<'_>, origin_key: &str) -> Result<(), ToolRequestError> {
    tx.execute(
        "DELETE FROM runtime_tool_requests WHERE origin_key=?1 AND kind=?2",
        params![origin_key, ToolRequestKind::ProviderCallback],
    )?;
    Ok(())
}
