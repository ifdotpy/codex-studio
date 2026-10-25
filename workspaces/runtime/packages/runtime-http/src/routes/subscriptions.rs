//! Resource-kind ownership for screen subscriptions. Wire transports dispatch
//! here; domain services publish changes without session or subscriber knowledge.

use futures_util::future::BoxFuture;
use studio_runtime_store::SyncDocumentsRepository;

use crate::error::ApiError;
use crate::generated::{ResourceRef, ResourceRevisionEntry, SubscriptionCursor};
use crate::router::HttpState;
use serde_json::Value;
use studio_runtime_schema::shared_values::SYNC_STREAM_EVENT_MAX_BYTES;

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
        _ => return Err(ApiError::internal()),
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
            | ResourceRef::WorkResource(_)
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
        ResourceRef::WorkResource(r) => {
            Ok(json!({"work":super::work_items::work_for_agent(state, r.agent_id).await?}))
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
                return Ok(json!({"legacyMessages":messages}));
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
                messages.remove(0);
                let first = messages[0]["seq"].clone();
                room["nextBefore"] = first;
            }
            Ok(json!({"room":room}))
        }
        ResourceRef::TerminalOutputResource(r) => {
            let offset = since
                .filter(|c| c.workspace_id == state.workspace_id)
                .map_or(0, |c| c.seq);
            let store = state.store.ok_or_else(ApiError::internal)?;
            let mut output =
                studio_runtime_services::terminal_driver::output(&store, &r.terminal_id, offset)
                    .await
                    .map_err(super::terminals::terminal_error)?;
            if offset > output["offset"].as_i64().unwrap_or_default() {
                output =
                    studio_runtime_services::terminal_driver::output(&store, &r.terminal_id, 0)
                        .await
                        .map_err(super::terminals::terminal_error)?;
            }
            // Worst-case JSON escaping is six bytes per scalar. This bound leaves
            // room for resource metadata and the event envelope.
            let max_chars = studio_runtime_schema::shared_values::SYNC_TERMINAL_STREAM_MAX_CHARS;
            let text = output["text"].as_str().ok_or_else(ApiError::internal)?;
            let count = text.chars().count();
            if count > max_chars {
                output["text"] = json!(text.chars().skip(count - max_chars).collect::<String>());
                output["truncated"] = json!(true);
            }
            Ok(json!({"terminalOutput":output}))
        }
        _ => Err(ApiError::internal()),
    }
}

async fn view_initial(
    state: HttpState,
    resource: ResourceRef,
    since: Option<SubscriptionCursor>,
) -> Result<ResourceRevisionEntry, ApiError> {
    let snapshot = view_value(state.clone(), resource.clone(), since.clone()).await?;
    let mut value = serde_json::json!({"revision":0,"resource":resource,"viewSnapshot":snapshot});
    if let Some(output) = snapshot.get("terminalOutput") {
        let offset = output["offset"].as_i64().ok_or_else(ApiError::internal)?;
        value["revision"] = serde_json::json!(offset);
        value["cursor"] = serde_json::json!({"workspaceId":state.workspace_id,"seq":offset});
        value["replyKind"] = serde_json::json!(if since
            .is_some_and(|c| c.workspace_id == state.workspace_id && c.seq <= offset)
            && output["truncated"] == false
        {
            "delta"
        } else {
            "snapshot"
        });
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
    WorkResourceHandler => WorkResource, view_initial;
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
    WorkspaceResourceHandler => WorkspaceResource, passive_initial;
    WorkResourceHandler => WorkResource, entity_initial;
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

pub(super) async fn resource_content(state: HttpState, resource: Value) -> Option<Vec<u8>> {
    let resource: ResourceRef = serde_json::from_value(resource).ok()?;
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
    if is_view_resource(&resource) {
        return view_value(state, resource, None)
            .await
            .ok()
            .and_then(|value| serde_json::to_vec(&value).ok());
    }
    let reference = serde_json::to_value(&resource).ok()?;
    if reference["kind"].as_str().is_some_and(|kind| {
        studio_runtime_schema::sync_protocol::subscription_entity_scopes().contains_key(kind)
    }) {
        return super::subscription_entities::content_fingerprint(state, resource).await;
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
                let receipt = studio_runtime_store::EventWriteRepository::receipt(
                    connection,
                    &resource.request_id,
                )?;
                let value = match receipt {
                    Some(receipt) => serde_json::from_str::<Value>(&receipt.result)?,
                    None => Value::Null,
                };
                let value = if value["id"] == resource.agent_id
                    && value["request_id"] == resource.request_id
                {
                    value
                } else {
                    Value::Null
                };
                serde_json::to_vec(&value).map_err(studio_runtime_store::StoreError::from)
            })
            .await
            .ok(),
        ResourceRef::AuthProgressResource(resource) => serde_json::to_vec(
            &state
                .claude_login?
                .progress(&resource.request_id)
                .await
                .ok()?,
        )
        .ok(),
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
                let cursor = studio_runtime_store::VoiceRepository::record_sequence(
                    connection,
                    &resource.agent_id,
                )?;
                let session = studio_runtime_store::VoiceRepository::latest_session(
                    connection,
                    &resource.agent_id,
                )?;
                serde_json::to_vec(&(cursor, session))
                    .map_err(studio_runtime_store::StoreError::from)
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
        _ => None,
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
    async fn subscribed_views_carry_bounded_records_and_terminal_resume() {
        use std::sync::Arc;
        use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore, StoreError};
        let directory = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("store"),
        );
        store.transaction_with(|tx| {
            AgentRepository::put(tx, &AgentRecord::from_value(json!({"id":"lead","rootId":"lead","isLead":true,"epoch":1,"status":"idle","name":"Lead","threadId":"thread"}))?)?;
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
            (json!({"kind":"work","agentId":"lead"}), "work"),
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
        let resource: ResourceRef =
            serde_json::from_value(json!({"kind":"terminal-output","terminalId":"shell"}))
                .expect("terminal ref");
        let cursor = SubscriptionCursor {
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
        let future = initial_snapshot(
            state.clone(),
            resource.clone(),
            Some(SubscriptionCursor {
                workspace_id: state.workspace_id.clone(),
                seq: 99,
            }),
        )
        .await
        .expect("future cursor snapshot");
        let future = serde_json::to_value(future).expect("future frame");
        assert_eq!(future["replyKind"], "snapshot");
        assert_eq!(future["viewSnapshot"]["terminalOutput"]["text"], "a🙂中");
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
