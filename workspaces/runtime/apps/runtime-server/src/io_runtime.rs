//! Owns local terminal sessions for the lifetime of the server.

#[cfg(unix)]
use std::sync::{Arc, Mutex, Weak};

#[cfg(unix)]
use studio_runtime_services::terminal_driver::{
    SessionError, TerminalDriver, TerminalError, TerminalNotification,
};
#[cfg(unix)]
use studio_runtime_services::terminal_pty::PtyTerminalSession;
#[cfg(unix)]
use studio_runtime_store::RuntimeStore;

#[cfg(unix)]
pub struct IoRuntime {
    terminals: Arc<TerminalDriver<PtyTerminalSession>>,
    reconciliation: Mutex<Reconciliation>,
}

#[cfg(unix)]
struct Reconciliation {
    complete: bool,
    diagnostic_reported: bool,
}

#[cfg(unix)]
const MAX_NOTIFICATION_PERSIST_ATTEMPTS: usize = 5;

#[cfg(unix)]
impl IoRuntime {
    pub fn start(store: Arc<RuntimeStore>) -> Result<Self, TerminalError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| TerminalError::StartupRecovery("system clock unavailable".into()))?
            .as_secs_f64();
        studio_runtime_services::monitor_runtime::recover_on_startup(&store, now)
            .map_err(|error| TerminalError::StartupRecovery(error.to_string()))?;
        let session = PtyTerminalSession::new();
        let terminals = Arc::new(TerminalDriver::new(store, session.clone()));
        let weak: Weak<_> = Arc::downgrade(&terminals);
        session.set_notification_callback(Arc::new(move |notification: TerminalNotification| {
            if let Some(terminals) = weak.upgrade() {
                let mut pending = Some(notification);
                persist_notification_until_saved(&mut pending, |notification| {
                    terminals.apply_notification(notification)
                })
                .map_err(|_| SessionError::Disconnected)
            } else {
                Err(SessionError::Disconnected)
            }
        }));
        let weak = Arc::downgrade(&terminals);
        session.set_identity_callback(Arc::new(move |id, identity| {
            let terminals = weak.upgrade().ok_or(SessionError::Disconnected)?;
            terminals
                .persist_process_identity(id, identity)
                .map_err(|_| SessionError::Disconnected)
        }));
        let mut reconciliation = Reconciliation {
            complete: true,
            diagnostic_reported: false,
        };
        if let Err(error) = terminals.reconcile_after_restart() {
            if matches!(&error, TerminalError::Store(_)) {
                return Err(error);
            }
            reconciliation.complete = false;
            Self::report_reconciliation_failure(&mut reconciliation, &error);
        }
        Ok(Self {
            terminals,
            reconciliation: Mutex::new(reconciliation),
        })
    }

    pub fn terminal_driver(&self) -> Arc<TerminalDriver<PtyTerminalSession>> {
        Arc::clone(&self.terminals)
    }

    pub fn reconcile_before_write(&self) -> bool {
        let Ok(mut reconciliation) = self.reconciliation.lock() else {
            return false;
        };
        if reconciliation.complete {
            return true;
        }
        match self.terminals.reconcile_after_restart() {
            Ok(_) => {
                reconciliation.complete = true;
                true
            }
            Err(_) => false,
        }
    }

    fn report_reconciliation_failure(reconciliation: &mut Reconciliation, error: &TerminalError) {
        if !reconciliation.diagnostic_reported {
            let kind = match error {
                TerminalError::Session(_) => "session",
                TerminalError::InvalidStoredObject(_) => "stored-terminal",
                TerminalError::Store(_) => "store",
                _ => "terminal",
            };
            eprintln!("terminal restart reconciliation failed (kind={kind})");
            reconciliation.diagnostic_reported = true;
        }
    }

    pub fn shutdown(&self) -> Result<(), studio_runtime_services::terminal_driver::TerminalError> {
        self.terminals.close_session()
    }
}

#[cfg(unix)]
fn persist_notification_until_saved(
    pending: &mut Option<TerminalNotification>,
    mut persist: impl FnMut(TerminalNotification) -> Result<(), TerminalError>,
) -> Result<(), TerminalError> {
    let mut delay = std::time::Duration::from_millis(10);
    for attempt in 0..MAX_NOTIFICATION_PERSIST_ATTEMPTS {
        let notification = pending
            .as_ref()
            .expect("notification remains present until persisted")
            .clone();
        match persist(notification) {
            Ok(()) => {
                *pending = None;
                return Ok(());
            }
            Err(error) => {
                eprintln!(
                    "Could not persist terminal notification (attempt {}/{MAX_NOTIFICATION_PERSIST_ATTEMPTS}); retaining it for retry: {error}",
                    attempt + 1
                );
                if attempt + 1 < MAX_NOTIFICATION_PERSIST_ATTEMPTS {
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(std::time::Duration::from_secs(1));
                } else {
                    return Err(error);
                }
            }
        }
    }
    Err(TerminalError::InvalidRequest)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn terminal_notification_is_retained_until_persistence_succeeds() {
        let attempts = AtomicUsize::new(0);
        let notification = TerminalNotification {
            terminal_id: "terminal".into(),
            sequence: 3,
            event: studio_runtime_services::terminal_driver::TerminalEvent::Output(
                b"chunk".to_vec(),
            ),
        };
        let mut pending = Some(notification);
        persist_notification_until_saved(&mut pending, |retry| {
            assert_eq!(retry.terminal_id, "terminal");
            assert_eq!(retry.sequence, 3);
            assert!(
                matches!(retry.event, studio_runtime_services::terminal_driver::TerminalEvent::Output(bytes) if bytes == b"chunk")
            );
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(TerminalError::InvalidRequest)
            } else {
                Ok(())
            }
        })
        .expect("fail-once callback retry succeeds");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(pending.is_none(), "persisted notification is retired");
    }

    #[test]
    fn terminal_notification_retry_is_bounded_and_retains_failed_event() {
        let mut pending = Some(TerminalNotification {
            terminal_id: "terminal".into(),
            sequence: 9,
            event: studio_runtime_services::terminal_driver::TerminalEvent::Output(
                b"retained".to_vec(),
            ),
        });
        let attempts = AtomicUsize::new(0);
        let result = persist_notification_until_saved(&mut pending, |retry| {
            assert_eq!(retry.terminal_id, "terminal");
            assert_eq!(retry.sequence, 9);
            assert!(
                matches!(retry.event, studio_runtime_services::terminal_driver::TerminalEvent::Output(bytes) if bytes == b"retained")
            );
            attempts.fetch_add(1, Ordering::SeqCst);
            Err(TerminalError::InvalidRequest)
        });

        assert!(result.is_err());
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            MAX_NOTIFICATION_PERSIST_ATTEMPTS
        );
        assert_eq!(pending.as_ref().map(|event| event.sequence), Some(9));
    }
}
