//! Compiled validators for the schemas persisted by the Rust runtime.

use std::collections::BTreeMap;
use std::sync::OnceLock;

pub use jsonschema::{Draft, Registry as JsonSchemaRegistry, Validator};
use serde_json::{Value, json};

pub const OPENAPI_URI: &str = "https://codex-studio.invalid/openapi.json";
const ACCOUNT_REGISTRY_URI: &str = "https://codex-studio.invalid/account-registry.schema.json";
const USAGE_LIMITS_URI: &str = "https://codex-studio.invalid/workspace-usage-limits.schema.json";
const TRANSCRIPT_ASSET_URI: &str =
    "https://codex-studio.invalid/runtime-transcript-asset.schema.json";
const TRANSCRIPT_RECORD_URI: &str =
    "https://codex-studio.invalid/persisted-transcript-record.schema.json";
const EVENT_METADATA_URI: &str = "https://codex-studio.invalid/runtime-event-metadata.schema.json";

const OPENAPI: &[u8] = include_bytes!("../../runtime-http/openapi.json");
const ACCOUNT_REGISTRY: &[u8] = include_bytes!("schemas/account-registry.schema.json");
const USAGE_LIMITS: &[u8] = include_bytes!("schemas/workspace-usage-limits.schema.json");
const TRANSCRIPT_ASSET: &[u8] = include_bytes!("schemas/runtime-transcript-asset.schema.json");
const TRANSCRIPT_RECORD: &[u8] = include_bytes!("schemas/persisted-transcript-record.schema.json");
const EVENT_METADATA: &[u8] = include_bytes!("schemas/runtime-event-metadata.schema.json");

type SchemaResult<T> = Result<T, String>;

struct CompiledSchemas {
    validators: BTreeMap<String, Validator>,
}

static COMPILED_SCHEMAS: OnceLock<SchemaResult<CompiledSchemas>> = OnceLock::new();

/// Build the reference registry from the committed OpenAPI and persisted-state schemas.
pub fn registry_for_document(document: &Value) -> SchemaResult<JsonSchemaRegistry<'_>> {
    let account_registry: Value = serde_json::from_slice(ACCOUNT_REGISTRY)
        .map_err(|error| format!("cannot parse account-registry schema: {error}"))?;
    let usage_limits: Value = serde_json::from_slice(USAGE_LIMITS)
        .map_err(|error| format!("cannot parse workspace-usage-limits schema: {error}"))?;
    let transcript_asset: Value = serde_json::from_slice(TRANSCRIPT_ASSET)
        .map_err(|error| format!("cannot parse persisted transcript-asset schema: {error}"))?;
    let transcript_record: Value = serde_json::from_slice(TRANSCRIPT_RECORD)
        .map_err(|error| format!("cannot parse persisted transcript-record schema: {error}"))?;
    let event_metadata: Value = serde_json::from_slice(EVENT_METADATA)
        .map_err(|error| format!("cannot parse event-metadata schema: {error}"))?;
    registry_for_documents(
        document,
        &account_registry,
        &usage_limits,
        &transcript_asset,
        &transcript_record,
        &event_metadata,
    )
}

fn registry_for_documents<'document>(
    document: &Value,
    account_registry: &Value,
    usage_limits: &Value,
    transcript_asset: &Value,
    transcript_record: &Value,
    event_metadata: &Value,
) -> SchemaResult<JsonSchemaRegistry<'document>> {
    JsonSchemaRegistry::new()
        .draft(Draft::Draft202012)
        .add(
            OPENAPI_URI,
            jsonschema::Resource::from_contents(document.clone()),
        )
        .map_err(|error| error.to_string())?
        .add(
            ACCOUNT_REGISTRY_URI,
            jsonschema::Resource::from_contents(account_registry.clone()),
        )
        .map_err(|error| error.to_string())?
        .add(
            USAGE_LIMITS_URI,
            jsonschema::Resource::from_contents(usage_limits.clone()),
        )
        .map_err(|error| error.to_string())?
        .add(
            TRANSCRIPT_ASSET_URI,
            jsonschema::Resource::from_contents(transcript_asset.clone()),
        )
        .map_err(|error| error.to_string())?
        .add(
            TRANSCRIPT_RECORD_URI,
            jsonschema::Resource::from_contents(transcript_record.clone()),
        )
        .map_err(|error| error.to_string())?
        .add(
            EVENT_METADATA_URI,
            jsonschema::Resource::from_contents(event_metadata.clone()),
        )
        .map_err(|error| error.to_string())?
        .prepare()
        .map_err(|error| error.to_string())
}

