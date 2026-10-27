//! Scoped intervals are read once in one SQLite snapshot. Large intervals spool
//! bounded wire frames to an unlinked temporary file, owned by the live stream.
use std::collections::BTreeMap;
use std::io::{Seek, Write};

use axum::response::sse::Event;
use serde_json::{Value, json};
use studio_runtime_schema::shared_values::{
    SYNC_ENTITY_STREAM_MAX_DOCUMENTS, SYNC_STREAM_EVENT_MAX_BYTES,
};
use studio_runtime_store::SyncDocumentsRepository;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::error::ApiError;
use crate::generated::{
    EntityChangeBatch, ResourceChangeEvent, ResourceRef, ResourceRevisionEntry, SubscriptionCursor,
};
use crate::router::HttpState;

pub(super) struct TransferReader(BufReader<tokio::fs::File>);
impl TransferReader {
    pub(super) async fn next(&mut self) -> Result<Option<Event>, ApiError> {
        let mut line = String::new();
        if self
            .0
            .read_line(&mut line)
            .await
            .map_err(|_| ApiError::internal())?
            == 0
        {
            return Ok(None);
        }
        let data = line.strip_suffix('\n').ok_or_else(ApiError::internal)?;
        Ok(Some(Event::default().event("entity-transfer").data(data)))
    }
}

pub(super) fn owns(resource: &ResourceRef) -> bool {
    super::subscription_entities::selection(resource).is_some()
}

/// The same per-scope cursor is used for the next delta; no changed notification
/// rereads or retransmits the complete scoped history after the baseline.
pub(super) async fn attach(
    state: &HttpState,
    event: &mut ResourceChangeEvent,
    since: &BTreeMap<String, SubscriptionCursor>,
    sequences: &mut BTreeMap<String, SubscriptionCursor>,
) -> Result<Vec<TransferReader>, ApiError> {
    let mut readers = Vec::new();
    let mut retained = Vec::new();
    let mut entries = Vec::new();
    for (resource, entry) in std::mem::take(&mut event.resources)
        .into_iter()
        .zip(std::mem::take(&mut event.resource_versions))
    {
        if !owns(&resource) {
            retained.push(resource);
            entries.push(entry);
            continue;
        }
        let key = serde_json::to_string(&resource).map_err(|_| ApiError::internal())?;
        let cursor = since
            .get(&key)
            .or_else(|| sequences.get(&key))
            .filter(|cursor| cursor.workspace_id == state.workspace_id)
            .cloned();
        let (entry, reader, cursor) = prepare(state, event, resource.clone(), cursor).await?;
        if matches!(
            event.reason,
            crate::generated::ResourceChangeEventReason::Change
        ) && entry.as_ref().is_some_and(|entry| {
            entry.reply_kind == Some(crate::generated::SubscriptionReplyKind::Delta)
                && entry
                    .entity_changes
                    .as_ref()
                    .and_then(Option::as_ref)
                    .is_some_and(|batch| batch.documents.is_empty())
        }) {
            // Keep the delivered base: the client never received this empty interval.
            continue;
        }
        sequences.insert(key, cursor);
        if let Some(reader) = reader {
            readers.push(reader);
        }
        if let Some(entry) = entry {
            retained.push(resource);
            entries.push(entry);
        }
    }
    event.resources = retained;
    event.resource_versions = entries;
    Ok(readers)
}

async fn prepare(
    state: &HttpState,
    event: &ResourceChangeEvent,
    resource: ResourceRef,
    since: Option<SubscriptionCursor>,
) -> Result<
    (
        Option<ResourceRevisionEntry>,
        Option<TransferReader>,
        SubscriptionCursor,
    ),
    ApiError,
