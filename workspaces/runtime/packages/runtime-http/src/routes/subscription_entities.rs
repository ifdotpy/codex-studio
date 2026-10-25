//! Entity view delivery. OpenAPI resource extensions own every collection and
//! identity filter; publication remains independent in runtime-services.

use crate::error::ApiError;
use crate::generated::{
    ResourceChangeEvent, ResourceRef, ResourceRevisionEntry, SubscriptionCursor,
};
use crate::router::HttpState;
use serde_json::{Value, json};
use studio_runtime_schema::shared_values::{
    SYNC_ENTITY_STREAM_MAX_DOCUMENTS, SYNC_STREAM_EVENT_MAX_BYTES,
};
use studio_runtime_schema::sync_protocol::{SubscriptionEntityScope, subscription_entity_scopes};
use studio_runtime_store::{EntityPage, SyncDocumentsRepository};

fn selection(
    resource: &ResourceRef,
) -> Result<(SubscriptionEntityScope, Option<String>, Option<String>), ApiError> {
    let reference = serde_json::to_value(resource).map_err(|_| ApiError::internal())?;
    let kind = reference["kind"].as_str().ok_or_else(ApiError::internal)?;
    let scope = subscription_entity_scopes()
        .get(kind)
        .cloned()
        .ok_or_else(ApiError::internal)?;
    let field = |name: &Option<String>| {
        name.as_deref()
            .and_then(|name| reference.get(name))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let id = field(&scope.id_field);
    let agent = field(&scope.agent_field);
    Ok((scope, id, agent))
}

async fn read_page(
    state: &HttpState,
    resource: &ResourceRef,
    since: Option<i64>,
) -> Result<EntityPage, ApiError> {
    let (scope, id, agent) = selection(resource)?;
    let collections =
        serde_json::to_string(&scope.collections).map_err(|_| ApiError::internal())?;
    state
        .store
        .as_ref()
        .ok_or_else(ApiError::internal)?
        .read_async("runtime-http/subscription-entities", move |tx| {
            SyncDocumentsRepository::resume_entity_scope(
                tx,
                since,
                Some(&collections),
                id.as_deref(),
                agent.as_deref(),
                SYNC_ENTITY_STREAM_MAX_DOCUMENTS,
            )
            .map_err(|_| ApiError::internal())
        })
        .await
}

pub(super) async fn initial_snapshot(
    state: HttpState,
    resource: ResourceRef,
    since: Option<SubscriptionCursor>,
) -> Result<ResourceRevisionEntry, ApiError> {
    let after = since
        .filter(|cursor| cursor.workspace_id == state.workspace_id)
        .map(|cursor| cursor.seq);
    let page = read_page(&state, &resource, after).await?;
    let revision = page.max_sequence;
    // Existing global pull pagination owns oversized initial lists. Never
    // advertise a complete per-view cursor for a truncated interval.
    if page.checkpoint < page.max_sequence {
        return serde_json::from_value(json!({"revision":revision,"resource":resource}))
            .map_err(|_| ApiError::internal());
    }
    let documents = page
        .rows
        .iter()
        .map(super::sync_documents::entity_document)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(ApiError::internal)?;
    let entry: ResourceRevisionEntry = serde_json::from_value(json!({
        "revision":revision,
        "resource":resource,
        "cursor":{"workspaceId":state.workspace_id,"seq":revision},
        "replyKind":if page.reset {"snapshot"} else {"delta"},
        "entityChanges":{"after":if page.reset {0} else {after.unwrap_or(0)},"through":revision,"documents":documents}
    })).map_err(|_| ApiError::internal())?;
    if serde_json::to_vec(&entry)
        .map_err(|_| ApiError::internal())?
        .len()
        > SYNC_STREAM_EVENT_MAX_BYTES
    {
        return serde_json::from_value(json!({"revision":revision,"resource":entry.resource}))
            .map_err(|_| ApiError::internal());
    }
    Ok(entry)
}

/// Shared scoped fingerprints suppress updates when only an offscreen view changed.
pub(super) async fn content_fingerprint(
    state: HttpState,
    resource: ResourceRef,
) -> Option<Vec<u8>> {
    let page = read_page(&state, &resource, None).await.ok()?;
    serde_json::to_vec(
        &page
            .rows
            .iter()
            .map(|row| (&row.collection, &row.id, row.sequence))
            .collect::<Vec<_>>(),
    )
    .ok()
}

/// Live panel frames carry a bounded replacement snapshot. This also conveys an
/// empty result, allowing clients to remove the last row without an HTTP read.
pub(super) async fn attach_scoped_entity_changes(
    state: Option<&HttpState>,
    event: &mut ResourceChangeEvent,
) {
    let Some(state) = state else { return };
    for (resource, entry) in event.resources.iter().zip(&mut event.resource_versions) {
        let reference = match serde_json::to_value(resource) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if reference["kind"] == "state" {
            continue;
        }
        if !reference["kind"]
            .as_str()
            .is_some_and(|kind| subscription_entity_scopes().contains_key(kind))
        {
            continue;
        }
        match initial_snapshot(state.clone(), resource.clone(), None).await {
            Ok(snapshot) => *entry = snapshot,
            Err(error) => eprintln!("scoped entity snapshot unavailable: {error:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore, StoreError};

    #[tokio::test]
    async fn view_scoped_entities_publish_list_and_detail_through_real_handlers() {
        let directory = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("store"),
        );
        studio_runtime_services::entity_publication::install(&store)
            .await
            .expect("publisher");
        store.transaction_with(|tx| {
            AgentRepository::put(tx, &AgentRecord::from_value(json!({"id":"chat","rootId":"chat","epoch":1,"status":"completed","name":"List chat","threadId":"thread","lastCompletedTurn":"turn","lastCompletedTurnStatus":"completed","lastAnswer":"Detailed answer","tail":"Detailed output"}))?)
        }).await.expect("source commit publishes both views");
        let workspace = store
            .test_read_transaction(|tx| SyncDocumentsRepository::workspace_id(tx))
            .expect("workspace");
        let state = HttpState::new(
            "token".into(),
            workspace,
            directory.path().to_string_lossy().into_owned(),
            4620,
            None,
            Some(store.clone()),
            None,
        );
        let list_ref: ResourceRef =
            serde_json::from_value(json!({"kind":"state"})).expect("state ref");
        let detail_ref: ResourceRef =
            serde_json::from_value(json!({"kind":"agentDetail","agentId":"chat"}))
                .expect("detail ref");
        store.transaction_with(|tx| {
            AgentRepository::put(tx,&AgentRecord::from_value(json!({"id":"worker","rootId":"chat","parentId":"chat","epoch":1,"status":"idle"}))?)?;
            tx.execute("INSERT INTO runtime_tasks(id,record) VALUES(?1,?2)",rusqlite::params!["worker-command",json!({"id":"worker-command","agent":"worker","kind":"command","status":"running","created":1}).to_string()])?;
            Ok::<_, StoreError>(())
        }).await.expect("worker command source");
        let tasks_ref: ResourceRef =
            serde_json::from_value(json!({"kind":"tasks","agentId":"chat"}))
                .expect("team task ref");
        let tasks = initial_snapshot(state.clone(), tasks_ref, None)
            .await
            .expect("team tasks");
        assert!(
            serde_json::to_value(tasks).expect("task frame")["entityChanges"]["documents"]
                .as_array()
                .expect("task records")
                .iter()
                .any(|row| row["id"] == "worker-command")
        );
        let before = content_fingerprint(state.clone(), list_ref.clone())
            .await
            .expect("list fingerprint");
        let list = initial_snapshot(state.clone(), list_ref.clone(), None)
            .await
            .expect("global snapshot");
        let list = serde_json::to_value(list).expect("typed entry");
        let documents = list["entityChanges"]["documents"]
            .as_array()
            .expect("complete list");
        assert!(
            documents
                .iter()
                .any(|row| row["collection"] == "agentSummary")
        );
        assert!(
            !documents
                .iter()
                .any(|row| row["collection"] == "agent" || row["collection"] == "agentDetail")
        );
        let summary = documents
            .iter()
            .find(|row| row["collection"] == "agentSummary")
            .expect("summary");
        assert_eq!(summary["value"]["name"], "List chat");
        assert!(summary["value"].get("lastAnswer").is_none());
        let detail = initial_snapshot(state.clone(), detail_ref.clone(), None)
            .await
            .expect("detail snapshot");
        let detail = serde_json::to_value(detail).expect("typed detail");
        assert_eq!(
            detail["entityChanges"]["documents"][0]["value"]["lastAnswer"],
            "Detailed answer"
        );
        store
            .transaction_with(|tx| {
                let mut agent =
                    AgentRepository::get(tx, &studio_runtime_store::AgentId::from("chat"))?
                        .expect("agent")
                        .value();
                agent["lastAnswer"] = json!("Changed answer");
                AgentRepository::put(tx, &AgentRecord::from_value(agent.clone())?)?;
                studio_runtime_services::agents_create::publish_agent(tx, &agent)?;
                Ok::<_, StoreError>(())
            })
            .await
            .expect("detail output commit");
        assert_eq!(
            content_fingerprint(state.clone(), list_ref)
                .await
                .expect("list fingerprint"),
            before
        );
        let mut event: ResourceChangeEvent = serde_json::from_value(json!({"protocol":studio_runtime_schema::SYNC_PROTOCOL_VERSION,"workspaceId":state.workspace_id,"epoch":"epoch","revision":1,"reason":"change","resources":[detail_ref],"resourceVersions":[{"revision":1}]})).expect("event");
        attach_scoped_entity_changes(Some(&state), &mut event).await;
        assert_eq!(
            serde_json::to_value(event).expect("live frame")["resourceVersions"][0]["entityChanges"]
                ["documents"][0]["value"]["lastAnswer"],
            "Changed answer"
        );
    }
}
