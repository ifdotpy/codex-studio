//! Authenticated remote transport. The renderer selects an existing credential,
//! never a private key or an arbitrary network authority.
use crate::server_keys::{self, Credential, Group, TextField, Vault, field};
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

#[derive(Default, serde::Deserialize)]
struct DiagnosticCode<'a> {
    #[serde(borrow)]
    code: Option<&'a str>,
}
#[derive(Default, serde::Deserialize)]
struct DiagnosticError<'a> {
    #[serde(borrow)]
    error: Option<&'a str>,
}
fn rejection_kind(value: &Value) -> &str {
    use serde::Deserialize;
    let (code, error) = if value.is_object() {
        (
            DiagnosticCode::deserialize(value).unwrap_or_default(),
            DiagnosticError::deserialize(value).unwrap_or_default(),
        )
    } else {
        (DiagnosticCode::default(), DiagnosticError::default())
    };
    match code.code {
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
        _ if error.error == Some("Invalid request") => "invalid-request",
        _ => "other-rejection",
    }
}

// These fields are the complete pairing ingress. Identity evidence remains JSON:
// an existing draft can be retried with values that predate string validation.
#[derive(Default)]
struct PairIngress {
    code: Option<String>,
    protocol: Option<u64>,
    paired: Option<bool>,
    server_id: Value,
    client_id: Value,
    origin: Value,
    public_key: Value,
    tailscale_user: Value,
    label: Option<String>,
}

