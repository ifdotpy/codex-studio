//! Pure filtering for the persisted rules list.

use std::collections::BTreeMap;

use serde_json::Value;
use studio_runtime_store::{AgentRecord, StoredAgent, StoredRule};
use thiserror::Error;

#[derive(Debug, Error, Eq, PartialEq)]
pub enum RulesError {
    #[error("invalid stored object {object}/{id}")]
    InvalidStoredObject { object: &'static str, id: String },
}

pub fn visible_rules(
    rules: Vec<StoredRule>,
    agents: Vec<StoredAgent>,
) -> Result<Vec<StoredRule>, RulesError> {
    let mut deleted_agents = BTreeMap::new();
    for agent in agents {
        let value = serde_json::from_str::<Value>(&agent.record)
            .map_err(|_| invalid_object("agents", agent.id.clone()))?;
        let record = serde_json::from_value::<AgentRecord>(value)
            .map_err(|_| invalid_object("agents", agent.id.clone()))?;
        if record.id().0 != agent.id {
            return Err(invalid_object("agents", agent.id));
        }
        let deleted = match record.get("deletedAt") {
            None => false,
            Some(Value::Number(value)) => value.as_f64().is_some_and(|number| number != 0.0),
            _ => return Err(invalid_object("agents", agent.id)),
        };
        deleted_agents.insert(agent.id, deleted);
    }

    let mut visible = Vec::with_capacity(rules.len());
    for rule in rules {
        let value = serde_json::from_str::<Value>(&rule.record)
            .map_err(|_| invalid_object("rules", rule.id.clone()))?;
        let Some(object) = value.as_object() else {
            return Err(invalid_object("rules", rule.id));
        };
        if object.get("id").and_then(Value::as_str) != Some(rule.id.as_str()) {
            return Err(invalid_object("rules", rule.id));
        }
        let Some(agent_id) = object.get("agent").and_then(Value::as_str) else {
            return Err(invalid_object("rules", rule.id));
        };
        let Some(deleted) = deleted_agents.get(agent_id) else {
            return Err(invalid_object("rules", rule.id));
        };
        if !deleted {
            visible.push(rule);
        }
    }
    Ok(visible)
}

fn invalid_object(object: &'static str, id: String) -> RulesError {
    RulesError::InvalidStoredObject { object, id }
}

#[cfg(test)]
mod tests {
    use super::{RulesError, visible_rules};
    use serde_json::json;
    use studio_runtime_store::{StoredAgent, StoredRule};

    fn agent(id: &str, deleted_at: Option<serde_json::Value>) -> StoredAgent {
        let mut record = json!({
            "id": id,
            "rootId": id,
            "epoch": 0,
            "status": "idle",
        });
        if let Some(value) = deleted_at {
            record["deletedAt"] = value;
        }
        StoredAgent {
            id: id.to_owned(),
            record: record.to_string(),
        }
    }

    fn rule(id: &str, agent: &str) -> StoredRule {
        StoredRule {
            id: id.to_owned(),
            record: json!({"id": id, "agent": agent}).to_string(),
        }
    }

    #[test]
    fn excludes_rules_with_deleted_agents_and_keeps_python_order() {
        let result = visible_rules(
            vec![rule("first", "live"), rule("second", "deleted")],
            vec![agent("live", None), agent("deleted", Some(json!(1)))],
        )
        .expect("valid stored records filter");
        assert_eq!(result, vec![rule("first", "live")]);
    }

    #[test]
    fn refuses_rule_with_missing_or_mismatched_agent() {
        assert_eq!(
            visible_rules(vec![rule("rule", "missing")], vec![]),
            Err(RulesError::InvalidStoredObject {
                object: "rules",
                id: "rule".to_owned(),
            })
        );
        let mut invalid = rule("rule", "agent");
        invalid.record = json!({"id": "other", "agent": "agent"}).to_string();
        assert_eq!(
            visible_rules(vec![invalid], vec![agent("agent", None)]),
            Err(RulesError::InvalidStoredObject {
                object: "rules",
                id: "rule".to_owned(),
            })
        );
    }

    #[test]
    fn refuses_invalid_agent_deletion_marker() {
        assert_eq!(
            visible_rules(
                vec![rule("rule", "agent")],
                vec![agent("agent", Some(json!(true)))]
            ),
            Err(RulesError::InvalidStoredObject {
                object: "agents",
                id: "agent".to_owned(),
            })
        );
    }
}
