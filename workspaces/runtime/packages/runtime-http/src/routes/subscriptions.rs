//! Resource-kind ownership for screen subscriptions. Wire transports dispatch
//! here; domain services publish changes without session or subscriber knowledge.

use futures_util::future::BoxFuture;
use studio_runtime_store::SyncDocumentsRepository;

use crate::error::ApiError;
use crate::generated::{ResourceRef, ResourceRevisionEntry, SubscriptionCursor};
use crate::router::HttpState;
use serde_json::Value;
use studio_runtime_schema::shared_values::SYNC_STREAM_EVENT_MAX_BYTES;

pub(super) async fn auth_progress(state: &HttpState, request_id: &str) -> Option<Value> {
    if let Some(driver) = state.account_driver.clone() {
        let id = request_id.to_owned();
        let receipt = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/auth-progress",
            move || {
                driver.snapshot().map(|snapshot| {
                    snapshot
                        .logins
                        .into_iter()
                        .find(|value| value.get("requestId").and_then(Value::as_str) == Some(&id))
                })
            },
        )
        .await
        .ok()?
        .ok()?;
        if let Some(receipt) = receipt {
            return Some(receipt);
        }
    }
    match &state.claude_login {
        Some(login) => Some(login.progress(request_id).await.unwrap_or(Value::Null)),
        None => Some(Value::Null),
    }
}

/// Each generated resource kind has exactly one registered handler. A handler
/// reads its records and durable cursor in one database snapshot. The byte bound
/// includes its encoded payload; oversized data uses scoped recovery.
pub(crate) trait ResourceKindHandler: Send + Sync {
    fn accepts(&self, resource: &ResourceRef) -> bool;

    fn initial_snapshot(
        &self,
        state: HttpState,
        resource: ResourceRef,
        since: Option<SubscriptionCursor>,
    ) -> BoxFuture<'_, Result<ResourceRevisionEntry, ApiError>>;

    fn matches_change(&self, resource: &ResourceRef, changed: &ResourceRef) -> bool;

    /// Encoded payload bound in bytes; shared stream limit remains its owner.
    fn max_bytes(&self) -> usize {
        SYNC_STREAM_EVENT_MAX_BYTES
    }

    /// Diagnostic key only: no caller may parse this back into a reference.
    fn debug_key(&self, resource: &ResourceRef) -> String {
        format!("{resource:?}")
    }

    fn content_fingerprint(
        &self,
        state: HttpState,
        resource: ResourceRef,
    ) -> BoxFuture<'_, Option<Vec<u8>>>;
}

/// Empty data for kinds whose baseline is already carried by the shared frame.
/// V4 preserves the existing revision when this entry has no durable cursor.
fn empty_entry() -> Result<ResourceRevisionEntry, ApiError> {
    serde_json::from_value(serde_json::json!({"revision": 0})).map_err(|_| ApiError::internal())
}

macro_rules! resource_handlers {
    ($( $handler:ident => $variant:ident, $initial:ident; )+) => {
        $(
            struct $handler;
            impl ResourceKindHandler for $handler {
                fn accepts(&self, resource: &ResourceRef) -> bool {
                    matches!(resource, ResourceRef::$variant(_))
                }
                fn initial_snapshot(&self, state: HttpState, resource: ResourceRef, since: Option<SubscriptionCursor>)
                    -> BoxFuture<'_, Result<ResourceRevisionEntry, ApiError>> {
                    Box::pin($initial(state, resource, since))
                }
                fn matches_change(&self, resource: &ResourceRef, changed: &ResourceRef) -> bool {
                    self.accepts(resource) && self.accepts(changed) && same_scope(resource, changed)
                }
                fn content_fingerprint(&self, state: HttpState, resource: ResourceRef) -> BoxFuture<'_, Option<Vec<u8>>> {
                    Box::pin(content_fingerprint(state, resource))
                }
            }
        )+
        static REGISTRY: &[&dyn ResourceKindHandler] = &[$(&$handler),+];
    };
}

async fn passive_initial(
    _state: HttpState,
    _resource: ResourceRef,
    _since: Option<SubscriptionCursor>,
) -> Result<ResourceRevisionEntry, ApiError> {
    empty_entry()
}

async fn transcript_initial(
    state: HttpState,
    resource: ResourceRef,
    since: Option<SubscriptionCursor>,
) -> Result<ResourceRevisionEntry, ApiError> {
    let tail = match &resource {
        ResourceRef::TranscriptResource(transcript) => transcript.tail,
        ResourceRef::AccountsResource(_)
        | ResourceRef::CostsResource(_)
        | ResourceRef::DesktopResource(_)
        | ResourceRef::DraftsResource(_)
        | ResourceRef::LimitsResource(_)
        | ResourceRef::ModelsResource(_)
        | ResourceRef::MultiServerResource(_)
        | ResourceRef::QueueResource(_)
        | ResourceRef::ReceiptsResource(_)
        | ResourceRef::RoomResource(_)
        | ResourceRef::SessionCostResource(_)
        | ResourceRef::StateResource(_)
        | ResourceRef::TaskResource(_)
        | ResourceRef::TasksResource(_)
        | ResourceRef::TerminalResource(_)
        | ResourceRef::TerminalsResource(_)
        | ResourceRef::TranscriptsResource(_)
        | ResourceRef::VoiceResource(_)
        | ResourceRef::WorkspaceResource(_)
        | ResourceRef::AuthProgressResource(_)
        | ResourceRef::VaultReadinessResource(_)
        | ResourceRef::VaultJobProgressResource(_)
        | ResourceRef::RenameProgressResource(_)
        | ResourceRef::AgentDetailResource(_)
        | ResourceRef::MonitorsResource(_)
        | ResourceRef::ProcessesResource(_)
        | ResourceRef::WorkResource(_)
        | ResourceRef::ComplaintResource(_)
        | ResourceRef::TerminalOutputResource(_) => return Err(ApiError::internal()),
    };
    let mut entry = super::sync_stream::transcript_initial_snapshot(state, resource, since).await?;
    if let Some(Some(page)) = entry.transcript_snapshot.take() {
        entry.transcript_snapshot = Some(Some(super::transcript::window_snapshot(page, tail)));
    }
    Ok(entry)
}

