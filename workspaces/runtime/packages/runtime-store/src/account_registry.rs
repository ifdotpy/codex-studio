//! Current account registry codec and atomic replacement; account order is durable UI state.
use serde::Deserialize;
use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

fn registry_violation(value: &Value) -> Result<Option<(String, String)>, ()> {
    studio_runtime_schema::first_error("account-registry/AccountRegistryRecord", value)
        .map_err(|_| ())
}

#[derive(Clone, Default)]
pub struct RegistryOrder {
    pub accounts: Vec<String>,
    pub logins: Vec<String>,
}

pub fn decode_registry(bytes: &[u8]) -> Result<(Value, RegistryOrder), (String, String)> {
    if bytes.len() > crate::account_state::MAX_PERSISTED_DOCUMENT_BYTES
        || !crate::account_state::persisted_document_within_bounds(bytes)
    {
        return Err(("/".to_owned(), "resourceLimit".to_owned()));
    }
    let parsed = serde_json::from_slice::<
        studio_runtime_schema::json_object::JsonObject<
            studio_runtime_schema::account_registry::AccountRegistry,
        >,
    >(bytes);
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| ("/".to_owned(), "invalid JSON".to_owned()))?;
    if parsed.is_err() {
        return Err(match registry_violation(&value) {
            Ok(Some((pointer, _keyword))) => (pointer, "schema validation failed".to_owned()),
            _ => ("/".to_owned(), "invalid registry shape".to_owned()),
        });
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let order = RegistryOrder::deserialize(&mut deserializer)
        .map_err(|_| ("/".to_owned(), "invalid registry order".to_owned()))?;
    Ok((value, order))
}

#[derive(Debug, thiserror::Error)]
pub enum EncodeRegistryError {
    #[error("invalid account registry object shape")]
    InvalidShape,
    #[error("account registry serialization failed")]
    Serialization(#[from] serde_json::Error),
}

pub fn encode_registry(
    value: &Value,
    order: &RegistryOrder,
) -> Result<Vec<u8>, EncodeRegistryError> {
    let Some(root) = value.as_object() else {
        return Err(EncodeRegistryError::InvalidShape);
    };
    let Some(accounts) = root.get("accounts").and_then(Value::as_object) else {
        return Err(EncodeRegistryError::InvalidShape);
    };
    let logins = root.get("logins").and_then(Value::as_object);
    let mut output = Vec::new();
    let mut serializer = serde_json::Serializer::new(&mut output);
    let mut map = serializer.serialize_map(Some(root.len()))?;
    for (key, field) in root {
        match key.as_str() {
            "accounts" => map.serialize_entry(
                key,
                &OrderedRegistryMap {
                    values: accounts,
                    keys: &order.accounts,
                },
            ),
            "logins" => map.serialize_entry(
                key,
                &OrderedRegistryMap {
                    values: logins.ok_or(EncodeRegistryError::InvalidShape)?,
                    keys: &order.logins,
                },
            ),
            _ => map.serialize_entry(key, field),
        }?;
    }
    map.end()?;
    Ok(output)
}

struct OrderedRegistryMap<'a> {
    values: &'a Map<String, Value>,
    keys: &'a [String],
}

impl Serialize for OrderedRegistryMap<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.values.len()))?;
        let mut emitted = HashSet::new();
        for key in self.keys {
            if let Some(value) = self.values.get(key) {
                map.serialize_entry(key, value)?;
                emitted.insert(key);
            }
        }
        for (key, value) in self.values {
            if !emitted.contains(key) {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for RegistryOrder {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct RootVisitor;
        impl<'de> serde::de::Visitor<'de> for RootVisitor {
            type Value = RegistryOrder;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("account registry object")
            }
            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: serde::de::MapAccess<'de>,
            {
                let mut result = RegistryOrder::default();
                let mut seen = HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !seen.insert(key.clone()) {
                        return Err(serde::de::Error::custom("duplicate registry field"));
                    }
                    match key.as_str() {
                        "accounts" => result.accounts = map.next_value_seed(KeysSeed)?,
                        "logins" => result.logins = map.next_value_seed(KeysSeed)?,
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(result)
            }
        }
        deserializer.deserialize_map(RootVisitor)
    }
}

struct KeysSeed;
impl<'de> serde::de::DeserializeSeed<'de> for KeysSeed {
    type Value = Vec<String>;
    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct KeysVisitor;
        impl<'de> serde::de::Visitor<'de> for KeysVisitor {
            type Value = Vec<String>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("ordered object")
            }
            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: serde::de::MapAccess<'de>,
            {
                let mut keys = Vec::new();
                let mut seen = HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !seen.insert(key.clone()) {
                        return Err(serde::de::Error::custom("duplicate registry key"));
                    }
                    map.next_value::<serde::de::IgnoredAny>()?;
                    keys.push(key);
                }
                Ok(keys)
            }
        }
        deserializer.deserialize_map(KeysVisitor)
    }
}

