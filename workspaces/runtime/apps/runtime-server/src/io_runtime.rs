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
                let mut delay = std::time::Duration::from_millis(10);
                loop {
                    match terminals.apply_notification(notification.clone()) {
                        Ok(()) => break,
                        Err(error) => {
                            eprintln!("Could not persist terminal notification; retaining it for retry: {error}");
                            std::thread::sleep(delay);
                            delay = (delay * 2).min(std::time::Duration::from_secs(1));
                        }
                    }
                }
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
