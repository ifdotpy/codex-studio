//! Incoming owner-paired requests. All authority comes from stored credentials,
//! a verified signature, and a trusted observation; caller identity headers alone
//! never establish ownership. The decision core has no effects.
use std::net::IpAddr;

use serde_json::{Value, json};
use studio_runtime_store::StoreError;
use subtle::ConstantTimeEq;
use thiserror::Error;

use crate::remote_transport::{self, MultiServerSigningInput};

pub const PAIR_PATH: &str = "/api/multi-server/v1/pair";
pub const AUTO_PAIR_PATH: &str = "/api/multi-server/v1/auto-pair";
pub const STATUS_PATH: &str = "/api/multi-server/v1/status";
pub const MAX_OBSERVATION_BYTES: usize = 1024 * 1024;
const SIGNED_HEADERS: [&str; 6] = [
    "x-studio-client",
    "x-studio-server",
    "x-studio-timestamp",
    "x-studio-nonce",
    "x-studio-request-id",
    "x-studio-signature",
];

#[derive(Debug, Error)]
pub enum AccessError {
    #[error("{message}")]
    Refused {
        status: u16,
        code: &'static str,
        message: &'static str,
    },
    #[error("invalid stored object {table}/{id}: {location}")]
    Stored {
        table: &'static str,
        id: String,
        location: String,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}
impl AccessError {
    pub fn status(&self) -> u16 {
        match self {
            Self::Refused { status, .. } => *status,
            _ => 503,
        }
    }
}
pub(crate) fn refused(status: u16, code: &'static str, message: &'static str) -> AccessError {
    AccessError::Refused {
        status,
        code,
        message,
    }
}
pub(crate) fn schema(schema: &str, value: &Value) -> Result<(), AccessError> {
    match studio_runtime_schema::first_error(schema, value) {
        Ok(None) => Ok(()),
        _ => Err(refused(
            400,
            "invalid_request",
            "Invalid multi-server request",
        )),
    }
}
pub(crate) fn stored(
    schema: &str,
    table: &'static str,
    id: &str,
    raw: &str,
) -> Result<Value, AccessError> {
    let invalid = |location: String| AccessError::Stored {
        table,
        id: id.to_owned(),
        location,
    };
    if raw.len() > remote_transport::MAX_RESPONSE_BYTES {
        return Err(invalid("record size".into()));
    }
    let value: Value = serde_json::from_str(raw).map_err(|_| invalid("JSON".into()))?;
    match studio_runtime_schema::first_error(schema, &value) {
        Ok(None) => Ok(value),
        Ok(Some((location, _))) => Err(invalid(location)),
        Err(_) => Err(invalid("schema".into())),
    }
}
pub(crate) fn text<'a>(value: &'a Value, field: &str) -> &'a str {
    value.get(field).and_then(Value::as_str).unwrap_or_default()
}
pub(crate) fn digest(raw: &[u8]) -> String {
    studio_runtime_store::sha256_hex(raw)
}
pub(crate) fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
}

