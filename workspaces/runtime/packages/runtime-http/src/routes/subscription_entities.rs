//! Entity view delivery. OpenAPI resource extensions own every collection and
//! identity filter; publication remains independent in runtime-services.

use crate::error::ApiError;
use crate::generated::{ResourceRef, ResourceRevisionEntry, SubscriptionCursor};
use crate::router::HttpState;
use serde_json::json;
use studio_runtime_schema::shared_values::{
    SYNC_ENTITY_STREAM_MAX_DOCUMENTS, SYNC_STREAM_EVENT_MAX_BYTES,
};
use studio_runtime_schema::sync_protocol::{SubscriptionEntityScope, subscription_entity_scopes};
use studio_runtime_store::{EntityPage, SyncDocumentsRepository};

/// OpenAPI owns collections; typed selectors keep payload probing out of delivery.
pub(super) fn selection(
    resource: &ResourceRef,
) -> Option<(&'static SubscriptionEntityScope, Option<&str>, Option<&str>)> {
    let (kind, id, agent) = match resource {
        ResourceRef::StateResource(_) => ("state", None, None),
        ResourceRef::CostsResource(_) => ("costs", None, None),
        ResourceRef::DesktopResource(_) => ("desktop", None, None),
        ResourceRef::TerminalsResource(_) => ("terminals", None, None),
        ResourceRef::AgentDetailResource(r) => ("agentDetail", Some(r.agent_id.as_str()), None),
        ResourceRef::SessionCostResource(r) => ("session-cost", Some(r.agent_id.as_str()), None),
        ResourceRef::TaskResource(r) => ("task", Some(r.task_id.as_str()), None),
        ResourceRef::TerminalResource(r) => ("terminal", Some(r.terminal_id.as_str()), None),
        ResourceRef::TasksResource(r) => ("tasks", None, r.agent_id.as_deref()),
        ResourceRef::MonitorsResource(r) => ("monitors", None, r.agent_id.as_deref()),
        ResourceRef::WorkResource(r) => ("work", None, Some(r.agent_id.as_str())),
        ResourceRef::AccountsResource(_)
        | ResourceRef::DraftsResource(_)
        | ResourceRef::LimitsResource(_)
        | ResourceRef::ModelsResource(_)
        | ResourceRef::MultiServerResource(_)
        | ResourceRef::QueueResource(_)
        | ResourceRef::ReceiptsResource(_)
        | ResourceRef::RoomResource(_)
        | ResourceRef::TranscriptResource(_)
        | ResourceRef::TranscriptsResource(_)
        | ResourceRef::VoiceResource(_)
        | ResourceRef::WorkspaceResource(_)
        | ResourceRef::AuthProgressResource(_)
        | ResourceRef::VaultReadinessResource(_)
        | ResourceRef::VaultJobProgressResource(_)
        | ResourceRef::RenameProgressResource(_)
        | ResourceRef::ProcessesResource(_)
        | ResourceRef::ComplaintResource(_)
        | ResourceRef::TerminalOutputResource(_) => return None,
    };
    Some((subscription_entity_scopes().get(kind)?, id, agent))
}

async fn read_page(
    state: &HttpState,
    resource: &ResourceRef,
    since: Option<i64>,
) -> Result<EntityPage, ApiError> {
    let (scope, id, agent) = selection(resource).ok_or_else(ApiError::internal)?;
    let id = id.map(str::to_owned);
    let agent = agent.map(str::to_owned);
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
                scope.include_team_members,
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

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "subscription_entities",
        routes: None,
        dependencies: &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore, StoreError};

    #[test]
    fn typed_entity_selectors_match_every_openapi_scope() {
        for (kind, scope) in subscription_entity_scopes() {
            let mut value = json!({"kind":kind});
            for field in scope.id_field.iter().chain(&scope.agent_field) {
                value[field] = json!("selector");
            }
            let resource: ResourceRef =
                serde_json::from_value(value.clone()).expect("typed entity reference");
            let (selected, id, agent) = selection(&resource).expect("every OpenAPI entity scope");
            assert_eq!(selected.collections, scope.collections);
            assert_eq!(
                id,
                scope
                    .id_field
                    .as_deref()
                    .and_then(|field| value[field].as_str()),
                "{kind} id owner"
            );
            assert_eq!(
                agent,
                scope
                    .agent_field
                    .as_deref()
                    .and_then(|field| value[field].as_str()),
                "{kind} agent owner"
            );
        }
    }

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
            AgentRepository::put(tx, &AgentRecord::from_value(json!({"compactions":0,"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,"id":"chat","rootId":"chat","epoch":1,"status":"completed","name":"List chat","threadId":"thread","lastCompletedTurn":"turn","lastCompletedTurnStatus":"completed","lastAnswer":"Detailed answer","tail":"Detailed output"}))?)
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
            AgentRepository::put(tx,&AgentRecord::from_value(json!({"compactions":0,"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,"id":"worker","rootId":"chat","parentId":"chat","epoch":1,"status":"idle"}))?)?;
            tx.execute("INSERT INTO runtime_tasks(id,record) VALUES(?1,?2)",rusqlite::params!["worker-command",json!({"id":"worker-command","agent":"worker","kind":"command","type":"commandExecution","name":"commandExecution","itemId":"command","turnId":"turn","status":"running","created":1}).to_string()])?;
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
        let list = initial_snapshot(state.clone(), list_ref.clone(), None)
            .await
            .expect("global snapshot");
        let before = list.cursor.clone();
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
        let unchanged = initial_snapshot(state.clone(), list_ref, before)
            .await
            .expect("unchanged list");
        assert!(
            unchanged
                .entity_changes
                .flatten()
                .expect("delta")
                .documents
                .is_empty()
        );
        let entry = initial_snapshot(state.clone(), detail_ref, None)
            .await
            .expect("changed detail");
        assert_eq!(
            serde_json::to_value(entry).expect("typed entry")["entityChanges"]["documents"][0]["value"]
                ["lastAnswer"],
            "Changed answer"
        );
    }
}
