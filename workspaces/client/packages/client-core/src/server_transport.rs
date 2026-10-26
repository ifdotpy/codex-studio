//! Authenticated remote transport. The renderer selects an existing credential,
//! never a private key or an arbitrary network authority.
use crate::server_keys::{self, Vault, field};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

// Maximum concurrent signed requests and response bodies per native client.
const MAX_OPEN_SERVER_RESPONSES: usize = 32;
// Maximum of those slots that requests to one server may hold. An unreachable
// server keeps each request open until its connect or send timeout, and must
// not take the slots of the other servers.
const MAX_OPEN_RESPONSES_PER_SERVER: usize = MAX_OPEN_SERVER_RESPONSES / 2;

// Maximum metadata-only transport diagnostic lines per process when opted in.
const MAX_TRANSPORT_DIAGNOSTIC_LINES: usize = 200;
static TRANSPORT_DIAGNOSTIC_LINES: AtomicUsize = AtomicUsize::new(0);

fn transport_diagnostic(method: &str, url: &reqwest::Url, status: u16, kind: &str) {
    if std::env::var("STUDIO_NATIVE_TRANSPORT_DIAGNOSTICS").as_deref() != Ok("1") {
        return;
    }
    if TRANSPORT_DIAGNOSTIC_LINES.fetch_add(1, Ordering::Relaxed) >= MAX_TRANSPORT_DIAGNOSTIC_LINES
    {
        return;
    }
    // Never log query values, credentials, request IDs, headers or payloads.
    eprintln!(
        "Server transport: method={method} host={} path={} status={status} kind={kind}",
        url.host_str().unwrap_or("unknown"),
        url.path()
    );
}

struct Body {
    _permits: (OwnedSemaphorePermit, OwnedSemaphorePermit),
    response: reqwest::Response,
    touched: Instant,
}
/// A streamed response body with the server id it belongs to.
type BodyEntry = (String, Arc<Mutex<Body>>);
pub struct RemoteClient {
    vault: Arc<dyn Vault>,
    mutation: Mutex<()>,
    response_slots: Arc<Semaphore>,
    server_slots: std::sync::Mutex<HashMap<String, Arc<Semaphore>>>,
    http: reqwest::Client,
    bodies: Mutex<HashMap<String, BodyEntry>>,
    registry_events: Arc<server_keys::RegistryEvents>,
}

/// A host-selected paired server. Credentials never cross the renderer boundary.
/// Every send checks that pairing has not changed since this context opened.
#[derive(Clone)]
pub struct RemoteBinding {
    client: Arc<RemoteClient>,
    server: String,
    credential: Value,
    origin: String,
}

impl RemoteBinding {
    /// Immutable host-selected server identity, without credential material.
    pub fn server_id(&self) -> &str {
        &self.server
    }

    /// Public metadata for this immutable pairing. The pair receipt uses clientId as credentialId.
    pub fn public_identity(&self) -> Value {
        json!({"origin":self.origin,"credentialId":self.credential["clientId"]})
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Stable private-store namespace for this exact pairing, not a caller path.
    pub fn storage_key(&self) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(
            serde_json::to_vec(&(&self.server, &self.credential)).expect("JSON credential"),
        )
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.client.vault.read()?["servers"][&self.server] != self.credential {
            return Err(
                "The paired server identity changed. Reopen its services before another request."
                    .into(),
            );
        }
        Ok(())
    }

    pub(crate) async fn send(
        &self,
        method: &str,
        url: &str,
        headers: &Value,
        body: Vec<u8>,
        id: &str,
    ) -> Result<reqwest::Response, String> {
        self.validate()?;
        let slots = self
            .client
            .server_slots
            .lock()
            .map_err(|_| "The server transport stopped.")?
            .entry(self.server.clone())
            .or_insert_with(|| Arc::new(Semaphore::new(MAX_OPEN_RESPONSES_PER_SERVER)))
            .clone();
        let permits = (
            slots.try_acquire_owned().map_err(
                |_| "Too many open responses for this server. The request was not sent.",
            )?,
            self.client
                .response_slots
                .clone()
                .try_acquire_owned()
                .map_err(|_| "Too many open server responses. The request was not sent.")?,
        );
        // Reserve before submission and retain through consumption of the body.
        let mut response = self
            .client
            .signed(&self.credential, method, url, headers, body, id)
            .await?;
        response.extensions_mut().insert(Arc::new(permits));
        Ok(response)
    }
}

impl RemoteClient {
    /// Public metadata only. The vault remains the sole pairing authority.
    pub fn registry_snapshot(&self) -> Result<Value, String> {
        server_keys::public_registry(self.vault.as_ref())
    }

    pub fn registry_changes(&self) -> tokio::sync::watch::Receiver<String> {
        self.registry_events.subscribe()
    }