async fn entity_initial(
    state: HttpState,
    resource: ResourceRef,
    since: Option<SubscriptionCursor>,
) -> Result<ResourceRevisionEntry, ApiError> {
    super::subscription_entities::initial_snapshot(state, resource, since).await
}

// View records are transient except resumable room/terminal output, which the
// renderer stores together with its applied cursor before notifying readers.
fn is_view_resource(resource: &ResourceRef) -> bool {
    matches!(
        resource,
        ResourceRef::ProcessesResource(_)
            | ResourceRef::ComplaintResource(_)
            | ResourceRef::RoomResource(_)
            | ResourceRef::TerminalOutputResource(_)
    )
}

async fn view_value(
    state: HttpState,
    resource: ResourceRef,
    since: Option<SubscriptionCursor>,
) -> Result<Value, ApiError> {
    use serde_json::json;
    match resource {
        ResourceRef::ProcessesResource(r) => {
            Ok(json!({"processes":super::process_controls::read_listing(state, r.agent_id).await?}))
        }
        ResourceRef::ComplaintResource(r) => Ok(
            json!({"complaint":super::questions_reads::complaint_for_id(state, r.complaint_id).await?}),
        ),
        ResourceRef::RoomResource(r) => {
            if r.legacy == Some(true) {
                let store = state.store.ok_or_else(ApiError::internal)?;
                let messages = store
                    .read_async("runtime-http/legacy-room-subscription", move |tx| {
                        studio_runtime_services::coordination::history(tx, &r.room_id)
                            .map_err(|_| ApiError::internal())
                    })
                    .await?;
                return Ok(
                    json!({"legacyMessages":studio_runtime_store::projection_value(&messages)}),
                );
            }
            let mut room = serde_json::to_value(
                super::messages::room_snapshot(
                    state,
                    r.room_id,
                    None,
                    None,
                    studio_runtime_schema::shared_values::SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS as i64,
                )
                .await?,
            )
            .map_err(|_| ApiError::internal())?;
            // Preserve an exclusive history boundary while fitting a bounded tail.
            while serde_json::to_vec(&room)
                .map_err(|_| ApiError::internal())?
                .len()
                > SYNC_STREAM_EVENT_MAX_BYTES / 2
            {
                let Some(messages) = room["messages"].as_array_mut() else {
                    break;
                };
                if messages.len() <= 1 {
                    break;
                }
                messages.drain(..messages.len() / 2);
                let first = messages[0]["seq"].clone();
                room["nextBefore"] = first;
            }
            Ok(json!({"room":room}))
        }
        ResourceRef::TerminalOutputResource(r) => {
            let since = since.filter(|c| c.workspace_id == state.workspace_id);
            let offset = since.as_ref().map_or(0, |c| c.seq);
            let generation = since.and_then(|c| c.terminal_generation);
            let store = state.store.ok_or_else(ApiError::internal)?;
            let (mut output, generation) =
                studio_runtime_services::terminal_driver::subscription_output(
                    &store,
                    &r.terminal_id,
                    offset,
                    generation,
                )
                .await
                .map_err(super::terminals::terminal_error)?;
            // Worst-case JSON escaping is six bytes per scalar. This bound leaves
            // room for resource metadata and the event envelope.
            let max_chars = studio_runtime_schema::shared_values::SYNC_TERMINAL_STREAM_MAX_CHARS;
            let text = output["text"].as_str().ok_or_else(ApiError::internal)?;
            let count = text.chars().count();
            if count > max_chars {
                output["text"] = json!(text.chars().skip(count - max_chars).collect::<String>());
                output["truncated"] = json!(true);
            }
            Ok(json!({"terminalOutput":output,"terminalGeneration":generation}))
        }
        ResourceRef::AccountsResource(_)
        | ResourceRef::CostsResource(_)
        | ResourceRef::DesktopResource(_)
        | ResourceRef::DraftsResource(_)
        | ResourceRef::LimitsResource(_)
        | ResourceRef::ModelsResource(_)
        | ResourceRef::MultiServerResource(_)
        | ResourceRef::QueueResource(_)
        | ResourceRef::ReceiptsResource(_)
        | ResourceRef::SessionCostResource(_)
        | ResourceRef::StateResource(_)
        | ResourceRef::TaskResource(_)
        | ResourceRef::TasksResource(_)
        | ResourceRef::TerminalResource(_)
        | ResourceRef::TerminalsResource(_)
        | ResourceRef::TranscriptResource(_)
        | ResourceRef::TranscriptsResource(_)
        | ResourceRef::VoiceResource(_)
        | ResourceRef::WorkspaceResource(_)
        | ResourceRef::AuthProgressResource(_)
        | ResourceRef::VaultReadinessResource(_)
        | ResourceRef::VaultJobProgressResource(_)
        | ResourceRef::RenameProgressResource(_)
        | ResourceRef::AgentDetailResource(_)
        | ResourceRef::MonitorsResource(_)
        | ResourceRef::WorkResource(_) => Err(ApiError::internal()),
    }
}

