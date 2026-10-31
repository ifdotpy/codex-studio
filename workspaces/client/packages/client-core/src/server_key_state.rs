//! Permissive persisted vault: malformed legacy values are checked when accessed.
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Debug, Default, PartialEq)]
pub enum TextField {
    #[default]
    Missing,
    Text(String),
    Other(Value),
}
impl<'de> Deserialize<'de> for TextField {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Value::deserialize(deserializer)? {
            Value::String(text) => Self::Text(text),
            value @ (Value::Null
            | Value::Bool(_)
            | Value::Number(_)
            | Value::Array(_)
            | Value::Object(_)) => Self::Other(value),
        })
    }
}
impl Serialize for TextField {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Missing => serializer.serialize_none(),
            Self::Text(text) => text.serialize(serializer),
            Self::Other(value) => value.serialize(serializer),
        }
    }
}
impl TextField {
    fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }
    fn put(self, map: &mut Map<String, Value>, key: &str) {
        match self {
            Self::Missing => {}
            Self::Text(text) => {
                map.insert(key.into(), Value::String(text));
            }
            Self::Other(value) => {
                map.insert(key.into(), value);
            }
        }
    }
    pub fn text(&self, name: &str) -> Result<&str, String> {
        match self {
            Self::Text(text) if !text.is_empty() && text.len() <= 16384 => Ok(text),
            Self::Missing | Self::Text(_) | Self::Other(_) => {
                Err(format!("Missing or invalid {name}."))
            }
        }
    }
    pub fn equals(&self, value: &Value) -> bool {
        match self {
            Self::Missing => value.is_null(),
            Self::Text(text) => value.as_str() == Some(text),
            Self::Other(other) => other == value,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialObject {
    #[serde(default, skip_serializing_if = "TextField::is_missing")]
    pub origin: TextField,
    #[serde(default, skip_serializing_if = "TextField::is_missing")]
    pub server_id: TextField,
    #[serde(default, skip_serializing_if = "TextField::is_missing")]
    pub client_id: TextField,
    #[serde(default, skip_serializing_if = "TextField::is_missing")]
    pub invite_id: TextField,
    #[serde(default, skip_serializing_if = "TextField::is_missing")]
    pub private_key: TextField,
    #[serde(default, skip_serializing_if = "TextField::is_missing")]
    pub body: TextField,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub receipt: Option<Value>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub rejected: Option<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Credential {
    Object(Box<CredentialObject>),
    Other(Value),
}
impl<'de> Deserialize<'de> for Credential {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            value @ Value::Object(_) => CredentialObject::deserialize(value)
                .map(|object| Self::Object(Box::new(object)))
                .map_err(serde::de::Error::custom),
            value @ (Value::Null
            | Value::Bool(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Array(_)) => Ok(Self::Other(value)),
        }
    }
}
impl Credential {
    pub fn from_value(value: Value) -> Self {
        Self::deserialize(value).expect("all JSON credential values are supported")
    }
    pub fn into_value(self) -> Value {
        let Self::Object(object) = self else {
            let Self::Other(value) = self else {
                unreachable!()
            };
            return value;
        };
        let CredentialObject {
            origin,
            server_id,
            client_id,
            invite_id,
            private_key,
            body,
            receipt,
            rejected,
            mut extra,
        } = *object;
        for (name, field) in [
            ("origin", origin),
            ("serverId", server_id),
            ("clientId", client_id),
            ("inviteId", invite_id),
            ("privateKey", private_key),
            ("body", body),
        ] {
            field.put(&mut extra, name);
        }
        if let Some(value) = receipt {
            extra.insert("receipt".into(), value);
        }
        if let Some(value) = rejected {
            extra.insert("rejected".into(), value);
        }
        Value::Object(extra)
    }
    pub fn field(&self, name: &str) -> Result<&str, String> {
        match self {
            Self::Object(object) => match name {
                "origin" => &object.origin,
                "serverId" => &object.server_id,
                "clientId" => &object.client_id,
                "inviteId" => &object.invite_id,
                "privateKey" => &object.private_key,
                "body" => &object.body,
                _ => return Err(format!("Missing or invalid {name}.")),
            }
            .text(name),
            Self::Other(_) => Err(format!("Missing or invalid {name}.")),
        }
    }
    pub fn object(&self) -> Option<&CredentialObject> {
        if let Self::Object(value) = self {
            Some(value)
        } else {
            None
        }
    }
    pub fn object_mut(&mut self) -> Option<&mut CredentialObject> {
        if let Self::Object(value) = self {
            Some(value)
        } else {
            None
        }
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Other(Value::Null))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Group {
    Entries(std::collections::BTreeMap<String, Credential>),
    Other(Value),
}
impl<'de> Deserialize<'de> for Group {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            value @ Value::Object(_) => {
                std::collections::BTreeMap::<String, Credential>::deserialize(value)
                    .map(Self::Entries)
                    .map_err(serde::de::Error::custom)
            }
            value @ (Value::Null
            | Value::Bool(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Array(_)) => Ok(Self::Other(value)),
        }
    }
}
impl Group {
    fn into_value(self) -> Value {
        match self {
            Self::Entries(map) => Value::Object(
                map.into_iter()
                    .map(|(key, value)| (key, value.into_value()))
                    .collect(),
            ),
            Self::Other(value) => value,
        }
    }
    pub fn entries(&self) -> Option<&std::collections::BTreeMap<String, Credential>> {
        if let Self::Entries(value) = self {
            Some(value)
        } else {
            None
        }
    }
    pub fn entries_mut(&mut self) -> Option<&mut std::collections::BTreeMap<String, Credential>> {
        if let Self::Entries(value) = self {
            Some(value)
        } else {
            None
        }
    }
}
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct VaultObject {
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub drafts: Option<Group>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub servers: Option<Group>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub imports: Option<Value>,
    /// The one key of this device: `{"id", "publicKey", "privateKey"}`.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub device: Option<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum VaultState {
    Object(VaultObject),
    Other(Value),
}
impl<'de> Deserialize<'de> for VaultState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            value @ Value::Object(_) => VaultObject::deserialize(value)
                .map(Self::Object)
                .map_err(serde::de::Error::custom),
            value @ (Value::Null
            | Value::Bool(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Array(_)) => Ok(Self::Other(value)),
        }
    }
}
impl VaultState {
    pub fn from_value(value: Value) -> Self {
        Self::deserialize(value).expect("all JSON vault values are supported")
    }
    pub fn into_value(self) -> Value {
        let Self::Object(object) = self else {
            let Self::Other(value) = self else {
                unreachable!()
            };
            return value;
        };
        let VaultObject {
            drafts,
            servers,
            imports,
            device,
            mut extra,
        } = object;
        if let Some(value) = drafts {
            extra.insert("drafts".into(), value.into_value());
        }
        if let Some(value) = servers {
            extra.insert("servers".into(), value.into_value());
        }
        if let Some(value) = imports {
            extra.insert("imports".into(), value);
        }
        if let Some(value) = device {
            extra.insert("device".into(), value);
        }
        Value::Object(extra)
    }
    pub fn group(&self, name: &str) -> Option<&Group> {
        match self {
            Self::Object(object) => match name {
                "drafts" => object.drafts.as_ref(),
                "servers" => object.servers.as_ref(),
                _ => None,
            },
            Self::Other(_) => None,
        }
    }
    pub fn group_mut(&mut self, name: &str) -> Option<&mut Group> {
        match self {
            Self::Object(object) => match name {
                "drafts" => object.drafts.as_mut(),
                "servers" => object.servers.as_mut(),
                _ => None,
            },
            Self::Other(_) => None,
        }
    }
    pub fn object(&self) -> Option<&VaultObject> {
        if let Self::Object(object) = self {
            Some(object)
        } else {
            None
        }
    }
    pub fn object_mut(&mut self) -> Option<&mut VaultObject> {
        if let Self::Object(object) = self {
            Some(object)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serde_vault_preserves_partial_invalid_and_opaque_fields() {
        for value in [
            json!(null),
            json!([1,{"future":true}]),
            json!({}),
            json!({"drafts":null,"servers":{"a":null,"b":42,"c":{"origin":null,"serverId":17,"clientId":"","receipt":null,"rejected":false,"future":[1,{"n":1.0}] }},"imports":null,"future":{"v":true}}),
            json!({"drafts":{"x":{"body":"exact body","privateKey":null}},"servers":false,"imports":[]}),
        ] {
            let owner: VaultState = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(&owner).unwrap(), value);
            assert_eq!(owner.into_value(), value);
        }
    }
}
