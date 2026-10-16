//! Transaction-only lifecycle composition. The account worker remains the sole
//! provider owner: adapters stage its existing durable effects, never send here.
use crate::agent_management::{Action, ManagementError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use studio_runtime_store::{RuntimeStore, Transaction};

pub type LifecycleCommitFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), ManagementError>> + Send + 'a>>;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleEffectKind {
    Interrupt,
    CancelResume,
    CancelMonitors,
    NotifyParent,
    ReleaseWork,
    FinalizeDeleted,
    VoiceStop,
    RemoteStop,
}
impl LifecycleEffectKind {
    pub fn dependency(self) -> &'static str {
        match self {
            Self::Interrupt => "account-worker/agent-interrupt",
            Self::CancelResume => "usage-policy/agent-stop",
            Self::CancelMonitors => "monitor-driver/agent-stop",
            Self::NotifyParent => "event-driver/child-stop-notification",
            Self::ReleaseWork => "work-driver/deletion-release",
            Self::FinalizeDeleted => "account-worker/deleted-settlement",
            Self::VoiceStop => "voice-driver/agent-stop",
            Self::RemoteStop => "cross-server/agent-management",
        }
    }
}

/// Created only from a checked snapshot while the writer transaction is held.
/// Preserve the complete attempt: submitted/Unknown input is never made pending.
#[derive(Clone, Debug, Serialize)]
pub struct LifecycleEffect {
    kind: LifecycleEffectKind,
    before: Value,
    stopped: Value,
    at: f64,
}
impl LifecycleEffect {
    pub(crate) fn new(kind: LifecycleEffectKind, before: Value, stopped: Value, at: f64) -> Self {
        Self {
            kind,
            before,
            stopped,
            at,
        }
    }
    pub fn kind(&self) -> LifecycleEffectKind {
        self.kind
    }
    pub fn before(&self) -> &Value {
        &self.before
    }
    pub fn stopped(&self) -> &Value {
        &self.stopped
    }
    pub fn at(&self) -> f64 {
        self.at
    }
}

/// Integrator ports for the existing worker, recovery, continuation, monitor,
/// notification and work services. All writes must use `tx`; no provider, file,
/// process, channel send, new receipt ledger, or external publication here.
///
/// Interrupt stages the existing account worker's effect using the saved attempt
/// and stopped epoch. It MUST NOT call SchedulerRepository::stop_agent again.
/// Persisted Pending/Unknown effects keep their exact account/connection/request
/// identity and are never resent on a retry. The account worker drains committed
/// effects after commit (including after restart); it alone owns transport ACKs.
///
/// FinalizeDeleted stages local cleanup after the original interrupt outcome;
/// it never declares Unknown input absent or terminal. P07 recovery is not called
/// by stop: its writer rechecks the complete changed agent and returns Superseded.
/// Cleanup may update the just-written agent, but must retain its stopped epoch,
/// autoWake=false and tombstone. NotifyParent uses the canonical child event;
/// it must never schedule a continuation of a stopped agent. A missing adapter
/// returns the kind's named Unavailable error and rolls the entire action back.
pub trait LifecyclePorts: Send + Sync {
    fn stage(&self, tx: &Transaction<'_>, effect: &LifecycleEffect) -> Result<(), ManagementError>;

    /// Save the provider stop identities before delete removes voice-session
    /// rows. Implementations dispatch them only after the transaction commits.
    fn stage_voice_delete(
        &self,
        tx: &Transaction<'_>,
        agent_id: &str,
    ) -> Result<(), ManagementError> {
        let has_sessions = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM voice_sessions \
             WHERE agent=?1 AND ended IS NULL AND native_thread IS NOT NULL \
             AND native_thread != '' AND connection_id IS NOT NULL AND connection_id != '')",
                [agent_id],
                |row| row.get::<_, bool>(0),
            )
            .map_err(studio_runtime_store::StoreError::from)?;
        if has_sessions {
            Err(ManagementError::Unavailable("voice-driver/agent-stop"))
        } else {
            Ok(())
        }
    }

    /// Wake the retained owners to drain their committed effects. This is not
    /// another Stop command and must not increment epochs or resend Unknown IO.
    fn committed(&self) -> LifecycleCommitFuture<'_>;
}

