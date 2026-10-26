//! Private pre-App migration coordinator. No renderer supplies source records,
//! profile paths, owner inventories, or completion evidence.
use crate::{
    archive_import, legacy_capture, native_origin_inventory, profile, service_context::Context,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};
use studio_client_core::{
    backend::Backend, client::Client, protocol::Request, server_transport::RemoteClient,
};
use tauri::{Emitter, Manager, WebviewWindow};

#[derive(Default)]
pub(crate) struct MigrationHost {
    serial: tokio::sync::Mutex<()>,
    held: tokio::sync::Mutex<BTreeMap<String, Context>>,
    admitted: tokio::sync::Mutex<BTreeMap<String, Arc<Context>>>,
    completed: tokio::sync::Mutex<Option<Value>>,
}

#[derive(Clone)]
struct Binding {
    server: String,
    storage: String,
    source: String,
    directory: PathBuf,
    backend: Backend,
}

struct OriginalSources {
    sources: BTreeMap<String, Value>,
}

type EvidenceFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

// Private native evidence. There is no renderer constructor or source selector.
trait SourceEvidence: Send + Sync {
    fn capture(&self) -> &legacy_capture::CapturedSource;
    fn origin_digest(&self) -> &str;
    fn recapture(&self) -> EvidenceFuture<'_, legacy_capture::CapturedSource>;
    fn authority(&self) -> EvidenceFuture<'_, (Vec<Binding>, Value)>;
    fn verify_origin(&self, after: &legacy_capture::CapturedSource) -> EvidenceFuture<'_, ()>;
    fn events(&self) -> Arc<dyn Fn(Value) + Send + Sync>;
}

struct NativeSourceEvidence {
    window: WebviewWindow,
    capture: legacy_capture::CapturedSource,
    origins: Arc<native_origin_inventory::OriginInventory>,
    selection: native_origin_inventory::TrustedStoreSelection,
    origin_digest: String,
    source_identity: String,
    profile: PathBuf,
    local: Backend,
    remote: Option<Arc<RemoteClient>>,
}

impl SourceEvidence for NativeSourceEvidence {
    fn capture(&self) -> &legacy_capture::CapturedSource {
        &self.capture
    }
    fn origin_digest(&self) -> &str {
        &self.origin_digest
    }
    fn recapture(&self) -> EvidenceFuture<'_, legacy_capture::CapturedSource> {
        Box::pin(legacy_capture::main_source(
            &self.window,
            Some(&self.capture.document_token),
        ))
    }
    fn authority(&self) -> EvidenceFuture<'_, (Vec<Binding>, Value)> {
        Box::pin(async move {
            Ok((
                bindings(&self.profile, self.local.clone(), self.remote.as_ref()).await?,
                registry_revision(self.remote.as_ref())?,
            ))
        })
    }
    fn verify_origin(&self, after: &legacy_capture::CapturedSource) -> EvidenceFuture<'_, ()> {
        let origins = self.origins.clone();
        let selection = self.selection.clone();
        let fence = native_origin_inventory::SourceFence {
            source_identity: self.source_identity.clone(),
            document_token: after.document_token.clone(),
        };
        Box::pin(async move {
            tokio::task::spawn_blocking(move || origins.require_same_source(&selection, &fence))
                .await
                .map_err(|_| "The private origin recheck failed.")?
                .map_err(|error| error.to_string())
        })
    }
    fn events(&self) -> Arc<dyn Fn(Value) + Send + Sync> {
        let events = self.window.clone();
        Arc::new(move |event| {
            let _ = events.emit("studio-client-event", event);
        })
    }
}

fn hash(value: &impl serde::Serialize) -> Result<String, String> {
    let bytes =
        serde_json::to_vec(value).map_err(|_| "The private migration evidence is invalid.")?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn scope(server: &str) -> String {
    if server == "local" {
        "root".into()
    } else {
        format!("server:{server}")
    }
}

/// Use this same owner for the initial host token and the final native binding.
pub(crate) fn source_identity(
    profile: &Path,
    server: &str,
    storage: &str,
) -> Result<String, String> {
    let archive = profile::optional_seal_identity(&scope(server))?;
    hash(&(profile, server, storage, &archive["id"], &archive["sha256"]))
}

fn empty_original() -> Value {
    json!({"owners":["root"],"preferences":{"root":{"version":1,"legacyPreferenceScope":"root","preferences":{},"preferenceReceipts":[]}},"receipts":[],"nativeLegacy":{"version":1,"drafts":[],"receiptIds":[],"sourceSha256":hash(&json!({"drafts":[],"receipts":[]})).expect("JSON evidence"),"preferenceReceipts":[]}})
}

impl OriginalSources {
    fn read(bindings: &[Binding]) -> Result<Self, String> {
        let mut sources = BTreeMap::new();
        for binding in bindings {
            let source = studio_client_core::store::read_original_migration_source(
                &binding.directory.join("studio-client.sqlite3"),
            )?
            .unwrap_or_else(empty_original);
            sources.insert(binding.server.clone(), source);
        }
        Ok(Self { sources })
    }

    fn digest(&self) -> Result<String, String> {
        // Schema additions and the native identity cache are not legacy input.
        // The read-only source owner validates their layouts before this point.
        hash(&self.sources.iter().map(|(server, source)| (server, json!({
            "owners":source["owners"],"preferences":source["preferences"],"nativeLegacy":source["nativeLegacy"]
        }))).collect::<BTreeMap<_,_>>())
    }

    fn owners(&self) -> Result<BTreeSet<String>, String> {
        let mut owners = BTreeSet::new();
        for source in self.sources.values() {
            for owner in source["owners"]
                .as_array()
                .ok_or("The original native owner inventory is incomplete.")?
            {
                let owner = owner
                    .as_str()
                    .ok_or("The original native owner is invalid.")?;
                let server = if owner == "root" {
                    "local"
                } else {
                    owner
                        .strip_prefix("server:")
                        .filter(|id| !id.is_empty())
                        .ok_or("The original native namespace has no owner.")?
                };
                owners.insert(server.to_owned());
            }
        }
        Ok(owners)
    }

    fn preferences(&self, server: &str) -> Result<Value, String> {
        let scope = scope(server);
        let mut preferences = BTreeMap::<String, Value>::new();
        let mut receipts = BTreeMap::<String, Value>::new();
        for source in self.sources.values() {
            let Some(saved) = source["preferences"].get(&scope) else {
                continue;
            };
            if saved["version"] != 1 || saved["legacyPreferenceScope"] != scope {
                return Err("The original native preference scope changed.".into());
            }
            for (key, value) in saved["preferences"]
                .as_object()
                .ok_or("The original native preferences are incomplete.")?
            {
                if preferences.get(key).is_some_and(|old| old != value) {
                    return Err(
                        "The original native preference owners disagree. Both sources remain held."
                            .into(),
                    );
                }
                preferences.insert(key.clone(), value.clone());
            }
            for receipt in saved["preferenceReceipts"]
                .as_array()
                .ok_or("The original native receipts are incomplete.")?
            {
                let id = receipt["id"]
                    .as_str()
                    .ok_or("The original native receipt has no identity.")?;
                if receipts.get(id).is_some_and(|old| old != receipt) {
                    return Err(
                        "The original native receipt identity has conflicting content.".into(),
                    );
                }
                receipts.insert(id.into(), receipt.clone());
            }
        }
        Ok(
            json!({"version":1,"legacyPreferenceScope":scope,"preferences":preferences,"preferenceReceipts":receipts.into_values().collect::<Vec<_>>()}),
        )
    }
}

async fn call(client: &Client, method: &str, params: Value) -> Result<Value, String> {
    let response = client
        .request(Request {
            id: studio_client_core::server_keys::random_id()?,
            method: method.into(),
            params,
        })
        .await?;
    response.result.ok_or_else(|| {
        response
            .error
            .map(|error| error.message)
            .unwrap_or("The private migration response is missing.".into())
    })
}

fn policy_method(key: &str) -> Result<&'static str, String> {
    profile::migration_policy()[key]
        .as_str()
        .ok_or_else(|| "The native migration method contract is incomplete.".into())
}

