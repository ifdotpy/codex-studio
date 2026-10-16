//! Pure bounded scheduling for the caller-owned background history worker.
//! Work is selected after an explicit store or file notification. A recorded
//! partial import remains immediately eligible until its checkpoint is current.
use crate::analytics_capture::CaptureError;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use studio_runtime_store::analytics_capture::CaptureTable;
const MAX_ACTORS: usize = 8192;
const ACTIVE_QUOTA: usize = 8;
#[derive(Clone)]
struct Poll {
    scope: Value,
    checked: f64,
    catching_up: bool,
}
#[derive(Default)]
pub struct HistorySchedule {
    polls: BTreeMap<String, Poll>,
    dirty: BTreeSet<String>,
    active_streak: usize,
}
impl HistorySchedule {
    pub fn notify(&mut self, agent: &str) -> Result<(), CaptureError> {
        if self.dirty.len() >= MAX_ACTORS && !self.dirty.contains(agent) {
            return Err(CaptureError::Invalid("/history/dirty"));
        }
        self.dirty.insert(agent.into());
        Ok(())
    }

    /// Reconcile the watched roster after a durable store notification.
    pub fn refresh(&mut self, actors: &[Value]) -> Result<(), CaptureError> {
        if actors.len() > MAX_ACTORS {
            return Err(CaptureError::Invalid("/history/roster"));
        }
        let mut ids = BTreeSet::new();
        for actor in actors {
            CaptureTable::Agents.validate(actor)?;
            let id = actor["id"]
                .as_str()
                .ok_or(CaptureError::Invalid("/agent/id"))?;
            ids.insert(id.to_owned());
        }
        self.polls.retain(|id, _| ids.contains(id));
        self.dirty.retain(|id| ids.contains(id));
        for actor in actors {
            let id = actor["id"]
                .as_str()
                .ok_or(CaptureError::Invalid("/agent/id"))?;
            if actor["threadId"].as_str().is_none_or(str::is_empty) {
                continue;
            }
            if self
                .polls
                .get(id)
                .is_none_or(|poll| poll.scope != scope(actor))
            {
                self.notify(id)?;
            }
        }
        Ok(())
    }
    /// Select one notified, new, changed, or still-catching-up actor from a
    /// checked roster. The caller owns notification delivery and retry timing.
    pub fn next<'a>(
        &mut self,
        actors: &'a [Value],
        now: f64,
    ) -> Result<Option<&'a Value>, CaptureError> {
        if !now.is_finite() {
            return Err(CaptureError::Invalid("/history/roster"));
        }
        let mut ids = BTreeSet::new();
        let mut due = Vec::new();
        for (index, actor) in actors.iter().enumerate() {
            CaptureTable::Agents.validate(actor)?;
            let id = actor["id"]
                .as_str()
                .ok_or(CaptureError::Invalid("/agent/id"))?;
            ids.insert(id.to_owned());
            if actor["threadId"].as_str().is_none_or(str::is_empty) {
                continue;
            }
            if self
                .polls
                .get(id)
                .is_none_or(|poll| poll.scope != scope(actor))
            {
                self.notify(id)?;
            }
            let poll = self.polls.get(id);
            let notified = self.dirty.contains(id);
            let catching_up = poll.is_some_and(|p| p.catching_up);
            if !notified && !catching_up {
                continue;
            }
            let active = active(actor);
            let priority = if active {
                0
            } else if notified || catching_up {
                1
            } else {
                2
            };
            let checked = if active {
                poll.map_or(0., |p| p.checked)
            } else {
                0.
            };
            due.push((priority, checked, index, active));
        }
        self.polls.retain(|id, _| ids.contains(id));
        self.dirty.retain(|id| ids.contains(id));
        due.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.total_cmp(&b.1))
                .then(a.2.cmp(&b.2))
        });
        let selected = if self.active_streak >= ACTIVE_QUOTA {
            due.iter().find(|v| !v.3).or_else(|| due.first())
        } else {
            due.first()
        };
        Ok(selected.map(|v| &actors[v.2]))
    }
    /// Call after an actual import attempt. Partial checkpoints remain pending
    /// immediately; current checkpoints wait for a new file or store event.
    pub fn completed(
        &mut self,
        agent: &Value,
        checkpoint: &Value,
        now: f64,
    ) -> Result<(), CaptureError> {
        CaptureTable::Agents.validate(agent)?;
        CaptureTable::History.validate(checkpoint)?;
        let id = agent["id"]
            .as_str()
            .ok_or(CaptureError::Invalid("/agent/id"))?;
        let active = active(agent);
        if !self.polls.contains_key(id) && self.polls.len() >= MAX_ACTORS {
            return Err(CaptureError::Invalid("/history/polls"));
        }
        let catching_up = checkpoint["status"] == "catchingUp";
        self.polls.insert(
            id.into(),
            Poll {
                scope: scope(agent),
                checked: now,
                catching_up,
            },
        );
        self.dirty.remove(id);
        if catching_up {
            self.notify(id)?;
        }
        self.active_streak = if active {
            self.active_streak.saturating_add(1)
        } else {
            0
        };
        Ok(())
    }
    pub fn has_pending(&self) -> bool {
        !self.dirty.is_empty() || self.polls.values().any(|poll| poll.catching_up)
    }
}
fn active(a: &Value) -> bool {
    studio_runtime_store::truthy(a.get("inFlight"))
        || matches!(
            a["status"].as_str(),
            Some("queued" | "starting" | "running" | "approval")
        )
}
fn scope(a: &Value) -> Value {
    Value::Array(
        [
            "accountKey",
            "threadId",
            "provider",
            "deletedAt",
            "status",
            "inFlight",
        ]
        .iter()
        .map(|k| a[*k].clone())
        .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn agent(status: &str) -> Value {
        json!({
            "id":"history-agent", "threadId":"thread-1", "accountKey":"default",
            "provider":"codex", "deletedAt":null, "status":status, "inFlight":false
        })
    }

    fn current_checkpoint(status: &str) -> Value {
        json!({"id":"history-agent:default:thread-1", "agent":"history-agent",
            "accountKey":"default", "threadId":"thread-1", "offset":0,
            "importedRecords":0, "malformedLines":0,
            "context":{"threadId":"thread-1"}, "status":status})
    }

    #[test]
    fn current_actor_waits_for_a_notification_without_a_time_deadline() {
        let actor = agent("paused");
        let mut schedule = HistorySchedule::default();
        assert_eq!(
            schedule.next(std::slice::from_ref(&actor), 0.0).unwrap(),
            Some(&actor)
        );
        schedule
            .completed(&actor, &current_checkpoint("current"), 0.0)
            .unwrap();
        assert_eq!(
            schedule
                .next(std::slice::from_ref(&actor), 10_000.0)
                .unwrap(),
            None
        );
        assert!(!schedule.has_pending());
        schedule
            .notify("history-agent")
            .expect("history notification");
        assert_eq!(
            schedule
                .next(std::slice::from_ref(&actor), 10_000.0)
                .unwrap(),
            Some(&actor)
        );
    }

    #[test]
    fn partial_checkpoint_remains_pending_until_caught_up() {
        let actor = agent("running");
        let mut schedule = HistorySchedule::default();
        schedule
            .completed(&actor, &current_checkpoint("catchingUp"), 0.0)
            .unwrap();
        assert_eq!(
            schedule.next(std::slice::from_ref(&actor), 0.0).unwrap(),
            Some(&actor)
        );
        schedule
            .completed(&actor, &current_checkpoint("current"), 1.0)
            .unwrap();
        assert_eq!(
            schedule.next(std::slice::from_ref(&actor), 1.0).unwrap(),
            None
        );
        assert!(!schedule.has_pending());
    }
}
