//! Persisted workspace operation and its open provider evidence.
use crate::{json_object::JsonObject, queue_command::Field};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::ops::{Deref, DerefMut};

macro_rules! wire_enum {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(try_from = "String")]
        pub enum $name { $(#[serde(rename = $wire)] $variant),+ }
        impl TryFrom<String> for $name {
            type Error = &'static str;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                match value.as_str() { $($wire => Ok(Self::$variant),)+ _ => Err("invalid workspace operation value") }
            }
        }
    };
}
wire_enum!(OperationKind { Capture => "capture", Checkpoint => "checkpoint", ClaudeRollback => "claude_rollback", ClaudeSettings => "claude_settings", Restore => "restore", Branch => "branch", Provision => "provision" });
wire_enum!(OperationPhase { CapturePending => "capture_pending", CaptureRunning => "capture_running", ProviderPending => "provider_pending", ProviderReady => "provider_ready", LocalMutation => "local_mutation", RecoveryRequired => "recovery_required", Completed => "completed", Failed => "failed" });

#[derive(Clone, Debug, PartialEq, Serialize)]
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationFields {
    pub id: String,
    pub kind: OperationKind,
    pub agent: String,
    pub phase: OperationPhase,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub signature: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub source: Field<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub provider: Field<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub result: Field<Value>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn operation_wire_matches_schema_and_preserves_presence_and_extensions() {
        let schema: Value = serde_json::from_str(include_str!(
            "schemas/persisted-workspace-operation-record.schema.json"
        ))
        .unwrap();
        let fields: Vec<_> = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            fields,
            [
                "agent",
                "id",
                "kind",
                "phase",
                "provider",
                "restartHold",
                "result",
                "signature",
                "source"
            ]
        );
        assert_eq!(
            schema["properties"]["kind"]["enum"],
            json!([
                "capture",
                "checkpoint",
                "restore",
                "branch",
                "provision",
                "claude_settings",
                "claude_rollback"
            ])
        );
        assert_eq!(
            schema["properties"]["phase"]["enum"],
            json!([
                "capture_pending",
                "capture_running",
                "provider_pending",
                "provider_ready",
                "local_mutation",
                "recovery_required",
                "completed",
                "failed"
            ])
        );
        for kind in [
            OperationKind::Capture,
            OperationKind::Checkpoint,
            OperationKind::Restore,
            OperationKind::Branch,
            OperationKind::Provision,
            OperationKind::ClaudeSettings,
            OperationKind::ClaudeRollback,
        ] {
            assert!(
                schema["properties"]["kind"]["enum"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(kind))
            );
        }
        for phase in [
            OperationPhase::CapturePending,
            OperationPhase::CaptureRunning,
            OperationPhase::ProviderPending,
            OperationPhase::ProviderReady,
            OperationPhase::LocalMutation,
            OperationPhase::RecoveryRequired,
            OperationPhase::Completed,
            OperationPhase::Failed,
        ] {
            assert!(
                schema["properties"]["phase"]["enum"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(phase))
            );
        }
        let base = json!({"id":"op","agent":"a","kind":"branch","phase":"provider_pending","other":{"opaque":[1,null]}});
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