async fn bindings(
    profile: &Path,
    local: Backend,
    remote: Option<&Arc<RemoteClient>>,
) -> Result<Vec<Binding>, String> {
    local.validate_identity()?;
    let mut bindings = vec![Binding {
        server: "local".into(),
        storage: "local".into(),
        source: source_identity(profile, "local", "local")?,
        directory: profile.join("client/local"),
        backend: local,
    }];
    if let Some(remote) = remote {
        for (server, binding) in remote.migration_bindings()? {
            if server == "local" {
                return Err("A paired server cannot replace the local migration owner.".into());
            }
            let storage = binding.storage_key();
            bindings.push(Binding {
                source: source_identity(profile, &server, &storage)?,
                directory: profile.join("client/servers").join(&storage),
                server,
                storage,
                backend: Backend::from_remote(binding)?,
            });
        }
    }
    Ok(bindings)
}

fn paired_storage(bindings: &[Binding]) -> BTreeMap<String, String> {
    bindings
        .iter()
        .filter(|binding| binding.server != "local")
        .map(|binding| (binding.server.clone(), binding.storage.clone()))
        .collect()
}

fn binding_inventory(bindings: &[Binding]) -> Result<String, String> {
    hash(&bindings.iter().map(|binding|json!({"serverId":binding.server,"storage":binding.storage,"sourceIdentity":binding.source,"transport":binding.backend.public_identity()})).collect::<Vec<_>>())
}

fn registry_revision(remote: Option<&Arc<RemoteClient>>) -> Result<Value, String> {
    remote
        .map(|client| {
            client
                .registry_snapshot()
                .map(|snapshot| snapshot["revision"].clone())
        })
        .transpose()
        .map(|revision| revision.unwrap_or(Value::Null))
}

fn require_owner_coverage(
    bindings: &[Binding],
    capture: &legacy_capture::CapturedSource,
    originals: &OriginalSources,
) -> Result<(), String> {
    let known = bindings
        .iter()
        .map(|binding| binding.server.clone())
        .collect::<BTreeSet<_>>();
    let mut retained = archive_import::current_owners(capture)?;
    retained.extend(originals.owners()?);
    for owner in profile::configured_archive_owners()? {
        retained.insert(if owner == "root" {
            "local".into()
        } else {
            owner
                .strip_prefix("server:")
                .ok_or("The archive has an unknown native owner.")?
                .into()
        });
    }
    if !retained.is_subset(&known) {
        return Err(
            "A retained source has no current native server authority. Its source remains held."
                .into(),
        );
    }
    Ok(())
}

fn pending_preflight(
    bindings: &[Binding],
    capture: &legacy_capture::CapturedSource,
    originals: &OriginalSources,
) -> Result<BTreeMap<String, Vec<Value>>, String> {
    let existing = originals
        .sources
        .get("local")
        .ok_or("The original preference receipt owner is missing.")?["receipts"]
        .as_array()
        .ok_or("The original preference receipt inventory is incomplete.")?;
    let mut pending = BTreeMap::new();
    let mut identities = BTreeMap::new();
    for binding in bindings {
        let writes = archive_import::pending_preferences(capture, &binding.server)?;
        for request in studio_client_core::store::prepare_original_preferences(
            &scope(&binding.server),
            &json!(writes),
        )? {
            let params = serde_json::to_string(&request.params)
                .map_err(|_| "The original preference parameters cannot be encoded.")?;
            let identity = (request.method.clone(), params.clone());
            if identities
                .get(&request.id)
                .is_some_and(|old| old != &identity)
            {
                return Err(
                    "A pending preference identity conflicts across original owners.".into(),
                );
            }
            identities.insert(request.id.clone(), identity);
            if let Some(receipt) = existing.iter().find(|receipt| receipt["id"] == request.id) {
                if receipt["method"] != request.method || receipt["serializedParams"] != params {
                    return Err(
                        "A pending preference identity conflicts with its original receipt.".into(),
                    );
                }
                let response: studio_client_core::protocol::Response = serde_json::from_str(
                    receipt["serializedResponse"]
                        .as_str()
                        .ok_or("The original preference receipt response is incomplete.")?,
                )
                .map_err(|_| "The original preference receipt response is invalid.")?;
                if response.id != request.id
                    || response.error.is_some()
                    || response.result.is_none()
                {
                    return Err("The original preference receipt refuses recovery.".into());
                }
            }
        }
        pending.insert(binding.server.clone(), writes);
    }
    Ok(pending)
}

fn require_origin_coverage(
    inventory: &native_origin_inventory::OriginInventory,
    capture: &legacy_capture::CapturedSource,
) -> Result<String, String> {
    use native_origin_inventory::{Origin, StorageKind};
    let parse = |value: &str| -> Result<Origin, String> {
        let url = tauri::Url::parse(value).map_err(|_| "The private source origin is invalid.")?;
        Ok(Origin {
            scheme: url.scheme().into(),
            host: url
                .host_str()
                .ok_or("The private source origin has no host.")?
                .into(),
            port: url.port(),
        })
    };
    let top = parse(&capture.top_origin)?;
    let client = parse(&capture.origin)?;
    if inventory
        .origins
        .iter()
        .any(|origin| origin.top != top || origin.client != client)
        || inventory.storage.iter().any(|entry| {
            entry.origin.top != top
                || entry.origin.client != client
                || entry
                    .kinds
                    .iter()
                    .any(|kind| !matches!(kind, StorageKind::LocalStorage | StorageKind::IndexedDb))
        })
    {
        return Err("A retained browser partition or storage kind has no complete private export. Its source remains held.".into());
    }
    hash(
        &json!({"format":inventory.format,"origins":inventory.origins.iter().map(|origin|format!("{origin:?}")).collect::<Vec<_>>(),"storage":inventory.storage.iter().map(|entry|format!("{:?}:{:?}",entry.origin,entry.kinds)).collect::<Vec<_>>()}),
    )
}

fn capture_digest(
    capture: &legacy_capture::CapturedSource,
    originals: &OriginalSources,
    archive: &str,
) -> Result<String, String> {
    hash(&json!({"browser":capture.digest,"native":originals.digest()?,"archive":archive}))
}

async fn open_targets(
    bindings: &[Binding],
    workspaces: &BTreeMap<String, String>,
    event: Arc<dyn Fn(Value) + Send + Sync>,
) -> Result<BTreeMap<String, Context>, String> {
    let mut contexts = BTreeMap::new();
    for binding in bindings {
        binding.backend.validate_identity()?;
        let client = Arc::new(Client::open_for_migration(
            binding.directory.clone(),
            binding.backend.clone(),
            {
                let event = event.clone();
                move |value| event(value)
            },
        )?);
        // This handshake is supplied by the private core connection owner. A
        // normal Client or an older runtime cannot be used as a paused target.
        if call(&client, policy_method("runtimeStateMethod")?, json!({})).await?
            != json!({"held":true})
        {
            return Err("The native migration runtime is not held before user effects.".into());
        }
        call(&client,"identity.set",json!({"workspaceId":workspaces.get(&binding.server).ok_or("The native migration workspace is missing.")?})).await?;
        contexts.insert(
            binding.server.clone(),
            Context {
                server_id: binding.server.clone(),
                workspace_id: workspaces
                    .get(&binding.server)
                    .ok_or("The native migration workspace is missing.")?
                    .clone(),
                client,
                backend: Some(binding.backend.clone()),
                source_identity: binding.source.clone(),
                migration_complete: Default::default(),
                archive_hold: true,
            },
        );
    }
    Ok(contexts)
}

