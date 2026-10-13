//! Atomic incoming access effects. Pairing consumes the invite and saves its
//! exact response together; restart never needs to repeat a pairing effect.
use base64::Engine;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use studio_runtime_store::{RemoteTransportRepository, RuntimeStore};

use crate::multi_server_auth::{
    self as auth, AccessError, AccessSnapshot, OwnerObservation, SignedRequest, VerifiedPrincipal,
    refused, text,
};

const PAIRING_WINDOW_SECONDS: i64 = 60;
const MAX_PAIRING_ATTEMPTS: usize = 128;

/// Host adapter must bind observations to the requested address, use a five
/// second deadline and cap each command/probe response at one MiB. No process
/// or network is invoked by the service; fixture adapters use generated data.
pub trait OwnerLookup: Send + Sync {
    fn observe(
        &self,
        address: &str,
        supplied_logins: &[String],
        auto_pair_origin: Option<&str>,
    ) -> Result<(OwnerObservation, Option<Value>), AccessError>;
}

type IdentitySource = dyn Fn(&RuntimeStore) -> Result<Value, AccessError> + Send + Sync;

pub struct MultiServerBoundary {
    identity: Arc<IdentitySource>,
    owner: Arc<dyn OwnerLookup>,
    pairing_attempts: Mutex<VecDeque<i64>>,
}
impl MultiServerBoundary {
    pub fn new(identity: Value, owner: Arc<dyn OwnerLookup>) -> Result<Self, AccessError> {
        auth::schema("ServerIdentityResponse", &identity)?;
        if !auth::identifier(text(&identity, "serverId")) || text(&identity, "publicKey").is_empty()
        {
            return Err(refused(
                503,
                "identity_unavailable",
                "The server identity is unavailable",
            ));
        }
        auth::origin_host(text(&identity, "origin"))?;
        Ok(Self {
            identity: Arc::new(move |_| Ok(identity.clone())),
            owner,
            pairing_attempts: Mutex::new(VecDeque::new()),
        })
    }
    /// Read the credentials service's current public projection at request time,
    /// outside the transaction, so a committed rename is visible immediately.
    pub fn with_identity_source(
        mut self,
        source: impl Fn(&RuntimeStore) -> Result<Value, AccessError> + Send + Sync + 'static,
    ) -> Self {
        self.identity = Arc::new(source);
        self
    }
    pub fn observe(
        &self,
        request: &SignedRequest,
        address: &str,
        logins: &[String],
    ) -> Result<(OwnerObservation, Option<Value>), AccessError> {
        let (owner, probe) = self.owner.observe(
            address,
            logins,
            request
                .is_auto_pairing()
                .then(|| text(request.body(), "origin")),
        )?;
        if address.parse::<std::net::IpAddr>().ok() != Some(owner.address()) {
            return Err(refused(
                403,
                "owner_mismatch",
                "The identity observation belongs to another address",
            ));
        }
        Ok((owner, probe))
    }
    pub fn discovery_identity(
        &self,
        store: &RuntimeStore,
        address: &str,
        logins: &[String],
    ) -> Result<Value, AccessError> {
        let (owner, _) = self.owner.observe(address, logins, None)?;
        if address.parse::<std::net::IpAddr>().ok() != Some(owner.address()) {
            return Err(refused(
                403,
                "owner_mismatch",
                "The identity observation belongs to another address",
            ));
        }
        let mut identity = (self.identity)(store)?;
        auth::schema("ServerIdentityResponse", &identity)?;
        let enabled = store.read_transaction(|tx| {
            Ok(tx.query_row(
                "SELECT auto_pair FROM runtime_access_settings WHERE id=1",
                [],
                |r| r.get::<_, bool>(0),
            )?)
        })?;
        identity["protocol"] = json!(1);
        identity["autoPair"] = json!(enabled);
        identity["tailscaleUser"] = json!(owner.login());
        Ok(identity)
    }
    pub fn handle(
        &self,
        store: &RuntimeStore,
        request: &SignedRequest,
        owner: &OwnerObservation,
        probe: Option<&Value>,
        now: i64,
    ) -> Result<Value, AccessError> {
        if !request.is_status() && !request.is_pairing() && !request.is_auto_pairing() {
            return Err(refused(
                501,
                "access_dispatch",
                "Not implemented: requires multi-server-runtime/signed-dispatch",
            ));
        }
        if request.is_pairing() || request.is_auto_pairing() {
            let mut attempts = self
                .pairing_attempts
                .lock()
                .map_err(|_| refused(503, "pairing_unavailable", "Pairing is unavailable"))?;
            while attempts
                .front()
                .is_some_and(|v| *v <= now.saturating_sub(PAIRING_WINDOW_SECONDS))
            {
                attempts.pop_front();
            }
            if attempts.len() >= MAX_PAIRING_ATTEMPTS {
                return Err(refused(
                    429,
                    "pairing_limit",
                    "The server pairing attempt limit is reached",
                ));
            }
            attempts.push_back(now);
        }
        let identity = (self.identity)(store)?;
        auth::schema("ServerIdentityResponse", &identity)?;
        store.transaction_with(|tx| {
            let client = read_record(tx, "runtime_access_clients", request.client_id())?;
            let invite_id = text(request.body(), "inviteId");
            let invite = if request.is_pairing() {
                read_record(tx, "runtime_access_invites", invite_id)?
            } else {
                None
            };
            let auto_pair: bool = tx.query_row(
                "SELECT auto_pair FROM runtime_access_settings WHERE id=1",
                [],
                |r| r.get(0),
            )?;
            let snapshot = AccessSnapshot::from_records(
                request.client_id(),
                client.as_deref(),
                invite_id,
                invite.as_deref(),
                auto_pair,
            )?;
            let decision = auth::authorize(
                &snapshot,
                request,
                text(&identity, "serverId"),
                owner,
                probe,
                now,
            )?;
            if !RemoteTransportRepository::consume_access_nonce(
                tx,
                request.client_id(),
                request.nonce(),
                now,
                decision.unpaired_attempt,
            )? {
                return Err(refused(
                    401,
                    "nonce_replay",
                    "The request nonce was already used or its limit was reached",
                ));
            }
            if let Some(client) = decision.client_write {
                tx.execute(
                    "UPDATE runtime_access_clients SET record=?1 WHERE id=?2",
                    params![client.to_string(), request.client_id()],
                )?;
                return Ok(identity.clone());
            }
            apply_pair(tx, &snapshot, request, &decision.principal, &identity, now)
        })
    }
}

