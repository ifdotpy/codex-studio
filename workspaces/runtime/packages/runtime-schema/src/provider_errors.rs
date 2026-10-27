//! Known Codex error classifications. The generated vendor leaf owns their
//! spellings and payloads; parity rejects a changed known set. This is a tag
//! projection, not a replacement for the lossless provider error JSON record.
use serde_json::{Value, json};

pub const CODEX_ERROR_SCHEMA: &str = include_str!("schemas/codex/CodexErrorInfo.json");

pub use crate::codex_generated::KnownCodexErrorKind;

/// External and historical records may carry a string or a single-key object.
/// Never choose an arbitrary key from an ambiguous provider extension object.
pub fn raw_error_kind(error: &Value) -> Option<&str> {
    let info = error.get("codexErrorInfo")?;
    info.as_str().or_else(|| {
        let object = info.as_object()?;
        if object.len() != 1 {
            return None;
        }
        object.keys().next().map(String::as_str)
    })
}

/// Classify known tags only. Unknown external records remain with their caller;
/// this function never rewrites the original payload or invents an enum variant.
pub fn known_error_kind(error: &Value) -> Option<KnownCodexErrorKind> {
    serde_json::from_value(Value::String(raw_error_kind(error)?.into())).ok()
}

/// The vendor's thread-scoped precaution stop, shared by admission and recovery.
pub fn is_misalignment_policy_violation(error: &Value) -> bool {
    known_error_kind(error) == Some(KnownCodexErrorKind::MisalignmentPolicyViolation)
}

fn without_annotations(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(key, _)| {
                    !matches!(
                        key.as_str(),
                        "$schema" | "$comment" | "title" | "description"
                    )
                })
                .map(|(key, value)| (key.clone(), without_annotations(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(without_annotations).collect()),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.clone(),
    }
}

/// Compatibility is scoped to the consumed error leaf, including its open
/// future-value branch and structured payloads. Documentation changes are inert.
pub fn codex_error_schema_compatible(notification: &Value) -> bool {
    let Ok(mut owner) = serde_json::from_str::<Value>(CODEX_ERROR_SCHEMA) else {
        return false;
    };
    let Some(candidate) = notification.pointer("/definitions/CodexErrorInfo") else {
        return false;
    };
    let Some(turn_kind) = notification.pointer("/definitions/NonSteerableTurnKind") else {
        return false;
    };
    let Some(definitions) = owner
        .as_object_mut()
        .and_then(|object| object.remove("definitions"))
    else {
        return false;
    };
    without_annotations(&owner) == without_annotations(candidate)
        && definitions
            .get("NonSteerableTurnKind")
            .is_some_and(|expected| without_annotations(expected) == without_annotations(turn_kind))
}

/// Vendor error payload types plus the closed classification projection, for
/// the existing TypeScript renderer. No complete vendor protocol is imported.
pub fn schemas() -> Result<Value, String> {
    let mut leaf: Value =
        serde_json::from_str(CODEX_ERROR_SCHEMA).map_err(|error| error.to_string())?;
    let definitions = leaf
        .as_object_mut()
        .ok_or("Codex error schema must be an object")?
        .remove("definitions")
        .ok_or("Codex error schema is missing definitions")?;
    // JSON Schema leaves object properties open by default. Spell this out for
    // openapi-typescript, whose default object rendering would erase extensions.
    let future = leaf["anyOf"]
        .as_array_mut()
        .and_then(|branches| branches.last_mut())
        .ok_or("Codex error schema is missing its future-value branch")?;
    if future["type"] != json!(["string", "object"]) {
        return Err("Codex error future-value branch changed".into());
    }
    future["additionalProperties"] = json!(true);
    let document = json!({
        "CodexErrorInfo": leaf,
        "NonSteerableTurnKind": definitions["NonSteerableTurnKind"],
        "KnownCodexErrorKind": schemars::schema_for!(KnownCodexErrorKind),
    });
    serde_json::from_str(
        &document
            .to_string()
            .replace("#/definitions/", "#/components/schemas/"),
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_error_classification_matches_vendor_leaf() {
        let leaf: Value = serde_json::from_str(CODEX_ERROR_SCHEMA).expect("vendor leaf");
        let mut known = Vec::new();
        for branch in leaf["anyOf"].as_array().expect("vendor variants") {
            if let Some(values) = branch["enum"].as_array() {
                known.extend(values.iter().cloned());
            } else if let Some(properties) = branch["properties"].as_object() {
                known.extend(properties.keys().map(|name| json!(name)));
            }
        }
        let mirror = schemars::schema_for!(KnownCodexErrorKind).to_value();
        let mut mirror = mirror["enum"]
            .as_array()
            .expect("closed classification")
            .clone();
        known.sort_by_key(Value::to_string);
        mirror.sort_by_key(Value::to_string);
        assert_eq!(
            mirror, known,
            "CodexErrorInfo.json owns every known classification"
        );
        for kind in known {
            assert!(known_error_kind(&json!({"codexErrorInfo":kind})).is_some());
        }
    }

    #[test]
    fn unknown_and_ambiguous_error_records_remain_lossless() {
        for info in [
            json!("futureCode"),
            json!({"futureCode":{"opaque":[1,null]}}),
            json!({"serverOverloaded":{},"futureCode":{}}),
            json!(["serverOverloaded"]),
        ] {
            let error = json!({"codexErrorInfo":info,"message":"failure","extension":{"id":42}});
            let original = error.clone();
            assert_eq!(known_error_kind(&error), None);
            assert_eq!(error, original);
        }
        assert_eq!(
            known_error_kind(&json!({"codexErrorInfo":{"tooManyDenials":{}}})),
            Some(KnownCodexErrorKind::TooManyDenials)
        );
    }

    #[test]
    fn error_leaf_compatibility_rejects_variant_and_payload_drift() {
        let leaf: Value = serde_json::from_str(CODEX_ERROR_SCHEMA).expect("vendor leaf");
        let mut error = leaf.clone();
        error
            .as_object_mut()
            .expect("leaf object")
            .remove("definitions");
        let source = json!({"definitions":{"CodexErrorInfo":error,"NonSteerableTurnKind":leaf["definitions"]["NonSteerableTurnKind"]}});
        assert!(codex_error_schema_compatible(&source));
        let mut documented = source.clone();
        documented["definitions"]["CodexErrorInfo"]["description"] = json!("new documentation");
        assert!(codex_error_schema_compatible(&documented));
        for pointer in [
            "/definitions/CodexErrorInfo/anyOf/0/enum/0",
            "/definitions/CodexErrorInfo/anyOf/1/properties/httpConnectionFailed/properties/httpStatusCode/type",
            "/definitions/NonSteerableTurnKind/enum/0",
        ] {
            let mut changed = source.clone();
            *changed.pointer_mut(pointer).expect("consumed contract") = json!("drift");
            assert!(!codex_error_schema_compatible(&changed), "{pointer}");
        }
    }
}
