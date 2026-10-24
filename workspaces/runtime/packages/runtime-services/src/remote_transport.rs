//! Signed outbound exchange primitives for multi-server and federation peers.
//!
//! The two protocols have different signing inputs and remain separate here.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use rustls_pki_types::{PrivatePkcs8KeyDer, SubjectPublicKeyInfoDer, pem::PemObject};
use serde_json::{Map, Value};
use spki::{ObjectIdentifier, SubjectPublicKeyInfoRef};
use std::collections::HashMap;
use std::fmt;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use thiserror::Error;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

use crate::event_write::python_json_compact;
pub use crate::multi_server_transport_limits::{MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES};
use studio_runtime_schema::multi_server_protocol::MULTI_SERVER_SIGNING_DOMAIN;
use studio_runtime_store::{RemoteTransportRepository, RuntimeStore, StoreError, sha256_hex};

pub const CLOCK_SKEW_SECONDS: i64 = 300;
pub const MAX_IDENTITY_BYTES: usize = 16 * 1024;

const FEDERATION_DOMAIN: &str = "studio-federation-v1";
const FEDERATION_MESSAGE_DOMAIN: &str = "studio-federation-message-v1\n";
const FEDERATION_RESPONSE_DOMAIN: &str = "studio-federation-response-v1\n";
const ED25519_OID: &str = "1.3.101.112";

#[derive(Debug, Error)]
pub enum RemoteTransportError {
    #[error("remote transport input is invalid")]
    InvalidInput,
    #[error("remote identity is invalid")]
    InvalidIdentity,
    #[error("remote identity storage is unavailable or unsafe")]
    IdentityStorage,
    #[error("remote identity loading is unsupported on Windows")]
    UnsupportedPlatform,
    #[error("invalid stored object {table}/{key}")]
    InvalidStoredObject { table: &'static str, key: String },
    #[error("remote signature is invalid")]
    InvalidSignature,
    #[error("remote request nonce is invalid or outside the replay window")]
    InvalidNonce,
    #[error("remote request was replayed or rate limited")]
    Replay,
    #[error("remote peer exchange failed; preserve the request identity")]
    ExchangeUnknown,
    #[error("remote peer response is invalid")]
    InvalidResponse,
    #[error("remote peer refused the signed request")]
    PeerRejected,
    #[error("remote request identity was reused with different content")]
    RequestConflict,
    #[error("remote peer response exceeds the size limit")]
    ResponseTooLarge,
    #[error("remote transport state operation failed")]
    Store(#[from] StoreError),
}

pub type RemoteTransportResult<T> = Result<T, RemoteTransportError>;

/// A loaded Ed25519 signing identity. Its `Debug` representation never includes key data.
pub struct RemoteIdentity {
    id: String,
    label: String,
    key_pair: Ed25519KeyPair,
    public_key_pem: String,
}

impl fmt::Debug for RemoteIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteIdentity")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("key_pair", &"[secret key elided]")
            .field("public_key_pem", &"[public key elided]")
            .finish()
    }
}

