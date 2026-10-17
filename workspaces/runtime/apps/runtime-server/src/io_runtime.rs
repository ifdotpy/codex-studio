//! Owns local terminal sessions for the lifetime of the server.

#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};
#[cfg(unix)]
use tokio::task::JoinHandle;

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
    persistence: mpsc::Sender<TerminalPersistence>,
    persistence_task: AsyncMutex<Option<JoinHandle<()>>>,
    shutdown: AsyncMutex<()>,
    reconciliation: AsyncMutex<Reconciliation>,
}

#[cfg(unix)]
struct Reconciliation {
    complete: bool,
    diagnostic_reported: bool,
}

#[cfg(unix)]
const MAX_NOTIFICATION_PERSIST_ATTEMPTS: usize = 5;
#[cfg(unix)]
const TERMINAL_PERSISTENCE_QUEUE_CAPACITY: usize = 64;

#[cfg(unix)]
enum TerminalPersistence {
    Notification(
        TerminalNotification,
        oneshot::Sender<Result<(), TerminalError>>,
    ),
    Identity(
        String,
        Option<studio_runtime_process::PtyIdentity>,
        oneshot::Sender<Result<(), TerminalError>>,
    ),
    Stop,
}

#[cfg(unix)]
impl IoRuntime {
    pub async fn start(store: Arc<RuntimeStore>) -> Result<Self, TerminalError> {
        let session = PtyTerminalSession::new();
        let terminals = Arc::new(TerminalDriver::new(store, session.clone()));
        let mut reconciliation = Reconciliation {
            complete: true,
            diagnostic_reported: false,
        };
        if let Err(error) = terminals.reconcile_after_restart().await {
            if matches!(&error, TerminalError::Store(_)) {
                return Err(error);
            }
            reconciliation.complete = false;
            Self::report_reconciliation_failure(&mut reconciliation, &error);
        }
        let (persistence, receiver) = mpsc::channel(TERMINAL_PERSISTENCE_QUEUE_CAPACITY);
        let persistence_task = tokio::spawn(run_terminal_persistence_owner(
            Arc::clone(&terminals),
            receiver,
        ));
        let notification_sender = persistence.clone();
        session.set_notification_callback(Arc::new(move |notification| {
            let mut pending = Some(notification);
            persist_notification_until_saved(&mut pending, |notification| {
                persist_notification_on_owner(&notification_sender, notification)
            })
            .map_err(|_| SessionError::Disconnected)
        }));
        let identity_sender = persistence.clone();
        session.set_identity_callback(Arc::new(move |id, identity| {
            persist_identity_on_owner(&identity_sender, id, identity.cloned())
                .map_err(|_| SessionError::Disconnected)
        }));
        Ok(Self {
            terminals,
            persistence,
            persistence_task: AsyncMutex::new(Some(persistence_task)),
            shutdown: AsyncMutex::new(()),
            reconciliation: AsyncMutex::new(reconciliation),
        })
    }

    pub fn terminal_driver(&self) -> Arc<TerminalDriver<PtyTerminalSession>> {
        Arc::clone(&self.terminals)
    }

