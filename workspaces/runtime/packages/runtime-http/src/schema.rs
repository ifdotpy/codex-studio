//! OpenAPI authority loading and canonical schema identity.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

const OPENAPI_DOCUMENT: &[u8] = include_bytes!("../openapi.json");
const DOCUMENTATION_KEYS: [&str; 4] = ["description", "summary", "examples", "externalDocs"];
const NAME_KEYED_MAPS: [&str; 18] = [
    "properties",
    "$defs",
    "definitions",
    "patternProperties",
    "dependentSchemas",
    "paths",
    "schemas",
    "parameters",
    "responses",
    "requestBodies",
    "headers",
    "links",
    "callbacks",
    "securitySchemes",
    "content",
    "encoding",
    "mapping",
    "webhooks",
];
const METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

static DOCUMENT: OnceLock<Value> = OnceLock::new();
static SCHEMA_HASH: OnceLock<String> = OnceLock::new();
static OPERATIONS: OnceLock<Result<BTreeSet<(String, String)>, serde_json::Error>> =
    OnceLock::new();

pub const API_SCHEMA_HASH_HEADER: &str = "X-Studio-API-Schema";
pub const API_SCHEMA_MISMATCH_HEADER: &str = "X-Studio-API-Schema-Mismatch";

fn document() -> &'static Value {
    DOCUMENT.get_or_init(|| {
        serde_json::from_slice(OPENAPI_DOCUMENT).expect("committed OpenAPI document is valid JSON")
    })
}

fn strip_documentation(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(strip_documentation).collect()),
        Value::Object(object) => {
            let mut canonical = Map::new();
            for (key, item) in object {
                if DOCUMENTATION_KEYS.contains(&key.as_str()) {
                    continue;
                }
                let mut child = match key.as_str() {
                    "default" => item.clone(),
                    "components" => strip_components(item),
                    name if NAME_KEYED_MAPS.contains(&name) => strip_named_map(item),
                    _ => strip_documentation(item),
                };
                if matches!(key.as_str(), "anyOf" | "oneOf")
                    && let Value::Array(members) = &mut child
                {
                    members.sort_by_cached_key(|member| {
                        serde_json::to_string(member).expect("JSON value serializes")
                    });
                }
                canonical.insert(key.clone(), child);
            }
            Value::Object(canonical)
        }
        _ => value.clone(),
    }
}

fn strip_named_map(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(name, child)| (name.clone(), strip_documentation(child)))
                .collect(),
        ),
        _ => strip_documentation(value),
    }
}

fn strip_components(value: &Value) -> Value {
    let Value::Object(categories) = value else {
        return strip_documentation(value);
    };
    Value::Object(
        categories
            .iter()
            .map(|(category, collection)| {
                let cleaned = if NAME_KEYED_MAPS.contains(&category.as_str()) {
                    strip_named_map(collection)
                } else {
                    strip_documentation(collection)
                };
                (category.clone(), cleaned)
            })
            .collect(),
    )
}

/// Canonicalizer-v3 byte representation used by the Python exporter.
pub fn canonical_openapi_bytes() -> Vec<u8> {
    serde_json::to_vec(&strip_documentation(document())).expect("canonical JSON serializes")
}

/// Hash the human-readable authority after applying canonicalizer-v3 rules.
pub fn api_schema_hash() -> &'static str {
    SCHEMA_HASH.get_or_init(|| {
        Sha256::digest(canonical_openapi_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    })
}

/// Return all method/path pairs from the committed OpenAPI authority.
pub fn operations() -> Result<&'static BTreeSet<(String, String)>, &'static serde_json::Error> {
    match OPERATIONS.get_or_init(|| {
        let mut operations = BTreeSet::new();
        if let Some(paths) = document().get("paths").and_then(Value::as_object) {
            for (path, item) in paths {
                if let Some(methods) = item.as_object() {
                    for method in METHODS {
                        if methods.contains_key(method) {
                            operations.insert((method.to_uppercase(), path.clone()));
                        }
                    }
                }
            }
        }
        Ok(operations)
    }) {
        Ok(operations) => Ok(operations),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_hash_matches_generated_client_identity() {
        let client_schema =
            include_str!("../../../../../workspaces/client/apps/web/src/generated/apiSchema.ts");
        let client_hash = client_schema
            .split('"')
            .nth(1)
            .expect("generated TypeScript schema hash is present");
        assert_eq!(api_schema_hash(), client_hash);
    }

    #[test]
    fn committed_schema_lists_api_operations() {
        assert!(
            operations()
                .expect("committed schema parses")
                .iter()
                .any(|(_, path)| path.starts_with("/api/"))
        );
    }
}