/// Ensure every schema used by persisted Rust records has compiled successfully.
pub fn initialize() -> SchemaResult<()> {
    compiled_schemas().map(|_| ())
}

/// Validate one persisted value against its named schema.
pub fn validate(schema: &str, value: &Value) -> SchemaResult<bool> {
    let validator = compiled_schemas()?
        .validators
        .get(schema)
        .ok_or_else(|| format!("unknown persisted schema {schema}"))?;
    Ok(validator.is_valid(value))
}

/// Return the first validation error's JSON pointer and keyword, if any.
pub fn first_error(schema: &str, value: &Value) -> SchemaResult<Option<(String, String)>> {
    let validator = compiled_schemas()?
        .validators
        .get(schema)
        .ok_or_else(|| format!("unknown persisted schema {schema}"))?;
    Ok(validator.iter_errors(value).next().map(|error| {
        (
            error.instance_path().to_string(),
            error.kind().keyword().to_owned(),
        )
    }))
}

fn compiled_schemas() -> SchemaResult<&'static CompiledSchemas> {
    match COMPILED_SCHEMAS.get_or_init(compile_schemas) {
        Ok(schemas) => Ok(schemas),
        Err(message) => Err(message.clone()),
    }
}

fn compile_schemas() -> SchemaResult<CompiledSchemas> {
    let openapi: Value = serde_json::from_slice(OPENAPI)
        .map_err(|error| format!("cannot parse OpenAPI schema document: {error}"))?;
    let account_registry: Value = serde_json::from_slice(ACCOUNT_REGISTRY)
        .map_err(|error| format!("cannot parse account-registry schema: {error}"))?;
    let usage_limits: Value = serde_json::from_slice(USAGE_LIMITS)
        .map_err(|error| format!("cannot parse workspace-usage-limits schema: {error}"))?;
    let transcript_asset: Value = serde_json::from_slice(TRANSCRIPT_ASSET)
        .map_err(|error| format!("cannot parse persisted transcript-asset schema: {error}"))?;
    let transcript_record: Value = serde_json::from_slice(TRANSCRIPT_RECORD)
        .map_err(|error| format!("cannot parse persisted transcript-record schema: {error}"))?;
    let event_metadata: Value = serde_json::from_slice(EVENT_METADATA)
        .map_err(|error| format!("cannot parse event-metadata schema: {error}"))?;
    let registry = registry_for_documents(
        &openapi,
        &account_registry,
        &usage_limits,
        &transcript_asset,
        &transcript_record,
        &event_metadata,
    )?;
    let mut validators = BTreeMap::new();

    for name in [
        "AgentRecordResponse",
        "TaskDetailResponse",
        "WorkItem",
        "PlanView",
        "QueueItem",
        "TranscriptAsset",
        "RuleRecordResponse",
        "AnnotationReceipt",
        "CheckpointSummary",
        "CheckpointsResponse",
        "AgentRequest",
        "QuestionEntry",
        "QuestionHistory",
    ] {
        let reference = format!("{OPENAPI_URI}#/components/schemas/{name}");
        let validator = compile(&registry, json!({"$ref": reference}), name)?;
        validators.insert(name.to_owned(), validator);
    }
    for (name, uri) in [
        ("PersistedTranscriptAsset", TRANSCRIPT_ASSET_URI),
        ("PersistedTranscriptRecord", TRANSCRIPT_RECORD_URI),
        ("RuntimeEventMetadataRecord", EVENT_METADATA_URI),
        (
            "account-registry/AccountRegistryRecord",
            ACCOUNT_REGISTRY_URI,
        ),
        (
            "workspace-usage-limits/UsageLimitsWorkspaceEnvelope",
            USAGE_LIMITS_URI,
        ),
    ] {
        let validator = compile(&registry, json!({"$ref": uri}), name)?;
        validators.insert(name.to_owned(), validator);
    }

    compile_definitions(
        &registry,
        ACCOUNT_REGISTRY_URI,
        "account-registry",
        &account_registry,
        &mut validators,
    )?;
    compile_definitions(
        &registry,
        USAGE_LIMITS_URI,
        "workspace-usage-limits",
        &usage_limits,
        &mut validators,
    )?;
    Ok(CompiledSchemas { validators })
}