> {
    let (scope, id, agent) =
        super::subscription_entities::selection(&resource).ok_or_else(ApiError::internal)?;
    let collections =
        serde_json::to_string(&scope.collections).map_err(|_| ApiError::internal())?;
    let id = id.map(str::to_owned);
    let agent = agent.map(str::to_owned);
    let workspace = state.workspace_id.clone();
    let epoch = event.epoch.clone();
    let revision = event.revision;
    let reason = event.reason.clone();
    let validators = state.validators.clone();
    state.store.as_ref().ok_or_else(ApiError::internal)?.read_async("runtime-http/entity-transfer", move |tx| {
        let after = since.as_ref().map(|cursor| cursor.seq);
        // Only actual ownership/team movement or expired cursors replace a scope.
        // Ordinary writes and tombstones retain indexed sequence deltas.
        let visibility = agent.as_deref().map(|agent| SyncDocumentsRepository::entity_scope_visibility_revision(tx, &collections, agent, scope.include_team_members)).transpose().map_err(|_| ApiError::internal())?.unwrap_or(0);
        let after = after.filter(|after| *after >= visibility);
        let mut page = SyncDocumentsRepository::resume_entity_scope(tx, after, Some(&collections), id.as_deref(), agent.as_deref(), scope.include_team_members, SYNC_ENTITY_STREAM_MAX_DOCUMENTS).map_err(|_| ApiError::internal())?;
        let through = page.max_sequence;
        let snapshot = page.reset;
        let mut after = if snapshot { 0 } else { since.as_ref().map_or(0, |cursor| cursor.seq) };
        let cursor = SubscriptionCursor { terminal_generation: None, workspace_id: workspace.clone(), seq: through };
        let transfer_id = uuid::Uuid::new_v4().to_string();
        let metadata = json!({"protocol":studio_runtime_schema::SYNC_PROTOCOL_VERSION,"workspaceId":workspace,"epoch":epoch,"revision":revision,"reason":reason,"resource":resource,"cursor":cursor,"replyKind":if snapshot {"snapshot"}else{"delta"},"transferId":transfer_id});
        let mut spool = None;
        let mut page_index = 0;
        loop {
            let checkpoint = page.checkpoint;
            let final_sql_page = checkpoint == through;
            let documents = page.rows.iter().filter(|row| !snapshot || !row.deleted).map(super::sync_documents::entity_document).collect::<Option<Vec<_>>>().ok_or_else(ApiError::internal)?;
            let batches = byte_pages(after, checkpoint, documents)?;
            for (index, batch) in batches.iter().enumerate() {
                let complete = final_sql_page && index + 1 == batches.len();
                if cfg!(debug_assertions) && !serde_json::to_value(batch).is_ok_and(|value| validators.validate_component("EntityChangeBatch", &value)) { return Err(ApiError::internal()); }
                if page_index == 0 && complete {
                    let entry = ResourceRevisionEntry { resource: Some(Some(resource.clone())), revision: through, cursor: Some(cursor.clone()), reply_kind: Some(if snapshot { crate::generated::SubscriptionReplyKind::Snapshot } else { crate::generated::SubscriptionReplyKind::Delta }), entity_changes: Some(Some(batch.clone())), discovery_snapshot: None, voice_records: None, view_snapshot: None, draft_changes: None, entity_sequence_reset: None, entity_sequences: None, terminal_base_offset: None, transcript_changes: None, transcript_snapshot: None };
                    let ordinary = ResourceChangeEvent { protocol: i64::from(studio_runtime_schema::SYNC_PROTOCOL_VERSION), workspace_id: workspace.clone(), epoch: epoch.clone(), revision, reason: reason.clone(), resources: vec![resource.clone()], resource_versions: vec![entry.clone()] };
                    if "event: resources\nid: \ndata: \n\n".len() + revision.to_string().len() + serde_json::to_vec(&ordinary).map_err(|_| ApiError::internal())?.len() <= SYNC_STREAM_EVENT_MAX_BYTES { return Ok((Some(entry), None, cursor)); }
                }
                let file = match &mut spool { Some(file) => file, None => spool.insert(tempfile::tempfile().map_err(|_| ApiError::internal())?) };
                spool_page(file, &metadata, page_index, complete, batch, &validators)?;
                page_index += 1;
            }
            if final_sql_page { break; }
            after = checkpoint;
            page = SyncDocumentsRepository::continue_entity_scope(tx, &page, Some(&collections), id.as_deref(), agent.as_deref(), scope.include_team_members, SYNC_ENTITY_STREAM_MAX_DOCUMENTS).map_err(|_| ApiError::internal())?;
        }
        let mut file = spool.ok_or_else(ApiError::internal)?;
        file.rewind().map_err(|_| ApiError::internal())?;
        Ok((None, Some(TransferReader(BufReader::new(tokio::fs::File::from_std(file)))), cursor))
    }).await
}

