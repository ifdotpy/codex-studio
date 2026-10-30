//! Stored graph-agent identity and opaque graph evidence.
use crate::json_object::JsonObject;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct GraphAgentRecord(GraphAgentFields);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(rename = "PersistedGraphAgentRecord")]
struct GraphAgentFields {
    id: String,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

impl<'de> Deserialize<'de> for GraphAgentRecord {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        JsonObject::<GraphAgentFields>::deserialize(deserializer).map(|value| Self(value.0))
    }
}
impl GraphAgentRecord {
    pub fn id(&self) -> &String {
        &self.0.id
    }
    pub fn into_value(self) -> Value {
        let GraphAgentFields { id, mut extra } = self.0;
        extra.insert("id".into(), Value::String(id));
        Value::Object(extra)
    }
}
pub(crate) fn schema() -> Value {
    schemars::schema_for!(GraphAgentRecord).to_value()
}
pub(crate) fn check_schema_parity() -> Result<(), String> {
    let stored: Value = serde_json::from_slice(include_bytes!(
        "schemas/persisted-graph-agent-record.schema.json"
    ))
    .map_err(|e| e.to_string())?;
    if schema() != stored {
        return Err("persisted graph agent schema must be generated from graph_agent_record::GraphAgentRecord".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn graph_agent_schema_and_owned_projection_match() {
        check_schema_parity().unwrap();
        for input in [
            json!({"id":""}),
            json!({"id":"g","opaque":[1,{"other":null}],"number":1.0}),
            json!({}),
            json!({"id":null}),
            json!({"id":7}),
            json!(["g"]),
        ] {
            let parsed = serde_json::from_value::<GraphAgentRecord>(input.clone());
            assert_eq!(
                parsed.is_ok(),
                crate::validate("PersistedGraphAgentRecord", &input).unwrap(),
                "{input}"
            );
            if let Ok(parsed) = parsed {
                assert_eq!(parsed.into_value(), input);
            }
        }
        let input = json!({"id":"owned-id","opaque":[{"data":"owned"}]});
        let record = serde_json::from_value::<GraphAgentRecord>(input.clone()).unwrap();
        let id = record.id().as_ptr();
        let opaque = record.0.extra["opaque"].as_array().unwrap().as_ptr();
        let result = record.into_value();
        assert_eq!(result, input);
        assert_eq!(result["id"].as_str().unwrap().as_ptr(), id);
        assert_eq!(result["opaque"].as_array().unwrap().as_ptr(), opaque);
    }
}
