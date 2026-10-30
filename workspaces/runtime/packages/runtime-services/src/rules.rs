//! Pure filtering for the persisted rules list.

use std::collections::BTreeMap;

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
        deleted_agents.insert(id, agent.is_deleted());
    }

    let mut visible = Vec::with_capacity(rules.len());
    for rule in rules {
        let id = rule.id().as_str().to_owned();
        let agent_id = rule.agent.as_str();
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
        let mut record = json!({"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,
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
        let raw = format!(
            r#"{{"id":{},"agent":{},"epoch":0,"name":"rule","kind":"interval","status":"active","intervalSeconds":60,"nextAt":1,"created":1,"command":"","text":"","checks":0,"wakes":0,"inFlight":false,"fileGeneration":0,"stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"livenessCommand":"","event":""}}"#,
            serde_json::to_string(id).expect("id"),
            serde_json::to_string(agent).expect("agent")
        );
        serde_json::from_str(&raw).expect("rule record")
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
        assert!(
            AgentRecord::from_value(json!({"id":"agent", "rootId":"agent", "epoch":0,
            "status":"idle", "accountKey":"default", "deletedAt":true}))
            .is_err()
        );
    }
}

/// Normalized save input; callers apply their own wire defaults before checking it.
pub struct RuleSaveInput {
    pub kind: String,
    pub minimum: Result<Option<serde_json::Number>, RuleSaveError>,
    pub duration: Result<Option<serde_json::Number>, RuleSaveError>,
    pub interval: Result<i64, RuleSaveError>,
    pub stall: Result<i64, RuleSaveError>,
    pub at: RuleTimeInput,
    pub event: String,
    pub command: Result<String, RuleSaveError>,
    pub liveness: Result<String, RuleSaveError>,
    pub name: Result<String, RuleSaveError>,
    pub text: Result<String, RuleSaveError>,
}

#[derive(Clone, Copy)]
pub struct RuleTime(f64);
impl RuleTime {
    pub fn check(value: f64, now: f64) -> Result<Self, RuleSaveError> {
        if !value.is_finite() || value < now - 1.0 || value > now + 31_536_000.0 {
            Err(RuleSaveError::At)
        } else {
            Ok(Self(value))
        }
    }
}
pub enum RuleTimeInput {
    Checked(RuleTime),
    Unchecked(f64),
}
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum RuleSaveOrder {
    Http,
    Managed,
}

#[derive(Clone, Debug)]
pub enum RuleKind {
    Interval,
    Once,
    File {
        stall: i64,
        liveness: String,
    },
    Event,
    LowWorkers {
        minimum: Option<serde_json::Number>,
        duration: Option<serde_json::Number>,
    },
}
impl RuleKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Interval => "interval",
            Self::Once => "once",
            Self::File { .. } => "file",
            Self::Event => "event",
            Self::LowWorkers { .. } => "low_workers",
        }
    }
    pub fn minimum(&self) -> Option<&serde_json::Number> {
        match self {
            Self::LowWorkers { minimum, .. } => minimum.as_ref(),
            Self::Interval | Self::Once | Self::File { .. } | Self::Event => None,
        }
    }
    pub fn duration(&self) -> Option<&serde_json::Number> {
        match self {
            Self::LowWorkers { duration, .. } => duration.as_ref(),
            Self::Interval | Self::Once | Self::File { .. } | Self::Event => None,
        }
    }
    pub fn stall(&self) -> i64 {
        match self {
            Self::File { stall, .. } => *stall,
            Self::Interval | Self::Once | Self::Event | Self::LowWorkers { .. } => 0,
        }
    }
    pub fn liveness(&self) -> &str {
        match self {
            Self::File { liveness, .. } => liveness,
            Self::Interval | Self::Once | Self::Event | Self::LowWorkers { .. } => "",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuleEvent {
    WorkerCompleted,
    MonitorExit,
    WorkReview,
    Complaint,
}
impl RuleEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkerCompleted => "worker_completed",
            Self::MonitorExit => "monitor_exit",
            Self::WorkReview => "work_review",
            Self::Complaint => "complaint",
        }
    }
}

