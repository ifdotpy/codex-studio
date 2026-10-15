//! Atomic inbox/outbox effects and authenticated federation consumer.
use crate::federation_messaging::{
    self as core, FederationMessageError as Error, MAX_BATCH, MAX_BATCH_BYTES, PROTOCOL, Result,
    Snapshot,
};
use crate::remote_transport::{self as transport, RemoteIdentity};
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use studio_runtime_store::federation_messaging::{
    FederationMessagingRepository as Repo, FederationTable as Table,
};
use studio_runtime_store::{
    AgentId, AgentRepository, RemoteTransportRepository, RuntimeStore, StoreError,
    SyncDocumentsRepository,
};

/// Opaque observation from the server-side trusted adapter.
///
/// ```compile_fail
/// use studio_runtime_services::federation_effects::OriginObservation;
/// let spoofed = OriginObservation { observed: Some(Some("header-user".into())) };
/// ```
pub struct OriginObservation {
    observed: Option<Option<String>>,
}
impl OriginObservation {
    pub fn unavailable() -> Self {
        Self { observed: None }
    }

    /// Call only after the HTTP boundary accepted the request through its
    /// authenticated Tailscale Serve origin policy. Serve strips client copies
    /// of `Tailscale-User-Login`; a missing value is a real missing observation.
    pub fn from_trusted_serve(user: Option<String>) -> Self {
        Self {
            observed: Some(
                user.map(|value| value.trim().to_ascii_lowercase())
                    .filter(|value| !value.is_empty()),
            ),
        }
    }

    // Unit tests may inject observations without pretending a client supplied a
    // trusted Serve header.
    #[cfg(test)]
    fn observed(user: Option<String>) -> Self {
        Self {
            observed: Some(user),
        }
    }
}

pub struct SignedRequest<'a> {
    pub action: FederationAction,
    pub peer_id: &'a str,
    pub timestamp: i64,
    pub nonce: &'a str,
    pub signature: &'a str,
    pub raw: &'a [u8],
}
#[derive(Clone, Copy)]
pub enum FederationAction {
    Pair,
    Message,
    Pull,
    Status,
}
impl FederationAction {
    pub fn path(self) -> &'static str {
        match self {
            Self::Pair => "/api/federation/v1/pair",
            Self::Message => "/api/federation/v1/message",
            Self::Pull => "/api/federation/v1/pull",
            Self::Status => "/api/federation/v1/status",
        }
    }
}

pub(crate) fn snapshot(
    tx: &Transaction<'_>,
    peer: &str,
    room: &str,
    message: &str,
    now: f64,
) -> Result<Snapshot> {
    Snapshot::checked(
        core::SnapshotKeys {
            peer,
            room,
            message,
        },
        Repo::record(tx, Table::Settings, "global", peer)?
            .unwrap_or_else(|| json!({"enabled":false})),
        Repo::record(tx, Table::Peer, peer, peer)?.ok_or(Error::Forbidden("unknown peer"))?,
        Repo::record(tx, Table::Room, room, peer)?,
        Repo::record(tx, Table::Inbox, message, peer)?,
        Repo::room_rate(tx, peer, room, now)?,
    )
}
fn consume_nonce(
    tx: &Transaction<'_>,
    peer: &str,
    nonce: &str,
    timestamp: i64,
    now: i64,
) -> Result<()> {
    transport::validate_federation_nonce(nonce, timestamp, now)?;
    if !RemoteTransportRepository::consume_federation_nonce(tx, peer, nonce, timestamp, now)? {
        return Err(Error::Forbidden("nonce replay or peer rate limit"));
    }
    Ok(())
}

