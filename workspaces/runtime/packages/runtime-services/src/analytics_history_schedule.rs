//! Pure bounded scheduling for the caller-owned background history worker.
//! Times are monotonic seconds supplied by its timer. A continuously active
//! backlog yields to one inactive actor after eight imports.
use crate::analytics_capture::CaptureError;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use studio_runtime_store::analytics_capture::CaptureTable;
const MAX_ACTORS: usize = 8192;
const ACTIVE_QUOTA: usize = 8;
const ACTIVE_RECONCILE_SECONDS: f64 = 5.;
const NOTIFIED_RECONCILE_SECONDS: f64 = 60.;
const CPU_SHARE: f64 = 0.15;
#[derive(Clone)]
struct Poll {
    scope: Value,
    due: f64,
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
    pub fn notify(&mut self, agent: &str) {
        if self.dirty.len() < MAX_ACTORS {
            self.dirty.insert(agent.into());
        }
    }
    /// Select one due actor from a checked persisted roster. No wall clock,
    /// filesystem watch, or runtime thread is owned by the decision core.
    pub fn next<'a>(
        &mut self,
        actors: &'a [Value],
        now: f64,
    ) -> Result<Option<&'a Value>, CaptureError> {
        if actors.len() > MAX_ACTORS || !now.is_finite() {
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
            let poll = self.polls.get(id);
            let changed = poll.is_none_or(|p| p.scope != scope(actor));
            let notified = self.dirty.contains(id);
            if !changed && !notified && poll.is_some_and(|p| now < p.due) {
                continue;
            }
            let active = active(actor);
            let priority = if active {
                0
            } else if changed || notified || poll.is_some_and(|p| p.catching_up) {
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
    /// Call after an actual import attempt. A newly registered watch gets one
    /// immediate reread to close the read/watch registration race.
    pub fn completed(
        &mut self,
        agent: &Value,
        checkpoint: &Value,
        now: f64,
        watch_attached: bool,
        watch_registered_now: bool,
    ) -> Result<(), CaptureError> {
        CaptureTable::Agents.validate(agent)?;
        CaptureTable::History.validate(checkpoint)?;
        let id = agent["id"]
            .as_str()
            .ok_or(CaptureError::Invalid("/agent/id"))?;
        if !self.polls.contains_key(id) && self.polls.len() >= MAX_ACTORS {
            return Err(CaptureError::Invalid("/history/polls"));
        }
        let catching_up = checkpoint["status"] == "catchingUp";
        let active = active(agent);
        let due = if catching_up || watch_registered_now {
            now
        } else {
            now + if watch_attached || !active {
                NOTIFIED_RECONCILE_SECONDS
            } else {
                ACTIVE_RECONCILE_SECONDS
            }
        };
        self.polls.insert(
            id.into(),
            Poll {
                scope: scope(agent),
                due,
                checked: now,
                catching_up,
            },
        );
        self.dirty.remove(id);
        self.active_streak = if active {
            self.active_streak.saturating_add(1)
        } else {
            0
        };
        Ok(())
    }
    pub fn delay(&self, now: f64, elapsed: f64, cpu_seconds: f64) -> f64 {
        let due = if !self.dirty.is_empty() {
            0.
        } else {
            self.polls
                .values()
                .map(|p| (p.due - now).max(0.))
                .reduce(f64::min)
                .unwrap_or(ACTIVE_RECONCILE_SECONDS)
        };
        due.max((cpu_seconds.max(0.) / CPU_SHARE - elapsed.max(0.)).max(0.))
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
