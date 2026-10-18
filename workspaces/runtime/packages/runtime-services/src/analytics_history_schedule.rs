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
    blocked_input: Option<BlockedInput>,
}
#[derive(Default)]
pub struct HistorySchedule {
    polls: BTreeMap<String, Poll>,
    dirty: BTreeSet<String>,
    budget_blocked: BTreeSet<String>,
    profile_blocked: BTreeMap<String, String>,
    active_streak: usize,
}
impl HistorySchedule {
    pub fn notify(&mut self, agent: &str) -> Result<(), CaptureError> {
        if self.dirty.len() >= MAX_ACTORS && !self.dirty.contains(agent) {
            return Err(CaptureError::Invalid("/history/dirty"));
        }
        if let Some(poll) = self.polls.get_mut(agent) {
            poll.blocked_input = None;
        }
        self.budget_blocked.remove(agent);
        self.profile_blocked.remove(agent);
        self.dirty.insert(agent.into());
        Ok(())
    }

    /// Rollout data changes can repair budget input, while only directory
    /// structure changes can alter a cached discovery entry bound.
    pub fn notify_rollout(&mut self, agent: &str, structural: bool) -> Result<(), CaptureError> {
        if !structural && self.profile_blocked.contains_key(agent) {
            return Ok(());
        }
        self.notify(agent)
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
        self.budget_blocked.retain(|id| ids.contains(id));
        self.profile_blocked.retain(|id, _| ids.contains(id));
        for actor in actors {
            let id = actor["id"]
                .as_str()
                .ok_or(CaptureError::Invalid("/agent/id"))?;
            if actor["threadId"].as_str().is_none_or(str::is_empty) {
                continue;
            }
            let poll_changed = poll_changed(self.polls.get(id), actor);
            if poll_changed {
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
            let poll_changed = poll_changed(self.polls.get(id), actor);
            if poll_changed {
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
                blocked_input: None,
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

    /// Suppress retries for a deterministic input failure until a relevant
    /// input signature changes or an explicit file/profile notification arrives.
    pub fn block_for_input_change(&mut self, agent: &Value, now: f64) -> Result<(), CaptureError> {
        self.block_for_change(agent, now, false)
    }

    /// Suppress budget-invalid actors until an actual runtime_budget commit or
    /// a change to that actor's durable input signature arrives.
    pub fn block_for_budget_change(&mut self, agent: &Value, now: f64) -> Result<(), CaptureError> {
        self.block_for_change(agent, now, true)
    }

    fn block_for_change(
        &mut self,
        agent: &Value,
        now: f64,
        budget: bool,
    ) -> Result<(), CaptureError> {
        CaptureTable::Agents.validate(agent)?;
        let id = agent["id"]
            .as_str()
            .ok_or(CaptureError::Invalid("/agent/id"))?;
        if !self.polls.contains_key(id) && self.polls.len() >= MAX_ACTORS {
            return Err(CaptureError::Invalid("/history/polls"));
        }
        self.polls.insert(
            id.to_owned(),
            Poll {
                scope: scope(agent),
                checked: now,
                catching_up: false,
                blocked_input: Some(BlockedInput::from_agent(agent)),
            },
        );
        self.dirty.remove(id);
        if budget {
            self.budget_blocked.insert(id.to_owned());
            self.profile_blocked.remove(id);
        } else {
            self.budget_blocked.remove(id);
            self.profile_blocked.insert(
                id.to_owned(),
                agent["accountKey"].as_str().unwrap_or("default").to_owned(),
            );
        }
        Ok(())
    }

    /// Resume only budget-invalid actors after the store reports a committed
    /// runtime_budget row change. This touches the bounded blocked set and does
    /// not need a roster read.
    pub fn budget_changed(&mut self) {
        for id in std::mem::take(&mut self.budget_blocked) {
            if let Some(poll) = self.polls.get_mut(&id) {
                poll.blocked_input = None;
                self.dirty.insert(id);
            }
        }
    }

    /// Resume profile-bound failures after a real profile tree event. The
    /// blocked set is bounded by the validated roster and needs no roster read.
    pub fn discovery_changed(&mut self, account: Option<&str>) {
        let resumed = self
            .profile_blocked
            .iter()
            .filter(|(_, blocked_account)| {
                account.is_none_or(|account| blocked_account == &account)
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in resumed {
            self.profile_blocked.remove(&id);
            if let Some(poll) = self.polls.get_mut(&id) {
                poll.blocked_input = None;
                self.dirty.insert(id);
            }
        }
    }

    /// True when the last import of this agent is final and its inputs have not changed.
    pub fn settled(&self, agent: &Value) -> bool {
        let Some(id) = agent["id"].as_str() else {
            return false;
        };
        self.polls.get(id).is_some_and(|poll| {
            !poll.catching_up
                && poll.blocked_input.is_none()
                && !self.dirty.contains(id)
                && !poll_changed(Some(poll), agent)
        })
    }

    pub fn has_pending(&self) -> bool {
        !self.dirty.is_empty() || self.polls.values().any(|poll| poll.catching_up)
    }
}

fn poll_changed(poll: Option<&Poll>, agent: &Value) -> bool {
    poll.is_none_or(|poll| {
        if let Some(blocked) = &poll.blocked_input {
            blocked.input_changed(agent)
        } else {
            poll.scope != scope(agent)
        }
    })
}

#[derive(Clone)]
struct BlockedInput {
    id: String,
    root_id: Option<String>,
    account_key: Option<String>,
    thread_id: Option<String>,
    epoch: Option<i64>,
    turn_id: Option<String>,
    last_completed_turn: Option<Value>,
    tokens_used: Option<Value>,
    token_usage_accounting: Option<Value>,
    token_budget: Option<Value>,
    created: Option<Value>,
    provider: Option<Value>,
    deleted_at: Option<Value>,
}

impl BlockedInput {
    fn from_agent(agent: &Value) -> Self {
        Self {
            id: agent["id"].as_str().unwrap_or_default().to_owned(),
            root_id: agent["rootId"].as_str().map(str::to_owned),
            account_key: agent["accountKey"].as_str().map(str::to_owned),
            thread_id: agent["threadId"].as_str().map(str::to_owned),
            epoch: agent["epoch"].as_i64(),
            turn_id: agent["turnId"].as_str().map(str::to_owned),
            last_completed_turn: present_value(agent, "lastCompletedTurn"),
            tokens_used: present_value(agent, "tokensUsed"),
            token_usage_accounting: present_value(agent, "tokenUsageAccounting"),
            token_budget: present_value(agent, "tokenBudget"),
            created: present_value(agent, "created"),
            provider: present_value(agent, "provider"),
            deleted_at: present_value(agent, "deletedAt"),
        }
    }

    fn input_changed(&self, agent: &Value) -> bool {
        self.id != agent["id"].as_str().unwrap_or_default()
            || self.root_id.as_deref() != agent["rootId"].as_str()
            || self.account_key.as_deref() != agent["accountKey"].as_str()
            || self.thread_id.as_deref() != agent["threadId"].as_str()
            || self.epoch != agent["epoch"].as_i64()
            || self.turn_id.as_deref() != agent["turnId"].as_str()
            || !same_optional_value(&self.last_completed_turn, agent.get("lastCompletedTurn"))
            || !same_optional_value(&self.tokens_used, agent.get("tokensUsed"))
            || !same_optional_value(
                &self.token_usage_accounting,
                agent.get("tokenUsageAccounting"),
            )
            || !same_optional_value(&self.token_budget, agent.get("tokenBudget"))
            || !same_optional_value(&self.created, agent.get("created"))
            || !same_optional_value(&self.provider, agent.get("provider"))
            || !same_optional_value(&self.deleted_at, agent.get("deletedAt"))
    }
}

fn present_value(agent: &Value, key: &str) -> Option<Value> {
    agent.get(key).filter(|value| !value.is_null()).cloned()
}

fn same_optional_value(expected: &Option<Value>, actual: Option<&Value>) -> bool {
    expected.as_ref() == actual.filter(|value| !value.is_null())
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

    #[test]
    fn blocked_input_retains_null_to_empty_and_zero_distinctions() {
        let mut actor = agent("paused");
        actor["rootId"] = Value::Null;
        actor["epoch"] = Value::Null;
        let blocked = BlockedInput::from_agent(&actor);
        actor["rootId"] = json!("");
        actor["epoch"] = json!(0);
        assert!(blocked.input_changed(&actor));
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
            schedule
                .next(std::slice::from_ref(&actor), 0.0)
                .expect("initial schedule selection"),
            Some(&actor)
        );
        schedule
            .completed(&actor, &current_checkpoint("current"), 0.0)
            .expect("complete initial history import");
        assert_eq!(
            schedule
                .next(std::slice::from_ref(&actor), 10_000.0)
                .expect("current actor should wait for notification"),
            None
        );
        assert!(!schedule.has_pending());
        schedule
            .notify("history-agent")
            .expect("history notification");
        assert_eq!(
            schedule
                .next(std::slice::from_ref(&actor), 10_000.0)
                .expect("notification should schedule actor"),
            Some(&actor)
        );
    }

    #[test]
    fn partial_checkpoint_remains_pending_until_caught_up() {
        let actor = agent("running");
        let mut schedule = HistorySchedule::default();
        schedule
            .completed(&actor, &current_checkpoint("catchingUp"), 0.0)
            .expect("record partial history import");
        assert_eq!(
            schedule
                .next(std::slice::from_ref(&actor), 0.0)
                .expect("partial actor remains scheduled"),
            Some(&actor)
        );
        schedule
            .completed(&actor, &current_checkpoint("current"), 1.0)
            .expect("complete caught-up history import");
        assert_eq!(
            schedule
                .next(std::slice::from_ref(&actor), 1.0)
                .expect("caught-up actor should leave the schedule"),
            None
        );
        assert!(!schedule.has_pending());
    }

    #[test]
    fn blocked_actor_waits_for_relevant_input_while_other_actors_progress() {
        let blocked = agent("paused");
        let mut healthy = agent("paused");
        healthy["id"] = json!("healthy-agent");
        healthy["threadId"] = json!("healthy-thread");
        let roster = vec![blocked.clone(), healthy.clone()];
        let mut schedule = HistorySchedule::default();
        schedule.refresh(&roster).expect("initial roster");

        assert_eq!(
            schedule.next(&roster, 0.0).expect("initial selection"),
            Some(&blocked)
        );
        schedule
            .block_for_input_change(&blocked, 1.0)
            .expect("block only invalid actor");
        assert_eq!(
            schedule
                .next(&roster, 1.0)
                .expect("healthy actor selection"),
            Some(&healthy)
        );
        schedule
            .completed(&healthy, &checkpoint_for(&healthy, "current"), 2.0)
            .expect("healthy actor completes");
        assert!(!schedule.has_pending(), "blocked actor has no timer work");

        let mut unrelated_change = roster.clone();
        unrelated_change[1]["tokensUsed"] = json!(42);
        schedule
            .refresh(&unrelated_change)
            .expect("unrelated actor update");
        assert!(!schedule.has_pending(), "unrelated actor does not unblock");
        let mut status_only = unrelated_change.clone();
        status_only[0]["status"] = json!("running");
        status_only[0]["inFlight"] = json!(true);
        schedule.refresh(&status_only).expect("status-only update");
        assert!(!schedule.has_pending(), "status changes do not unblock");

        let mut repaired = status_only;
        repaired[0]["tokenBudget"] = json!(1000);
        schedule.refresh(&repaired).expect("relevant budget repair");
        assert!(schedule.has_pending());
        assert_eq!(
            schedule
                .next(&repaired, 3.0)
                .expect("repaired actor selection"),
            Some(&repaired[0])
        );

        schedule
            .block_for_input_change(&repaired[0], 4.0)
            .expect("block after another permanent failure");
        let mut new_epoch = repaired;
        new_epoch[0]["epoch"] = json!(2);
        new_epoch[0]["turnId"] = json!("turn-2");
        schedule.refresh(&new_epoch).expect("new turn identity");
        assert!(schedule.has_pending(), "epoch and turn changes resume work");
    }

    #[test]
    fn rollout_data_keeps_discovery_bound_blocked_until_structure_changes() {
        let actor = agent("paused");
        let mut schedule = HistorySchedule::default();
        schedule
            .refresh(std::slice::from_ref(&actor))
            .expect("roster");
        schedule
            .block_for_input_change(&actor, 1.0)
            .expect("block invalid actor");
        assert!(!schedule.has_pending());

        schedule
            .notify_rollout("history-agent", false)
            .expect("rollout data notification");
        assert!(
            !schedule.has_pending(),
            "data append cannot change entry count"
        );
        schedule
            .notify_rollout("history-agent", true)
            .expect("rollout structure notification");
        assert!(schedule.has_pending());
        assert_eq!(
            schedule
                .next(std::slice::from_ref(&actor), 2.0)
                .expect("file event resumes actor"),
            Some(&actor)
        );
    }

    #[test]
    fn budget_commit_unblocks_only_budget_invalid_actors() {
        let budget_actor = agent("paused");
        let mut discovery_actor = agent("paused");
        discovery_actor["id"] = json!("discovery-agent");
        discovery_actor["threadId"] = json!("discovery-thread");
        let roster = vec![budget_actor.clone(), discovery_actor.clone()];
        let mut schedule = HistorySchedule::default();
        schedule.refresh(&roster).expect("initial roster");
        schedule
            .block_for_budget_change(&budget_actor, 1.0)
            .expect("block budget-invalid actor");
        schedule
            .block_for_input_change(&discovery_actor, 1.0)
            .expect("block discovery-limited actor");
        assert!(!schedule.has_pending());

        schedule.budget_changed();
        assert!(schedule.has_pending());
        assert_eq!(
            schedule.next(&roster, 2.0).expect("budget actor resumes"),
            Some(&budget_actor)
        );
        schedule
            .completed(
                &budget_actor,
                &checkpoint_for(&budget_actor, "current"),
                3.0,
            )
            .expect("budget actor completes");
        assert!(
            !schedule.has_pending(),
            "a budget commit does not retry discovery-blocked actors"
        );
    }

    #[test]
    fn profile_tree_change_unblocks_discovery_limits_without_roster_changes() {
        let actor = agent("paused");
        let mut unrelated = agent("paused");
        unrelated["id"] = json!("unrelated-profile-agent");
        unrelated["accountKey"] = json!("other-account");
        unrelated["threadId"] = json!("other-thread");
        let roster = vec![actor.clone(), unrelated.clone()];
        let mut schedule = HistorySchedule::default();
        schedule.refresh(&roster).expect("roster");
        schedule
            .block_for_input_change(&actor, 1.0)
            .expect("block discovery-limited actor");
        schedule
            .block_for_input_change(&unrelated, 1.0)
            .expect("block unrelated profile");
        assert!(!schedule.has_pending());

        schedule.discovery_changed(Some("default"));
        assert_eq!(
            schedule
                .next(&roster, 2.0)
                .expect("changed profile resumes"),
            Some(&actor)
        );
        schedule
            .completed(&actor, &checkpoint_for(&actor, "current"), 3.0)
            .expect("changed profile actor completes");
        assert!(!schedule.has_pending(), "unrelated profile remains blocked");

        schedule.discovery_changed(Some("other-account"));
        assert_eq!(
            schedule.next(&roster, 4.0).expect("other profile resumes"),
            Some(&unrelated)
        );
    }

    fn checkpoint_for(agent: &Value, status: &str) -> Value {
        let id = agent["id"].as_str().expect("fixture agent id");
        let account = agent["accountKey"].as_str().expect("fixture account key");
        let thread = agent["threadId"].as_str().expect("fixture thread id");
        json!({
            "id": format!("{id}:{account}:{thread}"),
            "agent": agent["id"], "accountKey": agent["accountKey"],
            "threadId": agent["threadId"], "offset": 0,
            "importedRecords": 0, "malformedLines": 0,
            "context": {"threadId": agent["threadId"]}, "status": status
        })
    }
}