    /// Host-only inventory. Bindings retain credentials inside their existing
    /// transport owner; callers expose only public_identity to a renderer.
    pub fn migration_bindings(self: &Arc<Self>) -> Result<Vec<(String, RemoteBinding)>, String> {
        let state = self.vault.read()?;
        let servers = state["servers"]
            .as_object()
            .ok_or("The saved paired server inventory is invalid.")?;
        let mut names = servers.keys().cloned().collect::<Vec<_>>();
        names.sort();
        let mut bindings = Vec::with_capacity(names.len());
        for server in names {
            bindings.push((server.clone(), self.bind(&server)?));
        }
        // Do not publish a partial inventory across concurrent pairing changes.
        if self.vault.read()?["servers"] != state["servers"] {
            return Err("The paired server inventory changed during verification.".into());
        }
        Ok(bindings)
    }

    pub fn bind(self: &Arc<Self>, server: &str) -> Result<RemoteBinding, String> {
        if !server_keys::selector(server) {
            return Err("Invalid paired server selector.".into());
        }
        let credential = self.vault.read()?["servers"][server].clone();
        if field(&credential, "serverId")? != server {
            return Err("The paired server registry identity does not match its key.".into());
        }
        let origin = server_keys::origin(field(&credential, "origin")?)?;
        field(&credential, "clientId")?;
        // Validate the actual key before any state directory is selected.
        server_keys::signature(
            &credential,
            "GET",
            "/api/sync/identity",
            "0",
            "binding",
            "binding",
            &[],
        )?;
        Ok(RemoteBinding {
            client: self.clone(),
            server: server.into(),
            credential,
            origin,
        })
    }
    pub fn new(vault: Arc<dyn Vault>) -> Result<Self, String> {
        let registry_events = server_keys::observe_registry(vault.as_ref())?;
        Ok(Self {
            vault,
            mutation: Mutex::new(()),
            response_slots: Arc::new(Semaphore::new(MAX_OPEN_SERVER_RESPONSES)),
            server_slots: std::sync::Mutex::new(HashMap::new()),
            http: crate::tls::client_builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .connect_timeout(Duration::from_secs(15))
                .build()
                .map_err(|e| e.to_string())?,
            bodies: Mutex::new(HashMap::new()),
            registry_events,
        })
    }
    async fn signed(
        &self,
        credential: &Value,
        method: &str,
        url: &str,
        headers: &Value,
        body: Vec<u8>,
        request_id: &str,
    ) -> Result<reqwest::Response, String> {
        let url = reqwest::Url::parse(url).map_err(|_| "Invalid server URL.")?;
        if url.origin().ascii_serialization() != server_keys::origin(field(credential, "origin")?)?
            || !url.path().starts_with("/api/")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || !matches!(method, "GET" | "POST" | "HEAD")
        {
            return Err("The request does not belong to this server API.".into());
        }
        let path = percent_encoding::percent_decode_str(url.path())
            .decode_utf8()
            .map_err(|_| "Invalid server path.")?;
        let path = path.trim_end_matches('/');
        if path.starts_with("/api/multi-server/v1/")
            || (method == "POST"
                && (path == "/api/multi-server" || path.starts_with("/api/multi-server/")))
        {
            return Err("Use the workspace shell for server management.".into());
        }
        self.send_signed(credential, method, url, headers, body, request_id)
            .await
    }

    /// Only pair() supplies this endpoint after validating the exact invitation.
    /// Public signed transport never receives this management exception.
    async fn pair_send(
        &self,
        draft: &Value,
        request_id: &str,
    ) -> Result<reqwest::Response, String> {
        let origin = server_keys::origin(field(draft, "origin")?)?;
        let url = reqwest::Url::parse(&(origin + "/api/multi-server/v1/pair"))
            .map_err(|_| "Invalid pairing endpoint.")?;
        self.send_signed(
            draft,
            "POST",
            url,
            &json!([["content-type", "application/json"]]),
            field(draft, "body")?.as_bytes().to_vec(),
            request_id,
        )
        .await
    }

