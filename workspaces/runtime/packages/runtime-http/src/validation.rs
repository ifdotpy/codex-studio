//! Runtime request validation compiled from the committed OpenAPI authority.
//!
//! This is deliberately a small internal boundary: callers identify an operation and
//! provide raw body/query/header values; they receive validated values or located errors.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use jsonschema::{Draft, Registry as SchemaRegistry, Validator};
use serde_json::{Map, Value, json};

const OPENAPI: &[u8] = include_bytes!("../openapi.json");
const OPENAPI_URI: &str = "https://codex-studio.invalid/openapi.json";
const MAX_SAFE_INTEGER: i128 = 9_007_199_254_740_991;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LocatedError {
    pub location: Vec<Value>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RawRequest {
    pub body: Option<Value>,
    pub query: BTreeMap<String, Vec<String>>,
    pub headers: BTreeMap<String, Vec<String>>,
    pub invalid_headers: BTreeSet<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ValidatedRequest {
    #[allow(dead_code, reason = "ApiJson consumes validated bodies")]
    pub body: Option<Value>,
    #[allow(
        dead_code,
        reason = "shared QueryParams retains the original ordered wire values"
    )]
    pub query: Map<String, Value>,
    #[allow(dead_code, reason = "shared header validation feeds route handlers")]
    pub headers: Map<String, Value>,
}

#[derive(Clone)]
struct Part {
    validator: Validator,
    location: &'static str,
    schema: Value,
    definitions: Arc<Map<String, Value>>,
}

#[derive(Clone, Default)]
struct OperationValidator {
    body: Option<Part>,
    body_required: bool,
    query: Option<Part>,
    headers: Option<Part>,
}

#[derive(Clone, Default)]
pub(crate) struct Registry {
    operations: Arc<BTreeMap<String, OperationValidator>>,
    preference_field: Option<Validator>,
    stored_schemas: Arc<BTreeMap<String, Validator>>,
}

#[derive(Debug)]
pub(crate) struct CompileError {
    pub operation_id: String,
    pub message: String,
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot compile OpenAPI validator for {}: {}",
            self.operation_id, self.message
        )
    }
}

impl std::error::Error for CompileError {}

impl Registry {
    pub(crate) fn compile(registered: &[(String, String)]) -> Result<Self, CompileError> {
        let document: Value = serde_json::from_slice(OPENAPI).map_err(|error| CompileError {
            operation_id: "<document>".to_owned(),
            message: error.to_string(),
        })?;
        let schema_registry = schema_registry(&document).map_err(|message| CompileError {
            operation_id: "<document>".to_owned(),
            message,
        })?;
        let definitions = Arc::new(
            document
                .pointer("/components/schemas")
                .and_then(Value::as_object)
                .cloned()
                .ok_or_else(|| CompileError {
                    operation_id: "<document>".to_owned(),
                    message: "OpenAPI components/schemas is missing".to_owned(),
                })?,
        );
        let preference_field = jsonschema::options()
            .with_registry(&schema_registry)
            .with_pattern_options(jsonschema::PatternOptions::regex())
            .build(&json!({"$ref": format!("{OPENAPI_URI}#/components/schemas/PreferenceField")}))
            .map_err(|error| CompileError {
                operation_id: "push_preferences_api_sync_preferences_post".to_owned(),
                message: error.to_string(),
            })?;
        let mut stored_schemas = BTreeMap::new();
        for name in [
            "AgentRecordResponse",
            "PlanView",
            "QueueItem",
            "TaskDetailResponse",
            "TranscriptAsset",
            "WorkItem",
        ] {
            let validator = jsonschema::options()
                .with_registry(&schema_registry)
                .with_pattern_options(jsonschema::PatternOptions::regex())
                .build(&json!({"$ref": format!("{OPENAPI_URI}#/components/schemas/{name}")}))
                .map_err(|error| CompileError {
                    operation_id: name.to_owned(),
                    message: error.to_string(),
                })?;
            stored_schemas.insert(name.to_owned(), validator);
        }
        let mut compiled = BTreeMap::new();
        for (method, path) in registered {
            let operation = document.pointer(&format!(
                "/paths/{}/{}",
                escape(path),
                method.to_lowercase()
            ));
            let Some(operation) = operation else { continue };
            let operation_id = operation
                .get("operationId")
                .and_then(Value::as_str)
                .unwrap_or(path)
                .to_owned();
            let result = compile_operation(
                &document,
                &schema_registry,
                &definitions,
                method,
                path,
                operation,
            );
            let validator = result.map_err(|message| CompileError {
                operation_id: operation_id.clone(),
                message,
            })?;
            compiled.insert(format!("{method} {path}"), validator);
        }
        Ok(Self {
            operations: Arc::new(compiled),
            preference_field: Some(preference_field),
            stored_schemas: Arc::new(stored_schemas),
        })
    }

