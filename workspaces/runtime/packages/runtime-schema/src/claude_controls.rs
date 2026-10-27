//! Claude settings and rollback command contract shared by HTTP and services.

use crate::{claude_permissions::PermissionMode, queue_command::Field};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

/// Maximum UTF-8 byte length of a Claude control agent, request, or turn identity.
pub const CONTROL_ID_MAX_BYTES: usize = 1024;
/// Schema upper bound in Unicode code points. Each code point uses at least one
/// UTF-8 byte; serde enforces the stronger byte bound on every ingress path.
pub const CONTROL_ID_MAX_CODEPOINTS: usize = CONTROL_ID_MAX_BYTES;
/// Inclusive compact window bounds, in tokens.
pub const COMPACT_WINDOW_MIN_TOKENS: u64 = 100_000;
pub const COMPACT_WINDOW_MAX_TOKENS: u64 = 1_000_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ControlId(#[schemars(length(min = 1, max = CONTROL_ID_MAX_CODEPOINTS))] String);

impl ControlId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de> Deserialize<'de> for ControlId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value.is_empty() || value.len() > CONTROL_ID_MAX_BYTES {
            return Err(serde::de::Error::custom(format!(
                "control identity must contain 1..={CONTROL_ID_MAX_BYTES} UTF-8 bytes"
            )));
        }
        Ok(Self(value))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CompactWindow(
    #[schemars(range(min = COMPACT_WINDOW_MIN_TOKENS, max = COMPACT_WINDOW_MAX_TOKENS))] u64,
);
impl<'de> Deserialize<'de> for CompactWindow {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = u64::deserialize(deserializer)?;
        if !(COMPACT_WINDOW_MIN_TOKENS..=COMPACT_WINDOW_MAX_TOKENS).contains(&value) {
            return Err(serde::de::Error::custom(
                "compact window is outside the token bounds",
            ));
        }
        Ok(Self(value))
    }
}

fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeSettings {
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "PermissionMode")]
    pub permission_mode: Option<PermissionMode>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "bool")]
    pub thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<CompactWindow>")]
    pub auto_compact_window: Field<CompactWindow>,
}

/// A validated mutation has exactly the arguments required by its action.
///
/// ```compile_fail
/// use studio_runtime_schema::claude_controls::ClaudeControlCommand;
/// let command = ClaudeControlCommand::Rollback { settings: Default::default() };
/// ```
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClaudeControlCommand {
    Settings {
        id: ControlId,
        request_id: ControlId,
        settings: ClaudeSettings,
    },
    Rollback {
        id: ControlId,
        request_id: ControlId,
        turn_id: ControlId,
        #[serde(default)]
        restore_files: bool,
    },
}
impl ClaudeControlCommand {
    pub fn id(&self) -> &str {
        match self {
            Self::Settings { id, .. } | Self::Rollback { id, .. } => id.as_str(),
        }
    }
    pub fn request_id(&self) -> &str {
        match self {
            Self::Settings { request_id, .. } | Self::Rollback { request_id, .. } => {
                request_id.as_str()
            }
        }
    }
    pub fn action_name(&self) -> &'static str {
        match self {
            Self::Settings { .. } => "settings",
            Self::Rollback { .. } => "rollback",
        }
    }
    pub fn restore_files(&self) -> bool {
        match self {
            Self::Settings { .. } => false,
            Self::Rollback { restore_files, .. } => *restore_files,
        }
    }
    pub fn operation_id(&self) -> String {
        format!("claude-control:{}", self.request_id())
    }
}

pub(crate) fn schemas() -> Vec<(&'static str, Value)> {
    let schema = schemars::generate::SchemaSettings::draft2020_12()
        .with(|settings| settings.inline_subschemas = true)
        .for_deserialize()
        .into_generator()
        .into_root_schema_for::<ClaudeControlCommand>()
        .to_value();
    let mut variants = schema["oneOf"]
        .as_array()
        .expect("tagged command variants")
        .clone()
        .into_iter();
    let mut settings_request = variants.next().expect("settings variant");
    let rollback_request = variants.next().expect("rollback variant");
    let settings = settings_request["properties"]["settings"].clone();
    settings_request["properties"]["settings"] =
        json!({"$ref":"#/components/schemas/ClaudeSettings"});
    vec![
        ("ClaudeSettings", settings),
        ("ClaudeSessionSettingsRequest", settings_request),
        ("ClaudeSessionRollbackRequest", rollback_request),
    ]
}