    async fn send_signed(
        &self,
        credential: &Value,
        method: &str,
        url: reqwest::Url,
        headers: &Value,
        body: Vec<u8>,
        request_id: &str,
    ) -> Result<reqwest::Response, String> {
        if request_id.is_empty()
            || request_id.len() > 128
            || request_id.contains(['\n', '\r'])
            || body.len() > 32 * 1024 * 1024
        {
            return Err("Invalid server request identity or body.".into());
        }
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_secs()
            .to_string();
        let nonce = server_keys::request_nonce()?;
        let target = format!(
            "{}{}",
            url.path(),
            url.query().map(|v| format!("?{v}")).unwrap_or_default()
        );
        let signature = server_keys::signature(
            credential, method, &target, &timestamp, &nonce, request_id, &body,
        )?;
        let mut request = self.http.request(
            method
                .parse::<reqwest::Method>()
                .map_err(|_| "Invalid server method.")?,
            url,
        );
        if let Some(headers) = headers.as_array() {
            for header in headers {
                let name = header[0]
                    .as_str()
                    .ok_or("Invalid server header.")?
                    .to_ascii_lowercase();
                let value = header[1].as_str().ok_or("Invalid server header.")?;
                if matches!(
                    name.as_str(),
                    "accept"
                        | "content-type"
                        | "x-studio-api-schema"
                        | "x-canvas-workspace"
                        | "if-none-match"
                        | "last-event-id"
                ) {
                    request = request.header(name, value);
                }
            }
        }
        request = request
            .header("x-studio-client", field(credential, "clientId")?)
            .header("x-studio-server", field(credential, "serverId")?)
            .header("x-studio-timestamp", timestamp)
            .header("x-studio-nonce", nonce)
            .header("x-studio-request-id", request_id)
            .header("x-studio-signature", signature);
        if method == "POST" {
            request = request.body(body);
        }
        tokio::time::timeout(Duration::from_secs(30), request.send())
            .await
            .map_err(|_| "The server did not respond. The request was not repeated.")?
            .map_err(|_| "The server request failed. Its outcome may be unknown.".into())
    }
    pub async fn dispatch(&self, method: &str, p: &Value) -> Result<Value, String> {
        match method {
            "servers.request" => {
                let state = self.vault.read()?;
                let server = field(p, "serverId")?;
                let credential = &state["servers"][server];
                if field(credential, "clientId")? != field(p, "credentialId")? {
                    return Err("The server key does not belong to this client.".into());
                }
                let target =
                    reqwest::Url::parse(field(p, "url")?).map_err(|_| "Invalid server URL.")?;
                let path = percent_encoding::percent_decode_str(target.path())
                    .decode_utf8()
                    .map_err(|_| "Invalid server path.")?;
                let path = path.trim_end_matches('/');
                if path.starts_with("/api/multi-server/v1/")
                    || (p["method"] == "POST"
                        && (path == "/api/multi-server" || path.starts_with("/api/multi-server/")))
                {
                    return Err("Use the workspace shell for server management.".into());
                }
                let body = STANDARD
                    .decode(p["base64"].as_str().unwrap_or(""))
                    .map_err(|_| "Invalid server request bytes.")?;
                let method = field(p, "method")?;
                let id = match p["requestId"].as_str() {
                    Some(v) => v.to_owned(),
                    None if method != "POST" => server_keys::random_id()?,
                    _ => return Err("A server write needs its original request ID.".into()),
                };
                // Reserve before sending: a capacity error must not discard a
                // response to a write that the server already accepted.
                self.bodies.lock().await.retain(|_, (_, body)| {
                    body.try_lock()
                        .map(|v| v.touched.elapsed() < Duration::from_secs(120))
                        .unwrap_or(true)
                });
                let server_slots = self
                    .server_slots
                    .lock()
                    .map_err(|_| "The server transport stopped.")?
                    .entry(server.to_owned())
                    .or_insert_with(|| Arc::new(Semaphore::new(MAX_OPEN_RESPONSES_PER_SERVER)))
                    .clone();
                let permits = match server_slots.clone().try_acquire_owned().and_then(
                    |server_permit| {
                        Ok((
                            server_permit,
                            self.response_slots.clone().try_acquire_owned()?,
                        ))
                    },
                ) {
                    Ok(permits) => permits,
                    Err(_) => {
                        let bodies = self.bodies.lock().await;
                        let mut idle = 0;
                        let mut reading = 0;
                        let mut paths = std::collections::BTreeMap::new();
                        for (_, body) in bodies.values() {
                            if let Ok(body) = body.try_lock() {
                                idle += 1;
                                // Exclude queries, headers and credentials from diagnostics.
                                *paths
                                    .entry(body.response.url().path().to_owned())
                                    .or_insert(0) += 1;
                            } else {
                                reading += 1;
                            }
                        }
                        let detail = format!(
                            "idle={idle}, reading={reading}, pending={}, server={}/{MAX_OPEN_RESPONSES_PER_SERVER}, paths={paths:?}",
                            MAX_OPEN_SERVER_RESPONSES
                                .saturating_sub(self.response_slots.available_permits())
                                .saturating_sub(bodies.len()),
                            MAX_OPEN_RESPONSES_PER_SERVER
                                .saturating_sub(server_slots.available_permits())
                        );
                        eprintln!("Server response capacity: {detail}");
                        return Err(format!(
                            "Too many open server responses. The request was not sent. ({detail})"
                        ));
                    }
                };
                let response = self
                    .signed(
                        credential,
                        method,
                        field(p, "url")?,
                        &p["headers"],
                        body,
                        &id,
                    )
                    .await
                    .inspect_err(|_| {
                        transport_diagnostic(method, &target, 0, "send-error");
                    })?;
                let status = response.status().as_u16();
                let kind = match response
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                {
                    Some(v) if v.starts_with("text/event-stream") => "event-stream",
                    Some(v) if v.starts_with("application/json") => "json",
                    _ => "other",
                };
                transport_diagnostic(method, response.url(), status, kind);
                let headers = response
                    .headers()
                    .iter()
                    .filter_map(|(name, value)| {
                        value
                            .to_str()
                            .ok()
                            .map(|value| json!([name.as_str(), value]))
                    })
                    .collect::<Vec<_>>();
                if method == "HEAD" || matches!(status, 204 | 205 | 304) {
                    return Ok(json!({"status":status,"headers":headers}));
                }
                let stream = server_keys::random_id()?;
                let mut bodies = self.bodies.lock().await;
                bodies.insert(
                    stream.clone(),
                    (
                        server.to_owned(),
                        Arc::new(Mutex::new(Body {
                            _permits: permits,
                            response,
                            touched: Instant::now(),
                        })),
                    ),
                );
                Ok(json!({"status":status,"headers":headers,"stream":stream}))
            }
            "servers.read" => {
                let stream = field(p, "stream")?;
                let body = self
                    .bodies
                    .lock()
                    .await
                    .get(stream)
                    .map(|(_, body)| body.clone())
                    .ok_or("The server response is closed.")?;
                let mut body = body
                    .try_lock()
                    .map_err(|_| "A server response read is already in progress.")?;
                body.touched = Instant::now();
                let result =
                    tokio::time::timeout(Duration::from_secs(25), body.response.chunk()).await;
                match result {
                    Ok(Ok(Some(bytes))) => {
                        if body.response.status().is_client_error()
                            && bytes.len() < 4096
                            && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
                        {
                            let code = match value["code"].as_str() {
                                Some(
                                    code @ ("invalid_credentials"
                                    | "invalid_id"
                                    | "server_mismatch"
                                    | "clock_difference"
                                    | "invalid_nonce"
                                    | "invalid_signature"
                                    | "client_revoked"
                                    | "owner_mismatch"
                                    | "request_conflict"
                                    | "replayed_nonce"),
                                ) => code,
                                _ if value["error"] == "Invalid request" => "invalid-request",
                                _ => "other-rejection",
                            };
                            transport_diagnostic(
                                "READ",
                                body.response.url(),
                                body.response.status().as_u16(),
                                code,
                            );
                        }
                        Ok(json!({"base64":STANDARD.encode(bytes)}))
                    }
                    Ok(Ok(None)) => {
                        transport_diagnostic(
                            "READ",
                            body.response.url(),
                            body.response.status().as_u16(),
                            "eof",
                        );
                        self.bodies.lock().await.remove(stream);
                        Ok(json!({"done":true}))
                    }
                    _ => {
                        transport_diagnostic(
                            "READ",
                            body.response.url(),
                            body.response.status().as_u16(),
                            if result.is_err() {
                                "timeout"
                            } else {
                                "read-error"
                            },
                        );
                        self.bodies.lock().await.remove(stream);
                        Err("The server response was interrupted.".into())
                    }
                }
            }
            "servers.cancel" => {
                self.bodies.lock().await.remove(field(p, "stream")?);
                Ok(Value::Null)
            }
            "servers.pairAttempt" => {
                let state = self.vault.read()?;
                let draft = &state["drafts"][field(p, "requestId")?];
                if draft.is_null() {
                    return Ok(json!(false));
                }
                if draft["forgotten"] == true {
                    return Ok(json!(false));
                }
                if field(draft, "serverId")? != field(p, "serverId")?
                    || field(draft, "origin")? != server_keys::origin(field(p, "origin")?)?
                    || p.get("inviteId").is_some_and(|v| v != &draft["inviteId"])
                {
                    return Err("The saved key belongs to another pairing attempt.".into());
                }
                Ok(json!(
                    !draft["receipt"].is_null()
                        || (draft["rejected"] != true && draft["body"].is_string())
                ))
            }
            "servers.forget" => {
                let _guard = self.mutation.lock().await;
                let server = field(p, "serverId")?;
                if !server_keys::selector(server) {
                    return Err("Invalid paired server selector.".into());
                }
                server_keys::mutate(self.vault.as_ref(), |state| {
                    if state["forgottenServers"].is_null() {
                        state["forgottenServers"] = json!({});
                    }
                    state["forgottenServers"]
                        .as_object_mut()
                        .ok_or("Invalid forgotten server identities.")?
                        .insert(server.into(), json!(true));
                    state["servers"]
                        .as_object_mut()
                        .ok_or("Invalid server key store.")?
                        .remove(server);
                    for draft in state["drafts"]
                        .as_object_mut()
                        .ok_or("Invalid server key store.")?
                        .values_mut()
                        .filter(|draft| draft["serverId"] == server)
                    {
                        let draft = draft
                            .as_object_mut()
                            .ok_or("Invalid saved pairing attempt.")?;
                        draft.remove("privateKey");
                        draft.remove("body");
                        draft.insert("forgotten".into(), json!(true));
                    }
                    Ok(())
                })?;
                // A removed pairing must not retain live authenticated streams.
                self.bodies
                    .lock()
                    .await
                    .retain(|_, (owner, _)| owner != server);
                Ok(Value::Null)
            }
            "servers.pair" => self.pair(p).await,
            _ => Err("Unknown server credential command.".into()),
        }
    }
    async fn pair(&self, p: &Value) -> Result<Value, String> {
        let _guard = self.mutation.lock().await;
        let invitation = &p["invitation"];
        let id = field(p, "requestId")?;
        if !server_keys::request_identity(id) {
            return Err("Invalid pairing request identity.".into());
        }
        let origin = server_keys::origin(field(p, "origin")?)?;
        if server_keys::origin(field(invitation, "origin")?)? != origin {
            return Err("The invitation belongs to another server.".into());
        }
        // Keep exact invitation and base authority in the private draft. Every
        // commit re-reads the vault after the network wait under the shared owner.
        let invitation_hash = server_keys::private_digest(invitation)?;
        let draft = server_keys::mutate(self.vault.as_ref(), |state| {
            if state["drafts"][id].is_null() {
                let mut draft = server_keys::new_pair(invitation, id)?;
                draft["invitationHash"] = json!(invitation_hash);
                draft["baseCredentialHash"] = json!(server_keys::private_digest(
                    &state["servers"][field(invitation, "serverId")?]
                )?);
                state["drafts"]
                    .as_object_mut()
                    .ok_or("Invalid server key store.")?
                    .insert(id.into(), draft);
            }
            Ok(state["drafts"][id].clone())
        })?;
        if draft["serverId"] != invitation["serverId"]
            || draft["inviteId"] != invitation["inviteId"]
            || draft["origin"] != origin
        {
            return Err("This request belongs to another pairing attempt.".into());
        }
        if draft["forgotten"] == true {
            return Err("This pairing attempt was forgotten. Keep its original outcome.".into());
        }
        if draft
            .get("invitationHash")
            .is_some_and(|saved| saved != &Value::String(invitation_hash.clone()))
        {
            return Err("This request belongs to another pairing invitation.".into());
        }
        if !draft["receipt"].is_null() {
            return server_keys::mutate(self.vault.as_ref(), |state| {
                let current = &state["servers"][field(&draft, "serverId")?];
                if field(current, "clientId")? != field(&draft["receipt"], "credentialId")?
                    || field(current, "origin")? != field(&draft["receipt"], "origin")?
                    || current["publicKey"] != draft["publicKey"]
                {
                    return Err("The completed pairing no longer owns this server.".into());
                }
                Ok(draft["receipt"].clone())
            });
        }
        if draft["rejected"] == true {
            return Ok(json!({"notApplied":"invite_unavailable"}));
        }
        if draft.get("baseCredentialHash").is_none() {
            return Err(
                "The saved pairing has no original authority proof. Its outcome remains unknown."
                    .into(),
            );
        }
        let mut response = self.pair_send(&draft, id).await?;
        let status = response.status();
        let bytes = tokio::time::timeout(Duration::from_secs(15), async {
            let mut bytes = Vec::new();
            if response
                .content_length()
                .is_some_and(|length| length > 1024 * 1024)
            {
                return Err("Pairing reply is too large.");
            }
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| "Pairing reply was lost. Retry the same invitation.")?
            {
                if bytes.len() + chunk.len() > 1024 * 1024 {
                    return Err("Pairing reply is too large.");
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(bytes)
        })
        .await
        .map_err(|_| "Pairing reply timed out. Retry the same invitation.")??;
        let receipt: Value = serde_json::from_slice(&bytes)
            .map_err(|_| "Invalid pairing reply. Retry the same invitation.")?;
        if status.as_u16() == 403 && receipt["code"] == "invite_unavailable" {
            server_keys::mutate(self.vault.as_ref(), |state| {
                if state["drafts"][id] != draft {
                    return Err("The pairing attempt changed while its reply was pending.".into());
                }
                state["drafts"][id]["rejected"] = json!(true);
                Ok(())
            })?;
            return Ok(json!({"notApplied":"invite_unavailable"}));
        }
        if !status.is_success() {
            return Err("Pairing failed. Retry the same invitation.".into());
        }
        if receipt["protocol"] != 1
            || receipt["paired"] != true
            || receipt["serverId"] != invitation["serverId"]
            || receipt["clientId"] != draft["clientId"]
            || server_keys::origin(field(&receipt, "origin")?)? != origin
            || receipt["publicKey"] != invitation["publicKey"]
            || receipt["tailscaleUser"] != invitation["tailscaleUser"]
        {
            return Err("The pairing reply does not match the invitation.".into());
        }
        let server = field(&receipt, "serverId")?;
        let result = json!({"id":server,"origin":origin,"label":receipt["label"].as_str().unwrap_or(server),"credentialId":draft["clientId"]});
        let mut credential = draft.clone();
        credential.as_object_mut().unwrap().remove("body");
        credential["targetPublicKey"] = receipt["publicKey"].clone();
        credential["tailscaleUser"] = receipt["tailscaleUser"].clone();
        credential["label"] = result["label"].clone();
        credential
            .as_object_mut()
            .expect("pair credential")
            .remove("invitationHash");
        credential
            .as_object_mut()
            .expect("pair credential")
            .remove("baseCredentialHash");
        server_keys::mutate(self.vault.as_ref(), |state| {
            if state["drafts"][id] != draft {
                if state["drafts"][id]["receipt"] == result
                    && state["servers"][server] == credential
                {
                    return Ok(());
                }
                return Err("The pairing attempt changed while its reply was pending.".into());
            }
            if draft["baseCredentialHash"].as_str()
                != Some(server_keys::private_digest(&state["servers"][server])?.as_str())
            {
                return Err("The paired server changed while its reply was pending.".into());
            }
            if !state["forgottenServers"].is_null() {
                state["forgottenServers"]
                    .as_object_mut()
                    .ok_or("Invalid forgotten server identities.")?
                    .remove(server);
            }
            state["servers"]
                .as_object_mut()
                .ok_or("Invalid server key store.")?
                .insert(server.to_owned(), credential);
            state["drafts"][id]
                .as_object_mut()
                .unwrap()
                .remove("privateKey");
            state["drafts"][id].as_object_mut().unwrap().remove("body");
            state["drafts"][id]["receipt"] = result.clone();
            Ok(())
        })?;
        Ok(result)
    }
}

#[cfg(test)]
#[path = "server_registry_tests.rs"]
mod registry_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::Mutex as StdMutex,
    };
    struct Memory(StdMutex<Value>);
    impl Vault for Memory {
        fn read(&self) -> Result<Value, String> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn write(&self, value: &Value) -> Result<(), String> {
            *self.0.lock().unwrap() = value.clone();
            Ok(())
        }
    }
    #[tokio::test]
    async fn native_host_capture_paired_inventory_retains_empty_profile_bindings() {
        let pair = |server: &str| {
            server_keys::new_pair(&json!({"protocol":1,"origin":format!("https://{server}.tailnet.ts.net"),"serverId":server,"inviteId":"synthetic-invite","token":"synthetic","publicKey":"fixture","tailscaleUser":"user"}),"original-pair").unwrap()
        };
        let first = pair("first");
        let second = pair("second");
        let vault = Arc::new(Memory(StdMutex::new(
            json!({"drafts":{},"servers":{"second":second,"first":first}}),
        )));
        let client = Arc::new(RemoteClient::new(vault.clone()).unwrap());
        // Neither pairing has a Store or a logical record. Both still own a
        // persistent native profile and must appear in the host inventory.
        let bindings = client
            .migration_bindings()
            .expect("Every paired profile needs an exact host binding, including empty profiles");
        assert_eq!(
            bindings
                .iter()
                .map(|(server, _)| server.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        for (server, binding) in bindings {
            assert_eq!(
                binding.storage_key(),
                client.bind(&server).unwrap().storage_key()
            );
            let public = binding.public_identity();
            assert_eq!(public["origin"], format!("https://{server}.tailnet.ts.net"));
            assert_eq!(
                public
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                vec!["credentialId", "origin"]
            );
            assert!(public["credentialId"].is_string());
        }
        vault.0.lock().unwrap()["servers"]["broken"] = json!({"serverId":"broken"});
        assert!(
            client.migration_bindings().is_err(),
            "A malformed retained pairing cannot disappear from a partial inventory"
        );
    }
    #[tokio::test]
    async fn native_server_context_uses_paired_transport_and_refuses_changed_authority() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let cert =
            rcgen::generate_simple_self_signed(vec!["context.tailnet.ts.net".into()]).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let origin = format!("https://context.tailnet.ts.net:{}", address.port());
        let invitation = json!({"protocol":1,"origin":origin,"serverId":"remote","inviteId":"invite","token":"test","publicKey":"fixture","tailscaleUser":"user"});
        let credential = server_keys::new_pair(&invitation, "pair").unwrap();
        let root = reqwest::Certificate::from_der(cert.cert.der().as_ref()).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into()),
        )
        .unwrap();
        let server = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut stream = rustls::StreamOwned::new(
                rustls::ServerConnection::new(Arc::new(config)).unwrap(),
                socket,
            );
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                bytes.push(byte[0]);
            }
            let header = String::from_utf8(bytes).unwrap();
            assert!(header.starts_with("POST /api/messages HTTP/1.1\r\n"));
            let headers = header
                .lines()
                .skip(1)
                .filter_map(|line| line.split_once(':'))
                .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
                .collect::<HashMap<_, _>>();
            assert_eq!(headers["x-studio-server"], "remote");
            assert_eq!(headers["x-studio-request-id"], "original-message");
            assert_eq!(headers["x-studio-api-schema"], "exact-schema");
            assert_eq!(headers["x-canvas-workspace"], "a".repeat(32));
            assert!(!headers.contains_key("x-canvas-token"));
            let mut body = vec![0; headers["content-length"].parse::<usize>().unwrap()];
            stream.read_exact(&mut body).unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap(),
                json!({"id":"original-message","text":"synthetic"})
            );
            let reply = br#"{"id":"original-message","status":"accepted"}"#;
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",reply.len()).unwrap();
            stream.write_all(reply).unwrap();
            stream.flush().unwrap();
            listener
        });
        let vault = Arc::new(Memory(StdMutex::new(
            json!({"drafts":{},"servers":{"remote":credential}}),
        )));
        let mut transport = RemoteClient::new(vault.clone()).unwrap();
        transport.http = reqwest::Client::builder()
            .no_proxy()
            .resolve("context.tailnet.ts.net", address)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .tls_certs_only([root])
            .build()
            .unwrap();
        let transport = Arc::new(transport);
        let binding = transport.bind("remote").unwrap();
        let namespace = binding.storage_key();
        assert_eq!(namespace.len(), 64);
        assert!(!namespace.contains("remote"));
        let backend = crate::backend::Backend::from_remote(binding).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let client = crate::client::Client::open_with_backend(
            directory.path().join("owned"),
            backend,
            |_| {},
        )
        .unwrap();
        let request = || crate::protocol::Request {
            id: "original-message".into(),
            method: "api.request".into(),
            params: json!({"path":"/api/messages","requestId":"original-message","apiSchema":"exact-schema","workspaceId":"a".repeat(32),"body":{"id":"original-message","text":"synthetic"}}),
        };
        assert_eq!(
            client.request(request()).await.unwrap().result.unwrap()["id"],
            "original-message"
        );
        let listener = server.join().unwrap();
        vault.0.lock().unwrap()["servers"]["remote"]["origin"] =
            json!("https://changed.tailnet.ts.net");
        let refused = client.request(request()).await.unwrap();
        assert!(refused.error.unwrap().message.contains("identity changed"));
        assert!(transport.bind("missing").is_err());
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    #[tokio::test]
    async fn one_server_cannot_hold_the_slots_of_another_server() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("address").port();
        let pair = |server: &str| {
            let invitation = json!({"protocol":1,"origin":format!("https://{server}.tailnet.ts.net:{port}"),"serverId":server,"inviteId":"invite","token":"test","publicKey":"fixture","tailscaleUser":"user"});
            server_keys::new_pair(&invitation, "pair").unwrap()
        };
        let (busy, other) = (pair("busy"), pair("other"));
        let request = |server: &str, credential: &Value| json!({"serverId":server,"credentialId":credential["clientId"],"url":format!("https://{server}.tailnet.ts.net:{port}/api/test"),"method":"GET","base64":"","headers":[]});
        let vault = Arc::new(Memory(StdMutex::new(
            json!({"drafts":{},"servers":{"busy":busy.clone(),"other":other.clone()}}),
        )));
        let mut client = RemoteClient::new(vault).unwrap();
        let local = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        client.http = crate::tls::client_builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .resolve("busy.tailnet.ts.net", local)
            .resolve("other.tailnet.ts.net", local)
            .build()
            .unwrap();
        // Open requests to the busy server hold its whole share of both limits,
        // as connects to an unreachable server do until their timeout.
        let busy_slots = Arc::new(Semaphore::new(MAX_OPEN_RESPONSES_PER_SERVER));
        client
            .server_slots
            .lock()
            .unwrap()
            .insert("busy".into(), busy_slots.clone());
        let _held = (
            busy_slots
                .try_acquire_many_owned(MAX_OPEN_RESPONSES_PER_SERVER as u32)
                .unwrap(),
            client
                .response_slots
                .clone()
                .try_acquire_many_owned(MAX_OPEN_RESPONSES_PER_SERVER as u32)
                .unwrap(),
        );
        let refused = client
            .dispatch("servers.request", &request("busy", &busy))
            .await
            .unwrap_err();
        assert!(refused.contains("The request was not sent"), "{refused}");
        assert!(refused.contains("server=16/16"), "{refused}");
        assert_eq!(
            listener.accept().expect_err("no refused connection").kind(),
            std::io::ErrorKind::WouldBlock
        );
        let listener = tokio::net::TcpListener::from_std(listener).expect("listener");
        let server = tokio::spawn(async move {
            // Accept exactly the admitted connection, then refuse its TLS handshake.
            drop(listener.accept().await.expect("admitted connection"));
        });
        let admitted = client
            .dispatch("servers.request", &request("other", &other))
            .await
            .unwrap_err();
        assert!(!admitted.contains("The request was not sent"), "{admitted}");
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("accepted connection")
            .expect("server");
        assert_eq!(
            client.response_slots.available_permits(),
            MAX_OPEN_SERVER_RESPONSES - MAX_OPEN_RESPONSES_PER_SERVER
        );
    }
    #[tokio::test]
    async fn https_transport_rejects_untrusted_certificate_before_sending_request() {
        let cert =
            rcgen::generate_simple_self_signed(vec!["fixture.tailnet.ts.net".into()]).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let origin = format!("https://fixture.tailnet.ts.net:{}", address.port());
        let invitation = json!({"protocol":1,"origin":origin,"serverId":"server","inviteId":"invite","token":"test","publicKey":"fixture","tailscaleUser":"user"});
        let credential = server_keys::new_pair(&invitation, "pair").unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into()),
        )
        .unwrap();
        let server = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut stream = rustls::StreamOwned::new(
                rustls::ServerConnection::new(Arc::new(config)).unwrap(),
                socket,
            );
            // Certificate rejection must happen before any signed HTTP bytes arrive.
            assert!(stream.read(&mut [0]).is_err());
            listener
        });
        let vault = Arc::new(Memory(StdMutex::new(
            json!({"drafts":{},"servers":{"server":credential}}),
        )));
        let mut client = RemoteClient::new(vault).unwrap();
        client.http = crate::tls::client_builder()
            .no_proxy()
            .resolve("fixture.tailnet.ts.net", address)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .unwrap();
        let params = json!({"serverId":"server","credentialId":credential["clientId"],"url":origin+"/api/test","method":"POST","base64":STANDARD.encode(b"null"),"requestId":"untrusted","headers":[]});
        assert!(client.dispatch("servers.request", &params).await.is_err());
        let listener = server.join().unwrap();
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "failed TLS requests must not retry"
        );
    }
    #[tokio::test]
    async fn https_transport_signs_exact_bytes_and_streams_without_forwarding_local_auth() {
        let cert =
            rcgen::generate_simple_self_signed(vec!["fixture.tailnet.ts.net".into()]).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let origin = format!("https://fixture.tailnet.ts.net:{}", address.port());
        let invitation = json!({"protocol":1,"origin":origin,"serverId":"server","inviteId":"invite","token":"test","publicKey":"fixture","tailscaleUser":"user"});
        let credential = server_keys::new_pair(&invitation, "pair").unwrap();
        let expected = credential.clone();
        let root = reqwest::Certificate::from_der(cert.cert.der().as_ref()).unwrap();
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.cert.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into()),
            )
            .unwrap();
        let server = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut stream = rustls::StreamOwned::new(
                rustls::ServerConnection::new(Arc::new(config)).unwrap(),
                socket,
            );
            let mut bytes = Vec::new();
            let mut byte = [0];
            while !bytes.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                bytes.push(byte[0]);
            }
            let header = String::from_utf8(bytes).unwrap();
            let headers = header
                .lines()
                .skip(1)
                .filter_map(|line| line.split_once(':'))
                .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
                .collect::<HashMap<_, _>>();
            let length = headers["content-length"].parse::<usize>().unwrap();
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            assert_eq!(body, b"null");
            assert!(!headers.contains_key("x-canvas-token"));
            assert!(!headers.contains_key("authorization"));
            assert!(!headers.contains_key("cookie"));
            assert_eq!(headers["x-studio-request-id"], "original");
            let nonce = &headers["x-studio-nonce"];
            assert_eq!(nonce.len(), 32);
            assert!(
                nonce
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
            );

            {
                // Verify the wire request with the public key; never sign the expectation
                // with the implementation under test. The body digest is the fixed null vector.
                let public = expected["publicKey"]
                    .as_str()
                    .unwrap()
                    .lines()
                    .filter(|line| !line.starts_with("-----"))
                    .collect::<String>();
                let public = STANDARD.decode(public).unwrap();
                assert_eq!(public.len(), 44);
                let message = format!(
                    "studio-multi-server-v1\nPOST\n/api/test?q=1\nserver\n{}\n{}\n{}\noriginal\n74234e98afe7498fb5daf1f36ac2d78acc339464f950703b8c019892f982b90b",
                    expected["clientId"].as_str().unwrap(),
                    headers["x-studio-timestamp"],
                    headers["x-studio-nonce"]
                );
                ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &public[12..])
                    .verify(
                        message.as_bytes(),
                        &STANDARD.decode(&headers["x-studio-signature"]).unwrap(),
                    )
                    .unwrap();
            };
            stream.write_all(b"HTTP/1.1 409 Conflict\r\nContent-Length: 3\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\na\0\xff").unwrap();
            stream.flush().unwrap();
        });
        let vault = Arc::new(Memory(StdMutex::new(
            json!({"drafts":{},"servers":{"server":credential}}),
        )));
        let mut client = RemoteClient::new(vault).unwrap();
        client.http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .resolve("fixture.tailnet.ts.net", address)
            .tls_certs_only([root])
            .build()
            .unwrap();
        let params = json!({"serverId":"server","credentialId":credential["clientId"],"url":origin.clone()+"/api/test?q=1","method":"POST","base64":STANDARD.encode(b"null"),"requestId":"original","headers":[["X-Canvas-Token","secret"],["Authorization","secret"],["Cookie","secret"]]});
        let mut wrong = params.clone();
        wrong["url"] = json!("https://other.tailnet.ts.net/api/test");
        assert!(client.dispatch("servers.request", &wrong).await.is_err());
        // Saturation rejects before the fixture receives any bytes. Releasing
        // the reservation lets the same original request execute exactly once.
        let reserved = client
            .response_slots
            .clone()
            .try_acquire_many_owned(MAX_OPEN_SERVER_RESPONSES as u32)
            .unwrap();
        assert!(
            client
                .dispatch("servers.request", &params)
                .await
                .unwrap_err()
                .contains("The request was not sent")
        );
        drop(reserved);
        let response = client.dispatch("servers.request", &params).await.unwrap();
        assert_eq!(response["status"], 409);
        let stream = json!({"stream":response["stream"]});
        let mut bytes = Vec::new();
        loop {
            let chunk = client.dispatch("servers.read", &stream).await.unwrap();
            if chunk["done"] == true {
                break;
            }
            bytes.extend(STANDARD.decode(chunk["base64"].as_str().unwrap()).unwrap());
        }
        assert_eq!(bytes, b"a\0\xff");
        assert_eq!(
            client.response_slots.available_permits(),
            MAX_OPEN_SERVER_RESPONSES
        );
        server.join().unwrap();
        assert!(client.dispatch("servers.read", &stream).await.is_err());
    }
}
