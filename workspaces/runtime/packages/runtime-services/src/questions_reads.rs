//! Projection rules for persisted questions, complaints, and checkpoints.

use std::collections::BTreeSet;
use std::fmt;

use serde::Deserialize;
use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde_json::{Map, Value, json};
use studio_runtime_store::{AgentRecord, CheckpointRecord, ComplaintRecord};
use thiserror::Error;

#[derive(Debug, Error, Eq, PartialEq)]
pub enum QuestionsReadsError {
    #[error("unknown agent")]
    UnknownAgent,
    #[error("this agent was deleted")]
    DeletedAgent,
    #[error("unknown complaint")]
    UnknownComplaint,
    #[error("invalid stored object {object}/{id}")]
    InvalidStoredObject { object: &'static str, id: String },
}

/// Question history projection published per request.
pub(crate) fn question_history_entity(raw: &str) -> Result<Option<Value>, QuestionsReadsError> {
    let record: Value = serde_json::from_str(raw).map_err(|_| invalid("requests", "unknown"))?;
    let Some(record) = record.as_object() else {
        return Err(invalid("requests", "unknown"));
    };
    if deleted_value(record.get("deletedAt")) || !is_question(record) {
        return Ok(None);
    }
    let id = string_member(record, "id").ok_or_else(|| invalid("requests", "unknown"))?;
    let order = elicitation_property_order(raw, id)?;
    let mut projected = question_projection(record, id, &order)?;
    // Secret answers are transient user input, never durable client cache data.
    if let Some(history) = projected
        .get_mut("answerHistory")
        .and_then(Value::as_array_mut)
    {
        for answer in history {
            if answer.get("isSecret").and_then(Value::as_bool) == Some(true) {
                answer["answer"] = Value::Null;
            }
        }
    }
    Ok(crate::sync_views::project_persisted_record(
        "questionHistory",
        &projected,
    ))
}

pub fn complaint_detail(stored: Option<ComplaintRecord>) -> Result<Value, QuestionsReadsError> {
    let record = stored.ok_or(QuestionsReadsError::UnknownComplaint)?;
    Ok(record.into_value())
}

pub fn checkpoint_list(
    agent_id: Option<&str>,
    agents: Vec<AgentRecord>,
    checkpoints: Vec<CheckpointRecord>,
) -> Result<Value, QuestionsReadsError> {
    let ids = if let Some(agent_id) = agent_id.filter(|id| !id.is_empty()) {
        let caller = agents
            .iter()
            .find(|agent| agent.id().as_ref() == agent_id)
            .ok_or(QuestionsReadsError::UnknownAgent)?;
        if deleted_value(caller.extra.get("deletedAt")) {
            return Err(QuestionsReadsError::DeletedAgent);
        }
        vec![agent_id.to_owned()]
    } else {
        agents
            .iter()
            .filter(|agent| !deleted_value(agent.extra.get("deletedAt")))
            .map(|agent| agent.id().as_ref().to_owned())
            .collect()
    };
    let ids = ids.into_iter().collect::<BTreeSet<_>>();
    let mut items = Vec::new();
    for checkpoint in checkpoints {
        if !ids.contains(checkpoint.agent().unwrap_or_default()) {
            continue;
        }
        let Value::Object(mut record) = checkpoint.into_value() else {
            unreachable!("typed checkpoint serializes as an object");
        };
        record.remove("items");
        record.remove("historyDelta");
        record.remove("_payloadBlobs");
        items.push(Value::Object(record));
    }
    Ok(json!({"checkpoints": items}))
}

fn deleted_value(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_f64)
        .is_some_and(|number| number != 0.0)
}

fn string_member<'a>(record: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    record.get(name).and_then(Value::as_str)
}

fn is_question(record: &Map<String, Value>) -> bool {
    match string_member(record, "method") {
        Some("agent/asyncQuestion" | "item/tool/requestUserInput") => true,
        Some("mcpServer/elicitation/request") => {
            record
                .get("params")
                .and_then(Value::as_object)
                .and_then(|params| params.get("mode"))
                .and_then(Value::as_str)
                == Some("form")
        }
        _ => false,
    }
}