async fn view_initial(
    state: HttpState,
    resource: ResourceRef,
    since: Option<SubscriptionCursor>,
) -> Result<ResourceRevisionEntry, ApiError> {
    let mut snapshot = view_value(state.clone(), resource.clone(), since.clone()).await?;
    let generation = snapshot
        .as_object_mut()
        .and_then(|s| s.remove("terminalGeneration"))
        .and_then(|generation| generation.as_i64());
    let mut value = serde_json::json!({"revision":0,"resource":resource,"viewSnapshot":snapshot});
    if let Some(output) = snapshot.get("terminalOutput") {
        let offset = output["offset"].as_i64().ok_or_else(ApiError::internal)?;
        value["revision"] = serde_json::json!(offset);
        value["cursor"] = serde_json::json!({"workspaceId":state.workspace_id,"seq":offset,"terminalGeneration":generation});
        value["replyKind"] = serde_json::json!(if since.as_ref().is_some_and(|c| c.workspace_id
            == state.workspace_id
            && c.seq <= offset
            && c.terminal_generation.is_some()
            && c.terminal_generation == generation)
            && output["truncated"] == false
        {
            "delta"
        } else {
            "snapshot"
        });
        if value["replyKind"] == "delta" {
            value["terminalBaseOffset"] = serde_json::json!(since.map(|cursor| cursor.seq));
        }
    }
    if let Some(room) = snapshot.get("room") {
        let seq = room["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .and_then(|message| message["seq"].as_i64())
            .unwrap_or(0);
        value["cursor"] = serde_json::json!({"workspaceId":state.workspace_id,"seq":seq});
        value["replyKind"] = serde_json::json!("snapshot");
    }
    serde_json::from_value(value).map_err(|_| ApiError::internal())
}

/// Each live stream owns only the last terminal offset it sent. Reuse the same
/// reader and reset rules as reconnect; other view handlers keep their snapshots.
pub(super) async fn attach_terminal_output_cursors(
    state: Option<&HttpState>,
    event: &mut crate::generated::ResourceChangeEvent,
    cursors: &mut std::collections::BTreeMap<String, SubscriptionCursor>,
) {
    let Some(state) = state else { return };
    for (resource, entry) in event.resources.iter().zip(&mut event.resource_versions) {
        let ResourceRef::TerminalOutputResource(terminal) = resource else {
            continue;
        };
        if entry.view_snapshot.is_none() {
            match initial_snapshot(
                state.clone(),
                resource.clone(),
                cursors.get(&terminal.terminal_id).cloned(),
            )
            .await
            {
                Ok(mut snapshot) => {
                    snapshot.revision = entry.revision;
                    *entry = snapshot;
                }
                Err(error) => {
                    cursors.remove(&terminal.terminal_id);
                    eprintln!("subscribed terminal interval unavailable: {error:?}");
                    continue;
                }
            }
        }
        if let Some(cursor) = &entry.cursor {
            cursors.insert(terminal.terminal_id.clone(), cursor.clone());
        }
    }
}

pub(super) async fn attach_view_snapshots(
    state: Option<&HttpState>,
    event: &mut crate::generated::ResourceChangeEvent,
) {
    let Some(state) = state else { return };
    for (resource, entry) in event.resources.iter().zip(&mut event.resource_versions) {
        if !is_view_resource(resource) || entry.view_snapshot.is_some() {
            continue;
        }
        match initial_snapshot(state.clone(), resource.clone(), None).await {
            Ok(mut snapshot) => {
                snapshot.revision = entry.revision;
                *entry = snapshot;
            }
            Err(error) => eprintln!("subscribed view snapshot unavailable: {error:?}"),
        }
    }
}

// This is the only resource-kind registration list. The parity witness compares
// its complete set against the generated OpenAPI ResourceRef discriminator.
resource_handlers! {
    ProcessesResourceHandler => ProcessesResource, view_initial;
    ComplaintResourceHandler => ComplaintResource, view_initial;
    TerminalOutputResourceHandler => TerminalOutputResource, view_initial;
    AgentDetailResourceHandler => AgentDetailResource, entity_initial;
    MonitorsResourceHandler => MonitorsResource, entity_initial;
    AccountsResourceHandler => AccountsResource, passive_initial;
    AuthProgressResourceHandler => AuthProgressResource, passive_initial;
    CostsResourceHandler => CostsResource, entity_initial;
    DesktopResourceHandler => DesktopResource, entity_initial;
    DraftsResourceHandler => DraftsResource, passive_initial;
    LimitsResourceHandler => LimitsResource, passive_initial;
    ModelsResourceHandler => ModelsResource, passive_initial;
    MultiServerResourceHandler => MultiServerResource, passive_initial;
    QueueResourceHandler => QueueResource, passive_initial;
    ReceiptsResourceHandler => ReceiptsResource, passive_initial;
    RenameProgressResourceHandler => RenameProgressResource, passive_initial;
    RoomResourceHandler => RoomResource, view_initial;
    SessionCostResourceHandler => SessionCostResource, entity_initial;
    StateResourceHandler => StateResource, entity_initial;
    TaskResourceHandler => TaskResource, entity_initial;
    TasksResourceHandler => TasksResource, entity_initial;
    TerminalResourceHandler => TerminalResource, entity_initial;
    TerminalsResourceHandler => TerminalsResource, entity_initial;
    TranscriptResourceHandler => TranscriptResource, transcript_initial;
    TranscriptsResourceHandler => TranscriptsResource, passive_initial;
    VaultJobProgressResourceHandler => VaultJobProgressResource, passive_initial;
    VaultReadinessResourceHandler => VaultReadinessResource, passive_initial;
    VoiceResourceHandler => VoiceResource, passive_initial;
    WorkResourceHandler => WorkResource, entity_initial;
    WorkspaceResourceHandler => WorkspaceResource, passive_initial;
}

pub(super) fn handler(
    resource: &ResourceRef,
) -> Result<&'static dyn ResourceKindHandler, ApiError> {
    REGISTRY
        .iter()
        .copied()
        .find(|handler| handler.accepts(resource))
        .ok_or_else(ApiError::internal)
}

