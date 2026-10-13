//! Local usage snapshot readers that do not require provider or turn drivers.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use thiserror::Error;

const COST_REFRESH_SECONDS: f64 = 120.0;

#[derive(Debug, Error, Eq, PartialEq)]
pub enum UsageError {
    #[error("This account's local cost history is unavailable.")]
    AccountUnavailable,
    #[error("The account has no local profile path.")]
    MissingProfile,
    #[error("Local cost cache could not be read.")]
    CacheRead,
}

/// Read an account's last locally scanned cost snapshot.
///
/// The response reports stale caches as stale and never invokes a scanner,
/// provider session, or billing endpoint.
pub fn account_cost_snapshot(
    state_dir: &Path,
    account_key: &str,
    account: &Value,
    now: f64,
) -> Result<Value, UsageError> {
    if account.get("status").and_then(Value::as_str) != Some("ready") {
        return Err(UsageError::AccountUnavailable);
    }
    let home = account
        .get("home")
        .and_then(Value::as_str)
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or(UsageError::MissingProfile)?;
    if account.get("provider").and_then(Value::as_str) == Some("claude")
        || account_key == "claude-local"
    {
        return claude_cost_snapshot(state_dir, account_key, &home, now);
    }
    let account_id = account.get("accountId").cloned().unwrap_or(Value::Null);
    let home_text = home.to_string_lossy().into_owned();
    let account_root = state_dir.join("account-costs").join(digest(&python_array(&[
        serde_json::to_value(account_key).map_err(|_| UsageError::CacheRead)?,
        account_id.clone(),
        serde_json::to_value(&home_text).map_err(|_| UsageError::CacheRead)?,
    ])));
    let cache_scope = digest(&python_array(&[
        serde_json::to_value(&home_text).map_err(|_| UsageError::CacheRead)?,
        serde_json::to_value(account_root.to_string_lossy().as_ref())
            .map_err(|_| UsageError::CacheRead)?,
    ]));
    let saved = read_json(&account_root.join("local-costs.json"));
    let mut state = saved
        .filter(|saved| saved.get("scope").and_then(Value::as_str) == Some(&cache_scope))
        .and_then(|saved| saved.get("state").cloned())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({"at": null, "error": null, "data": null}));
    let checked_at = state
        .get("checkedAt")
        .and_then(Value::as_f64)
        .or_else(|| state.get("at").and_then(Value::as_f64));
    let stale = checked_at.is_none_or(|at| now - at >= COST_REFRESH_SECONDS)
        || state.get("error").is_some_and(|error| !error.is_null());
    if let Some(data) = state.get_mut("data").and_then(Value::as_object_mut) {
        data.insert("accountId".to_owned(), account_id);
        data.insert(
            "source".to_owned(),
            Value::String("Codex profile logs".to_owned()),
        );
        data.insert("scope".to_owned(), Value::String("account".to_owned()));
        data.insert(
            "note".to_owned(),
            Value::String("API-rate estimate".to_owned()),
        );
        data.insert("kind".to_owned(), Value::String("api_estimate".to_owned()));
    }
    let mut response = state.as_object().cloned().unwrap_or_default();
    response.insert("refreshing".to_owned(), Value::Bool(false));
    response.insert("stale".to_owned(), Value::Bool(stale));
    response.insert(
        "accountKey".to_owned(),
        Value::String(account_key.to_owned()),
    );
    Ok(Value::Object(response))
}

fn claude_cost_snapshot(
    state_dir: &Path,
    account_key: &str,
    home: &Path,
    now: f64,
) -> Result<Value, UsageError> {
    let identity = format!("{account_key}{}", home.to_string_lossy());
    let path = state_dir
        .join("account-costs/claude")
        .join(digest(&identity))
        .join("claude-costs.json");
    let saved = read_json(&path);
    let data = saved
        .as_ref()
        .filter(|saved| {
            saved.get("config").and_then(Value::as_str) == Some(home.to_string_lossy().as_ref())
        })
        .and_then(|saved| saved.get("data"))
        .filter(|value| value.is_object())
        .cloned();
    let checked = saved
        .as_ref()
        .and_then(|saved| saved.get("checked"))
        .cloned()
        .unwrap_or(Value::Null);
    let stale = data.is_none()
        || checked
            .as_f64()
            .is_none_or(|checked| now - checked >= 300.0)
        || saved
            .as_ref()
            .and_then(|saved| saved.get("error"))
            .is_some_and(|error| !error.is_null());
    Ok(json!({
        "at": checked,
        "error": null,
        "data": data,
        "refreshing": false,
        "stale": stale,
        "accountKey": account_key,
    }))
}

fn read_json(path: &Path) -> Option<Value> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn python_array(values: &[Value]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn digest(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::account_cost_snapshot;
    use serde_json::json;

    #[test]
    fn reads_python_shaped_local_cache_without_starting_a_refresh() {
        let root = std::env::temp_dir().join(format!("usage-costs-{}", std::process::id()));
        let home = root.join("profile");
        let account_root = root.join("state/account-costs");
        let key = "default";
        let account_id = "account-1";
        let account_hash = super::digest(&format!(
            "[\"{key}\", \"{account_id}\", \"{}\"]",
            home.to_string_lossy()
        ));
        let cache = account_root.join(account_hash);
        let scope = super::digest(&format!(
            "[\"{}\", \"{}\"]",
            home.to_string_lossy(),
            cache.to_string_lossy()
        ));
        std::fs::create_dir_all(&cache).expect("fixture directory");
        std::fs::write(
            cache.join("local-costs.json"),
            json!({"scope":scope,"state":{"at":1,"checkedAt":1,"error":null,
                "data":{"todayUSD":2.5,"sourceDay":"1970-01-01"}}})
            .to_string(),
        )
        .expect("fixture cache");

        let response = account_cost_snapshot(
            &root.join("state"),
            key,
            &json!({"id":key,"accountId":account_id,"home":home,"status":"ready"}),
            122.0,
        )
        .expect("cache read");
        assert_eq!(response["data"]["todayUSD"], 2.5);
        assert_eq!(response["data"]["source"], "Codex profile logs");
        assert_eq!(response["refreshing"], false);
        assert_eq!(response["stale"], true);
        std::fs::remove_dir_all(root).expect("fixture cleanup");
    }
}