/// The checked domain shared by HTTP and managed rule writes.
#[derive(Clone, Debug)]
pub struct CheckedRuleSave {
    fields: RuleSaveFields,
}
#[derive(Clone, Debug)]
pub struct RuleSaveFields {
    pub kind: RuleKind,
    pub interval: i64,
    pub at: f64,
    pub event: RuleEvent,
    pub command: String,
    pub name: String,
    pub text: String,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuleSaveError {
    Kind,
    Minimum,
    Duration,
    Interval,
    Stall,
    At,
    Event,
    Command,
    Liveness,
    Name,
    Text,
    Lead,
    Integer,
    InputText,
}
impl CheckedRuleSave {
    pub fn new(
        input: RuleSaveInput,
        now: f64,
        is_lead: bool,
        order: RuleSaveOrder,
    ) -> Result<Self, RuleSaveError> {
        let kind = input.kind.as_str();
        if !matches!(kind, "interval" | "once" | "file" | "event" | "low_workers") {
            return Err(RuleSaveError::Kind);
        }
        let minimum = input.minimum?;
        let duration = input.duration?;
        if order == RuleSaveOrder::Http
            && kind == "low_workers"
            && !input.command.as_ref().map_err(|error| *error)?.is_empty()
        {
            return Err(RuleSaveError::Command);
        }
        if order == RuleSaveOrder::Http
            && kind != "file"
            && !input.liveness.as_ref().map_err(|error| *error)?.is_empty()
        {
            return Err(RuleSaveError::Liveness);
        }
        if kind == "low_workers" {
            if !is_lead {
                return Err(RuleSaveError::Lead);
            }
            if !minimum
                .as_ref()
                .and_then(serde_json::Number::as_f64)
                .is_none_or(|value| (1.0..=255.0).contains(&value))
            {
                return Err(RuleSaveError::Minimum);
            }
            if !duration
                .as_ref()
                .and_then(serde_json::Number::as_f64)
                .is_none_or(|value| (1.0..=525_600.0).contains(&value))
            {
                return Err(RuleSaveError::Duration);
            }
            if order == RuleSaveOrder::Managed
                && !input.command.as_ref().map_err(|error| *error)?.is_empty()
            {
                return Err(RuleSaveError::Command);
            }
        }
        let interval = input.interval?;
        if !(10..=31_536_000).contains(&interval) {
            return Err(RuleSaveError::Interval);
        }
        let stall = input.stall?;
        if !(0..=31_536_000).contains(&stall) {
            return Err(RuleSaveError::Stall);
        }
        let at = match input.at {
            RuleTimeInput::Checked(value) => value,
            RuleTimeInput::Unchecked(value) => RuleTime::check(value, now)?,
        };
        let event = match input.event.as_str() {
            "worker_completed" => RuleEvent::WorkerCompleted,
            "monitor_exit" => RuleEvent::MonitorExit,
            "work_review" => RuleEvent::WorkReview,
            "complaint" => RuleEvent::Complaint,
            _ => return Err(RuleSaveError::Event),
        };
        let command = input.command?;
        let liveness = input.liveness?;
        if order == RuleSaveOrder::Managed && kind != "file" && !liveness.is_empty() {
            return Err(RuleSaveError::Liveness);
        }
        if order == RuleSaveOrder::Http {
            let text = input.text.as_ref().map_err(|error| *error)?;
            if text.is_empty() {
                return Err(RuleSaveError::Text);
            }
        }
        let name = input.name?;
        if name.is_empty() {
            return Err(RuleSaveError::Name);
        }
        let text = input.text?;
        if text.is_empty() {
            return Err(RuleSaveError::Text);
        }
        let kind = match kind {
            "interval" => RuleKind::Interval,
            "once" => RuleKind::Once,
            "file" => RuleKind::File { stall, liveness },
            "event" => RuleKind::Event,
            "low_workers" => RuleKind::LowWorkers { minimum, duration },
            _ => return Err(RuleSaveError::Kind),
        };
        Ok(Self {
            fields: RuleSaveFields {
                kind,
                interval,
                at: at.0,
                event,
                command: command.trim().to_owned(),
                name,
                text,
            },
        })
    }
    pub fn fields(&self) -> &RuleSaveFields {
        &self.fields
    }
}

/// Inputs owned by a single rule save, after ingress validation.
#[derive(Clone, Copy)]
pub struct RuleRecordContext<'a> {
    pub id: &'a str,
    pub actor: &'a AgentRecord,
    pub previous: Option<&'a RuleRecord>,
    pub path: Option<&'a str>,
    pub fingerprint: Option<(i64, u64, u64)>,
    pub now: f64,
}

