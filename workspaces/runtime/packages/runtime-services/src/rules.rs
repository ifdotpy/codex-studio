//! Pure filtering for the persisted rules list.

use std::collections::BTreeMap;

use serde_json::Value;
use studio_runtime_store::{AgentRecord, RuleRecord};
use thiserror::Error;

#[derive(Debug, Error, Eq, PartialEq)]
pub enum RulesError {
    #[error("invalid stored object {object}/{id}")]
    InvalidStoredObject { object: &'static str, id: String },
}

pub fn visible_rules(
    rules: Vec<RuleRecord>,
    agents: Vec<AgentRecord>,
) -> Result<Vec<RuleRecord>, RulesError> {
    let mut deleted_agents = BTreeMap::new();
    for agent in agents {
        let id = agent.id().0.clone();
        let deleted = match agent.extra.get("deletedAt") {
            None | Some(Value::Null) => false,
            Some(Value::Number(value)) => value.as_f64().is_some_and(|number| number != 0.0),
            _ => return Err(invalid_object("agents", id)),
        };
        deleted_agents.insert(id, deleted);
    }

    let mut visible = Vec::with_capacity(rules.len());
    for rule in rules {
        let value = rule.value();
        let id = rule.id().as_str().to_owned();
        let Some(agent_id) = value.get("agent").and_then(Value::as_str) else {
            return Err(invalid_object("rules", id));
        };
        let Some(deleted) = deleted_agents.get(agent_id) else {
            return Err(invalid_object("rules", id));
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
    use studio_runtime_store::{AgentRecord, RuleRecord};

    fn agent(id: &str, deleted_at: Option<serde_json::Value>) -> AgentRecord {
        let mut record = json!({
            "id": id,
            "rootId": id,
            "epoch": 0,
            "status": "idle",
        });
        if let Some(value) = deleted_at {
            record["deletedAt"] = value;
        }
        AgentRecord::from_value(record).expect("agent record has an id")
    }

    fn rule(id: &str, agent: &str) -> RuleRecord {
        RuleRecord::from_value(json!({"id": id, "agent": agent})).expect("rule record has an id")
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
    fn refuses_rule_with_missing_agent() {
        assert_eq!(
            visible_rules(vec![rule("rule", "missing")], vec![]),
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
