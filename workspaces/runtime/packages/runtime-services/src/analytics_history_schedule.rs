//! Event-driven scheduling over a retained, checked history roster.
use crate::analytics_capture::CaptureError;
use serde::Deserialize;
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use studio_runtime_store::analytics_capture::CaptureTable;
/// Maximum retained actors and pending notifications, in actors.
const MAX_ACTORS: usize = 8192;
/// Consecutive active imports before an inactive actor gets a turn, in imports.
const ACTIVE_QUOTA: usize = 8;

// AnalyticsAgent permits arbitrary JSON for these extra fields. Keep that
// contract while parsing its named, schema-constrained fields once at ingestion.
#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Scope {
    account_key: Option<String>,
    thread_id: Option<String>,
    #[serde(default)]
    provider: Value,
    #[serde(default)]
    deleted_at: Value,
    #[serde(default)]
    status: Value,
    #[serde(default)]
    in_flight: Value,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActorInput {
    id: String,
    #[serde(flatten)]
    scope: Scope,
    root_id: Option<String>,
    epoch: Option<i64>,
    turn_id: Option<String>,
    #[serde(default)]
    last_completed_turn: Value,
    #[serde(default)]
    tokens_used: Value,
    #[serde(default)]
    token_usage_accounting: Value,
    #[serde(default)]
    token_budget: Value,
    #[serde(default)]
    created: Value,
}
impl ActorInput {
    fn active(&self) -> bool {
        studio_runtime_store::truthy(Some(&self.scope.in_flight))
            || matches!(
                self.scope.status.as_str(),
                Some("queued" | "starting" | "running" | "approval")
            )
    }
    fn has_thread(&self) -> bool {
        self.scope
            .thread_id
            .as_deref()
            .is_some_and(|thread| !thread.is_empty())
    }
}

/// Only ingestion constructs checked actors; selection has no unchecked roster.
struct CheckedActor {
    input: ActorInput,
    order: i64,
    ready: Option<ReadyKey>,
}
impl CheckedActor {
    fn new(value: &Value, order: i64) -> Result<Self, CaptureError> {
        validate_actor(value)?;
        let input =
            ActorInput::deserialize(value).map_err(|_| CaptureError::Invalid("/history/roster"))?;
        #[cfg(test)]
        SCHEDULE_WORK.with(|work| work.borrow_mut().scopes += 1);
        Ok(Self {
            input,
            order,
            ready: None,
        })
    }
}

#[derive(Clone)]
struct Poll {
    scope: Scope,
    checked: f64,
    catching_up: bool,
    blocked_input: Option<BlockedInput>,
}
#[derive(Clone, Debug)]
struct ReadyKey {
    checked: f64,
    order: i64,
    id: String,
}
impl Ord for ReadyKey {
    fn cmp(&self, other: &Self) -> Ordering {
        #[cfg(test)]
        SCHEDULE_WORK.with(|work| work.borrow_mut().comparisons += 1);
        self.checked
            .total_cmp(&other.checked)
            .then(self.order.cmp(&other.order))
            .then(self.id.cmp(&other.id))
    }
}
impl PartialOrd for ReadyKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for ReadyKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for ReadyKey {}

#[derive(Clone, Copy)]
enum BlockReason {
    ProfileInput,
    BudgetInput,
}

