//! Projection rules for persisted questions, complaints, and checkpoints.

use std::collections::BTreeSet;
use std::fmt;

use serde::Deserialize;
use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde_json::{Map, Value, json};
use thiserror::Error;

#[derive(Debug, Error, Eq, PartialEq)]
pub enum QuestionsReadsError {
    #[error("a conversation is required")]
    ConversationRequired,
    #[error("unknown agent")]
    UnknownAgent,
    #[error("this agent was deleted")]
    DeletedAgent,
    #[error("unknown complaint")]
    UnknownComplaint,
    #[error("invalid stored object {object}/{id}")]
    InvalidStoredObject { object: &'static str, id: String },
}

pub struct QuestionReadRecord {
    pub id: String,
    pub raw: String,
    pub value: Map<String, Value>,
}

pub fn question_history(
    agent_id: Option<&str>,
    agents: Vec<(String, Map<String, Value>)>,
    requests: Vec<QuestionReadRecord>,
) -> Result<Value, QuestionsReadsError> {
    let agent_id = agent_id
        .filter(|id| !id.is_empty())
        .ok_or(QuestionsReadsError::ConversationRequired)?;
    let owner = agents
        .iter()
        .find(|(id, _)| id == agent_id)
        .ok_or(QuestionsReadsError::UnknownAgent)?;
    validate_agents(&agents)?;
    if deleted_value(owner.1.get("deletedAt")) {
        return Err(QuestionsReadsError::DeletedAgent);
    }
    let root = string_member(&owner.1, "rootId").unwrap_or_default();
    let mut ids = BTreeSet::new();
    for (id, agent) in &agents {
        if string_member(agent, "rootId") == Some(root) && !deleted_value(agent.get("deletedAt")) {
            ids.insert(id.clone());
        }
    }

    let mut items = Vec::new();
    for QuestionReadRecord {
        id,
        raw,
        value: record,
    } in requests
    {
        if string_member(&record, "id") != Some(&id) {
            return Err(invalid("requests", &id));
        }
        let Some(agent) = string_member(&record, "agent") else {
            continue;
        };
        if !ids.contains(agent) || deleted_value(record.get("deletedAt")) {
            continue;
        }
        if !is_question(&record) {
            continue;
        }
        let property_order = elicitation_property_order(&raw, &id)?;
        items.push(question_projection(&record, &id, &property_order)?);
    }
    items.sort_by(|left, right| timestamp(right).total_cmp(&timestamp(left)));
    Ok(json!({"items": items}))
}

pub fn complaint_detail(
    stored: Option<(String, Map<String, Value>)>,
) -> Result<Value, QuestionsReadsError> {
    let (id, record) = stored.ok_or(QuestionsReadsError::UnknownComplaint)?;
    if string_member(&record, "id") != Some(&id) {
        return Err(invalid("complaints", &id));
    }
    Ok(Value::Object(record))
}

pub fn checkpoint_list(
    agent_id: Option<&str>,
    agents: Vec<(String, Map<String, Value>)>,
    checkpoints: Vec<(String, Map<String, Value>)>,
) -> Result<Value, QuestionsReadsError> {
    validate_agents(&agents)?;
    let ids = if let Some(agent_id) = agent_id.filter(|id| !id.is_empty()) {
        let caller = agents
            .iter()
            .find(|(id, _)| id == agent_id)
            .ok_or(QuestionsReadsError::UnknownAgent)?;
        if deleted_value(caller.1.get("deletedAt")) {
            return Err(QuestionsReadsError::DeletedAgent);
        }
        vec![agent_id.to_owned()]
    } else {
        agents
            .iter()
            .filter(|(_, agent)| !deleted_value(agent.get("deletedAt")))
            .map(|(id, _)| id.clone())
            .collect()
    };
    let ids = ids.into_iter().collect::<BTreeSet<_>>();
    let mut items = Vec::new();
    for (id, mut record) in checkpoints {
        if string_member(&record, "id") != Some(&id) {
            return Err(invalid("checkpoints", &id));
        }
        let checkpoint_agent = string_member(&record, "agent").unwrap_or_default();
        if !ids.contains(checkpoint_agent) {
            continue;
        }
        record.remove("items");
        record.remove("historyDelta");
        record.remove("_payloadBlobs");
        items.push(Value::Object(record));
    }
    Ok(json!({"checkpoints": items}))
}

fn validate_agents(agents: &[(String, Map<String, Value>)]) -> Result<(), QuestionsReadsError> {
    for (id, agent) in agents {
        if string_member(agent, "id") != Some(id) {
            return Err(invalid("agents", id));
        }
    }
    Ok(())
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
    result.insert("questions".to_owned(), Value::Array(questions));
    Ok(Value::Object(result))
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
            Ok(json!({"id": id_value, "question": question, "isSecret": secret}))
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

fn timestamp(record: &Value) -> f64 {
    let answered = record
        .get("answeredAt")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if answered != 0.0 {
        return answered;
    }
    record
        .get("createdAt")
        .and_then(Value::as_f64)
        .filter(|created| *created != 0.0)
        .unwrap_or(0.0)
}

fn invalid(object: &'static str, id: &str) -> QuestionsReadsError {
    QuestionsReadsError::InvalidStoredObject {
        object,
        id: id.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        QuestionReadRecord, QuestionsReadsError, checkpoint_list, complaint_detail,
        question_history,
    };
    use serde_json::{Map, Value, json};

    fn agent(id: &str, root: &str, deleted_at: Option<f64>) -> (String, Map<String, Value>) {
        let mut record = json!({"id":id,"rootId":root,"epoch":0,"status":"idle"});
        if let Some(deleted_at) = deleted_at {
            record["deletedAt"] = json!(deleted_at);
        }
        (
            id.to_owned(),
            record.as_object().expect("agent object").clone(),
        )
    }

    fn request(id: &str, agent: &str, method: &str, at: i64) -> QuestionReadRecord {
        let value = json!({
            "id":id,"agent":agent,"method":method,"status":"answered",
            "createdAt":at,"params":{"questions":[{"id":"name"}]}
        });
        QuestionReadRecord {
            id: id.to_owned(),
            raw: value.to_string(),
            value: value.as_object().expect("fixture object").clone(),
        }
    }

    #[test]
    fn projects_team_questions_and_orders_newest_first() {
        let result = question_history(
            Some("lead"),
            vec![
                agent("lead", "lead", None),
                agent("worker", "lead", None),
                agent("deleted", "lead", Some(1.0)),
            ],
            vec![
                request("old", "lead", "agent/asyncQuestion", 1),
                request("new", "worker", "item/tool/requestUserInput", 2),
                request("other", "other", "agent/asyncQuestion", 3),
                request("gone", "deleted", "agent/asyncQuestion", 4),
                request("not-question", "lead", "tools/call", 5),
            ],
        )
        .expect("question projection succeeds");
        assert_eq!(result["items"][0]["id"], "new");
        assert_eq!(result["items"][1]["id"], "old");
        assert_eq!(result["items"][0]["questions"][0]["question"], "name");
    }

    #[test]
    fn preserves_python_written_elicitation_property_order() {
        let source = r#"{"id":"form","agent":"lead","method":"mcpServer/elicitation/request","status":"pending","params":{"mode":"form","requestedSchema":{"properties":{"password":{"format":"password"},"color":{}}}}}"#;
        let value = serde_json::from_str(source).expect("stored object");
        let record = QuestionReadRecord {
            id: "form".to_owned(),
            raw: source.to_owned(),
            value,
        };
        let result = question_history(
            Some("lead"),
            vec![agent("lead", "lead", None)],
            vec![record],
        )
        .expect("form question projects");
        assert_eq!(result["items"][0]["questions"][0]["id"], "password");
        assert_eq!(result["items"][0]["questions"][1]["id"], "color");
    }

    #[test]
    fn refuses_invalid_records_and_unknown_references() {
        assert_eq!(
            question_history(Some("missing"), vec![agent("lead", "lead", None)], vec![]),
            Err(QuestionsReadsError::UnknownAgent),
        );
        let value = json!({"id":"elsewhere"});
        let invalid = QuestionReadRecord {
            id: "q".to_owned(),
            raw: value.to_string(),
            value: value.as_object().expect("record object").clone(),
        };
        assert_eq!(
            question_history(
                Some("lead"),
                vec![agent("lead", "lead", None)],
                vec![invalid]
            ),
            Err(QuestionsReadsError::InvalidStoredObject {
                object: "requests",
                id: "q".to_owned()
            }),
        );
        assert_eq!(
            complaint_detail(None),
            Err(QuestionsReadsError::UnknownComplaint)
        );
    }

    #[test]
    fn filters_checkpoint_rows_and_refuses_invalid_records() {
        let row = json!({"id":"checkpoint","agent":"lead","created":1.0,"items":["large"],"_payloadBlobs":{"secret":"sha"},"extension":9007199254740993_u64});
        let rows = vec![(
            "checkpoint".to_owned(),
            row.as_object().expect("checkpoint object").clone(),
        )];
        let result = checkpoint_list(Some("lead"), vec![agent("lead", "lead", None)], rows)
            .expect("checkpoint summary succeeds");
        assert!(result["checkpoints"][0].get("items").is_none());
        assert!(result["checkpoints"][0].get("_payloadBlobs").is_none());
        assert_eq!(
            result["checkpoints"][0]["extension"].as_u64(),
            Some(9_007_199_254_740_993)
        );
    }

    #[test]
    fn refuses_deleted_callers_for_both_read_paths() {
        let deleted = agent("deleted", "deleted", Some(1.0));
        assert_eq!(
            question_history(Some("deleted"), vec![deleted.clone()], vec![]),
            Err(QuestionsReadsError::DeletedAgent),
        );
        assert_eq!(
            checkpoint_list(Some("deleted"), vec![deleted], vec![]),
            Err(QuestionsReadsError::DeletedAgent),
        );
    }
}
