//! Projection rules for persisted questions, complaints, and checkpoints.

use std::collections::BTreeSet;
use std::fmt;

use serde::Deserialize;
use serde::de::{MapAccess, Visitor};
use serde_json::{Value, json};
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
pub(crate) fn question_history_entity(
    record: &studio_runtime_store::RequestRecord,
) -> Result<Option<Value>, QuestionsReadsError> {
    use studio_runtime_schema::request_record::RequestMethod;
    if record.deleted_at.is_some_and(|at| at != 0.0)
        || matches!(
            record.method,
            RequestMethod::BrowserOpen
                | RequestMethod::MonitorApprove
                | RequestMethod::CommandApproval
                | RequestMethod::FileApproval
                | RequestMethod::PermissionsApproval
        )
    {
        return Ok(None);
    }
    let params: HistoryParams =
        serde_json::from_str(record.params.get()).map_err(|_| invalid("requests", &record.id))?;
    if record.deleted_at.is_some_and(|at| at != 0.0)
        || !match record.method {
            RequestMethod::AsyncQuestion | RequestMethod::UserInput => true,
            RequestMethod::Elicitation => params.mode.as_deref() == Some("form"),
            RequestMethod::BrowserOpen
            | RequestMethod::MonitorApprove
            | RequestMethod::CommandApproval
            | RequestMethod::FileApproval
            | RequestMethod::PermissionsApproval => false,
        }
    {
        return Ok(None);
    }
    let questions = if record.method == RequestMethod::Elicitation {
        params.requested_schema.as_ref().and_then(|s| s.properties.as_ref()).map(|properties| properties.0.iter().map(|(id, p)| json!({"id":id,"question":p.title.as_deref().unwrap_or(id),"isSecret":p.is_secret.unwrap_or(false) || p.write_only.unwrap_or(false) || p.format.as_deref() == Some("password")})).collect::<Vec<_>>()).unwrap_or_default()
    } else {
        params.questions.as_ref().map(|questions| questions.iter().map(|q| {
            let options = q.options.as_ref().map(|items| items.iter().map(|option| match option { HistoryOption::Label(label) => json!({"label":label,"description":null}), HistoryOption::Detailed { label, description } => json!({"label":label,"description":description}) }).collect::<Vec<_>>()).unwrap_or_default();
            json!({"id":q.id,"question":q.question.as_deref().unwrap_or(&q.id),"isSecret":q.is_secret.unwrap_or(false),"header":q.header,"options":options,"multiSelect":q.multi_select.unwrap_or(false),"isOther":q.is_other.unwrap_or(false)})
        }).collect::<Vec<_>>()).unwrap_or_default()
    };
    let item_id = params
        .item_id
        .as_deref()
        .filter(|v| !v.is_empty())
        .or_else(|| {
            if record.method == RequestMethod::AsyncQuestion {
                record
                    .id
                    .strip_prefix(&record.agent)
                    .and_then(|s| s.strip_prefix(ASYNC_QUESTION_SEPARATOR))
                    .filter(|s| !s.is_empty())
            } else {
                None
            }
        });
    let turn_id = params
        .turn_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .or(record.turn_id.as_deref().filter(|s| !s.is_empty()));
    let history = record.answer_history.as_ref().map(|entries| entries.iter().map(|entry| json!({"id":entry.id,"question":entry.question,"isSecret":entry.is_secret,"answer":if entry.is_secret { json!(null) } else { json!(entry.answer) }})).collect::<Vec<_>>());
    let projected = json!({"id":record.id,"agent":record.agent,"method":record.method,"status":record.status,"createdAt":record.created_at,"answeredAt":record.answered_at,"answeredBy":record.answered_by,"decision":record.decision,"deferred":record.deferred,"deferredAt":record.deferred_at,"deferredBy":record.deferred_by,"answerHistory":history,"answerError":record.answer_error,"itemId":item_id,"turnId":turn_id,"questions":questions});
    Ok(crate::sync_views::project_persisted_record(
        "questionHistory",
        &projected,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryParams {
    mode: Option<String>,
    item_id: Option<String>,
    turn_id: Option<String>,
    questions: Option<Vec<HistoryQuestion>>,
    requested_schema: Option<HistorySchema>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryQuestion {
    id: String,
    question: Option<String>,
    is_secret: Option<bool>,
    header: Option<String>,
    options: Option<Vec<HistoryOption>>,
    multi_select: Option<bool>,
    is_other: Option<bool>,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum HistoryOption {
    Label(String),
    Detailed {
        label: String,
        description: Option<String>,
    },
}
#[derive(Deserialize)]
struct HistorySchema {
    properties: Option<OrderedProperties>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryProperty {
    title: Option<String>,
    is_secret: Option<bool>,
    write_only: Option<bool>,
    format: Option<String>,
}
struct OrderedProperties(Vec<(String, HistoryProperty)>);
impl<'de> Deserialize<'de> for OrderedProperties {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OrderedVisitor;
        impl<'de> Visitor<'de> for OrderedVisitor {
            type Value = OrderedProperties;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object of elicitation properties")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    entries.push(entry);
                }
                Ok(OrderedProperties(entries))
            }
        }
        deserializer.deserialize_map(OrderedVisitor)
    }
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
        if caller.is_deleted() {
            return Err(QuestionsReadsError::DeletedAgent);
        }
        vec![agent_id.to_owned()]
    } else {
        agents
            .iter()
            .filter(|agent| !agent.is_deleted())
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

/// Separator of an asynchronous question id: `<agent>:question:<agent message item>`.
const ASYNC_QUESTION_SEPARATOR: &str = ":question:";

/// Request id of the asynchronous question that a completed agent message asks.
pub fn async_question_id(agent: &str, item: &str) -> String {
    format!("{agent}{ASYNC_QUESTION_SEPARATOR}{item}")
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

    #[test]
    fn history_retains_property_order_and_redacts_secret_answers() {
        let raw = r#"{"id":"r","agent":"a","method":"mcpServer/elicitation/request","status":"answered","createdAt":1,"params":{"mode":"form","requestedSchema":{"properties":{"z":{"title":"Last","writeOnly":true},"a":{"title":"First"}}}},"answerHistory":[{"id":"z","question":"Last","isSecret":true,"answer":"secret"}]}"#;
        let record = serde_json::from_str(raw).unwrap();
        let result = super::question_history_entity(&record).unwrap().unwrap();
        assert_eq!(result["questions"][0]["id"], "z");
        assert_eq!(result["questions"][1]["id"], "a");
        assert_eq!(result["answerHistory"][0]["answer"], json!(null));
    }

    fn agent(id: &str, root: &str, deleted_at: Option<f64>) -> AgentRecord {
        let mut record = json!({"compactions":0,"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,"id":id,"rootId":root,"epoch":0,"status":"idle"});
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