/// Owned and bounded wire input. Header multiplicity is retained until checked.
pub struct SignedRequest {
    method: String,
    target: String,
    raw: Vec<u8>,
    client: String,
    server: String,
    timestamp: String,
    nonce: String,
    request: String,
    signature: String,
    body: Value,
}
impl SignedRequest {
    pub fn from_headers(
        method: &str,
        target: &str,
        headers: &[(String, String)],
        raw: &[u8],
    ) -> Result<Self, AccessError> {
        if raw.len() > remote_transport::MAX_REQUEST_BYTES || target.len() > 8192 {
            return Err(refused(413, "request_size", "Invalid request size"));
        }
        let mut values = Vec::with_capacity(SIGNED_HEADERS.len());
        for name in SIGNED_HEADERS {
            let mut matches = headers
                .iter()
                .filter(|(key, _)| key.eq_ignore_ascii_case(name));
            let Some((_, value)) = matches.next() else {
                return Err(refused(
                    401,
                    "invalid_credentials",
                    "A single signed credential header is required",
                ));
            };
            if matches.next().is_some() || value.is_empty() || value.len() > 512 {
                return Err(refused(
                    401,
                    "invalid_credentials",
                    "A single signed credential header is required",
                ));
            }
            values.push(value.clone());
        }
        let body = if raw.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(raw)
                .map_err(|_| refused(400, "invalid_request", "Invalid JSON request"))?
        };
        let mut request = Self {
            method: method.to_owned(),
            target: target.to_owned(),
            raw: raw.to_vec(),
            client: values[0].clone(),
            server: values[1].clone(),
            timestamp: values[2].clone(),
            nonce: values[3].clone(),
            request: values[4].clone(),
            signature: values[5].clone(),
            body,
        };
        if !identifier(&request.client) || !identifier(&request.request) {
            return Err(refused(400, "invalid_id", "Invalid request or client ID"));
        }
        if request.is_pairing() {
            schema("DevicePairRequest", &request.body)?;
        }
        if request.is_auto_pairing() {
            schema("AutoPairRequest", &request.body)?;
        }
        if request.is_auto_pairing()
            || (request.is_pairing() && text(&request.body, "kind") == "server")
        {
            request.body["origin"] = json!(normalize_origin(text(&request.body, "origin"))?);
        }
        Ok(request)
    }
    pub fn client_id(&self) -> &str {
        &self.client
    }
    pub fn request_id(&self) -> &str {
        &self.request
    }
    pub fn body(&self) -> &Value {
        &self.body
    }
    pub fn target(&self) -> &str {
        &self.target
    }
    pub fn nonce(&self) -> &str {
        &self.nonce
    }
    pub fn is_status(&self) -> bool {
        self.method == "GET" && self.target == STATUS_PATH
    }
    pub fn is_pairing(&self) -> bool {
        self.method == "POST" && self.target == PAIR_PATH
    }
    pub fn is_auto_pairing(&self) -> bool {
        self.method == "POST" && self.target == AUTO_PAIR_PATH
    }
    pub fn fingerprint(&self) -> String {
        digest(
            &[
                self.method.as_bytes(),
                b"\n",
                self.target.as_bytes(),
                b"\n",
                &self.raw,
            ]
            .concat(),
        )
    }
}

