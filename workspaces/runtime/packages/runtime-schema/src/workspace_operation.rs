//! Persisted workspace operation and its open provider evidence.
use crate::{json_object::JsonObject, queue_command::Field};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::ops::{Deref, DerefMut};

macro_rules! wire_enum {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, schemars::JsonSchema)]
        #[schemars(inline)]
        pub enum $name { $(#[serde(rename = $wire)] $variant),+ }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                match value.as_str() {
                    $($wire => Ok(Self::$variant),)+
                    _ => Err(serde::de::Error::custom("invalid workspace operation value")),
                }
            }
        }
    };
}

wire_enum!(OperationKind { Capture => "capture", Checkpoint => "checkpoint", Restore => "restore", Branch => "branch", Provision => "provision", ClaudeSettings => "claude_settings", ClaudeRollback => "claude_rollback" });
wire_enum!(OperationPhase { CapturePending => "capture_pending", CaptureRunning => "capture_running", ProviderPending => "provider_pending", ProviderReady => "provider_ready", LocalMutation => "local_mutation", RecoveryRequired => "recovery_required", Completed => "completed", Failed => "failed" });

#[derive(Clone, Debug, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct WorkspaceOperationRecord(pub OperationFields);
impl<'de> Deserialize<'de> for WorkspaceOperationRecord {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        JsonObject::<OperationFields>::deserialize(deserializer).map(|object| Self(object.0))
    }
}
impl Deref for WorkspaceOperationRecord {
    type Target = OperationFields;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for WorkspaceOperationRecord {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename = "PersistedWorkspaceOperationRecord")]
pub struct OperationFields {
    pub id: String,
    pub kind: OperationKind,
    pub agent: String,
    pub phase: OperationPhase,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    pub signature: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<Map<String, Value>>")]
    pub source: Field<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<Map<String, Value>>")]
    pub provider: Field<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Value")]
    pub result: Field<Value>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<bool>")]
    pub restart_hold: Field<bool>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

impl OperationFields {
    pub fn extensions(&self) -> &Map<String, Value> {
        &self.extra
    }
    pub fn set_extension(&mut self, key: &str, value: Value) -> Result<(), &'static str> {
        if matches!(
            key,
            "id" | "kind"
                | "agent"
                | "phase"
                | "signature"
                | "source"
                | "provider"
                | "result"
                | "restartHold"
        ) {
            return Err("operation field is not an extension");
        }
        self.extra.insert(key.to_owned(), value);
        Ok(())
    }
}

pub(crate) fn schema() -> Value {
    schemars::schema_for!(WorkspaceOperationRecord).to_value()
}
pub(crate) fn check_schema_parity() -> Result<(), String> {
    let stored: Value = serde_json::from_slice(include_bytes!(
        "schemas/persisted-workspace-operation-record.schema.json"
    ))
    .map_err(|error| error.to_string())?;
    if schema() != stored {
        return Err("persisted workspace operation schema must be generated from workspace_operation::WorkspaceOperationRecord".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn operation_wire_matches_schema_and_preserves_presence_and_extensions() {
        check_schema_parity().unwrap();
        let base = json!({"id":"op","agent":"a","kind":"branch","phase":"provider_pending","other":{"opaque":[1,null]}});
        let generated = schema();
        for required in generated["required"].as_array().unwrap() {
            let mut missing = base.clone();
            missing
                .as_object_mut()
                .unwrap()
                .remove(required.as_str().unwrap());
            assert!(
                WorkspaceOperationRecord::deserialize(&missing).is_err(),
                "{missing}"
            );
            assert!(!crate::validate("PersistedWorkspaceOperationRecord", &missing).unwrap());
        }
        for field in ["kind", "phase"] {
            for value in generated["properties"][field]["enum"].as_array().unwrap() {
                let mut input = base.clone();
                input[field] = value.clone();
                let typed = WorkspaceOperationRecord::deserialize(&input).unwrap();
                assert_eq!(serde_json::to_value(typed).unwrap(), input);
            }
        }
        for (key, value) in [
            ("id", json!(false)),
            ("agent", json!(null)),
            ("kind", json!({"branch":null})),
            ("phase", json!("other")),
            ("source", json!([])),
            ("provider", json!(false)),
            ("signature", json!(1)),
            ("restartHold", json!("true")),
            ("source", json!(null)),
            ("provider", json!({"future":[true]})),
            ("result", json!([null,{"future":true}])),
            ("signature", json!(null)),
            ("restartHold", json!(null)),
        ] {
            let mut input = base.clone();
            input[key] = value;
            let expected = crate::validate("PersistedWorkspaceOperationRecord", &input).unwrap();
            let decoded = WorkspaceOperationRecord::deserialize(&input);
            assert_eq!(decoded.is_ok(), expected, "{input}");
            if let Ok(decoded) = decoded {
                assert_eq!(serde_json::to_value(decoded).unwrap(), input);
            }
        }
        assert!(
            WorkspaceOperationRecord::deserialize(json!(["op", "branch", "a", "completed"]))
                .is_err()
        );
        let mut decoded = WorkspaceOperationRecord::deserialize(&base).unwrap();
        assert!(decoded.set_extension("phase", json!("failed")).is_err());
        assert_eq!(serde_json::to_value(&decoded).unwrap(), base);
        decoded
            .set_extension("future", json!({"opaque":true}))
            .unwrap();
        assert_eq!(decoded.extensions()["future"], json!({"opaque":true}));
    }
}
