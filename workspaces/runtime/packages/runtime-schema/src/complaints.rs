//! Status choices for a user's complaint response, distinct from stored lifecycle states.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComplaintResponseStatus {
    InProgress,
    Resolved,
    Declined,
}

pub(crate) fn schema() -> Value {
    ComplaintResponseStatus::json_schema(
        &mut schemars::generate::SchemaSettings::openapi3().into_generator(),
    )
    .to_value()
}

pub(crate) fn status_property() -> Value {
    json!({
        "anyOf": [{"$ref":"#/components/schemas/ComplaintResponseStatus"}, {"type":"null"}],
        "default": null,
        "title": "Status"
    })
}

pub(crate) fn check_schema_parity(openapi: &Value) -> Result<(), String> {
    for (pointer, expected) in [
        ("/components/schemas/ComplaintResponseStatus", schema()),
        (
            "/components/schemas/ComplaintBody/properties/status",
            status_property(),
        ),
    ] {
        if openapi.pointer(pointer) != Some(&expected) {
            return Err(format!(
                "{pointer} must match runtime-schema/src/complaints.rs"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complaint_response_status_is_closed_and_matches_openapi() {
        let openapi = serde_json::from_str(include_str!("../../runtime-http/openapi.json"))
            .expect("OpenAPI parses");
        check_schema_parity(&openapi).expect("complaint status owner parity");
        for status in ["in_progress", "resolved", "declined"] {
            let parsed: ComplaintResponseStatus =
                serde_json::from_value(json!(status)).expect("response status");
            assert_eq!(
                serde_json::to_value(parsed).expect("status wire value"),
                status
            );
        }
        for status in ["open", "answered", "stopped", "unknown"] {
            assert!(serde_json::from_value::<ComplaintResponseStatus>(json!(status)).is_err());
        }
    }
}
