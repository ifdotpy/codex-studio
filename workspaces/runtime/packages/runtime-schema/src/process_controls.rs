//! Process action commands and durable outcomes. Native provider payloads stay opaque.
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;

// JSON Schema integer ingress accepts integral decimal spellings. The retained
// transport value, rather than this numeric projection, remains identity evidence.
fn epoch<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let number = serde_json::Number::deserialize(deserializer)?;
    number
        .as_u64()
        .filter(|value| *value <= crate::shared_values::MAX_SAFE_INTEGER as u64)
        .or_else(|| {
            let value = number.as_f64()?;
            (value.is_finite()
                && value.fract() == 0.0
                && value >= 0.0
                && value <= crate::shared_values::MAX_SAFE_INTEGER as f64)
                .then_some(value as u64)
        })
        .ok_or_else(|| serde::de::Error::custom("Process epoch must be a nonnegative integer"))
}

/// Maximum character length of a process action's request, owner, account, or native response identity.
pub const ACTION_ID_MAX_CODEPOINTS: usize = 200;
/// Maximum encoded process identity key length, in characters.
pub const PROCESS_KEY_MAX_CODEPOINTS: usize = 2048;
/// Maximum process generation digest length, in characters.
pub const GENERATION_MAX_CODEPOINTS: usize = 256;
/// Maximum process identities in an owned-stop snapshot.
pub const MAX_PROCESS_SNAPSHOT: usize = 500;
/// Maximum UTF-8 byte length of the running command item targeted by a background move.
pub const BACKGROUND_ITEM_MAX_BYTES: usize = 200;
/// Each Unicode code point consumes at least one UTF-8 byte; ingress also applies the byte bound.
pub const BACKGROUND_ITEM_MAX_CODEPOINTS: usize = BACKGROUND_ITEM_MAX_BYTES;

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProcessActionScope {
    #[schemars(length(min = 1, max = ACTION_ID_MAX_CODEPOINTS))]
    pub request_id: String,
    #[schemars(length(min = 1, max = ACTION_ID_MAX_CODEPOINTS))]
    pub agent: String,
    #[schemars(length(min = 1, max = ACTION_ID_MAX_CODEPOINTS))]
    pub account_key: String,
    #[schemars(range(min = 0, max = crate::shared_values::MAX_SAFE_INTEGER))]
    #[serde(deserialize_with = "epoch")]
    pub epoch: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProcessIdentity {
    #[schemars(length(min = 1, max = PROCESS_KEY_MAX_CODEPOINTS))]
    pub key: String,
    #[schemars(length(min = 1, max = GENERATION_MAX_CODEPOINTS))]
    pub generation: String,
    #[schemars(length(min = 1, max = ACTION_ID_MAX_CODEPOINTS))]
    pub agent: String,
    #[schemars(length(min = 1, max = ACTION_ID_MAX_CODEPOINTS))]
    pub account_key: String,
    #[schemars(range(min = 0, max = crate::shared_values::MAX_SAFE_INTEGER))]
    #[serde(deserialize_with = "epoch")]
    pub epoch: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProcessResponseTarget {
    #[schemars(length(min = 1, max = ACTION_ID_MAX_CODEPOINTS))]
    pub agent: String,
    #[schemars(length(min = 1, max = ACTION_ID_MAX_CODEPOINTS))]
    pub account_key: String,
    #[schemars(range(min = 0, max = crate::shared_values::MAX_SAFE_INTEGER))]
    #[serde(deserialize_with = "epoch")]
    pub epoch: u64,
    #[schemars(length(min = 1, max = ACTION_ID_MAX_CODEPOINTS))]
    pub connection_id: String,
    #[schemars(length(min = 1, max = ACTION_ID_MAX_CODEPOINTS))]
    pub thread_id: String,
    #[schemars(length(min = 1, max = ACTION_ID_MAX_CODEPOINTS))]
    pub turn_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProcessControlRequest {
    #[serde(rename_all = "camelCase")]
    StopSelected {
        #[serde(flatten)]
        scope: ProcessActionScope,
        process: ProcessIdentity,
        #[serde(default)]
        include_descendants: bool,
        // Historical clients may send inactive nulls and an empty snapshot.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_target: Option<()>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<()>,
        #[serde(default)]
        #[schemars(extend("const" = []))]
        processes: [ProcessIdentity; 0],
    },
    #[serde(rename_all = "camelCase")]
    StopOwned {
        #[serde(flatten)]
        scope: ProcessActionScope,
        #[schemars(length(min = 1, max = MAX_PROCESS_SNAPSHOT))]
        processes: Vec<ProcessIdentity>,
        #[serde(default)]
        include_descendants: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        process: Option<()>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_target: Option<()>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<()>,
    },
    #[serde(rename_all = "camelCase")]
    StopResponse {
        #[serde(flatten)]
        scope: ProcessActionScope,
        response_target: ProcessResponseTarget,
        #[serde(default)]
        #[schemars(extend("const" = false))]
        include_descendants: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        process: Option<()>,
        #[serde(default)]
        #[schemars(extend("const" = []))]
        processes: [ProcessIdentity; 0],
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<()>,
    },
    #[serde(rename_all = "camelCase")]
    BackgroundCommand {
        #[serde(flatten)]
        scope: ProcessActionScope,
        response_target: ProcessResponseTarget,
        #[schemars(length(min = 1, max = BACKGROUND_ITEM_MAX_CODEPOINTS))]
        item_id: String,
        #[serde(default)]
        #[schemars(extend("const" = false))]
        include_descendants: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        process: Option<()>,
        #[serde(default)]
        #[schemars(extend("const" = []))]
        processes: [ProcessIdentity; 0],
    },
}
impl ProcessControlRequest {
    pub fn scope(&self) -> &ProcessActionScope {
        match self {
            Self::StopSelected { scope, .. }
            | Self::StopOwned { scope, .. }
            | Self::StopResponse { scope, .. }
            | Self::BackgroundCommand { scope, .. } => scope,
        }
    }
}

/// Keep the original transport body: inactive nulls and omissions are signature data.
#[derive(Clone, Debug)]
pub struct Request {
    raw: Value,
    command: ProcessControlRequest,
}
impl<'de> Deserialize<'de> for Request {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Box::<serde_json::value::RawValue>::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}
impl Request {
    pub fn raw(&self) -> &Value {
        &self.raw
    }
    pub fn command(&self) -> &ProcessControlRequest {
        &self.command
    }
    pub fn parse(raw: &serde_json::value::RawValue) -> Result<Self, String> {
        let command: ProcessControlRequest =
            serde_json::from_str(raw.get()).map_err(|error| error.to_string())?;
        let raw = serde_json::from_str(raw.get()).map_err(|error| error.to_string())?;
        match &command {
            ProcessControlRequest::StopOwned { processes, .. } if processes.is_empty() => {
                return Err("No owned process identities were supplied".into());
            }
            ProcessControlRequest::StopResponse {
                scope,
                response_target,
                include_descendants,
                ..
            }
            | ProcessControlRequest::BackgroundCommand {
                scope,
                response_target,
                include_descendants,
                ..
            } => {
                if *include_descendants {
                    return Err("Descendant scope applies only to process stop".into());
                }
                if scope.agent != response_target.agent
                    || scope.account_key != response_target.account_key
                    || scope.epoch != response_target.epoch
                {
                    return Err("Response identity does not match the process owner".into());
                }
            }
            ProcessControlRequest::StopSelected { .. }
            | ProcessControlRequest::StopOwned { .. } => {}
        }
        if let ProcessControlRequest::BackgroundCommand { item_id, .. } = &command
            && (item_id.is_empty() || item_id.len() > BACKGROUND_ITEM_MAX_BYTES)
        {
            return Err("Background move requires exactly one command item".into());
        }
        Ok(Self { raw, command })
    }
}

#[cfg(test)]
fn parse_fixture(value: &Value) -> Result<Request, String> {
    let raw = serde_json::value::RawValue::from_string(value.to_string()).expect("fixture JSON");
    Request::parse(&raw)
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProcessOutcome {
    Pending,
    Stopped,
    AlreadyFinished,
    Requested,
    Stale { error: String },
    Failed { error: String },
    Uncertain { error: Option<Value> },
}
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResponseOutcome {
    Requested,
    Backgrounded,
    NotRunning,
    Stale { error: String },
    Failed { error: String },
    Uncertain { error: Option<String> },
}
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct ProcessEntry {
    pub key: String,
    #[schemars(length(min = 1, max = ACTION_ID_MAX_CODEPOINTS))]
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    #[serde(flatten)]
    pub outcome: ProcessOutcome,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResponseEntry {
    #[schemars(length(min = 1, max = ACTION_ID_MAX_CODEPOINTS))]
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(flatten)]
    pub outcome: ResponseOutcome,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessActionStatus {
    Completed,
    Partial,
    Uncertain,
}
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProcessActionResult {
    #[schemars(length(min = 1, max = ACTION_ID_MAX_CODEPOINTS))]
    pub request_id: String,
    pub status: ProcessActionStatus,
    #[serde(default)]
    pub processes: Vec<ProcessEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ProcessResponse>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}
/// A response stop or background entry, or the receipt of HTTP monitor input.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum ProcessResponse {
    MonitorInput(MonitorInputResponse),
    Response(ResponseEntry),
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MonitorInputKind {
    MonitorInput,
}
/// Outcome of stdin sent to a monitor through HTTP; owned by monitor_http_input.rs.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MonitorInputStatus {
    Acknowledged,
    Rejected,
    Pending,
    Unknown,
}
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MonitorInputResponse {
    pub kind: MonitorInputKind,
    pub monitor_id: String,
    pub status: MonitorInputStatus,
}

#[derive(Serialize, JsonSchema)]
#[serde(untagged)]
pub enum ProcessReceipt {
    Found(FoundProcessReceipt),
    NotFound(MissingProcessReceipt),
}
#[derive(Serialize, JsonSchema)]
pub struct FoundProcessReceipt {
    #[schemars(extend("const" = true))]
    found: bool,
    pub result: ProcessActionResult,
}
#[derive(Serialize, JsonSchema)]
pub struct MissingProcessReceipt {
    #[schemars(extend("const" = false))]
    found: bool,
    result: (),
}
impl ProcessReceipt {
    /// Publish the typed result, including retained extension fields.
    pub fn found(result: ProcessActionResult) -> Self {
        Self::Found(FoundProcessReceipt {
            found: true,
            result,
        })
    }
    pub fn not_found() -> Self {
        Self::NotFound(MissingProcessReceipt {
            found: false,
            result: (),
        })
    }
}

fn schema_generator() -> schemars::SchemaGenerator {
    schemars::generate::SchemaSettings::draft2020_12()
        .for_deserialize()
        .with(|settings| settings.definitions_path = "/components/schemas".into())
        .into_generator()
}

pub(crate) fn schemas() -> Vec<(String, Value)> {
    let mut generator = schema_generator();
    generator.subschema_for::<ProcessControlRequest>();
    generator.subschema_for::<ProcessActionResult>();
    generator.subschema_for::<ProcessReceipt>();
    generator
        .definitions()
        .iter()
        .map(|(name, schema)| (name.clone(), schema.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_processes_defaults_to_empty_and_rejects_null() {
        let result: ProcessActionResult =
            serde_json::from_str(r#"{"requestId":"request","status":"completed"}"#)
                .expect("missing processes");
        assert_eq!(
            serde_json::to_value(result).unwrap()["processes"],
            json!([])
        );
        assert!(
            serde_json::from_str::<ProcessActionResult>(
                r#"{"requestId":"request","status":"completed","processes":null}"#,
            )
            .is_err()
        );
    }

    #[test]
    fn found_receipt_exposes_typed_result_and_preserves_wire_fields() {
        let result = json!({"requestId":"request", "status":"completed", "processes":[], "response": {
            "kind":"monitor_input", "monitorId":"monitor", "status":"acknowledged"
        }, "legacy":null});
        let typed =
            serde_json::from_str::<ProcessActionResult>(&result.to_string()).expect("valid result");
        let receipt = ProcessReceipt::found(typed);
        let ProcessReceipt::Found(found) = &receipt else {
            panic!("expected found receipt")
        };
        assert_eq!(found.result.status, ProcessActionStatus::Completed);
        assert!(matches!(
            found.result.response,
            Some(ProcessResponse::MonitorInput(MonitorInputResponse {
                status: MonitorInputStatus::Acknowledged,
                ..
            }))
        ));
        assert_eq!(
            serde_json::from_str::<Value>(
                &serde_json::to_string(&receipt).expect("serialize receipt")
            )
            .expect("decode receipt"),
            json!({"found":true,"result":result})
        );
        assert!(
            serde_json::from_str::<ProcessActionResult>(
                r#"{"requestId":"request","status":"invalid"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn process_entry_retains_opaque_extensions() {
        let raw = r#"{"key":"key","agent":"agent","status":"stopped","nativeExtension":[null,"retained"]}"#;
        let entry: ProcessEntry = serde_json::from_str(raw).expect("process entry");
        let encoded = serde_json::to_string(&entry).expect("serialize entry");
        assert_eq!(
            serde_json::from_str::<Value>(&encoded).expect("JSON"),
            serde_json::from_str::<Value>(raw).expect("fixture")
        );
    }

    #[test]
    fn schema_references_leave_literal_text_untouched() {
        #[derive(JsonSchema)]
        struct Child {
            value: String,
        }
        #[derive(JsonSchema)]
        struct Fixture {
            /// The literal #/$defs/Child is descriptive text.
            #[schemars(default = "literal")]
            text: String,
            child: Child,
        }
        fn literal() -> String {
            "#/$defs/Child".into()
        }
        let instance = Fixture {
            text: literal(),
            child: Child {
                value: String::new(),
            },
        };
        assert_eq!(instance.text, "#/$defs/Child");
        assert!(instance.child.value.is_empty());
        let mut generator = schema_generator();
        generator.subschema_for::<Fixture>();
        let fixture = &generator.definitions()["Fixture"];
        assert_eq!(fixture["properties"]["text"]["default"], "#/$defs/Child");
        assert_eq!(
            fixture["properties"]["text"]["description"],
            "The literal #/$defs/Child is descriptive text."
        );
        assert_eq!(
            fixture["properties"]["child"]["$ref"],
            "#/components/schemas/Child"
        );
        assert!(generator.definitions().contains_key("Child"));
    }
    #[test]
    fn process_command_variants_preserve_raw_legacy_fields() {
        let identity = json!({"key":"key","generation":"generation","agent":"owner","accountKey":"account","epoch":1});
        let target = json!({"agent":"owner","accountKey":"account","epoch":1,"connectionId":"connection","threadId":"thread","turnId":"turn"});
        for (action, fields) in [
            (
                "stop_selected",
                json!({"process":identity,"processes":[],"responseTarget":null,"itemId":null}),
            ),
            (
                "stop_owned",
                json!({"processes":[identity],"process":null,"responseTarget":null,"itemId":null}),
            ),
            (
                "stop_response",
                json!({"responseTarget":target,"process":null,"processes":[],"itemId":null}),
            ),
            (
                "background_command",
                json!({"responseTarget":target,"process":null,"processes":[],"itemId":"item"}),
            ),
        ] {
            let mut raw = json!({"action":action,"requestId":"request","agent":"owner","accountKey":"account","epoch":1});
            raw.as_object_mut()
                .expect("object")
                .extend(fields.as_object().expect("fields").clone());
            assert_eq!(parse_fixture(&raw).expect("legacy request").raw, raw);
            let required = match action {
                "stop_selected" => "process",
                "stop_owned" => "processes",
                "stop_response" | "background_command" => "responseTarget",
                _ => unreachable!(),
            };
            raw.as_object_mut().expect("object").remove(required);
            assert!(parse_fixture(&raw).is_err(), "{action} requires {required}");
        }
    }
    #[test]
    fn process_outcomes_require_status_specific_evidence() {
        for status in ["stale", "failed"] {
            assert!(serde_json::from_value::<ProcessOutcome>(json!({"status":status})).is_err());
            assert!(serde_json::from_value::<ResponseOutcome>(json!({"status":status})).is_err());
        }
        assert!(serde_json::from_value::<ProcessOutcome>(json!({"status":"invented"})).is_err());
        assert!(serde_json::from_value::<ResponseOutcome>(json!({"status":"invented"})).is_err());
    }
}

/// Commands and outcomes cannot omit the payload carried by their variant.
/// ```compile_fail
/// use studio_runtime_schema::process_controls::ProcessOutcome;
/// let failure = ProcessOutcome::Failed {};
/// ```
/// ```compile_fail
/// use studio_runtime_schema::process_controls::ProcessReceipt;
/// let receipt = ProcessReceipt::found();
/// // A found receipt carries its checked result.
/// let _: ProcessReceipt = receipt;
/// ```
/// ```compile_fail
/// use studio_runtime_schema::process_controls::{ProcessControlRequest, ProcessActionScope};
/// let command = ProcessControlRequest::BackgroundCommand {
///     scope: ProcessActionScope { request_id: "r".into(), agent: "a".into(), account_key: "account".into(), epoch: 1 },
///     // The current response target and command item cannot be omitted.
/// };
/// ```
const _: () = ();

pub(crate) fn check_schema_parity(openapi: &Value) -> Result<(), String> {
    for (name, schema) in schemas() {
        if openapi.pointer(&format!("/components/schemas/{name}")) != Some(&schema) {
            return Err(format!(
                "{name} must match runtime-schema/src/process_controls.rs"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod ingress_tests {
    use super::*;

    #[test]
    fn process_command_schema_and_serde_reject_cross_action_fields() {
        let schema = schemars::generate::SchemaSettings::draft2020_12()
            .for_deserialize()
            .into_generator()
            .into_root_schema_for::<ProcessControlRequest>()
            .to_value();
        let validator = jsonschema::validator_for(&schema).expect("command schema");
        let identity = json!({"key":"key","generation":"generation","agent":"owner","accountKey":"account","epoch":1});
        let target = json!({"agent":"owner","accountKey":"account","epoch":1,"connectionId":"connection","threadId":"thread","turnId":"turn"});
        for (action, fields, forbidden) in [
            (
                "stop_selected",
                json!({"process":identity}),
                json!({"responseTarget":target}),
            ),
            (
                "stop_owned",
                json!({"processes":[identity]}),
                json!({"process":identity}),
            ),
            (
                "stop_response",
                json!({"responseTarget":target}),
                json!({"processes":[identity]}),
            ),
            (
                "background_command",
                json!({"responseTarget":target,"itemId":"item"}),
                json!({"process":identity}),
            ),
        ] {
            let mut raw = json!({"action":action,"requestId":"request","agent":"owner","accountKey":"account","epoch":1});
            raw.as_object_mut()
                .expect("object")
                .extend(fields.as_object().expect("fields").clone());
            assert!(validator.is_valid(&raw), "schema: {raw}");
            assert!(parse_fixture(&raw).is_ok(), "serde: {raw}");
            raw.as_object_mut()
                .expect("object")
                .extend(forbidden.as_object().expect("fields").clone());
            assert!(!validator.is_valid(&raw), "schema: {raw}");
            assert!(parse_fixture(&raw).is_err(), "serde: {raw}");
        }
    }
}