    pub(crate) fn validate(
        &self,
        operation_id: &str,
        raw: RawRequest,
    ) -> Result<ValidatedRequest, LocatedError> {
        let Some(operation) = self.operations.get(operation_id) else {
            return Ok(ValidatedRequest::default());
        };
        let query_names = operation
            .query
            .as_ref()
            .and_then(|part| part.schema.get("properties"))
            .and_then(Value::as_object);
        if let Some((name, _)) = raw
            .query
            .iter()
            .find(|(name, _)| !query_names.is_some_and(|names| names.contains_key(*name)))
        {
            return Err(LocatedError {
                location: vec![
                    Value::String("query".to_owned()),
                    Value::String(name.clone()),
                ],
            });
        }
        if let Some((name, _)) = raw.query.iter().find(|(name, values)| {
            values.len() > 1
                && query_names
                    .and_then(|names| names.get(*name))
                    .and_then(|schema| schema.get("type"))
                    .and_then(Value::as_str)
                    != Some("array")
        }) {
            return Err(LocatedError {
                location: vec![
                    Value::String("query".to_owned()),
                    Value::String(name.clone()),
                ],
            });
        }
        let body = if let Some(part) = &operation.body {
            match raw.body {
                Some(mut value) => {
                    validate_part(part, &mut value)?;
                    Some(value)
                }
                None if operation.body_required => {
                    return Err(LocatedError {
                        location: vec![Value::String("body".to_owned())],
                    });
                }
                None => None,
            }
        } else {
            None
        };
        let mut query = Value::Object(parameter_object(raw.query, operation.query.as_ref()));
        if let Some(part) = &operation.query {
            validate_part(part, &mut query)?;
        }
        let headers =
            raw.headers
                .into_iter()
                .fold(BTreeMap::new(), |mut headers, (name, values)| {
                    headers
                        .entry(name.to_ascii_lowercase())
                        .or_insert_with(Vec::new)
                        .extend(values);
                    headers
                });
        if let Some(properties) = operation
            .headers
            .as_ref()
            .and_then(|part| part.schema.get("properties"))
            .and_then(Value::as_object)
            && let Some(name) = raw
                .invalid_headers
                .iter()
                .find(|name| properties.contains_key(*name))
        {
            return Err(LocatedError {
                location: vec![
                    Value::String("header".to_owned()),
                    Value::String(name.to_ascii_lowercase()),
                ],
            });
        }
        if let Some(properties) = operation
            .headers
            .as_ref()
            .and_then(|part| part.schema.get("properties"))
            .and_then(Value::as_object)
            && let Some((name, _)) = properties.iter().find(|(name, _)| {
                headers
                    .get(&name.to_ascii_lowercase())
                    .is_some_and(|values| values.len() > 1)
            })
        {
            return Err(LocatedError {
                location: vec![
                    Value::String("header".to_owned()),
                    Value::String(name.to_ascii_lowercase()),
                ],
            });
        }
        let mut headers = Value::Object(parameter_object(headers, operation.headers.as_ref()));
        if let Some(part) = &operation.headers {
            validate_part(part, &mut headers)?;
        }
        Ok(ValidatedRequest {
            body,
            query: query.as_object().cloned().unwrap_or_default(),
            headers: headers.as_object().cloned().unwrap_or_default(),
        })
    }

    pub(crate) fn validate_route(
        &self,
        method: &str,
        path_template: &str,
        raw: RawRequest,
    ) -> Result<ValidatedRequest, LocatedError> {
        self.validate(&format!("{method} {path_template}"), raw)
    }

    pub(crate) fn template_for(&self, method: &str, path: &str) -> Option<String> {
        crate::schema::matching_template(
            self.operations.keys().filter_map(|key| key.split_once(' ')),
            Some(method),
            path,
        )
        .map(str::to_owned)
    }

    pub(crate) fn has_body_schema(&self, method: &str, path: &str) -> Option<bool> {
        self.template_for(method, path).and_then(|template| {
            self.operations
                .get(&format!("{method} {template}"))
                .map(|operation| operation.body.is_some())
        })
    }

    pub(crate) fn validate_preference_field(&self, value: &Value) -> bool {
        self.preference_field
            .as_ref()
            .is_some_and(|validator| validator.is_valid(value))
    }

    pub(crate) fn validate_stored(&self, schema: &str, value: &Value) -> bool {
        self.stored_schemas
            .get(schema)
            .is_some_and(|validator| validator.is_valid(value))
    }
}

fn compile_operation(
    document: &Value,
    schema_registry: &SchemaRegistry<'_>,
    definitions: &Arc<Map<String, Value>>,
    method: &str,
    path: &str,
    operation: &Value,
) -> Result<OperationValidator, String> {
    let mut output = OperationValidator::default();
    let operation_id = operation
        .get("operationId")
        .and_then(Value::as_str)
        .unwrap_or(path);
    if let Some(schema) = operation.pointer("/requestBody/content/application~1json/schema") {
        check_safe_integer_bounds(
            document,
            schema,
            "/requestBody/content/application~1json/schema",
            operation_id,
        )?;
    }
    if let Some(parameters) = operation.get("parameters").and_then(Value::as_array) {
        for (index, parameter) in parameters.iter().enumerate() {
            if let Some(schema) = parameter.get("schema") {
                check_safe_integer_bounds(
                    document,
                    schema,
                    &format!("/parameters/{index}/schema"),
                    operation_id,
                )?;
            }
        }
    }
    if document
        .pointer(&format!("/paths/{}/parameters", escape(path)))
        .is_some()
    {
        return Err("path-item-level parameters are unsupported".to_owned());
    }
    if let Some(parameters) = operation.get("parameters").and_then(Value::as_array) {
        let mut query_properties = Map::new();
        let mut query_validator_properties = Map::new();
        let mut query_required = Vec::new();
        let mut header_properties = Map::new();
        let mut header_validator_properties = Map::new();
        let mut header_required = Vec::new();
        for (index, parameter) in parameters.iter().enumerate() {
            if parameter.get("$ref").is_some() || parameter.get("content").is_some() {
                return Err("parameter references and content are unsupported".to_owned());
            }
            let (Some(name), Some(location)) = (
                parameter.get("name").and_then(Value::as_str),
                parameter.get("in").and_then(Value::as_str),
            ) else {
                return Err("malformed OpenAPI parameter".to_owned());
            };
            if !matches!(location, "query" | "header") {
                return Err(format!("{location} parameters are unsupported"));
            }
            let expected_style = if location == "query" {
                "form"
            } else {
                "simple"
            };
            if parameter
                .get("style")
                .and_then(Value::as_str)
                .is_some_and(|style| style != expected_style)
                || parameter.get("explode").and_then(Value::as_bool) == Some(location == "header")
            {
                return Err(format!(
                    "unsupported serialization for {location} parameter {name}"
                ));
            }
            let Some(schema) = parameter.get("schema") else {
                return Err(format!("{location} parameter {name} has no schema"));
            };
            if !supported_parameter_schema(
                document,
                schema,
                location == "query",
                &mut BTreeSet::new(),
            ) {
                return Err(format!(
                    "unsupported parameter schema for {location} parameter {name}"
                ));
            }
            match location {
                "query" => {
                    query_properties.insert(name.to_owned(), schema.clone());
                    query_validator_properties.insert(
                        name.to_owned(),
                        json!({"$ref": parameter_schema_uri(method, path, index)}),
                    );
                    if parameter.get("required").and_then(Value::as_bool) == Some(true) {
                        query_required.push(Value::String(name.to_owned()));
                    }
                }
                "header" => {
                    let name = name.to_ascii_lowercase();
                    header_properties.insert(name.to_owned(), schema.clone());
                    header_validator_properties.insert(
                        name.clone(),
                        json!({"$ref": parameter_schema_uri(method, path, index)}),
                    );
                    if parameter.get("required").and_then(Value::as_bool) == Some(true) {
                        header_required.push(Value::String(name));
                    }
                }
                _ => unreachable!("unsupported parameter location rejected above"),
            }
        }
        let query_schema =
            json!({"type":"object","properties":query_properties,"required":query_required});
        let query_validator_schema = json!({"type":"object","properties":query_validator_properties,"required":query_required});
        output.query = Some(compile_part(
            query_schema,
            query_validator_schema,
            "query",
            schema_registry,
            definitions,
        )?);
        if !header_properties.is_empty() {
            let header_schema =
                json!({"type":"object","properties":header_properties,"required":header_required});
            let header_validator_schema = json!({"type":"object","properties":header_validator_properties,"required":header_required});
            output.headers = Some(compile_part(
                header_schema,
                header_validator_schema,
                "header",
                schema_registry,
                definitions,
            )?);
        }
    }
    if let Some(request_body) = operation.get("requestBody") {
        output.body_required = request_body.get("required").and_then(Value::as_bool) == Some(true);
        if let Some(schema) = request_body.pointer("/content/application~1json/schema") {
            output.body = Some(compile_part(
                schema.clone(),
                json!({"$ref": body_schema_uri(method, path)}),
                "body",
                schema_registry,
                definitions,
            )?);
        }
    }
    Ok(output)
}