pub(super) async fn initial_snapshot(
    state: HttpState,
    resource: ResourceRef,
    since: Option<SubscriptionCursor>,
) -> Result<ResourceRevisionEntry, ApiError> {
    let handler = handler(&resource)?;
    let reference = resource.clone();
    let entry = handler.initial_snapshot(state, resource, since).await?;
    if entry
        .resource
        .as_ref()
        .and_then(Option::as_ref)
        .is_some_and(|changed| !handler.matches_change(&reference, changed))
    {
        return Err(ApiError::internal());
    }
    if serde_json::to_vec(&entry)
        .map_err(|_| ApiError::internal())?
        .len()
        > handler.max_bytes()
    {
        return serde_json::from_value(
            serde_json::json!({"revision":entry.revision,"resource":reference}),
        )
        .map_err(|_| ApiError::internal());
    }
    Ok(entry)
}

pub(super) async fn resource_content(state: HttpState, resource: ResourceRef) -> Option<Vec<u8>> {
    let handler = handler(&resource).ok()?;
    let bytes = handler.content_fingerprint(state, resource).await?;
    (bytes.len() <= handler.max_bytes()).then_some(bytes)
}

fn same_scope(resource: &ResourceRef, changed: &ResourceRef) -> bool {
    match (resource, changed) {
        (ResourceRef::TranscriptResource(left), ResourceRef::TranscriptResource(right)) => {
            left.agent_id == right.agent_id
        }
        _ => serde_json::to_value(resource)
            .ok()
            .zip(serde_json::to_value(changed).ok())
            .is_some_and(|(left, right)| left == right),
    }
}

