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

    #[test]
    fn addressed_wakes_follow_rows_and_sequence_boundaries() {
        let wakes = AgentWakes::default();
        let mut own_one = wakes.wait(0, WakeRows::Own(1));
        let mut own_two = wakes.wait(0, WakeRows::Own(2));
        let mut any = wakes.wait(0, WakeRows::Any);
        wakes.committed(TableChange::Rows(&[2]));
        assert_eq!(wakes.sequence(), 1);
        assert_eq!(own_one.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(own_two.try_recv(), Ok(()));
        assert_eq!(any.try_recv(), Ok(()));

        let mut waiting_two = wakes.wait(1, WakeRows::Own(2));
        wakes.committed(TableChange::Rows(&[7]));
        assert_eq!(wakes.sequence(), 2);
        assert_eq!(own_one.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(waiting_two.try_recv(), Err(TryRecvError::Empty));
        // The row changed after sequence zero, but not after sequence two.
        assert_eq!(wakes.wait(0, WakeRows::Own(7)).try_recv(), Ok(()));
        let mut current = wakes.wait(2, WakeRows::Own(7));
        assert_eq!(current.try_recv(), Err(TryRecvError::Empty));
        wakes.committed(TableChange::Full);
        assert_eq!(own_one.try_recv(), Ok(()));
        assert_eq!(waiting_two.try_recv(), Ok(()));
        assert_eq!(current.try_recv(), Ok(()));

        let rows = (0..=RECENT_AGENT_ROWS as i64).collect::<Vec<_>>();
        wakes.committed(TableChange::Rows(&rows));
        assert_eq!(wakes.sequence(), 4);
        // A registration older than the retained bound must rescan even for an absent row.
        assert_eq!(wakes.wait(3, WakeRows::Own(99_999)).try_recv(), Ok(()));
        let mut after_bound = wakes.wait(4, WakeRows::Own(99_999));
        assert_eq!(after_bound.try_recv(), Err(TryRecvError::Empty));
        wakes.committed(TableChange::Rows(&[99_999]));
        assert_eq!(after_bound.try_recv(), Ok(()));
    }

    #[test]
    fn a_dropped_waiter_is_pruned_at_the_next_registration() {
        let wakes = AgentWakes::default();
        drop(wakes.wait(0, WakeRows::Own(1)));
        let _kept = wakes.wait(0, WakeRows::Own(2));
        assert_eq!(wakes.lock().waiters.len(), 1);
    }
}