fn question_projection(
    record: &Map<String, Value>,
    id: &str,
    property_order: &[String],
) -> Result<Value, QuestionsReadsError> {
    let params = record.get("params").and_then(Value::as_object);
    let empty = Map::new();
    let params = params.unwrap_or(&empty);
    let questions = if string_member(record, "method") == Some("mcpServer/elicitation/request") {
        elicitation_fields(params, id, property_order)?
    } else {
        request_fields(params, id)?
    };
    let mut result = Map::new();
    for field in [
        "id",
        "agent",
        "method",
        "status",
        "createdAt",
        "answeredAt",
        "answeredBy",
        "decision",
        "deferred",
        "deferredAt",
        "deferredBy",
        "answerHistory",
        "answerError",
    ] {
        result.insert(
            field.to_owned(),
            record.get(field).cloned().unwrap_or(Value::Null),
        );
    }
    result.insert(
        "itemId".to_owned(),
        question_item_id(record, id).map_or(Value::Null, |item| Value::String(item.to_owned())),
    );
    result.insert(
        "turnId".to_owned(),
        question_turn_id(record, params).map_or(Value::Null, |turn| Value::String(turn.to_owned())),
    );
    result.insert("questions".to_owned(), Value::Array(questions));
    Ok(Value::Object(result))
}

/// Separator of an asynchronous question id: `<agent>:question:<agent message item>`.
const ASYNC_QUESTION_SEPARATOR: &str = ":question:";

/// Request id of the asynchronous question that a completed agent message asks.
pub fn async_question_id(agent: &str, item: &str) -> String {
    format!("{agent}{ASYNC_QUESTION_SEPARATOR}{item}")
}

/// Provider transcript item that asked this question, when the record names one.
fn question_item_id<'a>(record: &'a Map<String, Value>, id: &'a str) -> Option<&'a str> {
    if let Some(item) = record
        .get("params")
        .and_then(Value::as_object)
        .and_then(|params| string_member(params, "itemId"))
        .filter(|item| !item.is_empty())
    {
        return Some(item);
    }
    if string_member(record, "method") != Some("agent/asyncQuestion") {
        return None;
    }
    let agent = string_member(record, "agent")?;
    id.strip_prefix(agent)
        .and_then(|rest| rest.strip_prefix(ASYNC_QUESTION_SEPARATOR))
        .filter(|item| !item.is_empty())
}

/// Native turn that asked this question: the provider request names it, else the record.
fn question_turn_id<'a>(
    record: &'a Map<String, Value>,
    params: &'a Map<String, Value>,
) -> Option<&'a str> {
    string_member(params, "turnId")
        .filter(|turn| !turn.is_empty())
        .or_else(|| string_member(record, "turnId").filter(|turn| !turn.is_empty()))
}

/// Header, choices and selection mode of a native or asynchronous question.
fn question_choices(field: &Map<String, Value>) -> [(&'static str, Value); 4] {
    let options = field
        .get("options")
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(|option| {
                    match option {
                    Value::String(label) => Some(json!({"label": label, "description": null})),
                    Value::Object(option) => string_member(option, "label").map(|label| {
                        json!({"label": label, "description": string_member(option, "description")})
                    }),
                    Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) => None,
                }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    [
        (
            "header",
            string_member(field, "header").map_or(Value::Null, |header| json!(header)),
        ),
        ("options", Value::Array(options)),
        (
            "multiSelect",
            json!(field.get("multiSelect") == Some(&Value::Bool(true))),
        ),
        (
            "isOther",
            json!(field.get("isOther") == Some(&Value::Bool(true))),
        ),
    ]
}

fn request_fields(
    params: &Map<String, Value>,
    id: &str,
) -> Result<Vec<Value>, QuestionsReadsError> {
    let Some(value) = params.get("questions") else {
        return Ok(Vec::new());
    };
    let fields = value.as_array().ok_or_else(|| invalid("requests", id))?;
    fields
        .iter()
        .map(|field| {
            let field = field.as_object().ok_or_else(|| invalid("requests", id))?;
            let id_value = string_member(field, "id").ok_or_else(|| invalid("requests", id))?;
            let question = match field.get("question") {
                None => id_value,
                Some(value) => value.as_str().ok_or_else(|| invalid("requests", id))?,
            };
            let secret = match field.get("isSecret") {
                None => false,
                Some(value) => value.as_bool().ok_or_else(|| invalid("requests", id))?,
            };
            let mut entry = json!({"id": id_value, "question": question, "isSecret": secret});
            for (name, value) in question_choices(field) {
                entry[name] = value;
            }
            Ok(entry)
        })
        .collect()
}

fn elicitation_fields(
    params: &Map<String, Value>,
    id: &str,
    property_order: &[String],
) -> Result<Vec<Value>, QuestionsReadsError> {
    let Some(schema) = params.get("requestedSchema") else {
        return Ok(Vec::new());
    };
    let schema = schema.as_object().ok_or_else(|| invalid("requests", id))?;
    let Some(properties) = schema.get("properties") else {
        return Ok(Vec::new());
    };
    let properties = properties
        .as_object()
        .ok_or_else(|| invalid("requests", id))?;
    property_order
        .iter()
        .map(|key| {
            let value = properties.get(key).ok_or_else(|| invalid("requests", id))?;
            let property = value.as_object().ok_or_else(|| invalid("requests", id))?;
            let question = match property.get("title") {
                None => key.as_str(),
                Some(value) => value.as_str().ok_or_else(|| invalid("requests", id))?,
            };
            let secret = ["isSecret", "writeOnly"].into_iter().try_fold(
                false,
                |secret, name| match property.get(name) {
                    None => Ok(secret),
                    Some(value) => value
                        .as_bool()
                        .map(|flag| secret || flag)
                        .ok_or_else(|| invalid("requests", id)),
                },
            )? || string_member(property, "format") == Some("password");
            Ok(json!({"id": key, "question": question, "isSecret": secret}))
        })
        .collect()
}

#[derive(Deserialize)]
struct StoredRequestOrder {
    #[serde(default)]
    params: Option<StoredRequestParamsOrder>,
}

#[derive(Deserialize)]
struct StoredRequestParamsOrder {
    #[serde(rename = "requestedSchema", default)]
    requested_schema: Option<StoredRequestSchemaOrder>,
}

#[derive(Deserialize)]
struct StoredRequestSchemaOrder {
    #[serde(default)]
    properties: OrderedPropertyKeys,
}

#[derive(Default)]
struct OrderedPropertyKeys(Vec<String>);

impl<'de> Deserialize<'de> for OrderedPropertyKeys {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct KeysVisitor;

        impl<'de> Visitor<'de> for KeysVisitor {
            type Value = OrderedPropertyKeys;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object of elicitation properties")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut keys = Vec::new();
                while let Some((key, _)) = map.next_entry::<String, IgnoredAny>()? {
                    keys.push(key);
                }
                Ok(OrderedPropertyKeys(keys))
            }
        }

        deserializer.deserialize_map(KeysVisitor)
    }
}