fn apply_pair(
    tx: &Transaction<'_>,
    snapshot: &AccessSnapshot,
    request: &SignedRequest,
    principal: &VerifiedPrincipal,
    identity: &Value,
    now: i64,
) -> Result<Value, AccessError> {
    let fingerprint = request.fingerprint();
    let receipt=tx.query_row("SELECT hash,status,record FROM runtime_access_receipts WHERE client=?1 AND request_id=?2",params![request.client_id(),request.request_id()],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?))).optional()?;
    let prefix = if request.is_pairing() {
        "pairing"
    } else {
        "autoPairing"
    };
    if let Some((hash, status, record)) = receipt {
        let record = auth::stored(
            "PersistedAccessReceipt",
            "runtime_access_receipts",
            request.request_id(),
            &record,
        )?;
        if hash != fingerprint {
            return Err(refused(
                409,
                "request_conflict",
                "The request ID has different content",
            ));
        }
        if status == "tombstone"
            || (status == "complete"
                && record["completed"]
                    .as_f64()
                    .is_some_and(|completed| completed < (now - 7 * 86400) as f64))
        {
            return Err(refused(
                410,
                "receipt_expired",
                "The saved response expired. The operation must not run again",
            ));
        }
        if status == "complete" {
            if record["completed"].as_f64().is_none() {
                return Err(refused(
                    503,
                    "receipt_invalid",
                    "The saved pairing response is invalid",
                ));
            }
            let raw = base64::engine::general_purpose::STANDARD
                .decode(text(&record, "body"))
                .map_err(|_| {
                    refused(
                        503,
                        "receipt_invalid",
                        "The saved pairing response is invalid",
                    )
                })?;
            let result: Value = serde_json::from_slice(&raw).map_err(|_| {
                refused(
                    503,
                    "receipt_invalid",
                    "The saved pairing response is invalid",
                )
            })?;
            auth::stored(
                "DevicePairResponse",
                "runtime_access_receipts",
                request.request_id(),
                &result.to_string(),
            )?;
            if record["status"].as_u64() != Some(200)
                || text(&result, "clientId") != request.client_id()
            {
                return Err(refused(
                    503,
                    "receipt_invalid",
                    "The saved pairing response is invalid",
                ));
            }
            return Ok(result);
        }
        if let Some(client) = snapshot.client()
            && text(client, &format!("{prefix}RequestId")) == request.request_id()
            && text(client, &format!("{prefix}Hash")) == fingerprint
            && let Some(result) = client
                .get(format!("{prefix}Receipt"))
                .filter(|v| v.is_object())
        {
            auth::stored(
                "DevicePairResponse",
                "runtime_access_clients",
                request.client_id(),
                &result.to_string(),
            )?;
            save_receipt(tx, request, &fingerprint, result, now)?;
            return Ok(result.clone());
        }
        // Only Python's explicit no-effect retry state permits execution.
        if status != "retry" {
            return Err(refused(
                409,
                "outcome_unknown",
                "The operation outcome is unknown. Recover its existing receipt",
            ));
        }
    }
    let body = request.body();
    let mut result = identity.clone();
    let fields = result.as_object_mut().ok_or_else(|| {
        refused(
            503,
            "identity_unavailable",
            "The server identity is unavailable",
        )
    })?;
    fields.extend(serde_json::Map::from_iter([
        ("protocol".to_owned(), json!(1)),
        ("clientId".to_owned(), json!(request.client_id())),
        (
            "tailscaleUser".to_owned(),
            json!(principal.tailscale_user()),
        ),
        ("paired".to_owned(), json!(true)),
    ]));
    let mut client = if request.is_pairing() {
        auth::check_invite(
            snapshot.invite(),
            body,
            Some(principal.tailscale_user()),
            now,
        )?;
        if snapshot.client().is_some() {
            return Err(refused(
                409,
                "client_exists",
                "Use a new client identity for a new pairing",
            ));
        }
        let mut invite = snapshot
            .invite()
            .cloned()
            .ok_or_else(|| refused(403, "invite_unavailable", "The invitation is unavailable"))?;
        invite["status"] = json!("used");
        tx.execute(
            "UPDATE runtime_access_invites SET record=?1 WHERE id=?2",
            params![invite.to_string(), text(body, "inviteId")],
        )?;
        new_client(request, principal, now)
    } else {
        snapshot
            .client()
            .cloned()
            .unwrap_or_else(|| new_client(request, principal, now))
    };
    client[format!("{prefix}RequestId")] = json!(request.request_id());
    client[format!("{prefix}Hash")] = json!(fingerprint);
    client[format!("{prefix}Receipt")] = result.clone();
    // Preserve the stored response in the Python client's recovery fields. This
    // is the commit evidence even when the outer response is lost.
    auth::stored(
        "PersistedAccessClient",
        "runtime_access_clients",
        request.client_id(),
        &client.to_string(),
    )?;
    tx.execute("INSERT INTO runtime_access_clients(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![request.client_id(),client.to_string()])?;
    save_receipt(tx, request, &fingerprint, &result, now)?;
    if snapshot.client().is_none() {
        tx.execute(
            "INSERT INTO runtime_access_audit(client,actor,action,created) VALUES(?1,?1,?2,?3)",
            params![
                request.client_id(),
                if request.is_pairing() {
                    "pair"
                } else {
                    "auto_pair"
                },
                now
            ],
        )?;
    }
    Ok(result)
}
fn save_receipt(
    tx: &Transaction<'_>,
    request: &SignedRequest,
    fingerprint: &str,
    result: &Value,
    now: i64,
) -> Result<(), AccessError> {
    let encoded = crate::remote_transport::canonical_json(result)
        .map_err(|_| refused(503, "receipt_invalid", "The pairing receipt is invalid"))?;
    let receipt = json!({"status":200,"headers":[["content-type","application/json"],["content-length",encoded.len().to_string()],["cache-control","no-store"]],"body":base64::engine::general_purpose::STANDARD.encode(encoded.as_bytes()),"completed":now});
    tx.execute("INSERT INTO runtime_access_receipts(client,request_id,hash,session,status,record) VALUES(?1,?2,?3,'rust','complete',?4) ON CONFLICT(client,request_id) DO UPDATE SET status='complete',record=excluded.record",params![request.client_id(),request.request_id(),fingerprint,receipt.to_string()])?;
    Ok(())
}
fn new_client(request: &SignedRequest, principal: &VerifiedPrincipal, now: i64) -> Value {
    let server = principal.kind() == "server";
    json!({"id":request.client_id(),"clientId":request.client_id(),"serverId":if server {Some(request.client_id())} else {None},"kind":principal.kind(),"label":text(request.body(),"label"),"origin":if server {Some(text(request.body(),"origin"))}else{None},"publicKey":text(request.body(),"publicKey"),"tailscaleUser":principal.tailscale_user(),"status":"paired","created":now,"lastAccess":if request.is_pairing(){Some(now)}else{None},"revoked":null})
}
pub(crate) fn read_record(
    tx: &Transaction<'_>,
    table: &str,
    id: &str,
) -> Result<Option<String>, AccessError> {
    Ok(tx
        .query_row(
            &format!("SELECT record FROM {table} WHERE id=?1"),
            [id],
            |r| r.get(0),
        )
        .optional()?)
}
