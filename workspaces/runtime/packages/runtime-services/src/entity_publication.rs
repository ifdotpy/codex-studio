//! Default client publication of the agent and request rows that a writer
//! transaction changes (the store side is `studio_runtime_store::entity_publication`).
//! A write path needs no publish call of its own: the store journals the rows it
//! changed and calls [`DefaultPublisher`] before the commit. A path that publishes
//! after its last write is not published twice.

use std::sync::Arc;

use studio_runtime_store::{
    EntityPublisher, PendingEntities, RuntimeStore, StoreResult, Transaction,
};

/// Publish pending rows through the projection owner,
/// [`crate::sync_policy::publish_entity_changes`].
struct DefaultPublisher;

impl EntityPublisher for DefaultPublisher {
    fn publish(&self, transaction: &Transaction<'_>, pending: &PendingEntities) -> StoreResult<()> {
        let changes = crate::sync_policy::EntityChanges {
            agents: pending.agents.clone(),
            requests: pending.requests.clone(),
            work: pending.work.clone(),
            source_work_changes: Some(pending.work_changes.clone()),
            work_refusal_owners: pending.work_refusal_owners.clone(),
            ..Default::default()
        };
        let server_id = RuntimeStore::transaction_local_server_id(transaction)?;
        for id in &pending.terminals {
            crate::terminal_driver::publish_terminal(transaction, id)?;
        }
        for id in &pending.queues {
            crate::queue_write::publish_queue(transaction, id)?;
        }
        crate::sync_policy::publish_entity_changes(
            transaction,
            &changes,
            server_id.as_deref(),
            &|_, _, _, _| true,
        )?;
        crate::sync_policy::publish_summary_changes(transaction, pending)?;
        if !pending.studio_errors.is_empty() {
            crate::studio_errors::publish_changes(transaction, &pending.studio_errors)?;
        }
        Ok(())
    }
}

/// Make every later writer transaction of `store` publish the client entities of the
/// agent and request rows it changed. The server calls it once, right after it opens
/// the store.
pub async fn install(store: &RuntimeStore) -> StoreResult<()> {
    store.install_entity_publisher(Arc::new(DefaultPublisher));
    store
        .transaction(crate::sync_policy::seed_work_details)
        .await?;
    // Upgrades may have source errors but no studioError entities yet.
    crate::studio_errors::refresh(store).await
}

#[cfg(test)]
pub(crate) mod test_support {
    use rusqlite::OptionalExtension;
    use serde_json::Value;
    use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore, StoreError};

    /// A store set up as the server sets it up: with the default publisher.
    pub(crate) async fn store() -> (tempfile::TempDir, RuntimeStore) {
        let dir = tempfile::tempdir().expect("isolated state");
        let store = RuntimeStore::open(Some(dir.path()))
            .await
            .expect("runtime store opens");
        super::install(&store)
            .await
            .expect("initial error projection");
        (dir, store)
    }

    /// Store `record` and publish its client view, as a chat looks before the write under test.
    pub(crate) async fn seed_chat(store: &RuntimeStore, record: Value) {
        store
            .transaction_with(move |tx| {
                let agent = AgentRecord::from_value(record)?;
                AgentRepository::put(tx, &agent)?;
                crate::agents_create::publish_agent(tx, &agent.value())?;
                Ok::<_, StoreError>(())
            })
            .await
            .expect("the chat is seeded and published");
    }

    /// The client entity of `collection/id`: whether it is retired, and its value.
    pub(crate) fn client_entity(
        store: &RuntimeStore,
        collection: &str,
        id: &str,
    ) -> Option<(bool, Value)> {
        let (collection, id) = (collection.to_owned(), id.to_owned());
        store
            .test_read_transaction(move |tx| {
                Ok(tx
                    .query_row(
                        "SELECT deleted,payload FROM sync_entities WHERE collection=?1 AND id=?2",
                        [collection, id],
                        |row| Ok((row.get::<_, bool>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()?)
            })
            .expect("the client entity reads")
            .map(|(deleted, payload)| {
                let payload: Value = serde_json::from_str(&payload).expect("entity JSON");
                (deleted, payload["value"].clone())
            })
    }

    /// The live client view of chat `id`.
    pub(crate) fn chat_view(store: &RuntimeStore, id: &str) -> Value {
        match client_entity(store, "agent", id) {
            Some((false, value)) => value,
            other => panic!("chat {id} has no live client view: {other:?}"),
        }
    }
}

#[cfg(test)]
#[path = "entity_publication_tests.rs"]
mod tests;