fn elicitation_property_order(raw: &str, id: &str) -> Result<Vec<String>, QuestionsReadsError> {
    let request =
        serde_json::from_str::<StoredRequestOrder>(raw).map_err(|_| invalid("requests", id))?;
    Ok(request
        .params
        .and_then(|params| params.requested_schema)
        .map_or_else(Vec::new, |schema| schema.properties.0))
}

fn invalid(object: &'static str, id: &str) -> QuestionsReadsError {
    QuestionsReadsError::InvalidStoredObject {
        object,
        id: id.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{QuestionsReadsError, checkpoint_list, complaint_detail};
    use serde_json::json;
    use studio_runtime_store::{AgentRecord, CheckpointRecord};

    fn agent(id: &str, root: &str, deleted_at: Option<f64>) -> AgentRecord {
        let mut record = json!({"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,"id":id,"rootId":root,"epoch":0,"status":"idle"});
        if let Some(deleted_at) = deleted_at {
            record["deletedAt"] = json!(deleted_at);
        }
        AgentRecord::from_value(record).expect("agent fixture validates")
    }

    #[test]
    fn refuses_unknown_references() {
        assert_eq!(
            complaint_detail(None),
            Err(QuestionsReadsError::UnknownComplaint)
        );
    }

    #[test]
    fn checkpoint_summaries_filter_agents_and_strip_payloads() {
        let row = json!({"id":"checkpoint","agent":"lead","created":1.0,"items":["large"],"historyDelta":["large"],"_payloadBlobs":{"secret":"sha"},"extension":9007199254740993_u64});
        let mut other = row.clone();
        other["id"] = json!("other-checkpoint");
        other["agent"] = json!("other");
        let rows = vec![row, other]
            .into_iter()
            .map(|row| CheckpointRecord::from_value(row).expect("checkpoint validates"))
            .collect();
        let result = checkpoint_list(Some("lead"), vec![agent("lead", "lead", None)], rows)
            .expect("checkpoint summary succeeds");
        assert_eq!(
            result["checkpoints"].as_array().expect("checkpoints").len(),
            1
        );
        assert_eq!(result["checkpoints"][0]["id"], "checkpoint");
        assert!(result["checkpoints"][0].get("historyDelta").is_none());
        assert!(result["checkpoints"][0].get("items").is_none());
        assert!(result["checkpoints"][0].get("_payloadBlobs").is_none());
        assert_eq!(
            result["checkpoints"][0]["extension"].as_u64(),
            Some(9_007_199_254_740_993)
        );
    }

    #[test]
    fn refuses_deleted_callers_for_checkpoint_reads() {
        let deleted = agent("deleted", "deleted", Some(1.0));
        assert_eq!(
            checkpoint_list(Some("deleted"), vec![deleted], vec![]),
            Err(QuestionsReadsError::DeletedAgent),
        );
    }
}
