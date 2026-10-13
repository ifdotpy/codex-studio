//! One bounded input queue; control admission remains available while evidence is held.
use super::{AccountInput, ProviderSessionInput};
use crate::provider_session::ProviderSessionInputSendError;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

pub(super) const CAPACITY: usize = 64;
#[derive(Default)]
struct State {
    queue: VecDeque<AccountInput>,
    transport: usize,
    controls: usize,
    rescan: bool,
    waiters: Vec<mpsc::Sender<()>>,
    quiescing: bool,
    closed: bool,
}
#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}
#[derive(Clone)]
pub(super) struct Sender(Arc<Shared>);
pub(super) struct Receiver(Arc<Shared>);
pub(super) fn channel() -> (Sender, Receiver) {
    let shared = Arc::new(Shared::default());
    (Sender(shared.clone()), Receiver(shared))
}
impl Sender {
    pub(super) fn send(&self, input: AccountInput) -> Result<(), ()> {
        let mut state = self.0.state.lock().map_err(|_| ())?;
        // Wakes are hints backed by SQLite; coalesce or defer them until the next settlement wake.
        if let AccountInput::Wake(agent, epoch) = &input
            && (state.controls == CAPACITY
                || state
                    .queue
                    .iter()
                    .any(|i| matches!(i, AccountInput::Wake(a,e) if a == agent && e == epoch)))
        {
            state.rescan |= state.controls == CAPACITY;
            return Ok(());
        }
        while state.controls == CAPACITY && !state.closed {
            state = self.0.changed.wait(state).map_err(|_| ())?;
        }
        if state.closed {
            return Err(());
        }
        state.controls += 1;
        state.queue.push_back(input);
        self.0.changed.notify_all();
        Ok(())
    }
    #[allow(clippy::result_large_err)] // Preserve the shared sink contract without boxing its input.
    pub(super) fn transport(
        &self,
        input: ProviderSessionInput,
    ) -> Result<(), ProviderSessionInputSendError> {
        let Ok(mut state) = self.0.state.lock() else {
            return Err(ProviderSessionInputSendError::Closed { input });
        };
        let barrier = matches!(input, ProviderSessionInput::Quiesced);
        if state.closed || (state.quiescing && !barrier && state.transport == CAPACITY) {
            return Err(ProviderSessionInputSendError::Closed { input });
        }
        if !barrier && state.transport == CAPACITY {
            let (notify, capacity_available) = mpsc::channel();
            // At most the session's reader and writer can wait, each retaining one input.
            state.waiters.push(notify);
            self.0.changed.notify_all();
            return Err(ProviderSessionInputSendError::Full {
                input,
                capacity_available,
            });
        }
        if !barrier {
            state.transport += 1;
        }
        state.queue.push_back(AccountInput::Transport(input));
        self.0.changed.notify_all();
        Ok(())
    }
    #[cfg(test)]
    pub(super) fn backpressure_barrier(&self) -> usize {
        let state = self.0.state.lock().expect("mailbox lock");
        let (state, timeout) = self
            .0
            .changed
            .wait_timeout_while(state, Duration::from_secs(20), |s| s.waiters.is_empty())
            .expect("mailbox signal");
        assert!(!timeout.timed_out(), "provider reaches full mailbox");
        state.transport
    }
    pub(super) fn defer_wake(&self) {
        if let Ok(mut state) = self.0.state.lock() {
            state.rescan = true;
        }
    }
    pub(super) fn take_rescan(&self) -> bool {
        self.0
            .state
            .lock()
            .is_ok_and(|mut state| std::mem::take(&mut state.rescan))
    }
    pub(super) fn quiesce(&self) {
        if let Ok(mut state) = self.0.state.lock() {
            state.quiescing = true;
            // Disconnection terminates blocked producers; their cursor was never acknowledged.
            state.waiters.clear();
            self.0.changed.notify_all();
        }
    }
    pub(super) fn discard_quiesced_transport(&self) {
        if let Ok(mut state) = self.0.state.lock() {
            // The producer barrier proves there can be no more old-session input.
            // Unhandled cursors remain in the supervisor for the next attachment.
            state
                .queue
                .retain(|input| !matches!(input, AccountInput::Transport(_)));
            state.transport = 0;
        }
    }
    pub(super) fn reopen(&self) {
        if let Ok(mut state) = self.0.state.lock() {
            state.quiescing = false;
        }
    }
}
impl Receiver {
    pub(super) fn receive(
        &self,
        paused: bool,
        timeout: Option<Duration>,
    ) -> Result<AccountInput, mpsc::RecvTimeoutError> {
        let deadline = timeout.map(|d| Instant::now() + d);
        let mut state = self
            .0
            .state
            .lock()
            .map_err(|_| mpsc::RecvTimeoutError::Disconnected)?;
        loop {
            let control = state.queue.iter().position(|input| {
                matches!(
                    input,
                    AccountInput::Stop(..) | AccountInput::Restart | AccountInput::Shutdown
                )
            });
            let index = control.or_else(|| state.queue.iter().position(|input| !paused || !matches!(input,AccountInput::Transport(i) if !matches!(i,ProviderSessionInput::Quiesced))));
            if let Some(index) = index {
                let input = state
                    .queue
                    .remove(index)
                    .ok_or(mpsc::RecvTimeoutError::Disconnected)?;
                match &input {
                    AccountInput::Transport(ProviderSessionInput::Quiesced) => {}
                    AccountInput::Transport(_) => {
                        state.transport -= 1;
                        for waiter in state.waiters.drain(..) {
                            let _ignored = waiter.send(());
                        }
                    }
                    _ => state.controls -= 1,
                }
                self.0.changed.notify_all();
                return Ok(input);
            }
            if state.closed {
                return Err(mpsc::RecvTimeoutError::Disconnected);
            }
            if let Some(deadline) = deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(mpsc::RecvTimeoutError::Timeout);
                }
                state = self
                    .0
                    .changed
                    .wait_timeout(state, remaining)
                    .map_err(|_| mpsc::RecvTimeoutError::Disconnected)?
                    .0;
            } else {
                state = self
                    .0
                    .changed
                    .wait(state)
                    .map_err(|_| mpsc::RecvTimeoutError::Disconnected)?;
            }
        }
    }
    #[cfg(test)]
    pub(super) fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<AccountInput, mpsc::RecvTimeoutError> {
        self.receive(false, Some(timeout))
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.state.lock() {
            state.closed = true;
            state.waiters.clear();
            self.0.changed.notify_all();
        }
    }
}