/// Consumer entry point for the HTTP adapter. Refuses before writing if origin observation is absent.
pub fn handle(
    store: &RuntimeStore,
    request: SignedRequest<'_>,
    origin: OriginObservation,
    now: f64,
) -> Result<Value> {
    let Some(observed) = origin.observed else {
        return Err(Error::Unavailable("federation/trusted-origin-observation"));
    };
    if !now.is_finite() || now < 0.0 || request.raw.len() > transport::MAX_REQUEST_BYTES {
        return Err(Error::Invalid("request"));
    }
    let body: Value = serde_json::from_slice(request.raw).map_err(|_| Error::Invalid("body"))?;
    if matches!(request.action, FederationAction::Pair) {
        return handle_pair(store, request, observed, &body, now);
    }
    let identity = transport::load_federation_identity(store)?;
    let result=store.transaction_with(|tx| {
        let state=snapshot(tx,request.peer_id,"","",now)?;
        state.authorize(matches!(request.action,FederationAction::Status))?;
        if let (Some(observed),Some(expected))=(&observed,&state.peer.whois_user)
            && observed.to_lowercase()!=expected.to_lowercase() { return Err(Error::Forbidden("origin identity mismatch")); }
        transport::validate_federation_nonce(request.nonce,request.timestamp,now as i64)?;
        transport::verify_signature(&state.peer.public_key,&transport::federation_signing_bytes("POST",request.action.path(),request.timestamp,request.nonce,request.raw),request.signature)?;
        match request.action {
            FederationAction::Pair => return Err(Error::Invalid("action")),
            FederationAction::Message=>core::validate("MessageRequest",&body,"body")?,
            FederationAction::Pull=>core::validate("PullRequest",&body,"body")?,
            FederationAction::Status=>core::validate("StatusRequest",&body,"body")?,
        }
        consume_nonce(tx,request.peer_id,request.nonce,request.timestamp,now as i64)?;
        check_identity(tx,&identity)?;
        match request.action {
            FederationAction::Pair => Err(Error::Invalid("action")),
            FederationAction::Message=>receive_locked(tx,request.peer_id,&body["envelope"],now),
            FederationAction::Pull=>pull_locked(tx,request.peer_id,&body,&identity,now),
            FederationAction::Status=>Ok(json!({"protocol":PROTOCOL,"approved":state.peer.allowed(),"revoked":state.peer.revoked()})),
        }
    })?;
    signed_response(&identity, result, now as i64)
}

