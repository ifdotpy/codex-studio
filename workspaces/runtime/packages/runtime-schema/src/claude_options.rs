//! Typed shape of Claude options stored in account registry records.
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Number, Value};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeOptionsRecord {
    #[serde(default, deserialize_with = "optional_integer")]
    pub auto_compact_window: Option<Number>,
    pub binary_path: Option<String>,
    pub config_dir: Option<String>,
    pub custom_models: Option<Vec<ClaudeCustomModelRecord>>,
    pub launch_args: Option<String>,
    #[serde(flatten)]
    pub extensions: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ClaudeCustomModelRecord {
    pub id: String,
    pub label: String,
    #[serde(flatten)]
    pub extensions: Map<String, Value>,
}

fn optional_integer<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Number>, D::Error> {
    let value = Option::<Number>::deserialize(deserializer)?;
    if value
        .as_ref()
        .is_some_and(|number| number.is_f64() && number.as_f64().is_some_and(|n| n.fract() != 0.0))
    {
        return Err(serde::de::Error::custom("expected an integer"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::ClaudeOptionsRecord;
    use serde_json::json;

    #[test]
    fn registry_schema_constraints_are_owned_by_this_parser() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("schemas/account-registry.schema.json")).unwrap();
        assert_eq!(
            schema["$defs"]["ClaudeOptionsRecord"],
            json!({
                "type":"object", "title":"ClaudeOptionsRecord", "additionalProperties":{"$ref":"#/$defs/JsonValue"},
                "properties": {
                    "autoCompactWindow":{"title":"Autocompactwindow","anyOf":[{"type":"integer","x-safe-integer":"unsigned"},{"type":"null"}]},
                    "binaryPath":{"title":"Binarypath","anyOf":[{"type":"string"},{"type":"null"}]},
                    "configDir":{"title":"Configdir","anyOf":[{"type":"string"},{"type":"null"}]},
                    "customModels":{"title":"Custommodels","anyOf":[{"type":"array","items":{"$ref":"#/$defs/ClaudeCustomModelRecord"}},{"type":"null"}]},
                    "launchArgs":{"title":"Launchargs","anyOf":[{"type":"string"},{"type":"null"}]}
                }
            })
        );
        assert_eq!(
            schema["$defs"]["ClaudeCustomModelRecord"],
            json!({
                "type":"object", "title":"ClaudeCustomModelRecord", "additionalProperties":{"$ref":"#/$defs/JsonValue"},
                "properties":{"id":{"type":"string","title":"Id"},"label":{"type":"string","title":"Label"}},
                "required":["id","label"]
            })
        );
    }

    #[test]
    fn matches_registry_schema_for_options_shapes() {
        let candidates = [
            json!(null),
            json!(true),
            json!(0),
            json!(-1),
            json!(1.0),
            json!(1.5),
            json!(9007199254740992u64),
            json!("text"),
            json!([]),
            json!({}),
            json!([{"id":"x","label":"X"}]),
            json!([{"id":"x"}]),
            json!([{"id":"x","label":"X","extension":{"nested":[null,true]}}]),
            json!([["x", "X"]]),
            json!([null]),
        ];
        let mut values = vec![
            json!({}),
            json!([]),
            json!(null),
            json!("text"),
            json!({"extension":{"nested":[null,true,1,"x"]}}),
        ];
        for field in [
            "autoCompactWindow",
            "binaryPath",
            "configDir",
            "customModels",
            "launchArgs",
        ] {
            for candidate in &candidates {
                values.push(json!({(field): candidate}));
            }
        }
        for field in ["id", "label"] {
            for candidate in &candidates {
                let mut model = json!({"id":"x","label":"X"});
                model[field] = candidate.clone();
                values.push(json!({"customModels":[model]}));
            }
        }
        values.extend([
            json!({"customModels":[{}]}),
            json!({"customModels":[{"label":"x"}]}),
            json!({"customModels":[{"id":"x"}]}),
            json!({"customModels":[{"id":"x","label":""}]}),
        ]);
        for value in values {
            assert_eq!(
                serde_json::from_value::<ClaudeOptionsRecord>(value.clone()).is_ok(),
                crate::validate("account-registry/ClaudeOptionsRecord", &value).unwrap(),
                "{value}"
            );
        }
    }
}