#[derive(Debug)]
pub enum ReplaceError {
    NotReplaced,
    ReplacedNotDurable,
}

pub fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<(), ReplaceError> {
    atomic_replace_with(
        path,
        bytes,
        |temp, target| fs::rename(temp, target),
        |parent| File::open(parent).and_then(|directory| directory.sync_all()),
    )
}

#[doc(hidden)]
pub fn atomic_replace_with(
    path: &Path,
    bytes: &[u8],
    replace: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
    sync_parent: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<(), ReplaceError> {
    let parent = path.parent().ok_or(ReplaceError::NotReplaced)?;
    // Registry rows may contain login codes and verification URLs. Unix
    // permissions must be private even when the previous file or directory was
    // created with broader permissions. Other platforms rely on their default
    // temporary-file ACLs; this code does not explicitly rewrite those ACLs.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|_| ReplaceError::NotReplaced)?;
    }
    let mut temp =
        tempfile::NamedTempFile::new_in(parent).map_err(|_| ReplaceError::NotReplaced)?;
    (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|_| ReplaceError::NotReplaced)?;
        }
        temp.write_all(bytes)
            .map_err(|_| ReplaceError::NotReplaced)?;
        temp.as_file()
            .sync_all()
            .map_err(|_| ReplaceError::NotReplaced)?;
        replace(temp.path(), path).map_err(|_| ReplaceError::NotReplaced)?;
        sync_parent(parent).map_err(|_| ReplaceError::ReplacedNotDurable)?;
        Ok(())
    })()
}

#[cfg(test)]
mod tests {
    use super::decode_registry;
    use serde_json::json;

    #[test]
    fn typed_registry_decode_preserves_order_and_error_location() {
        let row = |id: &str| json!({"id":id,"home":"/fixture","label":id,"source":"fixture","status":"ready","provider":"codex"});
        let source = format!(
            r#"{{"version":2.0,"defaultAccountKey":"b","accounts":{{"b":{},"a":{}}},"logins":{{}}}}"#,
            row("b"),
            row("a")
        );
        let (value, order) = decode_registry(source.as_bytes()).expect("registry decodes");
        assert_eq!(order.accounts, ["b", "a"]);
        assert_eq!(value["version"], json!(2.0));
        let mut invalid: serde_json::Value = serde_json::from_str(&source).unwrap();
        invalid["accounts"]["b"]["provider"] = json!("unknown");
        let error = match decode_registry(invalid.to_string().as_bytes()) {
            Ok(_) => panic!("unknown provider must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.0, "/accounts/b/provider");
        for (section, key, field, variant) in [
            ("accounts", "b", "provider", "claude"),
            ("accounts", "b", "status", "ready"),
            ("logins", "r", "status", "ready"),
        ] {
            let mut invalid: serde_json::Value = serde_json::from_str(&source).unwrap();
            if section == "logins" {
                invalid["logins"]["r"] = json!({"requestId":"r","accountKey":"b","status":"ready"});
            }
            invalid[section][key][field] = json!({(variant):null});
            assert!(
                decode_registry(invalid.to_string().as_bytes()).is_err(),
                "{invalid}"
            );
        }
        let duplicate = source.replacen("\"version\":2.0", "\"version\":2,\"version\":2", 1);
        assert!(decode_registry(duplicate.as_bytes()).is_err());
    }
}
