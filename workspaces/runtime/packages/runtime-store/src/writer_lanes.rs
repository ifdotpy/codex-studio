//! Two lanes into the single Canvas writer: commands first, background batches
//! in short parts. Design: target architecture, Latency rule 3 and Command
//! execution rule 5.
//!
//! A command is any write that is not a background part. It registers with the
//! [`CommandGate`] before it waits for writer admission. A background batch runs
//! as many small write transactions (parts). It submits the next part only when
//! no command is registered, so every queued command runs before the rest of
//! the batch. A running part delays a command by at most
//! [`BACKGROUND_PART_TIME_LIMIT_MS`] plus the cost of one row.
//!
//! A part body must not do I/O inside its transaction. Read the input before the
//! part and run effects after the batch.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

/// Time limit of one background part (one write transaction), in milliseconds.
/// A part stops before a row whose estimated cost (the slowest row so far)
/// would cross it. The first row of a part always runs, so one row slower than
/// the limit still advances.
pub const BACKGROUND_PART_TIME_LIMIT_MS: u64 = 50;

tokio::task_local! {
    /// True while a task submits a background part to the writer.
    pub(crate) static BACKGROUND_PART: bool;
}

/// A monotonic clock. Tests replace it to control part timing.
pub trait LaneClock {
    fn now(&self) -> Duration;
}

/// The real monotonic clock.
pub struct MonotonicClock(Instant);

impl MonotonicClock {
    pub fn new() -> Self {
        Self(Instant::now())
    }
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl LaneClock for MonotonicClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

/// Counts commands that wait for or hold the writer. Unit: commands.
#[derive(Default)]
pub struct CommandGate {
    pending: AtomicUsize,
    idle: Notify,
}

/// Registers one command until it is dropped.
pub struct CommandGuard(Arc<CommandGate>);

impl CommandGate {
    pub fn enter(self: &Arc<Self>) -> CommandGuard {
        self.pending.fetch_add(1, Ordering::AcqRel);
        CommandGuard(Arc::clone(self))
    }

    /// Resolve when no command is registered.
    pub async fn wait_idle(&self) {
        loop {
            let mut notified = pin!(self.idle.notified());
            notified.as_mut().enable();
            if self.pending.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for CommandGuard {
    fn drop(&mut self) {
        if self.0.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

/// Result of one background part.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PartOutcome {
    More,
    Done,
}

/// Process rows of `items` until the time limit would be crossed. At least one
/// row runs per part, so a batch always advances.
pub fn run_part<C, I, E>(
    clock: &C,
    limit: Duration,
    items: &mut I,
    mut process: impl FnMut(I::Item) -> Result<(), E>,
) -> Result<PartOutcome, E>
where
    C: LaneClock,
    I: Iterator,
{
    let started = clock.now();
    let mut slowest = Duration::ZERO;
    let mut ran = false;
    loop {
        let elapsed = clock.now().saturating_sub(started);
        if ran && elapsed.saturating_add(slowest) > limit {
            return Ok(PartOutcome::More);
        }
        let Some(item) = items.next() else {
            return Ok(PartOutcome::Done);
        };
        let row_started = clock.now();
        process(item)?;
        slowest = slowest.max(clock.now().saturating_sub(row_started));
        ran = true;
    }
}

/// Submit parts one at a time. Each part waits until no command is registered.
pub async fn run_parts<F, Fut, E>(gate: &CommandGate, mut part: F) -> Result<(), E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<PartOutcome, E>>,
{
    loop {
        gate.wait_idle().await;
        if part().await? == PartOutcome::Done {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::Mutex;

    struct FakeClock(Cell<Duration>);

    impl LaneClock for FakeClock {
        fn now(&self) -> Duration {
            self.0.get()
        }
    }

    #[tokio::test]
    async fn command_queued_behind_a_batch_runs_before_the_rest_of_it() {
        let gate = Arc::new(CommandGate::default());
        let log = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let part_gate = Arc::clone(&gate);
        let part_log = Arc::clone(&log);
        run_parts::<_, _, ()>(&gate, move || {
            let gate = Arc::clone(&part_gate);
            let log = Arc::clone(&part_log);
            let call = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                log.lock().expect("log").push("part");
                if call == 0 {
                    // A command arrives while the first part runs.
                    let guard = gate.enter();
                    tokio::spawn(async move {
                        log.lock().expect("log").push("command");
                        drop(guard);
                    });
                }
                Ok(if call == 2 {
                    PartOutcome::Done
                } else {
                    PartOutcome::More
                })
            }
        })
        .await
        .expect("batch");
        assert_eq!(
            *log.lock().expect("log"),
            ["part", "command", "part", "part"]
        );
    }

    #[test]
    fn a_part_never_exceeds_the_time_limit() {
        let clock = FakeClock(Cell::new(Duration::ZERO));
        let limit = Duration::from_millis(BACKGROUND_PART_TIME_LIMIT_MS);
        let mut rows = 0..100_u32;
        let mut parts = 0;
        loop {
            let started = clock.now();
            let outcome = run_part::<_, _, ()>(&clock, limit, &mut rows, |_| {
                clock.0.set(clock.0.get() + Duration::from_millis(7));
                Ok(())
            })
            .expect("part");
            assert!(clock.now() - started <= limit, "part crossed the limit");
            parts += 1;
            if outcome == PartOutcome::Done {
                break;
            }
        }
        assert!(parts > 1);
        assert_eq!(rows.next(), None);
    }

    #[test]
    fn a_row_slower_than_the_limit_still_advances() {
        let clock = FakeClock(Cell::new(Duration::ZERO));
        let mut rows = 0..3_u32;
        let outcome = run_part::<_, _, ()>(&clock, Duration::from_millis(10), &mut rows, |_| {
            clock.0.set(clock.0.get() + Duration::from_millis(100));
            Ok(())
        })
        .expect("part");
        assert_eq!(outcome, PartOutcome::More);
        assert_eq!(rows.next(), Some(1));
    }
}