#[derive(Default)]
pub struct HistorySchedule {
    actors: BTreeMap<String, CheckedActor>,
    rows: BTreeMap<i64, String>,
    active_ready: BTreeSet<ReadyKey>,
    inactive_ready: BTreeSet<ReadyKey>,
    polls: BTreeMap<String, Poll>,
    dirty: BTreeSet<String>,
    budget_blocked: BTreeSet<String>,
    profile_blocked: BTreeMap<String, String>,
    active_streak: usize,
}
impl HistorySchedule {
    fn sync_ready(&mut self, id: &str) {
        let Some(actor) = self.actors.get_mut(id) else {
            return;
        };
        if let Some(previous) = actor.ready.take() {
            self.active_ready.remove(&previous);
            self.inactive_ready.remove(&previous);
        }
        let poll = self.polls.get(id);
        if !actor.input.has_thread()
            || !(self.dirty.contains(id) || poll.is_some_and(|poll| poll.catching_up))
        {
            return;
        }
        let active = actor.input.active();
        let key = ReadyKey {
            checked: if active {
                poll.map_or(0., |poll| poll.checked)
            } else {
                0.
            },
            order: actor.order,
            id: id.to_owned(),
        };
        if active {
            self.active_ready.insert(key.clone());
        } else {
            self.inactive_ready.insert(key.clone());
        }
        actor.ready = Some(key);
    }

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
        self.sync_ready(agent);
        Ok(())
    }
    pub fn notify_rollout(&mut self, agent: &str, structural: bool) -> Result<(), CaptureError> {
        if !structural && self.profile_blocked.contains_key(agent) {
            return Ok(());
        }
        self.notify(agent)
    }

    /// Replace a full roster in caller order after an initial read or overflow.
    pub fn refresh(&mut self, actors: &[Value]) -> Result<(), CaptureError> {
        let row_ids = (0..actors.len())
            .map(|index| i64::try_from(index).map_err(|_| CaptureError::Invalid("/history/roster")))
            .collect::<Result<Vec<_>, _>>()?;
        self.refresh_ordered(&row_ids, actors)
    }

    /// Replace a full store roster, retaining durable rowid tie ordering.
    pub fn refresh_ordered(
        &mut self,
        row_ids: &[i64],
        actors: &[Value],
    ) -> Result<(), CaptureError> {
        if actors.len() > MAX_ACTORS || row_ids.len() != actors.len() {
            return Err(CaptureError::Invalid("/history/roster"));
        }
        // Check all changed inputs before mutating retained state.
        let checked = row_ids
            .iter()
            .zip(actors)
            .map(|(&row, value)| CheckedActor::new(value, row))
            .collect::<Result<Vec<_>, _>>()?;
        let ids = checked
            .iter()
            .map(|actor| actor.input.id.as_str())
            .collect::<BTreeSet<_>>();
        let removed = self
            .actors
            .keys()
            .filter(|id| !ids.contains(id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        for id in removed {
            self.remove(&id);
        }
        self.dirty.retain(|id| ids.contains(id.as_str()));
        self.rows.clear();
        for actor in checked {
            self.install(actor)?;
        }
        Ok(())
    }

    /// Apply only notified store rows; unchanged retained actors are never parsed.
    pub fn update_rows(&mut self, changes: &[(i64, Option<Value>)]) -> Result<(), CaptureError> {
        let checked = changes
            .iter()
            .map(|(row, value)| {
                value
                    .as_ref()
                    .map(|value| CheckedActor::new(value, *row))
                    .transpose()
            })
            .collect::<Result<Vec<_>, _>>()?;
        let incoming = checked
            .iter()
            .flatten()
            .map(|actor| actor.input.id.as_str())
            .collect::<BTreeSet<_>>();
        let removed = changes
            .iter()
            .filter_map(|(row, _)| self.rows.get(row))
            .filter(|id| !incoming.contains(id.as_str()))
            .cloned()
            .collect::<BTreeSet<_>>();
        let added = incoming
            .iter()
            .filter(|id| !self.actors.contains_key(**id))
            .count();
        if self.actors.len() - removed.len() + added > MAX_ACTORS {
            return Err(CaptureError::Invalid("/history/roster"));
        }
        for id in removed {
            self.remove(&id);
        }
        for (row, _) in changes {
            self.rows.remove(row);
        }
        for actor in checked.into_iter().flatten() {
            self.install(actor)?;
        }
        Ok(())
    }

    fn install(&mut self, checked: CheckedActor) -> Result<(), CaptureError> {
        let id = checked.input.id.clone();
        let row = checked.order;
        if let Some(previous_id) = self
            .rows
            .get(&row)
            .filter(|previous| **previous != id)
            .cloned()
        {
            self.remove(&previous_id);
        }
        if let Some(previous) = self.actors.get_mut(&id) {
            if self.rows.get(&previous.order) == Some(&id) {
                self.rows.remove(&previous.order);
            }
            if let Some(key) = previous.ready.take() {
                self.active_ready.remove(&key);
                self.inactive_ready.remove(&key);
            }
        }
        self.actors.insert(id.clone(), checked);
        self.rows.insert(row, id.clone());
        if self.actors.get(&id).is_some_and(|actor| {
            actor.input.has_thread() && poll_changed(self.polls.get(&id), &actor.input)
        }) {
            self.notify(&id)?;
        } else {
            self.sync_ready(&id);
        }
        Ok(())
    }

    /// Forget a removed or retired actor and all its notification/block state.
    pub fn remove(&mut self, id: &str) {
        if let Some(actor) = self.actors.remove(id) {
            self.rows.remove(&actor.order);
            if let Some(key) = actor.ready {
                self.active_ready.remove(&key);
                self.inactive_ready.remove(&key);
            }
        }
        self.polls.remove(id);
        self.dirty.remove(id);
        self.budget_blocked.remove(id);
        self.profile_blocked.remove(id);
    }

    /// Select from checked ready sets without scanning or parsing the roster.
    pub fn next(&self, now: f64) -> Result<Option<i64>, CaptureError> {
        if !now.is_finite() {
            return Err(CaptureError::Invalid("/history/roster"));
        }
        let selected = if self.active_streak >= ACTIVE_QUOTA {
            self.inactive_ready
                .first()
                .or_else(|| self.active_ready.first())
        } else {
            self.active_ready
                .first()
                .or_else(|| self.inactive_ready.first())
        };
        Ok(selected.map(|key| key.order))
    }

    pub fn completed(
        &mut self,
        agent: &Value,
        checkpoint: &Value,
        now: f64,
    ) -> Result<(), CaptureError> {
        validate_actor(agent)?;
        CaptureTable::History.validate(checkpoint)?;
        let id = actor_id(agent)?;
        let actor = self
            .actors
            .get(id)
            .ok_or(CaptureError::Invalid("/history/roster"))?;
        let active = actor.input.active();
        let catching_up = checkpoint["status"] == "catchingUp";
        self.polls.insert(
            id.into(),
            Poll {
                scope: snapshot_scope(&actor.input.scope),
                checked: now,
                catching_up,
                blocked_input: None,
            },
        );
        self.dirty.remove(id);
        if catching_up {
            self.notify(id)?;
        } else {
            self.sync_ready(id);
        }
        self.active_streak = if active {
            self.active_streak.saturating_add(1)
        } else {
            0
        };
        Ok(())
    }
    pub fn block_for_input_change(&mut self, agent: &Value, now: f64) -> Result<(), CaptureError> {
        self.block_for_change(agent, now, BlockReason::ProfileInput)
    }
    pub fn block_for_budget_change(&mut self, agent: &Value, now: f64) -> Result<(), CaptureError> {
        self.block_for_change(agent, now, BlockReason::BudgetInput)
    }
    fn block_for_change(
        &mut self,
        agent: &Value,
        now: f64,
        reason: BlockReason,
    ) -> Result<(), CaptureError> {
        validate_actor(agent)?;
        let id = actor_id(agent)?;
        let actor = self
            .actors
            .get(id)
            .ok_or(CaptureError::Invalid("/history/roster"))?;
        let account = actor
            .input
            .scope
            .account_key
            .clone()
            .unwrap_or_else(|| "default".to_owned());
        self.polls.insert(
            id.into(),
            Poll {
                scope: snapshot_scope(&actor.input.scope),
                checked: now,
                catching_up: false,
                blocked_input: Some(BlockedInput::from_input(&actor.input)),
            },
        );
        self.dirty.remove(id);
        if matches!(reason, BlockReason::BudgetInput) {
            self.budget_blocked.insert(id.into());
            self.profile_blocked.remove(id);
        } else {
            self.budget_blocked.remove(id);
            self.profile_blocked.insert(id.into(), account);
        }
        self.sync_ready(id);
        Ok(())
    }
    pub fn budget_changed(&mut self) {
        for id in std::mem::take(&mut self.budget_blocked) {
            if let Some(poll) = self.polls.get_mut(&id) {
                poll.blocked_input = None;
                self.dirty.insert(id.clone());
                self.sync_ready(&id);
            }
        }
    }
    pub fn discovery_changed(&mut self, account: Option<&str>) {
        let resumed = self
            .profile_blocked
            .iter()
            .filter(|(_, blocked)| account.is_none_or(|account| blocked == &account))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in resumed {
            self.profile_blocked.remove(&id);
            if let Some(poll) = self.polls.get_mut(&id) {
                poll.blocked_input = None;
                self.dirty.insert(id.clone());
                self.sync_ready(&id);
            }
        }
    }
    pub fn settled(&self, agent: &Value) -> bool {
        let Ok(id) = actor_id(agent) else {
            return false;
        };
        self.polls.get(id).is_some_and(|poll| {
            !poll.catching_up
                && poll.blocked_input.is_none()
                && !self.dirty.contains(id)
                && supplied_scope_matches(&poll.scope, agent)
        })
    }

    pub fn has_pending(&self) -> bool {
        !self.active_ready.is_empty() || !self.inactive_ready.is_empty()
    }
    pub(crate) fn roster_len(&self) -> usize {
        self.actors.len()
    }
}

fn actor_id(actor: &Value) -> Result<&str, CaptureError> {
    #[derive(Deserialize)]
    struct Id<'a> {
        id: &'a str,
    }
    Id::deserialize(actor)
        .map(|actor| actor.id)
        .map_err(|_| CaptureError::Invalid("/agent/id"))
}
fn validate_actor(actor: &Value) -> Result<(), CaptureError> {
    #[cfg(test)]
    SCHEDULE_WORK.with(|work| work.borrow_mut().validations += 1);
    CaptureTable::Agents.validate(actor)?;
    Ok(())
}
fn snapshot_scope(scope: &Scope) -> Scope {
    #[cfg(test)]
    SCHEDULE_WORK.with(|work| work.borrow_mut().scopes += 1);
    scope.clone()
}
fn supplied_scope_matches(scope: &Scope, agent: &Value) -> bool {
    #[cfg(test)]
    SCHEDULE_WORK.with(|work| work.borrow_mut().scopes += 1);
    Scope::deserialize(agent).is_ok_and(|supplied| scope == &supplied)
}
fn poll_changed(poll: Option<&Poll>, input: &ActorInput) -> bool {
    poll.is_none_or(|poll| {
        poll.blocked_input.as_ref().map_or_else(
            || poll.scope != input.scope,
            |blocked| blocked.input_changed(input),
        )
    })
}
#[derive(Clone, PartialEq)]
struct BlockedInput {
    id: String,
    root_id: Option<String>,
    account_key: Option<String>,
    thread_id: Option<String>,
    epoch: Option<i64>,
    turn_id: Option<String>,
    last_completed_turn: Value,
    tokens_used: Value,
    token_usage_accounting: Value,
    token_budget: Value,
    created: Value,
    provider: Value,
    deleted_at: Value,
}
impl BlockedInput {
    fn from_input(input: &ActorInput) -> Self {
        Self {
            id: input.id.clone(),
            root_id: input.root_id.clone(),
            account_key: input.scope.account_key.clone(),
            thread_id: input.scope.thread_id.clone(),
            epoch: input.epoch,
            turn_id: input.turn_id.clone(),
            last_completed_turn: input.last_completed_turn.clone(),
            tokens_used: input.tokens_used.clone(),
            token_usage_accounting: input.token_usage_accounting.clone(),
            token_budget: input.token_budget.clone(),
            created: input.created.clone(),
            provider: input.scope.provider.clone(),
            deleted_at: input.scope.deleted_at.clone(),
        }
    }
    fn input_changed(&self, input: &ActorInput) -> bool {
        self.id != input.id
            || self.root_id != input.root_id
            || self.account_key != input.scope.account_key
            || self.thread_id != input.scope.thread_id
            || self.epoch != input.epoch
            || self.turn_id != input.turn_id
            || self.last_completed_turn != input.last_completed_turn
            || self.tokens_used != input.tokens_used
            || self.token_usage_accounting != input.token_usage_accounting
            || self.token_budget != input.token_budget
            || self.created != input.created
            || self.provider != input.scope.provider
            || self.deleted_at != input.scope.deleted_at
    }
}
#[cfg(test)]
#[derive(Default, Debug)]
struct ScheduleWork {
    validations: usize,
    scopes: usize,
    comparisons: usize,
}
#[cfg(test)]
thread_local! {
    static SCHEDULE_WORK: std::cell::RefCell<ScheduleWork> = const {
        std::cell::RefCell::new(ScheduleWork { validations: 0, scopes: 0, comparisons: 0 })
    };
}