fn handle_pair(
    store: &RuntimeStore,
    request: SignedRequest<'_>,
    observed: Option<String>,
    body: &Value,
    now: f64,
) -> Result<Value> {
    core::validate("PairRequest", body, "body")?;
    let identity = transport::load_federation_identity(store)?;
    let invite_id = body["inviteId"]
        .as_str()
        .ok_or(Error::Invalid("inviteId"))?;
    let token = body["token"].as_str().ok_or(Error::Invalid("token"))?;
    let state_id = body["stateId"].as_str().ok_or(Error::Invalid("stateId"))?;
    let label = body["label"].as_str().ok_or(Error::Invalid("label"))?;
    let public_key = body["publicKey"]
        .as_str()
        .ok_or(Error::Invalid("publicKey"))?;
    let origin = transport::validate_federation_origin(
        body["origin"].as_str().ok_or(Error::Invalid("origin"))?,
    )?;
    if state_id != request.peer_id || label.trim().is_empty() || label.chars().count() > 80 {
        return Err(Error::Forbidden("pairing identity does not match request"));
    }
    uuid::Uuid::parse_str(state_id).map_err(|_| Error::Forbidden("invalid peer state identity"))?;
    if state_id == identity.id() {
        return Err(Error::Forbidden("cannot pair a server with itself"));
    }
    transport::validate_federation_nonce(request.nonce, request.timestamp, now as i64)?;
    transport::verify_signature(
        public_key,
        &transport::federation_signing_bytes(
            "POST",
            FederationAction::Pair.path(),
            request.timestamp,
            request.nonce,
            request.raw,
        ),
        request.signature,
    )?;
    let local_claim = body["tailscaleUser"].as_str().map(str::to_ascii_lowercase);
    let whois_status = if let Some(observed_user) = &observed {
        if local_claim
            .as_deref()
            .is_some_and(|claimed| claimed != observed_user)
        {
            return Err(Error::Forbidden("Tailscale whois identity mismatch"));
        }
        "verified"
    } else {
        "missing"
    };
    let result = store.transaction_with(|tx| {
        let current_identity = tx.query_row(
            "SELECT json_extract(record,'$.stateId') FROM runtime_federation_identity WHERE id='local'",
            [],
            |row| row.get::<_, String>(0),
        ).map_err(StoreError::from)?;
        if current_identity != identity.id() {
            return Err(Error::Conflict("local identity changed"));
        }
        let enabled: bool = tx.query_row(
            "SELECT COALESCE(json_extract(record,'$.enabled'),0) FROM runtime_federation_settings WHERE id='global'",
            [],
            |row| row.get(0),
        ).map_err(StoreError::from)?;
        if !enabled {
            return Err(Error::Forbidden("federation is disabled"));
        }
        if !RemoteTransportRepository::consume_federation_nonce(
            tx,
            state_id,
            request.nonce,
            request.timestamp,
            now as i64,
        )? {
            return Err(Error::Forbidden("nonce replay or peer rate limit"));
        }
        let invite: Option<String> = tx
            .query_row(
                "SELECT record FROM runtime_federation_invites WHERE id=?1",
                [invite_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)?;
        let mut invite: Value = invite
            .map(|raw| serde_json::from_str(&raw))
            .transpose()
            .map_err(|_| Error::Invalid("invite"))?
            .ok_or(Error::Forbidden("invalid pairing invitation"))?;
        let expected_hash = invite["tokenHash"].as_str().unwrap_or_default();
        let supplied_hash = studio_runtime_store::sha256_hex(token.as_bytes());
        let same_peer = invite["status"] == "used" && invite["usedBy"] == state_id;
        let expires = invite["expires"].as_i64().unwrap_or_default();
        if (!same_peer && (invite["status"] != "open" || expires < now as i64))
            || !constant_time_eq(expected_hash.as_bytes(), supplied_hash.as_bytes())
        {
            return Err(Error::Forbidden("pairing invitation is invalid, expired, or already used"));
        }
        if let Some(expected) = invite["expectedUser"].as_str()
            && observed.as_deref().is_some_and(|actual| !actual.eq_ignore_ascii_case(expected))
        {
            return Err(Error::Forbidden("Tailscale whois identity does not match invitation"));
        }
        let existing: Option<String> = tx
            .query_row(
                "SELECT record FROM runtime_federation_peers WHERE id=?1",
                [state_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)?;
        let mut peer: Value = existing
            .map(|raw| serde_json::from_str(&raw))
            .transpose()
            .map_err(|_| Error::Invalid("peer"))?
            .unwrap_or_else(|| json!({
                "stateId": state_id,
                "label": label.trim(),
                "origin": origin,
                "publicKey": public_key,
                "status": "pending",
                "localApproved": false,
                "remoteApproved": true,
                "created": now,
            }));
        if peer["publicKey"] != public_key {
            return Err(Error::Conflict("a different key is already registered for this server"));
        }
        peer["label"] = json!(label.trim());
        peer["origin"] = json!(origin);
        peer["remoteApproved"] = json!(true);
        peer["whoisStatus"] = json!(whois_status);
        peer["whoisUser"] = observed.as_ref().map_or(Value::Null, |value| json!(value));
        peer["updated"] = json!(now);
        peer["status"] = json!(if peer["localApproved"] == true { "approved" } else { "pending" });
        peer["leadName"] = body.get("leadName").cloned().unwrap_or(Value::Null);
        tx.execute(
            "INSERT INTO runtime_federation_peers(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            rusqlite::params![state_id, transport::canonical_json(&peer)?],
        ).map_err(StoreError::from)?;
        invite["status"] = json!("used");
        invite["usedBy"] = json!(state_id);
        tx.execute(
            "UPDATE runtime_federation_invites SET record=?1 WHERE id=?2",
            rusqlite::params![transport::canonical_json(&invite)?, invite_id],
        ).map_err(StoreError::from)?;
        Ok::<_, Error>(json!({
            "protocol": PROTOCOL,
            "accepted": true,
            "remoteApproved": peer["localApproved"] == true,
            "whoisStatus": whois_status,
            "stateId": identity.id(),
        }))
    })?;
    signed_response(&identity, result, now as i64)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    left.len() == right.len() && bool::from(left.ct_eq(right))
}

pub(crate) fn check_identity(tx: &Transaction<'_>, identity: &RemoteIdentity) -> Result<()> {
    let matches:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_federation_identity WHERE id='local' AND json_extract(record,'$.stateId')=?1 AND json_extract(record,'$.publicKey')=?2)",params![identity.id(),identity.public_key_pem()],|r|r.get(0)).map_err(StoreError::from)?;
    if !matches {
        return Err(Error::Conflict("local identity changed"));
    }
    Ok(())
}
pub fn signed_response(identity: &RemoteIdentity, result: Value, now: i64) -> Result<Value> {
    let mut proof = json!({"stateId":identity.id(),"timestamp":now,"nonce":transport::new_nonce()?,"result":result});
    let bytes = format!(
        "studio-federation-response-v1\n{}",
        transport::canonical_json(&proof)?
    );
    proof["signature"] = json!(identity.sign(bytes.as_bytes()));
    Ok(proof)
}

pub fn receive(store: &RuntimeStore, peer: &str, envelope: &Value, now: f64) -> Result<Value> {
    store.transaction_with(|tx| receive_locked(tx, peer, envelope, now))
}
fn receive_locked(tx: &Transaction<'_>, peer: &str, envelope: &Value, now: f64) -> Result<Value> {
    let message = core::checked_envelope(envelope)?;
    let state = snapshot(tx, peer, &message.room, &message.id, now)?;
    state.authorize(false)?;
    if message.sender_server != peer {
        return Err(Error::Forbidden("sender identity mismatch"));
    }
    transport::verify_signature(
        &state.peer.public_key,
        &transport::federation_message_signing_bytes(envelope)?,
        &message.signature,
    )?;
    consume_nonce(tx, peer, &message.nonce, message.timestamp, now as i64)?;
    let mut decision = core::decide_receive(&state, envelope, now)?;
    if let Some(chat) = &decision.chat {
        // The inbox is authoritative for replay. A pre-existing chat without its inbox
        // cannot arise from this transaction and is refused as inconsistent state.
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_chat_messages WHERE id=?1)",
                [&chat.id],
                |r| r.get(0),
            )
            .map_err(StoreError::from)?;
        if exists {
            return Err(Error::Conflict("chat exists without its inbox receipt"));
        }
        tx.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES(?1,?2,?3,?4,?5,'{}')",params![chat.id,chat.room,chat.sender,chat.text,chat.created]).map_err(StoreError::from)?;
        decision.receipt["sequence"] = json!(tx.last_insert_rowid());
        for member in &chat.recipients {
            let agent = AgentRepository::get(tx, &AgentId::from(member.as_str()))?
                .ok_or(Error::Invalid("room.localMembers"))?;
            if crate::event_write::truthy(agent.extra.get("deletedAt")) {
                continue;
            }
            crate::event_write::enqueue_event(
                tx,
                &agent,
                format!("{}{member}", chat.event_prefix),
                "agent_message",
                transport::canonical_json(&chat.event)?,
                now,
            )?;
        }
    }
    if let Some(room) = &decision.room {
        save_room(tx, peer, room)?;
    }
    if let Some(inbox) = &mut decision.inbox {
        inbox["receipt"] = decision.receipt.clone();
        Repo::put(tx, Table::Inbox, &message.id, peer, inbox)?;
    }
    Ok(decision.receipt)
}

