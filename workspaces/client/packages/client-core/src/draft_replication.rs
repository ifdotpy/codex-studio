//! Draft pull and push preserve the master document used for each attempt.
use crate::{
    backend::Backend,
    delivery::{StoreSender, state},
    protocol::Request,
    sync,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

async fn replicate(backend: &Backend, store: &StoreSender, workspace: &str) -> Result<(), String> {
    let identity = sync::identity(backend).await.map_err(|e| e.message)?;
    if identity["workspaceId"] != workspace {
        return Err("The server workspace changed. Saved drafts remain on this device.".into());
    }
    loop {
        let pending = state(store, "drafts.pending", json!({"workspace":workspace})).await?;
        let after = pending["checkpoint"]
            .as_u64()
            .ok_or("Invalid local draft checkpoint.")?;
        let result = sync::http(
            backend,
            &format!("/api/sync/pull?scope=drafts&after={after}&limit=100"),
        )
        .await
        .map_err(|e| e.message)?;
        if result["workspaceId"] != workspace {
            return Err("The server workspace changed during draft synchronization.".into());
        }
        let checkpoint = result["checkpoint"]["seq"]
            .as_u64()
            .filter(|v| *v >= after && *v <= 9_007_199_254_740_991)
            .ok_or("Invalid server draft checkpoint.")?;
        let documents = result["documents"]
            .as_array()
            .filter(|v| v.len() <= 100)
            .ok_or("Invalid server draft batch.")?;
        if !documents.is_empty() && checkpoint <= after {
            return Err("The draft checkpoint did not advance.".into());
        }
        if !documents.is_empty() {
            state(
                store,
                "drafts.receive",
                json!({"workspace":workspace,"documents":documents,"checkpoint":checkpoint}),
            )
            .await?;
        }
        if documents.len() < 100 {
            break;
        }
    }
    let pending = state(store, "drafts.pending", json!({"workspace":workspace})).await?;
    let rows = pending["rows"]
        .as_array()
        .ok_or("Invalid pending draft batch.")?;
    if rows.is_empty() {
        return Ok(());
    }
    let response=backend.dispatch(Request{id:"core-draft-push".into(),method:"api.request".into(),params:json!({
        "path":"/api/sync/drafts","body":{"rows":rows},"workspaceId":workspace,"timeoutMs":15000
    })}).await;
    let value = response.result.ok_or_else(|| {
        response
            .error
            .map(|e| e.message)
            .unwrap_or_else(|| "Missing draft response.".into())
    })?;
    let conflicts = value.as_array().ok_or("Invalid draft conflict response.")?;
    let mut conflicted = BTreeSet::new();
    for conflict in conflicts {
        let id = conflict["id"]
            .as_str()
            .ok_or("Missing draft conflict identity.")?;
        if !rows.iter().any(|r| r["newDocumentState"]["id"] == id) || !conflicted.insert(id) {
            return Err("The server returned an unrelated draft conflict.".into());
        }
    }
    let accepted = rows
        .iter()
        .map(|row| row["newDocumentState"].clone())
        .filter(|doc| !conflicted.contains(doc["id"].as_str().unwrap_or("")))
        .collect::<Vec<_>>();
    if !accepted.is_empty() {
        state(
            store,
            "drafts.acknowledge",
            json!({"workspace":workspace,"documents":accepted}),
        )
        .await?;
    }
    if !conflicts.is_empty() {
        state(
            store,
            "drafts.receive",
            json!({"workspace":workspace,"documents":conflicts}),
        )
        .await?;
    }
    Ok(())
}

pub async fn refresh(backend: &Backend, store: &StoreSender) {
    let cached = state(store, "identity.get", Value::Null)
        .await
        .unwrap_or(Value::Null);
    let Some(workspace) = cached["workspaceId"].as_str() else {
        return;
    };
    let result = replicate(backend, store, workspace).await;
    let _ = state(
        store,
        "drafts.sync_status",
        json!({"workspace":workspace,"error":result.err().unwrap_or_default()}),
    )
    .await;
}

/// Refresh only the opened workspace. This does not activate the message outbox.
pub async fn dispatch(
    backend: &Backend,
    store: &StoreSender,
    request: Request,
) -> crate::protocol::Response {
    let result = async {
        let identity = state(store, "identity.get", Value::Null).await?;
        let workspace = request.params["workspace"]
            .as_str()
            .filter(|value| crate::projections::valid_workspace(value))
            .ok_or("Invalid draft workspace.")?;
        if identity["workspaceId"] != workspace {
            return Err("The draft workspace is not the opened workspace.".to_owned());
        }
        refresh(backend, store).await;
        state(store, "drafts.view", json!({"workspace":workspace})).await
    }
    .await;
    match result {
        Ok(value) => crate::protocol::Response::success(request.id, value),
        Err(error) => crate::protocol::Response::failure(request.id, "draft_sync", error),
    }
}