fn check_safe_integer_bounds(
    document: &Value,
    schema: &Value,
    root_pointer: &str,
    operation_id: &str,
) -> Result<(), String> {
    let mut pending = vec![(root_pointer.to_owned(), schema.clone())];
    let mut visited_references = BTreeSet::new();
    while let Some((pointer, schema)) = pending.pop() {
        let Some(object) = schema.as_object() else {
            return Err(format!(
                "{operation_id} {pointer}: schema must be an object"
            ));
        };
        for keyword in object.keys() {
            if !SAFE_INTEGER_AUDIT_KEYWORDS.contains(&keyword.as_str())
                && !keyword.starts_with("x-")
            {
                return Err(format!(
                    "{operation_id} {pointer}: unsupported schema keyword {keyword}"
                ));
            }
        }
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str)
            && reference != "#/components/schemas/JsonValue"
            && visited_references.insert(reference.to_owned())
        {
            let target = document
                .pointer(reference.strip_prefix('#').unwrap_or_default())
                .ok_or_else(|| {
                    format!(
                        "{operation_id} {pointer}: unresolved local schema reference {reference}"
                    )
                })?;
            pending.push((reference.to_owned(), target.clone()));
        }
        let integer_type = schema.get("type").is_some_and(|kind| match kind {
            Value::String(kind) => kind == "integer",
            Value::Array(kinds) => kinds.iter().any(|kind| kind.as_str() == Some("integer")),
            _ => false,
        });
        if integer_type {
            let bound = |name: &str| {
                schema
                    .get(name)
                    .and_then(Value::as_number)
                    .and_then(|number| number.as_f64())
                    .filter(|value| {
                        value.is_finite()
                            && value.fract() == 0.0
                            && (-(MAX_SAFE_INTEGER as f64)..=(MAX_SAFE_INTEGER as f64))
                                .contains(value)
                    })
            };
            let minimum = bound("minimum");
            let maximum = bound("maximum");
            if minimum.is_none() || maximum.is_none() {
                return Err(format!(
                    "{operation_id} {pointer}: integer schema requires minimum and maximum within ±{MAX_SAFE_INTEGER}"
                ));
            }
        }
        for keyword in [
            "properties",
            "patternProperties",
            "dependentSchemas",
            "$defs",
            "definitions",
        ] {
            if let Some(children) = schema.get(keyword) {
                let Some(children) = children.as_object() else {
                    return Err(format!(
                        "{operation_id} {pointer}/{keyword}: expected an object"
                    ));
                };
                pending.extend(children.iter().map(|(name, value)| {
                    (
                        format!("{pointer}/{keyword}/{}", escape(name)),
                        value.clone(),
                    )
                }));
            }
        }
        for keyword in [
            "items",
            "additionalProperties",
            "unevaluatedProperties",
            "unevaluatedItems",
            "contains",
            "not",
            "if",
            "then",
            "else",
            "propertyNames",
            "contentSchema",
        ] {
            if let Some(value) = schema.get(keyword) {
                if !value.is_object() && !value.is_boolean() {
                    return Err(format!(
                        "{operation_id} {pointer}/{keyword}: expected a schema"
                    ));
                }
                if value.is_object() {
                    pending.push((format!("{pointer}/{keyword}"), value.clone()));
                }
            }
        }
        for keyword in ["allOf", "anyOf", "oneOf", "prefixItems"] {
            if let Some(values) = schema.get(keyword) {
                let Some(values) = values.as_array() else {
                    return Err(format!(
                        "{operation_id} {pointer}/{keyword}: expected an array"
                    ));
                };
                pending.extend(
                    values.iter().enumerate().map(|(index, value)| {
                        (format!("{pointer}/{keyword}/{index}"), value.clone())
                    }),
                );
            }
        }
    }
    Ok(())
}

const SAFE_INTEGER_AUDIT_KEYWORDS: &[&str] = &[
    "$ref",
    "$schema",
    "$id",
    "$anchor",
    "$dynamicRef",
    "$dynamicAnchor",
    "$defs",
    "definitions",
    "$comment",
    "title",
    "description",
    "default",
    "deprecated",
    "readOnly",
    "writeOnly",
    "examples",
    "type",
    "enum",
    "const",
    "multipleOf",
    "maximum",
    "exclusiveMaximum",
    "minimum",
    "exclusiveMinimum",
    "maxLength",
    "minLength",
    "pattern",
    "format",
    "contentEncoding",
    "contentMediaType",
    "contentSchema",
    "maxItems",
    "minItems",
    "uniqueItems",
    "maxContains",
    "minContains",
    "contains",
    "maxProperties",
    "minProperties",
    "required",
    "dependentRequired",
    "dependentSchemas",
    "properties",
    "patternProperties",
    "additionalProperties",
    "propertyNames",
    "unevaluatedItems",
    "unevaluatedProperties",
    "items",
    "prefixItems",
    "allOf",
    "anyOf",
    "oneOf",
    "not",
    "if",
    "then",
    "else",
    "discriminator",
    "xml",
    "externalDocs",
    "example",
];

