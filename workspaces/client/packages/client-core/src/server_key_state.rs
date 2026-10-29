//! Permissive persisted vault: malformed legacy values are checked when accessed.
use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq)]
pub enum TextField {
    Missing,
    Text(String),
    Other(Value),
}
impl TextField {
    fn take(map: &mut Map<String, Value>, key: &str) -> Self {
        match map.remove(key) {
            None => Self::Missing,
            Some(Value::String(text)) => Self::Text(text),
            Some(value) => Self::Other(value),
        }
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

#[derive(Clone, Debug, PartialEq)]
pub struct CredentialObject {
    pub origin: TextField,
    pub server_id: TextField,
    pub client_id: TextField,
    pub invite_id: TextField,
    pub private_key: TextField,
    pub body: TextField,
    pub receipt: Option<Value>,
    pub rejected: Option<Value>,
    pub extra: Map<String, Value>,
}
#[derive(Clone, Debug, PartialEq)]
pub enum Credential {
    Object(Box<CredentialObject>),
    Other(Value),
}
impl Credential {
    pub fn from_value(value: Value) -> Self {
        let Value::Object(mut map) = value else {
            return Self::Other(value);
        };
        Self::Object(Box::new(CredentialObject {
            origin: TextField::take(&mut map, "origin"),
            server_id: TextField::take(&mut map, "serverId"),
            client_id: TextField::take(&mut map, "clientId"),
            invite_id: TextField::take(&mut map, "inviteId"),
            private_key: TextField::take(&mut map, "privateKey"),
            body: TextField::take(&mut map, "body"),
            receipt: map.remove("receipt"),
            rejected: map.remove("rejected"),
            extra: map,
        }))
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

#[derive(Clone, Debug, PartialEq)]
pub enum Group {
    Entries(std::collections::BTreeMap<String, Credential>),
    Other(Value),
}
impl Group {
    fn from_value(value: Value) -> Self {
        match value {
            Value::Object(map) => Self::Entries(
                map.into_iter()
                    .map(|(key, value)| (key, Credential::from_value(value)))
                    .collect(),
            ),
            other @ (Value::Null
            | Value::Bool(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Array(_)) => Self::Other(other),
        }
    }
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
#[derive(Clone, Debug, PartialEq)]
pub struct VaultObject {
    pub drafts: Option<Group>,
    pub servers: Option<Group>,
    pub imports: Option<Value>,
    pub extra: Map<String, Value>,
}
#[derive(Clone, Debug, PartialEq)]
pub enum VaultState {
    Object(VaultObject),
    Other(Value),
}
impl VaultState {
    pub fn from_value(value: Value) -> Self {
        let Value::Object(mut map) = value else {
            return Self::Other(value);
        };
        Self::Object(VaultObject {
            drafts: map.remove("drafts").map(Group::from_value),
            servers: map.remove("servers").map(Group::from_value),
            imports: map.remove("imports"),
            extra: map,
        })
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
    pub fn object_mut(&mut self) -> Option<&mut VaultObject> {
        if let Self::Object(object) = self {
            Some(object)
        } else {
            None
        }
    }
}