#[cfg(test)]
mod tests {
    fn select<'a>(
        schedule: &HistorySchedule,
        roster: &'a [Value],
        now: f64,
    ) -> Result<Option<&'a Value>, CaptureError> {
        Ok(schedule
            .next(now)?
            .and_then(|row| schedule.rows.get(&row))
            .and_then(|id| {
                roster
                    .iter()
                    .find(|actor| actor_id(actor).is_ok_and(|actor_id| actor_id == id))
            }))
    }

    use super::*;
    use serde_json::json;

    fn agent(status: &str) -> Value {
        json!({
            "id":"history-agent", "threadId":"thread-1", "accountKey":"default",
            "provider":"codex", "deletedAt":null, "status":status, "inFlight":false
        })
    }

    #[test]
    fn stable_roster_drain_does_not_repeat_roster_validation_or_scopes() {
        const ACTORS: usize = 12;
        let roster = (0..ACTORS)
            .map(|index| {
                let mut actor = agent("paused");
                actor["id"] = json!(format!("actor-{index}"));
                actor
            })
            .collect::<Vec<_>>();
        let mut schedule = HistorySchedule::default();
        schedule.refresh(&roster).expect("ingest roster");
        SCHEDULE_WORK.with(|work| *work.borrow_mut() = ScheduleWork::default());
        for expected in &roster {
            let selected = select(&schedule, &roster, 0.0)
                .expect("selection")
                .expect("ready");
            assert_eq!(selected["id"], expected["id"], "preserve roster ties");
            let selected = selected.clone();
            schedule
                .completed(&selected, &checkpoint_for(&selected, "current"), 0.0)
                .expect("settle selected actor");
        }
        assert!(select(&schedule, &roster, 0.0).expect("drained").is_none());
        SCHEDULE_WORK.with(|work| {
            let work = work.borrow();
            assert!(work.validations <= ACTORS, "selected actors only: {work:?}");
            assert_eq!(
                work.scopes, ACTORS,
                "only completed actors snapshot scope: {work:?}"
            );
            // Two balanced-tree removals per completion, each logarithmic in roster size.
            const READY_SETS: usize = 2;
            let comparison_bound = ACTORS * (ACTORS.ilog2() as usize + 1) * READY_SETS;
            assert!(
                work.comparisons <= comparison_bound,
                "ordered removals only: {work:?}"
            );
        });
    }

    #[tokio::test]
    async fn collector_drain_reuses_checked_roster() {
        use crate::analytics_history::StoreHistoryBudgetSink;
        use crate::analytics_history::{HistoryClock, HistoryCollector};
        use std::sync::{Arc, atomic::AtomicBool};
        use studio_runtime_store::RuntimeStore;
        const ACTORS: usize = 4;
        let root = tempfile::tempdir().expect("disposable runtime");
        let store = RuntimeStore::open(Some(root.path())).await.expect("store");
        let roster = (0..ACTORS)
            .map(|index| {
                let mut actor = agent("paused");
                actor["id"] = json!(format!("collector-{index}"));
                // Live-only histories settle a final checkpoint without a native profile.
                actor["provider"] = json!("claude");
                actor
            })
            .collect::<Vec<_>>();
        let records = roster.clone();
        store
            .transaction(move |tx| {
                for actor in records {
                    tx.execute(
                        "INSERT INTO runtime_agents(id,record) VALUES (?1,?2)",
                        rusqlite::params![actor_id(&actor).expect("fixture id"), actor.to_string()],
                    )?;
                }
                Ok(())
            })
            .await
            .expect("persist actor identities");
        let mut collector = HistoryCollector::default();
        collector
            .schedule
            .refresh(&roster)
            .expect("ingest checked roster");
        SCHEDULE_WORK.with(|work| *work.borrow_mut() = ScheduleWork::default());
        let budget = StoreHistoryBudgetSink::new(&store);
        for expected in &roster {
            assert_eq!(
                select(&collector.schedule, &roster, 0.).expect("selection"),
                Some(expected)
            );
            assert!(
                !collector
                    .step(
                        &store,
                        |row| roster.get(usize::try_from(row).ok()?),
                        |_| panic!("live-only actor needs no profile"),
                        HistoryClock {
                            observed_at: 0.,
                            monotonic: 0.
                        },
                        Arc::new(AtomicBool::new(false)),
                        &budget
                    )
                    .await
                    .expect("settle final checkpoint")
            );
            assert!(collector.schedule.settled(expected));
        }
        assert!(!collector.schedule.has_pending());
        SCHEDULE_WORK.with(|work| {
            let work = work.borrow();
            assert_eq!(work.validations, ACTORS);
            // Completion snapshots scope, and each explicit settled probe reads it once.
            const SCOPE_READS_PER_ACTOR: usize = 2;
            assert_eq!(work.scopes, ACTORS * SCOPE_READS_PER_ACTOR);
        });
    }

    #[test]
    fn settled_compares_the_supplied_actor_scope() {
        let actor = agent("paused");
        let mut schedule = HistorySchedule::default();
        schedule
            .refresh(std::slice::from_ref(&actor))
            .expect("roster");
        schedule
            .completed(&actor, &checkpoint_for(&actor, "current"), 0.)
            .expect("current");
        assert!(schedule.settled(&actor));
        let mut changed = actor;
        changed["status"] = json!("running");
        assert!(
            !schedule.settled(&changed),
            "an uningested scope must not appear settled"
        );
    }

    #[test]
    fn blocked_input_retains_null_to_empty_and_zero_distinctions() {
        let mut actor = agent("paused");
        actor["rootId"] = Value::Null;
        actor["epoch"] = Value::Null;
        let blocked = BlockedInput::from_input(&ActorInput::deserialize(&actor).expect("input"));
        actor["rootId"] = json!("");
        actor["epoch"] = json!(0);
        assert!(blocked.input_changed(&ActorInput::deserialize(&actor).expect("input")));
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
        schedule
            .refresh(std::slice::from_ref(&actor))
            .expect("ingest actor");
        assert_eq!(
            select(&schedule, std::slice::from_ref(&actor), 0.0)
                .expect("initial schedule selection"),
            Some(&actor)
        );
        schedule
            .completed(&actor, &current_checkpoint("current"), 0.0)
            .expect("complete initial history import");
        assert_eq!(
            select(&schedule, std::slice::from_ref(&actor), 10_000.0)
                .expect("current actor should wait for notification"),
            None
        );
        assert!(!schedule.has_pending());
        schedule
            .notify("history-agent")
            .expect("history notification");
        assert_eq!(
            select(&schedule, std::slice::from_ref(&actor), 10_000.0)
                .expect("notification should schedule actor"),
            Some(&actor)
        );
    }

    #[test]
    fn partial_checkpoint_remains_pending_until_caught_up() {
        let actor = agent("running");
        let mut schedule = HistorySchedule::default();
        schedule
            .refresh(std::slice::from_ref(&actor))
            .expect("ingest actor");
        schedule
            .completed(&actor, &current_checkpoint("catchingUp"), 0.0)
            .expect("record partial history import");
        assert_eq!(
            select(&schedule, std::slice::from_ref(&actor), 0.0)
                .expect("partial actor remains scheduled"),
            Some(&actor)
        );
        schedule
            .completed(&actor, &current_checkpoint("current"), 1.0)
            .expect("complete caught-up history import");
        assert_eq!(
            select(&schedule, std::slice::from_ref(&actor), 1.0)
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
            select(&schedule, &roster, 0.0).expect("initial selection"),
            Some(&blocked)
        );
        schedule
            .block_for_input_change(&blocked, 1.0)
            .expect("block only invalid actor");
        assert_eq!(
            select(&schedule, &roster, 1.0).expect("healthy actor selection"),
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
            select(&schedule, &repaired, 3.0).expect("repaired actor selection"),
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
            select(&schedule, std::slice::from_ref(&actor), 2.0).expect("file event resumes actor"),
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
            select(&schedule, &roster, 2.0).expect("budget actor resumes"),
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
            select(&schedule, &roster, 2.0).expect("changed profile resumes"),
            Some(&actor)
        );
        schedule
            .completed(&actor, &checkpoint_for(&actor, "current"), 3.0)
            .expect("changed profile actor completes");
        assert!(!schedule.has_pending(), "unrelated profile remains blocked");

        schedule.discovery_changed(Some("other-account"));
        assert_eq!(
            select(&schedule, &roster, 4.0).expect("other profile resumes"),
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
    // Frozen pre-fix scheduler (dc645945): independent state and original
    // validate/scan/sort selector. Used only to compare scheduling semantics.
    mod legacy {
        use super::super::{ACTIVE_QUOTA, MAX_ACTORS};
        use crate::analytics_capture::CaptureError;
        use serde_json::Value;
        use std::collections::{BTreeMap, BTreeSet};
        use studio_runtime_store::analytics_capture::CaptureTable;
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
            pub fn notify_rollout(
                &mut self,
                agent: &str,
                structural: bool,
            ) -> Result<(), CaptureError> {
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
            pub fn block_for_input_change(
                &mut self,
                agent: &Value,
                now: f64,
            ) -> Result<(), CaptureError> {
                self.block_for_change(agent, now, false)
            }

            /// Suppress budget-invalid actors until an actual runtime_budget commit or
            /// a change to that actor's durable input signature arrives.
            pub fn block_for_budget_change(
                &mut self,
                agent: &Value,
                now: f64,
            ) -> Result<(), CaptureError> {
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
                    || !same_optional_value(
                        &self.last_completed_turn,
                        agent.get("lastCompletedTurn"),
                    )
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
    }

    fn parity_selection(
        schedule: &HistorySchedule,
        reference: &mut legacy::HistorySchedule,
        roster: &[Value],
        now: f64,
    ) -> Option<Value> {
        let expected = reference.next(roster, now).expect("reference selection");
        let actual = select(schedule, roster, now).expect("retained selection");
        assert_eq!(actual, expected, "same selected actor at {now}");
        actual.cloned()
    }

    #[test]
    fn retained_order_matches_original_selector_across_events() {
        const ROUNDS: usize = ACTIVE_QUOTA * 3;
        let mut roster = ["running", "queued", "paused", "completed"]
            .into_iter()
            .enumerate()
            .map(|(index, status)| {
                let mut actor = agent(status);
                actor["id"] = json!(format!("actor-{index}"));
                actor
            })
            .collect::<Vec<_>>();
        let mut schedule = HistorySchedule::default();
        let mut reference = legacy::HistorySchedule::default();
        schedule.refresh(&roster).expect("initial roster");
        reference.refresh(&roster).expect("reference roster");
        for round in 0..ROUNDS {
            let now = (round / ACTIVE_QUOTA) as f64;
            let selected =
                parity_selection(&schedule, &mut reference, &roster, now).expect("catch-up actor");
            let checkpoint = checkpoint_for(&selected, "catchingUp");
            schedule
                .completed(&selected, &checkpoint, now)
                .expect("partial");
            reference
                .completed(&selected, &checkpoint, now)
                .expect("reference partial");
        }
        let blocked =
            parity_selection(&schedule, &mut reference, &roster, 4.).expect("profile actor");
        schedule
            .block_for_input_change(&blocked, 4.)
            .expect("block profile");
        reference
            .block_for_input_change(&blocked, 4.)
            .expect("reference block profile");
        let budget =
            parity_selection(&schedule, &mut reference, &roster, 4.).expect("budget actor");
        schedule
            .block_for_budget_change(&budget, 4.)
            .expect("block budget");
        reference
            .block_for_budget_change(&budget, 4.)
            .expect("reference block budget");
        // Status and activity changes must reorder ready actors but not resume a blocked actor.
        for actor in &mut roster {
            actor["status"] = json!("paused");
            actor["inFlight"] = json!(true);
        }
        schedule
            .update_rows(
                &roster
                    .iter()
                    .enumerate()
                    .map(|(row, actor)| (row as i64, Some(actor.clone())))
                    .collect::<Vec<_>>(),
            )
            .expect("changed rows");
        reference.refresh(&roster).expect("reference changed rows");
        parity_selection(&schedule, &mut reference, &roster, 5.);
        schedule
            .notify_rollout(actor_id(&blocked).expect("id"), false)
            .expect("nonstructural rollout");
        reference
            .notify_rollout(actor_id(&blocked).expect("id"), false)
            .expect("reference rollout");
        parity_selection(&schedule, &mut reference, &roster, 5.);
        schedule.budget_changed();
        reference.budget_changed();
        parity_selection(&schedule, &mut reference, &roster, 5.);
        schedule.discovery_changed(Some("default"));
        reference.discovery_changed(Some("default"));
        parity_selection(&schedule, &mut reference, &roster, 5.);
        // Relevant input changes and deletion preserve the original row-order tie.
        roster[0]["epoch"] = json!(2);
        roster[0]["threadId"] = json!("new-thread");
        schedule
            .update_rows(&[(0, Some(roster[0].clone())), (1, None)])
            .expect("input change and deletion");
        roster.remove(1);
        reference.refresh(&roster).expect("reference removal");
        while let Some(selected) = parity_selection(&schedule, &mut reference, &roster, 6.) {
            let checkpoint = checkpoint_for(&selected, "current");
            schedule
                .completed(&selected, &checkpoint, 6.)
                .expect("current");
            reference
                .completed(&selected, &checkpoint, 6.)
                .expect("reference current");
            assert_eq!(schedule.settled(&selected), reference.settled(&selected));
        }
        assert_eq!(schedule.has_pending(), reference.has_pending());
        let id = actor_id(&roster[0]).expect("id");
        schedule.notify(id).expect("explicit notification");
        reference.notify(id).expect("reference notification");
        parity_selection(&schedule, &mut reference, &roster, 7.);
    }

    #[test]
    fn row_ingestion_checks_only_changed_actors_and_preserves_moves() {
        let first = agent("paused");
        let mut second = first.clone();
        second["id"] = json!("second");
        let mut schedule = HistorySchedule::default();
        schedule
            .refresh_ordered(&[10, 20], &[first.clone(), second.clone()])
            .expect("roster");
        SCHEDULE_WORK.with(|work| *work.borrow_mut() = ScheduleWork::default());
        second["status"] = json!("running");
        schedule
            .update_rows(&[(20, Some(second.clone()))])
            .expect("one changed row");
        SCHEDULE_WORK.with(|work| {
            let work = work.borrow();
            assert_eq!(work.validations, 1);
            assert_eq!(work.scopes, 1);
        });
        schedule
            .completed(&second, &checkpoint_for(&second, "current"), 1.)
            .expect("settle second");
        schedule
            .update_rows(&[(20, None), (5, Some(second.clone()))])
            .expect("row replacement preserves poll");
        assert!(schedule.settled(&second));
        assert_eq!(schedule.next(1.).expect("selection"), Some(10));
        schedule
            .refresh_ordered(&[5, 10], &[second.clone(), first])
            .expect("full read in row order");
        assert!(schedule.settled(&second));
        assert!(
            schedule
                .update_rows(&[(5, Some(json!({"id":"second", "epoch":-1})))])
                .is_err()
        );
        assert!(
            schedule.settled(&second),
            "invalid changed row does not mutate checked state"
        );
    }
}