fn supported_parameter_schema(
    document: &Value,
    schema: &Value,
    allow_array: bool,
    seen_references: &mut BTreeSet<String>,
) -> bool {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let Some(name) = reference.strip_prefix("#/components/schemas/") else {
            return false;
        };
        if name.is_empty() || name.contains(['/', '~']) {
            return false;
        }
        let Some(target) = document
            .pointer("/components/schemas")
            .and_then(Value::as_object)
            .and_then(|schemas| schemas.get(name))
        else {
            return false;
        };
        if allow_array && target.get("type").and_then(Value::as_str) == Some("array") {
            return false;
        }
        return seen_references.insert(reference.to_owned())
            && supported_parameter_schema(document, target, allow_array, seen_references);
    }
    if schema.get("allOf").is_some() || schema.get("type").is_some_and(Value::is_array) {
        return false;
    }
    if let Some(kind) = schema.get("type").and_then(Value::as_str) {
        if schema.get("anyOf").is_some() || schema.get("oneOf").is_some() {
            return false;
        }
        return match kind {
            "string" | "integer" | "number" | "boolean" => true,
            "array" if allow_array => schema.get("items").is_some_and(|items| {
                supported_parameter_schema(document, items, false, seen_references)
            }),
            _ => false,
        };
    }
    if schema.get("anyOf").is_some() && schema.get("oneOf").is_some() {
        return false;
    }
    let Some(branches) = schema
        .get("anyOf")
        .or_else(|| schema.get("oneOf"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    if branches.is_empty() {
        return false;
    }
    let mut scalar_kind = None;
    let mut nullable = false;
    for branch in branches {
        let resolved = branch
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|reference| document.pointer(reference.trim_start_matches('#')))
            .unwrap_or(branch);
        match resolved.get("type").and_then(Value::as_str) {
            Some("null") if !nullable => nullable = true,
            Some(kind @ ("string" | "integer" | "number" | "boolean")) => {
                if scalar_kind.is_some_and(|previous| previous != kind) {
                    return false;
                }
                scalar_kind = Some(kind);
            }
            _ => return false,
        }
    }
    scalar_kind.is_some()
}

fn compile_part(
    schema: Value,
    validator_schema: Value,
    location: &'static str,
    schema_registry: &SchemaRegistry<'_>,
    definitions: &Arc<Map<String, Value>>,
) -> Result<Part, String> {
    let validator = jsonschema::options()
        .with_registry(schema_registry)
        .with_pattern_options(jsonschema::PatternOptions::regex())
        .build(&validator_schema)
        .map_err(|error| error.to_string())?;
    Ok(Part {
        validator,
        location,
        schema,
        definitions: Arc::clone(definitions),
    })
}

fn parameter_schema_uri(method: &str, path: &str, index: usize) -> String {
    format!(
        "{OPENAPI_URI}#/paths/{}/{}/parameters/{index}/schema",
        escape(path),
        method.to_lowercase()
    )
}

fn body_schema_uri(method: &str, path: &str) -> String {
    format!(
        "{OPENAPI_URI}#/paths/{}/{}/requestBody/content/application~1json/schema",
        escape(path),
        method.to_lowercase()
    )
}

fn schema_registry(document: &Value) -> Result<SchemaRegistry<'_>, String> {
    SchemaRegistry::new()
        .draft(Draft::Draft202012)
        .add(
            OPENAPI_URI,
            jsonschema::Resource::from_contents(document.clone()),
        )
        .map_err(|error| error.to_string())?
        .prepare()
        .map_err(|error| error.to_string())
}

fn validate_part(part: &Part, value: &mut Value) -> Result<(), LocatedError> {
    if matches!(part.location, "query" | "header") {
        convert_parameters(&part.schema, &part.definitions, value);
    }
    match part.validator.iter_errors(value).next() {
        Some(error) => Err(convert_error(&error, part.location)),
        None => Ok(()),
    }
}

fn convert_error(error: &jsonschema::ValidationError<'_>, section: &str) -> LocatedError {
    let mut location = vec![Value::String(section.to_owned())];
    location.extend(error.instance_path().iter().map(|item| match item {
        jsonschema::paths::LocationSegment::Property(name) => Value::String(name.to_string()),
        jsonschema::paths::LocationSegment::Index(index) => Value::String(index.to_string()),
    }));
    match error.kind() {
        jsonschema::error::ValidationErrorKind::Required { property } => {
            location.push(property.clone());
        }
        jsonschema::error::ValidationErrorKind::AdditionalProperties { unexpected } => {
            if let Some(name) = unexpected.first() {
                location.push(Value::String(name.clone()));
            }
        }
        _ => {}
    }
    LocatedError { location }
}

pub(crate) fn convert_parameters(
    schema: &Value,
    definitions: &Map<String, Value>,
    value: &mut Value,
) {
    let (Some(properties), Some(values)) = (
        schema.get("properties").and_then(Value::as_object),
        value.as_object_mut(),
    ) else {
        return;
    };
    for (name, parameter_schema) in properties {
        if let Some(value) = values.get_mut(name) {
            convert_parameter(parameter_schema, definitions, value);
        }
    }
}

