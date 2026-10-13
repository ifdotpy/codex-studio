//! Shared workspace idle proof, then compare and reserve in one store transaction.
use super::{service::*, *};
use crate::checkpoints;
use std::sync::Arc;
use studio_runtime_store::{
    CheckpointsRepository, ClaudeControlRepository as Repository, RuntimeStore, Transaction,
};
pub struct StoredControls {
    store: Arc<RuntimeStore>,
    unload: Arc<dyn Fn(&str) + Send + Sync>,
}
impl StoredControls {
    pub fn new(store: Arc<RuntimeStore>, unload: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        Self { store, unload }
    }
}
fn store_error(_: studio_runtime_store::StoreError) -> ControlError {
    ControlError::Unavailable
}
impl From<studio_runtime_store::StoreError> for ControlError {
    fn from(error: studio_runtime_store::StoreError) -> Self {
        store_error(error)
    }
}
fn current(tx: &Transaction<'_>, expected: &ControlSnapshot) -> Result<ControlSnapshot> {
    let operation = expected.operation.as_ref().ok_or(ControlError::Changed)?;
    let (agent, saved) =
        Repository::load(tx, &expected.agent.id, &operation.id)?.ok_or(ControlError::Changed)?;
    let snapshot = ControlSnapshot::from_records(agent, saved)?;
    if snapshot.source() != expected.source()
        || snapshot.epoch() != expected.epoch()
        || snapshot.saved != expected.saved
    {
        return Err(ControlError::Changed);
    }
    snapshot.check_owned(snapshot.saved.as_ref().ok_or(ControlError::Changed)?)?;
    Ok(snapshot)
}
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| ControlError::Unavailable)?
}
fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |v| v.as_secs_f64())
}
impl ControlStorage for StoredControls {
    fn reserve(
        &self,
        request: ControlRequest,
        binding: ControlBinding,
    ) -> ControlFuture<'_, ControlSnapshot> {
        let store = self.store.clone();
        Box::pin(async move {
            blocking(move || {
                let workspace = checkpoints::capture_context(&store, &request.id)
                    .map_err(|_| ControlError::Unavailable)?;
                let saved = store
                    .transaction(|tx| Repository::load(tx, &request.id, &request.operation_id()))?;
                let is_new = saved.as_ref().is_some_and(|(_, op)| op.is_none());
                if is_new {
                    checkpoints::ensure_workspace_idle(&workspace, None)
                        .map_err(|_| ControlError::Busy)?;
                }
                let reserved = store.transaction_with(|tx| {
                    let (agent, operation) =
                        Repository::load(tx, &request.id, &request.operation_id())?
                            .ok_or(ControlError::Changed)?;
                    if operation.is_none()
                        && (!is_new || !CheckpointsRepository::workspace_unchanged(tx, &workspace)?)
                    {
                        return Err(ControlError::Changed);
                    }
                    let snapshot = ControlSnapshot::from_records(agent, operation)?;
                    if snapshot.agent.account_key != binding.account() {
                        return Err(ControlError::Changed);
                    }
                    let ticket =
                        serde_json::to_value(&binding).map_err(|_| ControlError::Changed)?;
                    let mut writes = match snapshot.reserve(&request, Repository::pending(tx, &request.id)?, now()) {
                        Ok(writes) => writes,
                        Err(ControlError::Unknown) => {
                            snapshot.check_owned(snapshot.saved.as_ref().ok_or(ControlError::Changed)?)?;
                            return Ok(Some(snapshot));
                        }
                        Err(error) => return Err(error),
                    };
                    if let Some(saved) = &snapshot.saved
                        && saved["phase"] != "completed"
                        && (saved.get("controlEpoch").and_then(Value::as_u64) != Some(snapshot.epoch())
                            || (saved.get("provider").is_none_or(Value::is_null) && saved.get("controlTransport") != Some(&ticket))) {
                        snapshot.check_owned(saved)?;
                        writes.operation["phase"] = json!("recovery_required");
                        writes.operation["error"] = json!("Claude control transport changed; inspect workspace recovery");
                        writes.agent["workspaceOperation"] = json!("branch_recovery");
                        writes.agent["status"] = json!("paused");
                        writes.agent["error"] = json!({"message":"Claude control transport changed; inspect workspace recovery"});
                        Repository::apply(tx, &writes.agent, &writes.operation, &[])?;
                        return Ok(None);
                    }
                    if snapshot.saved.is_none() {
                        writes.operation["controlTransport"] = ticket;
                        writes.operation["controlEpoch"] = json!(snapshot.epoch());
                    }

                    Repository::apply(tx, &writes.agent, &writes.operation, &[])?;
                    ControlSnapshot::from_records(writes.agent, Some(writes.operation)).map(Some)
                })?;
                reserved.ok_or(ControlError::Unknown)
            })
            .await
        })
    }
    fn submitted(
        &self,
        expected: ControlSnapshot,
        request: ControlRequest,
        native: Value,
        queue: Value,
    ) -> ControlFuture<'_, ControlSnapshot> {
        let store = self.store.clone();
        Box::pin(async move {
            blocking(move || {
                store.transaction_with(|tx| {
                    let current = current(tx, &expected)?;
                    let operation = current.submitted(&request, &native, &queue, now())?;
                    Repository::apply(tx, &current.raw, &operation, &[])?;
                    ControlSnapshot::from_records(current.raw, Some(operation))
                })
            })
            .await
        })
    }
    fn provider(
        &self,
        expected: ControlSnapshot,
        result: Value,
    ) -> ControlFuture<'_, ControlSnapshot> {
        let store = self.store.clone();
        Box::pin(async move {
            blocking(move || {
                store.transaction_with(|tx| {
                    let current = current(tx, &expected)?;
                    let operation = current.provider_ready(result, now())?;
                    Repository::apply(tx, &current.raw, &operation, &[])?;
                    ControlSnapshot::from_records(current.raw, Some(operation))
                })
            })
            .await
        })
    }
    fn complete(
        &self,
        expected: ControlSnapshot,
        request: ControlRequest,
    ) -> ControlFuture<'_, Value> {
        let store = self.store.clone();
        let unload = self.unload.clone();
        Box::pin(async move {
            blocking(move || {
                let id = expected.agent.id.clone();
                let result = store.transaction_with(|tx| {
                    let current = current(tx, &expected)?;
                    let writes = current.complete(&request, now())?;
                    Repository::apply(tx, &writes.agent, &writes.operation, &writes.removed)?;
                    Ok::<_, ControlError>(writes.result)
                })?;
                unload(&id);
                Ok(result)
            })
            .await
        })
    }
    fn failed(&self, expected: ControlSnapshot, uncertain: bool) -> ControlFuture<'_, ()> {
        let store = self.store.clone();
        Box::pin(async move {
            blocking(move||store.transaction_with(|tx|{
  let Some(op)=&expected.operation else{return Ok(())};
  let Some((mut agent,Some(mut operation)))=Repository::load(tx,&expected.agent.id,&op.id)?else{return Err(ControlError::Changed)};
  if operation["phase"]=="completed"{return Ok(())}
  if agent.get("epoch").and_then(Value::as_u64)!=Some(expected.epoch()) || CheckpointsRepository::workspace_source(&agent)!=expected.source() || agent.get("workspaceReservationId")!=operation.get("id")||operation.get("source")!=Some(&expected.source()){return Err(ControlError::Changed)}
  let unknown=uncertain||truthy(operation.get("submitted"))||truthy(operation.get("provider"));
  operation["phase"]=json!(if unknown{"recovery_required"}else{"failed"});operation["error"]=json!(if unknown{"Claude control outcome is unknown; inspect workspace recovery"}else{"Claude action was not submitted"});operation["updated"]=json!(now());
  agent["workspaceOperation"]=if unknown{json!("branch_recovery")}else{Value::Null};
  if unknown{agent["status"]=json!("paused");agent["error"]=json!({"message":"Claude control outcome is unknown; inspect workspace recovery"});}else{agent.as_object_mut().ok_or(ControlError::Changed)?.remove("workspaceReservationId");}
  Repository::apply(tx,&agent,&operation,&[])?;Ok(())
 })).await
        })
    }
}