pub(crate) fn save_room(tx: &Transaction<'_>, peer: &str, room: &Value) -> Result<()> {
    let id = room["id"].as_str().ok_or(Error::Invalid("room.id"))?;
    Repo::put(tx, Table::Room, id, peer, room)?;
    let standard = json!({"id":id,"kind":"federated","members":room["localMembers"],"updated":room["updated"],"name":room["name"],"federation":true,"peerId":room["peerId"],"peerLabel":room["peerLabel"]});
    tx.execute("INSERT INTO runtime_rooms(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![id,transport::canonical_json(&standard)?]).map_err(StoreError::from)?;
    let directory = crate::sync_entity_context::RoomDirectory::load(tx)?;
    let context = directory.for_room(tx, &standard, true)?;
    let view = crate::sync_views::room_entity_view(&standard, &context);
    SyncDocumentsRepository::put_entity(
        tx,
        "room",
        id,
        view.clone().unwrap_or_else(|| json!({})),
        view.is_none(),
    )?;
    Ok(())
}

pub(crate) fn outbound(item: &Value, identity: &RemoteIdentity, now: i64) -> Result<Value> {
    let mut envelope = json!({"protocol":PROTOCOL,"senderServer":identity.id(),"id":item["id"],"kind":item["kind"],"room":item["room"],"payload":item["payload"],"created":item["created"],"timestamp":now,"nonce":transport::new_nonce()?});
    envelope["signature"] =
        json!(identity.sign(&transport::federation_message_signing_bytes(&envelope)?));
    core::checked_envelope(&envelope)?;
    Ok(envelope)
}

pub fn pull(store: &RuntimeStore, peer: &str, body: &Value, now: f64) -> Result<Value> {
    core::validate("PullRequest", body, "body")?;
    let identity = transport::load_federation_identity(store)?;
    store.transaction_with(|tx| {
        snapshot(tx, peer, "", "", now)?.authorize(false)?;
        check_identity(tx, &identity)?;
        pull_locked(tx, peer, body, &identity, now)
    })
}
fn pull_locked(
    tx: &Transaction<'_>,
    peer: &str,
    body: &Value,
    identity: &RemoteIdentity,
    now: f64,
) -> Result<Value> {
    let acks = body
        .get("acks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut acknowledged = Vec::new();
    for ack in acks {
        if acknowledge_outbox(tx, peer, &ack, now, false)? {
            acknowledged.push(ack);
        }
    }
    let mut messages = Vec::new();
    let mut size = 0;
    for (id, mut item) in Repo::batch(tx, Table::Outbox, peer, f64::MAX)? {
        let envelope = outbound(&item, identity, now as i64)?;
        let encoded = transport::canonical_json(&envelope)?.len();
        if !messages.is_empty() && size + encoded > MAX_BATCH_BYTES {
            break;
        }
        size += encoded;
        item["hash"] = json!(core::envelope_hash(&envelope)?);
        Repo::put(tx, Table::Outbox, &id, peer, &item)?;
        messages.push(envelope);
    }
    let mut receipts = Vec::new();
    for (id, mut item) in Repo::batch(tx, Table::Inbox, peer, now)? {
        receipts.push(json!({"id":id,"hash":item["hash"]}));
        item["ack"] = json!("sent");
        Repo::put(tx, Table::Inbox, &id, peer, &item)?;
    }
    Ok(json!({"protocol":PROTOCOL,"messages":messages,"acks":receipts,"acknowledged":acknowledged}))
}

pub(crate) fn acknowledge_outbox(
    tx: &Transaction<'_>,
    peer: &str,
    ack: &Value,
    now: f64,
    publish_delivery: bool,
) -> Result<bool> {
    let id = ack["id"].as_str().ok_or(Error::Invalid("ack.id"))?;
    let hash = ack["hash"].as_str().ok_or(Error::Invalid("ack.hash"))?;
    let Some(mut item) = Repo::record(tx, Table::Outbox, id, peer)? else {
        return Ok(false);
    };
    if item["hash"] != hash {
        return Ok(false);
    }
    item["status"] = json!("delivered");
    item["delivered"] = json!(now);
    item["error"] = Value::Null;
    Repo::put(tx, Table::Outbox, id, peer, &item)?;
    if !publish_delivery {
        return Ok(true);
    }
    if item["kind"] == "room-accept" {
        let room_id = item["room"].as_str().ok_or(Error::Invalid("outbox.room"))?;
        if let Some(mut room) = Repo::record(tx, Table::Room, room_id, peer)?
            && room["peerId"] == peer
            && room["localApproved"] == true
        {
            room["status"] = json!("approved");
            room["remoteApproved"] = json!(true);
            room["updated"] = json!(now);
            save_room(tx, peer, &room)?;
        }
    }
    let raw: Option<String> = tx
        .query_row(
            "SELECT deliveries FROM runtime_chat_messages WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()
        .map_err(StoreError::from)?;
    if let Some(raw) = raw {
        let mut deliveries: Value = serde_json::from_str(&raw).map_err(StoreError::from)?;
        let object = deliveries
            .as_object_mut()
            .ok_or(Error::Invalid("chat.deliveries"))?;
        object.insert(format!("remote:{peer}"), json!("delivered"));
        tx.execute(
            "UPDATE runtime_chat_messages SET deliveries=?1 WHERE id=?2",
            params![transport::canonical_json(&deliveries)?, id],
        )
        .map_err(StoreError::from)?;
    }
    Ok(true)
}

/// A signed pull response is applied atomically; incoming envelopes each retain their own proof.
pub(crate) fn reconcile(
    store: &RuntimeStore,
    peer: &str,
    expected_key: &str,
    response: &Value,
    now: f64,
) -> Result<()> {
    core::validate("PullResult", response, "pull.response")?;
    for field in ["messages", "acks", "acknowledged"] {
        if response[field]
            .as_array()
            .is_none_or(|a| a.len() > MAX_BATCH)
        {
            return Err(Error::Invalid("pull.batch"));
        }
    }
    store.transaction_with(|tx| {
        let state = snapshot(tx, peer, "", "", now)?;
        state.authorize(false)?;
        if state.peer.public_key != expected_key {
            return Err(Error::Conflict("peer key changed"));
        }
        for ack in response["acks"]
            .as_array()
            .ok_or(Error::Invalid("pull.acks"))?
        {
            acknowledge_outbox(tx, peer, ack, now, true)?;
        }
        for ack in response["acknowledged"]
            .as_array()
            .ok_or(Error::Invalid("pull.acknowledged"))?
        {
            let id = ack["id"].as_str().ok_or(Error::Invalid("ack.id"))?;
            if let Some(mut item) = Repo::record(tx, Table::Inbox, id, peer)?
                && item["hash"] == ack["hash"]
            {
                item["ack"] = json!("confirmed");
                Repo::put(tx, Table::Inbox, id, peer, &item)?;
            }
        }
        for envelope in response["messages"]
            .as_array()
            .ok_or(Error::Invalid("pull.messages"))?
        {
            receive_locked(tx, peer, envelope, now)?;
        }
        Ok(())
    })
}

#[cfg(test)]
#[path = "../tests/federation/corpus.rs"]
mod federation_corpus;