async fn content_fingerprint(state: HttpState, resource: ResourceRef) -> Option<Vec<u8>> {
    if let ResourceRef::ComplaintResource(r) = &resource {
        let id = r.complaint_id.clone();
        return state
            .store?
            .read_async("runtime-http/complaint-fingerprint", move |connection| {
                studio_runtime_store::QuestionsReadsRepository::complaint_fingerprint(
                    connection, &id,
                )
            })
            .await
            .ok()?
            .map(String::into_bytes);
    }
    if let ResourceRef::TerminalOutputResource(r) = &resource {
        let id = r.terminal_id.clone();
        return state
            .store?
            .read_async("runtime-http/terminal-fingerprint", move |connection| {
                studio_runtime_store::TerminalsRepository::fingerprint(connection, &id)
            })
            .await
            .ok()?;
    }
    if is_view_resource(&resource) {
        return view_value(state, resource, None)
            .await
            .ok()
            .and_then(|value| serde_json::to_vec(&value).ok());
    }
    match resource {
        ResourceRef::DraftsResource(_) => state
            .store?
            .read_async(
                "runtime-http/routes/sync_stream/draft-revision",
                |connection| {
                    let sequence = SyncDocumentsRepository::draft_sequence(connection)?;
                    serde_json::to_vec(&sequence).map_err(studio_runtime_store::StoreError::from)
                },
            )
            .await
            .ok(),
        ResourceRef::TranscriptResource(resource) => {
            // A scoped stream hint may precede the next stored transcript flush.
            let publication = state
                .resource_notifications
                .published_revision(&serde_json::to_value(&resource).ok()?)
                .ok()?;
            let store = state.store?;
            store
                .read_async(
                    "runtime-http/routes/sync_stream/transcript-revision",
                    move |connection| {
                        let revision = studio_runtime_store::TranscriptRepository::revision(
                            connection,
                            &resource.agent_id,
                        )?;
                        serde_json::to_vec(&(revision, publication))
                            .map_err(studio_runtime_store::StoreError::from)
                    },
                )
                .await
                .ok()
        }
        ResourceRef::RenameProgressResource(resource) => state
            .store?
            .read_async("runtime-http/rename-progress", move |connection| {
                let reply = studio_runtime_services::title_generation::progress(
                    connection,
                    &resource.agent_id,
                    &resource.request_id,
                )
                .map_err(|_| studio_runtime_store::StoreError::RecordContract("rename progress"))?;
                serde_json::to_vec(&reply).map_err(studio_runtime_store::StoreError::from)
            })
            .await
            .ok(),
        ResourceRef::AuthProgressResource(resource) => {
            serde_json::to_vec(&auth_progress(&state, &resource.request_id).await?).ok()
        }
        ResourceRef::VaultJobProgressResource(resource) => serde_json::to_vec(
            &super::host_secrets::job_progress(&state, resource.job, &resource.request_id).await,
        )
        .ok(),
        ResourceRef::VaultReadinessResource(resource) => {
            let settings = super::host_secrets::settings_dir(&state.state_dir);
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-http/vault-readiness",
                move || studio_runtime_services::vault::keys_ready(&settings, &resource.prefix),
            )
            .await
            .ok()?
            .ok()
            .and_then(|ready| serde_json::to_vec(&ready).ok())
        }
        ResourceRef::MultiServerResource(_) if cfg!(unix) => {
            serde_json::to_vec(&super::multi_server::read_snapshot(&state).await.ok()?).ok()
        }
        ResourceRef::VoiceResource(resource) => state
            .store?
            .read_async("runtime-http/voice-resource-revision", move |connection| {
                studio_runtime_store::VoiceRepository::fingerprint(connection, &resource.agent_id)
            })
            .await
            .ok(),
        ResourceRef::AccountsResource(_) => None,
        ResourceRef::LimitsResource(resource) => {
            let mut snapshot = if let Some(port) = &state.account_session {
                port.limits_snapshot(&resource.account_key).ok()?
            } else {
                state.account_policy.as_ref()?.limits(&resource.account_key)
            };
            // Observation receipts advance privately without changing rateLimit entities.
            // Fingerprint the same semantic content so unrelated commits cannot
            // turn a timestamp refresh into another provider read by the client.
            if let Some(fields) = snapshot.as_object_mut() {
                for field in studio_runtime_store::account_state::LIMITS_OBSERVATION_TIME_FIELDS {
                    fields.remove(*field);
                }
            }
            let snapshot: crate::generated::UsageLimitsResponse =
                serde_json::from_value(snapshot).ok()?;
            serde_json::to_vec(&snapshot).ok()
        }
        ResourceRef::DesktopResource(_) | ResourceRef::TerminalsResource(_) => None,
        ResourceRef::QueueResource(_) => None,
        ResourceRef::SessionCostResource(_) | ResourceRef::CostsResource(_) => None,
        ResourceRef::ModelsResource(_)
        | ResourceRef::MultiServerResource(_)
        | ResourceRef::ReceiptsResource(_)
        | ResourceRef::RoomResource(_)
        | ResourceRef::StateResource(_)
        | ResourceRef::TaskResource(_)
        | ResourceRef::TasksResource(_)
        | ResourceRef::TerminalResource(_)
        | ResourceRef::TranscriptsResource(_)
        | ResourceRef::WorkspaceResource(_)
        | ResourceRef::AgentDetailResource(_)
        | ResourceRef::MonitorsResource(_)
        | ResourceRef::ProcessesResource(_)
        | ResourceRef::WorkResource(_)
        | ResourceRef::ComplaintResource(_)
        | ResourceRef::TerminalOutputResource(_) => None,
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "subscriptions",
        routes: None,
        dependencies: &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeSet;

    #[tokio::test]
    async fn stored_fingerprints_follow_edits_metadata_and_deletion() {
        use std::sync::Arc;
        use studio_runtime_store::{RuntimeStore, StoreError};
        let directory = tempfile::tempdir().expect("state");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("store"),
        );
        store.transaction(|tx| {
            tx.execute("INSERT INTO runtime_complaints(id,record) VALUES('c',?1)", [r#"{"id":"c","leadId":"a","author":"user","text":"first","status":"open","created":1}"#])?;
            tx.execute("INSERT INTO user_terminals(id,record,output,offset) VALUES('t',?1,'first',0)", [r#"{"id":"t","agent":"a","title":"Shell","cwd":"/","created":1,"status":"running"}"#])?;
            tx.execute("INSERT INTO voice_sessions(id,agent,created,state,answer) VALUES('v','a',1,'active','first')", [])?;
            Ok::<_, StoreError>(())
        }).await.expect("fixtures");
        let state = HttpState::new(
            "token".into(),
            "workspace".into(),
            directory.path().to_string_lossy().into_owned(),
            4620,
            None,
            Some(store.clone()),
            None,
        );
        let complaint = ResourceRef::ComplaintResource(crate::generated::ComplaintResource {
            complaint_id: "c".into(),
            kind: crate::generated::ComplaintResourceKind::Complaint,
        });
        let terminal =
            ResourceRef::TerminalOutputResource(crate::generated::TerminalOutputResource {
                terminal_id: "t".into(),
                kind: crate::generated::TerminalOutputResourceKind::TerminalOutput,
            });
        let voice = ResourceRef::VoiceResource(crate::generated::VoiceResource {
            agent_id: "a".into(),
            kind: crate::generated::VoiceResourceKind::Voice,
        });
        let before = [complaint.clone(), terminal.clone(), voice.clone()];
        let mut fingerprints = Vec::new();
        for resource in &before {
            fingerprints.push(resource_content(state.clone(), resource.clone()).await);
        }
        // Raw fingerprints retain stored spelling, without decoding a view.
        assert_eq!(fingerprints[0].as_deref(), Some(br#"{"id":"c","leadId":"a","author":"user","text":"first","status":"open","created":1}"#.as_slice()));
        store.transaction(|tx| {
            tx.execute("UPDATE runtime_complaints SET record=?1 WHERE id='c'", [r#"{"id":"c","leadId":"a","author":"user","text":"edited","status":"open","created":1}"#])?;
            tx.execute("UPDATE user_terminals SET output='edited' WHERE id='t'", [])?;
            tx.execute("UPDATE voice_sessions SET answer='edited' WHERE id='v'", [])?;
            Ok::<_, StoreError>(())
        }).await.expect("edit");
        for (index, resource) in before.iter().enumerate() {
            assert_ne!(
                fingerprints[index],
                resource_content(state.clone(), resource.clone()).await
            );
        }
        let mut edited = Vec::new();
        for resource in &before {
            edited.push(resource_content(state.clone(), resource.clone()).await);
        }
        store.transaction(|tx| {
            tx.execute("UPDATE runtime_complaints SET record=?1 WHERE id='c'", [r#"{"id":"c","leadId":"a","author":"user","text":"edited","status":"closed","created":1}"#])?;
            tx.execute("UPDATE user_terminals SET record=?1 WHERE id='t'", [r#"{"id":"t","agent":"a","title":"New","cwd":"/","created":1,"status":"exited"}"#])?;
            tx.execute("UPDATE voice_sessions SET state='ended' WHERE id='v'", [])?;
            Ok::<_, StoreError>(())
        }).await.expect("metadata");
        for (index, resource) in before.iter().enumerate() {
            let next = resource_content(state.clone(), resource.clone()).await;
            assert_ne!(edited[index], next, "metadata {index}");
            edited[index] = next;
        }
        store
            .transaction(|tx| {
                tx.execute("DELETE FROM runtime_complaints WHERE id='c'", [])?;
                tx.execute("DELETE FROM user_terminals WHERE id='t'", [])?;
                tx.execute("DELETE FROM voice_sessions WHERE id='v'", [])?;
                Ok::<_, StoreError>(())
            })
            .await
            .expect("delete");
        for (index, resource) in before.iter().enumerate() {
            assert_ne!(
                edited[index],
                resource_content(state.clone(), resource.clone()).await
            );
        }
    }

    #[tokio::test]
    async fn room_tail_reduces_geometrically_and_keeps_history_boundary() {
        use std::sync::Arc;
        use studio_runtime_store::{RuntimeStore, StoreError};
        let directory = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("store"),
        );
        store.transaction_with(|tx| {
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES('lead',?1)", [json!({"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,"id":"lead","rootId":"lead","isLead":true,"epoch":1,"status":"idle","name":"Lead","threadId":"thread"}).to_string()])?;
            tx.execute("INSERT INTO runtime_rooms(id,record) VALUES('room',?1)", [json!({"id":"room","kind":"broadcast","rootId":"lead","members":["lead"],"name":"Team"}).to_string()])?;
            for i in 0..40 {
                tx.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES(?1,'room','lead',?2,?3,'{}')", rusqlite::params![format!("message-{i}"), "x".repeat(10_000), i])?;
            }
            Ok::<_, StoreError>(())
        }).await.expect("room fixture");
        let state = HttpState::new(
            "token".into(),
            "workspace".into(),
            directory.path().to_string_lossy().into_owned(),
            4620,
            None,
            Some(store),
            None,
        );
        let resource =
            serde_json::from_value(json!({"kind":"room","roomId":"room"})).expect("room resource");
        let value = view_value(state.clone(), resource, None)
            .await
            .expect("room tail");
        let room = &value["room"];
        let messages = room["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 10);
        assert_eq!(room["nextBefore"], messages[0]["seq"]);
        let before = room["nextBefore"].as_i64().expect("history boundary");
        let history =
            super::super::messages::room_snapshot(state, "room".into(), Some(before), None, 40)
                .await
                .expect("history");
        let history = serde_json::to_value(history).expect("history value");
        assert_eq!(
            history["messages"]
                .as_array()
                .expect("history messages")
                .len(),
            30
        );
        assert!(
            serde_json::to_vec(room).expect("serialized tail").len()
                <= SYNC_STREAM_EVENT_MAX_BYTES / 2
        );
    }

    #[tokio::test]
    async fn subscribed_views_carry_bounded_records_and_terminal_resume() {
        use std::sync::Arc;
        use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore, StoreError};
        let directory = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("store"),
        );
        studio_runtime_services::entity_publication::install(&store)
            .await
            .expect("install production entity publisher");
        store.transaction_with(|tx| {
            AgentRepository::put(tx, &AgentRecord::from_value(json!({"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,"id":"lead","rootId":"lead","isLead":true,"epoch":1,"status":"idle","name":"Lead","threadId":"thread"}))?)?;
            tx.execute("INSERT INTO runtime_work(id,record) VALUES('assignment',?1)",[json!({"id":"assignment","rootId":"lead","title":"Published assignment","status":"running","created":1,"updated":1,"version":1,"results":[{"id":"evidence","agent":"lead","text":"Existing evidence","checks":"fixture","revision":"abc","files":[],"created":1}]}).to_string()])?;
            tx.execute("INSERT INTO runtime_complaints(id,record) VALUES('message',?1)", [json!({"id":"message","leadId":"lead","recipient":"lead","status":"open","responses":[],"author":"lead","text":"Initial complaint","created":1,"version":1}).to_string()])?;
            tx.execute("INSERT INTO runtime_rooms(id,record) VALUES('broadcast:lead',?1)", [json!({"id":"broadcast:lead","kind":"broadcast","rootId":"lead","members":["lead"],"name":"Team"}).to_string()])?;
            tx.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES('first','broadcast:lead','lead','First message',1,'{}')", [])?;
            tx.execute("INSERT INTO user_terminals(id,record,output,offset) VALUES('shell',?1,'a🙂',0)", [json!({"id":"shell","agent":"lead","title":"Shell","cwd":"/","created":1,"status":"running"}).to_string()])?;
            Ok::<_,StoreError>(())
        }).await.expect("source records");
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
        for (resource, field) in [
            (json!({"kind":"processes","agentId":"lead"}), "processes"),
            (
                json!({"kind":"complaint","complaintId":"message"}),
                "complaint",
            ),
            (json!({"kind":"room","roomId":"broadcast:lead"}), "room"),
            (
                json!({"kind":"terminal-output","terminalId":"shell"}),
                "terminalOutput",
            ),
        ] {
            let resource: ResourceRef = serde_json::from_value(resource).expect("typed reference");
            let baseline = initial_snapshot(state.clone(), resource.clone(), None)
                .await
                .expect("baseline");
            let baseline = serde_json::to_value(baseline).expect("frame");
            assert!(
                !baseline["viewSnapshot"][field].is_null(),
                "{field} baseline"
            );
            let mut event:crate::generated::ResourceChangeEvent = serde_json::from_value(json!({"protocol":studio_runtime_schema::SYNC_PROTOCOL_VERSION,"workspaceId":state.workspace_id,"epoch":"epoch","revision":2,"reason":"change","resources":[resource],"resourceVersions":[{"revision":2}]})).expect("event");
            attach_view_snapshots(Some(&state), &mut event).await;
            let live = serde_json::to_value(event).expect("live frame");
            assert_eq!(
                live["resourceVersions"][0]["viewSnapshot"][field],
                baseline["viewSnapshot"][field]
            );
            assert!(
                serde_json::to_vec(&live).expect("encoded").len() <= SYNC_STREAM_EVENT_MAX_BYTES
            );
        }
        // Work shares the public resource registry, but its records are entity
        // partitions rather than the full-view snapshots exercised above.
        let work: ResourceRef =
            serde_json::from_value(json!({"kind":"work","agentId":"lead"})).expect("work ref");
        let baseline = initial_snapshot(state.clone(), work.clone(), None)
            .await
            .expect("work entities");
        let baseline = serde_json::to_value(baseline).expect("work frame");
        assert!(baseline.get("viewSnapshot").is_none());
        assert_eq!(baseline["replyKind"], "snapshot");
        let documents = baseline["entityChanges"]["documents"]
            .as_array()
            .expect("typed work records");
        assert_eq!(
            documents
                .iter()
                .map(|row| row["collection"]
                    .as_str()
                    .expect("work entity collection name"))
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["workDetail", "workResult"])
        );
        let detail = documents
            .iter()
            .find(|row| row["collection"] == "workDetail")
            .expect("metadata");
        assert_eq!(detail["id"], "assignment");
        assert_eq!(detail["value"]["results"], json!([]));
        let result = documents
            .iter()
            .find(|row| row["collection"] == "workResult")
            .expect("history");
        assert_eq!(result["value"]["value"]["text"], "Existing evidence");
        assert!(
            serde_json::to_vec(&baseline)
                .expect("encode work baseline for byte bound")
                .len()
                <= SYNC_STREAM_EVENT_MAX_BYTES
        );
        let cursor = serde_json::from_value(baseline["cursor"].clone()).expect("work cursor");
        store.transaction_with(|tx| {
            tx.execute("UPDATE runtime_work SET record=json_set(record,'$.status','review','$.updated',2,'$.version',2) WHERE id='assignment'",[])?;
            Ok::<_,StoreError>(())
        }).await.expect("work status commit");
        let live = initial_snapshot(state.clone(), work.clone(), Some(cursor))
            .await
            .expect("work delta");
        let live = serde_json::to_value(live).expect("work delta frame");
        assert!(live.get("viewSnapshot").is_none());
        assert_eq!(live["replyKind"], "delta");
        let changed = live["entityChanges"]["documents"]
            .as_array()
            .expect("changed work records");
        assert_eq!(changed.len(), 1, "ordinary status must not replay history");
        assert_eq!(changed[0]["collection"], "workDetail");
        assert_eq!(changed[0]["value"]["status"], "review");
        assert!(
            serde_json::to_vec(&live)
                .expect("encode work delta for byte bound")
                .len()
                <= SYNC_STREAM_EVENT_MAX_BYTES
        );
        let mut event:crate::generated::ResourceChangeEvent=serde_json::from_value(json!({"protocol":studio_runtime_schema::SYNC_PROTOCOL_VERSION,"workspaceId":state.workspace_id,"epoch":"epoch","revision":2,"reason":"change","resources":[work],"resourceVersions":[{"revision":2}]})).expect("work notification");
        attach_view_snapshots(Some(&state), &mut event).await;
        assert!(
            serde_json::to_value(event).expect("serialize work notification")["resourceVersions"]
                [0]
            .get("viewSnapshot")
            .is_none(),
            "legacy full-view helper must not reattach work history"
        );
        let resource: ResourceRef =
            serde_json::from_value(json!({"kind":"terminal-output","terminalId":"shell"}))
                .expect("terminal ref");
        let cursor = SubscriptionCursor {
            terminal_generation: Some(0),
            workspace_id: state.workspace_id.clone(),
            seq: 2,
        };
        let first = initial_snapshot(state.clone(), resource.clone(), None)
            .await
            .expect("first");
        assert_eq!(
            serde_json::to_value(first).expect("first frame")["viewSnapshot"]["terminalOutput"]["text"],
            "a🙂"
        );
        store
            .transaction_with(|tx| {
                tx.execute(
                    "UPDATE user_terminals SET output='a🙂中' WHERE id='shell'",
                    [],
                )?;
                Ok::<_, StoreError>(())
            })
            .await
            .expect("output commit");
        let resumed = initial_snapshot(state.clone(), resource.clone(), Some(cursor))
            .await
            .expect("resume");
        let resumed = serde_json::to_value(resumed).expect("resumed frame");
        assert_eq!(resumed["viewSnapshot"]["terminalOutput"]["text"], "中");
        assert_eq!(resumed["cursor"]["seq"], 3);
        assert_eq!(resumed["replyKind"], "delta");
        assert_eq!(resumed["terminalBaseOffset"], 2);
        let future = initial_snapshot(
            state.clone(),
            resource.clone(),
            Some(SubscriptionCursor {
                terminal_generation: Some(0),
                workspace_id: state.workspace_id.clone(),
                seq: 99,
            }),
        )
        .await
        .expect("future cursor snapshot");
        let future = serde_json::to_value(future).expect("future frame");
        assert_eq!(future["replyKind"], "snapshot");
        assert_eq!(future["viewSnapshot"]["terminalOutput"]["text"], "a🙂中");
        // A failed client reset write leaves the old generation at offset 40.
        // Even after replacement output grows past 40, reconnect must snapshot.
        store
            .transaction_with(|tx| {
                let row =
                    studio_runtime_store::TerminalsRepository::get(tx, "shell")?.expect("terminal");
                studio_runtime_store::TerminalsRepository::put(
                    tx,
                    "shell",
                    &row.record,
                    &"o".repeat(40),
                    0,
                )?;
                Ok::<_, StoreError>(())
            })
            .await
            .expect("old output");
        let old = initial_snapshot(state.clone(), resource.clone(), None)
            .await
            .expect("old snapshot");
        store
            .transaction_with(|tx| {
                let row =
                    studio_runtime_store::TerminalsRepository::get(tx, "shell")?.expect("terminal");
                studio_runtime_store::TerminalsRepository::put(tx, "shell", &row.record, "new", 0)?;
                studio_runtime_store::TerminalsRepository::put(
                    tx,
                    "shell",
                    &row.record,
                    &("new".to_owned() + &"!".repeat(41)),
                    0,
                )?;
                Ok::<_, StoreError>(())
            })
            .await
            .expect("reset and grow");
        let recovered = initial_snapshot(state.clone(), resource.clone(), old.cursor)
            .await
            .expect("reset resume");
        let recovered = serde_json::to_value(recovered).expect("recovered frame");
        assert_eq!(recovered["replyKind"], "snapshot");
        assert_eq!(recovered["cursor"]["terminalGeneration"], 1);
        assert_eq!(
            recovered["viewSnapshot"]["terminalOutput"]["text"],
            "new".to_owned() + &"!".repeat(41)
        );
        let legacy = initial_snapshot(
            state.clone(),
            resource.clone(),
            Some(SubscriptionCursor {
                workspace_id: state.workspace_id.clone(),
                seq: 40,
                terminal_generation: None,
            }),
        )
        .await
        .expect("legacy resume");
        assert_eq!(
            serde_json::to_value(legacy).expect("legacy frame")["replyKind"],
            "snapshot"
        );
        let oversized =
            "x".repeat(studio_runtime_schema::shared_values::SYNC_TERMINAL_STREAM_MAX_CHARS + 1);
        store
            .transaction_with(move |tx| {
                tx.execute(
                    "UPDATE user_terminals SET output=?1 WHERE id='shell'",
                    [oversized],
                )?;
                Ok::<_, StoreError>(())
            })
            .await
            .expect("large append");
        let bounded = initial_snapshot(state, resource, None)
            .await
            .expect("bounded output");
        let bounded = serde_json::to_value(bounded).expect("bounded frame");
        assert_eq!(
            bounded["viewSnapshot"]["terminalOutput"]["text"]
                .as_str()
                .expect("text")
                .chars()
                .count(),
            studio_runtime_schema::shared_values::SYNC_TERMINAL_STREAM_MAX_CHARS
        );
        assert_eq!(bounded["viewSnapshot"]["terminalOutput"]["truncated"], true);
    }

    #[test]
    fn every_generated_resource_has_exactly_one_handler_and_data_contract() {
        let document: Value =
            serde_json::from_str(include_str!("../../openapi.json")).expect("OpenAPI owner");
        let schemas = &document["components"]["schemas"];
        let kinds = schemas["ResourceRef"]["discriminator"]["mapping"]
            .as_object()
            .expect("resource kinds");
        let data = schemas["SubscriptionDataMap"]["properties"]
            .as_object()
            .expect("generated data mapping");
        assert_eq!(
            kinds.keys().collect::<BTreeSet<_>>(),
            data.keys().collect(),
            "OpenAPI owns resource data kinds"
        );
        let mut used = BTreeSet::new();
        for (kind, path) in kinds {
            let name = path
                .as_str()
                .expect("schema path")
                .rsplit('/')
                .next()
                .expect("name");
            let properties = schemas[name]["properties"]
                .as_object()
                .expect("selector fields");
            let mut value = serde_json::Map::new();
            for (field, schema) in properties {
                let schema = schema
                    .get("$ref")
                    .and_then(Value::as_str)
                    .and_then(|path| document.pointer(path.trim_start_matches('#')))
                    .unwrap_or(schema);
                let field_value = schema
                    .get("const")
                    .cloned()
                    .or_else(|| {
                        schema
                            .get("enum")
                            .and_then(|items| items.as_array())
                            .and_then(|items| items.first())
                            .cloned()
                    })
                    .unwrap_or_else(|| {
                        if schema["type"] == "integer" {
                            schema.get("minimum").cloned().unwrap_or(json!(1))
                        } else if schema["type"] == "boolean" {
                            json!(false)
                        } else {
                            json!("test")
                        }
                    });
                value.insert(field.clone(), field_value);
            }
            value.insert("kind".into(), json!(kind));
            for optional_fields in [false, true] {
                let mut sample = value.clone();
                if !optional_fields {
                    let required = schemas[name]["required"]
                        .as_array()
                        .expect("required fields");
                    sample.retain(|field, _| required.iter().any(|item| item == field));
                }
                let original = Value::Object(sample);
                assert!(
                    studio_runtime_schema::validate("ResourceRef", &original).expect("schema"),
                    "{kind}"
                );
                let typed: ResourceRef =
                    serde_json::from_value(original.clone()).expect("generated reference");
                use studio_runtime_services::resource_notify::ResourceIdentity;
                let selector = crate::routes::sync_stream::SubscribedResource::new(typed.clone())
                    .expect("resource identity");
                assert_eq!(
                    selector.notification_kind().expect("typed role"),
                    original.notification_kind().expect("wire role"),
                    "notification role for {kind}"
                );
                assert_eq!(
                    serde_json::to_value(typed).expect("serialized reference"),
                    original,
                    "wire identity for {kind}"
                );
            }
            let resource: ResourceRef =
                serde_json::from_value(Value::Object(value)).expect("generated reference");
            let matches: Vec<_> = REGISTRY
                .iter()
                .enumerate()
                .filter(|(_, handler)| handler.accepts(&resource))
                .collect();
            assert_eq!(matches.len(), 1, "exactly one registry handler for {kind}");
            used.insert(matches[0].0);
            assert!(matches[0].1.max_bytes() <= SYNC_STREAM_EVENT_MAX_BYTES);
            assert!(matches[0].1.matches_change(&resource, &resource));
            assert!(!matches[0].1.debug_key(&resource).is_empty());
        }
        assert_eq!(
            used.len(),
            REGISTRY.len(),
            "registry contains no extra kinds"
        );
    }
}