pub struct UnavailableLifecyclePorts;
impl LifecyclePorts for UnavailableLifecyclePorts {
    fn stage(&self, _: &Transaction<'_>, effect: &LifecycleEffect) -> Result<(), ManagementError> {
        Err(ManagementError::Unavailable(effect.kind.dependency()))
    }
    fn committed(&self) -> LifecycleCommitFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}

/// HTTP and managed tools share this consumer. Provider calls are deliberately
/// absent; a successful return means the state and durable effects committed.
pub async fn execute(
    store: std::sync::Arc<RuntimeStore>,
    action: &Action,
    now: f64,
    ports: std::sync::Arc<dyn LifecyclePorts>,
) -> Result<Value, ManagementError> {
    execute_with_prepared_hook(store, action, now, ports, || {}).await
}

async fn execute_with_prepared_hook(
    store: std::sync::Arc<RuntimeStore>,
    action: &Action,
    now: f64,
    ports: std::sync::Arc<dyn LifecyclePorts>,
    on_attempt_boundary: impl FnMut() + Send,
) -> Result<Value, ManagementError> {
    let result = crate::agent_management_store::execute_prepared(
        store,
        action,
        now,
        Some(ports.clone()),
        on_attempt_boundary,
    )
    .await?;
    // A lost wake cannot roll back the committed intent. Restart drains the same
    // effect; report an internal failure, never a no-write capability refusal.
    ports.committed().await.map_err(|_| {
        studio_runtime_store::StoreError::RecordContract(
            "Lifecycle committed; effect delivery remains pending",
        )
    })?;
    Ok(result)
}

/// Actor-bound managed interrupt. Authorization against the sender epoch and
/// descendant tree occurs inside the same transaction that stages the stop.
pub async fn stop_by_agent(
    store: std::sync::Arc<RuntimeStore>,
    sender: &str,
    sender_epoch: i64,
    target: &str,
    descendants: bool,
    now: f64,
    ports: std::sync::Arc<dyn LifecyclePorts>,
) -> Result<Value, ManagementError> {
    execute(
        store,
        &Action::Stop {
            target: target.to_owned(),
            descendants,
            reason: None,
            sender: Some(sender.to_owned()),
            sender_epoch: Some(sender_epoch),
        },
        now,
        ports,
    )
    .await
}

/// Reserved worker integration calls this instead of its local epoch mutation.
/// The caller owns commit and wakes/drains its existing effects only afterward.
pub fn execute_in_transaction(
    tx: &Transaction<'_>,
    action: &Action,
    now: f64,
    ports: &dyn LifecyclePorts,
    rooms: &crate::sync_entity_context::RoomDirectory,
    vault_snapshot: &crate::sync_policy::ProjectVaultSnapshot,
) -> Result<Value, ManagementError> {
    crate::agent_management_store::execute_with_ports(tx, action, now, rooms, vault_snapshot, ports)
}

#[cfg(test)]
mod preparation_retry_tests {
    use super::*;
    use crate::agent_lifecycle_runtime::LifecycleEffect;
    use crate::agent_management::ManagementError;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use studio_runtime_store::{RuntimeStore, agent_management};

    fn start_interleaving_writer(
        store: std::sync::Arc<RuntimeStore>,
    ) -> (
        tokio::sync::mpsc::UnboundedSender<usize>,
        std::sync::Arc<std::sync::Mutex<mpsc::Receiver<()>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let (change_tx, mut change_rx) = tokio::sync::mpsc::unbounded_channel();
        let (committed_tx, committed_rx) = mpsc::channel();
        let writer = tokio::spawn(async move {
            while let Some(attempt) = change_rx.recv().await {
                store
                    .transaction(move |tx| {
                        tx.execute(
                            "UPDATE runtime_agents SET record=json_set(record,'$.name',?1) WHERE id='lead'",
                            [format!("change-{attempt}")],
                        )?;
                        Ok(())
                    })
                    .await
                    .expect("concurrent update commits between prepared attempts");
                committed_tx
                    .send(())
                    .expect("lifecycle test waits for each interleaved commit");
            }
        });
        (
            change_tx,
            std::sync::Arc::new(std::sync::Mutex::new(committed_rx)),
            writer,
        )
    }

