//! Canonical client preference policy. Server authorization remains server-owned.
use serde_json::{Map, Value, json};
use std::{cmp::Ordering, sync::OnceLock};

pub fn contract() -> &'static Value {
    static CONTRACT: OnceLock<Value> = OnceLock::new();
    CONTRACT.get_or_init(|| {
        serde_json::from_str(include_str!("../../../contracts/preference-storage.json"))
            .expect("valid preference contract")
    })
}
pub fn reserved_metadata_key(key: &str) -> bool {
    key == contract()["writerStorageKey"].as_str().expect("writer key")
        || key.starts_with(&format!(
            "{}:",
            contract()["fieldCacheKey"].as_str().expect("field key")
        ))
}
pub fn appearance_defaults(meta_shortcut: bool) -> Value {
    let mut value = contract()["appearance"]["defaults"].clone();
    value["sidebarShortcut"] = contract()["appearance"]["sidebarShortcut"]
        [if meta_shortcut { "mac" } else { "other" }]
    .clone();
    value
}
/// Validate the wire status using the same schema consumed by the UI adapter.
pub fn valid_remote_status(value: &Value) -> bool {
    let Some(value) = value.as_object() else {
        return false;
    };
    contract()["remoteStatusSchema"]["oneOf"]
        .as_array()
        .expect("status variants")
        .iter()
        .any(|variant| {
            let properties = variant["properties"]
                .as_object()
                .expect("status properties");
            variant["required"]
                .as_array()
                .expect("required status fields")
                .iter()
                .all(|key| value.contains_key(key.as_str().expect("field name")))
                && (variant["additionalProperties"] != false
                    || value.keys().all(|key| properties.contains_key(key)))
                && properties.iter().all(|(name, rule)| {
                    let Some(item) = value.get(name) else {
                        return false;
                    };
                    rule.get("const").is_none_or(|expected| item == expected)
                        && match rule["type"].as_str() {
                            Some("string") => item.as_str().is_some_and(|s| {
                                rule["minLength"]
                                    .as_u64()
                                    .is_none_or(|min| s.chars().count() as u64 >= min)
                            }),
                            Some("boolean") => item.is_boolean(),
                            None => true,
                            _ => false,
                        }
                })
        })
}
pub fn scope(key: &str, model_key: &str) -> Option<&'static str> {
    ["user", "server"].into_iter().find(|scope| {
        let policy = &contract()["scopes"][*scope];
        policy["keys"]
            .as_array()
            .expect("keys")
            .iter()
            .any(|v| v.as_str() == Some(key))
            || policy["prefixes"]
                .as_array()
                .expect("prefixes")
                .iter()
                .any(|v| key.starts_with(v.as_str().expect("prefix")))
            || (policy["includeModelWhitelistKey"] == true && key == model_key)
    })
}
pub fn string_order(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// Canonical objects use JavaScript UTF-16 key order, independent of map storage.
pub fn canonical(value: &Value) -> String {
    match value {
        Value::Object(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_by(|a, b| string_order(a, b));
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|k| format!("{}:{}", json!(k), canonical(&values[k])))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        Value::Array(values) => format!(
            "[{}]",
            values.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
        Value::Number(number) => {
            // JSON.parse uses binary64. Its shortest decimal uses fixed notation
            // from 1e-6 through values below 1e21, including integral floats.
            let n = number.as_f64().expect("finite JSON number");
            if n == 0.0 {
                return "0".into();
            }
            let raw = n.to_string();
            if n.abs() >= 1e21 || n.abs() < 1e-6 {
                let scientific = format!("{n:e}");
                let (mantissa, exponent) = scientific.split_once('e').expect("scientific number");
                let exponent: i32 = exponent.parse().expect("decimal exponent");
                format!(
                    "{mantissa}e{}{exponent}",
                    if exponent >= 0 { "+" } else { "" }
                )
            } else {
                raw
            }
        }
        _ => value.to_string(),
    }
}
pub fn valid_field(field: &Value) -> bool {
    field.is_object()
        && field.get("value").is_some()
        && field["timestamp"]
            .as_u64()
            .is_some_and(|v| v <= crate::MAX_SAFE_INTEGER as u64)
        && field["writer"].as_str().is_some_and(|s| !s.is_empty())
}
pub fn merge(prior: &Value, incoming: &Value) -> Value {
    let mut result = prior.as_object().cloned().unwrap_or_default();
    if let Some(fields) = incoming.as_object() {
        for (name, field) in fields {
            if !valid_field(field) {
                continue;
            }
            let replace = result.get(name).is_none_or(|old| {
                !valid_field(old)
                    || field["timestamp"].as_u64() > old["timestamp"].as_u64()
                    || (field["timestamp"] == old["timestamp"] && {
                        let order = string_order(
                            field["writer"].as_str().expect("valid writer"),
                            old["writer"].as_str().expect("valid writer"),
                        );
                        order.is_gt()
                            || (order.is_eq()
                                && string_order(
                                    &canonical(&field["value"]),
                                    &canonical(&old["value"]),
                                )
                                .is_gt())
                    })
            });
            if replace {
                result.insert(name.clone(), field.clone());
            }
        }
    }
    Value::Object(result)
}
/// Correct only fields that still equal the submitted version. The authenticated
/// ACK contains the complete server winner map. Later changes survive this ACK.
pub fn acknowledge(current: &Value, submitted: &Value, accepted: &Value) -> Value {
    let mut result = current.as_object().cloned().unwrap_or_default();
    let empty = Map::new();
    let submitted = submitted.as_object().unwrap_or(&empty);
    let protected = result
        .keys()
        .chain(submitted.keys())
        .filter(|name| result.get(*name) != submitted.get(*name))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    for (name, sent) in submitted {
        if result.get(name) != Some(sent) {
            continue;
        }
        if let Some(received) = accepted.get(name) {
            result.insert(name.clone(), received.clone());
        } else {
            result.remove(name);
        }
    }
    let accepted = Value::Object(
        accepted
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(name, _)| !protected.contains(*name))
            .map(|(name, field)| (name.clone(), field.clone()))
            .collect(),
    );
    merge(&Value::Object(result), &accepted)
}
pub fn storage_fields(key: &str, raw: Option<&str>) -> Value {
    let value = raw
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .unwrap_or(Value::Null);
    match value {
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(name, value)| (json!([key, name]).to_string(), value))
                .collect(),
        ),
        Value::Null => json!({}),
        value => json!({(json!([key]).to_string()):value}),
    }
}
pub fn edit(
    previous: &Map<String, Value>,
    next: &Map<String, Value>,
    latest: &Map<String, Value>,
) -> Map<String, Value> {
    let mut result = latest.clone();
    for key in previous.keys().chain(next.keys()) {
        if previous.get(key) == next.get(key) {
            continue;
        }
        if let Some(value) = next.get(key) {
            result.insert(key.clone(), value.clone());
        } else {
            result.remove(key);
        }
    }
    result
}
pub fn prune(fields: &Value, model_key: &str) -> Value {
    let mut protected = Map::new();
    let mut items = Vec::new();
    for (name, field) in fields.as_object().into_iter().flatten() {
        let path = serde_json::from_str::<Vec<Value>>(name).ok();
        let key = path
            .as_ref()
            .and_then(|v| v.first())
            .and_then(Value::as_str)
            .unwrap_or(name);
        let list = |property: &str, prefix: bool| {
            contract()[property]
                .as_array()
                .expect("field policy")
                .iter()
                .any(|v| {
                    if prefix {
                        key.starts_with(v.as_str().expect("prefix"))
                    } else {
                        v.as_str() == Some(key)
                    }
                })
        };
        if list("ignoredFieldKeyPrefixes", true) {
            continue;
        }
        if key == model_key
            || list("protectedFieldKeys", false)
            || list("protectedFieldKeyPrefixes", true)
        {
            protected.insert(name.clone(), field.clone());
        } else {
            items.push((name, field));
        }
    }
    items.sort_by(|(a, x), (b, y)| {
        y["timestamp"]
            .as_u64()
            .cmp(&x["timestamp"].as_u64())
            .then_with(|| string_order(b, a))
    });
    for (key, value) in items
        .into_iter()
        .take(contract()["itemFieldCap"].as_u64().expect("field cap") as usize)
    {
        protected.insert(key.clone(), value.clone());
    }
    Value::Object(protected)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preference_policy_shared_legacy_vectors() {
        let cases: Value = serde_json::from_str(include_str!(
            "../../../contracts/preference-policy-cases.json"
        ))
        .expect("policy fixtures");
        let model = "fixture-model-key";
        for case in cases["scopes"].as_array().expect("scopes") {
            let key = case["key"].as_str().expect("key");
            assert_eq!(
                json!(scope(
                    if key == "$modelWhitelistKey" {
                        model
                    } else {
                        key
                    },
                    model
                )),
                case["expected"]
            );
        }
        for case in cases["storage"].as_array().expect("storage") {
            assert_eq!(
                storage_fields(case["key"].as_str().expect("key"), case["raw"].as_str()),
                case["expected"]
            );
        }
        for case in cases["merges"].as_array().expect("merges") {
            assert_eq!(
                merge(&case["prior"], &case["incoming"]),
                case["expected"],
                "{}",
                case["name"]
            );
        }
        for case in cases["edits"].as_array().expect("edits") {
            assert_eq!(
                json!(edit(
                    case["previous"].as_object().expect("previous"),
                    case["next"].as_object().expect("next"),
                    case["latest"].as_object().expect("latest")
                )),
                case["expected"]
            );
        }
        for case in cases["acknowledgements"]
            .as_array()
            .expect("acknowledgements")
        {
            assert_eq!(
                acknowledge(&case["current"], &case["submitted"], &case["accepted"]),
                case["expected"],
                "{}",
                case["name"]
            );
        }
        let mut fields = cases["prune"]["protected"]
            .as_object()
            .expect("protected")
            .clone();
        fields.extend(
            cases["prune"]["ignored"]
                .as_object()
                .expect("ignored")
                .clone(),
        );
        for index in 0..cases["prune"]["itemCount"].as_u64().expect("count") {
            fields.insert(
                json!(["codex-project-tree:x", index.to_string()]).to_string(),
                json!({"value":true,"timestamp":index,"writer":"a"}),
            );
        }
        let fields = prune(&json!(fields), model);
        assert_eq!(
            fields.as_object().expect("pruned").len() as u64,
            cases["prune"]["expectedCount"].as_u64().expect("expected")
        );
        assert!(
            fields
                .get(json!(["codex-project-tree:x", "1"]).to_string())
                .is_none()
        );
        assert!(
            fields
                .get(json!(["codex-project-tree:x", "2"]).to_string())
                .is_some()
        );
    }
    #[test]
    fn preference_remote_status_uses_shared_schema() {
        for value in [
            json!({"state":"saved"}),
            json!({"state":"saving"}),
            json!({"state":"error","error":"Unknown result.","uncertain":true,"requestId":"exact-id"}),
        ] {
            assert!(valid_remote_status(&value));
        }
        for value in [
            Value::Null,
            json!({"state":"unknown"}),
            json!({"state":"saved","requestId":"old"}),
            json!({"state":"error","error":"Unknown result.","uncertain":"true","requestId":"id"}),
            json!({"state":"error","error":"Unknown result.","uncertain":true,"requestId":""}),
        ] {
            assert!(!valid_remote_status(&value));
        }
    }
}