fn convert_parameter(schema: &Value, definitions: &Map<String, Value>, value: &mut Value) {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str)
        && let Some(name) = reference.strip_prefix("#/components/schemas/")
        && let Some(resolved) = definitions.get(name)
    {
        convert_parameter(resolved, definitions, value);
        return;
    }
    if schema.get("type").and_then(Value::as_str) == Some("array") {
        if let (Some(items), Some(values)) = (schema.get("items"), value.as_array_mut()) {
            for item in values {
                convert_parameter(items, definitions, item);
            }
        }
        return;
    }
    for union_name in ["anyOf", "oneOf"] {
        if let Some(branches) = schema.get(union_name).and_then(Value::as_array) {
            for branch in branches {
                let mut converted = value.clone();
                if convert_parameter_branch(branch, definitions, &mut converted) {
                    *value = converted;
                    return;
                }
            }
            return;
        }
    }
    let _ = convert_parameter_branch(schema, definitions, value);
}

fn convert_parameter_branch(
    schema: &Value,
    definitions: &Map<String, Value>,
    value: &mut Value,
) -> bool {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str)
        && let Some(name) = reference.strip_prefix("#/components/schemas/")
        && let Some(resolved) = definitions.get(name)
    {
        return convert_parameter_branch(resolved, definitions, value);
    }
    let Some(text) = value.as_str().map(str::to_owned) else {
        return false;
    };
    match schema.get("type").and_then(Value::as_str) {
        Some("integer") => {
            let digits = text.strip_prefix('-').unwrap_or(&text);
            if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return false;
            }
            let number = text
                .parse::<i64>()
                .map(serde_json::Number::from)
                .or_else(|_| text.parse::<u64>().map(serde_json::Number::from));
            number.map(|number| *value = Value::Number(number)).is_ok()
        }
        Some("number") => text
            .parse::<serde_json::Number>()
            .map(|number| *value = Value::Number(number))
            .is_ok(),
        Some("boolean") => match text.as_bytes() {
            b"true" => {
                *value = Value::Bool(true);
                true
            }
            b"false" => {
                *value = Value::Bool(false);
                true
            }
            _ => false,
        },
        _ => false,
    }
}

fn parameter_object(
    pairs: BTreeMap<String, Vec<String>>,
    part: Option<&Part>,
) -> Map<String, Value> {
    let Some(part) = part else {
        return Map::new();
    };
    let Some(properties) = part.schema.get("properties").and_then(Value::as_object) else {
        return Map::new();
    };
    pairs
        .into_iter()
        .filter_map(|(name, values)| {
            let declared_name = properties.contains_key(&name).then_some(name.as_str())?;
            let is_array = properties
                .get(declared_name)
                .and_then(|schema| schema.get("type"))
                .and_then(Value::as_str)
                == Some("array");
            let value = if is_array {
                Value::Array(values.into_iter().map(Value::String).collect())
            } else {
                Value::String(values.into_iter().next()?)
            };
            Some((declared_name.to_owned(), value))
        })
        .collect()
}

