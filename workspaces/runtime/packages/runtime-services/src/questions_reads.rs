//! Projection rules for persisted questions, complaints, and checkpoints.

use std::collections::BTreeSet;

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

/// Separator of an asynchronous question id: `<agent>:question:<agent message item>`.
const ASYNC_QUESTION_SEPARATOR: &str = ":question:";

/// Request id of the asynchronous question that a completed agent message asks.
pub fn async_question_id(agent: &str, item: &str) -> String {
    format!("{agent}{ASYNC_QUESTION_SEPARATOR}{item}")
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
