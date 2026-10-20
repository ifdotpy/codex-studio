//! Addressed wakes of pending preparations by committed agent rows.
//!
//! A pending preparation waits for the agent rows it depends on: its own row
//! (workspace or VM readiness recorded on the agent), or, when its blocker is
//! not known, any agent row. The store's row hints carry the rowids of each
//! commit, so a commit wakes only the waiters of the rows it wrote. A commit
//! without row identity (hint overflow, unknown outcome) wakes every waiter.
//!
//! A waiter names the commit sequence read before its attempt. A commit after
//! that point that touched its rows wakes it at registration, so a change
//! committed while the attempt ran is not missed. The recent-row log is
//! bounded; a waiter older than the log is woken at once (full re-scan).
use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

use studio_runtime_store::TableChange;
use tokio::sync::oneshot;

/// Agent rowids kept to answer registrations that raced recent commits.
/// Unit: rowids. A registration older than the kept rows wakes at once.
const RECENT_AGENT_ROWS: usize = 4096;

/// The agent rows a pending preparation waits for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WakeRows {
    /// The waiter's own agent row.
    Own(i64),
    /// Any agent row: the blocker is not known.
    Any,
}

impl WakeRows {
    fn matches(self, row: i64) -> bool {
        match self {
            Self::Own(own) => own == row,
            Self::Any => true,
        }
    }
}

#[derive(Default)]
pub(super) struct AgentWakes {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Agent-row commits observed so far.
    sequence: u64,
    /// Rows written by recent commits, oldest first: (commit sequence, rowid).
    recent: VecDeque<(u64, i64)>,
    /// Commits up to this sequence are not all in `recent`.
    floor: u64,
    waiters: Vec<(WakeRows, oneshot::Sender<()>)>,
}

impl AgentWakes {
    /// The commit sequence to name in a later [`Self::wait`].
    pub(super) fn sequence(&self) -> u64 {
        self.lock().sequence
    }

    /// Resolve when a commit after `since` wrote one of `rows`.
    pub(super) fn wait(&self, since: u64, rows: WakeRows) -> oneshot::Receiver<()> {
        let (sender, receiver) = oneshot::channel();
        let mut state = self.lock();
        state.waiters.retain(|(_, waiter)| !waiter.is_closed());
        let missed = since < state.floor
            || state
                .recent
                .iter()
                .rev()
                .take_while(|(sequence, _)| *sequence > since)
                .any(|(_, row)| rows.matches(*row));
        if missed {
            let _woken = sender.send(());
        } else {
            state.waiters.push((rows, sender));
        }
        receiver
    }

    /// The store's agent-row observer: wake the waiters of the written rows.
    pub(super) fn committed(&self, change: TableChange<'_>) {
        let mut state = self.lock();
        state.sequence += 1;
        let sequence = state.sequence;
        let woken = match change {
            TableChange::Rows(rows) => {
                state.recent.extend(rows.iter().map(|row| (sequence, *row)));
                while state.recent.len() > RECENT_AGENT_ROWS {
                    if let Some((evicted, _)) = state.recent.pop_front() {
                        state.floor = evicted;
                    }
                }
                state
                    .waiters
                    .extract_if(.., |(wanted, _)| {
                        rows.iter().any(|row| wanted.matches(*row))
                    })
                    .collect::<Vec<_>>()
            }
            TableChange::Full => {
                state.recent.clear();
                state.floor = sequence;
                std::mem::take(&mut state.waiters)
            }
        };
        drop(state);
        for (_, waiter) in woken {
            let _woken = waiter.send(());
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentWakes, RECENT_AGENT_ROWS, WakeRows};
    use studio_runtime_store::TableChange;
    use tokio::sync::oneshot::error::TryRecvError;

    /// A commit in the oracle's full history: its rows, or `None` without row identity.
    type Commit = Option<Vec<i64>>;

    /// The full re-scan oracle: a waiter must wake when any commit after its
    /// sequence wrote one of its rows or carried no row identity.
    fn must_wake(history: &[Commit], since: u64, rows: WakeRows) -> bool {
        history
            .iter()
            .skip(usize::try_from(since).expect("sequence fits"))
            .any(|commit| match commit {
                Some(written) => written.iter().any(|row| rows.matches(*row)),
                None => true,
            })
    }

    struct Waiter {
        since: u64,
        rows: WakeRows,
        /// The registration was older than the kept rows, so it woke at once.
        overflowed: bool,
        receiver: tokio::sync::oneshot::Receiver<()>,
        woken: bool,
    }

    /// Deterministic corpus: commits of random rows (sometimes without identity
    /// or above the kept-row bound) interleaved with registrations that name
    /// past sequences. Addressed wakes must wake every waiter the full re-scan
    /// requires, and wake no other waiter unless its registration overflowed.
    #[test]
    fn addressed_wakes_match_the_full_rescan_oracle() {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = |bound: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % bound
        };
        let wakes = AgentWakes::default();
        let mut history: Vec<Commit> = Vec::new();
        let mut waiters: Vec<Waiter> = Vec::new();
        let (mut addressed, mut broadcast) = (0_usize, 0_usize);
        for _ in 0..4_000 {
            match next(10) {
                0..=4 => {
                    let commit: Commit = match next(50) {
                        0 => None,
                        1 => Some((0..RECENT_AGENT_ROWS as i64 + 1).collect()),
                        _ => Some((0..=next(3)).map(|_| next(64) as i64).collect()),
                    };
                    broadcast += waiters.iter().filter(|w| !w.woken).count();
                    match &commit {
                        Some(rows) => wakes.committed(TableChange::Rows(rows)),
                        None => wakes.committed(TableChange::Full),
                    }
                    history.push(commit);
                }
                _ => {
                    let current = wakes.sequence();
                    let since = current.saturating_sub(next(8));
                    let rows = if next(4) == 0 {
                        WakeRows::Any
                    } else {
                        WakeRows::Own(next(64) as i64)
                    };
                    let overflowed = since < wakes.lock().floor;
                    waiters.push(Waiter {
                        since,
                        rows,
                        overflowed,
                        receiver: wakes.wait(since, rows),
                        woken: false,
                    });
                }
            }
            for waiter in waiters.iter_mut().filter(|waiter| !waiter.woken) {
                let required = must_wake(&history, waiter.since, waiter.rows);
                match waiter.receiver.try_recv() {
                    Ok(()) => {
                        assert!(required || waiter.overflowed, "spurious wake");
                        waiter.woken = true;
                        addressed += 1;
                    }
                    Err(TryRecvError::Empty) => assert!(!required, "missed wake"),
                    Err(TryRecvError::Closed) => panic!("waiter dropped"),
                }
            }
        }
        assert!(waiters.iter().any(|waiter| waiter.woken));
        assert!(waiters.iter().any(|waiter| !waiter.woken));
        // Reported for the change record: a broadcast wakes every waiting
        // preparation on every agent commit.
        eprintln!(
            "agent wakes over {} commits: addressed {addressed}, broadcast {broadcast}",
            history.len()
        );
    }

    #[test]
    fn a_dropped_waiter_is_pruned_at_the_next_registration() {
        let wakes = AgentWakes::default();
        drop(wakes.wait(0, WakeRows::Own(1)));
        let _kept = wakes.wait(0, WakeRows::Own(2));
        assert_eq!(wakes.lock().waiters.len(), 1);
    }
}
