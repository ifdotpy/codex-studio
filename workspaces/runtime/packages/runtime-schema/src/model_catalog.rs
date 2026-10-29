//! Studio's consumed model catalog projection. Model IDs, efforts, service tier
//! IDs and unknown provider metadata remain upstream-owned, open values.

use crate::ProviderKind;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

// A missing field is absent; a present field must deserialize as its declared
// type. In particular, Option<Option<T>> preserves explicit upstream nulls.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModel {
    pub model: String,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "ProviderKind")]
    pub provider: Option<ProviderKind>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "String")]
    pub account_key: Option<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub resolved_model: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub display_name: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub description: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "bool")]
    pub hidden: Option<bool>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "bool")]
    pub is_default: Option<bool>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub default_reasoning_effort: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Vec<ModelReasoningEffort>")]
    pub supported_reasoning_efforts: Option<Vec<ModelReasoningEffort>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Vec<ModelServiceTier>")]
    pub service_tiers: Option<Vec<ModelServiceTier>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub available_access_programs: Option<Option<ModelAccessPrograms>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ProviderModel {
    pub fn cyber_programs(&self) -> Option<&[String]> {
        self.available_access_programs
            .as_ref()?
            .as_ref()?
            .cyber
            .as_deref()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelReasoningEffort {
    pub reasoning_effort: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModelServiceTier {
    pub id: String,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub description: Option<Option<String>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModelAccessPrograms {
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Vec<String>")]
    pub cyber: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One validated provider page, or the combined catalog after pagination.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalog {
    pub data: Vec<ProviderModel>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub next_cursor: Option<Option<String>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ModelCatalog {
    pub fn parse(value: Value) -> Result<Self, serde_json::Error> {
        let catalog: Self = serde_json::from_value(value)?;
        if catalog.data.iter().any(|row| row.model.trim().is_empty()) {
            return Err(<serde_json::Error as serde::de::Error>::custom(
                "empty catalog model",
            ));
        }
        Ok(catalog)
    }
}

/// A user's explicit model picker choice. Pickers offer every model that is
/// not blacklisted, so a model new to the catalog starts untouched and enabled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ModelChoice {
    Untouched,
    Whitelisted,
    Blacklisted,
}

/// Server preference value keyed by provider-qualified model identity; an
/// absent key is untouched. The schema is the server validator.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(extend(
    "maxProperties" = crate::shared_values::MODEL_CHOICES_MAX_KEYS,
    "propertyNames" = {
        "pattern": crate::shared_values::MODEL_CHOICE_KEY_PATTERN,
        "maxLength": crate::shared_values::MODEL_CHOICE_KEY_MAX_CODEPOINTS,
    },
))]
pub struct ModelChoices(pub std::collections::BTreeMap<String, ModelChoice>);

pub fn schemas() -> Map<String, Value> {
    let mut generator = schemars::generate::SchemaSettings::draft2020_12()
        .for_serialize()
        .with(|settings| settings.definitions_path = "/components/schemas".into())
        .into_generator();
    generator.subschema_for::<ModelCatalog>();
    generator.subschema_for::<ModelChoices>();
    let mut schemas = generator.take_definitions(true);
    // OpenAPI's existing JsonValue owns the recursive opaque JSON leaf.
    for schema in schemas.values_mut() {
        if schema.get("additionalProperties") == Some(&Value::Bool(true)) {
            schema["additionalProperties"] =
                serde_json::json!({"$ref":"#/components/schemas/JsonValue"});
        }
    }
    schemas
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn optional_nullable_fields_and_opaque_metadata_round_trip() {
        for row in [
            json!({"model":"future", "vendor":{"a":null}}),
            json!({"model":"future", "resolvedModel":null, "defaultReasoningEffort":null, "availableAccessPrograms":null}),
            json!({"model":"future", "supportedReasoningEfforts":[{"reasoningEffort":"future-effort","description":7}], "serviceTiers":[{"id":"future-tier"}], "availableAccessPrograms":{"cyber":["future-program"],"future":true}}),
        ] {
            let value = json!({"data":[row],"futureMetadata":42});
            let parsed = ModelCatalog::parse(value.clone()).expect("valid open catalog");
            assert_eq!(serde_json::to_value(parsed).expect("serialize"), value);
        }
        for provider in ProviderKind::ALL {
            let value = json!({"data":[{"model":"future", "provider":provider}]});
            let parsed = ModelCatalog::parse(value.clone()).expect("known Studio provider");
            assert_eq!(parsed.data[0].provider, Some(provider));
            assert_eq!(serde_json::to_value(parsed).expect("serialize"), value);
        }
    }

    #[test]
    fn known_fields_reject_invalid_types_in_serde_and_generated_schema() {
        let mut schemas = schemas();
        schemas.insert("JsonValue".into(), json!(true));
        let schema =
            json!({"components":{"schemas":schemas}, "$ref":"#/components/schemas/ModelCatalog"});
        let validator = jsonschema::validator_for(&schema).expect("schema compiles");
        for (field, invalid) in [
            ("provider", json!("future-provider")),
            ("provider", json!(null)),
            ("provider", json!(3)),
            ("hidden", json!(null)),
            ("hidden", json!("true")),
            ("isDefault", json!(1)),
            ("resolvedModel", json!(false)),
            (
                "supportedReasoningEfforts",
                json!([{"reasoningEffort":true}]),
            ),
            ("serviceTiers", json!([{"id":1}])),
            ("availableAccessPrograms", json!({"cyber":[true]})),
        ] {
            let mut row = json!({"model":"valid"});
            row[field] = invalid;
            let value = json!({"data":[row]});
            assert!(ModelCatalog::parse(value.clone()).is_err(), "{field}");
            assert!(!validator.is_valid(&value), "{field}");
        }
    }
}
