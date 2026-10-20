//! The per-account turn start throttle, the same for every provider.
//!
//! Providers limit requests per account, so each account has three settings
//! ([`StartThrottle`], persisted on its registry row): at most `burst` turn
//! starts at once, a `pause` after the start that ends a burst (`burst` starts
//! made, or the last free running slot taken), and at most `max_running`
//! running turns. A burst is starts close together: a start at least `pause`
//! after the previous one begins a new burst, so a trickle of starts never
//! pauses. A wake that would start a turn over a limit waits here
//! before its claim, so its input stays queued with its identity and starts
//! later exactly once; nothing fails. The agent shows the wait (`startWait`)
//! until its claim clears it.
//!
//! The state is in memory, one [`StartGate`] per account in this process.
//! Nothing polls: the first waiter sleeps until the pause ends, or, when every
//! slot is taken, until an agent row commits (a running turn may have settled)
//! or the account settings change. A running turn is one this gate admitted
//! whose claim made it in flight, or an in-flight turn found when the gate
//! was created; it frees its slot when its agent row is no longer in flight.
//! Studio controls turn starts only, not the provider's own requests inside a
//! turn.
use crate::account_driver::AccountDriver;
use crate::continuation::ContinuationError;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use studio_runtime_providers::ProviderKind;
use studio_runtime_schema::shared_values::{
    ACCOUNT_RUNNING_TURNS_DEFAULT_CLAUDE, ACCOUNT_RUNNING_TURNS_DEFAULT_CODEX,
    ACCOUNT_RUNNING_TURNS_MAX, ACCOUNT_RUNNING_TURNS_MIN, ACCOUNT_START_BURST_DEFAULT_CLAUDE,
    ACCOUNT_START_BURST_DEFAULT_CODEX, ACCOUNT_START_BURST_MAX, ACCOUNT_START_BURST_MIN,
    ACCOUNT_START_PAUSE_DEFAULT_SECONDS_CLAUDE, ACCOUNT_START_PAUSE_DEFAULT_SECONDS_CODEX,
    ACCOUNT_START_PAUSE_MAX_SECONDS, ACCOUNT_START_PAUSE_MIN_SECONDS,
};
use studio_runtime_store::{AgentId, AgentRepository, AgentStatus, RuntimeStore, StoreError};
use tokio::sync::{Notify, watch};
use tokio::time::Instant;

/// The account registry row field that stores [`StartThrottle`].
pub const REGISTRY_FIELD: &str = "startThrottle";
/// The agent record field that shows a start waiting here.
pub const WAIT_FIELD: &str = "startWait";

/// One account's turn start throttle. Bounds and the per-provider defaults
/// (with the measurements behind them) are owned by the shared values
/// `ACCOUNT_START_*` and `ACCOUNT_RUNNING_TURNS_*`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StartThrottle {
    /// Turn starts per burst; unit: turn starts.
    pub burst: u64,
    /// Wait after the start that ends a burst.
    pub pause: Duration,
    /// Turns that may run at once; unit: running turns.
    pub max_running: u64,
}

impl StartThrottle {
    /// The throttle a new account of `provider` takes.
    pub fn provider_default(provider: ProviderKind) -> Self {
        let (burst, pause, running) = match provider {
            ProviderKind::Codex => (
                ACCOUNT_START_BURST_DEFAULT_CODEX,
                ACCOUNT_START_PAUSE_DEFAULT_SECONDS_CODEX,
                ACCOUNT_RUNNING_TURNS_DEFAULT_CODEX,
            ),
            ProviderKind::Claude => (
                ACCOUNT_START_BURST_DEFAULT_CLAUDE,
                ACCOUNT_START_PAUSE_DEFAULT_SECONDS_CLAUDE,
                ACCOUNT_RUNNING_TURNS_DEFAULT_CLAUDE,
            ),
        };
        Self {
            burst,
            pause: Duration::from_secs(pause),
            max_running: running,
        }
    }