fn compile_definitions(
    registry: &JsonSchemaRegistry<'_>,
    document_uri: &str,
    namespace: &str,
    document: &Value,
    validators: &mut BTreeMap<String, Validator>,
) -> SchemaResult<()> {
    let definitions = document
        .get("$defs")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{namespace} schema has no $defs object"))?;
    for name in definitions.keys() {
        let reference = format!("{document_uri}#/$defs/{}", escape_pointer(name));
        let validator = compile(registry, json!({"$ref": reference}), name)?;
        validators.insert(format!("{namespace}/{name}"), validator);
    }
    Ok(())
}

fn compile(
    registry: &JsonSchemaRegistry<'_>,
    schema: Value,
    name: &str,
) -> SchemaResult<Validator> {
    jsonschema::options()
        .with_registry(registry)
        .with_pattern_options(jsonschema::PatternOptions::regex())
        .build(&schema)
        .map_err(|error| format!("cannot compile persisted schema {name}: {error}"))
}

fn escape_pointer(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{initialize, registry_for_document, validate};

    #[test]
    fn compiled_record_schemas_accept_known_records_and_reject_invalid_values() {
        initialize().expect("persisted schemas compile");
        assert!(
            validate(
                "AgentRecordResponse",
                &json!({"id":"agent-1","rootId":"agent-1","epoch":0,"status":"idle"})
            )
            .expect("agent schema exists")
        );
        assert!(validate(
            "WorkItem",
            &json!({"id":"task-1","rootId":"agent-1","title":"T","status":"ready","created":1,"updated":1,"version":1})
        )
        .expect("work item schema exists"));
        assert!(
            validate(
                "TaskDetailResponse",
                &json!({"id":"t","agent":"a","kind":"command","status":"completed","created":1})
            )
            .expect("runtime task schema exists")
        );
        assert!(validate(
            "PersistedTranscriptAsset",
            &json!({"id":"asset","agent":"a","name":"n","path":"/tmp/n","mime":"text/plain","image":false,"size":1,"hash":"sha256:x","created":1.0,"future":true})
        )
        .expect("persisted asset schema exists"));
        assert!(
            validate(
                "RuntimeEventMetadataRecord",
                &json!({"timing":{"enqueuedAt":123},"queueMutationRevision":4})
            )
            .expect("event metadata schema exists")
        );
        assert!(!validate("WorkItem", &json!([])).expect("work item schema exists"));
        assert!(
            validate(
                "account-registry/AccountRecord",
                &json!({
                    "id":"default",
                    "home":"/tmp/default",
                    "label":"Default",
                    "source":"test",
                    "status":"ready"
                })
            )
            .expect("account state schema exists")
        );
        assert!(
            validate(
                "workspace-usage-limits/UsageLimitsResponse",
                &json!({"accountKey":"default"})
            )
            .expect("usage limits schema exists")
        );
    }

    #[test]
    fn request_registry_includes_shared_state_documents() {
        let document = json!({"components":{"schemas":{}}});
        registry_for_document(&document).expect("shared registry prepares");
    }

    #[test]
    fn unknown_schema_names_are_configuration_errors() {
        assert!(validate("MissingRecordSchema", &json!({})).is_err());
    }
}
