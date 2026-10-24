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
            Err(error) => tracing::warn!(?error, "scoped entity snapshot unavailable"),
        }
    }
}
