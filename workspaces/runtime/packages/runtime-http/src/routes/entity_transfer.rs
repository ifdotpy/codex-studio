//! Scoped intervals are read once in one SQLite snapshot. Large intervals spool
//! bounded wire frames to an unlinked temporary file, owned by the live stream.
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Seek, Write};

use axum::response::sse::Event;
use serde_json::{Value, json};
use studio_runtime_schema::shared_values::{
    SYNC_ENTITY_STREAM_MAX_DOCUMENTS, SYNC_STREAM_EVENT_MAX_BYTES,
};
use studio_runtime_schema::sync_protocol::subscription_entity_scopes;
use studio_runtime_store::SyncDocumentsRepository;

use crate::error::ApiError;
use crate::generated::{
    EntityChangeBatch, ResourceChangeEvent, ResourceRef, ResourceRevisionEntry, SubscriptionCursor,
};
use crate::router::HttpState;

pub(super) struct TransferReader(BufReader<std::fs::File>);
impl TransferReader {
    pub(super) fn next(&mut self) -> Result<Option<Event>, ApiError> {
        let mut line = String::new();
        if self
            .0
            .read_line(&mut line)
            .map_err(|_| ApiError::internal())?
            == 0
        {
            return Ok(None);
        }
        let value: Value = serde_json::from_str(&line).map_err(|_| ApiError::internal())?;
        Ok(Some(super::sse_event("entity-transfer", None, value)))
    }
}

pub(super) fn owns(resource: &ResourceRef) -> bool {
    serde_json::to_value(resource)
        .ok()
        .and_then(|value| {
            value["kind"]
                .as_str()
                .map(|kind| subscription_entity_scopes().contains_key(kind))
        })
        .unwrap_or(false)
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
    let reference = serde_json::to_value(&resource).map_err(|_| ApiError::internal())?;
    let scope = subscription_entity_scopes()
        .get(reference["kind"].as_str().ok_or_else(ApiError::internal)?)
        .ok_or_else(ApiError::internal)?
        .clone();
    let collections =
        serde_json::to_string(&scope.collections).map_err(|_| ApiError::internal())?;
    let id = scope
        .id_field
        .as_deref()
        .and_then(|field| reference[field].as_str())
        .map(str::to_owned);
    let agent = scope
        .agent_field
        .as_deref()
        .and_then(|field| reference[field].as_str())
        .map(str::to_owned);
    let workspace = state.workspace_id.clone();
    let epoch = event.epoch.clone();
    let revision = event.revision;
    let reason = event.reason.clone();
    let validators = state.validators.clone();
    state.store.as_ref().ok_or_else(ApiError::internal)?.read_async("runtime-http/entity-transfer", move |tx| {
        let mut page = SyncDocumentsRepository::resume_entity_scope(tx, since.as_ref().map(|cursor| cursor.seq), Some(&collections), id.as_deref(), agent.as_deref(), SYNC_ENTITY_STREAM_MAX_DOCUMENTS).map_err(|_| ApiError::internal())?;
        let through = page.max_sequence;
        let snapshot = page.reset;
        let mut after = if snapshot { 0 } else { since.as_ref().map_or(0, |cursor| cursor.seq) };
        let cursor = SubscriptionCursor { workspace_id: workspace.clone(), seq: through };
        let transfer_id = uuid::Uuid::new_v4().to_string();
        let metadata = json!({"protocol":studio_runtime_schema::SYNC_PROTOCOL_VERSION,"workspaceId":workspace,"epoch":epoch,"revision":revision,"reason":reason,"resource":resource,"cursor":cursor,"replyKind":if snapshot {"snapshot"}else{"delta"},"transferId":transfer_id});
        let mut spool = None;
        let mut page_index = 0;
        loop {
            let checkpoint = page.checkpoint;
            let final_sql_page = checkpoint == through;
            let documents = page.rows.iter().filter(|row| !snapshot || !row.deleted).map(super::super::sync_documents::entity_document).collect::<Option<Vec<_>>>().ok_or_else(ApiError::internal)?;
            let batches = byte_pages(after, checkpoint, documents)?;
            for (index, batch) in batches.iter().enumerate() {
                let complete = final_sql_page && index + 1 == batches.len();
                let canonical = serde_json::to_value(batch).map_err(|_| ApiError::internal())?;
                if !validators.validate_component("EntityChangeBatch", &canonical) { return Err(ApiError::internal()); }
                if page_index == 0 && complete {
                    let entry: ResourceRevisionEntry = serde_json::from_value(json!({"resource":resource,"revision":through,"cursor":cursor,"replyKind":if snapshot {"snapshot"}else{"delta"},"entityChanges":batch})).map_err(|_| ApiError::internal())?;
                    let ordinary = json!({"protocol":studio_runtime_schema::SYNC_PROTOCOL_VERSION,"workspaceId":workspace,"epoch":epoch,"revision":revision,"reason":reason,"resources":[resource],"resourceVersions":[entry]});
                    if wire_bytes("resources", Some(revision), &ordinary)? <= SYNC_STREAM_EVENT_MAX_BYTES { return Ok((Some(entry), None, cursor)); }
                }
                let file = match &mut spool { Some(file) => file, None => spool.insert(tempfile::tempfile().map_err(|_| ApiError::internal())?) };
                spool_page(file, &metadata, page_index, complete, &canonical, &validators)?;
                page_index += 1;
            }
            if final_sql_page { break; }
            after = checkpoint;
            page = SyncDocumentsRepository::resume_entity_scope(tx, Some(checkpoint), Some(&collections), id.as_deref(), agent.as_deref(), SYNC_ENTITY_STREAM_MAX_DOCUMENTS).map_err(|_| ApiError::internal())?;
        }
        let mut file = spool.ok_or_else(ApiError::internal)?;
        file.rewind().map_err(|_| ApiError::internal())?;
        Ok((None, Some(TransferReader(BufReader::new(file))), cursor))
    }).await
}

fn wire_bytes(name: &str, revision: Option<i64>, value: &Value) -> Result<usize, ApiError> {
    Ok("event: \ndata: \n\n".len()
        + name.len()
        + revision.map_or(0, |revision| "id: \n".len() + revision.to_string().len())
        + serde_json::to_vec(value)
            .map_err(|_| ApiError::internal())?
            .len())
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
    page: &Value,
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
    if wire_bytes("entity-transfer", None, &direct)? <= SYNC_STREAM_EVENT_MAX_BYTES {
        if !validators.validate_component("ResourceEntityTransferEvent", &direct) {
            return Err(ApiError::internal());
        }
        serde_json::to_writer(&mut *file, &direct).map_err(|_| ApiError::internal())?;
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
        if wire_bytes("entity-transfer", None, &frame)? > SYNC_STREAM_EVENT_MAX_BYTES
            || !validators.validate_component("ResourceEntityTransferEvent", &frame)
        {
            return Err(ApiError::internal());
        }
        serde_json::to_writer(&mut *file, &frame).map_err(|_| ApiError::internal())?;
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
        while let Some(frame) = reader.next().expect("next frame") {
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