    pub async fn reconcile_before_write(&self) -> bool {
        let mut reconciliation = self.reconciliation.lock().await;
        if reconciliation.complete {
            return true;
        }
        match self.terminals.reconcile_after_restart().await {
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

    pub async fn shutdown(&self) -> Result<(), TerminalError> {
        let _shutdown = self.shutdown.lock().await;
        let close_result = self.terminals.close_session().await;
        self.shutdown_persistence_owner(close_result).await
    }

    async fn shutdown_persistence_owner(
        &self,
        close_result: Result<(), TerminalError>,
    ) -> Result<(), TerminalError> {
        let task = { self.persistence_task.lock().await.take() };
        let owner_result = if let Some(task) = task {
            // Stop even after a PTY close failure: the owner processes jobs
            // already ahead of Stop, then dropping its receiver releases any
            // racing callback with its existing disconnected/unknown path.
            let stop_result = self
                .persistence
                .send(TerminalPersistence::Stop)
                .await
                .map_err(|_| TerminalError::Session(SessionError::Disconnected));
            let join_result = task.await.map_err(|_| TerminalError::BlockingTask);
            stop_result.and(join_result)
        } else {
            Ok(())
        };
        close_result?;
        owner_result
    }
}

#[cfg(unix)]
async fn run_terminal_persistence_owner(
    terminals: Arc<TerminalDriver<PtyTerminalSession>>,
    mut receiver: mpsc::Receiver<TerminalPersistence>,
) {
    while let Some(operation) = receiver.recv().await {
        match operation {
            TerminalPersistence::Notification(notification, reply) => {
                let result = terminals.apply_notification(notification).await;
                let _ignored_reply = reply.send(result);
            }
            TerminalPersistence::Identity(id, identity, reply) => {
                let result = terminals
                    .persist_process_identity(&id, identity.as_ref())
                    .await;
                let _ignored_reply = reply.send(result);
            }
            TerminalPersistence::Stop => break,
        }
    }
}

#[cfg(unix)]
fn persist_notification_on_owner(
    sender: &mpsc::Sender<TerminalPersistence>,
    notification: TerminalNotification,
) -> Result<(), TerminalError> {
    // PtyTerminalSession invokes this only on its reader or driver blocking
    // thread. Wait there for the actor-backed async persistence result; never
    // block_on a Tokio future from the callback thread.
    let (reply, result) = oneshot::channel();
    sender
        .blocking_send(TerminalPersistence::Notification(notification, reply))
        .map_err(|_| TerminalError::Session(SessionError::Disconnected))?;
    result
        .blocking_recv()
        .map_err(|_| TerminalError::Session(SessionError::Disconnected))?
}

#[cfg(unix)]
fn persist_identity_on_owner(
    sender: &mpsc::Sender<TerminalPersistence>,
    id: &str,
    identity: Option<studio_runtime_process::PtyIdentity>,
) -> Result<(), TerminalError> {
    let (reply, result) = oneshot::channel();
    sender
        .blocking_send(TerminalPersistence::Identity(
            id.to_owned(),
            identity,
            reply,
        ))
        .map_err(|_| TerminalError::Session(SessionError::Disconnected))?;
    result
        .blocking_recv()
        .map_err(|_| TerminalError::Session(SessionError::Disconnected))?
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
    use studio_runtime_store::{TerminalRecord, TerminalsRepository};

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

    #[tokio::test]
    async fn callback_completion_and_owner_drain_survive_terminal_close_failure() {
        let directory = tempfile::tempdir().expect("state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("runtime store"),
        );
        let runtime = IoRuntime::start(Arc::clone(&store))
            .await
            .expect("IO runtime");
        let record = TerminalRecord::from_value(serde_json::json!({
            "id": "terminal-fixture",
            "agent": "agent-fixture",
            "title": "Terminal",
            "cwd": "/tmp",
            "status": "running",
            "created": 1.0,
            "updated": 1.0,
            "exitCode": null
        }))
        .expect("typed terminal record");
        store
            .transaction(move |transaction| {
                TerminalsRepository::put(transaction, "terminal-fixture", &record, "", 0)
            })
            .await
            .expect("terminal fixture");

        let bridge_identity = studio_runtime_process::PtyIdentity {
            pid: 41,
            process_group: 41,
            session: 41,
            start_time_ticks: 123,
            boot_id: "fixture-boot".into(),
        };
        let bridge_sender = runtime.persistence.clone();
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-server/io_runtime",
            move || {
                persist_identity_on_owner(&bridge_sender, "terminal-fixture", Some(bridge_identity))
            },
        )
        .await
        .expect("blocking callback bridge")
        .expect("callback persisted identity");

        let queued_identity = studio_runtime_process::PtyIdentity {
            pid: 42,
            process_group: 42,
            session: 42,
            start_time_ticks: 124,
            boot_id: "fixture-boot".into(),
        };
        let expected_identity = serde_json::to_value(&queued_identity).expect("identity JSON");
        let (reply, completion) = oneshot::channel();
        runtime
            .persistence
            .send(TerminalPersistence::Identity(
                "terminal-fixture".into(),
                Some(queued_identity),
                reply,
            ))
            .await
            .expect("queue identity callback");

        let close_error = runtime
            .shutdown_persistence_owner(Err(TerminalError::Session(SessionError::Disconnected)))
            .await
            .expect_err("preserve PTY close failure");
        assert!(matches!(
            close_error,
            TerminalError::Session(SessionError::Disconnected)
        ));
        completion
            .await
            .expect("callback response")
            .expect("identity write completed before owner stop");
        assert!(runtime.persistence_task.lock().await.is_none());
        assert!(
            runtime
                .persistence
                .send(TerminalPersistence::Stop)
                .await
                .is_err()
        );
        let saved =
            TerminalsRepository::get(&store.read().expect("store read"), "terminal-fixture")
                .expect("terminal read")
                .expect("terminal exists");
        assert_eq!(
            saved.record.get("_processIdentity"),
            Some(&expected_identity)
        );
    }
}