fn escape(pointer: &str) -> String {
    pointer.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;

    use super::{
        OperationValidator, RawRequest, Registry, check_safe_integer_bounds, compile_part,
        schema_registry,
    };
    use axum::http::{HeaderMap, HeaderValue};
    use serde_json::{Map, Value, json};

    fn compile_for_test(
        document: &Value,
        path: &str,
        operation: &Value,
    ) -> Result<OperationValidator, String> {
        let mut document = document.clone();
        document["paths"][path]["get"] = operation.clone();
        let registry = schema_registry(&document)?;
        let definitions = Arc::new(
            document
                .pointer("/components/schemas")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
        );
        super::compile_operation(
            &document,
            &registry,
            &definitions,
            "get",
            path,
            &document["paths"][path]["get"],
        )
    }

    #[test]
    fn invalid_declared_header_bytes_are_rejected_from_real_header_map() {
        let document = json!({"components":{"schemas":{}},"paths":{"/x":{}}});
        let operation = json!({"parameters":[{
            "name":"X-Optional","in":"header","required":false,"schema":{"type":"string"}
        }]});
        let compiled = compile_for_test(&document, "/x", &operation).expect("header compiles");
        let registry = Registry {
            operations: Arc::new(BTreeMap::from([("GET /x".to_owned(), compiled)])),
            preference_field: None,
            stored_schemas: Arc::new(BTreeMap::new()),
        };
        for alongside_valid in [false, true] {
            let mut actual_headers = HeaderMap::new();
            if alongside_valid {
                actual_headers.append("X-Optional", HeaderValue::from_static("valid"));
            }
            actual_headers.append(
                "X-Optional",
                HeaderValue::from_bytes(&[0xff]).expect("HeaderValue retains non-ASCII byte"),
            );
            let (headers, invalid_headers) =
                crate::middleware::validated_header_values(&actual_headers);
            let error = registry
                .validate(
                    "GET /x",
                    RawRequest {
                        headers,
                        invalid_headers,
                        ..RawRequest::default()
                    },
                )
                .expect_err("invalid declared header bytes are not treated as absent");
            assert_eq!(error.location, vec![json!("header"), json!("x-optional")]);
        }
    }

    fn resident_kib() -> Option<usize> {
        #[cfg(target_os = "linux")]
        {
            let status = std::fs::read_to_string("/proc/self/status").ok()?;
            status.lines().find_map(|line| {
                line.strip_prefix("VmRSS:")
                    .and_then(|value| value.split_whitespace().next())
                    .and_then(|value| value.parse().ok())
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    #[test]
    fn validators_compile_only_for_the_requested_operation_and_enforce_constraints() {
        let current =
            super::super::routes::manifest::operation_manifest().expect("registered routes");
        let rss_before = resident_kib();
        let started = std::time::Instant::now();
        let current_registry = Registry::compile(
            &current
                .iter()
                .map(|operation| operation.key())
                .collect::<Vec<_>>(),
        )
        .expect("registered validators compile");
        eprintln!(
            "registered_validator_compile_ms={} operations={} rss_before_kib={:?} rss_after_kib={:?}",
            started.elapsed().as_millis(),
            current.len(),
            rss_before,
            resident_kib(),
        );
        drop(current_registry);
        let registry = Registry::compile(&[
            ("POST".to_owned(), "/api/projects".to_owned()),
            ("POST".to_owned(), "/api/agents".to_owned()),
        ])
        .expect("documented validators compile with local references");
        let project = registry
            .validate_route(
                "POST",
                "/api/projects",
                RawRequest {
                    body: Some(json!({"path":""})),
                    ..RawRequest::default()
                },
            )
            .expect_err("minimum string length is enforced");
        assert_eq!(project.location.first(), Some(&json!("body")));
        let agent = registry
            .validate_route(
                "POST",
                "/api/agents",
                RawRequest {
                    body: Some(json!({"prompt":"x", "concurrency":513})),
                    ..RawRequest::default()
                },
            )
            .expect_err("maximum concurrency is enforced");
        assert_eq!(agent.location.first(), Some(&json!("body")));
    }

    #[test]
    fn integer_schema_uses_json_schema_integral_decimal_semantics() {
        let registry = Registry::compile(&[("POST".to_owned(), "/api/agents".to_owned())])
            .expect("documented integer schema compiles");
        let validated = registry
            .validate_route(
                "POST",
                "/api/agents",
                RawRequest {
                    body: Some(json!({"prompt":"x", "concurrency":1.0})),
                    ..RawRequest::default()
                },
            )
            .expect("JSON Schema treats an integral decimal as integer");
        assert_eq!(
            validated.body.expect("body exists")["concurrency"],
            json!(1.0)
        );
    }

    #[test]
    fn parsed_numbers_follow_json_schema_and_object_keys_remain_data() {
        let registry = Registry::compile(&[
            ("POST".to_owned(), "/api/agents".to_owned()),
            ("POST".to_owned(), "/api/answer".to_owned()),
        ])
        .expect("request schemas compile");

        let magic_key: Value = crate::json_body::parse_bounded(
            br#"{"prompt":"x","concurrency":{"$serde_json::private::Number":"1"}}"#,
            1024,
        )
        .expect("magic key is valid JSON");
        assert!(
            registry
                .validate_route(
                    "POST",
                    "/api/agents",
                    RawRequest {
                        body: Some(magic_key),
                        ..RawRequest::default()
                    },
                )
                .is_err(),
            "an object cannot masquerade as an integer"
        );

        let object: Value = crate::json_body::parse_bounded(
            br#"{"id":"x","answers":{"$serde_json::private::Number":"1"}}"#,
            1024,
        )
        .expect("magic key object parses");
        assert!(
            registry
                .validate_route(
                    "POST",
                    "/api/answer",
                    RawRequest {
                        body: Some(object.clone()),
                        ..RawRequest::default()
                    },
                )
                .is_ok(),
            "the same key remains ordinary object data where objects are allowed"
        );
        assert_eq!(object["answers"]["$serde_json::private::Number"], "1");

        let almost_integer: Value =
            serde_json::from_slice(br#"{"prompt":"x","concurrency":1.00000000000000000001}"#)
                .expect("valid JSON number");
        assert!(
            registry
                .validate_route(
                    "POST",
                    "/api/agents",
                    RawRequest {
                        body: Some(almost_integer),
                        ..RawRequest::default()
                    },
                )
                .is_ok(),
            "the parsed value follows JSON Schema integer semantics"
        );
    }

    #[test]
    fn query_scalars_are_converted_from_the_registered_parameter_schema() {
        let registry = Registry::compile(&[("GET".to_owned(), "/api/sync/pull".to_owned())])
            .expect("documented query schema compiles");
        let validated = registry
            .validate_route(
                "GET",
                "/api/sync/pull",
                RawRequest {
                    query: BTreeMap::from([
                        ("scope".to_owned(), vec!["state:entities:v1".to_owned()]),
                        ("after".to_owned(), vec!["1".to_owned()]),
                        ("limit".to_owned(), vec!["500".to_owned()]),
                    ]),
                    ..RawRequest::default()
                },
            )
            .expect("query matches its schema");
        assert_eq!(validated.query["after"], json!(1));
        assert_eq!(validated.query["limit"], json!(500));
        assert_eq!(validated.query["scope"], json!("state:entities:v1"));
    }

    #[test]
    fn scalar_header_parameters_are_case_insensitive_single_schema_converted_values() {
        let document = json!({"components":{"schemas":{}}});
        let registry = schema_registry(&document).expect("embedded test document registers");
        let definitions = Arc::new(Map::new());
        let header_schema = json!({"type":"object","properties":{"last-event-id":{"type":"integer","minimum":0,"maximum":9007199254740991_i64}},"required":[]});
        let header = compile_part(
            header_schema.clone(),
            header_schema,
            "header",
            &registry,
            &definitions,
        )
        .expect("scalar header schema compiles");
        let registry = Registry {
            operations: Arc::new(BTreeMap::from([(
                "GET /api/sync/stream".to_owned(),
                OperationValidator {
                    headers: Some(header),
                    ..OperationValidator::default()
                },
            )])),
            preference_field: None,
            stored_schemas: Arc::new(BTreeMap::new()),
        };
        let valid = registry
            .validate_route(
                "GET",
                "/api/sync/stream",
                RawRequest {
                    headers: BTreeMap::from([("LAST-EVENT-ID".to_owned(), vec!["1".to_owned()])]),
                    ..RawRequest::default()
                },
            )
            .expect("header keys are matched case-insensitively and converted by schema");
        assert_eq!(valid.headers["last-event-id"], json!(1));
        let invalid_ascii = registry
            .validate_route(
                "GET",
                "/api/sync/stream",
                RawRequest {
                    invalid_headers: BTreeSet::from(["last-event-id".to_owned()]),
                    ..RawRequest::default()
                },
            )
            .expect_err("a declared header with invalid visible ASCII is not absent");
        assert_eq!(
            invalid_ascii.location,
            vec![json!("header"), json!("last-event-id")]
        );
        for value in ["0", "1", "9007199254740991", "01"] {
            let validated = registry
                .validate_route(
                    "GET",
                    "/api/sync/stream",
                    RawRequest {
                        headers: BTreeMap::from([(
                            "Last-Event-ID".to_owned(),
                            vec![value.to_owned()],
                        )]),
                        ..RawRequest::default()
                    },
                )
                .unwrap_or_else(|error| panic!("valid decimal header {value:?}: {error:?}"));
            assert_eq!(
                validated.headers["last-event-id"],
                json!(
                    value
                        .parse::<u64>()
                        .expect("listed value is a valid integer")
                )
            );
        }
        for value in ["9007199254740992", "+1", " 1", "1.0", ""] {
            assert!(
                registry
                    .validate_route(
                        "GET",
                        "/api/sync/stream",
                        RawRequest {
                            headers: BTreeMap::from([(
                                "Last-Event-ID".to_owned(),
                                vec![value.to_owned()],
                            )]),
                            ..RawRequest::default()
                        },
                    )
                    .is_err(),
                "invalid decimal header {value:?} is rejected"
            );
        }
        let repeated = registry.validate_route(
            "GET",
            "/api/sync/stream",
            RawRequest {
                headers: BTreeMap::from([(
                    "last-event-id".to_owned(),
                    vec!["1".to_owned(), "1".to_owned()],
                )]),
                ..RawRequest::default()
            },
        );
        assert!(repeated.is_err(), "scalar headers reject repeated values");
    }

    #[test]
    fn header_scalar_contract_compiles_and_unimplemented_parameter_forms_fail_startup() {
        let document = json!({
            "components":{"schemas":{}},
            "paths":{"/stream":{}}
        });
        let operation = json!({
            "parameters":[
                {"name":"X-Codex-Sync-Protocol","in":"header","required":true,"schema":{"type":"integer","minimum":0,"maximum":9007199254740991_i64,"enum":[3]}},
                {"name":"protocol","in":"query","required":true,"schema":{"type":"string","enum":["3"]}}
            ]
        });
        let compiled = compile_for_test(&document, "/stream", &operation)
            .expect("scalar header and query values compile");
        assert!(compiled.headers.is_some());
        assert!(compiled.query.is_some());

        let path_item_parameter = json!({
            "components":{"schemas":{}},
            "paths":{"/stream":{"parameters":[{"name":"x","in":"query","schema":{"type":"string"}}]}}
        });
        assert!(compile_for_test(&path_item_parameter, "/stream", &operation).is_err());

        let cookie =
            json!({"parameters":[{"name":"cookie","in":"cookie","schema":{"type":"string"}}]});
        assert!(compile_for_test(&json!({"components":{"schemas":{}}}), "/x", &cookie).is_err());
        let array_header = json!({"parameters":[{"name":"x","in":"header","schema":{"type":"array","items":{"type":"string"}}}]});
        assert!(
            compile_for_test(&json!({"components":{"schemas":{}}}), "/x", &array_header).is_err()
        );
        let impossible = json!({
            "parameters":[{"name":"Last-Event-ID","in":"header","schema":{
                "type":"integer","minimum":0,"maximum":9007199254740991_i64,
                "anyOf":[{"type":"null"},{"type":"string"}]
            }}]
        });
        assert!(
            compile_for_test(&json!({"components":{"schemas":{}}}), "/x", &impossible)
                .err()
                .expect("unsupported parameter forms fail closed")
                .contains("unsupported parameter schema for header parameter Last-Event-ID")
        );
        assert!(compile_for_test(&path_item_parameter, "/stream", &json!({})).is_err());
    }

    #[test]
    fn query_parameter_subset_is_scalar_or_array_of_scalars_only() {
        let document = json!({"components":{"schemas":{}},"paths":{"/x":{}}});
        let accepted = [
            json!({"type":"string"}),
            json!({"anyOf":[{"type":"integer","minimum":-9007199254740991_i64,"maximum":9007199254740991_i64},{"type":"null"}]}),
            json!({"type":"array","items":{"type":"string"}}),
        ];
        for schema in accepted {
            let operation = json!({"parameters":[{"name":"p","in":"query","schema":schema}]});
            assert!(
                compile_for_test(&document, "/x", &operation).is_ok(),
                "{schema}"
            );
        }
        let rejected = [
            json!({"type":"object","properties":{"x":{"type":"string"}}}),
            json!({"type":"array","items":{"type":"array","items":{"type":"string"}}}),
            json!({"allOf":[{"type":"string"}]}),
            json!({"anyOf":[{"type":"string"},{"type":"integer"}]}),
            json!({"type":"string","anyOf":[{"type":"string"},{"type":"integer"}]}),
            json!({"type":["string","null"]}),
        ];
        for schema in rejected {
            let operation = json!({"parameters":[{"name":"p","in":"query","schema":schema}]});
            assert!(
                compile_for_test(&document, "/x", &operation).is_err(),
                "{schema}"
            );
        }
        let array_reference = json!({
            "components":{"schemas":{"StringList":{"type":"array","items":{"type":"string"}}}},
            "paths":{"/x":{}}
        });
        let operation = json!({"parameters":[{
            "name":"p","in":"query","schema":{"$ref":"#/components/schemas/StringList"}
        }]});
        assert!(compile_for_test(&array_reference, "/x", &operation).is_err());

        let nested_component_reference = json!({
            "components":{"schemas":{"Container":{"type":"object","properties":{"value":{"type":"string"}}}}},
            "paths":{"/x":{}}
        });
        let operation = json!({"parameters":[{"name":"p","in":"query","schema":{
            "$ref":"#/components/schemas/Container/properties/value"
        }}]});
        assert!(
            compile_for_test(&nested_component_reference, "/x", &operation)
                .err()
                .expect("component subpointers are outside the converter resolver")
                .contains("unsupported parameter schema for query parameter p")
        );
    }

    #[test]
    fn query_arrays_follow_the_declared_schema_and_undeclared_names_are_rejected_globally() {
        // The committed document declares no array parameter, so the array form uses a fixture.
        let document = json!({"components":{"schemas":{}},"paths":{"/x":{}}});
        let operation = json!({"parameters":[{
            "name":"history","in":"query","schema":{"type":"array","items":{"type":"string"}}
        }]});
        let compiled = compile_for_test(&document, "/x", &operation).expect("array query compiles");
        let arrays = Registry {
            operations: Arc::new(BTreeMap::from([("GET /x".to_owned(), compiled)])),
            preference_field: None,
            stored_schemas: Arc::new(BTreeMap::new()),
        };
        let validated = arrays
            .validate(
                "GET /x",
                RawRequest {
                    query: BTreeMap::from([(
                        "history".to_owned(),
                        vec!["before".to_owned(), "after".to_owned()],
                    )]),
                    ..RawRequest::default()
                },
            )
            .expect("array query values validate");
        assert_eq!(validated.query["history"], json!(["before", "after"]));
        let registry = Registry::compile(&[("GET".to_owned(), "/api/session".to_owned())])
            .expect("query schemas compile");
        let unspecified = registry.validate_route(
            "GET",
            "/api/session",
            RawRequest {
                query: BTreeMap::from([("unexpected".to_owned(), vec!["1".to_owned()])]),
                ..RawRequest::default()
            },
        );
        assert_eq!(
            unspecified
                .expect_err("undeclared query names are rejected globally")
                .location,
            vec![json!("query"), json!("unexpected")]
        );
    }

    #[test]
    fn repeated_scalar_query_parameters_are_refused_even_when_empty() {
        let registry = Registry::compile(&[("GET".to_owned(), "/api/agent-chat".to_owned())])
            .expect("query schema compiles");
        for values in [
            vec!["01".to_owned(), "2".to_owned()],
            vec![String::new(), String::new()],
            vec![String::new(), "1".to_owned()],
        ] {
            let error = registry
                .validate_route(
                    "GET",
                    "/api/agent-chat",
                    RawRequest {
                        query: BTreeMap::from([("before".to_owned(), values)]),
                        ..RawRequest::default()
                    },
                )
                .expect_err("a declared scalar must occur at most once");
            assert_eq!(error.location, vec![json!("query"), json!("before")]);
        }
    }

    #[test]
    fn oversized_integer_query_parameter_returns_a_located_validation_error() {
        let registry = Registry::compile(&[("GET".to_owned(), "/api/agent-chat".to_owned())])
            .expect("query schema compiles");
        let huge = "9".repeat(401);
        let error = registry
            .validate_route(
                "GET",
                "/api/agent-chat",
                RawRequest {
                    query: BTreeMap::from([("before".to_owned(), vec![huge])]),
                    ..RawRequest::default()
                },
            )
            .expect_err("an integer outside the serde_json exact range is rejected");
        assert_eq!(error.location, vec![json!("query"), json!("before")]);
    }

    #[test]
    fn full_authority_validator_cost_estimate() {
        let operations = crate::schema::operations().expect("OpenAPI operations");
        let memory_before = resident_kib();
        let started = std::time::Instant::now();
        Registry::compile(&operations.iter().cloned().collect::<Vec<_>>())
            .expect("all authority validators compile");
        eprintln!(
            "all_authority_validator_compile_ms={} operations={} rss_before_kib={:?} rss_after_kib={:?}",
            started.elapsed().as_millis(),
            operations.len(),
            memory_before,
            resident_kib(),
        );
    }

    #[test]
    fn integer_bound_audit_reports_operation_and_pointer_and_skips_opaque_json() {
        let document = json!({
            "components": {"schemas": {"JsonValue": {"anyOf": [
                {"type":"null"}, {"type":"integer"}, {"type":"object"}
            ]}}}
        });
        let unsafe_schema = json!({"type":"object","properties":{"count":{"type":"integer"}}});
        let error = check_safe_integer_bounds(
            &document,
            &unsafe_schema,
            "/requestBody/schema",
            "create_widget",
        )
        .expect_err("unbounded typed integer is a schema gap");
        assert!(error.contains("create_widget"));
        assert!(error.contains("/requestBody/schema/properties/count"));

        let safe_schema = json!({"type":"integer","minimum":-9007199254740991_i64,"maximum":9007199254740991_i64});
        assert!(check_safe_integer_bounds(&document, &safe_schema, "/query/limit", "find").is_ok());
        let integral_float_spelling = json!({"type":"integer","minimum":1.0,"maximum":3.0});
        assert!(
            check_safe_integer_bounds(&document, &integral_float_spelling, "/query/depth", "find")
                .is_ok()
        );
        let opaque = json!({"$ref":"#/components/schemas/JsonValue"});
        assert!(check_safe_integer_bounds(&document, &opaque, "/body/payload", "send").is_ok());
    }

    #[test]
    fn integer_bound_audit_walks_supported_schema_forms_and_fails_closed() {
        let document = json!({"components":{"schemas":{"Count":{
            "type":"integer","minimum":-9007199254740991_i64,"maximum":9007199254740991_i64
        }}}});
        let nested_integer = json!({"type":"integer"});
        let schemas_with_hidden_integer = [
            json!({"$ref":"#/components/schemas/Count"}),
            json!({"$ref":"#/components/schemas/Count","type":"integer"}),
            json!({"type":["integer","null"]}),
            json!({"anyOf":[{"type":"string"},{"type":"integer"}]}),
            json!({"oneOf":[{"type":"string"},{"type":"integer"}]}),
            json!({"allOf":[{"type":"object","properties":{"x":nested_integer.clone()}}]}),
            json!({"type":"object","patternProperties":{".*":nested_integer.clone()}}),
            json!({"if":{"type":"string"},"then":{"properties":{"x":nested_integer.clone()}}}),
            json!({"if":{"type":"string"},"else":{"properties":{"x":nested_integer.clone()}}}),
            json!({"dependentSchemas":{"payload":{"properties":{"x":nested_integer.clone()}}}}),
            json!({"prefixItems":[nested_integer.clone()]}),
        ];
        for schema in schemas_with_hidden_integer.iter().skip(1) {
            assert!(
                check_safe_integer_bounds(&document, schema, "/body", "fixture_operation").is_err(),
                "walker must visit integer branch: {schema}"
            );
        }
        assert!(
            check_safe_integer_bounds(
                &document,
                &schemas_with_hidden_integer[0],
                "/body",
                "fixture_operation"
            )
            .is_ok()
        );
        let unsupported = json!({"type":"object","unevaluatedSomething":{"type":"integer"}});
        let error =
            check_safe_integer_bounds(&document, &unsupported, "/body", "fixture_operation")
                .expect_err("unknown constructs fail closed");
        assert!(
            error.contains(
                "fixture_operation /body: unsupported schema keyword unevaluatedSomething"
            )
        );
    }
}
