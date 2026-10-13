//! Signed outbound exchange primitives for multi-server and federation peers.
//!
//! The two protocols have different signing inputs and remain separate here.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use rustls_pki_types::{PrivatePkcs8KeyDer, SubjectPublicKeyInfoDer, pem::PemObject};
use serde_json::{Map, Value};
use spki::{ObjectIdentifier, SubjectPublicKeyInfoRef};
use std::fmt;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

use crate::event_write::python_json_compact;
use studio_runtime_store::{RemoteTransportRepository, RuntimeStore, StoreError, sha256_hex};

pub const CLOCK_SKEW_SECONDS: i64 = 300;
include!("multi_server_transport_limits.rs");
pub const MAX_IDENTITY_BYTES: usize = 16 * 1024;

const MULTI_SERVER_DOMAIN: &str = "studio-multi-server-v1";
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
pub fn load_federation_identity(store: &RuntimeStore) -> RemoteTransportResult<RemoteIdentity> {
    const TABLE: &str = "runtime_federation_identity";
    const KEY: &str = "local";
    let invalid = || RemoteTransportError::InvalidStoredObject {
        table: TABLE,
        key: KEY.to_owned(),
    };
    let connection = store.read().map_err(|_| invalid())?;
    let Some(record) =
        RemoteTransportRepository::federation_identity(&connection).map_err(|_| invalid())?
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
        "{MULTI_SERVER_DOMAIN}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{digest}",
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
pub fn exchange_multi_server(
    store: &RuntimeStore,
    identity: &RemoteIdentity,
    input: &MultiServerExchangeInput<'_>,
    now: i64,
) -> RemoteTransportResult<Value> {
    exchange_multi_server_with(
        store,
        identity,
        input,
        now,
        |method, url, headers, body, timeout, limit| {
            SignedHttpClient::exchange(method, url, headers, body, timeout, limit)
        },
    )
}

fn exchange_multi_server_with(
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
    let reserved = store.transaction_with(|transaction| {
        RemoteTransportRepository::reserve_multi_server_request(
            transaction,
            input.server_id,
            input.request_id,
            &fingerprint,
            now as f64,
        )
        .map_err(|error| map_outbound_error(error, input.server_id, input.request_id))
    })?;
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
    store.transaction_with(|transaction| {
        if invitation_response {
            RemoteTransportRepository::redact_multi_server_request(
                transaction,
                input.server_id,
                input.request_id,
                &fingerprint,
                now as f64,
            )
        } else {
            RemoteTransportRepository::complete_multi_server_request(
                transaction,
                input.server_id,
                input.request_id,
                &fingerprint,
                &value,
                now as f64,
            )
        }
        .map_err(|error| map_outbound_error(error, input.server_id, input.request_id))
    })?;
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

pub fn new_nonce() -> RemoteTransportResult<String> {
    let mut random = [0u8; 24];
    getrandom::fill(&mut random).map_err(|_| RemoteTransportError::InvalidInput)?;
    Ok(URL_SAFE_NO_PAD.encode(random))
}

pub fn consume_federation_nonce(
    store: &RuntimeStore,
    peer_id: &str,
    nonce: &str,
    timestamp: i64,
    now: i64,
) -> RemoteTransportResult<()> {
    validate_federation_nonce(nonce, timestamp, now)?;
    store.transaction_with(|transaction| {
        let accepted = RemoteTransportRepository::consume_federation_nonce(
            transaction,
            peer_id,
            nonce,
            timestamp,
            now,
        )?;
        if accepted {
            Ok(())
        } else {
            Err(RemoteTransportError::Replay)
        }
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MultiServerPairingContext {
    Paired,
    Pairing,
    AutoPairing,
    Recovery,
}

pub fn consume_multi_server_nonce(
    store: &RuntimeStore,
    client_id: &str,
    nonce: &str,
    timestamp: i64,
    now: i64,
    pairing: MultiServerPairingContext,
) -> RemoteTransportResult<()> {
    validate_multi_server_nonce(nonce, timestamp, now)?;
    store.transaction_with(|transaction| {
        let accepted = RemoteTransportRepository::consume_access_nonce(
            transaction,
            client_id,
            nonce,
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
}

pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
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
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x0002_0000); // O_NOFOLLOW
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x0000_0100); // O_NOFOLLOW
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly",
    )))]
    {
        let _ = options;
        return Err(RemoteTransportError::IdentityStorage);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{
        MultiServerExchangeInput, MultiServerRequest, MultiServerSigningInput, RemoteIdentity,
        canonical_json, ed25519_public_key, federation_message_signing_bytes,
        federation_signed_headers, federation_signing_bytes, load_multi_server_identity,
        multi_server_request_fingerprint, multi_server_signed_headers, multi_server_signing_bytes,
        new_nonce, validate_federation_nonce, validate_https_url, validate_multi_server_nonce,
        verify_federation_response, verify_multi_server_response, verify_signature,
    };
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use ring::signature::{ED25519, UnparsedPublicKey};
    use serde_json::json;
    use std::path::PathBuf;
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
        let mut der = vec![
            0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
        ];
        der.extend_from_slice(&PUBLIC);
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

    pub(crate) fn runtime_store_fixture() -> (PathBuf, RuntimeStore) {
        use std::fs;
        use std::time::{SystemTime, UNIX_EPOCH};

        let root = std::env::temp_dir().join(format!(
            "studio-remote-transport-store-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock is after epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("temporary state directory creates");
        let snapshot: serde_json::Value = serde_json::from_str(include_str!(
            "../../../apps/server/tests/fixtures/runtime-state-schema.json"
        ))
        .expect("committed state schema parses");
        for name in ["canvas.sqlite3", "analytics.sqlite3"] {
            let path = root.join(name);
            let connection =
                studio_runtime_store::Connection::open(path).expect("fixture database opens");
            connection
                .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
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
        }
        let store = RuntimeStore::open(Some(&root)).expect("schema fixture opens");
        (root, store)
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
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("studio-remote-identity-{unique}"));
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
        fs::remove_dir_all(root).expect("fixture identity tree is removed");
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

    #[test]
    fn refusal_rolls_back_nonce_cleanup_through_the_service_transaction() {
        let (root, store) = runtime_store_fixture();
        let now = 1_700_000_000_i64;
        store
            .transaction(|transaction| {
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
            .expect("fixture rows commit");
        assert!(matches!(
            super::consume_federation_nonce(
                &store,
                "peer-a",
                "incoming-federation-nonce",
                now,
                now,
            ),
            Err(super::RemoteTransportError::Replay)
        ));
        let connection = store.read().expect("read transaction opens");
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
            .transaction(|transaction| {
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
            .expect("unpaired cap fixture rows commit");
        assert!(matches!(
            super::consume_multi_server_nonce(
                &store,
                "new-client",
                "abcdefghijklmnopqrstuvwxyz012345",
                now,
                now,
                super::MultiServerPairingContext::Pairing,
            ),
            Err(super::RemoteTransportError::Replay)
        ));
        let connection = store.read().expect("multi-server stale row reads");
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
        std::fs::remove_dir_all(root).expect("temporary state is removed");
    }

    #[test]
    fn invalid_stored_federation_identity_names_its_table_and_primary_key() {
        let (root, store) = runtime_store_fixture();
        store
            .transaction(|transaction| {
                transaction.execute(
                    "INSERT INTO runtime_federation_identity(id,record) VALUES('local','{}')",
                    [],
                )?;
                Ok(())
            })
            .expect("malformed identity fixture commits");
        let error = super::load_federation_identity(&store).expect_err("malformed key is refused");
        assert!(matches!(
            error,
            super::RemoteTransportError::InvalidStoredObject {
                table: "runtime_federation_identity",
                key
            } if key == "local"
        ));
        drop(store);
        std::fs::remove_dir_all(root).expect("temporary state is removed");
    }

    #[test]
    fn exchange_service_replays_saved_receipt_and_never_sends_unknown_after_restart() {
        let (root, store) = runtime_store_fixture();
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
                    body: response.clone(),
                })
            },
        )
        .expect("new exchange verifies and completes");
        drop(store);

        let store = RuntimeStore::open(Some(&root)).expect("saved receipt opens after restart");
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
        .expect("saved result is replayed without transport");
        assert_eq!(first, replay);
        assert_eq!(sends, 1);
        drop(store);

        let store =
            RuntimeStore::open(Some(&root)).expect("unknown reservation opens after restart");
        let fingerprint =
            super::multi_server_request_fingerprint("POST", input.url, br#"{"other":"body"}"#)
                .expect("unknown fixture fingerprint computes");
        store
            .transaction(|transaction| {
                RemoteTransportRepository::reserve_multi_server_request(
                    transaction,
                    "server-peer",
                    "unknown-request",
                    &fingerprint,
                    1_700_000_000.0,
                )?;
                Ok(())
            })
            .expect("unknown reservation commits");
        drop(store);
        let store = RuntimeStore::open(Some(&root)).expect("unknown receipt reopens");
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
        );
        assert!(matches!(
            result,
            Err(super::RemoteTransportError::ExchangeUnknown)
        ));
        assert_eq!(sends, 1, "unknown request identity is never sent again");
        drop(store);
        std::fs::remove_dir_all(root).expect("temporary state is removed");
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

        let mismatch = json!({"identity":{"serverId":"other-peer"}});
        let body = serde_json::to_vec(&mismatch).expect("response serializes");
        assert!(verify_multi_server_response(&body, "server-peer", &spki_pem()).is_err());
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
}
