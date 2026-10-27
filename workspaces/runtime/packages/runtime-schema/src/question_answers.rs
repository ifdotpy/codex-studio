//! Fresh answer construction for the supported question cards. This is a producer
//! contract: native legacy answers remain opaque at their transport boundary.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum QuestionDecision {
    Accept,
    Decline,
    Cancel,
}

#[derive(Serialize, JsonSchema)]
pub struct QuestionAnswerStrings {
    pub answers: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct QuestionSelections {
    pub answers: BTreeMap<String, QuestionAnswerStrings>,
}

#[derive(Serialize, JsonSchema)]
pub struct QuestionApproval {
    pub decision: QuestionDecision,
}

#[derive(Serialize, JsonSchema)]
pub struct QuestionElicitation {
    pub decision: QuestionDecision,
    #[schemars(schema_with = "opaque_object")]
    pub content: BTreeMap<String, Value>,
}

/// The method selects the required answer fields. The renderer sends only data;
/// the stored request method remains the server's authority.
#[derive(Serialize, JsonSchema)]
#[serde(tag = "method", content = "data")]
pub enum QuestionCardAnswer {
    #[serde(rename = "item/commandExecution/requestApproval")]
    Command(QuestionApproval),
    #[serde(rename = "item/fileChange/requestApproval")]
    File(QuestionApproval),
    #[serde(rename = "item/permissions/requestApproval")]
    Permissions(QuestionApproval),
    #[serde(rename = "item/tool/requestUserInput")]
    UserInput(QuestionSelections),
    #[serde(rename = "agent/asyncQuestion")]
    AsyncQuestion(QuestionSelections),
    #[serde(rename = "mcpServer/elicitation/request")]
    Elicitation(QuestionElicitation),
}

fn opaque_object(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type":"object", "additionalProperties":{"$ref": "#/components/schemas/JsonValue"}})
}

pub(crate) fn schemas() -> serde_json::Map<String, Value> {
    let mut generator = schemars::generate::SchemaSettings::draft2020_12()
        .for_serialize()
        .with(|settings| settings.definitions_path = "/components/schemas".into())
        .into_generator();
    generator.subschema_for::<QuestionCardAnswer>();
    generator.take_definitions(true)
}

pub(crate) fn answer_body_decision() -> Value {
    let mut decisions = vec![serde_json::json!("answer")];
    let schema = schemars::schema_for!(QuestionDecision).to_value();
    // The decision owner is a fieldless enum, so Schemars always emits its tags.
    decisions.extend(
        schema["enum"]
            .as_array()
            .expect("decision enum")
            .iter()
            .cloned(),
    );
    serde_json::json!({"anyOf":[{"type":"string","enum":decisions},{"type":"null"}],"default":null,"title":"Decision"})
}

pub(crate) fn check_schema_parity(openapi: &Value) -> Result<(), String> {
    for (name, schema) in schemas() {
        if openapi.pointer(&format!("/components/schemas/{name}")) != Some(&schema) {
            return Err(format!(
                "{name} must match runtime-schema/src/question_answers.rs"
            ));
        }
    }
    if openapi.pointer("/components/schemas/AnswerBody/properties/decision")
        != Some(&answer_body_decision())
    {
        return Err(
            "AnswerBody decision must include the question_answers.rs decision owner".into(),
        );
    }
    Ok(())
}
