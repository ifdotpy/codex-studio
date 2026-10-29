//! Pure filtering for the persisted rules list.

use std::collections::BTreeMap;

use serde_json::Value;
use studio_runtime_store::{AgentRecord, RuleRecord};
use thiserror::Error;

/// Names accepted at rule action ingress. The HTTP schema is checked against
/// this owner by the rules contract test.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuleActionName {
    Save,
    Pause,
    Resume,
    Delete,
}

impl RuleActionName {
    pub const ALL: [Self; 4] = [Self::Save, Self::Pause, Self::Resume, Self::Delete];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Save => "save",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Delete => "delete",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|action| action.as_str() == value)
    }
}

/// A rule mutation and its target. Control actions have no save fields; each
/// ingress supplies its save payload.
#[derive(Clone, Debug)]
pub enum RuleAction<T> {
    Save { id: String, data: T },
    Pause { id: String },
    Resume { id: String },
    Delete { id: String },
}

impl<T> RuleAction<T> {
    pub fn id(&self) -> &str {
        match self {
            Self::Save { id, .. }
            | Self::Pause { id }
            | Self::Resume { id }
            | Self::Delete { id } => id,
        }
    }
}

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
    use super::{RuleActionName, RulesError, visible_rules};
    use serde_json::json;
    use studio_runtime_store::{AgentRecord, RuleRecord};

    #[test]
    fn rule_action_names_match_the_http_wire_contract() {
        let openapi: serde_json::Value =
            serde_json::from_str(include_str!("../../runtime-http/openapi.json"))
                .expect("HTTP contract parses");
        let wire = openapi["components"]["schemas"]["RuleBody"]["properties"]["action"]["enum"]
            .as_array()
            .expect("action enum");
        let wire: std::collections::BTreeSet<_> = wire
            .iter()
            .map(|value| value.as_str().expect("action name"))
            .collect();
        let owned: std::collections::BTreeSet<_> = RuleActionName::ALL
            .into_iter()
            .map(RuleActionName::as_str)
            .collect();
        assert_eq!(wire, owned);
        for action in RuleActionName::ALL {
            assert_eq!(RuleActionName::parse(action.as_str()), Some(action));
        }
        assert_eq!(RuleActionName::parse("unknown"), None);
    }

    fn agent(id: &str, deleted_at: Option<serde_json::Value>) -> AgentRecord {
        let mut record = json!({"provider":studio_runtime_schema::ProviderKind::Codex,
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
            vec![
                rule("z-first", "live"),
                rule("deleted", "deleted"),
                rule("a-last", "live"),
            ],
            vec![agent("live", None), agent("deleted", Some(json!(1)))],
        )
        .expect("valid stored records filter");
        assert_eq!(
            result,
            vec![rule("z-first", "live"), rule("a-last", "live")]
        );
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
