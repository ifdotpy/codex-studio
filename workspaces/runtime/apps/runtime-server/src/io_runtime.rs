//! Owns local terminal sessions for the lifetime of the server.

#[cfg(unix)]
use std::sync::{Arc, Weak};

#[cfg(unix)]
use studio_runtime_services::terminal_driver::{
    SessionError, TerminalDriver, TerminalError, TerminalNotification,
};
#[cfg(unix)]
use studio_runtime_services::terminal_pty::PtyTerminalSession;
#[cfg(unix)]
use studio_runtime_store::RuntimeStore;

#[cfg(unix)]
#[derive(Clone)]
pub struct IoRuntime {
    terminals: Arc<TerminalDriver<PtyTerminalSession>>,
}

#[cfg(unix)]
impl IoRuntime {
    pub fn start(store: Arc<RuntimeStore>) -> Result<Self, TerminalError> {
        let session = PtyTerminalSession::new();
        let terminals = Arc::new(TerminalDriver::new(store, session.clone()));
        let weak: Weak<_> = Arc::downgrade(&terminals);
        session.set_notification_callback(Arc::new(move |notification: TerminalNotification| {
            if let Some(terminals) = weak.upgrade() {
                let _apply_result = terminals.apply_notification(notification);
            }
        }));
        let weak = Arc::downgrade(&terminals);
        session.set_identity_callback(Arc::new(move |id, identity| {
            let terminals = weak.upgrade().ok_or(SessionError::Disconnected)?;
            terminals
                .persist_process_identity(id, identity)
                .map_err(|_| SessionError::Disconnected)
        }));
        terminals.reconcile_after_restart()?;
        Ok(Self { terminals })
    }

    pub fn terminal_driver(&self) -> Arc<TerminalDriver<PtyTerminalSession>> {
        Arc::clone(&self.terminals)
    }

    pub fn shutdown(&self) -> Result<(), studio_runtime_services::terminal_driver::TerminalError> {
        self.terminals.close_session()
    }
}
