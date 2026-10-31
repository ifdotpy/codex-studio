//! API schema identity: the names a renderer uses to send its schema hash and
//! the canonical hash of the committed `openapi.json`. This file is the owner;
//! the web copies in `apiSchema.ts` are rendered by `studio-contracts`.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Request and response header that carries an API schema hash.
pub const API_SCHEMA_HASH_HEADER: &str = "X-Studio-API-Schema";
/// Query parameter that carries the renderer's API schema hash where headers are unavailable.
pub const API_SCHEMA_HASH_PARAM: &str = "apiSchema";
/// Response header set when the renderer's API schema hash differs from the server's.
pub const API_SCHEMA_MISMATCH_HEADER: &str = "X-Studio-API-Schema-Mismatch";
/// Field of the `api-schema` stream event that reports a hash mismatch.
pub const API_SCHEMA_MISMATCH_FIELD: &str = "mismatch";

/// How many earlier contract hashes a server keeps and accepts from its peers. Installs
/// are less frequent than contract changes, so a peer one install behind can be several
/// contract changes behind. Unit: hashes.
pub const PEER_HASH_HISTORY_LEN: usize = 20;

/// Earlier contract hashes, newest first. The committed file `previous-api-schema-hashes.txt`
/// owns the list: `studio-contracts --write` adds the old current hash to it when the
/// current hash changes, and keeps `PEER_HASH_HISTORY_LEN` entries.
pub fn previous_api_schema_hashes() -> impl Iterator<Item = &'static str> {
    include_str!("../previous-api-schema-hashes.txt")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
}

/// A peer server is accepted when its hash is the current hash or one of the earlier hashes.
pub fn peer_hash_accepted(current: &str, offered: &str) -> bool {
    offered == current || previous_api_schema_hashes().any(|hash| hash == offered)
}

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

fn strip_named_map(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(name, child)| (name.clone(), strip_documentation(child)))
                .collect(),
        ),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Array(_) => {
            strip_documentation(value)
        }
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
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.clone(),
    }
}

/// Canonicalizer-v3 bytes of an OpenAPI document: documentation removed,
/// `anyOf`/`oneOf` members sorted, compact JSON with sorted keys.
pub fn canonical_bytes(document: &Value) -> Vec<u8> {
    serde_json::to_vec(&strip_documentation(document)).expect("canonical JSON serializes")
}

/// Lowercase hex SHA-256 of `canonical_bytes`: the API schema hash that
/// renderers and the server compare.
pub fn schema_hash(document: &Value) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(canonical_bytes(document)))
}

#[cfg(test)]
mod peer_version_tests {
    use super::*;

    const CURRENT: &str = "c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0";

    #[test]
    fn the_history_file_holds_sha256_hex_hashes_up_to_the_limit() {
        let hashes = previous_api_schema_hashes().collect::<Vec<_>>();
        assert!(!hashes.is_empty() && hashes.len() <= PEER_HASH_HISTORY_LEN);
        for hash in hashes {
            assert_eq!(hash.len(), 64, "{hash}");
            assert!(
                hash.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            );
        }
    }

    #[test]
    fn a_peer_with_the_current_or_an_earlier_hash_is_accepted() {
        assert!(peer_hash_accepted(CURRENT, CURRENT));
        for hash in previous_api_schema_hashes() {
            assert!(peer_hash_accepted(CURRENT, hash));
        }
    }

    #[test]
    fn a_peer_with_an_unknown_hash_is_refused() {
        let unknown = "9aa282663c082a75fc10906d145769eec177755035b36b8611d96c949b56fa11";
        assert!(!previous_api_schema_hashes().any(|hash| hash == unknown));
        assert!(!peer_hash_accepted(CURRENT, unknown));
        assert!(!peer_hash_accepted(CURRENT, ""));
    }
}