    /// The throttle with these values, or `None` when one is out of bounds.
    pub fn new(burst: u64, pause_seconds: u64, max_running: u64) -> Option<Self> {
        ((ACCOUNT_START_BURST_MIN..=ACCOUNT_START_BURST_MAX).contains(&burst)
            && (ACCOUNT_START_PAUSE_MIN_SECONDS..=ACCOUNT_START_PAUSE_MAX_SECONDS)
                .contains(&pause_seconds)
            && (ACCOUNT_RUNNING_TURNS_MIN..=ACCOUNT_RUNNING_TURNS_MAX).contains(&max_running))
        .then(|| Self {
            burst,
            pause: Duration::from_secs(pause_seconds),
            max_running,
        })
    }

    /// The throttle stored on an account registry row; its provider's
    /// defaults when the row has none (a row without a known provider is the
    /// legacy Codex form). The registry schema validates a stored value.
    pub fn of_record(row: &Value) -> Self {
        let stored = &row[REGISTRY_FIELD];
        let field = |key: &str| stored.get(key).and_then(Value::as_u64);
        field("burst")
            .zip(field("pauseSeconds"))
            .zip(field("maxRunningTurns"))
            .and_then(|((burst, pause), running)| Self::new(burst, pause, running))
            .unwrap_or_else(|| {
                Self::provider_default(ProviderKind::of_record(row).unwrap_or(ProviderKind::Codex))
            })
    }

    /// The registry row value of this throttle.
    pub fn to_record(self) -> Value {
        json!({
            "burst": self.burst,
            "pauseSeconds": self.pause.as_secs(),
            "maxRunningTurns": self.max_running,
        })
    }
}

/// Why a start waits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Wait {
    /// Every running slot is taken; `running` turns run.
    Slot { running: u64 },
    /// The pause after a burst lasts until `until`.
    Pause { until: Instant },
    /// Free to start, behind an earlier start that goes first.
    Behind,
}

/// One account's admission state.
#[derive(Default)]
struct GateState {
    /// Admitted agents; `true` once the claim made the turn in flight.
    running: BTreeMap<String, bool>,
    /// In-flight turns found when the gate was created are counted.
    seeded: bool,
    /// Starts in the current burst.
    burst_starts: u64,
    /// The latest admitted start; a start at least a pause after it begins a
    /// new burst.
    last_start_at: Option<Instant>,
    /// The start that ended the last burst; the pause runs from it, so a
    /// changed pause applies at once.
    burst_ended_at: Option<Instant>,
    /// Waiting starts in arrival order; only the first may be admitted.
    waiters: VecDeque<(u64, Arc<Notify>)>,
    next_ticket: u64,
}

/// What [`GateState::decide`] found for one waiting start.
#[derive(Debug, Eq, PartialEq)]
enum Decision {
    /// A new running turn.
    Admit,
    /// The agent already holds a slot; its wake starts no new turn here.
    Pass,
    /// `first` is whether this start is the next to be admitted.
    Wait { wait: Wait, first: bool },
}

impl GateState {
    fn enter(&mut self) -> (u64, Arc<Notify>) {
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        let notify = Arc::new(Notify::new());
        self.waiters.push_back((ticket, Arc::clone(&notify)));
        (ticket, notify)
    }

    /// Remove `ticket` and let the next waiter look again.
    fn leave(&mut self, ticket: u64) {
        self.waiters.retain(|(waiter, _)| *waiter != ticket);
        self.wake_first();
    }

    fn wake_first(&self) {
        if let Some((_, notify)) = self.waiters.front() {
            notify.notify_one();
        }
    }