impl RemoteIdentity {
    fn from_key_fields(
        id: String,
        label: String,
        private_key_pem: &[u8],
        public_key_pem: String,
    ) -> RemoteTransportResult<Self> {
        if validate_identifier(&id).is_err() || label.is_empty() || label.chars().count() > 80 {
            return Err(RemoteTransportError::InvalidIdentity);
        }
        let key_der = PrivatePkcs8KeyDer::from_pem_slice(private_key_pem)
            .map_err(|_| RemoteTransportError::InvalidIdentity)?;
        let key_pair = Ed25519KeyPair::from_pkcs8(key_der.secret_pkcs8_der())
            .or_else(|_| {
                // Node/OpenSSL can store Ed25519 PKCS#8 v1 without an embedded public key;
                // the separately stored SPKI key below must match the derived public key.
                Ed25519KeyPair::from_pkcs8_maybe_unchecked(key_der.secret_pkcs8_der())
            })
            .map_err(|_| RemoteTransportError::InvalidIdentity)?;
        let public_key = ed25519_public_key(&public_key_pem)?;
        if key_pair.public_key().as_ref() != public_key.as_slice() {
            return Err(RemoteTransportError::InvalidIdentity);
        }
        Ok(Self {
            id,
            label,
            key_pair,
            public_key_pem,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn public_key_pem(&self) -> &str {
        &self.public_key_pem
    }

    pub fn sign(&self, message: &[u8]) -> String {
        STANDARD.encode(self.key_pair.sign(message).as_ref())
    }
}

/// Read Python's existing `multi-server/identity.json` without creating or replacing it.
pub fn load_multi_server_identity(state_root: &Path) -> RemoteTransportResult<RemoteIdentity> {
    read_multi_server_identity(state_root)?.signing_identity()
}

/// Serialize the existing peer-link child configuration. The returned JSON
/// line contains the private key and must be written only to the child's stdin.
pub fn peer_link_config_line(
    state_root: &Path,
    listen: &str,
    peers: &[String],
    backend_port: u16,
    forwarded_host: &str,
    socket: &Path,
) -> RemoteTransportResult<String> {
    if listen.parse::<std::net::SocketAddr>().is_err()
        || peers.len() > 1024
        || forwarded_host.is_empty()
        || forwarded_host.len() > 255
        || socket.as_os_str().is_empty()
    {
        return Err(RemoteTransportError::InvalidInput);
    }
    for peer in peers {
        ed25519_public_key(peer)?;
    }
    let identity = read_multi_server_identity(state_root)?;
    serde_json::to_string(&serde_json::json!({
        "listen": listen,
        "privateKey": identity.private_key,
        "publicKey": identity.public_key,
        "peers": peers,
        "backendPort": backend_port,
        "forwardedHost": forwarded_host,
        "socket": socket,
    }))
    .map_err(|_| RemoteTransportError::InvalidIdentity)
}

pub(crate) fn read_multi_server_identity(
    state_root: &Path,
) -> RemoteTransportResult<StoredMultiServerIdentity> {
    #[cfg(windows)]
    {
        let _ = state_root;
        return Err(RemoteTransportError::UnsupportedPlatform);
    }
    #[cfg(not(windows))]
    {
        let directory = state_root.join("multi-server");
        let path = directory.join("identity.json");
        reject_symlink(&directory)?;
        let directory_metadata = std::fs::symlink_metadata(&directory)
            .map_err(|_| RemoteTransportError::IdentityStorage)?;
        if !directory_metadata.is_dir() {
            return Err(RemoteTransportError::IdentityStorage);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if directory_metadata.permissions().mode() & 0o077 != 0 {
                return Err(RemoteTransportError::IdentityStorage);
            }
        }

        reject_symlink(&path)?;
        let mut options = OpenOptions::new();
        options.read(true);
        set_no_follow(&mut options)?;
        let file = options
            .open(&path)
            .map_err(|_| RemoteTransportError::IdentityStorage)?;
        let metadata = file
            .metadata()
            .map_err(|_| RemoteTransportError::IdentityStorage)?;
        if !metadata.is_file() || metadata.len() > MAX_IDENTITY_BYTES as u64 {
            return Err(RemoteTransportError::IdentityStorage);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(RemoteTransportError::IdentityStorage);
            }
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        let mut bounded = file.take((MAX_IDENTITY_BYTES + 1) as u64);
        bounded
            .read_to_end(&mut bytes)
            .map_err(|_| RemoteTransportError::IdentityStorage)?;
        if bytes.len() > MAX_IDENTITY_BYTES {
            return Err(RemoteTransportError::IdentityStorage);
        }
        let record: StoredMultiServerIdentity = serde_json::from_slice(&bytes).map_err(|_| {
            RemoteTransportError::InvalidStoredObject {
                table: "multi-server/identity.json",
                key: "serverId".to_owned(),
            }
        })?;
        record.signing_identity()?;
        Ok(record)
    }
}

/// Load the Python federation identity row (`id='local'`) without changing it.
pub async fn load_federation_identity(
    store: &RuntimeStore,
) -> RemoteTransportResult<RemoteIdentity> {
    const TABLE: &str = "runtime_federation_identity";
    const KEY: &str = "local";
    let invalid = || RemoteTransportError::InvalidStoredObject {
        table: TABLE,
        key: KEY.to_owned(),
    };
    let Some(record) = store
        .read_async("runtime-services/remote_transport", |tx| {
            RemoteTransportRepository::federation_identity(tx)
        })
        .await
        .map_err(|_| invalid())?
    else {
        return Err(invalid());
    };
    let identity: StoredFederationIdentity =
        serde_json::from_str(&record).map_err(|_| invalid())?;
    RemoteIdentity::from_key_fields(
        identity.state_id,
        identity.label,
        identity.private_key.as_bytes(),
        identity.public_key,
    )
    .map_err(|_| invalid())
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredMultiServerIdentity {
    pub(crate) server_id: String,
    pub(crate) label: String,
    pub(crate) private_key: String,
    pub(crate) public_key: String,
}

impl StoredMultiServerIdentity {
    pub(crate) fn signing_identity(&self) -> RemoteTransportResult<RemoteIdentity> {
        RemoteIdentity::from_key_fields(
            self.server_id.clone(),
            self.label.clone(),
            self.private_key.as_bytes(),
            self.public_key.clone(),
        )
        .map_err(|_| RemoteTransportError::InvalidStoredObject {
            table: "multi-server/identity.json",
            key: "local".to_owned(),
        })
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredFederationIdentity {
    state_id: String,
    label: String,
    private_key: String,
    public_key: String,
}

/// Inputs for the Python multi-server signature byte format.
pub struct MultiServerSigningInput<'a> {
    pub method: &'a str,
    pub target: &'a str,
    pub server_id: &'a str,
    pub client_id: &'a str,
    pub timestamp: &'a str,
    pub nonce: &'a str,
    pub request_id: &'a str,
    pub body: &'a [u8],
}

/// Fields used to sign one outgoing multi-server request.
pub struct MultiServerRequest<'a> {
    pub server_id: &'a str,
    pub method: &'a str,
    pub url: &'a str,
    pub request_id: &'a str,
    pub timestamp: i64,
    pub nonce: &'a str,
    pub body: &'a [u8],
}

/// Python multi-server protocol: newline-delimited UTF-8 fields and a lowercase SHA-256 body hash.
pub fn multi_server_signing_bytes(input: MultiServerSigningInput<'_>) -> Vec<u8> {
    let method = input.method.to_ascii_uppercase();
    multi_server_signing_bytes_normalized(MultiServerSigningInput {
        method: &method,
        ..input
    })
}

fn multi_server_signing_bytes_normalized(input: MultiServerSigningInput<'_>) -> Vec<u8> {
    let digest = sha256_hex(input.body);
    format!(
        "{MULTI_SERVER_SIGNING_DOMAIN}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{digest}",
        input.method,
        input.target,
        input.server_id,
        input.client_id,
        input.timestamp,
        input.nonce,
        input.request_id,
    )
    .into_bytes()
}

/// Python multi-server's stable request journal fingerprint (method, path and raw body).
pub fn multi_server_request_fingerprint(
    method: &str,
    url: &str,
    body: &[u8],
) -> RemoteTransportResult<String> {
    let method = method.to_ascii_uppercase();
    multi_server_request_fingerprint_normalized(&method, url, body)
}

fn multi_server_request_fingerprint_normalized(
    method: &str,
    url: &str,
    body: &[u8],
) -> RemoteTransportResult<String> {
    let target = canonical_request_target(url)?;
    let mut input = Vec::with_capacity(method.len() + target.len() + body.len() + 2);
    input.extend_from_slice(method.as_bytes());
    input.push(b'\n');
    input.extend_from_slice(target.as_bytes());
    input.push(b'\n');
    input.extend_from_slice(body);
    Ok(sha256_hex(&input))
}

/// Python federation protocol: raw JSON bytes are signed with the exact method and path.
pub fn federation_signing_bytes(
    method: &str,
    path: &str,
    timestamp: i64,
    nonce: &str,
    body: &[u8],
) -> Vec<u8> {
    format!(
        "{FEDERATION_DOMAIN}\n{method}\n{path}\n{timestamp}\n{nonce}\n{}",
        sha256_hex(body)
    )
    .into_bytes()
}

/// Build the multi-server signature headers for one HTTPS request.
pub fn multi_server_signed_headers(
    identity: &RemoteIdentity,
    request: &MultiServerRequest<'_>,
    now: i64,
) -> RemoteTransportResult<Vec<(String, String)>> {
    let method = request.method.to_ascii_uppercase();
    multi_server_signed_headers_normalized(identity, request, &method, now)
}

fn multi_server_signed_headers_normalized(
    identity: &RemoteIdentity,
    request: &MultiServerRequest<'_>,
    method: &str,
    now: i64,
) -> RemoteTransportResult<Vec<(String, String)>> {
    validate_multi_server_nonce(request.nonce, request.timestamp, now)?;
    validate_identifier(request.server_id)?;
    validate_identifier(&identity.id)?;
    validate_identifier(request.request_id)?;
    let timestamp = request.timestamp.to_string();
    let target = canonical_request_target(request.url)?;
    let signature = identity.sign(&multi_server_signing_bytes_normalized(
        MultiServerSigningInput {
            method,
            target: &target,
            server_id: request.server_id,
            client_id: &identity.id,
            timestamp: &timestamp,
            nonce: request.nonce,
            request_id: request.request_id,
            body: request.body,
        },
    ));
    Ok(vec![
        ("X-Studio-Client".to_owned(), identity.id.clone()),
        ("X-Studio-Server".to_owned(), request.server_id.to_owned()),
        ("X-Studio-Timestamp".to_owned(), timestamp),
        ("X-Studio-Nonce".to_owned(), request.nonce.to_owned()),
        (
            "X-Studio-Request-Id".to_owned(),
            request.request_id.to_owned(),
        ),
        ("X-Studio-Signature".to_owned(), signature),
    ])
}

/// Build federation signature headers for one HTTPS request.
pub fn federation_signed_headers(
    identity: &RemoteIdentity,
    method: &str,
    url: &str,
    timestamp: i64,
    nonce: &str,
    body: &[u8],
    now: i64,
) -> RemoteTransportResult<Vec<(String, String)>> {
    validate_federation_nonce(nonce, timestamp, now)?;
    let path = canonical_request_target(url)?;
    let signature = identity.sign(&federation_signing_bytes(
        method, &path, timestamp, nonce, body,
    ));
    Ok(vec![
        ("X-Studio-Federation-State".to_owned(), identity.id.clone()),
        ("X-Studio-Federation-Time".to_owned(), timestamp.to_string()),
        ("X-Studio-Federation-Nonce".to_owned(), nonce.to_owned()),
        ("X-Studio-Federation-Signature".to_owned(), signature),
    ])
}

pub fn federation_message_signing_bytes(envelope: &Value) -> RemoteTransportResult<Vec<u8>> {
    let object = envelope
        .as_object()
        .ok_or(RemoteTransportError::InvalidInput)?;
    let unsigned = object
        .iter()
        .filter(|(key, _)| key.as_str() != "signature")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Map<_, _>>();
    let canonical = canonical_json(&Value::Object(unsigned))?;
    Ok(format!("{FEDERATION_MESSAGE_DOMAIN}{canonical}").into_bytes())
}

/// Canonical compact JSON compatible with `json.dumps(sort_keys=True, ensure_ascii=False)`.
pub fn canonical_json(value: &Value) -> RemoteTransportResult<String> {
    Ok(python_json_compact(value))
}

pub fn verify_signature(
    public_key_pem: &str,
    message: &[u8],
    signature_base64: &str,
) -> RemoteTransportResult<()> {
    let public_key = ed25519_public_key(public_key_pem)?;
    let signature = STANDARD
        .decode(signature_base64)
        .map_err(|_| RemoteTransportError::InvalidSignature)?;
    if signature.len() != 64 {
        return Err(RemoteTransportError::InvalidSignature);
    }
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(message, &signature)
        .map_err(|_| RemoteTransportError::InvalidSignature)
}

/// Check Python multi-server's optional response identity fields against the paired peer.
pub fn verify_multi_server_response(
    response_body: &[u8],
    expected_server_id: &str,
    expected_public_key: &str,
) -> RemoteTransportResult<Value> {
    ed25519_public_key(expected_public_key)?;
    let response = parse_exact_json(response_body)?;
    let object = response
        .as_object()
        .ok_or(RemoteTransportError::InvalidResponse)?;
    for identity in [
        object,
        object
            .get("identity")
            .and_then(Value::as_object)
            .unwrap_or(object),
    ] {
        if identity
            .get("serverId")
            .is_some_and(|server_id| server_id.as_str() != Some(expected_server_id))
            || identity
                .get("publicKey")
                .is_some_and(|public_key| public_key.as_str() != Some(expected_public_key))
        {
            return Err(RemoteTransportError::InvalidSignature);
        }
    }
    Ok(response)
}

fn ed25519_public_key(public_key_pem: &str) -> RemoteTransportResult<Vec<u8>> {
    if public_key_pem.len() > 4096
        || !public_key_pem.starts_with("-----BEGIN PUBLIC KEY-----")
        || !public_key_pem
            .trim_end()
            .ends_with("-----END PUBLIC KEY-----")
    {
        return Err(RemoteTransportError::InvalidIdentity);
    }
    let spki_der = SubjectPublicKeyInfoDer::from_pem_slice(public_key_pem.as_bytes())
        .map_err(|_| RemoteTransportError::InvalidIdentity)?;
    let spki = SubjectPublicKeyInfoRef::try_from(spki_der.as_ref())
        .map_err(|_| RemoteTransportError::InvalidIdentity)?;
    let ed25519_oid =
        ObjectIdentifier::new(ED25519_OID).map_err(|_| RemoteTransportError::InvalidIdentity)?;
    if spki.algorithm.oid != ed25519_oid
        || spki.algorithm.parameters.is_some()
        || spki.subject_public_key.unused_bits() != 0
        || spki.subject_public_key.raw_bytes().len() != 32
    {
        return Err(RemoteTransportError::InvalidIdentity);
    }
    Ok(spki.subject_public_key.raw_bytes().to_vec())
}

/// Verify the federation server's signed response wrapper.
pub fn verify_federation_response(
    wrapper: &[u8],
    expected_public_key: &str,
    expected_state_id: Option<&str>,
    now: i64,
) -> RemoteTransportResult<Value> {
    verify_federation_response_value(
        &parse_exact_json(wrapper)?,
        expected_public_key,
        expected_state_id,
        now,
    )
}

fn verify_federation_response_value(
    wrapper: &Value,
    expected_public_key: &str,
    expected_state_id: Option<&str>,
    now: i64,
) -> RemoteTransportResult<Value> {
    let object = wrapper
        .as_object()
        .ok_or(RemoteTransportError::InvalidResponse)?;
    let state_value = object.get("stateId").cloned().unwrap_or(Value::Null);
    if !state_value.is_null() && !state_value.is_string() {
        return Err(RemoteTransportError::InvalidResponse);
    }
    if expected_state_id.is_some_and(|expected| state_value.as_str() != Some(expected)) {
        return Err(RemoteTransportError::InvalidSignature);
    }
    let timestamp = object
        .get("timestamp")
        .and_then(Value::as_i64)
        .ok_or(RemoteTransportError::InvalidResponse)?;
    if timestamp.abs_diff(now) > CLOCK_SKEW_SECONDS as u64 {
        return Err(RemoteTransportError::InvalidSignature);
    }
    let nonce = object
        .get("nonce")
        .and_then(Value::as_str)
        .ok_or(RemoteTransportError::InvalidResponse)?;
    validate_federation_nonce(nonce, timestamp, now)?;
    let result = object
        .get("result")
        .filter(|value| value.is_object())
        .ok_or(RemoteTransportError::InvalidResponse)?;
    let signature = object
        .get("signature")
        .and_then(Value::as_str)
        .ok_or(RemoteTransportError::InvalidSignature)?;
    let proof = serde_json::json!({
        "stateId": state_value,
        "timestamp": timestamp,
        "nonce": nonce,
        "result": result
    });
    let canonical = canonical_json(&proof)?;
    verify_signature(
        expected_public_key,
        format!("{FEDERATION_RESPONSE_DOMAIN}{canonical}").as_bytes(),
        signature,
    )?;
    Ok(result.clone())
}

fn parse_exact_json(bytes: &[u8]) -> RemoteTransportResult<Value> {
    reject_unrepresentable_integer_tokens(bytes)?;
    serde_json::from_slice(bytes).map_err(|_| RemoteTransportError::InvalidResponse)
}

fn reject_unrepresentable_integer_tokens(bytes: &[u8]) -> RemoteTransportResult<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes[offset] == b'"' {
            offset += 1;
            while offset < bytes.len() {
                match bytes[offset] {
                    b'\\' => offset = (offset + 2).min(bytes.len()),
                    b'"' => {
                        offset += 1;
                        break;
                    }
                    _ => offset += 1,
                }
            }
            continue;
        }
        if bytes[offset] == b'-' || bytes[offset].is_ascii_digit() {
            let start = offset;
            while offset < bytes.len()
                && (bytes[offset].is_ascii_digit()
                    || matches!(bytes[offset], b'-' | b'+' | b'.' | b'e' | b'E'))
            {
                offset += 1;
            }
            let token = &bytes[start..offset];
            if !token.iter().any(|byte| matches!(byte, b'.' | b'e' | b'E')) {
                let representable = if token.first() == Some(&b'-') {
                    std::str::from_utf8(token)
                        .ok()
                        .and_then(|text| text.parse::<i64>().ok())
                        .is_some()
                } else {
                    std::str::from_utf8(token)
                        .ok()
                        .and_then(|text| text.parse::<u64>().ok())
                        .is_some()
                };
                if !representable {
                    return Err(RemoteTransportError::InvalidResponse);
                }
            }
            continue;
        }
        offset += 1;
    }
    Ok(())
}

pub fn validate_multi_server_nonce(
    nonce: &str,
    timestamp: i64,
    now: i64,
) -> RemoteTransportResult<()> {
    if timestamp.abs_diff(now) > CLOCK_SKEW_SECONDS as u64
        || nonce.len() != 32
        || !nonce
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(RemoteTransportError::InvalidNonce);
    }
    Ok(())
}

pub struct MultiServerExchangeInput<'a> {
    pub server_id: &'a str,
    pub method: &'a str,
    pub url: &'a str,
    pub request_id: &'a str,
    pub timestamp: i64,
    pub nonce: &'a str,
    pub body: &'a [u8],
    pub expected_public_key: &'a str,
    pub timeout: Duration,
}

