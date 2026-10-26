//! Public registry views use the existing vault and immutable service contexts.
use crate::service_context::Context;
use serde_json::{Value, json};
use std::{future::Future, sync::Arc};
use studio_client_core::server_transport::RemoteClient;

pub(crate) fn snapshot(
    remote: &Arc<RemoteClient>,
    local: &Context,
    selected: &Context,
) -> Result<Value, String> {
    let workspace = |value: &str| {
        value.len() == 32
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    if local.server_id != "local"
        || !workspace(&local.workspace_id)
        || !workspace(&selected.workspace_id)
    {
        return Err("The registry needs exact verified service contexts.".into());
    }
    let local_backend = local
        .backend
        .as_ref()
        .ok_or("The local server context is unavailable.")?;
    local_backend.validate_identity()?;
    let local_identity = local_backend.public_identity();
    if local_backend.remote_server_id().is_some()
        || !local_identity["credentialId"].is_null()
        || !local_identity["origin"].is_string()
    {
        return Err("A remote pairing cannot provide the local registry identity.".into());
    }
    let selected_backend = selected
        .backend
        .as_ref()
        .ok_or("The selected server context is unavailable.")?;
    selected_backend.validate_identity()?;
    let selected_identity = selected_backend.public_identity();
    let current = remote.registry_snapshot()?;
    let mut rows = current["servers"]
        .as_array()
        .ok_or("The native registry is invalid.")?
        .clone();
    if selected.server_id == "local" {
        if selected_backend.remote_server_id().is_some()
            || selected.workspace_id != local.workspace_id
            || selected_identity != local_identity
        {
            return Err("The selected local registry context changed.".into());
        }
    } else {
        if selected_backend.remote_server_id() != Some(selected.server_id.as_str()) {
            return Err("The selected server ID differs from its exact pairing.".into());
        }
        let row = rows
            .iter_mut()
            .find(|row| row["id"] == selected.server_id)
            .ok_or("The selected pairing was removed.")?;
        if row["origin"] != selected_identity["origin"]
            || row["credentialId"] != selected_identity["credentialId"]
        {
            return Err("The selected pairing no longer matches its context.".into());
        }
        row["workspaceId"] = json!(selected.workspace_id);
    }
    // A credential can change during the checks. Never publish a mixed revision.
    selected_backend.validate_identity()?;
    if remote.registry_snapshot()? != current {
        return Err("The registry changed during its context check.".into());
    }
    Ok(
        json!({"serverId":selected.server_id,"workspaceId":selected.workspace_id,"revision":current["revision"],
        "local":{"id":"local","label":"This device","origin":local_identity["origin"],"workspaceId":local.workspace_id},"servers":rows}),
    )
}

pub(crate) fn event(handle: &str, snapshot: Result<Value, String>) -> Value {
    let topic = studio_client_core::server_keys::registry_contract()["eventTopic"]
        .as_str()
        .expect("registry event topic");
    let name = format!("{handle}:{topic}");
    match snapshot {
        Ok(value) => json!({"event":name,"value":value}),
        Err(message) => json!({"event":name,"error":{"message":message}}),
    }
}

/// One host-owned relay. The existing native bus carries full snapshots.
pub(crate) struct Relay(tokio::task::JoinHandle<()>);
impl Drop for Relay {
    fn drop(&mut self) {
        self.0.abort();
    }
}
pub(crate) fn relay<F, Fut, E>(
    mut changes: tokio::sync::watch::Receiver<String>,
    snapshots: F,
    emit: E,
) -> Relay
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Vec<(String, Result<Value, String>)>> + Send,
    E: Fn(Value) + Send + Sync + 'static,
{
    Relay(tokio::spawn(async move {
        while changes.changed().await.is_ok() {
            for (handle, value) in snapshots().await {
                emit(event(&handle, value));
            }
        }
    }))
}

#[cfg(test)]
#[path = "native_registry_tests.rs"]
mod tests;