    fn interleave(
        attempt: usize,
        changes: &tokio::sync::mpsc::UnboundedSender<usize>,
        committed: &std::sync::Mutex<mpsc::Receiver<()>>,
    ) {
        changes
            .send(attempt)
            .expect("concurrent writer task remains active");
        committed
            .lock()
            .expect("commit receiver lock")
            .recv()
            .expect("concurrent writer commits before lifecycle retry");
    }

    struct NoopPorts;
    impl LifecyclePorts for NoopPorts {
        fn stage(&self, _: &Transaction<'_>, _: &LifecycleEffect) -> Result<(), ManagementError> {
            Ok(())
        }

        fn committed(&self) -> LifecycleCommitFuture<'_> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retries_when_agent_changes_during_filesystem_preparation() {
        let root = tempfile::tempdir().expect("state dir");
        let store = std::sync::Arc::new(
            RuntimeStore::open(Some(root.path()))
                .await
                .expect("store opens"),
        );
        store
            .transaction(|tx| {
                agent_management::write(
                    tx,
                    "runtime_agents",
                    &json!({"id":"lead","rootId":"lead","epoch":0,"status":"idle","isLead":true}),
                )?;
                Ok(())
            })
            .await
            .expect("agent seed");

        let preparations = AtomicUsize::new(0);
        let (changes, committed, writer) = start_interleaving_writer(store.clone());
        let hook_changes = changes.clone();
        let result = execute_with_prepared_hook(
            store.clone(),
            &Action::Stop {
                target: "lead".to_owned(),
                descendants: true,
                reason: None,
                sender: None,
                sender_epoch: None,
            },
            1_800_000_000.0,
            std::sync::Arc::new(NoopPorts),
            || {
                if preparations.fetch_add(1, Ordering::SeqCst) == 0 {
                    interleave(0, &hook_changes, &committed);
                }
            },
        )
        .await
        .expect("stop retries with current lifecycle inputs");
        drop(hook_changes);
        drop(changes);
        writer.await.expect("interleaving writer task exits");

        assert_eq!(
            preparations.load(Ordering::SeqCst),
            2,
            "one stale snapshot is retried once"
        );
        assert_eq!(result["stopped"], json!(["lead"]));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn final_writer_admitted_attempt_succeeds_after_two_snapshot_conflicts() {
        let root = tempfile::tempdir().expect("state dir");
        let store = std::sync::Arc::new(
            RuntimeStore::open(Some(root.path()))
                .await
                .expect("store opens"),
        );
        store
            .transaction(|tx| {
                agent_management::write(
                    tx,
                    "runtime_agents",
                    &json!({"id":"lead","rootId":"lead","epoch":0,"status":"idle","isLead":true}),
                )?;
                Ok(())
            })
            .await
            .expect("agent seed");

        let preparations = AtomicUsize::new(0);
        let (changes, committed, writer) = start_interleaving_writer(store.clone());
        let hook_changes = changes.clone();
        let result = execute_with_prepared_hook(
            store.clone(),
            &Action::Stop {
                target: "lead".to_owned(),
                descendants: true,
                reason: None,
                sender: None,
                sender_epoch: None,
            },
            1_800_000_000.0,
            std::sync::Arc::new(NoopPorts),
            || {
                let attempt = preparations.fetch_add(1, Ordering::SeqCst);
                interleave(attempt, &hook_changes, &committed);
            },
        )
        .await
        .expect("last attempt captures under writer admission and stops current state");
        drop(hook_changes);
        drop(changes);
        writer.await.expect("interleaving writer task exits");

        assert_eq!(
            preparations.load(Ordering::SeqCst),
            3,
            "bound is three total attempts"
        );
        assert_eq!(result["stopped"], json!(["lead"]));
        let reader = store.read().expect("reader");
        let agent = agent_management::read(&reader).expect("records")["runtime_agents"][0].clone();
        assert_eq!(agent["status"], "paused");
        assert_eq!(agent["epoch"], 1);
    }
}
