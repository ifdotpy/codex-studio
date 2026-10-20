//! OpenAPI authority loading and canonical schema identity.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use serde_json::Value;

const METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

static SCHEMA_HASH: OnceLock<String> = OnceLock::new();
static OPERATIONS: OnceLock<Result<BTreeSet<(String, String)>, serde_json::Error>> =
    OnceLock::new();

pub use studio_runtime_schema::api_schema::{
    API_SCHEMA_HASH_HEADER, API_SCHEMA_HASH_PARAM, API_SCHEMA_MISMATCH_FIELD,
    API_SCHEMA_MISMATCH_HEADER,
};

pub(crate) fn path_matches(template: &str, path: &str) -> bool {
    let mut expected = template.split('/');
    let mut actual = path.split('/');
    loop {
        match (expected.next(), actual.next()) {
            (Some(expected), Some(actual)) if expected.starts_with('{') || expected == actual => {}
            (Some(_), Some(_)) => return false,
            (None, None) => return true,
            _ => return false,
        }
    }
}

pub(crate) fn matching_template<'a>(
    operations: impl Iterator<Item = (&'a str, &'a str)>,
    method: Option<&str>,
    path: &str,
) -> Option<&'a str> {
    operations
        .filter(|(candidate, _)| method.is_none_or(|method| *candidate == method))
        .find_map(|(_, template)| path_matches(template, path).then_some(template))
}

pub(crate) fn operation_has_etag(method: &str, template: &str) -> bool {
    document()
        .pointer(&format!(
            "/paths/{}/{}",
            template.replace('~', "~0").replace('/', "~1"),
            method.to_ascii_lowercase()
        ))
        .and_then(|operation| operation.pointer("/responses/200/headers/ETag"))
        .is_some()
}

fn document() -> &'static Value {
    studio_runtime_services::sync_policy::openapi_document()
}

pub(crate) fn openapi_document() -> &'static Value {
    document()
}

/// Hash the human-readable authority after applying canonicalizer-v3 rules.
pub fn api_schema_hash() -> &'static str {
    SCHEMA_HASH.get_or_init(|| studio_runtime_schema::api_schema::schema_hash(document()))
}

/// Return all method/path pairs from the committed OpenAPI authority.
pub(crate) fn methods_for_path(path: &str) -> Option<Vec<String>> {
    let operations = operations().ok()?;
    let methods = operations
        .iter()
        .filter(|(_, template)| path_matches(template, path))
        .map(|(method, _)| method.clone())
        .collect::<BTreeSet<_>>();
    (!methods.is_empty()).then(|| methods.into_iter().collect())
}

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