/// Observations are supplied by the host's trusted Tailscale adapter, never the
/// request body. The request IP is checked against the observation's lookup key.
pub struct OwnerObservation {
    address: IpAddr,
    login: String,
    dns: Option<String>,
}
impl OwnerObservation {
    pub fn from_json(
        address: &str,
        supplied_logins: &[String],
        status: &Value,
        whois: &Value,
    ) -> Result<Self, AccessError> {
        let unavailable = || {
            refused(
                403,
                "identity_unavailable",
                "The Tailscale client identity is unavailable",
            )
        };
        let address: IpAddr = address.parse().map_err(|_| unavailable())?;
        if !tailnet_address(address)
            || status.to_string().len() > MAX_OBSERVATION_BYTES
            || whois.to_string().len() > MAX_OBSERVATION_BYTES
        {
            return Err(unavailable());
        }
        let node = status
            .get("Self")
            .filter(|v| v.is_object())
            .ok_or_else(unavailable)?;
        let peer = whois
            .get("Node")
            .filter(|v| v.is_object())
            .ok_or_else(unavailable)?;
        let user = node
            .get("UserID")
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string())
            })
            .ok_or_else(unavailable)?;
        let owner = status
            .get("User")
            .and_then(|v| v.get(&user))
            .and_then(|v| v.get("LoginName"))
            .and_then(Value::as_str)
            .ok_or_else(unavailable)?;
        let login = whois
            .get("UserProfile")
            .and_then(|v| v.get("LoginName"))
            .and_then(Value::as_str)
            .ok_or_else(unavailable)?;
        let tagged = |v: &Value| {
            v.get("Tags").is_some_and(|tags| {
                !tags.is_null() && tags.as_array().is_none_or(|a| !a.is_empty())
            })
        };
        if status.get("BackendState").and_then(Value::as_str) != Some("Running")
            || tagged(node)
            || tagged(peer)
            || [owner, login]
                .iter()
                .any(|v| v.trim().is_empty() || v.len() > 254)
        {
            return Err(unavailable());
        }
        let login = login.trim().to_lowercase();
        if owner.trim().to_lowercase() != login
            || supplied_logins.len() > 1
            || supplied_logins
                .first()
                .is_some_and(|v| v.trim().to_lowercase() != login)
        {
            return Err(refused(
                403,
                "owner_mismatch",
                "The Tailscale client must be the server owner",
            ));
        }
        let dns = peer
            .get("Name")
            .or_else(|| peer.get("DNSName"))
            .and_then(Value::as_str)
            .map(|v| v.trim_end_matches('.').to_lowercase());
        Ok(Self {
            address,
            login,
            dns,
        })
    }
    pub fn address(&self) -> IpAddr {
        self.address
    }
    pub fn login(&self) -> &str {
        &self.login
    }
}
pub fn tailnet_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(a) => u32::from(a) & 0xffc0_0000 == 0x6440_0000,
        IpAddr::V6(a) => {
            u128::from(a) & (!0_u128 << 80) == 0xfd7a_115c_a1e0_0000_0000_0000_0000_0000
        }
    }
}
fn origin_url(origin: &str) -> Result<url::Url, AccessError> {
    let invalid = || {
        refused(
            400,
            "invalid_origin",
            "Use an HTTPS Tailscale origin without a path",
        )
    };
    let url = url::Url::parse(origin).map_err(|_| invalid())?;
    if !origin.is_ascii()
        || origin.contains(['@', '\\'])
        || url.scheme() != "https"
        || url.host_str().is_none_or(|host| {
            !host.ends_with(".ts.net")
                || !host
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-".contains(&b))
        })
        || !matches!(url.path(), "" | "/")
        || url.query().is_some_and(|v| !v.is_empty())
        || url.fragment().is_some_and(|v| !v.is_empty())
        || url.port() == Some(0)
    {
        return Err(invalid());
    }
    Ok(url)
}
pub(crate) fn normalize_origin(origin: &str) -> Result<String, AccessError> {
    Ok(origin_url(origin)?.origin().ascii_serialization())
}
pub(crate) fn origin_host(origin: &str) -> Result<String, AccessError> {
    origin_url(origin)?
        .host_str()
        .map(str::to_owned)
        .ok_or_else(|| refused(400, "invalid_origin", "Invalid Tailscale origin"))
}

/// Only constructed from validated stored records, preserving unknown fields.
pub struct AccessSnapshot {
    client: Option<Value>,
    invite: Option<Value>,
    auto_pair: bool,
}
impl AccessSnapshot {
    pub fn from_records(
        client_id: &str,
        client: Option<&str>,
        invite_id: &str,
        invite: Option<&str>,
        auto_pair: bool,
    ) -> Result<Self, AccessError> {
        let client = client
            .map(|v| {
                stored(
                    "PersistedAccessClient",
                    "runtime_access_clients",
                    client_id,
                    v,
                )
            })
            .transpose()?;
        let invite = invite
            .map(|v| {
                stored(
                    "PersistedAccessInvite",
                    "runtime_access_invites",
                    invite_id,
                    v,
                )
            })
            .transpose()?;
        if client
            .as_ref()
            .is_some_and(|v| text(v, "id") != client_id || text(v, "clientId") != client_id)
            || invite
                .as_ref()
                .is_some_and(|v| text(v, "inviteId") != invite_id)
        {
            return Err(refused(
                503,
                "invalid_record",
                "Stored access identity differs from its row",
            ));
        }
        Ok(Self {
            client,
            invite,
            auto_pair,
        })
    }
    pub(crate) fn client(&self) -> Option<&Value> {
        self.client.as_ref()
    }
    pub(crate) fn invite(&self) -> Option<&Value> {
        self.invite.as_ref()
    }
}
#[derive(Clone)]
pub struct VerifiedPrincipal {
    record: Value,
    client: String,
    request: String,
    server: String,
    kind: String,
    login: String,
    fingerprint: String,
    pairing: bool,
    auto_pairing: bool,
}
impl VerifiedPrincipal {
    pub fn value(&self) -> &Value {
        &self.record
    }
    pub fn client_id(&self) -> &str {
        &self.client
    }
    pub fn request_id(&self) -> &str {
        &self.request
    }
    pub fn target_server_id(&self) -> &str {
        &self.server
    }
    pub fn kind(&self) -> &str {
        &self.kind
    }
    pub fn tailscale_user(&self) -> &str {
        &self.login
    }
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    pub fn pairing(&self) -> bool {
        self.pairing
    }
    pub fn auto_pairing(&self) -> bool {
        self.auto_pairing
    }
}
pub struct AuthDecision {
    pub principal: VerifiedPrincipal,
    pub client_write: Option<Value>,
    pub unpaired_attempt: bool,
}