fn owner_inventory(
    binding: &Binding,
    workspace: &str,
    plan: &archive_import::ValidatedSource,
) -> Result<String, String> {
    hash(
        &json!({"serverId":binding.server,"workspaceId":workspace,"binding":binding.backend.public_identity(),"sourceIdentity":binding.source,"plan":plan.inventory()}),
    )
}

fn validate_receipt(
    context: &Context,
    receipt: &Value,
    source: &str,
    owner: &str,
) -> Result<(), String> {
    if receipt["complete"] != true
        || receipt["hostVerified"] != true
        || receipt["proofVersion"] != profile::migration_policy()["proofVersion"]
        || receipt["serverId"] != context.server_id
        || receipt["sourceIdentity"] != context.source_identity
        || receipt["sourceDigest"] != source
        || receipt["ownerInventoryDigest"] != owner
        || !receipt["inventoryDigest"]
            .as_str()
            .is_some_and(profile::valid_sha256)
    {
        return Err(
            "The durable native migration receipt does not match its private source and owner."
                .into(),
        );
    }
    Ok(())
}

async fn prepare_owner(
    binding: &Binding,
    context: &Context,
    source: archive_import::ValidatedSource,
    source_digest: &str,
    native_snapshot: &Value,
) -> Result<Value, String> {
    let owner_digest = owner_inventory(binding, &context.workspace_id, &source)?;
    let previous = call(&context.client,policy_method("statusMethod")?,json!({"sourceIdentity":context.source_identity,"serverId":context.server_id,"sourceDigest":source_digest})).await?;
    if previous["complete"] == true {
        validate_receipt(context, &previous, source_digest, &owner_digest)?;
        return Ok(previous);
    }
    if previous["held"] == true {
        return Err("The original native source changed after its durable cutover. Its current data remains held.".into());
    }
    if previous["held"] != false {
        return Err(
            "The native runtime cannot distinguish an unstarted import from a changed baseline."
                .into(),
        );
    }
    let native = call(&context.client,policy_method("nativeImportMethod")?,json!({"sourceIdentity":context.source_identity,"expectedSourceSha256":native_snapshot["sourceSha256"]})).await?;
    let mut prepared = source.execute(context).await?;
    prepared["metadata"]
        .as_array_mut()
        .ok_or("The imported metadata receipt inventory is incomplete.")?
        .push(json!({"manifestId":native["manifestId"],"sourceSha256":native["sourceSha256"]}));
    Ok(json!({"prepared":prepared,"ownerDigest":owner_digest}))
}

async fn publish_owner(
    context: &Context,
    saved: Value,
    source_digest: &str,
    origin_digest: &str,
) -> Result<Value, String> {
    if saved["complete"] == true {
        return Ok(saved);
    }
    let prepared = &saved["prepared"];
    let receipt = call(&context.client,policy_method("finishMethod")?,json!({"sourceIdentity":context.source_identity,"serverId":context.server_id,"metadata":prepared["metadata"],"bytes":prepared["bytes"],"archive":prepared["archive"],"capture":{"proofVersion":profile::migration_policy()["proofVersion"],"beforeDigest":source_digest,"afterDigest":source_digest,"originInventoryDigest":origin_digest,"ownerInventoryDigest":saved["ownerDigest"],"allOriginsVerified":true}})).await?;
    validate_receipt(
        context,
        &receipt,
        source_digest,
        saved["ownerDigest"]
            .as_str()
            .ok_or("The private owner digest is missing.")?,
    )?;
    Ok(receipt)
}

async fn admit_owner(
    context: &mut Context,
    receipt: &Value,
    source_digest: &str,
) -> Result<(), String> {
    if call(
        &context.client,
        policy_method("admitMethod")?,
        json!({"receipt":receipt}),
    )
    .await?
        != json!({"admitted":true})
    {
        return Err("The native runtime did not admit its exact durable migration receipt.".into());
    }
    context.confirm_host_capture(receipt, source_digest)
}

impl MigrationHost {
    /// The root host holds ProfileLock and the pre-App/legacy-write fences for
    /// this entire call. The selected server affects only the returned receipt.
    pub(crate) async fn migrate(
        &self,
        window: &WebviewWindow,
        profile: &Path,
        local: Backend,
        remote: Option<&Arc<RemoteClient>>,
        selected: &str,
        expected_source: &str,
    ) -> Result<Value, String> {
        let _serial = self.serial.lock().await;
        crate::trusted(window)?;
        let state = window
            .app_handle()
            .try_state::<crate::Clients>()
            .ok_or("The native migration host state is missing.")?;
        let legacy_fence = state.legacy_write_admission.lock().await;
        if !*legacy_fence {
            return Err(
                "Close legacy write admission before the private migration capture.".into(),
            );
        }
        let profile = profile
            .canonicalize()
            .map_err(|_| "The locked native profile is unavailable.")?;
        let locked = window
            .app_handle()
            .try_state::<crate::ProfileLock>()
            .ok_or("The native migration profile lock is missing.")?;
        if locked.directory != profile || window.app_handle().webview_windows().len() != 1 {
            return Err(
                "The native migration requires its exact locked profile and one source view."
                    .into(),
            );
        }
        let registry_revision = self::registry_revision(remote)?;
        let bindings = bindings(&profile, local.clone(), remote).await?;
        if self::registry_revision(remote)? != registry_revision {
            return Err("The native server authority changed during inventory.".into());
        }
        let selected_binding = bindings
            .iter()
            .find(|binding| binding.server == selected)
            .ok_or("The selected migration owner is unavailable.")?;
        if !profile::valid_sha256(expected_source) || selected_binding.source != expected_source {
            return Err("The validated native cutover source changed before capture.".into());
        }
        let capture = legacy_capture::main_source(window, None).await?;
        let selection =
            legacy_capture::store_selection(window, &selected_binding.source, &capture).await?;
        let scan_selection = selection.clone();
        let origins =
            tokio::task::spawn_blocking(move || native_origin_inventory::inspect(&scan_selection))
                .await
                .map_err(|_| "The private origin inventory failed.")?
                .map_err(|error| error.to_string())?;
        let origin_digest = require_origin_coverage(&origins, &capture)?;
        let evidence = NativeSourceEvidence {
            window: window.clone(),
            capture,
            origins: Arc::new(origins),
            selection,
            origin_digest,
            source_identity: selected_binding.source.clone(),
            profile: profile.clone(),
            local,
            remote: remote.cloned(),
        };
        self.coordinate(&profile, bindings, registry_revision, selected, evidence)
            .await
    }

