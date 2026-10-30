//! The per-account turn start throttle, the same for every provider.
//!
//! Providers limit requests per account, so each account has three settings
//! ([`StartThrottle`], persisted on its registry row): at most `burst` turn
//! starts at once, a `pause` after the `burst`-th start, and at most
//! `max_running` running turns. A full set of running turns only makes the
//! next start wait for a slot; it starts no pause of its own. A burst is starts close together: a start at least `pause`
//! after the previous one begins a new burst, so a trickle of starts never
//! pauses. A wake that would start a turn over a limit waits here
//! before its claim, so its input stays queued with its identity and starts
//! later exactly once; nothing fails. A root agent (a lead or user-facing
//! chat, no `parentId`) enters at the front of the queue, so its turn is not
//! stuck behind its own workers; it still counts against burst and running
//! limits, and a direct user message does not bypass them. The agent shows the
//! wait (`startWait`) until its claim clears it.
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
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use studio_runtime_schema::ProviderKind;
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
pub use studio_runtime_schema::agent_progress::AgentStartWait;
/// The agent record field that shows a start waiting here.
pub use studio_runtime_schema::agent_progress::START_WAIT_FIELD as WAIT_FIELD;

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
    /// defaults when the row has none. A known provider is required even when
    /// the row contains valid stored limits.
    pub fn of_record(row: &Value) -> Option<Self> {
        let provider = ProviderKind::of_record(row).ok()?;
        let stored = &row[REGISTRY_FIELD];
        let field = |key: &str| stored.get(key).and_then(Value::as_u64);
        Some(
            field("burst")
                .zip(field("pauseSeconds"))
                .zip(field("maxRunningTurns"))
                .and_then(|((burst, pause), running)| Self::new(burst, pause, running))
                .unwrap_or_else(|| Self::provider_default(provider)),
        )
    }

    /// Read a public account projection with the same fallback as the raw boundary.
    pub fn of_public_account(row: &crate::account_policy::PublicAccount) -> Option<Self> {
        let provider = row.provider_kind()?;
        let stored = match &row.start_throttle {
            Some(crate::account_policy::PublicThrottle::Fields(value)) => value
                .0
                .burst
                .as_u64()
                .zip(value.0.pause_seconds.as_u64())
                .zip(value.0.max_running_turns.as_u64())
                .and_then(|((burst, pause), running)| Self::new(burst, pause, running)),
            None | Some(crate::account_policy::PublicThrottle::Other(_)) => None,
        };
        Some(stored.unwrap_or_else(|| Self::provider_default(provider)))
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
    /// Queue a start: a `root` agent's at the front, others' at the back.
    fn enter(&mut self, root: bool) -> (u64, Arc<Notify>) {
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        let notify = Arc::new(Notify::new());
        if root {
            self.waiters.push_front((ticket, Arc::clone(&notify)));
        } else {
            self.waiters.push_back((ticket, Arc::clone(&notify)));
        }
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
        if self.burst_starts >= throttle.burst {
            self.burst_starts = 0;
            self.burst_ended_at = Some(now);
        }
        self.wake_first();
        Decision::Admit
    }

    /// Free the claimed slot of `agent` when its turn has settled, so its
    /// next wake is a new start and not a pass.
    fn release_settled(&mut self, agent: &str) {
        if self.running.get(agent) == Some(&true) {
            self.release(agent);
        }
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
        let (root, in_flight) = agent_standing(&sources.store, agent).await?;
        let (id, notify) = {
            let mut state = self.lock();
            if !in_flight {
                state.release_settled(agent);
            }
            state.enter(root)
        };
        let mut ticket = Ticket {
            gate: self,
            id: Some(id),
        };
        let mut accounts = sources.driver.change_signal().subscribe();
        let mut shown = None;
        loop {
            let throttle = sources
                .driver
                .start_throttle(&sources.account)
                .map_err(|error| {
                    ContinuationError::Invalid(format!(
                        "account-start/{}: {error}",
                        sources.account
                    ))
                })?;
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
            if shown != Some((wait, throttle)) {
                show_wait(&sources.store, agent, epoch, wait, throttle).await?;
                shown = Some((wait, throttle));
            }
            let pause_end = match wait {
                Wait::Pause { until } if first => Some(until),
                Wait::Slot { .. } | Wait::Pause { .. } | Wait::Behind => None,
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
                     WHERE account_key=?1 \
                       AND in_flight=1 \
                       AND deleted_at IS NULL",
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
                       AND in_flight=1 \
                       AND deleted_at IS NULL",
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

/// Whether `agent` is a root agent (no `parentId`) and its turn is in flight.
async fn agent_standing(
    store: &RuntimeStore,
    agent: &str,
) -> Result<(bool, bool), ContinuationError> {
    let agent = agent.to_owned();
    Ok(store
        .read_async("runtime-services/account_start_gate", move |db| {
            let mut statement = db.prepare_cached(
                "SELECT json_extract(record,'$.parentId') IS NULL, \
                        COALESCE(json_extract(record,'$.inFlight'),0)=1 \
                 FROM runtime_agents WHERE id=?1",
            )?;
            let standing = statement
                .query_row([agent], |row| Ok((row.get(0)?, row.get(1)?)))
                .optional()?;
            Ok::<_, StoreError>(standing.unwrap_or((false, false)))
        })
        .await?)
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
        Wait::Behind => AgentStartWait::StartQueue { epoch },
        Wait::Slot { running } => AgentStartWait::Slot {
            epoch,
            running_turns: running,
            max_running_turns: throttle.max_running,
        },
        Wait::Pause { until } => AgentStartWait::Pause {
            epoch,
            until: crate::event_write::unix_time()
                .map_err(|_| ContinuationError::Unavailable("account-start/clock"))?
                + until
                    .saturating_duration_since(Instant::now())
                    .as_secs_f64(),
        },
    };
    show_blocked(store, vec![(agent.to_owned(), value)]).await?;
    Ok(())
}

/// Publish the current admission owner's wait through the existing agent projection.
pub async fn show_blocked(
    store: &RuntimeStore,
    waits: Vec<(String, AgentStartWait)>,
) -> Result<(), StoreError> {
    if waits.is_empty() {
        return Ok(());
    }
    crate::scheduler::lifecycle_transaction_with(store, move |tx| {
        for (agent, wait) in waits {
            let Some(mut record) = AgentRepository::get(tx, &AgentId::from(agent.as_str()))? else {
                continue;
            };
            if !matches!(record.status, AgentStatus::Queued | AgentStatus::Starting)
                || record.epoch != wait.epoch()
            {
                continue;
            }
            let value = serde_json::to_value(wait)?;
            if record.extra.get(WAIT_FIELD) != Some(&value) {
                record.extra.insert(WAIT_FIELD.into(), value);
                AgentRepository::put(tx, &record)?;
            }
        }
        Ok::<_, StoreError>(())
    })
    .await
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
        let (ticket, _) = state.enter(false);
        let decision = state.decide(ticket, agent, limits, Instant::now());
        if decision != Decision::Admit {
            state.leave(ticket);
        }
        decision
    }

    #[tokio::test]
    async fn admission_wait_reaches_sync_and_stale_epoch_cannot_replace_it() {
        use serde_json::json;
        use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore, StoreError};
        let dir = tempfile::tempdir().expect("state directory");
        let store = RuntimeStore::open(Some(dir.path())).await.expect("store");
        store.transaction_with(|tx| {
            let agent = AgentRecord::from_value(json!({"accountKey":"default","provider":"codex", "id":"worker", "rootId":"worker", "epoch":4, "status":"queued", "autoWake":true})).expect("agent");
            AgentRepository::put(tx, &agent)
        }).await.expect("seed");
        super::show_wait(
            &store,
            "worker",
            4,
            Wait::Slot { running: 2 },
            throttle(8, 1, 3),
        )
        .await
        .expect("slot wait");
        super::show_wait(&store, "worker", 3, Wait::Behind, throttle(8, 1, 3))
            .await
            .expect("stale wait ignored");
        let view = store
            .read_async("test/admission-wait", |tx| {
                let agent = AgentRepository::get(tx, &"worker".into())?.expect("agent");
                Ok::<_, StoreError>(
                    crate::sync_views::agent_entity_view(&agent.value(), &Default::default())
                        .expect("projection"),
                )
            })
            .await
            .expect("view");
        assert_eq!(
            view["startWait"],
            json!({"reason":"slot", "epoch":4, "runningTurns":2, "maxRunningTurns":3})
        );
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
    async fn a_full_set_of_running_turns_waits_for_a_slot_and_a_settled_turn_frees_it() {
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

    /// Starts admitted in `minutes` simulated minutes for a backlog of
    /// workers whose turns last `turn` seconds. Settled slots are freed only
    /// when a start meets a full set of running turns, as the pruning does.
    async fn starts_per_minute(limits: StartThrottle, turn: u64, minutes: u64) -> u64 {
        let mut state = GateState::default();
        let mut finishing: Vec<(Instant, String)> = Vec::new();
        let mut starts = 0;
        for second in 0..minutes * 60 {
            loop {
                let agent = format!("worker-{starts}");
                let (ticket, _) = state.enter(false);
                let mut decision = state.decide(ticket, &agent, limits, Instant::now());
                if matches!(
                    decision,
                    Decision::Wait {
                        wait: Wait::Slot { .. },
                        ..
                    }
                ) {
                    let now = Instant::now();
                    finishing.retain(|(end, settled)| {
                        let done = *end <= now;
                        if done {
                            state.release(settled);
                        }
                        !done
                    });
                    decision = state.decide(ticket, &agent, limits, now);
                }
                if decision != Decision::Admit {
                    state.leave(ticket);
                    break;
                }
                state.running.insert(agent.clone(), true);
                finishing.push((Instant::now() + Duration::from_secs(turn), agent));
                starts += 1;
            }
            let _ = second;
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        starts / minutes
    }

    #[tokio::test(start_paused = true)]
    async fn throughput_is_one_burst_per_pause_and_never_pauses_for_a_full_set_of_slots() {
        // Burst 8 per 30 s is 16 starts a minute; 8 s turns never fill 16 slots.
        assert_eq!(starts_per_minute(throttle(8, 30, 16), 8, 5).await, 16);
    }

    #[tokio::test(start_paused = true)]
    async fn taking_the_last_slot_starts_no_pause_of_its_own() {
        // 12 slots of 40 s turns, burst 8 per 30 s: the burst that fills the
        // slots used to start an extra pause (12 starts a minute, not 14).
        assert_eq!(starts_per_minute(throttle(8, 30, 12), 40, 5).await, 14);
    }

    #[tokio::test(start_paused = true)]
    async fn throughput_with_the_defaults_matches_burst_and_running_limits() {
        // Codex 16 / 30 s / 32: 32 starts a minute while turns settle in time.
        assert_eq!(starts_per_minute(throttle(16, 30, 32), 20, 5).await, 32);
        // Claude 32 / 5 s / 64: 6 bursts of 32 a minute are capped by 64 slots
        // of 30 s turns: 64 starts per 30 s, 128 a minute.
        assert_eq!(starts_per_minute(throttle(32, 5, 64), 30, 5).await, 128);
    }

    #[tokio::test(start_paused = true)]
    async fn a_root_agent_starts_before_the_worker_backlog() {
        let limits = throttle(1, 1, 8);
        let mut state = GateState::default();
        assert_eq!(try_start(&mut state, "w0", limits), Decision::Admit);
        let workers: Vec<_> = (1..=20).map(|_| state.enter(false)).collect();
        let (lead, _) = state.enter(true);
        tokio::time::advance(Duration::from_secs(1)).await;
        let now = Instant::now();
        assert!(matches!(
            state.decide(workers[0].0, "w1", limits, now),
            Decision::Wait { first: false, .. }
        ));
        assert_eq!(state.decide(lead, "lead", limits, now), Decision::Admit);
        // The lead still used its burst: the next worker waits for the pause.
        assert!(matches!(
            state.decide(workers[0].0, "w1", limits, now),
            Decision::Wait {
                wait: Wait::Pause { .. },
                first: true
            }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_settled_turns_slot_cannot_pass_the_throttle() {
        let limits = throttle(1, 60, 8);
        let mut state = GateState::default();
        assert_eq!(try_start(&mut state, "a", limits), Decision::Admit);
        state.running.insert("a".into(), true);
        // Its turn settled; the next wake releases the slot and is a new start.
        state.release_settled("a");
        assert!(matches!(
            try_start(&mut state, "a", limits),
            Decision::Wait {
                wait: Wait::Pause { .. },
                ..
            }
        ));
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
        let (first, _) = state.enter(false);
        let (second, _) = state.enter(false);
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
    fn provider_identity_throttle_rejects_unknown_even_with_stored_limits() {
        use super::REGISTRY_FIELD;
        use serde_json::{Value, json};
        let stored = throttle(2, 5, 3);
        for provider in [
            None,
            Some(Value::Null),
            Some(json!("future")),
            Some(json!(7)),
        ] {
            for limits in [None, Some(stored.to_record())] {
                let mut row = json!({});
                if let Some(provider) = provider.clone() {
                    row["provider"] = provider;
                }
                if let Some(limits) = limits {
                    row[REGISTRY_FIELD] = limits;
                }
                assert_eq!(StartThrottle::of_record(&row), None, "{row}");
            }
        }
    }

    #[tokio::test]
    async fn provider_identity_gate_refuses_missing_account_without_admission() {
        use super::{AccountDriver, ContinuationError, StartGate, StartSources};
        use serde_json::json;
        use std::sync::Arc;
        use studio_runtime_schema::shared_values::ACCOUNT_REGISTRY_FORMAT_VERSION;
        use studio_runtime_store::RuntimeStore;
        let root = tempfile::tempdir().expect("state directory");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let registry = root.path().join("accounts/registry.json");
        std::fs::create_dir_all(registry.parent().expect("registry parent"))
            .expect("accounts directory");
        std::fs::write(&registry, json!({"version":ACCOUNT_REGISTRY_FORMAT_VERSION,"defaultAccountKey":"default","accounts":{"default":{"id":"default","provider":"codex","home":root.path(),"label":"Fixture","source":"Codex CLI","status":"signedOut"}},"logins":{}}).to_string()).expect("registry");
        let driver = Arc::new(AccountDriver::open(registry).expect("driver"));
        assert!(driver.start_throttle("default").is_ok());
        assert!(matches!(
            driver.start_throttle("missing"),
            Err(crate::account_driver::AccountDriverError::UnknownAccount)
        ));
        let (_changes, agent_changes) = tokio::sync::watch::channel(0);
        let gate = Arc::new(StartGate::default());
        let result = gate
            .admit(
                "a",
                0,
                StartSources {
                    store,
                    agent_changes,
                    driver,
                    account: "missing".into(),
                },
            )
            .await;
        assert!(
            matches!(result, Err(ContinuationError::Invalid(reason)) if reason.contains("account-start/missing"))
        );
        assert!(gate.lock().running.is_empty());
        assert!(gate.lock().waiters.is_empty());
    }

    #[test]
    fn public_throttle_matches_legacy_for_extensions_and_numeric_types() {
        use serde_json::json;
        for row in [
            json!({"provider":"codex","startThrottle":{"burst":2,"pauseSeconds":5,"maxRunningTurns":3,"vendor":true}}),
            json!({"provider":"codex","startThrottle":{"burst":2.0,"pauseSeconds":5,"maxRunningTurns":3}}),
            json!({"provider":"codex","startThrottle":{"burst":2,"pauseSeconds":5}}),
            json!({"provider":"unknown","startThrottle":null}),
        ] {
            let public = serde::Deserialize::deserialize(row.clone()).expect("public account");
            assert_eq!(
                StartThrottle::of_public_account(&public),
                StartThrottle::of_record(&row),
                "{row}"
            );
        }
    }

    #[test]
    fn stored_throttles_outside_the_bounds_read_as_the_provider_defaults() {
        use serde_json::json;
        use studio_runtime_schema::ProviderKind;
        let codex = StartThrottle::provider_default(ProviderKind::Codex);
        let claude = StartThrottle::provider_default(ProviderKind::Claude);
        assert_ne!(codex, claude);
        assert_eq!(
            StartThrottle::of_record(&json!({"provider":"codex"})),
            Some(codex)
        );
        assert_eq!(
            StartThrottle::of_record(&json!({"provider":"claude"})),
            Some(claude)
        );
        let stored = throttle(2, 5, 3);
        assert_eq!(
            StartThrottle::of_record(
                &json!({"provider":"codex","startThrottle": stored.to_record()})
            ),
            Some(stored)
        );
        for invalid in [
            json!({"burst":0,"pauseSeconds":1,"maxRunningTurns":1}),
            json!({"burst":1,"pauseSeconds":1,"maxRunningTurns":0}),
            json!({"burst":1,"pauseSeconds":-1,"maxRunningTurns":1}),
        ] {
            assert_eq!(
                StartThrottle::of_record(&json!({ "provider":"codex", "startThrottle": invalid })),
                Some(codex)
            );
            assert_eq!(
                StartThrottle::of_record(
                    &json!({ "provider": "claude", "startThrottle": invalid })
                ),
                Some(claude)
            );
        }
    }
}
