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
    super::sync_stream::transcript_initial_snapshot(state, resource, since).await
}

async fn entity_initial(
    state: HttpState,
    resource: ResourceRef,
    since: Option<SubscriptionCursor>,
) -> Result<ResourceRevisionEntry, ApiError> {
    super::subscription_entities::initial_snapshot(state, resource, since).await
}

// This is the only resource-kind registration list. The parity witness compares
// its complete set against the generated OpenAPI ResourceRef discriminator.
resource_handlers! {
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
    RoomResourceHandler => RoomResource, passive_initial;
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
    handler.initial_snapshot(state, resource, since).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeSet;

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