    async fn coordinate(
        &self,
        profile: &Path,
        bindings: Vec<Binding>,
        registry_revision: Value,
        selected: &str,
        evidence: impl SourceEvidence,
    ) -> Result<Value, String> {
        let binding_digest = binding_inventory(&bindings)?;
        let capture = evidence.capture();
        let origin_digest = evidence.origin_digest().to_owned();
        let retained = profile::retained_native_profiles(profile, &paired_storage(&bindings))?;
        let originals = OriginalSources::read(&bindings)?;
        require_owner_coverage(&bindings, capture, &originals)?;
        let pending = pending_preflight(&bindings, capture, &originals)?;
        let archive = profile::archive_fingerprint()?;
        let mut workspaces = BTreeMap::new();
        let mut owner_digests = BTreeMap::new();
        for binding in &bindings {
            let identity = studio_client_core::sync::identity(&binding.backend)
                .await
                .map_err(|error| error.message)?;
            let workspace = identity["workspaceId"]
                .as_str()
                .ok_or("The native migration workspace is missing.")?
                .to_owned();
            let original_identity = &originals.sources[&binding.server]["identity"];
            if !original_identity.is_null() && original_identity["workspaceId"] != workspace {
                return Err(
                    "The original native workspace differs from its current server authority."
                        .into(),
                );
            }
            workspaces.insert(binding.server.clone(), workspace.clone());
            // Preflight every payload before a writable Client is opened.
            // These IDs describe planned original-owner replay, not receipts.
            let planned_ids = pending[&binding.server]
                .iter()
                .map(|write| {
                    write["id"]
                        .as_str()
                        .map(str::to_owned)
                        .ok_or("The original pending preference has no identity.")
                })
                .collect::<Result<BTreeSet<_>, _>>()?;
            let plan = archive_import::validated_source(
                &archive_import::SourceOwner {
                    server_id: binding.server.clone(),
                    workspace_id: workspace.clone(),
                    source_identity: binding.source.clone(),
                },
                &archive_import::owner_capture(capture, &binding.server)?,
                &originals.preferences(&binding.server)?,
                profile::optional_sealed(&scope(&binding.server))?.as_ref(),
                &planned_ids,
            )?;
            owner_digests.insert(
                binding.server.clone(),
                owner_inventory(binding, &workspace, &plan)?,
            );
        }
        let authority_digest = hash(
            &json!({"revision":registry_revision,"owners":bindings.iter().map(|binding|json!({"serverId":binding.server,"sourceIdentity":binding.source,"workspaceId":workspaces[&binding.server],"transport":binding.backend.public_identity()})).collect::<Vec<_>>()}),
        )?;
        if let Some(previous) = self.completed.lock().await.as_ref() {
            if previous["sourceDigest"] != capture_digest(capture, &originals, &archive)?
                || previous["authorityDigest"] != authority_digest
            {
                return Err(
                    "The completed native source changed. Its current data remains held.".into(),
                );
            }
            let after = evidence.recapture().await?;
            let (current_bindings, current_revision) = evidence.authority().await?;
            if after.digest != capture.digest
                || profile::archive_fingerprint()? != archive
                || binding_inventory(&current_bindings)? != binding_digest
                || current_revision != registry_revision
                || capture_digest(&after, &OriginalSources::read(&bindings)?, &archive)?
                    != previous["sourceDigest"]
            {
                return Err("The completed native source changed during verification.".into());
            }
            evidence.verify_origin(&after).await?;
            return selected_result(previous, selected);
        }
        let mut held = self.held.lock().await;
        if held.is_empty() {
            *held = open_targets(&bindings, &workspaces, evidence.events()).await?;
        }
        if held.len() != bindings.len()
            || bindings.iter().any(|binding| {
                held.get(&binding.server).is_none_or(|context| {
                    context.source_identity != binding.source
                        || context.workspace_id != workspaces[&binding.server]
                })
            })
        {
            return Err(
                "The paused native owner set changed. Restart its private migration check.".into(),
            );
        }
        let initial_digest = capture_digest(capture, &originals, &archive)?;
        // Check every saved baseline before touching any original journal.
        // An absent capability is held, never treated as a fresh empty source.
        for binding in &bindings {
            let context = &held[&binding.server];
            let status=call(&context.client,policy_method("statusMethod")?,json!({"sourceIdentity":context.source_identity,"serverId":context.server_id,"sourceDigest":initial_digest})).await?;
            if status["complete"] == true {
                validate_receipt(
                    context,
                    &status,
                    &initial_digest,
                    &owner_digests[&binding.server],
                )?;
            } else if status["held"] != false {
                return Err("The original native source changed or its baseline status is unsupported. Its data remains held.".into());
            }
        }
        let original_receipts = originals.sources["local"]["nativeLegacy"]["preferenceReceipts"]
            .as_array()
            .ok_or("The genuine original preference receipt inventory is incomplete.")?
            .iter()
            .map(|receipt| {
                receipt["id"]
                    .as_str()
                    .map(str::to_owned)
                    .ok_or("The original preference receipt has no identity.")
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        // The former adapter always wrote its scoped journal on the original
        // local connection. Preserve those original IDs and receipt parameters.
        for binding in &bindings {
            let writes = pending[&binding.server]
                .iter()
                .filter(|write| {
                    !write["id"]
                        .as_str()
                        .is_some_and(|id| original_receipts.contains(id))
                })
                .cloned()
                .collect::<Vec<_>>();
            if writes.is_empty() {
                continue;
            }
            call(
                &held["local"].client,
                "client_storage.replay_preferences",
                json!({"scope":scope(&binding.server),"writes":writes}),
            )
            .await?;
        }
        let originals = OriginalSources::read(&bindings)?;
        require_owner_coverage(&bindings, capture, &originals)?;
        let before_digest = capture_digest(capture, &originals, &archive)?;
        let mut receipts = BTreeMap::new();
        for binding in &bindings {
            let context = &held[&binding.server];
            let confirmed = call(
                &held["local"].client,
                "client_storage.export_preferences",
                json!({"scope":scope(&binding.server)}),
            )
            .await?;
            let replayed = confirmed["preferenceReceipts"]
                .as_array()
                .ok_or("The original replay receipt inventory is incomplete.")?
                .iter()
                .map(|receipt| {
                    receipt["id"]
                        .as_str()
                        .map(str::to_owned)
                        .ok_or("The original replay receipt has no identity.")
                })
                .collect::<Result<BTreeSet<_>, _>>()?;
            let source = archive_import::validated_source(
                &archive_import::SourceOwner {
                    server_id: binding.server.clone(),
                    workspace_id: context.workspace_id.clone(),
                    source_identity: binding.source.clone(),
                },
                &archive_import::owner_capture(capture, &binding.server)?,
                &originals.preferences(&binding.server)?,
                profile::optional_sealed(&scope(&binding.server))?.as_ref(),
                &replayed,
            )?;
            let original = &originals.sources[&binding.server]["nativeLegacy"];
            receipts.insert(
                binding.server.clone(),
                prepare_owner(binding, context, source, &before_digest, original).await?,
            );
        }
        let after = evidence.recapture().await?;
        let (current_bindings, current_revision) = evidence.authority().await?;
        if after.digest != capture.digest
            || profile::archive_fingerprint()? != archive
            || binding_inventory(&current_bindings)? != binding_digest
            || current_revision != registry_revision
            || capture_digest(&after, &OriginalSources::read(&bindings)?, &archive)?
                != before_digest
        {
            return Err(
                "The original source changed during native import. Imported rows remain held."
                    .into(),
            );
        }
        evidence.verify_origin(&after).await?;
        let current_profiles =
            profile::retained_native_profiles(profile, &paired_storage(&bindings))?;
        if retained
            .evidence
            .as_object()
            .ok_or("The original native profile inventory is invalid.")?
            .iter()
            .any(|(key, value)| current_profiles.evidence.get(key) != Some(value))
        {
            return Err("An original native profile was replaced during import.".into());
        }
        // All byte and metadata readbacks exist before the first completion.
        for binding in &bindings {
            let context = &held[&binding.server];
            let receipt = publish_owner(
                context,
                receipts[&binding.server].clone(),
                &before_digest,
                &origin_digest,
            )
            .await?;
            receipts.insert(binding.server.clone(), receipt);
        }
        // Runtime admission is separate from receipt publication. No context is
        // exposed if any full-cohort receipt or admission check fails.
        for (server, context) in held.iter_mut() {
            let receipt = &receipts[server];
            admit_owner(context, receipt, &before_digest).await?;
        }
        let owners = held.iter().map(|(server,context)|json!({"serverId":server,"workspaceId":context.workspace_id,"sourceIdentity":context.source_identity,"receipt":receipts[server]})).collect::<Vec<_>>();
        let completed = json!({"complete":true,"hostVerified":true,"proofVersion":profile::migration_policy()["proofVersion"],"sourceDigest":before_digest,"authorityDigest":authority_digest,"owners":owners});
        self.admitted.lock().await.extend(
            std::mem::take(&mut *held)
                .into_iter()
                .map(|(server, context)| (server, Arc::new(context))),
        );
        *self.completed.lock().await = Some(completed.clone());
        selected_result(&completed, selected)
    }

    pub(crate) async fn context(&self, server: &str) -> Result<Arc<Context>, String> {
        let context = self
            .admitted
            .lock()
            .await
            .get(server)
            .cloned()
            .ok_or_else(|| {
                "The native owner has no complete private migration proof.".to_owned()
            })?;
        if let Some(backend) = &context.backend {
            backend.validate_identity()?;
        }
        Ok(context)
    }

    pub(crate) async fn verify(
        &self,
        window: &WebviewWindow,
        profile: &Path,
        local: Backend,
        remote: Option<&Arc<RemoteClient>>,
        server: &str,
        expected_source: &str,
        expected: &Value,
    ) -> Result<Value, String> {
        // Verification never creates an initial proof or opens an initial Store.
        if self.completed.lock().await.is_none() {
            return Err("The native host has no completed source capture to verify.".into());
        }
        let actual = self
            .migrate(window, profile, local, remote, server, expected_source)
            .await?;
        if expected
            .as_object()
            .is_some_and(|expected| expected.is_empty())
        {
            return Ok(actual);
        }
        for key in ["proofVersion", "sourceDigest", "inventoryDigest"] {
            if expected.get(key).is_none() || expected[key] != actual[key] {
                return Err("The requested native verification proof changed.".into());
            }
        }
        Ok(actual)
    }
}

fn selected_result(completed: &Value, server: &str) -> Result<Value, String> {
    let owner = completed["owners"]
        .as_array()
        .and_then(|owners| owners.iter().find(|owner| owner["serverId"] == server))
        .ok_or("The selected owner has no private migration receipt.")?;
    let mut result = owner["receipt"].clone();
    result["workspaceId"] = owner["workspaceId"].clone();
    result["owners"] = completed["owners"].clone();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct LoopbackIdentity {
        backend: Backend,
        requests: Arc<std::sync::atomic::AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for LoopbackIdentity {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    impl LoopbackIdentity {
        async fn start(workspace: Option<String>) -> Self {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let backend =
                Backend::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
            let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let observed = requests.clone();
            let task = tokio::spawn(async move {
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let Some(workspace) = &workspace else {
                        drop(socket);
                        continue;
                    };
                    // A bounded private HTTP fixture, never a model/provider.
                    const IDENTITY_FIXTURE_HEADER_MAX_BYTES: usize = 8192;
                    let mut header = vec![0u8; IDENTITY_FIXTURE_HEADER_MAX_BYTES];
                    let mut used = 0;
                    while used < header.len()
                        && !header[..used]
                            .windows(4)
                            .any(|window| window == b"\r\n\r\n")
                    {
                        let count = socket.read(&mut header[used..]).await.unwrap();
                        if count == 0 {
                            break;
                        }
                        used += count;
                    }
                    assert!(
                        String::from_utf8_lossy(&header[..used])
                            .starts_with("GET /api/sync/identity ")
                    );
                    let body = json!({"workspaceId":workspace}).to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                    socket.shutdown().await.unwrap();
                }
            });
            Self {
                backend,
                requests,
                task,
            }
        }
    }

    struct FixtureEvidence {
        capture: legacy_capture::CapturedSource,
        path: PathBuf,
        bindings: Vec<Binding>,
        origin_digest: String,
        recaptures: Arc<std::sync::atomic::AtomicUsize>,
    }
    fn fixture_capture(path: &Path) -> Result<legacy_capture::CapturedSource, String> {
        let packet: Value =
            serde_json::from_slice(&std::fs::read(path).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        if packet["frameCoverage"] != json!({"before":true,"after":true}) {
            return Err("The anonymous source frame proof is incomplete.".into());
        }
        let origin = packet["origin"]
            .as_str()
            .ok_or("The anonymous source origin is missing.")?
            .to_owned();
        let document_token = packet["documentToken"]
            .as_str()
            .ok_or("The anonymous private document token is missing.")?
            .to_owned();
        let mut snapshot = packet["snapshot"].clone();
        snapshot["origin"] = json!(origin);
        let digest = hash(&json!({"topOrigin":origin,"origin":origin,"snapshot":snapshot}))?;
        Ok(legacy_capture::CapturedSource {
            top_origin: origin.clone(),
            origin,
            document_token,
            snapshot,
            digest,
        })
    }
    impl FixtureEvidence {
        fn new(profile: &Path, bindings: Vec<Binding>, snapshot: Value) -> Self {
            let path = profile.join("anonymous-private-capture.json");
            std::fs::write(&path,json!({"origin":"tauri://localhost","documentToken":"anonymous-private-token","frameCoverage":{"before":true,"after":true},"snapshot":snapshot}).to_string()).unwrap();
            let capture = fixture_capture(&path).unwrap();
            Self {
                capture,
                path,
                bindings,
                origin_digest: "a".repeat(64),
                recaptures: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
    }
    impl SourceEvidence for FixtureEvidence {
        fn capture(&self) -> &legacy_capture::CapturedSource {
            &self.capture
        }
        fn origin_digest(&self) -> &str {
            &self.origin_digest
        }
        fn recapture(&self) -> EvidenceFuture<'_, legacy_capture::CapturedSource> {
            Box::pin(async move {
                self.recaptures
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                fixture_capture(&self.path)
            })
        }
        fn authority(&self) -> EvidenceFuture<'_, (Vec<Binding>, Value)> {
            Box::pin(async move { Ok((self.bindings.clone(), json!("0"))) })
        }
        fn verify_origin(&self, after: &legacy_capture::CapturedSource) -> EvidenceFuture<'_, ()> {
            let same = self.capture.top_origin == after.top_origin
                && self.capture.origin == after.origin
                && self.capture.document_token == after.document_token;
            Box::pin(async move {
                if same {
                    Ok(())
                } else {
                    Err("The anonymous source origin or document token changed.".into())
                }
            })
        }
        fn events(&self) -> Arc<dyn Fn(Value) + Send + Sync> {
            Arc::new(|_| {})
        }
    }

    fn fixture_binding(profile: &Path, server: &str, backend: Backend) -> Binding {
        let storage = if server == "local" {
            "local".into()
        } else {
            hash(&server).unwrap()
        };
        Binding {
            server: server.into(),
            directory: if server == "local" {
                profile.join("client/local")
            } else {
                profile.join("client/servers").join(&storage)
            },
            source: hash(&(server, &storage)).unwrap(),
            storage,
            backend,
        }
    }
    fn seed_native_source(binding: &Binding, workspaces: &[String]) {
        std::fs::create_dir_all(&binding.directory).unwrap();
        let mut store = studio_client_core::store::Store::open(
            &binding.directory.join("studio-client.sqlite3"),
        )
        .unwrap();
        for (index, workspace) in workspaces.iter().enumerate() {
            assert!(store.dispatch(lifecycle_write(&format!("{}-original-draft-{index}",binding.server),"draft.set",json!({"workspace":workspace,"room":"chat","text":format!("retained {} draft {index}",binding.server),"base_revision":0}))).error.is_none());
        }
        let source =
            json!({"id":format!("{}-original-http",binding.server),"text":"original payload"})
                .to_string();
        let entry=store.dispatch(lifecycle_write("seed-unknown","mutation.begin",json!({"path":"/api/messages","serializedPayload":source,"explicitRequestId":format!("{}-original-http",binding.server)}))).result.unwrap();
        assert!(
            store
                .dispatch(lifecycle_write(
                    "mark-unknown",
                    "mutation.unknown",
                    json!({"requestId":entry["requestId"],"payloadDigest":entry["payloadDigest"]})
                ))
                .error
                .is_none()
        );
    }
    fn tagged(value: Value) -> Value {
        match value {
            Value::Object(fields) => json!([
                "object",
                fields
                    .into_iter()
                    .map(|(name, value)| json!([name, tagged(value)]))
                    .collect::<Vec<_>>()
            ]),
            Value::Array(values) => {
                json!(["array", values.into_iter().map(tagged).collect::<Vec<_>>()])
            }
            value => json!(["value", value]),
        }
    }
    fn source_store(name: &str, records: Vec<(Value, Value)>) -> Value {
        json!({"name":name,"keyPath":null,"autoIncrement":false,"indexes":[],"records":records.into_iter().map(|(key,value)|json!([tagged(key),value])).collect::<Vec<_>>()})
    }
    fn offline_payload(server: &str, workspaces: &[String]) -> Value {
        let encoded = server
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let upload = json!({"id":"upload-original","workspace":workspaces[0],"stateDir":"saved-state","agent":"chat","name":"document-name.bin","mime":"application/octet-stream","size":3,"created":1});
        let recording = json!({"id":"recording-original","chatId":"chat","created":2,"sampleRate":16000,"samples":2,"state":"ready","deletedAt":4,"transcriptionAttempt":"speech-original","transcriptionState":"transcribing"});
        let mut databases = vec![
            json!({"name":format!("codex-studio-uploads:server:{server}"),"version":1,"stores":[source_store("uploads",vec![(json!("upload-original"),tagged(upload))]),source_store("contents",vec![(json!("upload-original"),json!(["object",[["id",["value","upload-original"]],["file",["file","original-file.bin","application/original",-10,["bytes","AP+A"]]]]]))])]}),
            json!({"name":format!("codex-studio-dictation:server:{server}"),"version":1,"stores":[source_store("recordings",vec![(json!("recording-original"),tagged(recording))]),source_store("chunks",vec![(json!(["recording-original",0]),json!(["object",[["id",["value","recording-original"]],["index",["value",0]],["pcm",["view","Int16Array",["bytes","AQACAA=="]]]]]))])]}),
            json!({"name":"studio-monitor-input-journal-v1","version":1,"stores":[source_store("inputs",vec![(json!([server,"monitor-original"]),tagged(json!({"requestId":"monitor-write-original","payloadDigest":"a".repeat(64),"uncertain":true})))])]}),
        ];
        for (index, workspace) in workspaces.iter().enumerate() {
            let id = format!("message-original-{index}");
            let outbox = json!({"body":{"id":id,"room":"chat","text":"original body"},"status":"uncertain","attempted":true,"created":1,"displayPending":true,"attachments":[]});
            databases.push(json!({"name":format!("rxdb-dexie-studio{workspace}server{encoded}--0--outbox"),"version":1,"stores":[source_store("docs",vec![(json!(id),tagged(json!({"id":id,"payload":outbox.to_string(),"seq":1,"_deleted":false})))])]}));
        }
        let mutation_key = format!(
            ":server:{server}:{}{}",
            profile::migration_policy()["legacyMutationStoragePrefix"]
                .as_str()
                .unwrap(),
            "b".repeat(64)
        );
        json!({"version":1,"preferences":{(mutation_key):json!({"requestId":"generic-write-original","uncertain":true}).to_string()},"databases":databases})
    }

    async fn read_preserved_entry(client: &Client, receipt: &Value, entry: &str) -> Vec<u8> {
        use base64::Engine;
        let descriptor = receipt["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|saved| saved["id"] == entry)
            .unwrap();
        let total = usize::try_from(descriptor["bytes"].as_u64().unwrap()).unwrap();
        let mut bytes = Vec::new();
        while bytes.len() < total {
            let part = call(
                client,
                policy_method("sourceReadMethod").unwrap(),
                json!({"manifestId":receipt["manifestId"],"entryId":entry,"offset":bytes.len()}),
            )
            .await
            .unwrap();
            let data = base64::engine::general_purpose::STANDARD
                .decode(part["base64"].as_str().unwrap())
                .unwrap();
            assert!(!data.is_empty());
            assert!(bytes.len() + data.len() <= total);
            bytes.extend_from_slice(&data);
            assert_eq!(part["nextOffset"].as_u64(), Some(bytes.len() as u64));
        }
        let sha = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(descriptor["sha256"], sha);
        bytes
    }

    #[tokio::test]
    async fn native_host_migration_coordinator_keeps_native_capture_rechecks_on_the_local_path() {
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path().canonicalize().unwrap();
        let workspace = "a".repeat(32);
        let local = LoopbackIdentity::start(Some(workspace.clone())).await;
        let bindings = vec![fixture_binding(&profile, "local", local.backend.clone())];
        seed_native_source(&bindings[0], std::slice::from_ref(&workspace));
        let evidence = FixtureEvidence::new(
            &profile,
            bindings.clone(),
            json!({"version":1,"preferences":{},"databases":[]}),
        );
        let rechecks = evidence.recaptures.clone();
        let host = MigrationHost::default();
        let result = host
            .coordinate(&profile, bindings, json!("0"), "local", evidence)
            .await
            .unwrap();
        assert_eq!(result["workspaceId"], workspace);
        assert!(host.context("local").await.is_ok());
        assert!(local.requests.load(std::sync::atomic::Ordering::SeqCst) > 0);
        assert!(rechecks.load(std::sync::atomic::Ordering::SeqCst) > 0);
    }

    #[tokio::test]
    async fn native_host_migration_preserves_every_source_before_local_admission_with_an_offline_peer()
     {
        use std::time::Duration;
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path().canonicalize().unwrap();
        let workspaces = vec!["a".repeat(32), "b".repeat(32)];
        let local = LoopbackIdentity::start(Some(workspaces[0].clone())).await;
        let offline = LoopbackIdentity::start(None).await;
        let bindings = vec![
            fixture_binding(&profile, "local", local.backend.clone()),
            fixture_binding(&profile, "offline-peer", offline.backend.clone()),
            fixture_binding(&profile, "empty-peer", offline.backend.clone()),
        ];
        seed_native_source(&bindings[0], &workspaces[..1]);
        seed_native_source(&bindings[1], &workspaces);
        let original_peer = studio_client_core::store::read_original_migration_source(
            &bindings[1].directory.join("studio-client.sqlite3"),
        )
        .unwrap()
        .unwrap();
        let snapshot = offline_payload("offline-peer", &workspaces);
        let evidence = FixtureEvidence::new(&profile, bindings.clone(), snapshot);
        let expected_snapshot = archive_import::owner_capture(&evidence.capture, "offline-peer")
            .unwrap()
            .snapshot;
        let host = MigrationHost::default();
        assert!(host.context("local").await.is_err());
        const COORDINATOR_FIXTURE_DEADLINE: Duration = Duration::from_secs(5);
        let outcome = tokio::time::timeout(
            COORDINATOR_FIXTURE_DEADLINE,
            host.coordinate(&profile, bindings, json!("0"), "local", evidence),
        )
        .await
        .expect("An offline peer cannot leave native admission pending forever");
        let result=outcome.expect("Complete source preservation must permit the freshly authenticated local owner while its peer remains offline");
        assert_eq!(result["workspaceId"], workspaces[0]);
        let owners = result["owners"].as_array().unwrap();
        assert_eq!(owners.len(), 3);
        let held = owners
            .iter()
            .find(|owner| owner["serverId"] == "offline-peer")
            .unwrap();
        assert_eq!(held["sourcePreserved"], true);
        assert_eq!(held["networkAdmitted"], false);
        assert!(
            held["workspaceId"].is_null(),
            "Historical workspaces are storage evidence only"
        );
        let empty = owners
            .iter()
            .find(|owner| owner["serverId"] == "empty-peer")
            .unwrap();
        assert_eq!(empty["sourcePreserved"], true);
        assert_eq!(empty["networkAdmitted"], false);
        assert!(host.context("offline-peer").await.is_err());
        assert!(host.context("empty-peer").await.is_err());
        let admitted = host.context("local").await.unwrap();
        assert!(local.requests.load(std::sync::atomic::Ordering::SeqCst) > 0);
        let proof =
            host.completed.lock().await.as_ref().unwrap()["sourcePreservationReceipt"].clone();
        assert_eq!(
            call(
                &admitted.client,
                policy_method("sourceReadbackMethod").unwrap(),
                json!({"manifestId":proof["manifestId"]})
            )
            .await
            .unwrap(),
            proof
        );
        assert!(
            call(
                &admitted.client,
                policy_method("admitMethod").unwrap(),
                json!({"receipt":proof})
            )
            .await
            .is_err(),
            "Preservation alone never authorizes runtime effects"
        );
        let peer = proof["owners"]
            .as_array()
            .unwrap()
            .iter()
            .find(|owner| owner["serverId"] == "offline-peer")
            .unwrap();
        let saved: Value = serde_json::from_slice(
            &read_preserved_entry(
                &admitted.client,
                &proof,
                peer["sourceEntryId"].as_str().unwrap(),
            )
            .await,
        )
        .unwrap();
        assert_eq!(
            saved["snapshot"], expected_snapshot,
            "All File metadata, PCM, Unknown IDs, outbox bodies and both workspaces must survive exact readback"
        );
        let database = read_preserved_entry(
            &admitted.client,
            &proof,
            peer["nativeEntryId"].as_str().unwrap(),
        )
        .await;
        let restored = profile.join("verified-preserved-native.sqlite3");
        std::fs::write(&restored, database).unwrap();
        let preserved = studio_client_core::store::read_original_migration_source(&restored)
            .unwrap()
            .unwrap();
        for field in ["preferences", "nativeLegacy", "receipts"] {
            assert_eq!(
                preserved[field], original_peer[field],
                "The preserved native owner changed {field}"
            );
        }
        let mut restored = studio_client_core::store::Store::open_for_migration(&restored).unwrap();
        let entry=restored.dispatch(lifecycle_write("read-unknown","mutation.begin",json!({"path":"/api/messages","serializedPayload":json!({"id":"offline-peer-original-http","text":"original payload"}).to_string(),"explicitRequestId":"offline-peer-original-http"}))).result.unwrap();
        assert_eq!(entry["requestId"], "offline-peer-original-http");
        assert_eq!(entry["uncertain"], true);
    }

    fn binding(directory: &Path) -> Binding {
        Binding {
            server: "local".into(),
            storage: "local".into(),
            source: "b".repeat(64),
            directory: directory.into(),
            backend: Backend::new("http://127.0.0.1:9").unwrap(),
        }
    }

    #[test]
    fn native_host_migration_reads_original_without_recovery_or_receipt_rewrite() {
        let directory = tempfile::tempdir().unwrap();
        let owned = directory.path().canonicalize().unwrap();
        let path = owned.join("studio-client.sqlite3");
        let mut store = studio_client_core::store::Store::open(&path).unwrap();
        let request = Request {
            id: "original-preference".into(),
            method: "preferences.raw_set".into(),
            params: json!({"key":"browser:\"root\":theme","raw":"dark"}),
        };
        assert!(store.dispatch(request).error.is_none());
        drop(store);
        let bindings = vec![binding(&owned)];
        let before = std::fs::read(&path).unwrap();
        let captured = OriginalSources::read(&bindings).unwrap();
        assert_eq!(
            captured.preferences("local").unwrap()["preferenceReceipts"][0]["id"],
            "original-preference"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            OriginalSources::read(&bindings).unwrap().digest().unwrap(),
            captured.digest().unwrap()
        );
    }

    #[test]
    fn native_host_migration_refuses_conflicting_original_receipt_identity() {
        let receipt = |raw: &str| json!({"version":1,"legacyPreferenceScope":"root","preferences":{},"preferenceReceipts":[{"id":"same-operation","params":{"raw":raw}}]});
        let originals = OriginalSources {
            sources: BTreeMap::from([
                (
                    "local".into(),
                    json!({"preferences":{"root":receipt("old")}}),
                ),
                (
                    "peer".into(),
                    json!({"preferences":{"root":receipt("changed")}}),
                ),
            ]),
        };
        assert!(originals.preferences("local").is_err());
    }

    #[test]
    fn native_host_migration_original_empty_owner_is_not_an_empty_inventory_claim() {
        let directory = tempfile::tempdir().unwrap();
        let capture = legacy_capture::CapturedSource {
            top_origin: "tauri://localhost".into(),
            origin: "tauri://localhost".into(),
            document_token: "private-document".into(),
            snapshot: json!({"version":1,"preferences":{":server:orphan:theme":"dark"},"databases":[]}),
            digest: "c".repeat(64),
        };
        let originals = OriginalSources {
            sources: BTreeMap::from([("local".into(), empty_original())]),
        };
        assert!(
            require_owner_coverage(&[binding(directory.path())], &capture, &originals).is_err()
        );
    }

    #[test]
    fn native_host_migration_preflights_later_owner_commands_before_original_replay() {
        let directory = tempfile::tempdir().unwrap();
        let local = binding(directory.path());
        let mut peer = binding(directory.path());
        peer.server = "peer".into();
        let capture = legacy_capture::CapturedSource {
            top_origin: "tauri://localhost".into(),
            origin: "tauri://localhost".into(),
            document_token: "private".into(),
            digest: "c".repeat(64),
            snapshot: json!({"version":1,"preferences":{
                "studio-native-preferences-journal-v1":json!([{"id":"original-write","method":"preferences.raw_set","params":{"key":"theme","raw":"dark"}}]).to_string(),
                ":server:peer:studio-native-preferences-journal-v1":json!([{"id":"unsupported-later-owner","method":"api.request","params":{"path":"/api/messages"}}]).to_string()
            },"databases":[]}),
        };
        let originals = OriginalSources {
            sources: BTreeMap::from([("local".into(), empty_original())]),
        };
        assert!(pending_preflight(&[local, peer], &capture, &originals).is_err());
    }

    #[test]
    fn native_host_migration_preflights_cross_scope_request_identity_before_replay() {
        let directory = tempfile::tempdir().unwrap();
        let local = binding(directory.path());
        let mut peer = binding(directory.path());
        peer.server = "peer".into();
        let write = json!([{"id":"same-original-id","method":"preferences.raw_set","params":{"key":"theme","raw":"dark"}}]).to_string();
        let capture = legacy_capture::CapturedSource {
            top_origin: "tauri://localhost".into(),
            origin: "tauri://localhost".into(),
            document_token: "private".into(),
            digest: "c".repeat(64),
            snapshot: json!({"version":1,"preferences":{"studio-native-preferences-journal-v1":write,":server:peer:studio-native-preferences-journal-v1":write},"databases":[]}),
        };
        let originals = OriginalSources {
            sources: BTreeMap::from([("local".into(), empty_original())]),
        };
        assert!(pending_preflight(&[local, peer], &capture, &originals).is_err());
    }

    #[tokio::test]
    async fn native_host_migration_no_context_before_all_owner_proof() {
        let host = MigrationHost::default();
        assert!(host.context("local").await.is_err());
        assert!(host.context("peer").await.is_err());
    }

    #[tokio::test]
    async fn native_host_migration_actual_target_boundary_requires_paused_constructor() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("studio-client.sqlite3");
        let store = studio_client_core::store::Store::open(&path).unwrap();
        drop(store);
        let owned = binding(directory.path());
        let clients = open_targets(
            &[owned],
            &BTreeMap::from([("local".into(), "a".repeat(32))]),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
        let client = &clients["local"].client;
        assert!(
            call(client, "outbox.activate", json!({})).await.is_err(),
            "The actual host constructor cannot admit outbox effects before full owner proof"
        );
    }

    fn lifecycle_capture() -> legacy_capture::CapturedSource {
        legacy_capture::CapturedSource {
            top_origin: "tauri://localhost".into(),
            origin: "tauri://localhost".into(),
            document_token: "private-fixture-document".into(),
            snapshot: json!({"version":1,"origin":"tauri://localhost","preferences":{},"databases":[]}),
            digest: "c".repeat(64),
        }
    }

    fn lifecycle_plan(
        binding: &Binding,
        capture: &legacy_capture::CapturedSource,
        originals: &OriginalSources,
    ) -> archive_import::ValidatedSource {
        archive_import::validated_source(
            &archive_import::SourceOwner {
                server_id: binding.server.clone(),
                workspace_id: "a".repeat(32),
                source_identity: binding.source.clone(),
            },
            capture,
            &originals.preferences("local").unwrap(),
            None,
            &BTreeSet::new(),
        )
        .unwrap()
    }

    fn lifecycle_write(id: &str, method: &str, params: Value) -> Request {
        Request {
            id: id.into(),
            method: method.into(),
            params,
        }
    }

    // Launched by the outer witness. A fresh process gives an actual Store and
    // host restart without sleeping or guessing when a worker releases its lock.
    #[tokio::test]
    async fn native_host_migration_restart_process_phase() {
        let Some(directory) = std::env::var_os("STUDIO_NATIVE_MIGRATION_FIXTURE_DIR") else {
            return;
        };
        let directory = PathBuf::from(directory).canonicalize().unwrap();
        let phase = std::env::var("STUDIO_NATIVE_MIGRATION_FIXTURE_PHASE").unwrap();
        let binding = binding(&directory.join("owner"));
        let capture = lifecycle_capture();
        let originals = OriginalSources::read(std::slice::from_ref(&binding)).unwrap();
        let digest = capture_digest(&capture, &originals, "no-archive").unwrap();
        let mut targets = open_targets(
            std::slice::from_ref(&binding),
            &BTreeMap::from([("local".into(), "a".repeat(32))]),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
        let context = targets.get_mut("local").unwrap();
        let prepared = prepare_owner(
            &binding,
            context,
            lifecycle_plan(&binding, &capture, &originals),
            &digest,
            &originals.sources["local"]["nativeLegacy"],
        )
        .await
        .unwrap();
        let receipt = publish_owner(context, prepared, &digest, &"d".repeat(64))
            .await
            .unwrap();
        admit_owner(context, &receipt, &digest).await.unwrap();
        let preference = lifecycle_write(
            "native-preference-after-cutover",
            "preferences.raw_set",
            json!({"key":"font","raw":"post-cutover"}),
        );
        let draft = lifecycle_write(
            "native-draft-after-cutover",
            "drafts.edit",
            json!({"workspace":"a".repeat(32),"changes":{"chat":"post-cutover draft"}}),
        );
        let evidence = directory.join("expected.json");
        if phase == "initial" {
            use std::{io::Write, os::unix::fs::OpenOptionsExt};
            let preference_response = context.client.request(preference).await.unwrap();
            assert!(preference_response.error.is_none());
            let draft_response = context.client.request(draft).await.unwrap();
            assert!(draft_response.error.is_none());
            let after = OriginalSources::read(std::slice::from_ref(&binding)).unwrap();
            assert_eq!(
                after.digest().unwrap(),
                originals.digest().unwrap(),
                "New canonical native writes are not changes to the retained legacy source"
            );
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(evidence)
                .unwrap();
            file.write_all(json!({"sourceDigest":digest,"proof":receipt,"original":originals.sources["local"],"preferenceResponse":preference_response,"draftResponse":draft_response}).to_string().as_bytes()).unwrap();
            file.sync_all().unwrap();
        } else {
            assert_eq!(phase, "restart");
            let expected: Value =
                serde_json::from_slice(&std::fs::read(evidence).unwrap()).unwrap();
            assert_eq!(digest, expected["sourceDigest"]);
            assert_eq!(
                receipt, expected["proof"],
                "The original durable proof is reused after process restart"
            );
            assert_eq!(
                call(&context.client, "preferences.raw_all", json!({}))
                    .await
                    .unwrap()["font"],
                "post-cutover"
            );
            assert_eq!(
                call(
                    &context.client,
                    "drafts.view",
                    json!({"workspace":"a".repeat(32)})
                )
                .await
                .unwrap()["drafts"]["chat"],
                "post-cutover draft"
            );
            assert_eq!(
                serde_json::to_value(context.client.request(preference).await.unwrap()).unwrap(),
                expected["preferenceResponse"],
                "The exact new preference receipt is retained"
            );
            assert_eq!(
                serde_json::to_value(context.client.request(draft).await.unwrap()).unwrap(),
                expected["draftResponse"],
                "The exact new draft receipt is retained"
            );
            let unchanged = OriginalSources::read(std::slice::from_ref(&binding)).unwrap();
            assert_eq!(
                unchanged.sources["local"]["nativeLegacy"],
                expected["original"]["nativeLegacy"]
            );
            assert_eq!(
                unchanged.preferences("local").unwrap(),
                originals.preferences("local").unwrap()
            );
            // Simulate an external old-source change. The host must hold that
            // source instead of importing its old values over current state.
            assert!(
                context
                    .client
                    .request(lifecycle_write(
                        "external-old-source-change",
                        "preferences.raw_set",
                        json!({"key":"browser:\"root\":font","raw":"changed-old-source"})
                    ))
                    .await
                    .unwrap()
                    .error
                    .is_none()
            );
            let changed = OriginalSources::read(std::slice::from_ref(&binding)).unwrap();
            let changed_digest = capture_digest(&capture, &changed, "no-archive").unwrap();
            assert_ne!(changed_digest, digest);
            let error = prepare_owner(
                &binding,
                context,
                lifecycle_plan(&binding, &capture, &changed),
                &changed_digest,
                &changed.sources["local"]["nativeLegacy"],
            )
            .await
            .unwrap_err();
            assert!(
                error.contains("changed after its durable cutover"),
                "A changed old source must be refused before another import: {error}"
            );
            assert_eq!(
                call(&context.client, "preferences.raw_all", json!({}))
                    .await
                    .unwrap()["font"],
                "post-cutover"
            );
            assert_eq!(
                call(
                    &context.client,
                    "drafts.view",
                    json!({"workspace":"a".repeat(32)})
                )
                .await
                .unwrap()["drafts"]["chat"],
                "post-cutover draft"
            );
        }
    }

    #[tokio::test]
    async fn native_host_migration_restart_preserves_new_native_writes_and_changed_old_source_hold()
    {
        use std::{process::Stdio, time::Duration};
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("owner")).unwrap();
        let mut store = studio_client_core::store::Store::open(
            &directory.path().join("owner/studio-client.sqlite3"),
        )
        .unwrap();
        assert!(
            store
                .dispatch(lifecycle_write(
                    "original-native-preference",
                    "preferences.raw_set",
                    json!({"key":"browser:\"root\":font","raw":"baseline"})
                ))
                .error
                .is_none()
        );
        assert!(store.dispatch(lifecycle_write("original-native-draft","draft.set",json!({"workspace":"a".repeat(32),"room":"chat","text":"original draft","base_revision":0}))).error.is_none());
        drop(store);
        for phase in ["initial", "restart"] {
            let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "migration_host::tests::native_host_migration_restart_process_phase",
                    "--nocapture",
                ])
                .env("STUDIO_NATIVE_MIGRATION_FIXTURE_DIR", directory.path())
                .env("STUDIO_NATIVE_MIGRATION_FIXTURE_PHASE", phase)
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            const RESTART_PROCESS_DEADLINE: Duration = Duration::from_secs(15);
            match tokio::time::timeout(RESTART_PROCESS_DEADLINE, child.wait()).await {
                Ok(status) => assert!(
                    status.unwrap().success(),
                    "The actual native restart phase failed: {phase}"
                ),
                Err(_) => {
                    child
                        .kill()
                        .await
                        .expect("The owned fixture process must be killed and reaped");
                    panic!("The native migration fixture phase exceeded its deadline: {phase}");
                }
            }
        }
    }
}
