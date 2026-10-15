//! Transaction-only lifecycle composition. The account worker remains the sole
//! provider owner: adapters stage its existing durable effects, never send here.
use crate::agent_management::{Action, ManagementError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use studio_runtime_store::{RuntimeStore, Transaction};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleEffectKind {
    Interrupt,
    CancelResume,
    CancelMonitors,
    NotifyParent,
    ReleaseWork,
    FinalizeDeleted,
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
    fn committed(&self) -> Result<(), ManagementError>;
}

pub struct UnavailableLifecyclePorts;
impl LifecyclePorts for UnavailableLifecyclePorts {
    fn stage(&self, _: &Transaction<'_>, effect: &LifecycleEffect) -> Result<(), ManagementError> {
        Err(ManagementError::Unavailable(effect.kind.dependency()))
    }
    fn committed(&self) -> Result<(), ManagementError> {
        Ok(())
    }
}

/// HTTP and managed tools share this consumer. Provider calls are deliberately
/// absent; a successful return means the state and durable effects committed.
pub fn execute(
    store: &RuntimeStore,
    action: &Action,
    now: f64,
    ports: &dyn LifecyclePorts,
) -> Result<Value, ManagementError> {
    let reader = store.read()?;
    let rooms = crate::sync_entity_context::RoomDirectory::prepare(&reader)?;
    let projects = studio_runtime_store::ProjectsRepository::list(&reader)?;
    let vault_snapshot = crate::sync_policy::ProjectVaultSnapshot::capture(store.root(), &projects);
    // The lifecycle transaction and committed-effect wake happen after these
    // immutable inputs have been captured; do not retain a pooled reader across
    // that work, which may perform its own store reads.
    drop(reader);
    let result = store.transaction_with_error(|tx| {
        execute_in_transaction(tx, action, now, ports, &rooms, &vault_snapshot)
    })?;
    // A lost wake cannot roll back the committed intent. Restart drains the same
    // effect; report an internal failure, never a no-write capability refusal.
    ports.committed().map_err(|_| {
        studio_runtime_store::StoreError::RecordContract(
            "Lifecycle committed; effect delivery remains pending",
        )
    })?;
    Ok(result)
}

/// Actor-bound managed interrupt. Authorization against the sender epoch and
/// descendant tree occurs inside the same transaction that stages the stop.
pub fn stop_by_agent(
    store: &RuntimeStore,
    sender: &str,
    sender_epoch: i64,
    target: &str,
    descendants: bool,
    now: f64,
    ports: &dyn LifecyclePorts,
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