pub(crate) fn check_schema_parity(openapi: &Value) -> Result<(), String> {
    for (name, schema) in schemas() {
        if openapi.pointer(&format!("/components/schemas/{name}")) != Some(&schema) {
            return Err(format!(
                "{name} must match runtime-schema/src/claude_controls.rs"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(value: Value, expected: bool) {
        let schema = schemars::generate::SchemaSettings::draft2020_12()
            .for_deserialize()
            .into_generator()
            .into_root_schema_for::<ClaudeControlCommand>()
            .to_value();
        assert_eq!(
            jsonschema::validator_for(&schema)
                .expect("command schema")
                .is_valid(&value),
            expected,
            "schema: {value}"
        );
        assert_eq!(
            serde_json::from_value::<ClaudeControlCommand>(value.clone()).is_ok(),
            expected,
            "serde: {value}"
        );
    }

    #[test]
    fn claude_control_command_ingress_contract() {
        for action in ["settings", "rollback"] {
            let base = match action {
                "settings" => {
                    json!({"action":action,"id":"agent","request_id":"request","settings":{}})
                }
                "rollback" => {
                    json!({"action":action,"id":"agent","request_id":"request","turn_id":"turn"})
                }
                _ => unreachable!(),
            };
            check(base.clone(), true);
            for field in ["id", "request_id"] {
                let mut absent = base.clone();
                absent.as_object_mut().expect("object").remove(field);
                check(absent, false);
                for invalid in [
                    Value::Null,
                    json!(""),
                    json!(7),
                    json!(false),
                    json!([]),
                    json!({}),
                    json!("x".repeat(1025)),
                ] {
                    let mut wire = base.clone();
                    wire[field] = invalid;
                    check(wire, false);
                }
                let mut wire = base.clone();
                wire[field] = json!("x".repeat(1024));
                check(wire, true);
            }
            let mut unknown = base;
            unknown["extra"] = json!(true);
            check(unknown, false);
        }
        for (settings, expected) in [
            (json!({"thinking":null}), false),
            (json!({"thinking":false}), true),
            (json!({"thinking":"true"}), false),
            (json!({"permissionMode":null}), false),
            (json!({"permissionMode":"unknown"}), false),
            (json!({"autoCompactWindow":null}), true),
            (json!({"autoCompactWindow":99999}), false),
            (json!({"autoCompactWindow":100000}), true),
            (json!({"autoCompactWindow":1000000}), true),
            (json!({"autoCompactWindow":1000001}), false),
            (json!({"autoCompactWindow":100000.5}), false),
            (json!({"autoCompactWindow":"100000"}), false),
            (json!({"binaryPath":"ignored"}), false),
        ] {
            check(
                json!({"action":"settings","id":"agent","request_id":"request","settings":settings}),
                expected,
            );
        }
        for mode in crate::claude_permissions::PERMISSION_MODES {
            check(
                json!({"action":"settings","id":"agent","request_id":"request","settings":{"permissionMode":mode}}),
                true,
            );
        }
        for invalid in [Value::Null, json!(""), json!(1)] {
            check(
                json!({"action":"rollback","id":"agent","request_id":"request","turn_id":invalid}),
                false,
            );
            check(
                json!({"action":"rollback","id":"agent","request_id":"request","turn_id":"turn","restore_files":invalid}),
                false,
            );
        }
        for restore in [false, true] {
            check(
                json!({"action":"rollback","id":"agent","request_id":"request","turn_id":"turn","restore_files":restore}),
                true,
            );
        }
        check(
            json!({"action":"rollback","id":"agent","request_id":"request","turn_id":"turn","settings":{}}),
            false,
        );
        check(
            json!({"action":"settings","id":"agent","request_id":"request","settings":{},"restore_files":false}),
            false,
        );
        check(
            json!({"action":"settings","id":"agent","request_id":"request","settings":{},"turn_id":"turn"}),
            false,
        );
        check(
            json!({"action":"rollback","id":"agent","request_id":"request"}),
            false,
        );
        check(
            json!({"action":"settings","id":"agent","request_id":"request"}),
            false,
        );
        for wire in [
            Value::Null,
            json!([]),
            json!(true),
            json!({"action":"unknown"}),
        ] {
            check(wire, false);
        }
    }

    #[test]
    fn claude_control_identity_limit_remains_utf8_bytes() {
        for (text, expected) in [("🙂".repeat(256), true), ("🙂".repeat(257), false)] {
            assert_eq!(
                serde_json::from_value::<ControlId>(json!(text)).is_ok(),
                expected
            );
        }
    }

    #[test]
    fn claude_control_settings_preserve_presence() {
        for settings in [
            json!({}),
            json!({"autoCompactWindow":null}),
            json!({"thinking":false,"permissionMode":"plan","autoCompactWindow":100000}),
        ] {
            let typed: ClaudeSettings = serde_json::from_value(settings.clone()).expect("settings");
            assert_eq!(serde_json::to_value(typed).expect("serialize"), settings);
        }
        let schema = schemars::schema_for!(PermissionMode).to_value();
        assert_eq!(
            schema["enum"],
            json!(crate::claude_permissions::PERMISSION_MODES)
        );
    }

    #[test]
    fn claude_control_schema_parity_rejects_drift() {
        let openapi: Value =
            serde_json::from_str(include_str!("../../runtime-http/openapi.json")).expect("OpenAPI");
        check_schema_parity(&openapi).expect("generated command schemas");
        for (name, _) in schemas() {
            let mut drifted = openapi.clone();
            drifted["components"]["schemas"][name] = json!({"type":"object"});
            assert!(
                check_schema_parity(&drifted)
                    .expect_err("drift")
                    .contains("claude_controls.rs")
            );
        }
    }

    #[test]
    fn settings_schema_rejects_null_nonnullable_fields() {
        let openapi: Value =
            serde_json::from_str(include_str!("../../runtime-http/openapi.json")).expect("OpenAPI");
        let validator =
            jsonschema::validator_for(&openapi["components"]["schemas"]["ClaudeSettings"])
                .expect("settings schema");
        for field in ["thinking", "permissionMode"] {
            assert!(
                !validator.is_valid(&json!({field:null})),
                "{field} must reject null"
            );
        }
        assert!(validator.is_valid(&json!({"autoCompactWindow":null})));
        assert!(validator.is_valid(&json!({})));
    }
}