/// Reserve, send at most once, verify and durably complete one multi-server exchange.
pub async fn exchange_multi_server(
    store: std::sync::Arc<RuntimeStore>,
    identity: &RemoteIdentity,
    input: &MultiServerExchangeInput<'_>,
    now: i64,
) -> RemoteTransportResult<Value> {
    validate_multi_server_nonce(input.nonce, input.timestamp, now)?;
    validate_identifier(input.server_id)?;
    validate_identifier(input.request_id)?;
    ed25519_public_key(input.expected_public_key)?;
    let method = input.method.to_ascii_uppercase();
    validate_exchange_arguments(
        &method,
        input.url,
        input.body,
        input.timeout,
        MAX_RESPONSE_BYTES,
    )?;
    let target = canonical_request_target(input.url)?;
    let fingerprint = multi_server_request_fingerprint_normalized(&method, input.url, input.body)?;
    let invitation_response = method == "POST"
        && target == "/api/multi-server"
        && serde_json::from_slice::<Value>(input.body)
            .ok()
            .and_then(|body| {
                body.get("action")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .as_deref()
            == Some("create_invite");
    let server_id = input.server_id.to_owned();
    let request_id = input.request_id.to_owned();
    let fingerprint_for_reservation = fingerprint.clone();
    let reserved = store
        .transaction_with(move |transaction| {
            RemoteTransportRepository::reserve_multi_server_request(
                transaction,
                &server_id,
                &request_id,
                &fingerprint_for_reservation,
                now as f64,
            )
            .map_err(|error| map_outbound_error(error, &server_id, &request_id))
        })
        .await?;
    let replay_redacted = matches!(
        &reserved,
        studio_runtime_store::OutboundReservation::Redacted
    );
    match reserved {
        studio_runtime_store::OutboundReservation::Complete(value) => {
            let bytes =
                serde_json::to_vec(&value).map_err(|_| RemoteTransportError::InvalidResponse)?;
            return verify_multi_server_response(
                &bytes,
                input.server_id,
                input.expected_public_key,
            )
            .map_err(|_| RemoteTransportError::InvalidStoredObject {
                table: "runtime_access_outbound",
                key: format!("{}/{}", input.server_id, input.request_id),
            });
        }
        studio_runtime_store::OutboundReservation::Redacted if invitation_response => {}
        studio_runtime_store::OutboundReservation::Redacted => {
            return Ok(serde_json::json!({"outcome":"redacted"}));
        }
        studio_runtime_store::OutboundReservation::Unknown => {
            return Err(RemoteTransportError::ExchangeUnknown);
        }
        studio_runtime_store::OutboundReservation::New => {}
    }

    let request = MultiServerRequest {
        server_id: input.server_id,
        method: &method,
        url: input.url,
        request_id: input.request_id,
        timestamp: input.timestamp,
        nonce: input.nonce,
        body: input.body,
    };
    let headers = multi_server_signed_headers_normalized(identity, &request, &method, now)?;
    let mut owned_headers = headers;
    owned_headers.push(("Accept-Encoding".to_owned(), "identity".to_owned()));
    if !input.body.is_empty() {
        owned_headers.push(("Content-Type".to_owned(), "application/json".to_owned()));
    }
    let server_id = input.server_id.to_owned();
    let public_key = input.expected_public_key.to_owned();
    let url = input.url.to_owned();
    let body = input.body.to_vec();
    let timeout = input.timeout;
    let store_for_transport = std::sync::Arc::clone(&store);
    let response = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/remote_transport",
        move || {
            let headers = owned_headers
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect::<Vec<_>>();
            exchange_paired_request(
                &store_for_transport,
                PairedRequest {
                    server_id: &server_id,
                    public_key: &public_key,
                    method: &method,
                    url: &url,
                    headers: &headers,
                    body: &body,
                    timeout,
                    response_limit: MAX_RESPONSE_BYTES,
                },
            )
        },
    )
    .await
    .map_err(|_| RemoteTransportError::ExchangeUnknown)??;
    if (300..400).contains(&response.status) {
        return Err(RemoteTransportError::InvalidResponse);
    }
    if !(200..300).contains(&response.status) {
        return Err(RemoteTransportError::PeerRejected);
    }
    #[cfg(unix)]
    {
        let advert = response
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("x-studio-link"))
            .map(|(_, value)| value.clone());
        PeerLinkClient::new(store.root()).learn(input.server_id, advert.as_deref());
    }
    let value =
        verify_multi_server_response(&response.body, input.server_id, input.expected_public_key)?;
    if matches!(
        value.get("outcome").and_then(Value::as_str),
        Some("pending" | "unknown")
    ) {
        return Ok(value);
    }
    if !replay_redacted {
        let server_id = input.server_id.to_owned();
        let request_id = input.request_id.to_owned();
        let fingerprint = fingerprint.clone();
        let value_for_commit = value.clone();
        store
            .transaction_with(move |transaction| {
                let result = if invitation_response {
                    RemoteTransportRepository::redact_multi_server_request(
                        transaction,
                        &server_id,
                        &request_id,
                        &fingerprint,
                        now as f64,
                    )
                } else {
                    RemoteTransportRepository::complete_multi_server_request(
                        transaction,
                        &server_id,
                        &request_id,
                        &fingerprint,
                        &value_for_commit,
                        now as f64,
                    )
                };
                result.map_err(|error| map_outbound_error(error, &server_id, &request_id))
            })
            .await?;
    }
    Ok(value)
}

