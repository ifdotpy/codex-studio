//! Pure sync document, cursor and preference decisions.

use serde_json::{Map, Value, json};
use std::time::{SystemTime, UNIX_EPOCH};
use studio_runtime_store::{RuntimeStore, StoreError, SyncDocumentsRepository};
use thiserror::Error;

pub const ENTITY_PAGE_LIMIT: i64 = 500;
pub const OTHER_PAGE_LIMIT: i64 = 100;
const PREFERENCE_FIELD_CAP: usize = 3000;
const MAX_CLOCK_SKEW_MS: i64 = 5 * 60 * 1000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreferenceScope {
    User,
    Server,
}

#[derive(Debug, Error)]
pub enum SyncPolicyError {
    #[error("invalid draft")]
    InvalidDraft,
    #[error("draft identity does not match its key")]
    DraftIdentity,
    #[error("invalid preference fields")]
    InvalidPreferences,
    #[error("invalid stored sync object {scope}/{id}")]
    InvalidStoredObject { scope: String, id: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub fn effective_limit(scope: &str, limit: i64) -> i64 {
    limit.clamp(
        1,
        if scope == "state:entities:v1" {
            ENTITY_PAGE_LIMIT
        } else {
            OTHER_PAGE_LIMIT
        },
    )
}

pub fn validate_draft(id: &str, payload: &Value) -> Result<(), SyncPolicyError> {
    let object = payload.as_object().ok_or(SyncPolicyError::InvalidDraft)?;
    let device = object
        .get("device")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(SyncPolicyError::DraftIdentity)?;
    let session = object
        .get("session")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(SyncPolicyError::DraftIdentity)?;
    validate_draft_identity(id, object, device, session)
}

pub fn validate_stored_draft(id: &str, payload: &Value) -> Result<(), SyncPolicyError> {
    let object = payload.as_object().ok_or(SyncPolicyError::InvalidDraft)?;
    let device = object
        .get("device")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(SyncPolicyError::DraftIdentity)?;
    let session = object
        .get("session")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(SyncPolicyError::DraftIdentity)?;
    validate_draft_identity(id, object, device, session)
}

fn validate_draft_identity(
    id: &str,
    object: &Map<String, Value>,
    device: &str,
    session: &str,
) -> Result<(), SyncPolicyError> {
    let prefix = format!("{device}:");
    let suffix = format!(":{session}");
    let writer = id
        .strip_prefix(&prefix)
        .and_then(|tail| tail.strip_suffix(&suffix))
        .unwrap_or_default();
    let identity_matches =
        id == format!("{device}:{session}") || (!writer.is_empty() && !writer.contains(':'));
    if !identity_matches
        || object
            .get("id")
            .is_some_and(|value| !value.is_null() && value.as_str() != Some(id))
    {
        return Err(SyncPolicyError::DraftIdentity);
    }
    Ok(())
}

pub fn parse_preferences(
    value: &Value,
    scope: PreferenceScope,
) -> Result<Map<String, Value>, SyncPolicyError> {
    let object = value
        .as_object()
        .ok_or(SyncPolicyError::InvalidPreferences)?;
    let mut fields = Map::new();
    for (name, field) in object {
        validate_preference(name, field, scope)?;
        fields.insert(name.clone(), field.clone());
    }
    Ok(fields)
}

pub fn merge_preferences(
    current: &Map<String, Value>,
    incoming: &Map<String, Value>,
    now_ms: i64,
) -> Result<Map<String, Value>, SyncPolicyError> {
    let mut fields = current.clone();
    for (key, value) in incoming {
        let mut candidate = value.clone();
        let timestamp = candidate
            .get("timestamp")
            .and_then(Value::as_i64)
            .ok_or(SyncPolicyError::InvalidPreferences)?;
        if timestamp > now_ms.saturating_add(MAX_CLOCK_SKEW_MS) {
            candidate["timestamp"] = json!(now_ms);
        }
        let replace = fields
            .get(key)
            .is_none_or(|prior| preference_rank(&candidate) > preference_rank(prior));
        if replace {
            fields.insert(key.clone(), candidate);
        }
    }
    prune_preferences(&mut fields);
    Ok(fields)
}

fn validate_preference(
    name: &str,
    field: &Value,
    scope: PreferenceScope,
) -> Result<(), SyncPolicyError> {
    if name.chars().count() > 2048 {
        return Err(SyncPolicyError::InvalidPreferences);
    }
    let parts: Vec<String> =
        serde_json::from_str(name).map_err(|_| SyncPolicyError::InvalidPreferences)?;
    if !matches!(parts.len(), 1 | 2) || canonical_json(&json!(parts)) != name {
        return Err(SyncPolicyError::InvalidPreferences);
    }
    let key = &parts[0];
    let value = field.get("value").unwrap_or(&Value::Null);

    let appearance = key == "codex-studio-preferences-v1";
    let visual = key.starts_with("codex-project-tree:")
        || key.starts_with("codex-project-compact:")
        || key.starts_with("codex-progress-hidden:")
        || key.starts_with("studio-prompt-bookmarks:")
        || matches!(
            key.as_str(),
            "codex-worker-disclosures"
                | "studio-logical-project-collapsed"
                | "studio-logical-project-compact"
                | "studio-server-project-compact-v1"
        );
    let shell = key == "server-order"
        || key.starts_with("server-alias:")
        || key.starts_with("server-label:");
    if !appearance && !visual && !shell {
        return Err(SyncPolicyError::InvalidPreferences);
    }
    let expected_user_scope = appearance
        || key.starts_with("server-")
        || key.starts_with("studio-logical-")
        || key == "studio-server-project-compact-v1";
    if expected_user_scope != matches!(scope, PreferenceScope::User) {
        return Err(SyncPolicyError::InvalidPreferences);
    }

    if appearance {
        if parts.len() != 2 || !valid_appearance(&parts[1], value) {
            return Err(SyncPolicyError::InvalidPreferences);
        }
    } else if visual {
        if !value.is_null() && !value.is_boolean() {
            return Err(SyncPolicyError::InvalidPreferences);
        }
    } else if key == "server-order" {
        if !value.as_array().is_some_and(|items| {
            items.len() <= 256
                && items
                    .iter()
                    .all(|item| item.as_str().is_some_and(|s| s.chars().count() <= 256))
        }) {
            return Err(SyncPolicyError::InvalidPreferences);
        }
    } else if key.starts_with("server-alias:") {
        let Some(label) = value.as_str() else {
            return Err(SyncPolicyError::InvalidPreferences);
        };
        if label.is_empty()
            || label.len() > 3
            || !label.is_ascii()
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphabetic() && byte.is_ascii_uppercase())
        {
            return Err(SyncPolicyError::InvalidPreferences);
        }
    } else if !value.as_str().is_some_and(|label| {
        !crate::event_write::python_strip(label).is_empty() && label.chars().count() <= 128
    }) {
        return Err(SyncPolicyError::InvalidPreferences);
    }
    Ok(())
}

fn valid_appearance(name: &str, value: &Value) -> bool {
    match name {
        "theme" => matches!(value.as_str(), Some("auto" | "light" | "dark")),
        "typography" | "contentLayout" => matches!(value.as_str(), Some("original" | "custom")),
        "fontFamily" => matches!(
            value.as_str(),
            Some("system" | "arial" | "inter" | "georgia" | "verdana")
        ),
        "sidebarFontSize" | "mainFontSize" => {
            value.as_i64().is_some_and(|n| (12..=24).contains(&n))
        }
        "contentWidth" => value.as_i64().is_some_and(|n| (60..=100).contains(&n)),
        "sidebarShortcut" => value.as_str().is_some_and(|s| s.chars().count() <= 32),
        "showMessageAvatars" | "checkProviderUpdates" => value.is_boolean(),
        "hideOldChatsThreshold" => value.as_i64().is_some_and(|n| (1..=50).contains(&n)),
        _ => false,
    }
}

fn preference_rank(field: &Value) -> (i64, String, String) {
    (
        field
            .get("timestamp")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        field
            .get("writer")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        canonical_json(field.get("value").unwrap_or(&Value::Null)),
    )
}

fn prune_preferences(fields: &mut Map<String, Value>) {
    let mut removable = fields
        .iter()
        .filter(|(name, _)| {
            !preference_key(name).is_some_and(|key| {
                key == "codex-studio-preferences-v1" || key.starts_with("server-")
            })
        })
        .map(|(name, field)| {
            (
                field
                    .get("timestamp")
                    .and_then(Value::as_i64)
                    .unwrap_or_default(),
                name.clone(),
            )
        })
        .collect::<Vec<_>>();
    removable.sort_by(|left, right| right.cmp(left));
    let retained = removable
        .into_iter()
        .take(PREFERENCE_FIELD_CAP)
        .map(|(_, name)| name)
        .collect::<std::collections::BTreeSet<_>>();
    fields.retain(|name, _| {
        preference_key(name)
            .is_some_and(|key| key == "codex-studio-preferences-v1" || key.starts_with("server-"))
            || retained.contains(name)
    });
}

fn preference_key(name: &str) -> Option<String> {
    serde_json::from_str::<Vec<String>>(name)
        .ok()
        .and_then(|parts| parts.into_iter().next())
}

fn canonical_json(value: &Value) -> String {
    serde_json::to_string(value).expect("serde_json::Value is serializable")
}

fn stored_preference_fields(
    payload: Option<String>,
    scope: &str,
    validate_field: &(impl Fn(&Value) -> bool + Send + Sync),
) -> Result<Map<String, Value>, SyncPolicyError> {
    let Some(payload) = payload else {
        return Ok(Map::new());
    };
    let invalid_object = || SyncPolicyError::InvalidStoredObject {
        scope: "uiPreferences".to_owned(),
        id: scope.to_owned(),
    };
    let envelope: Value = serde_json::from_str(&payload).map_err(|_| invalid_object())?;
    if envelope.as_object().is_none_or(|object| {
        object.len() != 3
            || envelope.get("collection").and_then(Value::as_str) != Some("uiPreferences")
            || envelope.get("id").and_then(Value::as_str) != Some(scope)
    }) {
        return Err(invalid_object());
    }
    let value = envelope
        .get("value")
        .and_then(Value::as_object)
        .ok_or_else(invalid_object)?;
    if value.len() != 1 {
        return Err(invalid_object());
    }
    let mut fields = value
        .get("fields")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(invalid_object)?;
    for (name, field) in &fields {
        if !validate_field(field)
            || validate_preference(
                name,
                field,
                if scope == "user" {
                    PreferenceScope::User
                } else {
                    PreferenceScope::Server
                },
            )
            .is_err()
        {
            return Err(invalid_object());
        }
    }
    fields.retain(|name, _| {
        !preference_key(name).is_some_and(|key| key.starts_with("studio-turns:"))
    });
    Ok(fields)
}

pub fn push_preferences(
    store: &RuntimeStore,
    scope: PreferenceScope,
    incoming: &Map<String, Value>,
    validate_field: impl Fn(&Value) -> bool + Send + Sync,
) -> Result<Map<String, Value>, SyncPolicyError> {
    let scope_name = match scope {
        PreferenceScope::User => "user",
        PreferenceScope::Server => "server",
    };
    store
        .transaction(|transaction| {
            let current_payload =
                SyncDocumentsRepository::preference_payload(transaction, scope_name)?;
            let current = stored_preference_fields(current_payload, scope_name, &validate_field)
                .map_err(|error| match error {
                    SyncPolicyError::InvalidStoredObject { scope, id } => {
                        StoreError::InvalidSyncObject { scope, id }
                    }
                    other => StoreError::SchemaMismatch(other.to_string()),
                })?;
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| {
                    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
                });
            let merged = merge_preferences(&current, incoming, now_ms)
                .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
            let payload = canonical_json(&json!({
                "collection": "uiPreferences",
                "id": scope_name,
                "value": {"fields": merged},
            }));
            SyncDocumentsRepository::put_preferences(transaction, scope_name, &payload)?;
            Ok(merged)
        })
        .map_err(|error| match error {
            StoreError::InvalidSyncObject { scope, id } => {
                SyncPolicyError::InvalidStoredObject { scope, id }
            }
            other => SyncPolicyError::Store(other),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_identity_matches_row_key_and_allows_extension_members() {
        let payload = json!({"device":"phone","session":"chat","id":"phone:tab:chat","text":"draft","extra":true});
        assert!(validate_draft("phone:tab:chat", &payload).is_ok());
        assert!(validate_stored_draft("phone:tab:chat", &payload).is_ok());
        assert!(validate_stored_draft("phone:chat", &json!({})).is_err());
        assert!(validate_stored_draft("phone:chat", &Value::Null).is_err());
        assert!(
            validate_stored_draft("phone:chat", &json!({"device":"other","session":"chat"}))
                .is_err()
        );
    }

    #[test]
    fn preference_rank_clamps_future_clock_and_keeps_other_fields() {
        let scope = PreferenceScope::User;
        let current = Map::new();
        let incoming = parse_preferences(&json!({
            "[\"codex-studio-preferences-v1\",\"theme\"]": {"value":"dark","timestamp":900000,"writer":"a"},
            "[\"codex-studio-preferences-v1\",\"mainFontSize\"]": {"value":18,"timestamp":100,"writer":"a"}
        }), scope).expect("valid preference fields");
        let merged = merge_preferences(&current, &incoming, 1_000).expect("merge succeeds");
        assert_eq!(
            merged["[\"codex-studio-preferences-v1\",\"theme\"]"]["timestamp"],
            1_000
        );
        assert_eq!(
            merged["[\"codex-studio-preferences-v1\",\"mainFontSize\"]"]["value"],
            18
        );
    }

    #[test]
    fn preference_validation_rejects_unknown_fields_and_scope_leaks() {
        assert!(parse_preferences(&json!({
            "[\"codex-studio-preferences-v1\",\"theme\"]": {"value":"blue","timestamp":1,"writer":"a"}
        }), PreferenceScope::User).is_err());
        assert!(parse_preferences(&json!({
            "[\"codex-project-tree:/state\",\"/repo\"]": {"value":true,"timestamp":1,"writer":"a"}
        }), PreferenceScope::User).is_err());
    }

    #[test]
    fn cursor_limits_match_scope_contract() {
        assert_eq!(effective_limit("state:entities:v1", 900), 500);
        assert_eq!(effective_limit("drafts", 900), 100);
        assert_eq!(effective_limit("drafts", 0), 1);
    }

    #[test]
    fn overflowing_numbers_are_rejected_by_json_parser() {
        assert!(serde_json::from_str::<Value>(r#"{"nested":[1e309]}"#).is_err());
    }

    #[test]
    fn stored_preference_envelope_identity_must_match_key() {
        let payload =
            Some(r#"{"collection":"agent","id":"wrong","value":{"fields":{}}}"#.to_owned());
        assert!(matches!(
            stored_preference_fields(payload, "user", &|_| true),
            Err(SyncPolicyError::InvalidStoredObject { scope, id })
                if scope == "uiPreferences" && id == "user"
        ));
    }
}