    fn decide(
        &mut self,
        ticket: u64,
        agent: &str,
        throttle: StartThrottle,
        now: Instant,
    ) -> Decision {
        if self.running.contains_key(agent) {
            self.leave(ticket);
            return Decision::Pass;
        }
        let first = self.waiters.front().is_some_and(|(id, _)| *id == ticket);
        let running = self.running.len() as u64;
        if running >= throttle.max_running {
            return Decision::Wait {
                wait: Wait::Slot { running },
                first,
            };
        }
        if let Some(until) = self
            .burst_ended_at
            .map(|ended| ended + throttle.pause)
            .filter(|until| *until > now)
        {
            return Decision::Wait {
                wait: Wait::Pause { until },
                first,
            };
        }
        if !first {
            return Decision::Wait {
                wait: Wait::Behind,
                first,
            };
        }
        self.waiters.pop_front();
        self.running.insert(agent.to_owned(), false);
        if self
            .last_start_at
            .is_none_or(|last| now >= last + throttle.pause)
        {
            self.burst_starts = 0;
        }
        self.last_start_at = Some(now);
        self.burst_starts += 1;
        if self.burst_starts >= throttle.burst || self.running.len() as u64 >= throttle.max_running
        {
            self.burst_starts = 0;
            self.burst_ended_at = Some(now);
        }
        self.wake_first();
        Decision::Admit
    }

    /// Free the slot of `agent` if it still has one.
    fn release(&mut self, agent: &str) {
        if self.running.remove(agent).is_some() {
            self.wake_first();
        }
    }
}

/// The admission of one account's turn starts in this process.
#[derive(Default)]
pub struct StartGate {
    state: Mutex<GateState>,
}

/// What [`StartGate::admit`] reads besides the gate: the store for the
/// in-flight check and the wait record, the agent-row change signal, and the
/// account registry for the live settings.
pub struct StartSources {
    pub store: Arc<RuntimeStore>,
    pub agent_changes: watch::Receiver<u64>,
    pub driver: Arc<AccountDriver>,
    pub account: String,
}

/// A slot admitted for one start. Dropped before [`StartSlot::claimed`], it
/// frees the slot: the wake started no turn.
pub struct StartSlot {
    gate: Option<Arc<StartGate>>,
    agent: String,
}

impl StartSlot {
    /// The claim made the turn in flight; the slot stays taken until the
    /// agent row is no longer in flight.
    pub fn claimed(mut self) {
        if let Some(gate) = self.gate.take() {
            gate.lock().running.insert(self.agent.clone(), true);
        }
    }
}

impl Drop for StartSlot {
    fn drop(&mut self) {
        if let Some(gate) = self.gate.take() {
            gate.lock().release(&self.agent);
        }
    }
}

/// Removes an abandoned waiter (its wake task ended or was aborted), so the
/// starts behind it are not stranded.
struct Ticket<'a> {
    gate: &'a StartGate,
    id: Option<u64>,
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.gate.lock().leave(id);
        }
    }
}

impl StartGate {
    fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wait until `agent` (queued at `epoch`) may start a turn on this
    /// account. The returned slot passes through when the agent already holds
    /// one. While waiting, the agent's `startWait` shows why.
    pub async fn admit(
        self: &Arc<Self>,
        agent: &str,
        epoch: i64,
        mut sources: StartSources,
    ) -> Result<StartSlot, ContinuationError> {
        self.seed(&sources).await?;
        let (id, notify) = self.lock().enter();
        let mut ticket = Ticket {
            gate: self,
            id: Some(id),
        };
        let mut accounts = sources.driver.change_signal().subscribe();
        let mut shown = None;
        loop {
            let throttle = sources.driver.start_throttle(&sources.account);
            // Mark the current agent-row generation seen before reading the
            // in-flight state, so a commit after the read wakes this waiter.
            sources.agent_changes.borrow_and_update();
            let decision = self.lock().decide(id, agent, throttle, Instant::now());
            let (wait, first) = match decision {
                Decision::Admit => {
                    ticket.id = None;
                    return Ok(StartSlot {
                        gate: Some(Arc::clone(self)),
                        agent: agent.to_owned(),
                    });
                }
                Decision::Pass => {
                    ticket.id = None;
                    return Ok(StartSlot {
                        gate: None,
                        agent: agent.to_owned(),
                    });
                }
                Decision::Wait { wait, first } => (wait, first),
            };
            if first && matches!(wait, Wait::Slot { .. }) && self.prune(&sources.store).await? {
                continue;
            }
            let kind = std::mem::discriminant(&wait);
            if wait != Wait::Behind && shown != Some(kind) {
                show_wait(&sources.store, agent, epoch, wait, throttle).await?;
                shown = Some(kind);
            }
            let pause_end = match wait {
                Wait::Pause { until } if first => Some(until),
                _ => None,
            };
            let slot_wait = first && matches!(wait, Wait::Slot { .. });
            tokio::select! {
                () = notify.notified() => {}
                () = sleep_until(pause_end), if pause_end.is_some() => {}
                changed = sources.agent_changes.changed(), if slot_wait => {
                    changed.map_err(|_| ContinuationError::Unavailable("account-start/agents"))?;
                }
                changed = accounts.changed(), if first => {
                    changed.map_err(|_| ContinuationError::Unavailable("account-start/settings"))?;
                }
            }
        }
    }

