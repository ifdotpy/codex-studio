//! Pure federation decisions. Inputs are checked stored records; no I/O or clock.
pub use crate::federation_limits::{
    MAX_BATCH_BYTES, MAX_BATCH_ITEMS as MAX_BATCH, MAX_MESSAGE_CODEPOINTS as MAX_MESSAGE,
    MAX_ROOM_MEMBERS,
};
use crate::remote_transport::{RemoteTransportError, canonical_json};
use serde::Deserialize;
use serde_json::{Value, json};
use studio_runtime_store::federation_messaging::FederationTable;
use thiserror::Error;

pub const PROTOCOL: u64 = 1;
const MAX_QUEUE_ROWS: u64 = 10_000;
const MAX_QUEUE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ROOM_RATE: u64 = 30;

#[derive(Debug, Error)]
pub enum FederationMessageError {
    #[error(transparent)]
    Store(#[from] studio_runtime_store::StoreError),
    #[error(transparent)]
    Transport(#[from] RemoteTransportError),
    #[error(transparent)]
    Event(#[from] crate::event_write::EventWriteError),
    #[error("invalid federation object at {0}")]
    Invalid(&'static str),
    #[error("federation permission refused: {0}")]
    Forbidden(&'static str),
    #[error("federation request conflicts: {0}")]
    Conflict(&'static str),
    #[error("Not implemented: requires {0}")]
    Unavailable(&'static str),
}
pub type Result<T> = std::result::Result<T, FederationMessageError>;

pub(crate) fn validate(schema: &str, value: &Value, location: &'static str) -> Result<()> {
    if !studio_runtime_schema::validate(schema, value)
        .map_err(studio_runtime_store::StoreError::SchemaValidation)?
    {
        return Err(FederationMessageError::Invalid(location));
    }
    Ok(())
}
fn decode<T: serde::de::DeserializeOwned>(value: Value, location: &'static str) -> Result<T> {
    serde_json::from_value(value).map_err(|_| FederationMessageError::Invalid(location))
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum MessageKind {
    ChatMessage,
    RoomInvite,
    RoomAccept,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Peer {
    pub(crate) state_id: String,
    pub(crate) label: String,
    pub(crate) origin: String,
    pub(crate) public_key: String,
    status: PeerStatus,
    local_approved: bool,
    remote_approved: bool,
    #[serde(default)]
    pub(crate) whois_user: Option<String>,
}
#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum PeerStatus {
    Approved,
    Pending,
    Revoked,
}
impl Peer {
    pub(crate) fn allowed(&self) -> bool {
        self.status == PeerStatus::Approved && self.local_approved && self.remote_approved
    }
    pub(crate) fn revoked(&self) -> bool {
        self.status == PeerStatus::Revoked
    }
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Room {
    peer_id: String,
    status: PeerStatus,
    local_approved: bool,
    remote_approved: bool,
    local_members: Vec<String>,
    remote_members: Vec<Value>,
    #[serde(default)]
    remote_sequence: u64,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Envelope {
    pub(crate) sender_server: String,
    pub(crate) id: String,
    pub(crate) room: String,
    pub(crate) kind: MessageKind,
    pub(crate) payload: Value,
    pub(crate) nonce: String,
    pub(crate) timestamp: i64,
    pub(crate) signature: String,
}
pub(crate) fn checked_envelope(value: &Value) -> Result<Envelope> {
    validate("SignedEnvelope", value, "envelope")?;
    let envelope: Envelope = decode(value.clone(), "envelope")?;
    validate(
        match envelope.kind {
            MessageKind::ChatMessage => "FederationChatPayload",
            _ => "FederationRoomPayload",
        },
        &envelope.payload,
        "envelope.payload",
    )?;
    Ok(envelope)
}

/// One constructor validates every record used by a decision, including primary keys.
pub struct Snapshot {
    enabled: bool,
    pub(crate) peer: Peer,
    room: Option<Room>,
    pub(crate) room_value: Option<Value>,
    previous: Option<Value>,
    rate: u64,
}
pub struct SnapshotKeys<'a> {
    pub peer: &'a str,
    pub room: &'a str,
    pub message: &'a str,
}
impl Snapshot {
    pub fn checked(
        keys: SnapshotKeys<'_>,
        settings: Value,
        peer: Value,
        room: Option<Value>,
        previous: Option<Value>,
        rate: u64,
    ) -> Result<Self> {
        let SnapshotKeys {
            peer: peer_id,
            room: room_id,
            message: message_id,
        } = keys;
        FederationTable::Settings.validate("global", &settings)?;
        FederationTable::Peer.validate(peer_id, &peer)?;
        if let Some(value) = &room {
            FederationTable::Room.validate(room_id, value)?;
        }
        if let Some(value) = &previous {
            FederationTable::Inbox.validate(message_id, value)?;
            validate(
                "studio_api__federation__models__MessageReceipt",
                &value["receipt"],
                "inbox.receipt",
            )?;
            if value["receipt"]["messageId"] != message_id || value["receipt"]["room"] != room_id {
                return Err(FederationMessageError::Invalid("inbox.receipt.identity"));
            }
        }
        let enabled = settings["enabled"]
            .as_bool()
            .ok_or(FederationMessageError::Invalid("settings.enabled"))?;
        Ok(Self {
            enabled,
            peer: decode(peer, "peer")?,
            room: room.clone().map(|v| decode(v, "room")).transpose()?,
            room_value: room,
            previous,
            rate,
        })
    }
    pub(crate) fn authorize(&self, allow_revoked: bool) -> Result<()> {
        if !self.enabled || !(self.peer.allowed() || allow_revoked && self.peer.revoked()) {
            return Err(FederationMessageError::Forbidden(
                "disabled or unapproved peer",
            ));
        }
        Ok(())
    }
}

pub struct ReceiveDecision {
    pub(crate) receipt: Value,
    pub(crate) room: Option<Value>,
    pub(crate) chat: Option<ChatEffect>,
    pub(crate) inbox: Option<Value>,
}
pub(crate) struct ChatEffect {
    pub(crate) id: String,
    pub(crate) room: String,
    pub(crate) sender: String,
    pub(crate) text: String,
    pub(crate) created: f64,
    pub(crate) recipients: Vec<String>,
    pub(crate) event: Value,
    pub(crate) event_prefix: String,
}

pub fn envelope_hash(envelope: &Value) -> Result<String> {
    let fields = envelope
        .as_object()
        .ok_or(FederationMessageError::Invalid("envelope"))?;
    let immutable = Value::Object(
        fields
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "timestamp" | "nonce" | "signature"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    );
    Ok(studio_runtime_store::sha256_hex(
        canonical_json(&immutable)?.as_bytes(),
    ))
}

/// Cross-field disclosure rules cannot be expressed by the participant API schema.
fn participants(payload: &Value) -> Result<Vec<Value>> {
    let object = payload
        .as_object()
        .ok_or(FederationMessageError::Invalid("payload"))?;
    if object.len() != 4
        || !["room", "participants", "shareNames", "shareStatus"]
            .iter()
            .all(|k| object.contains_key(*k))
    {
        return Err(FederationMessageError::Invalid("payload.participants"));
    }
    let names = payload["shareNames"]
        .as_bool()
        .ok_or(FederationMessageError::Invalid("payload.shareNames"))?;
    let status = payload["shareStatus"]
        .as_bool()
        .ok_or(FederationMessageError::Invalid("payload.shareStatus"))?;
    let people = payload["participants"]
        .as_array()
        .filter(|v| !v.is_empty() && v.len() <= MAX_ROOM_MEMBERS)
        .ok_or(FederationMessageError::Invalid("payload.participants"))?;
    let mut seen = std::collections::BTreeSet::new();
    for p in people {
        validate("FederationParticipant", p, "payload.participants")?;
        if !seen.insert(p["id"].clone().to_string())
            || p.get("name")
                .is_some_and(|v| !v.is_string() || p["role"] != "lead" && !names)
            || p.get("status").is_some_and(|v| !v.is_string() || !status)
        {
            return Err(FederationMessageError::Invalid(
                "payload.participants.disclosure",
            ));
        }
    }
    Ok(people.clone())
}

pub fn decide_receive(snapshot: &Snapshot, envelope: &Value, now: f64) -> Result<ReceiveDecision> {
    let message = checked_envelope(envelope)?;
    snapshot.authorize(false)?;
    if message.sender_server != snapshot.peer.state_id {
        return Err(FederationMessageError::Forbidden("sender identity"));
    }
    let hash = envelope_hash(envelope)?;
    if let Some(previous) = &snapshot.previous {
        if previous["hash"] != hash {
            return Err(FederationMessageError::Conflict(
                "message identity has different content",
            ));
        }
        return Ok(ReceiveDecision {
            receipt: previous["receipt"].clone(),
            room: None,
            chat: None,
            inbox: None,
        });
    }
    let mut receipt = json!({"protocol":PROTOCOL,"messageId":message.id,"accepted":true,"room":message.room,"sequence":null});
    let mut chat = None;
    let room = match message.kind {
        MessageKind::RoomInvite | MessageKind::RoomAccept => {
            if message.payload["room"] != message.room {
                return Err(FederationMessageError::Invalid("payload.room"));
            }
            let people = participants(&message.payload)?;
            if snapshot
                .room
                .as_ref()
                .is_some_and(|r| r.peer_id != snapshot.peer.state_id)
            {
                return Err(FederationMessageError::Forbidden(
                    "room belongs to another peer",
                ));
            }
            if message.kind == MessageKind::RoomAccept
                && snapshot.room.as_ref().is_none_or(|r| !r.local_approved)
            {
                return Err(FederationMessageError::Forbidden(
                    "room is not awaiting approval",
                ));
            }
            let mut room=snapshot.room_value.clone().unwrap_or_else(||json!({
                "id":message.room,"kind":"federated","peerId":snapshot.peer.state_id,"peerLabel":snapshot.peer.label,
                "name":format!("{} · Shared room",snapshot.peer.label),"status":"pending","localMembers":[],
                "localParticipants":[],"remoteMembers":[],"localApproved":false,"remoteApproved":true,
                "shareNames":message.payload["shareNames"],"shareStatus":message.payload["shareStatus"],
                "created":now,"updated":now,"sequence":0,"remoteSequence":0
            }));
            room["remoteMembers"] = json!(people);
            room["remoteApproved"] = json!(true);
            room["updated"] = json!(now);
            if message.kind == MessageKind::RoomAccept {
                room["status"] = json!("approved");
            }
            room
        }
        MessageKind::ChatMessage => {
            let room = snapshot
                .room
                .as_ref()
                .filter(|r| {
                    r.peer_id == snapshot.peer.state_id
                        && r.status == PeerStatus::Approved
                        && r.local_approved
                        && r.remote_approved
                })
                .ok_or(FederationMessageError::Forbidden("room is not approved"))?;
            let sender = message.payload["sender"]
                .as_str()
                .ok_or(FederationMessageError::Invalid("payload.sender"))?;
            let person = room
                .remote_members
                .iter()
                .find(|p| p["id"] == sender)
                .ok_or(FederationMessageError::Forbidden(
                    "sender is not a room member",
                ))?;
            if message.payload.get("senderName").unwrap_or(&Value::Null)
                != person.get("name").unwrap_or(&Value::Null)
            {
                return Err(FederationMessageError::Forbidden(
                    "sender name is not approved",
                ));
            }
            let text = message.payload["text"]
                .as_str()
                .filter(|s| !s.is_empty() && s.chars().count() <= MAX_MESSAGE)
                .ok_or(FederationMessageError::Invalid("payload.text"))?;
            let sequence = message.payload["sequence"]
                .as_u64()
                .filter(|s| *s > room.remote_sequence)
                .ok_or(FederationMessageError::Invalid("payload.sequence"))?;
            let created = message.payload["created"]
                .as_f64()
                .filter(|v| v.is_finite() && *v > 0.0)
                .ok_or(FederationMessageError::Invalid("payload.created"))?;
            if snapshot.rate >= MAX_ROOM_RATE {
                return Err(FederationMessageError::Forbidden("room rate limit"));
            }
            let display = format!("remote:{}:{sender}", snapshot.peer.state_id);
            let row_id = format!(
                "federated:{}",
                studio_runtime_store::sha256_hex(
                    format!("{}:{}", snapshot.peer.state_id, message.id).as_bytes()
                )
            );
            let event = json!({"room":message.room,"message_id":message.id,"sender":display,"sender_name":person.get("name").filter(|v|v.as_str().is_some_and(|s|!s.is_empty())).cloned().unwrap_or_else(||json!(snapshot.peer.label)),"text":text,"untrusted_remote":true,"authority":"room-data-only"});
            chat = Some(ChatEffect {
                id: row_id,
                room: message.room.clone(),
                sender: display,
                text: text.into(),
                created,
                recipients: room.local_members.clone(),
                event,
                event_prefix: format!("federation:{}:{}:", snapshot.peer.state_id, message.id),
            });
            let mut value = snapshot
                .room_value
                .clone()
                .ok_or(FederationMessageError::Invalid("room"))?;
            value["remoteSequence"] = json!(sequence);
            value["updated"] = json!(created.max(now));
            receipt["sequence"] = Value::Null; // Assigned by the atomic chat insert, before the inbox write.
            value
        }
    };
    let mut inbox = json!({"hash":hash,"receipt":receipt,"ack":"pending","created":now});
    if message.kind == MessageKind::ChatMessage {
        inbox["kind"] = json!("chat-message");
        inbox["room"] = json!(message.room);
    }
    Ok(ReceiveDecision {
        receipt,
        room: Some(room),
        chat,
        inbox: Some(inbox),
    })
}

pub fn decide_queue(existing: Option<&Value>, item: Value, pending: (u64, u64)) -> Result<Value> {
    let id = item["id"]
        .as_str()
        .ok_or(FederationMessageError::Invalid("outbox.id"))?;
    FederationTable::Outbox.validate(id, &item)?;
    if let Some(existing) = existing {
        FederationTable::Outbox.validate(id, existing)?;
        if existing["kind"] != item["kind"]
            || existing["room"] != item["room"]
            || existing["payload"] != item["payload"]
        {
            return Err(FederationMessageError::Conflict(
                "message identity has different content",
            ));
        }
        return Ok(existing.clone());
    }
    if pending.0 >= MAX_QUEUE_ROWS
        || pending
            .1
            .saturating_add(canonical_json(&item)?.len() as u64)
            > MAX_QUEUE_BYTES
    {
        return Err(FederationMessageError::Conflict("queue is full"));
    }
    Ok(item)
}