impl<'de> serde::Deserialize<'de> for PairIngress {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = PairIngress;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a pairing reply object")
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<PairIngress, M::Error> {
                let mut receipt = PairIngress::default();
                while let Some(name) = map.next_key::<String>()? {
                    match name.as_str() {
                        "code" => {
                            receipt.code = map.next_value::<Value>()?.as_str().map(str::to_owned)
                        }
                        "protocol" => receipt.protocol = map.next_value::<Value>()?.as_u64(),
                        "paired" => receipt.paired = map.next_value::<Value>()?.as_bool(),
                        "serverId" => receipt.server_id = map.next_value()?,
                        "clientId" => receipt.client_id = map.next_value()?,
                        "origin" => receipt.origin = map.next_value()?,
                        "publicKey" => receipt.public_key = map.next_value()?,
                        "tailscaleUser" => receipt.tailscale_user = map.next_value()?,
                        "label" => {
                            receipt.label = map.next_value::<Value>()?.as_str().map(str::to_owned)
                        }
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(receipt)
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

struct PairReceipt {
    server_id: String,
    label: String,
    public_key: Value,
    tailscale_user: Value,
}

enum PairReply {
    InviteUnavailable,
    Accepted(PairReceipt),
}

fn pair_reply(
    bytes: &[u8],
    status: reqwest::StatusCode,
    invitation: &Value,
    draft: &Credential,
    origin: &str,
) -> Result<PairReply, String> {
    // Parse JSON syntax independently from HTTP status; even a refusal with
    // invalid JSON has always reported an invalid reply.
    let raw: Box<serde_json::value::RawValue> = serde_json::from_slice(bytes)
        .map_err(|_| "Invalid pairing reply. Retry the same invitation.")?;
    let ingress: Option<PairIngress> = serde_json::from_str(raw.get()).ok();
    if status.as_u16() == 403
        && ingress.as_ref().and_then(|reply| reply.code.as_deref()) == Some("invite_unavailable")
    {
        return Ok(PairReply::InviteUnavailable);
    }
    if !status.is_success() {
        return Err("Pairing failed. Retry the same invitation.".into());
    }
    let receipt = ingress.ok_or("The pairing reply does not match the invitation.")?;
    if receipt.protocol
        != Some(studio_runtime_schema::multi_server_protocol::ACCESS_PROTOCOL_VERSION)
        || receipt.paired != Some(true)
        || receipt.server_id != invitation["serverId"]
        || !draft
            .object()
            .is_some_and(|object| object.client_id.equals(&receipt.client_id))
        || server_keys::origin(server_keys::field_value(&receipt.origin, "origin")?)? != origin
        || receipt.public_key != invitation["publicKey"]
        || receipt.tailscale_user != invitation["tailscaleUser"]
    {
        return Err("The pairing reply does not match the invitation.".into());
    }
    let server = server_keys::field_value(&receipt.server_id, "serverId")?.to_owned();
    Ok(PairReply::Accepted(PairReceipt {
        label: receipt.label.unwrap_or_else(|| server.clone()),
        server_id: server,
        public_key: receipt.public_key,
        tailscale_user: receipt.tailscale_user,
    }))
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
}
impl RemoteClient {
    pub fn new(vault: Arc<dyn Vault>) -> Result<Self, String> {
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
        })
    }
    async fn signed(
        &self,
        credential: &Credential,
        method: &str,
        url: &str,
        headers: &Value,
        body: Vec<u8>,
        request_id: &str,
    ) -> Result<reqwest::Response, String> {
        let url = reqwest::Url::parse(url).map_err(|_| "Invalid server URL.")?;
        if url.origin().ascii_serialization() != server_keys::origin(credential.field("origin")?)?
            || !url.path().starts_with("/api/")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || !matches!(method, "GET" | "POST" | "HEAD")
        {
            return Err("The request does not belong to this server API.".into());
        }
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
            .header("x-studio-client", credential.field("clientId")?)
            .header("x-studio-server", credential.field("serverId")?)
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
                let missing = Credential::Other(Value::Null);
                let credential = state
                    .group("servers")
                    .and_then(Group::entries)
                    .and_then(|v| v.get(server))
                    .unwrap_or(&missing);
                if credential.field("clientId")? != field(p, "credentialId")? {
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
                            let code = rejection_kind(&value);
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
                let request_id = field(p, "requestId")?;
                let missing = Credential::Other(Value::Null);
                let draft = state
                    .group("drafts")
                    .and_then(Group::entries)
                    .and_then(|v| v.get(request_id))
                    .unwrap_or(&missing);
                if draft.is_null() {
                    return Ok(json!(false));
                }
                if draft.field("serverId")? != field(p, "serverId")?
                    || draft.field("origin")? != server_keys::origin(field(p, "origin")?)?
                    || p.get("inviteId")
                        .is_some_and(|v| !draft.object().is_some_and(|d| d.invite_id.equals(v)))
                {
                    return Err("The saved key belongs to another pairing attempt.".into());
                }
                let object = draft.object().expect("validated draft");
                Ok(json!(
                    object.receipt.as_ref().is_some_and(|v| !v.is_null())
                        || (object.rejected.as_ref() != Some(&json!(true))
                            && matches!(object.body, TextField::Text(_)))
                ))
            }

            "servers.forget" => {
                let _guard = self.mutation.lock().await;
                let mut state = self.vault.read()?;
                let server = field(p, "serverId")?;
                state
                    .group_mut("servers")
                    .and_then(Group::entries_mut)
                    .ok_or("Invalid server key store.")?
                    .remove(server);
                state
                    .group_mut("drafts")
                    .and_then(Group::entries_mut)
                    .ok_or("Invalid server key store.")?
                    .retain(|_, v| {
                        !v.object()
                            .is_some_and(|d| d.server_id.equals(&json!(server)))
                    });
                self.vault.write(&state)?;
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
        let origin = server_keys::origin(field(p, "origin")?)?;
        if server_keys::origin(field(invitation, "origin")?)? != origin {
            return Err("The invitation belongs to another server.".into());
        }
        let mut state = self.vault.read()?;
        let missing = Credential::Other(Value::Null);
        if state
            .group("drafts")
            .and_then(Group::entries)
            .and_then(|v| v.get(id))
            .unwrap_or(&missing)
            .is_null()
        {
            let draft = server_keys::new_pair(invitation, id)?;
            state
                .group_mut("drafts")
                .and_then(Group::entries_mut)
                .ok_or("Invalid server key store.")?
                .insert(id.to_owned(), draft);
            self.vault.write(&state)?;
        }
        let draft = state
            .group("drafts")
            .and_then(Group::entries)
            .and_then(|v| v.get(id))
            .unwrap_or(&missing)
            .clone();
        if !draft.object().is_some_and(|d| {
            d.server_id.equals(&invitation["serverId"])
                && d.invite_id.equals(&invitation["inviteId"])
                && d.origin.equals(&json!(origin))
        }) {
            return Err("This request belongs to another pairing attempt.".into());
        }
        if let Some(receipt) = draft
            .object()
            .and_then(|d| d.receipt.as_ref())
            .filter(|v| !v.is_null())
        {
            return Ok(receipt.clone());
        }
        let mut response = self
            .signed(
                &draft,
                "POST",
                &(origin.clone() + "/api/multi-server/v1/pair"),
                &json!([["content-type", "application/json"]]),
                draft.field("body")?.as_bytes().to_vec(),
                id,
            )
            .await?;
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
        let receipt = match pair_reply(&bytes, status, invitation, &draft, &origin)? {
            PairReply::InviteUnavailable => {
                state
                    .group_mut("drafts")
                    .and_then(Group::entries_mut)
                    .and_then(|v| v.get_mut(id))
                    .and_then(Credential::object_mut)
                    .expect("validated draft")
                    .rejected = Some(json!(true));
                self.vault.write(&state)?;
                return Ok(json!({"notApplied":"invite_unavailable"}));
            }
            PairReply::Accepted(receipt) => receipt,
        };
        let server = &receipt.server_id;
        let result = json!({"id":server,"origin":origin,"label":receipt.label,"credentialId":draft.field("clientId")?});
        let mut credential = draft.clone();
        let fields = credential.object_mut().expect("validated draft");
        fields.body = TextField::Missing;
        fields
            .extra
            .insert("targetPublicKey".into(), receipt.public_key);
        fields
            .extra
            .insert("tailscaleUser".into(), receipt.tailscale_user);
        state
            .group_mut("servers")
            .and_then(Group::entries_mut)
            .ok_or("Invalid server key store.")?
            .insert(server.clone(), credential);
        let fields = state
            .group_mut("drafts")
            .and_then(Group::entries_mut)
            .and_then(|v| v.get_mut(id))
            .and_then(Credential::object_mut)
            .expect("validated draft");
        fields.private_key = TextField::Missing;
        fields.body = TextField::Missing;
        fields.receipt = Some(result.clone());
        self.vault.write(&state)?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::Mutex as StdMutex,
    };
    struct Memory(StdMutex<Value>);
    #[test]
    fn rejection_diagnostics_keep_tolerant_field_precedence() {
        for (raw, expected) in [
            (
                r#"{"code":"bad","code":"invalid_nonce","error":"Invalid request"}"#,
                "invalid_nonce",
            ),
            (
                r#"{"code":null,"error":"Invalid request"}"#,
                "invalid-request",
            ),
            (
                r#"{"code":{},"error":"Invalid request"}"#,
                "invalid-request",
            ),
            (r#"{"code":"unknown","error":null}"#, "other-rejection"),
            (r#""Invalid request""#, "other-rejection"),
        ] {
            let value: Value = serde_json::from_str(raw).unwrap();
            assert_eq!(rejection_kind(&value), expected);
        }
    }
    impl Vault for Memory {
        fn read(&self) -> Result<server_keys::VaultState, String> {
            Ok(server_keys::VaultState::from_value(
                self.0.lock().unwrap().clone(),
            ))
        }
        fn write(&self, value: &server_keys::VaultState) -> Result<(), String> {
            *self.0.lock().unwrap() = value.clone().into_value();
            Ok(())
        }
    }
    #[test]
    fn pairing_receipt_boundary_preserves_refusal_and_retry_validation() {
        let invitation = json!({"serverId":"server", "publicKey":"key", "tailscaleUser":"user"});
        let draft = Credential::from_value(json!({"clientId":"client"}));
        let origin = "https://fixture.tailnet.ts.net";
        let receipt = json!({"protocol":1,"paired":true,"serverId":"server","clientId":"client","origin":origin,"publicKey":"key","tailscaleUser":"user","label":null});
        let parse = |value: &Value, status| {
            pair_reply(
                value.to_string().as_bytes(),
                status,
                &invitation,
                &draft,
                origin,
            )
        };
        for _ in 0..2 {
            match parse(&receipt, reqwest::StatusCode::OK).unwrap() {
                PairReply::Accepted(receipt) => {
                    assert_eq!(receipt.server_id, "server");
                    assert_eq!(receipt.label, "server");
                    assert_eq!(receipt.public_key, json!("key"));
                }
                PairReply::InviteUnavailable => panic!("unexpected refusal"),
            }
        }
        assert!(matches!(
            parse(
                &json!({"code":"invite_unavailable"}),
                reqwest::StatusCode::FORBIDDEN
            ),
            Ok(PairReply::InviteUnavailable)
        ));
        assert!(
            parse(
                &json!({"code":"invite_unavailable"}),
                reqwest::StatusCode::CONFLICT
            )
            .err()
            .unwrap()
            .contains("Pairing failed")
        );
        let mut mismatch = receipt.clone();
        mismatch["clientId"] = json!("other");
        assert!(
            parse(&mismatch, reqwest::StatusCode::OK)
                .err()
                .unwrap()
                .contains("does not match")
        );
        assert!(
            pair_reply(
                b"{",
                reqwest::StatusCode::FORBIDDEN,
                &invitation,
                &draft,
                origin
            )
            .err()
            .unwrap()
            .contains("Invalid pairing reply")
        );
        assert_eq!(
            pair_reply(b"[]", reqwest::StatusCode::OK, &invitation, &draft, origin)
                .err()
                .unwrap(),
            "The pairing reply does not match the invitation."
        );
        assert_eq!(
            pair_reply(
                b"[]",
                reqwest::StatusCode::FORBIDDEN,
                &invitation,
                &draft,
                origin
            )
            .err()
            .unwrap(),
            "Pairing failed. Retry the same invitation."
        );
        let mut bad_origin = receipt.clone();
        bad_origin["origin"] = Value::Null;
        bad_origin["publicKey"] = json!("different");
        assert_eq!(
            parse(&bad_origin, reqwest::StatusCode::OK).err().unwrap(),
            "Missing or invalid origin."
        );
        let mut nullable = receipt;
        nullable["publicKey"] = Value::Null;
        nullable["tailscaleUser"] = Value::Null;
        let invitation = json!({"serverId":"server"});
        assert!(matches!(
            pair_reply(
                nullable.to_string().as_bytes(),
                reqwest::StatusCode::OK,
                &invitation,
                &draft,
                origin
            ),
            Ok(PairReply::Accepted(PairReceipt {
                public_key: Value::Null,
                tailscale_user: Value::Null,
                ..
            }))
        ));
    }
    #[test]
    fn pairing_ingress_matches_json_field_semantics() {
        // Compare with the former Value indexing path, including duplicate keys.
        for bytes in [
            br#"{}"#.as_slice(),
            br#"{"protocol":1,"protocol":"1","paired":true,"paired":null,"code":"invite_unavailable","code":false}"#,
            br#"{"protocol":2,"protocol":1,"paired":false,"paired":true,"serverId":7,"clientId":[1],"origin":null,"publicKey":{"x":1},"tailscaleUser":[true],"label":[],"label":"last"}"#,
        ] {
            let old: Value = serde_json::from_slice(bytes).unwrap();
            let ingress: PairIngress = serde_json::from_slice(bytes).unwrap();
            assert_eq!(ingress.code.as_deref(), old["code"].as_str());
            assert_eq!(ingress.protocol, old["protocol"].as_u64());
            assert_eq!(ingress.paired, old["paired"].as_bool());
            assert_eq!(ingress.server_id, old["serverId"]);
            assert_eq!(ingress.client_id, old["clientId"]);
            assert_eq!(ingress.origin, old["origin"]);
            assert_eq!(ingress.public_key, old["publicKey"]);
            assert_eq!(ingress.tailscale_user, old["tailscaleUser"]);
            assert_eq!(ingress.label.as_deref(), old["label"].as_str());
        }
        let invitation = json!({});
        let draft = Credential::from_value(json!({}));
        let duplicate = br#"{"code":"other","code":"invite_unavailable"}"#;
        assert!(matches!(
            pair_reply(
                duplicate,
                reqwest::StatusCode::FORBIDDEN,
                &invitation,
                &draft,
                ""
            ),
            Ok(PairReply::InviteUnavailable)
        ));
        let duplicate = br#"{"code":"invite_unavailable","code":"other"}"#;
        assert_eq!(
            pair_reply(
                duplicate,
                reqwest::StatusCode::FORBIDDEN,
                &invitation,
                &draft,
                ""
            )
            .err()
            .unwrap(),
            "Pairing failed. Retry the same invitation."
        );
    }
    #[tokio::test]
    async fn partial_drafts_and_forget_keep_access_time_behavior() {
        let vault = Arc::new(Memory(StdMutex::new(
            json!({"drafts":{"null":null,"bad":42,"done":{"origin":"https://fixture.tailnet.ts.net","serverId":"server","inviteId":"invite","receipt":{"ok":true},"future":1},"keep":{"serverId":"elsewhere"}},"servers":{"server":{"opaque":true},"other":null},"future":[1]}),
        )));
        let client = RemoteClient::new(vault.clone()).unwrap();
        let attempt = |id| json!({"requestId":id,"serverId":"server","origin":"https://fixture.tailnet.ts.net","inviteId":"invite"});
        assert_eq!(
            client
                .dispatch("servers.pairAttempt", &attempt("null"))
                .await
                .unwrap(),
            false
        );
        assert!(
            client
                .dispatch("servers.pairAttempt", &attempt("bad"))
                .await
                .is_err()
        );
        assert_eq!(
            client
                .dispatch("servers.pairAttempt", &attempt("done"))
                .await
                .unwrap(),
            true
        );
        client
            .dispatch("servers.forget", &json!({"serverId":"server"}))
            .await
            .unwrap();
        assert_eq!(
            vault.read().unwrap().into_value(),
            json!({"drafts":{"null":null,"bad":42,"keep":{"serverId":"elsewhere"}},"servers":{"other":null},"future":[1]})
        );
    }

    #[tokio::test]
    async fn pairing_retry_reuses_draft_after_mismatched_receipt() {
        let cert =
            rcgen::generate_simple_self_signed(vec!["fixture.tailnet.ts.net".into()]).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let origin = format!("https://fixture.tailnet.ts.net:{}", address.port());
        let invitation = json!({"protocol":1,"origin":origin,"serverId":"server","inviteId":"invite","token":"test","publicKey":"fixture","tailscaleUser":"user"});
        let root = reqwest::Certificate::from_der(cert.cert.der().as_ref()).unwrap();
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.cert.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into()),
            )
            .unwrap();
        let server_origin = origin.clone();
        let server = std::thread::spawn(move || {
            let mut first_body = Vec::new();
            for attempt in 0..2 {
                let (socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut stream = rustls::StreamOwned::new(
                    rustls::ServerConnection::new(Arc::new(config.clone())).unwrap(),
                    socket,
                );
                let mut header = Vec::new();
                let mut byte = [0];
                while !header.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    header.push(byte[0]);
                }
                let header = String::from_utf8(header).unwrap();
                let headers = header
                    .lines()
                    .skip(1)
                    .filter_map(|line| line.split_once(':'))
                    .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
                    .collect::<HashMap<_, _>>();
                assert_eq!(headers["x-studio-request-id"], "pair-request");
                let mut body = vec![0; headers["content-length"].parse().unwrap()];
                stream.read_exact(&mut body).unwrap();
                if attempt == 0 {
                    first_body = body.clone();
                } else {
                    assert_eq!(body, first_body);
                }
                use sha2::Digest;
                let digest = sha2::Sha256::digest(&body)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                let body: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["requestId"], "pair-request");
                assert_eq!(
                    headers["x-studio-client"],
                    body["clientId"].as_str().unwrap()
                );
                let public = body["publicKey"]
                    .as_str()
                    .unwrap()
                    .lines()
                    .filter(|line| !line.starts_with("-----"))
                    .collect::<String>();
                let public = STANDARD.decode(public).unwrap();
                let message = format!(
                    "studio-multi-server-v1\nPOST\n/api/multi-server/v1/pair\nserver\n{}\n{}\n{}\npair-request\n{}",
                    headers["x-studio-client"],
                    headers["x-studio-timestamp"],
                    headers["x-studio-nonce"],
                    digest
                );
                ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &public[12..])
                    .verify(
                        message.as_bytes(),
                        &STANDARD.decode(&headers["x-studio-signature"]).unwrap(),
                    )
                    .unwrap();
                let client_id = if attempt == 0 {
                    json!("wrong")
                } else {
                    body["clientId"].clone()
                };
                let reply = json!({"protocol":1,"paired":true,"serverId":"server","clientId":client_id,"origin":server_origin,"publicKey":{"historic":1},"tailscaleUser":["historic"]}).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    reply.len(),
                    reply
                )
                .unwrap();
                stream.flush().unwrap();
            }
        });
        let vault = Arc::new(Memory(StdMutex::new(json!({"drafts":{},"servers":{}}))));
        let mut client = RemoteClient::new(vault.clone()).unwrap();
        client.http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .resolve("fixture.tailnet.ts.net", address)
            .tls_certs_only([root])
            .build()
            .unwrap();
        let params = json!({"requestId":"pair-request","origin":origin,"invitation":invitation});
        assert!(
            client
                .dispatch("servers.pair", &params)
                .await
                .unwrap_err()
                .contains("does not match")
        );
        let draft = vault.read().unwrap().into_value()["drafts"]["pair-request"].clone();
        let mut retry = params.clone();
        retry["invitation"]["publicKey"] = json!({"historic":1});
        retry["invitation"]["tailscaleUser"] = json!(["historic"]);
        let result = client.dispatch("servers.pair", &retry).await.unwrap();
        assert_eq!(result["credentialId"], draft["clientId"]);
        assert_eq!(result["label"], "server");
        assert_eq!(
            client.dispatch("servers.pair", &retry).await.unwrap(),
            result
        );
        let state = vault.read().unwrap().into_value();
        assert_eq!(state["drafts"]["pair-request"]["receipt"], result);
        assert_eq!(
            state["servers"]["server"]["targetPublicKey"],
            json!({"historic":1})
        );
        assert_eq!(
            state["servers"]["server"]["tailscaleUser"],
            json!(["historic"])
        );
        assert!(state["drafts"]["pair-request"].get("privateKey").is_none());
        assert!(state["drafts"]["pair-request"].get("body").is_none());
        server.join().unwrap();
    }
    #[tokio::test]
    async fn one_server_cannot_hold_the_slots_of_another_server() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("address").port();
        let pair = |server: &str| {
            let invitation = json!({"protocol":1,"origin":format!("https://{server}.tailnet.ts.net:{port}"),"serverId":server,"inviteId":"invite","token":"test","publicKey":"fixture","tailscaleUser":"user"});
            server_keys::new_pair(&invitation, "pair")
                .unwrap()
                .into_value()
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
        let credential = server_keys::new_pair(&invitation, "pair")
            .unwrap()
            .into_value();
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
        for response_body in [
            b"a\0\xff".to_vec(),
            br#"{"code":"bad","code":"invalid_nonce","error":"Invalid request"}"#.to_vec(),
        ] {
            let cert =
                rcgen::generate_simple_self_signed(vec!["fixture.tailnet.ts.net".into()]).unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let origin = format!("https://fixture.tailnet.ts.net:{}", address.port());
            let invitation = json!({"protocol":1,"origin":origin,"serverId":"server","inviteId":"invite","token":"test","publicKey":"fixture","tailscaleUser":"user"});
            let credential = server_keys::new_pair(&invitation, "pair")
                .unwrap()
                .into_value();
            let expected = credential.clone();
            let expected_response = response_body.clone();
            let root = reqwest::Certificate::from_der(cert.cert.der().as_ref()).unwrap();
            let config = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![cert.cert.der().clone()],
                    rustls::pki_types::PrivateKeyDer::Pkcs8(
                        cert.signing_key.serialize_der().into(),
                    ),
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
                    ring::signature::UnparsedPublicKey::new(
                        &ring::signature::ED25519,
                        &public[12..],
                    )
                    .verify(
                        message.as_bytes(),
                        &STANDARD.decode(&headers["x-studio-signature"]).unwrap(),
                    )
                    .unwrap();
                };
                stream.write_all(format!("HTTP/1.1 409 Conflict\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n", response_body.len()).as_bytes()).unwrap();
                stream.write_all(&response_body).unwrap();
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
            assert_eq!(bytes, expected_response);
            assert_eq!(
                client.response_slots.available_permits(),
                MAX_OPEN_SERVER_RESPONSES
            );
            server.join().unwrap();
            assert!(client.dispatch("servers.read", &stream).await.is_err());
        }
    }
}