pub fn authorize(
    snapshot: &AccessSnapshot,
    request: &SignedRequest,
    local_server: &str,
    owner: &OwnerObservation,
    auto_probe: Option<&Value>,
    now: i64,
) -> Result<AuthDecision, AccessError> {
    if request.server != local_server {
        return Err(refused(
            401,
            "server_mismatch",
            "The target server identity does not match",
        ));
    }
    let timestamp = request.timestamp.parse::<i64>().map_err(|_| {
        refused(
            401,
            "clock_difference",
            "The request timestamp is outside the allowed clock difference",
        )
    })?;
    if request.timestamp.len() > 12
        || !request.timestamp.bytes().all(|b| b.is_ascii_digit())
        || remote_transport::validate_multi_server_nonce(&request.nonce, timestamp, now).is_err()
    {
        return Err(refused(
            401,
            "invalid_nonce",
            "The request timestamp or nonce is invalid",
        ));
    }
    let pairing = request.is_pairing();
    let auto = request.is_auto_pairing();
    let client = snapshot.client.as_ref();
    if client.is_some_and(|v| text(v, "status") == "revoked") {
        return Err(refused(403, "client_revoked", "The client is revoked"));
    }
    let fingerprint = request.fingerprint();
    let recovery = pairing
        && client.is_some_and(|v| {
            text(v, "pairingRequestId") == request.request && text(v, "pairingHash") == fingerprint
        });
    let (key, kind) = if pairing || auto {
        let field = if pairing { "clientId" } else { "serverId" };
        if text(&request.body, field) != request.client
            || text(&request.body, "requestId") != request.request
        {
            return Err(refused(
                400,
                "invalid_pairing",
                "The pairing identity or protocol does not match",
            ));
        }
        let kind = if auto {
            "server"
        } else {
            text(&request.body, "kind")
        };
        if kind == "server" {
            origin_host(text(&request.body, "origin"))?;
        }
        if pairing && !recovery {
            check_invite(snapshot.invite.as_ref(), &request.body, None, now)?;
        }
        if auto {
            if !snapshot.auto_pair {
                return Err(refused(
                    403,
                    "auto_pair_disabled",
                    "Automatic server pairing is disabled",
                ));
            }
            if client.is_some_and(|v| {
                text(v, "publicKey") != text(&request.body, "publicKey")
                    || text(v, "kind") != "server"
                    || text(v, "origin") != text(&request.body, "origin")
            }) {
                return Err(refused(
                    403,
                    "identity_mismatch",
                    "The registered server identity differs",
                ));
            }
            let probe = auto_probe.ok_or_else(|| {
                refused(
                    501,
                    "owner_lookup",
                    "Not implemented: requires remote-transport/owner-lookup",
                )
            })?;
            schema("DiscoveryIdentity", probe)?;
            if owner.dns.as_deref() != Some(origin_host(text(&request.body, "origin"))?.as_str())
                || text(probe, "serverId") != request.client
                || text(probe, "publicKey") != text(&request.body, "publicKey")
                || text(probe, "origin") != text(&request.body, "origin")
                || text(probe, "tailscaleUser").trim().to_lowercase() != owner.login
            {
                return Err(refused(
                    403,
                    "identity_mismatch",
                    "The caller identity does not match its proof",
                ));
            }
            if probe.get("autoPair") != Some(&Value::Bool(true)) {
                return Err(refused(
                    403,
                    "auto_pair_disabled",
                    "Automatic server pairing is disabled on the caller",
                ));
            }
        }
        (text(&request.body, "publicKey"), kind)
    } else {
        let c = client
            .filter(|v| text(v, "status") == "paired")
            .ok_or_else(|| {
                refused(
                    403,
                    "client_revoked",
                    "The client is not paired or is revoked",
                )
            })?;
        (text(c, "publicKey"), text(c, "kind"))
    };
    remote_transport::verify_signature(
        key,
        &remote_transport::multi_server_signing_bytes(MultiServerSigningInput {
            method: &request.method,
            target: &request.target,
            server_id: &request.server,
            client_id: &request.client,
            timestamp: &request.timestamp,
            nonce: &request.nonce,
            request_id: &request.request,
            body: &request.raw,
        }),
        &request.signature,
    )
    .map_err(|_| refused(401, "invalid_signature", "The request signature is invalid"))?;
    if client
        .and_then(|v| v.get("tailscaleUser"))
        .and_then(Value::as_str)
        .is_some_and(|v| v != owner.login)
    {
        return Err(refused(
            403,
            "owner_mismatch",
            "The Tailscale client must be the server owner",
        ));
    }
    let client_write = if !pairing && !auto {
        client.cloned().map(|mut v| {
            v["lastAccess"] = json!(now);
            v
        })
    } else {
        None
    };
    let mut record = if pairing || auto {
        json!({"clientId":request.client,"kind":kind,"publicKey":key})
    } else {
        client.cloned().unwrap_or(Value::Null)
    };
    if let Some(fields) = record.as_object_mut() {
        fields.extend(serde_json::Map::from_iter([
            ("clientId".to_owned(), json!(request.client)),
            (
                "serverId".to_owned(),
                if kind == "server" {
                    json!(request.client)
                } else {
                    Value::Null
                },
            ),
            ("targetServerId".to_owned(), json!(local_server)),
            ("requestId".to_owned(), json!(request.request)),
            ("tailscaleUser".to_owned(), json!(owner.login)),
            ("pairing".to_owned(), json!(pairing)),
            ("autoPairing".to_owned(), json!(auto)),
        ]));
    }
    Ok(AuthDecision {
        principal: VerifiedPrincipal {
            record,
            client: request.client.clone(),
            request: request.request.clone(),
            server: local_server.to_owned(),
            kind: kind.to_owned(),
            login: owner.login.clone(),
            fingerprint,
            pairing,
            auto_pairing: auto,
        },
        client_write,
        unpaired_attempt: (pairing || auto) && !recovery,
    })
}
pub(crate) fn check_invite(
    invite: Option<&Value>,
    body: &Value,
    owner: Option<&str>,
    now: i64,
) -> Result<(), AccessError> {
    let invite = invite
        .filter(|v| {
            text(v, "status") == "open"
                && v.get("expires")
                    .and_then(Value::as_f64)
                    .is_some_and(|v| v >= now as f64)
        })
        .ok_or_else(|| {
            refused(
                403,
                "invite_unavailable",
                "The invitation is expired or was already used",
            )
        })?;
    if !bool::from(
        text(invite, "tokenHash")
            .as_bytes()
            .ct_eq(digest(text(body, "token").as_bytes()).as_bytes()),
    ) {
        return Err(refused(
            403,
            "invalid_invite",
            "The invitation secret is invalid",
        ));
    }
    if owner.is_some_and(|login| text(invite, "tailscaleUser") != login) {
        return Err(refused(
            403,
            "owner_mismatch",
            "The invitation owner does not match",
        ));
    }
    Ok(())
}