/// Construct the persisted record shared by HTTP and managed rule saves.
pub fn persisted_rule(
    checked: &CheckedRuleSave,
    context: RuleRecordContext<'_>,
) -> Result<RuleRecord, RuleSaveError> {
    use studio_runtime_schema::rule_record::{RuleKind as StoredKind, RuleStatus};
    let fields = checked.fields();
    let kind = match &fields.kind {
        RuleKind::Interval => StoredKind::Interval,
        RuleKind::Once => StoredKind::Once,
        RuleKind::Event => StoredKind::Event,
        RuleKind::File { .. } => StoredKind::File {
            path: context.path.ok_or(RuleSaveError::InputText)?.to_owned(),
        },
        RuleKind::LowWorkers { minimum, duration } => StoredKind::LowWorkers {
            minimum_workers: rule_bound(minimum.as_ref(), RuleSaveError::Minimum)?,
            duration_minutes: rule_bound(duration.as_ref(), RuleSaveError::Duration)?,
        },
    };
    let low_workers = matches!(kind, StoredKind::LowWorkers { .. });
    let now = serde_json::Number::from_f64(context.now).ok_or(RuleSaveError::At)?;
    let previous = context.previous;
    Ok(RuleRecord {
        id: context.id.to_owned(),
        agent: context.actor.id.0.clone(),
        root_id: Some(context.actor.root_id.0.clone()),
        epoch: context
            .actor
            .epoch
            .try_into()
            .map_err(|_| RuleSaveError::Integer)?,
        name: fields
            .name
            .clone()
            .try_into()
            .map_err(|_| RuleSaveError::Name)?,
        kind,
        interval_seconds: fields
            .interval
            .try_into()
            .map_err(|_| RuleSaveError::Interval)?,
        next_at: serde_json::Number::from_f64(fields.at).ok_or(RuleSaveError::At)?,
        at: Some(serde_json::Number::from_f64(fields.at).ok_or(RuleSaveError::At)?),
        event: fields.event.as_str().to_owned(),
        command: fields
            .command
            .clone()
            .try_into()
            .map_err(|_| RuleSaveError::Command)?,
        stall_timeout_seconds: fields
            .kind
            .stall()
            .try_into()
            .map_err(|_| RuleSaveError::Stall)?,
        liveness_command: fields
            .kind
            .liveness()
            .to_owned()
            .try_into()
            .map_err(|_| RuleSaveError::Liveness)?,
        file_activity_at: Some(
            previous
                .and_then(|r| r.file_activity_at.clone())
                .unwrap_or(now.clone()),
        ),
        file_generation: previous
            .map(|r| r.file_generation)
            .unwrap_or(0_i64.try_into().map_err(|_| RuleSaveError::Integer)?),
        stall_wake_generation: previous
            .map(|r| r.stall_wake_generation)
            .unwrap_or((-1_i64).try_into().map_err(|_| RuleSaveError::Integer)?),
        text: fields.text.clone(),
        status: if context.actor.auto_wake == Some(true) {
            RuleStatus::Active
        } else {
            RuleStatus::Paused
        },
        created: previous.map(|r| r.created.clone()).unwrap_or(now),
        in_flight: false,
        checks: previous
            .map(|r| r.checks)
            .unwrap_or(0_i64.try_into().map_err(|_| RuleSaveError::Integer)?),
        wakes: previous
            .map(|r| r.wakes)
            .unwrap_or(0_i64.try_into().map_err(|_| RuleSaveError::Integer)?),
        fingerprint: if matches!(&fields.kind, RuleKind::File { .. }) {
            context.fingerprint
        } else {
            None
        },
        low_since: None,
        alerted: low_workers.then_some(false),
        active_workers: None,
        error: None,
        event_text: None,
        last_at: None,
        last_event: None,
        last_exit_code: None,
        last_finished: None,
        last_output: None,
        last_stall_error: None,
        last_stall_exit_code: None,
        last_stall_finished: None,
        launch_intent: None,
        owner_hold: None,
        restart_check: None,
        stall_event_key: None,
        stall_probe: None,
        stall_text: None,
    })
}
fn rule_bound<const MIN: i64, const MAX: u64>(
    value: Option<&serde_json::Number>,
    error: RuleSaveError,
) -> Result<studio_runtime_schema::bounded_integer::BoundedInteger<MIN, MAX>, RuleSaveError> {
    let value = value
        .and_then(serde_json::Number::as_f64)
        .filter(|v| v.fract() == 0.0)
        .ok_or(error)?;
    (value as i64).try_into().map_err(|_| error)
}