    /// Count the in-flight turns of this account once, when the gate is new.
    async fn seed(&self, sources: &StartSources) -> Result<(), ContinuationError> {
        if self.lock().seeded {
            return Ok(());
        }
        let account = sources.account.clone();
        let in_flight = sources
            .store
            .read_async("runtime-services/account_start_gate", move |db| {
                let mut statement = db.prepare_cached(
                    "SELECT id FROM runtime_agents \
                     WHERE COALESCE(json_extract(record,'$.accountKey'),'default')=?1 \
                       AND json_extract(record,'$.inFlight')=1 \
                       AND json_extract(record,'$.deletedAt') IS NULL",
                )?;
                let rows = statement
                    .query_map([account], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, StoreError>(rows)
            })
            .await?;
        let mut state = self.lock();
        if !state.seeded {
            state.seeded = true;
            for agent in in_flight {
                state.running.insert(agent, true);
            }
        }
        Ok(())
    }

    /// Free the slots of claimed turns whose agent is no longer in flight.
    /// Returns whether one was freed.
    async fn prune(&self, store: &RuntimeStore) -> Result<bool, ContinuationError> {
        let claimed = self
            .lock()
            .running
            .iter()
            .filter(|(_, claimed)| **claimed)
            .map(|(agent, _)| agent.clone())
            .collect::<Vec<_>>();
        if claimed.is_empty() {
            return Ok(false);
        }
        let settled = store
            .read_async("runtime-services/account_start_gate", move |db| {
                let mut statement = db.prepare_cached(
                    "SELECT 1 FROM runtime_agents WHERE id=?1 \
                       AND json_extract(record,'$.inFlight')=1 \
                       AND json_extract(record,'$.deletedAt') IS NULL",
                )?;
                let mut settled = Vec::new();
                for agent in claimed {
                    if !statement.exists([&agent])? {
                        settled.push(agent);
                    }
                }
                Ok::<_, StoreError>(settled)
            })
            .await?;
        let mut state = self.lock();
        let mut freed = false;
        for agent in settled {
            if state.running.get(&agent) == Some(&true) {
                state.running.remove(&agent);
                freed = true;
            }
        }
        Ok(freed)
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Record why `agent`'s queued start waits, while it is still queued at `epoch`.
async fn show_wait(
    store: &RuntimeStore,
    agent: &str,
    epoch: i64,
    wait: Wait,
    throttle: StartThrottle,
) -> Result<(), ContinuationError> {
    let value = match wait {
        Wait::Behind => return Ok(()),
        Wait::Slot { running } => json!({
            "reason": "slot", "epoch": epoch, "runningTurns": running,
            "maxRunningTurns": throttle.max_running,
        }),
        Wait::Pause { until } => {
            let remaining = until.saturating_duration_since(Instant::now());
            let until = crate::event_write::unix_time()
                .map_err(|_| ContinuationError::Unavailable("account-start/clock"))?
                + remaining.as_secs_f64();
            json!({
                "reason": "pause", "epoch": epoch, "until": until,
                "runningTurns": 0, "maxRunningTurns": throttle.max_running,
            })
        }
    };
    let agent = agent.to_owned();
    crate::scheduler::lifecycle_transaction_with(store, move |tx| {
        let Some(mut record) = AgentRepository::get(tx, &AgentId::from(agent.as_str()))? else {
            return Ok::<_, StoreError>(());
        };
        if record.status != AgentStatus::Queued || record.epoch != epoch {
            return Ok(());
        }
        record.extra.insert(WAIT_FIELD.into(), value);
        AgentRepository::put(tx, &record)
    })
    .await?;
    Ok(())
}

/// The start gates of every account in this process.
#[derive(Default)]
pub struct StartGates {
    gates: Mutex<HashMap<String, Arc<StartGate>>>,
}

impl StartGates {
    pub fn gate(&self, account: &str) -> Arc<StartGate> {
        Arc::clone(
            self.gates
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(account.to_owned())
                .or_default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Decision, GateState, StartThrottle, Wait};
    use std::time::Duration;
    use tokio::time::Instant;

    fn throttle(burst: u64, pause: u64, running: u64) -> StartThrottle {
        StartThrottle::new(burst, pause, running).expect("valid throttle")
    }

    /// Admit `agent` now, or return why it waits.
    fn try_start(state: &mut GateState, agent: &str, limits: StartThrottle) -> Decision {
        let (ticket, _) = state.enter();
        let decision = state.decide(ticket, agent, limits, Instant::now());
        if decision != Decision::Admit {
            state.leave(ticket);
        }
        decision
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_starts_at_once_and_the_next_start_waits_for_the_pause() {
        let limits = throttle(3, 2, 8);
        let mut state = GateState::default();
        let start = Instant::now();
        for agent in ["a", "b", "c"] {
            assert_eq!(try_start(&mut state, agent, limits), Decision::Admit);
        }
        assert_eq!(
            try_start(&mut state, "d", limits),
            Decision::Wait {
                wait: Wait::Pause {
                    until: start + Duration::from_secs(2)
                },
                first: true
            }
        );
        tokio::time::advance(Duration::from_millis(1999)).await;
        assert!(matches!(
            try_start(&mut state, "d", limits),
            Decision::Wait { .. }
        ));
        tokio::time::advance(Duration::from_millis(1)).await;
        for agent in ["d", "e", "f"] {
            assert_eq!(try_start(&mut state, agent, limits), Decision::Admit);
        }
        assert!(matches!(
            try_start(&mut state, "g", limits),
            Decision::Wait {
                wait: Wait::Pause { .. },
                ..
            }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_trickle_of_starts_never_waits() {
        let limits = throttle(2, 10, 64);
        let mut state = GateState::default();
        for index in 0..10 {
            assert_eq!(
                try_start(&mut state, &format!("agent-{index}"), limits),
                Decision::Admit,
                "start {index}"
            );
            tokio::time::advance(Duration::from_secs(10)).await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn starts_closer_than_the_pause_form_one_burst_and_idle_resets_it() {
        let limits = throttle(3, 10, 64);
        let mut state = GateState::default();
        assert_eq!(try_start(&mut state, "a", limits), Decision::Admit);
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(try_start(&mut state, "b", limits), Decision::Admit);
        // Idle for less than the pause: still the same burst.
        tokio::time::advance(Duration::from_secs(9)).await;
        let third = Instant::now();
        assert_eq!(try_start(&mut state, "c", limits), Decision::Admit);
        assert_eq!(
            try_start(&mut state, "d", limits),
            Decision::Wait {
                wait: Wait::Pause {
                    until: third + Duration::from_secs(10)
                },
                first: true
            },
            "the third start ends the burst"
        );
        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(try_start(&mut state, "d", limits), Decision::Admit);
        // Idle for a whole pause after one start: the next starts a new burst.
        tokio::time::advance(Duration::from_secs(10)).await;
        for agent in ["e", "f", "g"] {
            assert_eq!(try_start(&mut state, agent, limits), Decision::Admit);
        }
        assert!(matches!(
            try_start(&mut state, "h", limits),
            Decision::Wait {
                wait: Wait::Pause { .. },
                ..
            }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn taking_the_last_slot_ends_the_burst_and_a_settled_turn_frees_it() {
        let limits = throttle(8, 1, 2);
        let mut state = GateState::default();
        assert_eq!(try_start(&mut state, "a", limits), Decision::Admit);
        assert_eq!(try_start(&mut state, "b", limits), Decision::Admit);
        assert_eq!(
            try_start(&mut state, "c", limits),
            Decision::Wait {
                wait: Wait::Slot { running: 2 },
                first: true
            }
        );
        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(matches!(
            try_start(&mut state, "c", limits),
            Decision::Wait {
                wait: Wait::Slot { .. },
                ..
            }
        ));
        state.release("a");
        assert_eq!(try_start(&mut state, "c", limits), Decision::Admit);
        assert_eq!(
            try_start(&mut state, "a", limits),
            Decision::Wait {
                wait: Wait::Slot { running: 2 },
                first: true
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_agent_holding_a_slot_passes_without_a_new_start() {
        let limits = throttle(1, 60, 1);
        let mut state = GateState::default();
        assert_eq!(try_start(&mut state, "a", limits), Decision::Admit);
        assert_eq!(try_start(&mut state, "a", limits), Decision::Pass);
        assert!(state.waiters.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn starts_are_admitted_in_arrival_order() {
        let limits = throttle(1, 1, 8);
        let mut state = GateState::default();
        assert_eq!(try_start(&mut state, "a", limits), Decision::Admit);
        let (first, _) = state.enter();
        let (second, _) = state.enter();
        tokio::time::advance(Duration::from_secs(1)).await;
        let now = Instant::now();
        assert!(matches!(
            state.decide(second, "c", limits, now),
            Decision::Wait { first: false, .. }
        ));
        assert_eq!(state.decide(first, "b", limits, now), Decision::Admit);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            state.decide(second, "c", limits, Instant::now()),
            Decision::Admit
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_changed_throttle_applies_to_the_next_decision() {
        let mut state = GateState::default();
        assert_eq!(
            try_start(&mut state, "a", throttle(8, 0, 1)),
            Decision::Admit
        );
        assert!(matches!(
            try_start(&mut state, "b", throttle(8, 0, 1)),
            Decision::Wait {
                wait: Wait::Slot { .. },
                ..
            }
        ));
        assert_eq!(
            try_start(&mut state, "b", throttle(8, 0, 2)),
            Decision::Admit
        );
    }

    #[test]
    fn stored_throttles_outside_the_bounds_read_as_the_provider_defaults() {
        use serde_json::json;
        use studio_runtime_providers::ProviderKind;
        let codex = StartThrottle::provider_default(ProviderKind::Codex);
        let claude = StartThrottle::provider_default(ProviderKind::Claude);
        assert_ne!(codex, claude);
        assert_eq!(StartThrottle::of_record(&json!({})), codex);
        assert_eq!(
            StartThrottle::of_record(&json!({"provider":"claude"})),
            claude
        );
        let stored = throttle(2, 5, 3);
        assert_eq!(
            StartThrottle::of_record(&json!({"startThrottle": stored.to_record()})),
            stored
        );
        for invalid in [
            json!({"burst":0,"pauseSeconds":1,"maxRunningTurns":1}),
            json!({"burst":1,"pauseSeconds":1,"maxRunningTurns":0}),
            json!({"burst":1,"pauseSeconds":-1,"maxRunningTurns":1}),
        ] {
            assert_eq!(
                StartThrottle::of_record(&json!({ "startThrottle": invalid })),
                codex
            );
            assert_eq!(
                StartThrottle::of_record(
                    &json!({ "provider": "claude", "startThrottle": invalid })
                ),
                claude
            );
        }
    }
}
