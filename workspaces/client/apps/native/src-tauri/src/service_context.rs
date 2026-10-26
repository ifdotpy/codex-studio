//! Host-owned service identities. A renderer receives a capability, not a path,
//! origin, key, or permission to redirect an existing context.
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use studio_client_core::{
    backend::Backend,
    client::Client,
    protocol::{Request, Response},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Open {
    pub server_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LegacyPreferences {
    pub server_id: String,
    pub writes: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CutoverCheck {
    pub server_id: String,
    pub token: String,
}

#[derive(Default)]
pub struct Cutovers {
    proofs: HashMap<String, (String, String)>,
}
impl Cutovers {
    pub fn begin(&mut self, server: &str, source: &str) -> Result<Value, String> {
        let (token, identity) = self
            .proofs
            .entry(server.into())
            .or_insert((studio_client_core::server_keys::random_id()?, source.into()));
        if identity != source {
            return Err("The native storage source identity changed.".into());
        }
        Ok(json!({"token":token,"serverId":server,"sourceIdentity":identity}))
    }
    pub fn verify(&self, server: &str, token: Option<&str>, source: &str) -> Result<(), String> {
        match self.proofs.get(server) {
            Some((saved,identity)) if identity == source && token.is_none_or(|token|token == saved) => Ok(()),
            _ => Err("Native storage requires the exact host cutover proof before its source can be read or imported.".into()),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Call {
    pub handle: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

pub struct Context {
    pub server_id: String,
    pub workspace_id: String,
    pub client: Arc<Client>,
    pub backend: Option<Backend>,
    pub source_identity: String,
    pub migration_complete: std::sync::atomic::AtomicBool,
    pub archive_hold: bool,
}

/// These commands release resources owned by the original client connection.
/// Their core handlers retain the recording, transcription, and session checks.
fn is_local_cleanup(method: &str) -> bool {
    matches!(
        method,
        "media.recordingStop" | "speech.cancel" | "voice.sessionEnd"
    )
}

/// Archive verification needs these local hash commands before the cutover seal.
fn is_archive_hash(method: &str) -> bool {
    matches!(
        method,
        "files.hashBegin" | "files.hashChunk" | "files.hashFinish"
    )
}

/// Stored receipt and cache reads remain available while the backend is offline.
pub fn needs_network(request: &Request) -> bool {
    if is_local_cleanup(&request.method) {
        return false;
    }
    matches!(
        request.method.as_str(),
        "api.request"
            | "api.prepareSession"
            | "sync.open"
            | "drafts.refresh"
            | "drafts.push"
            | "desktop.downloadAndSave"
            | "desktop.fileAction"
            | "outbox.deliver"
            | "outbox.activate"
            | "uploads.deliver"
            | "uploads.download"
            | "voice.sessionStart"
    ) || request.method.starts_with("subscriptions.")
        || request.method.starts_with("transcript_view.")
        || (request.method == "view.subscribe"
            && matches!(
                request.params["topic"].as_str(),
                Some("sse" | "subscriptions")
            ))
}

/// Public compatibility effects must not bypass a selected context's cutover.
pub fn legacy_effect_scope(request: &Request) -> Option<&str> {
    let method = request.method.as_str();
    let post = |params: &Value| {
        params["method"].as_str() == Some("POST")
            || (params.get("method").is_none() && params.get("body").is_some())
    };
    if method == "servers.request" && post(&request.params) {
        return Some(request.params["serverId"].as_str().unwrap_or(""));
    }
    if (method == "api.request" && post(&request.params))
        || method.starts_with("desktop.")
        || (method.starts_with("media.") && method != "media.recordingState")
        || (method.starts_with("voice.") && method != "voice.sessionState")
        || (method.starts_with("speech.") && method != "speech.languages")
        || matches!(
            method,
            "servers.pair" | "servers.forget" | "servers.pairAttempt"
        )
        || (method == "view.subscribe"
            && matches!(
                request.params["topic"].as_str(),
                Some("microphone" | "voice")
            ))
    {
        Some("local")
    } else {
        None
    }
}

#[derive(Default)]
pub struct Contexts {
    by_handle: HashMap<String, Arc<Context>>,
    by_server: HashMap<String, String>,
}

impl Contexts {
    pub fn is_empty(&self) -> bool {
        self.by_handle.is_empty()
    }
    pub fn existing(&self, server: &str) -> Option<(String, Arc<Context>)> {
        let handle = self.by_server.get(server)?;
        Some((handle.clone(), self.by_handle.get(handle)?.clone()))
    }

    pub fn get(&self, handle: &str) -> Result<Arc<Context>, String> {
        self.by_handle
            .get(handle)
            .cloned()
            .ok_or_else(|| "The native server context is unavailable.".into())
    }

    pub fn insert(&mut self, context: Context) -> Result<Value, String> {
        if !crate::profile::valid_sha256(&context.source_identity) {
            return Err("The server context requires its exact host cutover source digest.".into());
        }
        let handle = studio_client_core::server_keys::random_id()?;
        let value = context.descriptor(&handle);
        self.by_server
            .insert(context.server_id.clone(), handle.clone());
        self.by_handle.insert(handle, Arc::new(context));
        Ok(value)
    }

    /// Register the private migration owner's existing connection. A retry
    /// retains its capability; another context cannot replace that authority.
    pub(crate) fn insert_shared(&mut self, context: Arc<Context>) -> Result<Value, String> {
        if !crate::profile::valid_sha256(&context.source_identity)
            || context.archive_hold
            || !context
                .migration_complete
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(
                "The shared native context requires its completed private migration proof.".into(),
            );
        }
        if let Some(backend) = &context.backend {
            backend.validate_identity()?;
        }
        if let Some(handle) = self.by_server.get(&context.server_id) {
            let existing = self
                .by_handle
                .get(handle)
                .ok_or("The existing native context capability is missing.")?;
            if !Arc::ptr_eq(existing, &context) {
                return Err("Another native context already owns this server capability.".into());
            }
            return Ok(context.descriptor(handle));
        }
        let handle = studio_client_core::server_keys::random_id()?;
        if self.by_handle.contains_key(&handle) {
            return Err("The native context capability identity is already registered.".into());
        }
        let descriptor = context.descriptor(&handle);
        self.by_server
            .insert(context.server_id.clone(), handle.clone());
        self.by_handle.insert(handle, context);
        Ok(descriptor)
    }
}

impl Context {
    pub async fn ensure_ready(&self, request: &Request) -> Result<(), String> {
        if is_archive_hash(&request.method)
            || matches!(
                request.method.as_str(),
                "system.uuid" | "system.uuidBatch" | "system.sha256"
            )
        {
            return Ok(());
        }
        if self.archive_hold {
            return Err("The sealed archive needs privileged source inventory verification before native writes can start.".into());
        }
        if self
            .migration_complete
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(());
        }
        Err(
            "Native storage migration is incomplete. User writes and delivery remain disabled."
                .into(),
        )
    }

    /// Only the private host caller supplies this receipt after checking its
    /// current source capture. Neither a renderer request nor a legacy row can.
    pub(crate) fn confirm_host_capture(
        &mut self,
        receipt: &Value,
        current_digest: &str,
    ) -> Result<(), String> {
        let valid_hash = |value: &Value| value.as_str().is_some_and(crate::profile::valid_sha256);
        if receipt["complete"] != true
            || receipt["hostVerified"] != true
            || receipt["serverId"] != self.server_id
            || receipt["sourceIdentity"] != self.source_identity
            || receipt["proofVersion"] != crate::profile::migration_policy()["proofVersion"]
            || receipt["sourceDigest"].as_str() != Some(current_digest)
            || !valid_hash(&receipt["sourceDigest"])
            || !valid_hash(&receipt["inventoryDigest"])
        {
            return Err("The private host capture proof does not match this context.".into());
        }
        if let Some(backend) = &self.backend {
            backend.validate_identity()?;
        }
        self.archive_hold = false;
        self.migration_complete
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }
    pub fn descriptor(&self, handle: &str) -> Value {
        let identity = self
            .backend
            .as_ref()
            .map(Backend::public_identity)
            .unwrap_or(Value::Null);
        json!({"handle":handle,"serverId":self.server_id,"workspaceId":self.workspace_id,"origin":identity["origin"],"credentialId":identity["credentialId"]})
    }

    pub fn admission(&self, id: String, call: Call) -> Result<Request, String> {
        if id.is_empty() || id.len() > 128 {
            return Err("Invalid native request identity.".into());
        }
        if !is_local_cleanup(&call.method)
            && let Some(backend) = &self.backend
        {
            backend.validate_identity()?;
        }
        let method = call.method.as_str();
        if ![
            "preferences.",
            "drafts.",
            "outbox.",
            "monitor_input.",
            "mutation.",
            "subscriptions.",
            "transcript_view.",
            "uploads.",
        ]
        .iter()
        .any(|prefix| method.starts_with(prefix))
            && !is_archive_hash(method)
            && !matches!(
                method,
                "api.request"
                    | "api.prepareSession"
                    | "sync.open"
                    | "projection.get"
                    | "projection.readSubscription"
                    | "projection.stateSnapshot"
                    | "identity.get"
                    | "view.subscribe"
                    | "view.unsubscribe"
                    | "system.uuid"
                    | "system.uuidBatch"
                    | "system.sha256"
                    | "files.stage"
                    | "files.stage_many"
                    | "files.text"
                    | "desktop.pickFiles"
                    | "desktop.pickDirectory"
                    | "desktop.revealPath"
                    | "desktop.showVaultKeys"
                    | "desktop.fileAction"
                    | "desktop.requestMicrophone"
                    | "desktop.prepareTranscription"
                    | "desktop.saveRecording"
                    | "media.recordingStart"
                    | "media.recordingStop"
                    | "media.recordingState"
                    | "speech.transcribe"
                    | "speech.languages"
                    | "speech.cancel"
                    | "voice.sessionStart"
                    | "voice.sessionState"
                    | "voice.sessionEnd"
                    | "voice.sessionMute"
                    | "desktop.downloadAndSave"
                    | "desktop.saveText"
                    | "dictation.list"
                    | "dictation.deleted"
                    | "dictation.get"
                    | "dictation.trash"
                    | "dictation.restore"
                    | "dictation.update"
            )
        {
            return Err("This service is not available in a server context.".into());
        }
        if self.server_id != "local"
            && matches!(
                method,
                "desktop.pickDirectory"
                    | "desktop.revealPath"
                    | "desktop.showVaultKeys"
                    | "desktop.fileAction"
            )
        {
            return Err("This action requires the local server context.".into());
        }
        let mut params = if call.params.is_null() {
            json!({})
        } else {
            call.params
        };
        let fields = params
            .as_object_mut()
            .ok_or("Invalid native service parameters.")?;
        if matches!(
            method,
            "preferences.raw_set"
                | "preferences.raw_remove"
                | "preferences.raw_compare_exchange"
                | "preferences.edit"
                | "preferences.import"
        ) && (fields
            .get("key")
            .and_then(Value::as_str)
            .is_some_and(studio_client_core::reserved_preference_metadata_key)
            || fields
                .get("entries")
                .and_then(Value::as_object)
                .is_some_and(|entries| {
                    entries
                        .keys()
                        .any(|key| studio_client_core::reserved_preference_metadata_key(key))
                }))
        {
            return Err(
                "Preference source metadata is available only to the host migration owner.".into(),
            );
        }
        if method.starts_with("preferences.")
            && (fields
                .get("key")
                .and_then(Value::as_str)
                .is_some_and(|key| key.starts_with("browser:"))
                || fields
                    .get("entries")
                    .and_then(Value::as_object)
                    .is_some_and(|entries| entries.keys().any(|key| key.starts_with("browser:"))))
        {
            return Err(
                "Legacy preference namespaces are available only to the host migration owner."
                    .into(),
            );
        }
        // Workspace is an assertion of the host scope, never a scope selector.
        for key in ["workspace", "workspaceId"] {
            if fields
                .get(key)
                .is_some_and(|value| value.as_str() != Some(&self.workspace_id))
            {
                return Err("The request belongs to another server workspace.".into());
            }
        }
        if matches!(method, "api.request") {
            if fields
                .get("requestId")
                .is_some_and(|value| value.as_str() != Some(id.as_str()))
            {
                return Err(
                    "The HTTP command must keep its original native request identity.".into(),
                );
            }
            if fields.get("download").is_some_and(|value| value != false)
                || fields
                    .get("httpResponse")
                    .is_some_and(|value| value != false)
            {
                return Err("Native bytes require the owner-bound file service.".into());
            }
            fields.insert("workspaceId".into(), self.workspace_id.clone().into());
            fields.insert("jsonResponse".into(), true.into());
            fields
                .entry("requestId")
                .or_insert_with(|| id.clone().into());
        }
        if method == "view.subscribe"
            && !matches!(
                fields.get("topic").and_then(Value::as_str),
                Some(
                    "sse"
                        | "drafts"
                        | "outbox"
                        | "recordings"
                        | "state"
                        | "subscriptions"
                        | "transcript-view"
                        | "voice-session"
                        | "preferences"
                        | "uploads"
                )
            )
        {
            return Err("This native subscription topic is unavailable.".into());
        }
        if matches!(method, "view.subscribe" | "view.unsubscribe") {
            let subscription = fields
                .get("subscription")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.len() <= 64)
                .ok_or("Invalid native subscription identity.")?;
            fields.insert(
                "subscription".into(),
                format!("{}:{subscription}", call.handle).into(),
            );
            if method == "view.subscribe" {
                fields.insert("workspace".into(), self.workspace_id.clone().into());
            }
        }
        Ok(Request {
            id,
            method: call.method,
            params,
        })
    }

    pub async fn send(&self, request: Request) -> Result<Response, String> {
        self.ensure_ready(&request).await?;
        let checks_identity = matches!(request.method.as_str(), "sync.open" | "identity.get");
        let preference_snapshot = request.method == "preferences.raw_all";
        let mut response = self.client.request(request).await?;
        if preference_snapshot && let Some(Value::Object(values)) = response.result.as_mut() {
            values.retain(|key, _| {
                !key.starts_with("browser:")
                    && !studio_client_core::reserved_preference_metadata_key(key)
            });
        }
        if checks_identity
            && let Some(value) = &response.result
            && value["workspaceId"].as_str() != Some(&self.workspace_id)
        {
            return Err(
                "The server workspace changed. Reopen its services before another request.".into(),
            );
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_context_shared_registration_preserves_exact_admitted_client_and_retry_handle() {
        let directory = tempfile::tempdir().unwrap();
        let backend = Backend::new("http://127.0.0.1:9").unwrap();
        let client = Arc::new(
            Client::open_with_backend(directory.path().join("owned"), backend.clone(), |_| {})
                .unwrap(),
        );
        let original = Arc::new(Context {
            server_id: "local".into(),
            workspace_id: "a".repeat(32),
            client: client.clone(),
            backend: Some(backend.clone()),
            source_identity: "b".repeat(64),
            migration_complete: true.into(),
            archive_hold: false,
        });
        let mut contexts = Contexts::default();
        let first = contexts.insert_shared(original.clone()).unwrap();
        let handle = first["handle"].as_str().unwrap();
        assert!(Arc::ptr_eq(&contexts.get(handle).unwrap(), &original));
        assert!(Arc::ptr_eq(&contexts.get(handle).unwrap().client, &client));
        assert_eq!(contexts.insert_shared(original.clone()).unwrap(), first);
        assert_eq!(contexts.by_handle.len(), 1);
        let replacement = Arc::new(Context {
            server_id: "local".into(),
            workspace_id: "c".repeat(32),
            client,
            backend: Some(backend),
            source_identity: "d".repeat(64),
            migration_complete: true.into(),
            archive_hold: false,
        });
        assert!(contexts.insert_shared(replacement).is_err());
        assert_eq!(contexts.by_handle.len(), 1);
        assert!(Arc::ptr_eq(
            &contexts.existing("local").unwrap().1,
            &original
        ));
        assert_eq!(contexts.existing("local").unwrap().0, handle);
    }

    #[tokio::test]
    async fn native_context_shared_registration_refuses_a_held_or_unproved_context() {
        let directory = tempfile::tempdir().unwrap();
        let backend = Backend::new("http://127.0.0.1:9").unwrap();
        let client = Arc::new(
            Client::open_with_backend(directory.path().join("owned"), backend.clone(), |_| {})
                .unwrap(),
        );
        let mut contexts = Contexts::default();
        for (complete, held, source) in [
            (false, true, "b".repeat(64)),
            (true, true, "b".repeat(64)),
            (false, false, "b".repeat(64)),
            (true, false, "invalid-source".into()),
        ] {
            assert!(
                contexts
                    .insert_shared(Arc::new(Context {
                        server_id: "local".into(),
                        workspace_id: "a".repeat(32),
                        client: client.clone(),
                        backend: Some(backend.clone()),
                        source_identity: source,
                        migration_complete: complete.into(),
                        archive_hold: held
                    }))
                    .is_err()
            );
            assert!(contexts.is_empty());
        }
    }

    #[tokio::test]
    async fn native_context_local_file_controls_keep_their_exact_scope() {
        let directory = tempfile::tempdir().unwrap();
        for server in ["local", "remote"] {
            let backend = Backend::new("http://127.0.0.1:9").unwrap();
            let client = Arc::new(
                Client::open_with_backend(directory.path().join(server), backend.clone(), |_| {})
                    .unwrap(),
            );
            let context = Context {
                server_id: server.into(),
                workspace_id: "a".repeat(32),
                client,
                backend: Some(backend),
                source_identity: "b".repeat(64),
                migration_complete: true.into(),
                archive_hold: false,
            };
            for (method, params) in [
                ("desktop.pickDirectory", json!({})),
                ("desktop.revealPath", json!({"value":"/owned-folder"})),
                ("desktop.showVaultKeys", json!({"value":"owned-prefix"})),
                (
                    "desktop.fileAction",
                    json!({"value":{"action":"preview","target":{"asset":"owned-asset"}}}),
                ),
            ] {
                let result = context.admission(
                    "original-command".into(),
                    Call {
                        handle: "owned-handle".into(),
                        method: method.into(),
                        params: params.clone(),
                    },
                );
                if server == "local" {
                    let admitted = result.unwrap_or_else(|error| panic!("{method}: {error}"));
                    assert_eq!(admitted.id, "original-command");
                    assert_eq!(admitted.method, method);
                    assert_eq!(admitted.params, params);
                } else {
                    assert!(
                        result.is_err(),
                        "{method} cannot acquire a local path through a remote context"
                    );
                }
            }
        }
    }

    struct PairingMemory(std::sync::Mutex<Value>);
    impl studio_client_core::server_keys::Vault for PairingMemory {
        fn read(&self) -> Result<Value, String> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn write(&self, value: &Value) -> Result<(), String> {
            *self.0.lock().unwrap() = value.clone();
            Ok(())
        }
    }

    #[tokio::test]
    async fn native_context_revoked_pairing_keeps_only_owned_cleanup_admissible() {
        let directory = tempfile::tempdir().unwrap();
        let credential = studio_client_core::server_keys::new_pair(
            &json!({"protocol":1,"origin":"https://cleanup-fixture.ts.net","serverId":"remote","inviteId":"fixture","token":"fixture","publicKey":"fixture","tailscaleUser":"fixture"}),
            "fixture-pair",
        )
        .unwrap();
        let credential_id = credential["clientId"].clone();
        let vault = Arc::new(PairingMemory(std::sync::Mutex::new(
            json!({"servers":{"remote":credential}}),
        )));
        let remote = Arc::new(
            studio_client_core::server_transport::RemoteClient::new(vault.clone()).unwrap(),
        );
        let backend = Backend::from_remote(remote.bind("remote").unwrap()).unwrap();
        let client = Arc::new(
            Client::open_with_backend(directory.path().join("owned"), backend.clone(), |_| {})
                .unwrap(),
        );
        let mut contexts = Contexts::default();
        let descriptor = contexts
            .insert(Context {
                server_id: "remote".into(),
                workspace_id: "a".repeat(32),
                client,
                backend: Some(backend.clone()),
                source_identity: "b".repeat(64),
                migration_complete: true.into(),
                archive_hold: false,
            })
            .unwrap();
        let handle = descriptor["handle"].as_str().unwrap();
        let context = contexts.get(handle).unwrap();
        let descriptor = context.descriptor(handle);
        assert_eq!(descriptor["origin"], "https://cleanup-fixture.ts.net");
        assert_eq!(descriptor["credentialId"], credential_id);
        assert!(descriptor.get("privateKey").is_none());
        assert!(backend.validate_identity().is_ok());
        vault.0.lock().unwrap()["servers"] = json!({});
        assert!(backend.validate_identity().is_err());

        for method in [
            "voice.sessionStart",
            "media.recordingStart",
            "speech.transcribe",
            "desktop.requestMicrophone",
            "desktop.pickFiles",
            "desktop.downloadAndSave",
            "api.request",
        ] {
            assert!(
                context
                    .admission(
                        "new-effect".into(),
                        Call {
                            handle: handle.into(),
                            method: method.into(),
                            params: json!({"agent":"chat-a"}),
                        }
                    )
                    .is_err(),
                "Revocation must not grant a fresh effect: {method}"
            );
        }
        assert!(contexts.get("sibling-copied-handle").is_err());
        for method in ["media.recordingStop", "speech.cancel", "voice.sessionEnd"] {
            assert!(context.admission("foreign-scope".into(), Call {
                handle: handle.into(), method: method.into(),
                params: json!({"workspace":"c".repeat(32),"agent":"chat-a","sessionId":"owned-session"}),
            }).is_err());
            let request = context.admission("owned-stop".into(), Call {
                handle: handle.into(), method: method.into(),
                params: json!({"agent":"chat-a","sessionId":"owned-session","requestId":"original-capture"}),
            }).unwrap_or_else(|error| panic!("The original owner must reach {method} after revoke: {error}"));
            assert_eq!(request.id, "owned-stop");
            assert_eq!(request.params["sessionId"], "owned-session");
            assert_eq!(request.params["requestId"], "original-capture");
            context.ensure_ready(&request).await.unwrap();
            assert!(
                !needs_network(&request),
                "Local cleanup must not wait for the offline backend: {method}"
            );
        }
    }

    #[test]
    fn native_context_offline_cleanup_does_not_require_a_new_network_verification() {
        for method in ["media.recordingStop", "speech.cancel", "voice.sessionEnd"] {
            let request = Request {
                id: "owned-stop".into(),
                method: method.into(),
                params: json!({"agent":"chat-a","sessionId":"owned-session"}),
            };
            assert!(!needs_network(&request), "{method}");
        }
        for method in [
            "voice.sessionStart",
            "api.request",
            "desktop.downloadAndSave",
        ] {
            assert!(
                needs_network(&Request {
                    id: "fresh-effect".into(),
                    method: method.into(),
                    params: json!({}),
                }),
                "{method}"
            );
        }
    }

    #[tokio::test]
    async fn native_archive_hash_commands_are_ready_before_the_cutover_seal() {
        let directory = tempfile::tempdir().unwrap();
        let client = Arc::new(
            Client::open_with_backend(
                directory.path().join("owned"),
                Backend::new("http://127.0.0.1:9").unwrap(),
                |_| {},
            )
            .unwrap(),
        );
        let context = Context {
            server_id: "local".into(),
            workspace_id: "a".repeat(32),
            client,
            backend: None,
            source_identity: "b".repeat(64),
            migration_complete: Default::default(),
            archive_hold: true,
        };
        for method in ["files.hashBegin", "files.hashChunk", "files.hashFinish"] {
            context
                .ensure_ready(&Request {
                    id: "archive-hash".into(),
                    method: method.into(),
                    params: json!({"id":"source"}),
                })
                .await
                .unwrap_or_else(|error| {
                    panic!("Archive verification must precede the seal: {method}: {error}")
                });
        }
        assert!(
            !context
                .migration_complete
                .load(std::sync::atomic::Ordering::Acquire)
        );
    }

    #[tokio::test]
    async fn native_archive_hash_uses_context_admission_and_dispatch_without_releasing_user_writes()
    {
        let directory = tempfile::tempdir().unwrap();
        let client = Arc::new(
            Client::open_with_backend(
                directory.path().join("owned"),
                Backend::new("http://127.0.0.1:9").unwrap(),
                |_| {},
            )
            .unwrap(),
        );
        let mut contexts = Contexts::default();
        let descriptor = contexts
            .insert(Context {
                server_id: "local".into(),
                workspace_id: "a".repeat(32),
                client,
                backend: None,
                source_identity: "b".repeat(64),
                migration_complete: Default::default(),
                archive_hold: true,
            })
            .unwrap();
        let handle = descriptor["handle"].as_str().unwrap();
        let context = contexts.get(handle).unwrap();
        for (method, params) in [
            ("files.hashBegin", json!({"id":"source","bytes":3})),
            (
                "files.hashChunk",
                json!({"id":"source","offset":0,"base64":"YWJj"}),
            ),
            ("files.hashFinish", json!({"id":"source"})),
            ("files.hashFinish", json!({"id":"source"})),
        ] {
            let request = context
                .admission(
                    "archive-hash".into(),
                    Call {
                        handle: handle.into(),
                        method: method.into(),
                        params,
                    },
                )
                .unwrap_or_else(|error| panic!("The archive caller must reach {method}: {error}"));
            context.ensure_ready(&request).await.unwrap();
            let response = context.send(request).await.unwrap();
            assert!(response.error.is_none(), "{method}: {:?}", response.error);
            if method == "files.hashFinish" {
                let result = response.result.unwrap();
                assert_eq!(result["bytes"], 3);
                assert_eq!(
                    result["sha256"],
                    "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
                );
            }
        }
        for method in [
            "preferences.raw_set",
            "outbox.deliver",
            "voice.sessionStart",
            "desktop.pickFiles",
        ] {
            let request = context
                .admission(
                    "fresh-effect".into(),
                    Call {
                        handle: handle.into(),
                        method: method.into(),
                        params: json!({}),
                    },
                )
                .unwrap();
            assert!(context.ensure_ready(&request).await.is_err(), "{method}");
        }
        assert!(
            context
                .admission(
                    "foreign".into(),
                    Call {
                        handle: handle.into(),
                        method: "files.hashBegin".into(),
                        params: json!({"id":"source","bytes":3,"workspace":"c".repeat(32)}),
                    }
                )
                .is_err()
        );
        assert!(
            !context
                .migration_complete
                .load(std::sync::atomic::Ordering::Acquire)
        );
    }

    #[test]
    fn native_cutover_requires_a_seal_for_legacy_effects_but_not_bootstrap_reads() {
        for method in [
            "desktop.pickAttachments",
            "desktop.saveBegin",
            "media.captureStart",
            "voice.create",
            "speech.transcribe",
            "servers.pair",
        ] {
            let request = Request {
                id: "exact".into(),
                method: method.into(),
                params: json!({}),
            };
            assert_eq!(legacy_effect_scope(&request), Some("local"), "{method}");
        }
        for params in [
            json!({"method":"POST","path":"/api/messages"}),
            json!({"path":"/api/messages","body":{"id":"original"}}),
        ] {
            assert_eq!(
                legacy_effect_scope(&Request {
                    id: "exact".into(),
                    method: "api.request".into(),
                    params
                }),
                Some("local")
            );
        }
        let remote = Request {
            id: "exact".into(),
            method: "servers.request".into(),
            params: json!({"serverId":"remote","method":"POST"}),
        };
        assert_eq!(legacy_effect_scope(&remote), Some("remote"));
        for method in [
            "preferences.raw_all",
            "identity.get",
            "sync.open",
            "system.uuid",
            "service.open",
            "service.legacy_preferences",
            "service.finish_cutover",
        ] {
            assert_eq!(
                legacy_effect_scope(&Request {
                    id: "read".into(),
                    method: method.into(),
                    params: json!({})
                }),
                None,
                "{method}"
            );
        }
        assert_eq!(
            legacy_effect_scope(&Request {
                id: "read".into(),
                method: "api.request".into(),
                params: json!({"path":"/api/accounts","method":"GET"})
            }),
            None
        );
    }
    #[test]
    fn native_cutover_proof_requires_the_same_source_and_server() {
        let mut cuts = Cutovers::default();
        assert!(cuts.verify("local", None, "source").is_err());
        let proof = cuts.begin("local", "source").unwrap();
        let token = proof["token"].as_str().unwrap();
        assert!(cuts.verify("local", Some(token), "source").is_ok());
        assert!(cuts.verify("remote", Some(token), "source").is_err());
        assert!(cuts.verify("local", Some("foreign"), "source").is_err());
        assert!(
            cuts.verify("local", Some(token), "changed-profile-or-archive")
                .is_err()
        );
        assert_eq!(cuts.begin("local", "source").unwrap(), proof);
        assert!(cuts.begin("local", "changed").is_err());
    }
    #[test]
    fn native_context_open_rejects_renderer_paths_and_credentials() {
        for params in [
            json!({"serverId":"remote","origin":"http://127.0.0.1"}),
            json!({"serverId":"remote","stateDirectory":"/foreign"}),
            json!({"serverId":"remote","credentialId":"foreign"}),
        ] {
            assert!(serde_json::from_value::<Open>(params).is_err());
        }
        assert!(serde_json::from_value::<Open>(json!({"serverId":"remote"})).is_ok());
    }

    #[tokio::test]
    async fn native_context_holds_user_writes_until_the_durable_final_cutover_seal() {
        let directory = tempfile::tempdir().unwrap();
        let backend = Backend::new("http://127.0.0.1:9").unwrap();
        let client = Arc::new(
            Client::open_with_backend(directory.path().join("owned"), backend.clone(), |_| {})
                .unwrap(),
        );
        let source = "a".repeat(64);
        let mut context = Context {
            server_id: "local".into(),
            workspace_id: "b".repeat(32),
            client: client.clone(),
            backend: Some(backend),
            source_identity: source.clone(),
            migration_complete: Default::default(),
            archive_hold: false,
        };
        let descriptor = context.descriptor("host-selected");
        assert_eq!(descriptor["origin"], "http://127.0.0.1:9");
        assert_eq!(descriptor["credentialId"], Value::Null);
        let write = || Request {
            id: "original-write".into(),
            method: "preferences.raw_set".into(),
            params: json!({"key":"value","raw":"native"}),
        };
        assert!(
            context
                .send(write())
                .await
                .unwrap_err()
                .contains("migration is incomplete")
        );
        let source_bytes =
            json!({"version":1,"sourceIdentity":source,"expectedByteImports":[],"preferences":{}})
                .to_string();
        let imported = client
            .request(Request {
                id: "import".into(),
                method: "client_storage.import".into(),
                params: json!({"manifestId":"source-metadata","serializedSource":source_bytes}),
            })
            .await
            .unwrap()
            .result
            .unwrap();
        // Metadata alone does not release user writes.
        assert!(context.send(write()).await.is_err());
        let finish = Request {
            id: "exact-finish".into(),
            method: crate::profile::migration_policy()["finishMethod"]
                .as_str()
                .unwrap()
                .into(),
            params: json!({"sourceIdentity":source,"serverId":"local","metadata":[{"manifestId":"source-metadata","sourceSha256":imported["sourceSha256"]}],"bytes":[],"archive":null,"capture":{"proofVersion":crate::profile::migration_policy()["proofVersion"],"beforeDigest":"c".repeat(64),"afterDigest":"c".repeat(64),"originInventoryDigest":"d".repeat(64),"ownerInventoryDigest":"e".repeat(64),"allOriginsVerified":true}}),
        };
        let first = client
            .request(Request {
                id: finish.id.clone(),
                method: finish.method.clone(),
                params: finish.params.clone(),
            })
            .await
            .unwrap();
        assert_eq!(first.result.as_ref().unwrap()["complete"], true);
        assert_eq!(client.request(finish).await.unwrap().result, first.result);
        assert!(
            context.send(write()).await.is_err(),
            "A receipt alone cannot replace the host's current-source check"
        );
        let mut foreign = first.result.clone().unwrap();
        foreign["serverId"] = json!("other");
        assert!(
            context
                .confirm_host_capture(&foreign, &"c".repeat(64))
                .is_err()
        );
        assert!(
            context
                .confirm_host_capture(first.result.as_ref().unwrap(), &"f".repeat(64))
                .is_err()
        );
        context
            .confirm_host_capture(first.result.as_ref().unwrap(), &"c".repeat(64))
            .unwrap();
        assert!(context.send(write()).await.unwrap().error.is_none());
        let saved = client
            .request(Request {
                id: "read".into(),
                method: "preferences.raw_all".into(),
                params: json!({}),
            })
            .await
            .unwrap();
        assert_eq!(saved.result.unwrap()["value"], "native");
        context.archive_hold = true;
        assert!(
            context
                .send(write())
                .await
                .unwrap_err()
                .contains("privileged source inventory")
        );
    }

    #[tokio::test]
    async fn native_context_does_not_accept_legacy_renderer_completion_as_host_capture() {
        let directory = tempfile::tempdir().unwrap();
        let client = Arc::new(
            Client::open_with_backend(
                directory.path().join("owned"),
                Backend::new("http://127.0.0.1:9").unwrap(),
                |_| {},
            )
            .unwrap(),
        );
        let source = "a".repeat(64);
        let metadata = client.request(Request {
            id:"legacy-import".into(),method:"client_storage.import".into(),
            params:json!({"manifestId":"legacy-metadata","serializedSource":json!({"version":1,"sourceIdentity":source,"expectedByteImports":[],"preferences":{}}).to_string()}),
        }).await.unwrap().result.unwrap();
        let legacy = client.request(Request {
            id:"legacy-finish".into(),method:"client_storage.finish_cutover".into(),
            params:json!({"sourceIdentity":source,"serverId":"local","metadata":[{"manifestId":"legacy-metadata","sourceSha256":metadata["sourceSha256"]}],"bytes":[],"archive":null}),
        }).await.unwrap().result.unwrap();
        assert_eq!(legacy["complete"], true);
        let context = Context {
            server_id: "local".into(),
            workspace_id: "b".repeat(32),
            client,
            backend: None,
            source_identity: source,
            migration_complete: Default::default(),
            archive_hold: false,
        };
        let response = context
            .send(Request {
                id: "must-stay-held".into(),
                method: "preferences.raw_set".into(),
                params: json!({"key":"theme","raw":"injected"}),
            })
            .await;
        assert!(
            response.is_err(),
            "A renderer completion row cannot replace fresh private host verification"
        );
    }

    #[tokio::test]
    async fn native_context_reserved_preference_metadata_is_not_a_renderer_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let backend = Backend::new("http://127.0.0.1:9").unwrap();
        let client = Arc::new(
            Client::open_with_backend(directory.path().join("owned"), backend, |_| {}).unwrap(),
        );
        let context = Context {
            server_id: "local".into(),
            workspace_id: "a".repeat(32),
            client,
            backend: None,
            source_identity: "b".repeat(64),
            migration_complete: Default::default(),
            archive_hold: false,
        };
        let policy: Value = serde_json::from_str(include_str!(
            "../../../../contracts/preference-storage.json"
        ))
        .unwrap();
        let writer = policy["writerStorageKey"].as_str().unwrap();
        let fields = policy["fieldCacheKey"].as_str().unwrap();
        for key in [
            writer.to_owned(),
            format!("{fields}:user"),
            format!("{fields}:server"),
            format!("{fields}:migration"),
        ] {
            for rpc in ["rawSet", "rawRemove", "compareExchange", "edit"] {
                let method = policy["rpc"][rpc].as_str().unwrap();
                assert!(context.admission("forged-metadata".into(),Call {handle:"owned".into(),method:method.into(),params:json!({"key":key,"raw":"{}","expectedRaw":null,"previous":{},"next":{}})}).is_err(),"{method}: {key}");
            }
            let mut entries = serde_json::Map::new();
            entries.insert(key, json!("{}"));
            assert!(
                context
                    .admission(
                        "forged-bulk".into(),
                        Call {
                            handle: "owned".into(),
                            method: "preferences.import".into(),
                            params: json!({"version":1,"entries":entries})
                        }
                    )
                    .is_err()
            );
        }
        assert!(
            context
                .admission(
                    "ordinary".into(),
                    Call {
                        handle: "owned".into(),
                        method: policy["rpc"]["rawSet"].as_str().unwrap().into(),
                        params: json!({"key":"theme","raw":"dark"})
                    }
                )
                .is_ok()
        );
    }

    #[tokio::test]
    async fn native_context_storage_topics_keep_their_original_scope() {
        let directory = tempfile::tempdir().unwrap();
        let backend = Backend::new("http://127.0.0.1:9").unwrap();
        let client = Arc::new(
            Client::open_with_backend(directory.path().join("owned"), backend, |_| {}).unwrap(),
        );
        let context = Context {
            server_id: "local".into(),
            workspace_id: "a".repeat(32),
            client,
            backend: None,
            source_identity: "b".repeat(64),
            migration_complete: Default::default(),
            archive_hold: false,
        };
        let policy: Value = serde_json::from_str(include_str!(
            "../../../../contracts/preference-storage.json"
        ))
        .unwrap();
        for topic in policy["topics"].as_object().unwrap().values() {
            let request = context
                .admission(
                    "subscribe-original".into(),
                    Call {
                        handle: "owned".into(),
                        method: "view.subscribe".into(),
                        params: json!({"subscription":"original","topic":topic}),
                    },
                )
                .unwrap();
            assert_eq!(request.params["subscription"], "owned:original");
            assert_eq!(request.params["workspace"], context.workspace_id);
        }
    }

    #[tokio::test]
    async fn native_context_preference_snapshot_keeps_private_metadata_out_of_public_storage() {
        let directory = tempfile::tempdir().unwrap();
        let client = Arc::new(
            Client::open_with_backend(
                directory.path().join("owned"),
                Backend::new("http://127.0.0.1:9").unwrap(),
                |_| {},
            )
            .unwrap(),
        );
        let policy: Value = serde_json::from_str(include_str!(
            "../../../../contracts/preference-storage.json"
        ))
        .unwrap();
        let keys = [
            policy["writerStorageKey"].as_str().unwrap().to_owned(),
            format!("{}:user", policy["fieldCacheKey"].as_str().unwrap()),
            "browser:\"root\":source".into(),
            "theme".into(),
        ];
        for (index, key) in keys.iter().enumerate() {
            let saved = client
                .request(Request {
                    id: format!("original-{index}"),
                    method: "preferences.raw_set".into(),
                    params: json!({"key":key,"raw":"retained"}),
                })
                .await
                .unwrap();
            assert!(saved.error.is_none());
        }
        let context = Context {
            server_id: "local".into(),
            workspace_id: "a".repeat(32),
            client: client.clone(),
            backend: None,
            source_identity: "b".repeat(64),
            migration_complete: std::sync::atomic::AtomicBool::new(true),
            archive_hold: false,
        };
        let public = context
            .send(Request {
                id: "public-snapshot".into(),
                method: "preferences.raw_all".into(),
                params: json!({}),
            })
            .await
            .unwrap()
            .result
            .unwrap();
        assert_eq!(
            public,
            json!({"theme":"retained"}),
            "Public clear/remove must not see host-only source metadata as user preferences"
        );
        let private = client
            .request(Request {
                id: "private-snapshot".into(),
                method: "preferences.raw_all".into(),
                params: json!({}),
            })
            .await
            .unwrap()
            .result
            .unwrap();
        for key in keys {
            assert_eq!(
                private[&key], "retained",
                "The migration source must remain intact"
            );
        }
    }

    #[tokio::test]
    async fn native_context_fences_workspace_methods_and_subscription_events() {
        let directory = tempfile::tempdir().unwrap();
        let backend = Backend::new("http://127.0.0.1:9").unwrap();
        let client = Arc::new(
            Client::open_with_backend(directory.path().join("owned"), backend, |_| {}).unwrap(),
        );
        let context = Context {
            server_id: "remote".into(),
            workspace_id: "a".repeat(32),
            client,
            backend: None,
            source_identity: "c".repeat(64),
            migration_complete: Default::default(),
            archive_hold: false,
        };
        let call = |method: &str, params: Value| Call {
            handle: "opaque".into(),
            method: method.into(),
            params,
        };
        for (method, params) in [
            ("drafts.view", json!({"workspace":"b".repeat(32)})),
            (
                "api.request",
                json!({"workspaceId":"b".repeat(32),"path":"/api/messages"}),
            ),
            (
                "view.subscribe",
                json!({"subscription":"same","topic":"projection"}),
            ),
            ("desktop.revealPath", json!({"path":"/foreign"})),
            (
                "dictation.wav",
                json!({"id":"recording","directory":"/foreign"}),
            ),
            ("dictation.captureAppend", json!({})),
            ("files.saveClaim", json!({})),
            ("files.saveSettle", json!({})),
            ("files.exportForSave", json!({})),
            ("files.stageDownload", json!({})),
            (
                "api.request",
                json!({"path":"/api/accounts","requestId":"different"}),
            ),
            (
                "api.request",
                json!({"path":"/api/accounts","httpResponse":true}),
            ),
            (
                "api.request",
                json!({"path":"/api/accounts","download":true}),
            ),
            ("client_storage.export_preferences", json!({"scope":"root"})),
            (
                "client_storage.import",
                json!({"manifestId":"forged","serializedSource":"{}"}),
            ),
            ("files.importBegin", json!({"manifest":{}})),
            (
                "files.importChunk",
                json!({"manifestId":"forged","entry":0,"offset":0,"base64":""}),
            ),
            ("files.importCommit", json!({"manifestId":"forged"})),
            ("client_storage.host_capture_finish", json!({"capture":{}})),
            ("identity.set", json!({"workspaceId":"b".repeat(32)})),
            (
                "projection.put",
                json!({"workspace":"a".repeat(32),"entity":"uiPreferences","payload":"{}"}),
            ),
            (
                "projection.applyEntities",
                json!({"workspace":"a".repeat(32),"entities":[]}),
            ),
            (
                "projection.applySubscription",
                json!({"workspace":"a".repeat(32),"items":[]}),
            ),
            (
                "projection.stageSubscriptionFragment",
                json!({"workspace":"a".repeat(32)}),
            ),
            (
                "sync.applyEntities",
                json!({"workspaceId":"a".repeat(32),"entities":[]}),
            ),
            (
                "preferences.raw_set",
                json!({"key":"browser:\"server:other\":secret","raw":"x"}),
            ),
        ] {
            assert!(
                context
                    .admission("original".into(), call(method, params))
                    .is_err(),
                "{method}"
            );
        }
        let read = context
            .admission(
                "original".into(),
                call(
                    "view.subscribe",
                    json!({"subscription":"same","topic":"subscriptions"}),
                ),
            )
            .unwrap();
        for method in ["projection.readSubscription", "projection.stateSnapshot"] {
            assert!(
                context
                    .admission(
                        "read".into(),
                        call(method, json!({"workspace":"a".repeat(32)}))
                    )
                    .is_ok(),
                "{method}"
            );
        }
        assert_eq!(read.params["subscription"], "opaque:same");
        assert_eq!(read.params["workspace"], "a".repeat(32));
        let input = context
            .admission(
                "original".into(),
                call(
                    "api.request",
                    json!({"path":"/api/monitor/input","body":{"id":"monitor","text":"synthetic"}}),
                ),
            )
            .unwrap();
        assert_eq!(input.id, "original");
        assert_eq!(input.params["requestId"], "original");
        assert_eq!(input.params["workspaceId"], "a".repeat(32));
        let mut contexts = Contexts::default();
        assert!(
            contexts
                .insert(Context {
                    server_id: "invalid".into(),
                    workspace_id: context.workspace_id.clone(),
                    client: context.client.clone(),
                    backend: None,
                    source_identity: json!({"preferences":{"key":"raw"}}).to_string(),
                    migration_complete: Default::default(),
                    archive_hold: false
                })
                .is_err(),
            "Serialized preference data cannot replace the host source digest."
        );
        let descriptor = contexts.insert(context).unwrap();
        let handle = descriptor["handle"].as_str().unwrap();
        assert!(contexts.get("copied-invalid-handle").is_err());
        assert_eq!(contexts.get(handle).unwrap().server_id, "remote");
        assert_eq!(contexts.existing("remote").unwrap().0, handle);
    }
}