#[cfg(test)]
async fn exchange_multi_server_with(
    store: &RuntimeStore,
    identity: &RemoteIdentity,
    input: &MultiServerExchangeInput<'_>,
    now: i64,
    exchange: impl FnOnce(
        &str,
        &str,
        &[(&str, &str)],
        &[u8],
        Duration,
        usize,
    ) -> RemoteTransportResult<HttpResponse>,
) -> RemoteTransportResult<Value> {
    validate_multi_server_nonce(input.nonce, input.timestamp, now)?;
    validate_identifier(input.server_id)?;
    validate_identifier(input.request_id)?;
    ed25519_public_key(input.expected_public_key)?;
    let method = input.method.to_ascii_uppercase();
    validate_exchange_arguments(
        &method,
        input.url,
        input.body,
        input.timeout,
        MAX_RESPONSE_BYTES,
    )?;
    let target = canonical_request_target(input.url)?;
    let fingerprint = multi_server_request_fingerprint_normalized(&method, input.url, input.body)?;
    let invitation_response = method == "POST"
        && target == "/api/multi-server"
        && serde_json::from_slice::<Value>(input.body)
            .ok()
            .and_then(|body| {
                body.get("action")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .as_deref()
            == Some("create_invite");
    let server_id = input.server_id.to_owned();
    let request_id = input.request_id.to_owned();
    let fingerprint_for_reservation = fingerprint.clone();
    let reserved = store
        .transaction_with(move |transaction| {
            RemoteTransportRepository::reserve_multi_server_request(
                transaction,
                &server_id,
                &request_id,
                &fingerprint_for_reservation,
                now as f64,
            )
            .map_err(|error| map_outbound_error(error, &server_id, &request_id))
        })
        .await?;
    let replay_redacted = matches!(
        &reserved,
        studio_runtime_store::OutboundReservation::Redacted
    );
    match reserved {
        studio_runtime_store::OutboundReservation::Complete(value) => {
            let bytes =
                serde_json::to_vec(&value).map_err(|_| RemoteTransportError::InvalidResponse)?;
            return verify_multi_server_response(
                &bytes,
                input.server_id,
                input.expected_public_key,
            )
            .map_err(|_| RemoteTransportError::InvalidStoredObject {
                table: "runtime_access_outbound",
                key: format!("{}/{}", input.server_id, input.request_id),
            });
        }
        studio_runtime_store::OutboundReservation::Redacted if invitation_response => {}
        studio_runtime_store::OutboundReservation::Redacted => {
            return Ok(serde_json::json!({"outcome":"redacted"}));
        }
        studio_runtime_store::OutboundReservation::Unknown => {
            return Err(RemoteTransportError::ExchangeUnknown);
        }
        studio_runtime_store::OutboundReservation::New => {}
    }

    let request = MultiServerRequest {
        server_id: input.server_id,
        method: &method,
        url: input.url,
        request_id: input.request_id,
        timestamp: input.timestamp,
        nonce: input.nonce,
        body: input.body,
    };
    let headers = multi_server_signed_headers_normalized(identity, &request, &method, now)?;
    let mut headers = headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    headers.push(("Accept-Encoding", "identity"));
    if !input.body.is_empty() {
        headers.push(("Content-Type", "application/json"));
    }
    let response = exchange(
        &method,
        input.url,
        &headers,
        input.body,
        input.timeout,
        MAX_RESPONSE_BYTES,
    )?;
    if (300..400).contains(&response.status) {
        return Err(RemoteTransportError::InvalidResponse);
    }
    if !(200..300).contains(&response.status) {
        return Err(RemoteTransportError::PeerRejected);
    }
    let value =
        verify_multi_server_response(&response.body, input.server_id, input.expected_public_key)?;
    if matches!(
        value.get("outcome").and_then(Value::as_str),
        Some("pending" | "unknown")
    ) {
        return Ok(value);
    }

    if !replay_redacted {
        let server_id = input.server_id.to_owned();
        let request_id = input.request_id.to_owned();
        let fingerprint = fingerprint.clone();
        let value = value.clone();
        store
            .transaction_with(move |transaction| {
                if invitation_response {
                    RemoteTransportRepository::redact_multi_server_request(
                        transaction,
                        &server_id,
                        &request_id,
                        &fingerprint,
                        now as f64,
                    )
                } else {
                    RemoteTransportRepository::complete_multi_server_request(
                        transaction,
                        &server_id,
                        &request_id,
                        &fingerprint,
                        &value,
                        now as f64,
                    )
                }
                .map_err(|error| map_outbound_error(error, &server_id, &request_id))
            })
            .await?;
    }
    Ok(value)
}

fn map_outbound_error(
    error: StoreError,
    server_id: &str,
    request_id: &str,
) -> RemoteTransportError {
    match &error {
        StoreError::SchemaMismatch(message)
            if message.contains("conflicts with stored content") =>
        {
            RemoteTransportError::RequestConflict
        }
        StoreError::SchemaMismatch(message) if message.contains("runtime_access_outbound") => {
            RemoteTransportError::InvalidStoredObject {
                table: "runtime_access_outbound",
                key: format!("{server_id}/{request_id}"),
            }
        }
        _ => RemoteTransportError::Store(error),
    }
}

pub fn validate_federation_nonce(
    nonce: &str,
    timestamp: i64,
    now: i64,
) -> RemoteTransportResult<()> {
    if timestamp.abs_diff(now) > CLOCK_SKEW_SECONDS as u64
        || !(16..=128).contains(&nonce.chars().count())
    {
        return Err(RemoteTransportError::InvalidNonce);
    }
    Ok(())
}

/// Normalize the HTTPS Tailscale Serve origin accepted by Python federation.
pub fn validate_federation_origin(value: &str) -> RemoteTransportResult<String> {
    let parsed = url::Url::parse(value).map_err(|_| RemoteTransportError::InvalidInput)?;
    let host = parsed
        .host_str()
        .ok_or(RemoteTransportError::InvalidInput)?;
    if parsed.scheme() != "https"
        || !host.ends_with(".ts.net")
        || host.is_empty()
        || parsed.username() != ""
        || parsed.password().is_some()
        || !matches!(parsed.path(), "" | "/")
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b".-".contains(&byte))
    {
        return Err(RemoteTransportError::InvalidInput);
    }
    let port = parsed.port().filter(|port| *port != 443);
    Ok(match port {
        Some(port) => format!("https://{host}:{port}"),
        None => format!("https://{host}"),
    })
}

pub fn new_nonce() -> RemoteTransportResult<String> {
    let mut random = [0u8; 24];
    getrandom::fill(&mut random).map_err(|_| RemoteTransportError::InvalidInput)?;
    Ok(URL_SAFE_NO_PAD.encode(random))
}

pub async fn consume_federation_nonce(
    store: &RuntimeStore,
    peer_id: &str,
    nonce: &str,
    timestamp: i64,
    now: i64,
) -> RemoteTransportResult<()> {
    validate_federation_nonce(nonce, timestamp, now)?;
    let peer_id = peer_id.to_owned();
    let nonce = nonce.to_owned();
    store
        .transaction_with(move |transaction| {
            let accepted = RemoteTransportRepository::consume_federation_nonce(
                transaction,
                &peer_id,
                &nonce,
                timestamp,
                now,
            )?;
            if accepted {
                Ok(())
            } else {
                Err(RemoteTransportError::Replay)
            }
        })
        .await
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MultiServerPairingContext {
    Paired,
    Pairing,
    AutoPairing,
    Recovery,
}

pub async fn consume_multi_server_nonce(
    store: &RuntimeStore,
    client_id: &str,
    nonce: &str,
    timestamp: i64,
    now: i64,
    pairing: MultiServerPairingContext,
) -> RemoteTransportResult<()> {
    validate_multi_server_nonce(nonce, timestamp, now)?;
    let client_id = client_id.to_owned();
    let nonce = nonce.to_owned();
    store
        .transaction_with(move |transaction| {
            let accepted = RemoteTransportRepository::consume_access_nonce(
                transaction,
                &client_id,
                &nonce,
                now,
                matches!(
                    pairing,
                    MultiServerPairingContext::Pairing | MultiServerPairingContext::AutoPairing
                ),
            )?;
            if accepted {
                Ok(())
            } else {
                Err(RemoteTransportError::Replay)
            }
        })
        .await
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub struct PairedRequest<'a> {
    pub server_id: &'a str,
    pub public_key: &'a str,
    pub method: &'a str,
    pub url: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    pub body: &'a [u8],
    pub timeout: Duration,
    pub response_limit: usize,
}

#[cfg(unix)]
pub(crate) struct PeerLinkExchange<'a> {
    pub(crate) server: &'a str,
    pub(crate) public_key: &'a str,
    pub(crate) method: &'a str,
    pub(crate) path: &'a str,
    pub(crate) headers: &'a [(&'a str, &'a str)],
    pub(crate) body: &'a [u8],
    pub(crate) timeout: Duration,
}

/// Send one already-signed paired-server request over the learned QUIC peer
/// link first, then HTTPS only when the link confirms that it sent nothing.
/// Both orchestration and the journaled multi-server exchange use this path.
pub fn exchange_paired_request(
    store: &RuntimeStore,
    request: PairedRequest<'_>,
) -> RemoteTransportResult<HttpResponse> {
    validate_exchange_arguments(
        request.method,
        request.url,
        request.body,
        request.timeout,
        request.response_limit,
    )?;
    let path = canonical_request_target(request.url)?;
    #[cfg(unix)]
    let response =
        exchange_paired_request_with(&PeerLinkClient::new(store.root()), &request, &path, || {
            SignedHttpClient::exchange(
                request.method,
                request.url,
                request.headers,
                request.body,
                request.timeout,
                request.response_limit,
            )
        })?;
    #[cfg(not(unix))]
    let response = SignedHttpClient::exchange(
        request.method,
        request.url,
        request.headers,
        request.body,
        request.timeout,
        request.response_limit,
    )?;
    if response.body.len() > request.response_limit {
        return Err(RemoteTransportError::ResponseTooLarge);
    }
    Ok(response)
}

#[cfg(unix)]
fn exchange_paired_request_with(
    peer_link: &PeerLinkClient,
    request: &PairedRequest<'_>,
    path: &str,
    https_fallback: impl FnOnce() -> RemoteTransportResult<HttpResponse>,
) -> RemoteTransportResult<HttpResponse> {
    let response = match peer_link.exchange(PeerLinkExchange {
        server: request.server_id,
        public_key: request.public_key,
        method: request.method,
        path,
        headers: request.headers,
        body: request.body,
        timeout: request.timeout,
    }) {
        Ok(Some(response)) => response,
        Ok(None) | Err(PeerLinkFailure { sent: false }) => https_fallback()?,
        Err(PeerLinkFailure { sent: true }) => return Err(RemoteTransportError::ExchangeUnknown),
    };
    if response.body.len() > request.response_limit {
        return Err(RemoteTransportError::ResponseTooLarge);
    }
    Ok(response)
}

/// Update the shared learned-route owner after a paired response has passed
/// its signature and identity checks.
pub fn learn_paired_request_link(store: &RuntimeStore, server_id: &str, advert: Option<&str>) {
    #[cfg(unix)]
    PeerLinkClient::new(store.root()).learn(server_id, advert);
    #[cfg(not(unix))]
    let _ = (store, server_id, advert);
}

#[cfg(unix)]
struct LearnedPeerLink {
    address: String,
    unavailable_until: Option<Instant>,
}

#[cfg(unix)]
type LearnedPeerRouteMap = HashMap<String, LearnedPeerLink>;
#[cfg(unix)]
type LearnedPeerRouteCache = Mutex<HashMap<PathBuf, Arc<Mutex<LearnedPeerRouteMap>>>>;

/// Local Unix-socket client for the existing `studio-peer-link` QUIC process.
/// Routes are learned only from paired-server response headers.
#[cfg(unix)]
pub(crate) struct PeerLinkClient {
    socket: std::path::PathBuf,
    peers: Arc<Mutex<HashMap<String, LearnedPeerLink>>>,
}

#[cfg(unix)]
impl PeerLinkClient {
    const MAX_REPLY_LINE: usize = 64 * 1024;
    const DOWN_SECONDS: Duration = Duration::from_secs(60);
    const SOCKET_GRACE: Duration = Duration::from_secs(5);

    pub(crate) fn new(state_root: &Path) -> Self {
        static ROUTES: OnceLock<LearnedPeerRouteCache> = OnceLock::new();
        let socket = state_root.join("multi-server").join("peer-link.sock");
        let peers = ROUTES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .ok()
            .map(|mut routes| {
                Arc::clone(
                    routes
                        .entry(socket.clone())
                        .or_insert_with(|| Arc::new(Mutex::new(HashMap::new()))),
                )
            })
            .unwrap_or_else(|| Arc::new(Mutex::new(HashMap::new())));
        Self { socket, peers }
    }

    pub(crate) fn learn(&self, server: &str, advert: Option<&str>) {
        let route = advert.and_then(parse_peer_link_advert);
        if let Ok(mut peers) = self.peers.lock() {
            match route {
                Some(address) => {
                    peers.insert(
                        server.to_owned(),
                        LearnedPeerLink {
                            address,
                            unavailable_until: None,
                        },
                    );
                }
                None => {
                    peers.remove(server);
                }
            }
        }
    }

    pub(crate) fn exchange(
        &self,
        request: PeerLinkExchange<'_>,
    ) -> Result<Option<HttpResponse>, PeerLinkFailure> {
        use std::os::unix::net::UnixStream;

        let address = {
            let peers = self
                .peers
                .lock()
                .map_err(|_| PeerLinkFailure { sent: false })?;
            let Some(peer) = peers.get(request.server) else {
                return Ok(None);
            };
            if peer
                .unavailable_until
                .is_some_and(|until| Instant::now() < until)
            {
                return Ok(None);
            }
            peer.address.clone()
        };
        let mut connection = match UnixStream::connect(&self.socket) {
            Ok(connection) => connection,
            Err(_) => return Ok(None),
        };
        let socket_timeout = request.timeout.saturating_add(Self::SOCKET_GRACE);
        if connection.set_read_timeout(Some(socket_timeout)).is_err()
            || connection.set_write_timeout(Some(socket_timeout)).is_err()
        {
            return Ok(None);
        }
        let head = serde_json::json!({
            "address": address,
            "publicKey": request.public_key,
            "method": request.method,
            "path": request.path,
            "headers": request.headers.iter().map(|(name, value)| [*name, *value]).collect::<Vec<_>>(),
            "bodyLength": request.body.len(),
            "timeout": request.timeout.as_secs_f64(),
        });
        let mut encoded = serde_json::to_vec(&head).map_err(|_| PeerLinkFailure { sent: false })?;
        encoded.push(b'\n');
        if connection.write_all(&encoded).is_err() || connection.write_all(request.body).is_err() {
            // studio-peer-link forwards only a complete local request body.
            return Ok(None);
        }
        let mut reader = BufReader::new(connection);
        let mut line = Vec::new();
        (&mut reader)
            .take((Self::MAX_REPLY_LINE + 1) as u64)
            .read_until(b'\n', &mut line)
            .map_err(|_| PeerLinkFailure { sent: true })?;
        if line.len() > Self::MAX_REPLY_LINE || line.last() != Some(&b'\n') {
            return Err(PeerLinkFailure { sent: true });
        }
        let reply: Value = serde_json::from_slice(&line[..line.len() - 1])
            .map_err(|_| PeerLinkFailure { sent: true })?;
        if reply.get("failure").is_some() {
            let sent = reply.get("sent") != Some(&Value::Bool(false));
            if !sent
                && let Ok(mut peers) = self.peers.lock()
                && let Some(peer) = peers.get_mut(request.server)
            {
                peer.unavailable_until = Some(Instant::now() + Self::DOWN_SECONDS);
            }
            return Err(PeerLinkFailure { sent });
        }
        let status = reply
            .get("status")
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
            .ok_or(PeerLinkFailure { sent: true })?;
        let response_headers = reply
            .get("headers")
            .and_then(Value::as_array)
            .ok_or(PeerLinkFailure { sent: true })?
            .iter()
            .map(|header| {
                let pair = header.as_array().ok_or(PeerLinkFailure { sent: true })?;
                let name = pair
                    .first()
                    .and_then(Value::as_str)
                    .ok_or(PeerLinkFailure { sent: true })?;
                let value = pair
                    .get(1)
                    .and_then(Value::as_str)
                    .ok_or(PeerLinkFailure { sent: true })?;
                Ok((name.to_owned(), value.to_owned()))
            })
            .collect::<Result<Vec<_>, PeerLinkFailure>>()?;
        let mut response_body = Vec::new();
        let mut received = 0_usize;
        loop {
            let mut length = [0_u8; 4];
            reader
                .read_exact(&mut length)
                .map_err(|_| PeerLinkFailure { sent: true })?;
            let length = u32::from_be_bytes(length) as usize;
            if length == 0 {
                break;
            }
            received = received
                .checked_add(length)
                .filter(|size| *size <= MAX_RESPONSE_BYTES)
                .ok_or(PeerLinkFailure { sent: true })?;
            let start = response_body.len();
            response_body.resize(start + length, 0);
            reader
                .read_exact(&mut response_body[start..])
                .map_err(|_| PeerLinkFailure { sent: true })?;
        }
        Ok(Some(HttpResponse {
            status,
            headers: response_headers,
            body: response_body,
        }))
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct PeerLinkFailure {
    pub(crate) sent: bool,
}

#[cfg(unix)]
fn parse_peer_link_advert(value: &str) -> Option<String> {
    let (address, port) = value.rsplit_once(':')?;
    if address.contains(':') || port.is_empty() || port.len() > 5 {
        return None;
    }
    let ip = address.parse::<std::net::Ipv4Addr>().ok()?;
    let octets = ip.octets();
    if octets[0] != 100 || !(64..=127).contains(&octets[1]) {
        return None;
    }
    let port = port.parse::<u16>().ok().filter(|port| *port > 0)?;
    Some(format!("{ip}:{port}"))
}

/// Synchronous outbound HTTP with HTTPS, platform trust roots and bounded bodies.
pub struct SignedHttpClient;

impl SignedHttpClient {
    pub fn exchange(
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &[u8],
        timeout: Duration,
        response_limit: usize,
    ) -> RemoteTransportResult<HttpResponse> {
        validate_exchange_arguments(method, url, body, timeout, response_limit)?;
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .root_certs(RootCerts::PlatformVerifier)
            .build();
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .http_status_as_error(false)
            .max_redirects(0)
            .tls_config(tls)
            .timeout_global(Some(timeout))
            .build();
        let agent = ureq::Agent::new_with_config(config);
        exchange_with_agent(&agent, method, url, headers, body, response_limit)
    }
}

fn validate_exchange_arguments(
    method: &str,
    url: &str,
    body: &[u8],
    timeout: Duration,
    response_limit: usize,
) -> RemoteTransportResult<()> {
    if body.len() > MAX_REQUEST_BYTES
        || response_limit == 0
        || response_limit > MAX_RESPONSE_BYTES
        || timeout.is_zero()
        || timeout > Duration::from_secs(120)
        || method.parse::<ureq::http::Method>().is_err()
    {
        return Err(RemoteTransportError::InvalidInput);
    }
    validate_https_url(url)
}

fn exchange_with_agent(
    agent: &ureq::Agent,
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    response_limit: usize,
) -> RemoteTransportResult<HttpResponse> {
    let mut request = ureq::http::Request::builder().method(method).uri(url);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let request = request
        .body(body.to_vec())
        .map_err(|_| RemoteTransportError::InvalidInput)?;
    let response = agent.run(request).map_err(|error| match error {
        ureq::Error::BodyExceedsLimit(_) => RemoteTransportError::ResponseTooLarge,
        _ => RemoteTransportError::ExchangeUnknown,
    })?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect();
    let response_body = response
        .into_body()
        .into_with_config()
        .limit(response_limit as u64)
        .read_to_vec()
        .map_err(|error| match error {
            ureq::Error::BodyExceedsLimit(_) => RemoteTransportError::ResponseTooLarge,
            _ => RemoteTransportError::ExchangeUnknown,
        })?;
    Ok(HttpResponse {
        status,
        headers,
        body: response_body,
    })
}

fn validate_https_url(url: &str) -> RemoteTransportResult<()> {
    let uri = parse_https_url(url)?;
    let authority = uri.authority().ok_or(RemoteTransportError::InvalidInput)?;
    if uri.scheme_str() != Some("https")
        || authority.as_str().contains('@')
        || uri.path_and_query().is_none()
        || url.contains('#')
    {
        return Err(RemoteTransportError::InvalidInput);
    }
    Ok(())
}

fn parse_https_url(url: &str) -> RemoteTransportResult<ureq::http::Uri> {
    let uri = url
        .parse::<ureq::http::Uri>()
        .map_err(|_| RemoteTransportError::InvalidInput)?;
    let authority = uri.authority().ok_or(RemoteTransportError::InvalidInput)?;
    if uri.scheme_str() != Some("https")
        || authority.as_str().contains('@')
        || uri.path_and_query().is_none()
        || url.contains('#')
    {
        return Err(RemoteTransportError::InvalidInput);
    }
    Ok(uri)
}

fn canonical_request_target(url: &str) -> RemoteTransportResult<String> {
    let uri = parse_https_url(url)?;
    Ok(uri
        .path_and_query()
        .map_or_else(|| "/".to_owned(), |value| value.as_str().to_owned()))
}

fn validate_identifier(value: &str) -> RemoteTransportResult<()> {
    if value.is_empty()
        || value.len() > 128
        || !value.as_bytes()[0].is_ascii_alphanumeric()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
    {
        return Err(RemoteTransportError::InvalidInput);
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> RemoteTransportResult<()> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|_| RemoteTransportError::IdentityStorage)?;
    if metadata.file_type().is_symlink() {
        return Err(RemoteTransportError::IdentityStorage);
    }
    Ok(())
}

fn set_no_follow(options: &mut OpenOptions) -> RemoteTransportResult<()> {
    // The O_NOFOLLOW value differs between architectures (0x20000 is O_LARGEFILE on arm64
    // Linux), so it comes from rustix rather than a literal.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let flag = rustix::fs::OFlags::NOFOLLOW.bits();
        options
            .custom_flags(i32::try_from(flag).map_err(|_| RemoteTransportError::IdentityStorage)?);
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = options;
        Err(RemoteTransportError::IdentityStorage)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use std::sync::Arc;

    #[cfg(unix)]
    #[test]
    fn no_follow_open_refuses_a_symbolic_link() {
        let root = tempfile::tempdir().expect("temporary root");
        let target = root.path().join("identity.json");
        std::fs::write(&target, b"{}").expect("target");
        let link = root.path().join("link.json");
        std::os::unix::fs::symlink(&target, &link).expect("link");
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        super::set_no_follow(&mut options).expect("flag");
        assert!(options.open(&link).is_err());
        assert!(options.open(&target).is_ok());
    }

    use super::{
        HttpResponse, MultiServerExchangeInput, MultiServerRequest, MultiServerSigningInput,
        PeerLinkClient, RemoteIdentity, RemoteTransportError, canonical_json, ed25519_public_key,
        exchange_paired_request_with, federation_message_signing_bytes, federation_signed_headers,
        federation_signing_bytes, load_multi_server_identity, multi_server_request_fingerprint,
        multi_server_signed_headers, multi_server_signing_bytes, new_nonce,
        validate_federation_nonce, validate_https_url, validate_multi_server_nonce,
        verify_federation_response, verify_multi_server_response, verify_signature,
    };
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use ring::signature::{ED25519, UnparsedPublicKey};
    use serde_json::{Value, json};
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use studio_runtime_store::{RemoteTransportRepository, RuntimeStore};

    const SEED: [u8; 32] = [
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c,
        0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae,
        0x7f, 0x60,
    ];
    const PUBLIC: [u8; 32] = [
        0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07,
        0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07,
        0x51, 0x1a,
    ];
    const PYTHON_SIGNATURE_EMPTY: [u8; 64] = [
        0xe5, 0x56, 0x43, 0x00, 0xc3, 0x60, 0xac, 0x72, 0x90, 0x86, 0xe2, 0xcc, 0x80, 0x6e, 0x82,
        0x8a, 0x84, 0x87, 0x7f, 0x1e, 0xb8, 0xe5, 0xd9, 0x74, 0xd8, 0x73, 0xe0, 0x65, 0x22, 0x49,
        0x01, 0x55, 0x5f, 0xb8, 0x82, 0x15, 0x90, 0xa3, 0x3b, 0xac, 0xc6, 0x1e, 0x39, 0x70, 0x1c,
        0xf9, 0xb4, 0x6b, 0xd2, 0x5b, 0xf5, 0xf0, 0x59, 0x5b, 0xbe, 0x24, 0x65, 0x51, 0x41, 0x43,
        0x8e, 0x7a, 0x10, 0x0b,
    ];

    fn pkcs8_pem() -> Vec<u8> {
        let mut der = vec![
            0x30, 0x53, 0x02, 0x01, 0x01, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
            0x04, 0x20,
        ];
        der.extend_from_slice(&SEED);
        der.extend_from_slice(&[0xa1, 0x23, 0x03, 0x21, 0x00]);
        der.extend_from_slice(&PUBLIC);
        format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            STANDARD.encode(der)
        )
        .into_bytes()
    }

    fn pkcs8_v1_pem() -> Vec<u8> {
        let mut der = vec![
            0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
            0x04, 0x20,
        ];
        der.extend_from_slice(&SEED);
        format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            STANDARD.encode(der)
        )
        .into_bytes()
    }

    fn spki_pem() -> String {
        spki_pem_for(&PUBLIC)
    }

    fn spki_pem_for(public: &[u8]) -> String {
        let mut der = vec![
            0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
        ];
        der.extend_from_slice(public);
        format!(
            "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
            STANDARD.encode(der)
        )
    }

    fn identity() -> RemoteIdentity {
        RemoteIdentity::from_key_fields(
            "test-identity".to_owned(),
            "Loopback fixture".to_owned(),
            &pkcs8_pem(),
            spki_pem(),
        )
        .expect("fixed RFC Ed25519 fixture loads")
    }

    pub(crate) async fn runtime_store_fixture() -> (tempfile::TempDir, PathBuf, Arc<RuntimeStore>) {
        use std::fs;

        let directory = tempfile::tempdir().expect("owned test directory");
        let root = directory.path().to_path_buf();
        fs::create_dir_all(&root).expect("temporary state directory creates");
        let snapshot: serde_json::Value = serde_json::from_str(include_str!(
            "../../runtime-store/src/runtime-state-schema.json"
        ))
        .expect("committed state schema parses");
        for name in ["canvas.sqlite3", "analytics.sqlite3"] {
            let path = root.join(name);
            let connection =
                studio_runtime_store::Connection::open(path).expect("fixture database opens");
            studio_runtime_store::sqlite_durability::configure_writer(&connection)
                .expect("fixture pragmas apply");
            connection
                .execute_batch("BEGIN;")
                .expect("fixture pragmas apply");
            let mut statements = snapshot[name]["sqlite_master"]
                .as_array()
                .expect("fixture schema entries")
                .iter()
                .filter_map(|entry| {
                    Some((
                        entry["type"].as_str()?,
                        entry["name"].as_str()?,
                        entry["sql"].as_str()?,
                    ))
                })
                .collect::<Vec<_>>();
            statements.sort_by_key(|(kind, _, _)| match *kind {
                "table" => 0,
                "index" => 1,
                "view" => 2,
                _ => 3,
            });
            for (_, name, sql) in statements {
                let exists: bool = connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
                        [name],
                        |row| row.get(0),
                    )
                    .expect("fixture schema lookup");
                if !exists {
                    connection
                        .execute_batch(sql)
                        .expect("schema object creates");
                }
            }
            connection
                .execute_batch("COMMIT;")
                .expect("fixture schema commits");
        }
        let store = RuntimeStore::open(Some(&root))
            .await
            .expect("schema fixture opens");
        (directory, root, Arc::new(store))
    }

    #[test]
    fn python_ed25519_signature_verifies_and_rust_signing_is_interoperable() {
        let public_pem = spki_pem();
        verify_signature(&public_pem, b"", &STANDARD.encode(PYTHON_SIGNATURE_EMPTY))
            .expect("RFC/Python-compatible signature verifies");
        let signed = identity().sign(b"");
        assert_eq!(
            STANDARD.decode(&signed).expect("standard base64 decodes"),
            PYTHON_SIGNATURE_EMPTY
        );
        UnparsedPublicKey::new(&ED25519, PUBLIC)
            .verify(b"", &STANDARD.decode(signed).expect("signature decodes"))
            .expect("Rust signature verifies with Ed25519");
    }

    #[test]
    fn distinct_wire_formats_and_sorted_json_are_stable() {
        assert_eq!(
            multi_server_signing_bytes(MultiServerSigningInput {
                method: "post",
                target: "/api/x",
                server_id: "s",
                client_id: "c",
                timestamp: "1",
                nonce: "n",
                request_id: "r",
                body: b"{}",
            }),
            b"studio-multi-server-v1\nPOST\n/api/x\ns\nc\n1\nn\nr\n44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
        assert_eq!(
            federation_signing_bytes("POST", "/api/f", 1, "n", b"{}"),
            b"studio-federation-v1\nPOST\n/api/f\n1\nn\n44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
        assert_eq!(
            multi_server_request_fingerprint("post", "https://peer.example/api/test", b"{}")
                .expect("multi-server fingerprint uses the canonical target"),
            "1bcf659b703526f662c19c9b1d5da67a1d2e9570d3eada12d4d3b5fefe42b93a"
        );
        assert_eq!(
            canonical_json(&json!({"z":1,"a":"é"})).expect("JSON serializes"),
            "{\"a\":\"é\",\"z\":1}"
        );
        let message = federation_message_signing_bytes(&json!({"signature":"ignored","id":"x"}))
            .expect("message serializes");
        assert_eq!(
            message,
            "studio-federation-message-v1\n{\"id\":\"x\"}".as_bytes()
        );
    }

    #[test]
    fn identity_debug_redacts_private_and_public_pem() {
        let identity = identity();
        let rendered = format!("{identity:?}");
        assert!(!rendered.contains("PRIVATE KEY"));
        assert!(!rendered.contains("PUBLIC KEY"));
        assert!(!rendered.contains(&STANDARD.encode(SEED)));
    }

    #[test]
    fn multi_server_identity_loads_existing_private_owner_only_file_without_creating_one() {
        use std::fs;

        let directory_guard = tempfile::tempdir().expect("owned test directory");
        let root = directory_guard.path().to_path_buf();
        let directory = root.join("multi-server");
        fs::create_dir_all(&directory).expect("identity directory creates");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
                .expect("identity directory is private");
        }
        let identity_path = directory.join("identity.json");
        let record = serde_json::json!({
            "serverId":"fixture-server",
            "label":"Fixture",
            "privateKey":String::from_utf8(pkcs8_pem()).expect("fixture PEM is UTF-8"),
            "publicKey":spki_pem()
        });
        fs::write(
            &identity_path,
            serde_json::to_vec(&record).expect("fixture record serializes"),
        )
        .expect("fixture identity writes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&identity_path, fs::Permissions::from_mode(0o600))
                .expect("identity file is private");
        }

        let loaded = load_multi_server_identity(&root).expect("existing fixture identity loads");
        assert_eq!(loaded.id(), "fixture-server");
        assert_eq!(loaded.public_key_pem(), spki_pem());

        let absent_root = root.join("absent-state");
        assert!(load_multi_server_identity(&absent_root).is_err());
        assert!(!absent_root.exists());
    }

    #[test]
    fn node_pkcs8_v1_private_key_loads_only_with_matching_spki_identity() {
        let identity = RemoteIdentity::from_key_fields(
            "node-identity".to_owned(),
            "Fixture".to_owned(),
            &pkcs8_v1_pem(),
            spki_pem(),
        )
        .expect("PKCS#8 v1 fixture loads after public-key consistency check");
        assert_eq!(identity.id(), "node-identity");
        let other_key = Ed25519KeyPair::from_seed_unchecked(&[7; 32]).expect("distinct key");
        assert!(matches!(
            RemoteIdentity::from_key_fields(
                "node-identity".into(),
                "Fixture".into(),
                &pkcs8_v1_pem(),
                spki_pem_for(other_key.public_key().as_ref()),
            ),
            Err(RemoteTransportError::InvalidIdentity)
        ));
        assert!(
            RemoteIdentity::from_key_fields(
                "node-identity".to_owned(),
                "Fixture".to_owned(),
                &pkcs8_v1_pem(),
                "not a public key".to_owned(),
            )
            .is_err()
        );
    }

    #[test]
    fn signing_headers_and_federation_response_match_the_python_contract() {
        let identity = identity();
        let nonce = "abcdefghijklmnopqrstuvwxyz012345";
        let body = br#"{"a":1}"#;
        let multi = multi_server_signed_headers(
            &identity,
            &MultiServerRequest {
                server_id: "server-peer",
                method: "post",
                url: "https://peer.example/api/multi?cursor=2",
                request_id: "request-1",
                timestamp: 1_700_000_000,
                nonce,
                body,
            },
            1_700_000_000,
        )
        .expect("multi-server headers sign the exact request target");
        let signature = multi
            .iter()
            .find(|(name, _)| name == "X-Studio-Signature")
            .map(|(_, value)| value)
            .expect("signature header exists");
        verify_signature(
            &spki_pem(),
            &multi_server_signing_bytes(MultiServerSigningInput {
                method: "POST",
                target: "/api/multi?cursor=2",
                server_id: "server-peer",
                client_id: identity.id(),
                timestamp: "1700000000",
                nonce,
                request_id: "request-1",
                body,
            }),
            signature,
        )
        .expect("generated header verifies against its Python wire bytes");

        let response_proof = json!({
            "stateId":"peer-state",
            "timestamp":1_700_000_000,
            "nonce":nonce,
            "result":{"ok":true}
        });
        let signed_response = identity.sign(
            format!(
                "studio-federation-response-v1\n{}",
                canonical_json(&response_proof).expect("proof canonicalizes")
            )
            .as_bytes(),
        );
        let wrapper = json!({
            "stateId":"peer-state",
            "timestamp":1_700_000_000,
            "nonce":nonce,
            "result":{"ok":true},
            "signature":signed_response
        });
        assert_eq!(
            verify_federation_response(
                &serde_json::to_vec(&wrapper).expect("response wrapper serializes"),
                &spki_pem(),
                Some("peer-state"),
                1_700_000_000,
            )
            .expect("signed federation response verifies"),
            json!({"ok":true})
        );
        assert_eq!(
            federation_signed_headers(
                &identity,
                "POST",
                "https://peer.example/api/federation",
                1_700_000_000,
                nonce,
                body,
                1_700_000_000,
            )
            .expect("federation headers sign exact path")[0]
                .0,
            "X-Studio-Federation-State"
        );
    }

    #[test]
    fn python_produced_federation_vectors_verify_with_shared_canonical_json() {
        let vectors: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/remote-transport-python-vectors.json"
        ))
        .expect("review's Python-generated vectors parse");
        let public_key = vectors["public"].as_str().expect("fixture public key");
        for case in vectors["responses"].as_array().expect("response list") {
            let name = case["name"].as_str().expect("vector name");
            let wrapper = &case["wrapper"];
            let proof = json!({
                "stateId":wrapper["stateId"],
                "timestamp":wrapper["timestamp"],
                "nonce":wrapper["nonce"],
                "result":wrapper["result"]
            });
            let canonical = canonical_json(&proof).expect("Python response proof canonicalizes");
            verify_signature(
                public_key,
                format!("studio-federation-response-v1\n{canonical}").as_bytes(),
                wrapper["signature"]
                    .as_str()
                    .expect("Python response signature"),
            )
            .expect("Python-produced response signature verifies over shared canonical bytes");
            let expected_state = if name == "null_state" || name == "short_nonce" {
                None
            } else {
                Some("peer")
            };
            let wire = serde_json::to_vec(wrapper).expect("Python response wrapper serializes");
            let verified =
                super::verify_federation_response(&wire, public_key, expected_state, 1_700_000_000);
            if name == "short_nonce" {
                assert!(matches!(
                    verified,
                    Err(super::RemoteTransportError::InvalidNonce)
                ));
            } else {
                assert!(verified.is_ok(), "Python-produced {name} envelope verifies");
            }
        }

        let signatures = vectors["signatures"].as_array().expect("signature list");
        for case in signatures {
            let signature = case["signature"].as_str().expect("signature value");
            let accepted = verify_signature(public_key, b"", signature).is_ok();
            assert_eq!(
                accepted,
                case["name"] == "standard",
                "signature encoding policy for {}",
                case["name"]
            );
        }

        let canonical = vectors["canonical"].as_array().expect("canonical list");
        for case in &canonical[..2] {
            let wire = case["wire"].as_str().expect("canonical wire value");
            let parsed: serde_json::Value = serde_json::from_str(wire).expect("float JSON parses");
            assert_eq!(canonical_json(&parsed).expect("float canonicalizes"), wire);
        }
        assert!(super::parse_exact_json(br#"{"n":18446744073709551617}"#).is_err());
        assert!(super::parse_exact_json(br#"{"text":"18446744073709551617","n":1e+16}"#).is_ok());
        assert!(
            super::verify_federation_response(
                &serde_json::to_vec(&vectors["responses"][3]["wrapper"])
                    .expect("null state wrapper serializes"),
                public_key,
                Some("peer"),
                1_700_000_000
            )
            .is_err(),
            "null state cannot match an expected federation state"
        );
        let mut missing_state = vectors["responses"][3]["wrapper"].clone();
        missing_state
            .as_object_mut()
            .expect("fixture wrapper is an object")
            .remove("stateId");
        assert!(
            super::verify_federation_response(
                &serde_json::to_vec(&missing_state).expect("missing state wrapper serializes"),
                public_key,
                None,
                1_700_000_000,
            )
            .is_ok()
        );
    }

    #[test]
    fn federation_identifiers_use_python_first_character_rule() {
        assert!(super::validate_identifier("a_server-1").is_ok());
        assert!(super::validate_identifier(".server").is_err());
        assert!(super::validate_identifier("_server").is_err());
        assert!(super::validate_identifier(":server").is_err());
        assert!(super::validate_identifier("-server").is_err());
    }

    #[tokio::test]
    async fn refusal_rolls_back_nonce_cleanup_through_the_service_transaction() {
        let (_directory, _root, store) = runtime_store_fixture().await;
        let now = 1_700_000_000_i64;
        store
            .transaction(move |transaction| {
                transaction.execute(
                    "INSERT INTO runtime_federation_nonces(peer_id,nonce,created) VALUES('peer-a','stale',?1)",
                    [now - 1_000],
                )?;
                for index in 0..240 {
                    transaction.execute(
                        "INSERT INTO runtime_federation_nonces(peer_id,nonce,created) VALUES('peer-a',?1,?2)",
                        rusqlite::params![format!("recent-{index}"), now],
                    )?;
                }
                Ok(())
            })
            .await
            .expect("fixture rows commit");
        assert!(matches!(
            super::consume_federation_nonce(
                &store,
                "peer-a",
                "incoming-federation-nonce",
                now,
                now,
            )
            .await,
            Err(super::RemoteTransportError::Replay)
        ));
        let connection = store.test_reader().expect("read transaction opens");
        let stale: i64 = connection
            .query_row(
                "SELECT count(*) FROM runtime_federation_nonces WHERE nonce='stale'",
                [],
                |row| row.get(0),
            )
            .expect("stale row count reads");
        assert_eq!(stale, 1, "rate refusal rolled back stale-row cleanup");
        drop(connection);

        store
            .transaction(move |transaction| {
                transaction.execute(
                    "INSERT INTO runtime_access_nonces(client,nonce,created) VALUES('old-client','stale',?1)",
                    [now - 1_000],
                )?;
                for index in 0..1_280 {
                    transaction.execute(
                        "INSERT INTO runtime_access_nonces(client,nonce,created) VALUES(?1,?2,?3)",
                        rusqlite::params![format!("unpaired-{index}"), format!("{index:032}"), now],
                    )?;
                }
                Ok(())
            })
            .await
            .expect("unpaired cap fixture rows commit");
        assert!(matches!(
            super::consume_multi_server_nonce(
                &store,
                "new-client",
                "abcdefghijklmnopqrstuvwxyz012345",
                now,
                now,
                super::MultiServerPairingContext::Pairing,
            )
            .await,
            Err(super::RemoteTransportError::Replay)
        ));
        let connection = store.test_reader().expect("multi-server stale row reads");
        let stale: i64 = connection
            .query_row(
                "SELECT count(*) FROM runtime_access_nonces WHERE nonce='stale'",
                [],
                |row| row.get(0),
            )
            .expect("multi-server stale-row count reads");
        assert_eq!(stale, 1, "pairing-limit refusal rolled back cleanup");
        drop(connection);
        drop(store);
    }

    #[tokio::test]
    async fn invalid_stored_federation_identity_names_its_table_and_primary_key() {
        let (_directory, _root, store) = runtime_store_fixture().await;
        store
            .transaction(|transaction| {
                transaction.execute(
                    "INSERT INTO runtime_federation_identity(id,record) VALUES('local','{}')",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("malformed identity fixture commits");
        let error = super::load_federation_identity(&store)
            .await
            .expect_err("malformed key is refused");
        assert!(matches!(
            error,
            super::RemoteTransportError::InvalidStoredObject {
                table: "runtime_federation_identity",
                key
            } if key == "local"
        ));
        drop(store);
    }

    #[tokio::test]
    async fn exchange_service_replays_saved_receipt_and_never_sends_unknown_after_restart() {
        let (_directory, root, store) = runtime_store_fixture().await;
        let identity = identity();
        let public_key = spki_pem();
        let body = br#"{"action":"rename_server","label":"fixture"}"#;
        let input = MultiServerExchangeInput {
            server_id: "server-peer",
            method: "post",
            url: "https://peer.example/api/multi-server",
            request_id: "request-a",
            timestamp: 1_700_000_000,
            nonce: "abcdefghijklmnopqrstuvwxyz012345",
            body,
            expected_public_key: &public_key,
            timeout: std::time::Duration::from_secs(2),
        };
        let response = serde_json::to_vec(&json!({
            "serverId":"server-peer",
            "publicKey":public_key,
            "renamed":true
        }))
        .expect("fixture response serializes");
        let mut sends = 0;
        let first = super::exchange_multi_server_with(
            &store,
            &identity,
            &input,
            1_700_000_000,
            |method, _, _, _, _, _| {
                assert_eq!(
                    method, "POST",
                    "one normalized method reaches the transport"
                );
                sends += 1;
                Ok(super::HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: response.clone(),
                })
            },
        )
        .await
        .expect("new exchange verifies and completes");
        drop(store);

        let store = RuntimeStore::open(Some(&root))
            .await
            .expect("saved receipt opens after restart");
        let replay = super::exchange_multi_server_with(
            &store,
            &identity,
            &input,
            1_700_000_000,
            |_, _, _, _, _, _| {
                sends += 1;
                Err(super::RemoteTransportError::ExchangeUnknown)
            },
        )
        .await
        .expect("saved result is replayed without transport");
        assert_eq!(first, replay);
        assert_eq!(sends, 1);
        drop(store);

        let store = RuntimeStore::open(Some(&root))
            .await
            .expect("unknown reservation opens after restart");
        let fingerprint =
            super::multi_server_request_fingerprint("POST", input.url, br#"{"other":"body"}"#)
                .expect("unknown fixture fingerprint computes");
        store
            .transaction(move |transaction| {
                RemoteTransportRepository::reserve_multi_server_request(
                    transaction,
                    "server-peer",
                    "unknown-request",
                    &fingerprint,
                    1_700_000_000.0,
                )?;
                Ok(())
            })
            .await
            .expect("unknown reservation commits");
        drop(store);
        let store = RuntimeStore::open(Some(&root))
            .await
            .expect("unknown receipt reopens");
        let unknown_input = MultiServerExchangeInput {
            request_id: "unknown-request",
            body: br#"{"other":"body"}"#,
            ..input
        };
        let result = super::exchange_multi_server_with(
            &store,
            &identity,
            &unknown_input,
            1_700_000_000,
            |_, _, _, _, _, _| {
                sends += 1;
                Err(super::RemoteTransportError::ExchangeUnknown)
            },
        )
        .await;
        assert!(matches!(
            result,
            Err(super::RemoteTransportError::ExchangeUnknown)
        ));
        assert_eq!(sends, 1, "unknown request identity is never sent again");
        drop(store);
    }

    #[tokio::test]
    async fn redacted_invitation_retry_reuses_the_same_request_and_recovers_its_secret() {
        let (_directory, root, store) = runtime_store_fixture().await;
        let identity = identity();
        let public_key = spki_pem();
        let body = br#"{"action":"create_invite","requestId":"invite-request"}"#;
        let first_input = MultiServerExchangeInput {
            server_id: "server-peer",
            method: "POST",
            url: "https://peer.example/api/multi-server",
            request_id: "invite-request",
            timestamp: 1_700_000_000,
            nonce: "abcdefghijklmnopqrstuvwxyz012345",
            body,
            expected_public_key: &public_key,
            timeout: Duration::from_secs(2),
        };
        let response = serde_json::to_vec(&json!({
            "serverId":"server-peer",
            "publicKey":public_key,
            "invitation":{"serverId":"server-peer","token":"fixture-secret"},
            "expires":1_700_000_900
        }))
        .expect("fixture invitation serializes");
        let mut sends = 0;
        let first = super::exchange_multi_server_with(
            &store,
            &identity,
            &first_input,
            1_700_000_000,
            |_, _, headers, request_body, _, _| {
                sends += 1;
                assert_eq!(request_body, body);
                assert!(headers.iter().any(|(name, value)| {
                    *name == "X-Studio-Request-Id" && *value == "invite-request"
                }));
                Ok(super::HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: response.clone(),
                })
            },
        )
        .await
        .expect("first invitation response is saved redacted");
        let outbound: String = store.test_reader().expect("outbound reader")
            .query_row("SELECT record FROM runtime_access_outbound WHERE server='server-peer' AND request_id='invite-request'", [], |row| row.get(0))
            .expect("durable outbound receipt");
        assert!(
            !outbound.contains("fixture-secret"),
            "invitation secret must not be persisted"
        );

        drop(store);

        let store = RuntimeStore::open(Some(&root))
            .await
            .expect("redacted request survives restart");
        let retry_input = MultiServerExchangeInput {
            nonce: "0123456789abcdefghijklmnopqrstuv",
            ..first_input
        };
        let retry = super::exchange_multi_server_with(
            &store,
            &identity,
            &retry_input,
            1_700_000_000,
            |_, _, headers, request_body, _, _| {
                sends += 1;
                assert_eq!(request_body, body);
                assert!(headers.iter().any(|(name, value)| {
                    *name == "X-Studio-Request-Id" && *value == "invite-request"
                }));
                Ok(super::HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: response.clone(),
                })
            },
        )
        .await
        .expect("same logical invitation retrieves its receiver-saved response");
        assert_eq!(retry, first);
        assert_eq!(retry["invitation"]["token"], "fixture-secret");
        assert_eq!(sends, 2);
        drop(store);
    }

    #[test]
    fn multi_server_response_identity_must_match_the_paired_key() {
        let public_key = spki_pem();
        let response = json!({
            "serverId":"server-peer",
            "identity":{"publicKey":public_key}
        });
        let body = serde_json::to_vec(&response).expect("response serializes");
        assert_eq!(
            verify_multi_server_response(&body, "server-peer", &spki_pem())
                .expect("matching peer identity verifies"),
            response
        );

        let other_key = Ed25519KeyPair::from_seed_unchecked(&[7; 32]).expect("distinct key");
        for (field, wrong) in [
            ("publicKey", spki_pem_for(other_key.public_key().as_ref())),
            ("serverId", "other-peer".to_owned()),
        ] {
            let mut mismatch = response.clone();
            mismatch["identity"][field] = json!(wrong);
            let body = serde_json::to_vec(&mismatch).expect("response serializes");
            assert!(
                matches!(
                    verify_multi_server_response(&body, "server-peer", &spki_pem()),
                    Err(RemoteTransportError::InvalidSignature)
                ),
                "{field}"
            );
        }
    }

    #[test]
    fn nonce_and_https_url_validation_are_fail_closed() {
        let nonce = new_nonce().expect("OS randomness available");
        assert_eq!(nonce.len(), 32);
        assert!(validate_multi_server_nonce(&nonce, 1_000, 1_000).is_ok());
        assert!(validate_multi_server_nonce("short", 1_000, 1_000).is_err());
        assert!(validate_federation_nonce("!non-url-safe-federation!", 1_000, 1_000).is_ok());
        assert!(validate_federation_nonce("short", 1_000, 1_000).is_err());
        assert!(validate_federation_nonce(&"n".repeat(129), 1_000, 1_000).is_err());
        assert!(validate_https_url("https://peer.example/api").is_ok());
        assert!(validate_https_url("http://peer.example/api").is_err());
        assert!(validate_https_url("https://user@peer.example/api").is_err());
        assert_eq!(
            ed25519_public_key(&spki_pem()).expect("Ed25519 SPKI parses"),
            PUBLIC
        );
    }

    #[test]
    fn signed_exchange_uses_a_loopback_server_and_reads_a_bounded_receipt() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener binds");
        let address = listener
            .local_addr()
            .expect("loopback address is available");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("test client connects");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("test stream timeout applies");
            let mut request = Vec::new();
            let mut chunk = [0u8; 512];
            loop {
                let count = stream.read(&mut chunk).expect("request headers arrive");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request_text = String::from_utf8_lossy(&request).to_ascii_lowercase();
            assert!(request_text.starts_with("post /api/test http/1.1\r\n"));
            assert!(request_text.contains("x-studio-signature:"));
            assert!(request_text.contains("content-length: 2"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .expect("test receipt writes");
        });

        let identity = identity();
        let body = b"{}";
        let nonce = "abcdefghijklmnopqrstuvwxyz012345";
        let timestamp = 1_700_000_000;
        let timestamp_text = timestamp.to_string();
        let signature = identity.sign(&multi_server_signing_bytes(MultiServerSigningInput {
            method: "POST",
            target: "/api/test",
            server_id: "server-a",
            client_id: identity.id(),
            timestamp: &timestamp_text,
            nonce,
            request_id: "request-a",
            body,
        }));
        let headers = [
            ("X-Studio-Client", identity.id()),
            ("X-Studio-Server", "server-a"),
            ("X-Studio-Timestamp", "1700000000"),
            ("X-Studio-Nonce", nonce),
            ("X-Studio-Request-Id", "request-a"),
            ("X-Studio-Signature", signature.as_str()),
        ];
        let config = ureq::Agent::config_builder()
            .https_only(false)
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(2)))
            .build();
        let agent = ureq::Agent::new_with_config(config);
        let response = super::exchange_with_agent(
            &agent,
            "POST",
            &format!("http://{address}/api/test"),
            &headers,
            body,
            8,
        )
        .expect("local test exchange succeeds");
        server.join().expect("loopback test server exits");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"ok");
    }

    #[test]
    fn signed_exchange_enforces_deadline_and_body_bounds_before_network() {
        use super::{
            MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, RemoteTransportError, SignedHttpClient,
        };
        use std::time::Duration;

        assert!(matches!(
            SignedHttpClient::exchange(
                "GET",
                "https://peer.example/api",
                &[],
                b"",
                Duration::ZERO,
                1
            ),
            Err(RemoteTransportError::InvalidInput)
        ));
        assert!(matches!(
            SignedHttpClient::exchange(
                "GET",
                "https://peer.example/api",
                &[],
                b"",
                Duration::from_secs(121),
                1
            ),
            Err(RemoteTransportError::InvalidInput)
        ));
        let oversized_request = vec![0; MAX_REQUEST_BYTES + 1];
        assert!(matches!(
            SignedHttpClient::exchange(
                "POST",
                "https://peer.example/api",
                &[],
                &oversized_request,
                Duration::from_secs(1),
                1,
            ),
            Err(RemoteTransportError::InvalidInput)
        ));
        assert!(matches!(
            SignedHttpClient::exchange(
                "GET",
                "https://peer.example/api",
                &[],
                b"",
                Duration::from_secs(1),
                MAX_RESPONSE_BYTES + 1,
            ),
            Err(RemoteTransportError::InvalidInput)
        ));
    }

    #[test]
    fn bounded_ureq_body_reader_refuses_oversized_response() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback binds");
        let address = listener.local_addr().expect("loopback port is available");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("test request connects");
            let mut request = [0; 1024];
            let _ = stream.read(&mut request).expect("request reads");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\n123456789",
                )
                .expect("oversized body writes");
        });
        let config = ureq::Agent::config_builder()
            .https_only(false)
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(2)))
            .build();
        let agent = ureq::Agent::new_with_config(config);
        let error = super::exchange_with_agent(
            &agent,
            "GET",
            &format!("http://{address}/oversized"),
            &[],
            b"",
            8,
        )
        .err()
        .expect("ureq body limit rejects the ninth byte");
        server.join().expect("loopback server exits");
        assert!(matches!(
            error,
            super::RemoteTransportError::ResponseTooLarge
        ));
    }

    #[cfg(unix)]
    #[test]
    fn unified_paired_exchange_uses_link_and_only_falls_back_when_unsent() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::os::unix::net::UnixListener;
        use std::sync::atomic::{AtomicUsize, Ordering};

        fn serve(root: &Path, reply: &'static [u8]) -> std::thread::JoinHandle<()> {
            let directory = root.join("multi-server");
            std::fs::create_dir_all(&directory).expect("peer-link directory");
            let listener = UnixListener::bind(directory.join("peer-link.sock"))
                .expect("fake peer-link socket");
            std::thread::spawn(move || {
                let (stream, _) = listener.accept().expect("request accepted");
                let mut reader = BufReader::new(stream);
                let mut line = Vec::new();
                reader.read_until(b'\n', &mut line).expect("request head");
                let head: Value = serde_json::from_slice(&line[..line.len() - 1])
                    .expect("Python-format request head");
                assert_eq!(head["address"], "100.81.2.3:4621");
                let body_len = head["bodyLength"].as_u64().expect("body length") as usize;
                let mut body = vec![0; body_len];
                reader.read_exact(&mut body).expect("request body");
                assert_eq!(body, b"signed-body");
                let mut stream = reader.into_inner();
                let separator = reply
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .expect("fake response head has a newline");
                let (head, rest) = reply.split_at(separator);
                let body = &rest[1..];
                stream
                    .write_all(head)
                    .and_then(|_| stream.write_all(b"\n"))
                    .and_then(|_| stream.write_all(body))
                    .expect("fake peer response");
            })
        }

        let headers = [("X-Studio-Signature", "fixture-signature")];
        let timeout = Duration::from_secs(2);

        let success_root = tempfile::tempdir().expect("success fake state");
        let success_client = PeerLinkClient::new(success_root.path());
        success_client.learn("peer", Some("100.81.2.3:4621"));
        let shared_client = PeerLinkClient::new(success_root.path());
        let success_peer = serve(
            success_root.path(),
            b"{\"status\":200,\"headers\":[]}\n\0\0\0\x05reply\0\0\0\0",
        );
        let fallback_calls = AtomicUsize::new(0);
        let response = exchange_paired_request_with(
            &shared_client,
            &super::PairedRequest {
                server_id: "peer",
                public_key: "fixture-key",
                method: "POST",
                url: "https://peer.example.test/api/test",
                headers: &headers,
                body: b"signed-body",
                timeout,
                response_limit: 1024,
            },
            "/api/test",
            || {
                fallback_calls.fetch_add(1, Ordering::SeqCst);
                Err(RemoteTransportError::ExchangeUnknown)
            },
        );
        success_peer.join().expect("success fake exits");
        assert_eq!(
            fallback_calls.load(Ordering::SeqCst),
            0,
            "learned route used QUIC"
        );
        let response = response.expect("QUIC response succeeds");
        assert_eq!(response.body, b"reply");
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);

        let unsent_root = tempfile::tempdir().expect("unsent fake state");
        let unsent_client = PeerLinkClient::new(unsent_root.path());
        unsent_client.learn("peer", Some("100.81.2.3:4621"));
        let unsent_peer = serve(
            unsent_root.path(),
            b"{\"failure\":\"unreachable\",\"sent\":false}\n",
        );
        let fallback_calls = AtomicUsize::new(0);
        let response = exchange_paired_request_with(
            &unsent_client,
            &super::PairedRequest {
                server_id: "peer",
                public_key: "fixture-key",
                method: "POST",
                url: "https://peer.example.test/api/test",
                headers: &headers,
                body: b"signed-body",
                timeout,
                response_limit: 1024,
            },
            "/api/test",
            || {
                fallback_calls.fetch_add(1, Ordering::SeqCst);
                Ok(HttpResponse {
                    status: 200,
                    headers: vec![],
                    body: b"https-fallback".to_vec(),
                })
            },
        )
        .expect("an unsent link request falls back to HTTPS");
        unsent_peer.join().expect("unsent fake exits");
        assert_eq!(response.body, b"https-fallback");
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);

        let sent_root = tempfile::tempdir().expect("sent fake state");
        let sent_client = PeerLinkClient::new(sent_root.path());
        sent_client.learn("peer", Some("100.81.2.3:4621"));
        let sent_peer = serve(
            sent_root.path(),
            b"{\"failure\":\"timeout\",\"sent\":true}\n",
        );
        let fallback_calls = AtomicUsize::new(0);
        let error = exchange_paired_request_with(
            &sent_client,
            &super::PairedRequest {
                server_id: "peer",
                public_key: "fixture-key",
                method: "POST",
                url: "https://peer.example.test/api/test",
                headers: &headers,
                body: b"signed-body",
                timeout,
                response_limit: 1024,
            },
            "/api/test",
            || {
                fallback_calls.fetch_add(1, Ordering::SeqCst);
                Ok(HttpResponse {
                    status: 200,
                    headers: vec![],
                    body: Vec::new(),
                })
            },
        );
        sent_peer.join().expect("sent fake exits");
        let error = error
            .err()
            .expect("sent link failure is an unknown outcome");
        assert!(matches!(error, RemoteTransportError::ExchangeUnknown));
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
    }

    #[cfg(unix)]
    #[test]
    fn peer_link_client_uses_the_python_unix_framing_with_a_local_fake_peer() {
        use serde_json::Value;
        use std::io::{BufRead, BufReader, Read, Write};
        use std::os::unix::net::UnixListener;
        use std::time::Duration;

        let test_root = std::env::var_os("CODEX_SERVER_TEST_TMP_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        std::fs::create_dir_all(&test_root).expect("isolated test root exists");
        let root = tempfile::Builder::new()
            .prefix("peer-link-client-")
            .tempdir_in(test_root)
            .expect("isolated state directory");
        let socket_directory = root.path().join("multi-server");
        std::fs::create_dir_all(&socket_directory).expect("socket directory exists");
        let socket = socket_directory.join("peer-link.sock");
        let listener = UnixListener::bind(&socket).expect("fake peer-link socket binds locally");
        let peer = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("local fake peer connects");
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader
                .read_until(b'\n', &mut line)
                .expect("request head reads");
            let head: Value =
                serde_json::from_slice(&line[..line.len() - 1]).expect("Python-format head parses");
            assert_eq!(head["address"], "100.80.1.2:4621");
            assert_eq!(head["publicKey"], "fixture-peer-key");
            assert_eq!(head["method"], "POST");
            assert_eq!(head["path"], "/api/servers/orchestration");
            assert_eq!(
                head["headers"][0],
                json!(["X-Studio-Signature", "fixture-signature"])
            );
            let body_length = head["bodyLength"].as_u64().expect("body length") as usize;
            let mut body = vec![0; body_length];
            reader
                .read_exact(&mut body)
                .expect("complete request body reads");
            assert_eq!(body, br#"{"requestId":"request-1"}"#);
            let mut stream = reader.into_inner();
            stream
                .write_all(
                    b"{\"status\":200,\"headers\":[[\"X-Studio-Link\",\"100.81.1.3:4621\"]]}\n",
                )
                .expect("response metadata writes");
            let response = br#"{"requestId":"request-1","outcome":"applied"}"#;
            stream
                .write_all(&(response.len() as u32).to_be_bytes())
                .and_then(|_| stream.write_all(response))
                .and_then(|_| stream.write_all(&0_u32.to_be_bytes()))
                .expect("framed response writes");
        });
        let client = super::PeerLinkClient::new(root.path());
        assert_eq!(
            super::parse_peer_link_advert("100.80.1.2:4621"),
            Some("100.80.1.2:4621".to_owned())
        );
        assert!(super::parse_peer_link_advert("192.0.2.1:4621").is_none());
        client.learn("peer-server", Some("100.80.1.2:4621"));
        let headers = [("X-Studio-Signature", "fixture-signature")];
        let response = client
            .exchange(super::PeerLinkExchange {
                server: "peer-server",
                public_key: "fixture-peer-key",
                method: "POST",
                path: "/api/servers/orchestration",
                headers: &headers,
                body: br#"{"requestId":"request-1"}"#,
                timeout: Duration::from_secs(2),
            })
            .expect("local protocol exchange completes")
            .expect("learned route uses the peer link");
        peer.join().expect("fake peer exits");
        assert_eq!(response.status, 200);
        assert_eq!(
            response.body,
            br#"{"requestId":"request-1","outcome":"applied"}"#
        );
        assert_eq!(response.headers[0].0, "X-Studio-Link");
    }

    #[cfg(unix)]
    #[test]
    fn peer_link_unsent_failure_allows_https_fallback_and_marks_route_down() {
        use serde_json::Value;
        use std::io::{BufRead, BufReader, Read, Write};
        use std::os::unix::net::UnixListener;
        use std::time::Duration;

        let test_root = std::env::var_os("CODEX_SERVER_TEST_TMP_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        std::fs::create_dir_all(&test_root).expect("isolated test root exists");
        let root = tempfile::Builder::new()
            .prefix("peer-link-fallback-")
            .tempdir_in(test_root)
            .expect("isolated fake peer state");
        let socket_directory = root.path().join("multi-server");
        std::fs::create_dir_all(&socket_directory).expect("socket directory exists");
        let socket = socket_directory.join("peer-link.sock");
        let listener = UnixListener::bind(&socket).expect("fake peer socket binds locally");
        let peer = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("fake peer accepts one request");
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).expect("head reads");
            let head: Value = serde_json::from_slice(&line[..line.len() - 1])
                .expect("Python peer-link head parses");
            let body_length = head["bodyLength"].as_u64().expect("body size") as usize;
            let mut body = vec![0; body_length];
            reader.read_exact(&mut body).expect("request body reads");
            let mut stream = reader.into_inner();
            stream
                .write_all(b"{\"failure\":\"unreachable\",\"sent\":false}\n")
                .expect("unsent failure writes");
        });
        let client = super::PeerLinkClient::new(root.path());
        client.learn("peer-server", Some("100.80.1.2:4621"));
        let headers = [("X-Studio-Signature", "fixture-signature")];
        assert!(matches!(
            client.exchange(super::PeerLinkExchange {
                server: "peer-server",
                public_key: "fixture-peer-key",
                method: "POST",
                path: "/api/servers/orchestration",
                headers: &headers,
                body: br#"{"requestId":"request-1"}"#,
                timeout: Duration::from_secs(2),
            }),
            Err(super::PeerLinkFailure { sent: false })
        ));
        peer.join().expect("fake peer exits");
        assert!(
            client
                .exchange(super::PeerLinkExchange {
                    server: "peer-server",
                    public_key: "fixture-peer-key",
                    method: "POST",
                    path: "/api/servers/orchestration",
                    headers: &headers,
                    body: br#"{"requestId":"request-1"}"#,
                    timeout: Duration::from_secs(2),
                })
                .expect("down route selects HTTPS fallback")
                .is_none()
        );
    }
}