/// Split at record boundaries first. A single existing oversized canonical
/// record remains one page and uses serialization fragments in spool_page.
fn byte_pages(
    after: i64,
    through: i64,
    documents: Vec<crate::generated::SyncEntity>,
) -> Result<Vec<EntityChangeBatch>, ApiError> {
    let mut batches = Vec::new();
    let mut batch = EntityChangeBatch {
        after,
        through,
        documents: Vec::new(),
    };
    let mut bytes = 0;
    for document in documents {
        let size = serde_json::to_vec(&document)
            .map_err(|_| ApiError::internal())?
            .len();
        if !batch.documents.is_empty() && bytes + size > SYNC_STREAM_EVENT_MAX_BYTES / 2 {
            batch.through = batch.documents.last().ok_or_else(ApiError::internal)?.seq;
            let next = EntityChangeBatch {
                after: batch.through,
                through,
                documents: Vec::new(),
            };
            batches.push(std::mem::replace(&mut batch, next));
            bytes = 0;
        }
        bytes += size;
        batch.documents.push(document);
    }
    batches.push(batch);
    Ok(batches)
}

fn spool_page(
    file: &mut std::fs::File,
    metadata: &Value,
    page_index: usize,
    complete: bool,
    page: &EntityChangeBatch,
    validators: &crate::validation::Registry,
) -> Result<(), ApiError> {
    let data = serde_json::to_string(page).map_err(|_| ApiError::internal())?;
    let mut direct = metadata.clone();
    let fields = direct.as_object_mut().ok_or_else(ApiError::internal)?;
    fields.insert("pageIndex".into(), json!(page_index));
    fields.insert("partIndex".into(), json!(0));
    fields.insert("partCount".into(), json!(1));
    fields.insert("complete".into(), json!(complete));
    fields.insert("data".into(), json!(&data));
    let encoded = serde_json::to_vec(&direct).map_err(|_| ApiError::internal())?;
    if "event: entity-transfer\ndata: \n\n".len() + encoded.len() <= SYNC_STREAM_EVENT_MAX_BYTES {
        if cfg!(debug_assertions)
            && !validators.validate_component("ResourceEntityTransferEvent", &direct)
        {
            return Err(ApiError::internal());
        }
        file.write_all(&encoded).map_err(|_| ApiError::internal())?;
        file.write_all(b"\n").map_err(|_| ApiError::internal())?;
        return Ok(());
    }
    // JSON string escaping can expand each source byte sixfold. Leave room for
    // the resource selector and authenticated stream metadata, then verify exactly.
    let fragment_bytes = SYNC_STREAM_EVENT_MAX_BYTES / 8;
    let mut parts = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let mut end = (offset + fragment_bytes).min(data.len());
        while !data.is_char_boundary(end) {
            end -= 1;
        }
        parts.push(&data[offset..end]);
        offset = end;
    }
    for (index, part) in parts.iter().enumerate() {
        let mut frame = metadata.clone();
        let fields = frame.as_object_mut().ok_or_else(ApiError::internal)?;
        fields.insert("pageIndex".into(), json!(page_index));
        fields.insert("partIndex".into(), json!(index));
        fields.insert("partCount".into(), json!(parts.len()));
        fields.insert(
            "complete".into(),
            json!(complete && index + 1 == parts.len()),
        );
        fields.insert("data".into(), json!(part));
        let encoded = serde_json::to_vec(&frame).map_err(|_| ApiError::internal())?;
        if "event: entity-transfer\ndata: \n\n".len() + encoded.len() > SYNC_STREAM_EVENT_MAX_BYTES
            || (cfg!(debug_assertions)
                && !validators.validate_component("ResourceEntityTransferEvent", &frame))
        {
            return Err(ApiError::internal());
        }
        file.write_all(&encoded).map_err(|_| ApiError::internal())?;
        file.write_all(b"\n").map_err(|_| ApiError::internal())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::{IntoResponse, Sse};
    use std::sync::Arc;
    use studio_runtime_store::RuntimeStore;

    async fn fixture() -> (tempfile::TempDir, HttpState) {
        let directory = tempfile::tempdir().expect("isolated source");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("store"),
        );
        let workspace = store
            .test_read_transaction(|tx| SyncDocumentsRepository::workspace_id(tx))
            .expect("identity");
        let mut state = HttpState::new(
            "token".into(),
            workspace,
            directory.path().display().to_string(),
            4620,
            None,
            Some(store),
            None,
        );
        state.validators = crate::validation::Registry::compile(&[]).expect("canonical validators");
        (directory, state)
    }
    fn event(state: &HttpState) -> ResourceChangeEvent {
        serde_json::from_value(json!({"protocol":studio_runtime_schema::SYNC_PROTOCOL_VERSION,"workspaceId":state.workspace_id,"epoch":"fixture","revision":1,"reason":"initial","resources":[],"resourceVersions":[]})).expect("event")
    }
    async fn wire(event: Event) -> Vec<u8> {
        let response = Sse::new(futures_util::stream::iter([Ok::<
            _,
            std::convert::Infallible,
        >(event)]))
        .into_response();
        axum::body::to_bytes(response.into_body(), SYNC_STREAM_EVENT_MAX_BYTES + 1)
            .await
            .expect("bounded frame")
            .to_vec()
    }
    #[tokio::test]
    async fn reconnect_seq_cursor_delivers_exact_changes_and_empty_live_intervals_are_suppressed() {
        let (_directory, state) = fixture().await;
        let store = state.store.as_ref().expect("store");
        let resource = ResourceRef::StateResource(crate::generated::StateResource {
            kind: crate::generated::StateResourceKind::State,
        });
        let (_, _, baseline) = prepare(&state, &event(&state), resource.clone(), None)
            .await
            .expect("baseline");
        store
            .transaction(|tx| {
                for index in 0..3 {
                    SyncDocumentsRepository::put_entity(
                        tx,
                        "workspace",
                        &format!("change-{index}"),
                        json!({"stateDir":"changed"}),
                        false,
                    )?;
                }
                SyncDocumentsRepository::put_entity(tx, "workspace", "removed", json!({}), true)?;
                SyncDocumentsRepository::put_entity(
                    tx,
                    "agentDetail",
                    "unrelated",
                    json!({}),
                    false,
                )?;
                Ok(())
            })
            .await
            .expect("four scoped changes while disconnected");
        let (entry, _, cursor) = prepare(
            &state,
            &event(&state),
            resource.clone(),
            Some(baseline.clone()),
        )
        .await
        .expect("reconnect");
        let candidate = entry.expect("inline delta");
        let batch = candidate
            .entity_changes
            .as_ref()
            .and_then(Option::as_ref)
            .expect("changes");
        assert_eq!(batch.after, baseline.seq);
        assert_eq!(
            batch.documents.len(),
            4,
            "exactly the missed changes, including deletion"
        );
        assert_eq!(batch.documents[3].id, "removed");
        assert_eq!(
            batch.documents[3].op,
            crate::generated::SyncEntityOp::Delete
        );
        store
            .transaction(|tx| {
                SyncDocumentsRepository::put_entity(
                    tx,
                    "agentDetail",
                    "unrelated",
                    json!({"ignored":true}),
                    false,
                )?;
                Ok(())
            })
            .await
            .expect("unrelated notification");
        let mut live = event(&state);
        live.reason = crate::generated::ResourceChangeEventReason::Change;
        live.resources.push(resource.clone());
        live.resource_versions.push(candidate.clone());
        let mut cursors = BTreeMap::from([(
            serde_json::to_string(&resource).expect("key"),
            cursor.clone(),
        )]);
        let readers = attach(&state, &mut live, &BTreeMap::new(), &mut cursors)
            .await
            .expect("empty live interval");
        assert!(readers.is_empty());
        assert!(
            live.resources.is_empty(),
            "an empty live delta must not notify a consumer"
        );
        assert_eq!(
            cursors.values().next().expect("retained cursor").seq,
            cursor.seq,
            "a suppressed interval cannot silently advance the delivered base"
        );
        let (reconnected, _, _) = prepare(
            &state,
            &event(&state),
            resource.clone(),
            Some(cursor.clone()),
        )
        .await
        .expect("current reconnect");
        assert!(
            reconnected
                .expect("reconnect acknowledgement")
                .entity_changes
                .flatten()
                .expect("empty delta")
                .documents
                .is_empty()
        );
        store
            .transaction(|tx| {
                SyncDocumentsRepository::put_entity(
                    tx,
                    "workspace",
                    "later",
                    json!({"stateDir":"later"}),
                    false,
                )?;
                Ok(())
            })
            .await
            .expect("later visible change");
        live.resources.push(resource.clone());
        live.resource_versions.push(candidate);
        attach(&state, &mut live, &BTreeMap::new(), &mut cursors)
            .await
            .expect("later delta");
        let later = live.resource_versions[0]
            .entity_changes
            .as_ref()
            .and_then(Option::as_ref)
            .expect("later batch");
        assert_eq!(
            later.after, cursor.seq,
            "client's durable base survives the suppressed notification"
        );
        assert_eq!(later.documents.len(), 1);
    }

    #[tokio::test]
    async fn spool_frame_is_emitted_without_reencoding() {
        let mut file = tempfile::tempfile().expect("spool");
        let saved = r#"{ "z": 1, "a": 2 }"#;
        writeln!(file, "{saved}").expect("write frame");
        file.rewind().expect("rewind");
        let mut reader = TransferReader(BufReader::new(tokio::fs::File::from_std(file)));
        let frame = reader.next().await.expect("read frame").expect("frame");
        assert_eq!(
            wire(frame).await,
            format!("event: entity-transfer\ndata: {saved}\n\n").as_bytes()
        );
    }
    #[tokio::test]
    async fn scoped_membership_swaps_and_reassignment_reset_but_unrelated_moves_and_appends_stay_delta()
     {
        let (_directory, state) = fixture().await;
        let store = state.store.as_ref().expect("store");
        store.transaction(|tx| {
            for (id, agent) in [("a-task","a"),("b-task","b"),("latest","lead")] {
                SyncDocumentsRepository::put_entity(tx,"task",id,json!({"id":id,"agent":agent,"kind":"command","status":"running","created":1}),false)?;
            }
            for (id, root) in [("a","lead"),("b","other")] { SyncDocumentsRepository::put_entity(tx,"agentSummary",id,json!({"id":id,"rootId":root,"status":"idle","source":"managed"}),false)?; }
            Ok(())
        }).await.expect("scope source");
        let resource: ResourceRef =
            serde_json::from_value(json!({"kind":"tasks","agentId":"lead"})).expect("resource");
        let (_, _, before) = prepare(&state, &event(&state), resource.clone(), None)
            .await
            .expect("baseline");
        store
            .transaction(|tx| {
                tx.execute("UPDATE sync_entities SET payload=json_set(payload,'$.value.rootId',CASE id WHEN 'a' THEN 'other' ELSE 'lead' END),seq=seq+100 WHERE collection='agentSummary'", [])?;
                Ok(())
            })
            .await
            .expect("equal-size team swap");
        let (entry, _, swapped) = prepare(&state, &event(&state), resource.clone(), Some(before))
            .await
            .expect("swapped scope");
        let entry = serde_json::to_value(entry.expect("small snapshot")).expect("entry");
        assert_eq!(
            entry["replyKind"], "snapshot",
            "membership movement must not advance an empty delta past old selected rows"
        );
        assert_eq!(
            entry["entityChanges"]["documents"]
                .as_array()
                .expect("rows")
                .iter()
                .map(|row| row["id"].as_str().expect("id"))
                .collect::<Vec<_>>(),
            ["b-task", "latest"]
        );
        store.transaction(|tx| { SyncDocumentsRepository::put_entity(tx,"task","b-task",json!({"id":"b-task","agent":"a","kind":"command","status":"running","created":1}),false)?; Ok(()) }).await.expect("existing task moved away");
        let (entry, _, moved) = prepare(&state, &event(&state), resource.clone(), Some(swapped))
            .await
            .expect("moved scope");
        let entry = serde_json::to_value(entry.expect("replacement")).expect("entry");
        assert_eq!(entry["replyKind"], "snapshot");
        assert_eq!(
            entry["entityChanges"]["documents"]
                .as_array()
                .expect("rows")
                .len(),
            1
        );
        store.transaction(|tx| { SyncDocumentsRepository::put_entity(tx,"agentSummary","unrelated",json!({"id":"unrelated","rootId":"other","status":"idle","source":"managed"}),false)?; SyncDocumentsRepository::put_entity(tx,"task","latest",json!({"id":"latest","agent":"lead","kind":"command","status":"completed","created":1}),false)?; Ok(()) }).await.expect("other team and local content");
        let (entry, _, current) = prepare(&state, &event(&state), resource.clone(), Some(moved))
            .await
            .expect("content delta");
        let entry = serde_json::to_value(entry.expect("delta")).expect("entry");
        assert_eq!(entry["replyKind"], "delta");
        assert_eq!(
            entry["entityChanges"]["documents"]
                .as_array()
                .expect("rows")
                .len(),
            1
        );
        store.transaction(|tx| { SyncDocumentsRepository::put_entity(tx,"task","new",json!({"id":"new","agent":"lead","kind":"command","status":"running","created":1}),false)?; Ok(()) }).await.expect("ordinary append");
        let (entry, _, _) = prepare(&state, &event(&state), resource, Some(current))
            .await
            .expect("append delta");
        let entry = serde_json::to_value(entry.expect("delta")).expect("entry");
        assert_eq!(entry["replyKind"], "delta");
        assert_eq!(
            entry["entityChanges"]["documents"]
                .as_array()
                .expect("rows")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn restored_ownership_replaces_the_old_scope_and_delivers_the_new_scope() {
        let (_directory, state) = fixture().await;
        let store = state.store.as_ref().expect("fixture store");
        store.transaction(|tx| { SyncDocumentsRepository::put_entity(tx,"task","restored",json!({"id":"restored","agent":"alpha","kind":"command","status":"running","created":1}),false)?; Ok(()) }).await.expect("publish initial task owner");
        let alpha: ResourceRef = serde_json::from_value(json!({"kind":"tasks","agentId":"alpha"}))
            .expect("alpha task resource");
        let beta: ResourceRef = serde_json::from_value(json!({"kind":"tasks","agentId":"beta"}))
            .expect("beta task resource");
        let (_, _, baseline) = prepare(&state, &event(&state), alpha.clone(), None)
            .await
            .expect("initial owner baseline");
        store.transaction(|tx| {
            SyncDocumentsRepository::put_entity(tx,"task","restored",json!({}),true)?;
            SyncDocumentsRepository::put_entity(tx,"task","restored",json!({"id":"restored","agent":"beta","kind":"command","status":"running","created":1}),false)?;
            Ok(())
        }).await.expect("delete and restore under new owner");
        let (old, _, _) = prepare(&state, &event(&state), alpha, Some(baseline.clone()))
            .await
            .expect("old scope replacement");
        let old = serde_json::to_value(old.expect("inline old scope entry"))
            .expect("serialize old scope replacement");
        assert_eq!(old["replyKind"], "snapshot");
        assert!(
            old["entityChanges"]["documents"]
                .as_array()
                .expect("old scope replacement documents")
                .is_empty()
        );
        let (new, _, _) = prepare(&state, &event(&state), beta, Some(baseline))
            .await
            .expect("new owner interval");
        let new = serde_json::to_value(new.expect("inline new owner entry"))
            .expect("serialize new owner interval");
        assert_eq!(
            new["entityChanges"]["documents"][0]["value"]["agent"],
            "beta"
        );
    }

    #[tokio::test]
    async fn snapshot_continuation_below_tombstone_floor_makes_bounded_progress() {
        let (_directory, state) = fixture().await;
        let store = state.store.as_ref().expect("store");
        store.transaction(|tx| {
            for index in 0..513 { SyncDocumentsRepository::put_entity(tx, "workspace", &format!("old-{index}"), json!({"stateDir":"fixture"}), false)?; }
            tx.execute("INSERT INTO sync_entity_meta(key,value) VALUES('entity_tombstone_floor','1000') ON CONFLICT(key) DO UPDATE SET value=excluded.value", [])?;
            Ok(())
        }).await.expect("old live rows and a pruned tombstone floor");
        store
            .test_read_transaction(|tx| {
                let first = SyncDocumentsRepository::resume_entity_scope(
                    tx,
                    None,
                    Some(r#"["workspace"]"#),
                    None,
                    None,
                    true,
                    SYNC_ENTITY_STREAM_MAX_DOCUMENTS,
                )?;
                assert!(first.reset);
                assert!(first.checkpoint < first.tombstone_floor);
                // Exactly two reads: the regression must fail without starting an
                // unbounded spool or relying on a timeout to stop repeated pages.
                let next = SyncDocumentsRepository::continue_entity_scope(
                    tx,
                    &first,
                    Some(r#"["workspace"]"#),
                    None,
                    None,
                    true,
                    SYNC_ENTITY_STREAM_MAX_DOCUMENTS,
                )?;
                assert!(
                    next.checkpoint > first.checkpoint,
                    "internal snapshot continuation must not re-evaluate expired external cursors"
                );
                Ok(())
            })
            .expect("bounded snapshot continuation");
    }

    #[tokio::test]
    async fn scoped_transfer_preserves_large_snapshot_and_single_oversized_record_then_sends_only_delta()
     {
        let (_directory, state) = fixture().await;
        state
            .store
            .as_ref()
            .expect("store")
            .transaction(|tx| {
                for index in 0..514 {
                    SyncDocumentsRepository::put_entity(
                        tx,
                        "workspace",
                        &format!("row-{index}"),
                        json!({"stateDir":format!("fixture-{index}")}),
                        false,
                    )?;
                }
                SyncDocumentsRepository::put_entity(
                    tx,
                    "workspace",
                    "oversized",
                    json!({"stateDir":"界\\\"\n".repeat(SYNC_STREAM_EVENT_MAX_BYTES / 4)}),
                    false,
                )?;
                Ok(())
            })
            .await
            .expect("source fixture");
        let resource: ResourceRef =
            serde_json::from_value(json!({"kind":"state"})).expect("resource");
        let (small, reader, cursor) = prepare(&state, &event(&state), resource.clone(), None)
            .await
            .expect("spooled interval");
        assert!(small.is_none());
        let mut reader = reader.expect("large snapshot is streamed");
        let mut encoded = String::new();
        let mut records = 0;
        let mut pages = 0;
        let mut maximum_wire_bytes = 0;
        let mut complete = false;
        while let Some(frame) = reader.next().await.expect("next frame") {
            let wire = wire(frame).await;
            maximum_wire_bytes = maximum_wire_bytes.max(wire.len());
            assert!(wire.len() <= SYNC_STREAM_EVENT_MAX_BYTES);
            let text = std::str::from_utf8(&wire).expect("utf8");
            let frame: Value = serde_json::from_str(
                text.lines()
                    .find_map(|line| line.strip_prefix("data: "))
                    .expect("data"),
            )
            .expect("strict frame");
            assert!(
                state
                    .validators
                    .validate_component("ResourceEntityTransferEvent", &frame)
            );
            encoded.push_str(frame["data"].as_str().expect("part"));
            if frame["partIndex"].as_u64().expect("index") + 1
                == frame["partCount"].as_u64().expect("parts")
            {
                let page: Value = serde_json::from_str(&encoded).expect("canonical page");
                assert!(
                    state
                        .validators
                        .validate_component("EntityChangeBatch", &page)
                );
                assert!(
                    page["documents"].as_array().expect("records").len()
                        <= SYNC_ENTITY_STREAM_MAX_DOCUMENTS as usize
                );
                records += page["documents"].as_array().expect("records").len();
                if let Some(oversized) = page["documents"]
                    .as_array()
                    .expect("records")
                    .iter()
                    .find(|row| row["id"] == "oversized")
                {
                    assert_eq!(
                        oversized["value"]["stateDir"],
                        "界\\\"\n".repeat(SYNC_STREAM_EVENT_MAX_BYTES / 4)
                    );
                }
                pages += 1;
                encoded.clear();
            }
            complete = frame["complete"] == true;
        }
        assert!(complete);
        assert_eq!(records, 516);
        assert!(pages >= 3);
        state
            .store
            .as_ref()
            .expect("store")
            .transaction(|tx| {
                SyncDocumentsRepository::put_entity(
                    tx,
                    "workspace",
                    "row-513",
                    json!({"stateDir":"changed"}),
                    false,
                )?;
                Ok(())
            })
            .await
            .expect("small source update");
        let (entry, reader, after_cursor) =
            prepare(&state, &event(&state), resource, Some(cursor.clone()))
                .await
                .expect("delta");
        assert!(
            reader.is_none(),
            "one changed record stays on ordinary fast path"
        );
        let batch = entry
            .expect("direct delta")
            .entity_changes
            .expect("batch")
            .expect("present");
        assert_eq!(batch.after, cursor.seq);
        assert_eq!(batch.documents.len(), 1, "no history retransmission");
        assert_eq!(batch.documents[0].id, "row-513");
        assert_eq!(batch.through, after_cursor.seq);
        eprintln!(
            "entity_transfer_fixture snapshot_records={records} pages={pages} maximum_wire_bytes={maximum_wire_bytes} delta_records={}",
            batch.documents.len()
        );
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "entity_transfer",
        routes: None,
        dependencies: &[],
    }
}
