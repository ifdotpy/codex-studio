//! Bounded federation delivery pass. Network work runs outside store transactions.
use crate::federation_effects::{self as effects, snapshot};
use crate::federation_messaging::{
    self as core, FederationMessageError as Error, PROTOCOL, Result,
};
use crate::remote_transport::{self as transport, RemoteIdentity};
use futures_util::future::BoxFuture;
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use studio_runtime_store::federation_messaging::{
    FederationMessagingRepository as Repo, FederationTable as Table,
};
use studio_runtime_store::{AgentId, AgentRepository, RuntimeStore, StoreError};
const PASS_TIMEOUT: Duration = Duration::from_secs(8);

/// A single absolute deadline and cancellation signal shared by every exchange.
pub struct PassBudget {
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}
impl PassBudget {
    pub fn new(deadline: Instant, cancelled: Arc<AtomicBool>) -> Self {
        Self {
            deadline,
            cancelled,
        }
    }
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    pub fn remaining(&self) -> transport::RemoteTransportResult<Duration> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(transport::RemoteTransportError::ExchangeUnknown);
        }
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(transport::RemoteTransportError::ExchangeUnknown)
    }
}

/// Implementations must honor this pass budget, including its cancellation signal.
pub struct FederationExchange {
    pub store: Arc<RuntimeStore>,
    pub server_id: String,
    pub public_key: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub deadline: Instant,
    pub cancelled: Arc<AtomicBool>,
}

pub trait FederationTransport: Send + Sync {
    fn exchange<'a>(
        &'a self,
        request: FederationExchange,
    ) -> BoxFuture<'a, transport::RemoteTransportResult<FederationTransportResponse>>;
}

pub struct FederationTransportResponse {
    pub body: Vec<u8>,
    pub link_advert: Option<String>,
}

pub struct HttpsFederationTransport;
impl FederationTransport for HttpsFederationTransport {
    fn exchange<'a>(
        &'a self,
        request: FederationExchange,
    ) -> BoxFuture<'a, transport::RemoteTransportResult<FederationTransportResponse>> {
        Box::pin(async move {
            let FederationExchange {
                store,
                server_id,
                public_key,
                url,
                headers,
                body,
                deadline,
                cancelled,
            } = request;
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err(transport::RemoteTransportError::ExchangeUnknown);
            }
            let response = tokio::task::spawn_blocking(move || {
                let timeout = deadline
                    .checked_duration_since(Instant::now())
                    .filter(|remaining| !remaining.is_zero())
                    .filter(|_| !cancelled.load(Ordering::Acquire))
                    .ok_or(transport::RemoteTransportError::ExchangeUnknown)?;
                if cancelled.load(Ordering::Acquire) {
                    return Err(transport::RemoteTransportError::ExchangeUnknown);
                }
                let headers = headers
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str()))
                    .collect::<Vec<_>>();
                let response = transport::exchange_paired_request(
                    &store,
                    transport::PairedRequest {
                        server_id: &server_id,
                        public_key: &public_key,
                        method: "POST",
                        url: &url,
                        headers: &headers,
                        body: &body,
                        timeout,
                        response_limit: transport::MAX_REQUEST_BYTES,
                    },
                )?;
                if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                    return Err(transport::RemoteTransportError::ExchangeUnknown);
                }
                if response.status != 200 {
                    return Err(transport::RemoteTransportError::InvalidResponse);
                }
                let link_advert = response
                    .headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("x-studio-link"))
                    .map(|(_, value)| value.clone());
                Ok(FederationTransportResponse {
                    body: response.body,
                    link_advert,
                })
            })
            .await
            .map_err(|_| transport::RemoteTransportError::ExchangeUnknown)??;
            Ok(response)
        })
    }
}

pub fn pending_pairings(store: &RuntimeStore) -> std::result::Result<Vec<String>, StoreError> {
    store.read_transaction(|connection| {
        let mut statement = connection.prepare_cached(
            "SELECT id FROM runtime_federation_peers WHERE json_extract(record,'$.localApproved')=1 AND json_extract(record,'$.remoteApproved')=0 AND json_extract(record,'$.status')!='revoked' AND json_extract(record,'$.inviteId') IS NOT NULL ORDER BY id",
        )?;
        let rows = statement.query_map([], |row| row.get(0))?;
        rows.collect::<std::result::Result<Vec<String>, _>>()
            .map_err(StoreError::from)
    })
}

/// Queued delivery deadlines for approved peers. The server uses the peer key
/// to apply in-memory network failure backoff without delaying unrelated peers.
pub fn delivery_deadlines_by_peer(
    store: &RuntimeStore,
) -> std::result::Result<Vec<(String, f64)>, StoreError> {
    store.read_transaction(|connection| {
        let mut statement = connection.prepare(
                "SELECT peer.id,MIN(CASE WHEN json_type(outbox.record,'$.nextAt') IN ('integer','real') \
                     THEN CAST(json_extract(outbox.record,'$.nextAt') AS REAL) END) \
                 FROM runtime_federation_outbox AS outbox \
                 JOIN runtime_federation_peers AS peer ON peer.id=outbox.peer_id \
                 WHERE json_extract(peer.record,'$.status')='approved' \
                   AND json_extract(outbox.record,'$.status')='queued' \
                 GROUP BY peer.id ORDER BY peer.id",
            )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<f64>>(1)?))
        })?;
        rows.filter_map(|row| match row {
            Ok((peer, Some(deadline))) => Some(Ok((peer, deadline))),
            Ok((_, None)) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(StoreError::from)
    })
}

/// Complete a durable outgoing invitation pairing. The peer row already holds
/// the invitation ID/token before any network call, so retries keep its identity.
pub async fn pair_peer(
    store: Arc<RuntimeStore>,
    peer_id: &str,
    now: f64,
    transport: &impl FederationTransport,
    cancelled: Arc<AtomicBool>,
) -> Result<Value> {
    let store_for_read = Arc::clone(&store);
    let peer_id_for_read = peer_id.to_owned();
    let (identity, record) = tokio::task::spawn_blocking(move || {
        let identity = transport::load_federation_identity(&store_for_read)?;
        let record = store_for_read.read_transaction(|connection| {
            let raw: Option<String> = connection
                .query_row(
                    "SELECT record FROM runtime_federation_peers WHERE id=?1",
                    [&peer_id_for_read],
                    |row| row.get(0),
                )
                .optional()?;
            raw.map(|raw| serde_json::from_str::<Value>(&raw))
                .transpose()
                .map_err(|_| {
                    StoreError::SchemaMismatch("invalid federation peer record".to_owned())
                })?
                .ok_or_else(|| StoreError::SchemaMismatch("unknown federation peer".to_owned()))
        })?;
        Ok::<_, Error>((identity, record))
    })
    .await
    .map_err(|_| Error::Unavailable("federation/peer-read-worker"))??;
    if record["status"] == "revoked"
        || record["localApproved"] != true
        || record["remoteApproved"] == true
    {
        return Err(Error::Forbidden("peer is not awaiting local pairing"));
    }
    let peer: core::Peer =
        serde_json::from_value(record.clone()).map_err(|_| Error::Invalid("peer"))?;
    let invite_id = record["inviteId"]
        .as_str()
        .ok_or(Error::Invalid("peer.inviteId"))?;
    let invite_token = record["inviteToken"]
        .as_str()
        .ok_or(Error::Invalid("peer.inviteToken"))?;
    let local_origin = record["localOrigin"]
        .as_str()
        .ok_or(Error::Invalid("peer.localOrigin"))?;
    let lead_name = record["localLeadName"].as_str().unwrap_or("Lead");
    let body = json!({
        "protocol": PROTOCOL,
        "inviteId": invite_id,
        "token": invite_token,
        "stateId": identity.id(),
        "label": identity.label(),
        "leadName": lead_name,
        "origin": local_origin,
        "publicKey": identity.public_key_pem(),
        "tailscaleUser": Value::Null,
        "tailscaleStatus": "unavailable",
    });
    core::validate("PairRequest", &body, "body")?;
    let deadline = Instant::now() + PASS_TIMEOUT;
    let budget = PassBudget::new(deadline, cancelled);
    let response = request(
        Arc::clone(&store),
        transport,
        Request {
            peer: &peer,
            identity: &identity,
            path: "/api/federation/v1/pair",
            body,
            now: now as i64,
            budget: &budget,
        },
    )
    .await?;
    let whois_status = response["whoisStatus"]
        .as_str()
        .filter(|status| matches!(*status, "verified" | "missing" | "unavailable"))
        .ok_or(Error::Invalid("pair.response.whoisStatus"))?;
    if response["accepted"] != true || response["stateId"] != peer_id {
        return Err(Error::Invalid("pair.response"));
    }
    let peer_id = peer_id.to_owned();
    let invite_id = invite_id.to_owned();
    let invite_token = invite_token.to_owned();
    let record = record.clone();
    let response = response.clone();
    let whois_status = whois_status.to_owned();
    store.transaction_with(move |tx| {
        let latest = Repo::record(tx, Table::Peer, &peer_id, &peer_id)?
            .ok_or(Error::Conflict("pairing intent disappeared"))?;
        if latest["inviteId"] != invite_id
            || latest["inviteToken"] != invite_token
            || latest["publicKey"] != record["publicKey"]
        {
            return Err(Error::Conflict("pairing intent changed"));
        }
        let mut updated = latest;
        updated["remoteApproved"] = response
            .get("remoteApproved")
            .cloned()
            .unwrap_or(Value::Bool(false));
        updated["whoisStatus"] = json!(whois_status);
        updated["whoisUser"] = record
            .get("whoisUser")
            .cloned()
            .unwrap_or(Value::Null);
        if whois_status == "missing" {
            updated["localApproved"] = Value::Bool(false);
            updated["lastError"] = json!("Tailscale identity was unavailable; explicitly accept this warning to finish pairing");
        } else {
            updated["lastError"] = Value::Null;
        }
        updated["status"] = json!(if updated["localApproved"] == true && updated["remoteApproved"] == true {
            "approved"
        } else {
            "pending"
        });
        updated["updated"] = json!(now);
        Repo::put(tx, Table::Peer, &peer_id, &peer_id, &updated)?;
        Ok::<_, Error>(updated)
    }).await
}
struct Request<'a> {
    peer: &'a core::Peer,
    identity: &'a RemoteIdentity,
    path: &'a str,
    body: Value,
    now: i64,
    budget: &'a PassBudget,
}

async fn request(
    store: Arc<RuntimeStore>,
    transport: &impl FederationTransport,
    request: Request<'_>,
) -> Result<Value> {
    let Request {
        peer,
        identity,
        path,
        body,
        now,
        budget,
    } = request;
    let body = transport::canonical_json(&body)?.into_bytes();
    let url = format!("{}{path}", peer.origin);
    let headers = transport::federation_signed_headers(
        identity,
        "POST",
        &url,
        now,
        &transport::new_nonce()?,
        &body,
        now,
    )?;
    budget.remaining()?;
    let response = transport
        .exchange(FederationExchange {
            store: Arc::clone(&store),
            server_id: peer.state_id.clone(),
            public_key: peer.public_key.clone(),
            url,
            headers,
            body,
            deadline: budget.deadline(),
            cancelled: Arc::clone(&budget.cancelled),
        })
        .await?;
    budget.remaining()?;
    if response.body.len() > transport::MAX_REQUEST_BYTES {
        return Err(Error::Invalid("response.size"));
    }
    let verified = transport::verify_federation_response(
        &response.body,
        &peer.public_key,
        Some(&peer.state_id),
        now,
    )?;
    transport::learn_paired_request_link(&store, &peer.state_id, response.link_advert.as_deref());
    Ok(verified)
}

/// One bounded pass for an approved peer; the runtime timer owns scheduling/cancellation
/// between calls. Failed delivery keeps the same immutable message for safe replay.
pub async fn pump_peer(
    store: Arc<RuntimeStore>,
    peer_id: &str,
    now: f64,
    transport: &impl FederationTransport,
    cancelled: Arc<AtomicBool>,
) -> Result<usize> {
    pump_peer_until(
        store,
        peer_id,
        now,
        transport,
        PassBudget::new(Instant::now() + PASS_TIMEOUT, cancelled),
    )
    .await
}

pub(crate) async fn pump_peer_until(
    store: Arc<RuntimeStore>,
    peer_id: &str,
    now: f64,
    transport: &impl FederationTransport,
    budget: PassBudget,
) -> Result<usize> {
    if budget.remaining().is_err() {
        return Ok(0);
    }
    let store_for_read = Arc::clone(&store);
    let identity =
        tokio::task::spawn_blocking(move || transport::load_federation_identity(&store_for_read))
            .await
            .map_err(|_| Error::Unavailable("federation/identity-read-worker"))??;
    let identity = Arc::new(identity);
    let peer_id_owned = peer_id.to_owned();
    let identity_for_claim = Arc::clone(&identity);
    let (peer, queued) = store
        .transaction_with(move |tx| {
            let state = snapshot(tx, &peer_id_owned, "", "", now)?;
            state.authorize(false)?;
            effects::check_identity(tx, identity_for_claim.as_ref())?;
            Ok::<_, Error>((
                state.peer,
                Repo::batch(tx, Table::Outbox, &peer_id_owned, now)?,
            ))
        })
        .await?;
    let mut delivered = 0;
    for (id, item) in queued {
        if budget.remaining().is_err() {
            return Ok(delivered);
        }
        // Re-check revocation and record identity immediately before each send. A later
        // revocation may race with the network; its receipt cannot revive local state.
        let envelope = effects::outbound(&item, &identity, now as i64)?;
        let hash = core::envelope_hash(&envelope)?;
        let peer_id_owned = peer_id.to_owned();
        let identity_for_check = Arc::clone(&identity);
        let peer_for_check = peer.clone();
        let id_for_check = id.clone();
        let hash_for_check = hash.clone();
        store
            .transaction_with(move |tx| {
                let state = snapshot(tx, &peer_id_owned, "", "", now)?;
                state.authorize(false)?;
                if state.peer.public_key != peer_for_check.public_key {
                    return Err(Error::Conflict("peer key changed"));
                }
                let mut latest = Repo::record(tx, Table::Outbox, &id_for_check, &peer_id_owned)?
                    .ok_or(Error::Conflict("outbox removed"))?;
                if latest["status"] != "queued"
                    || core::envelope_hash(&effects::outbound(
                        &latest,
                        identity_for_check.as_ref(),
                        now as i64,
                    )?)? != hash_for_check
                {
                    return Err(Error::Conflict("outbox changed"));
                }
                latest["hash"] = json!(hash_for_check);
                Repo::put(tx, Table::Outbox, &id_for_check, &peer_id_owned, &latest)?;
                Ok(())
            })
            .await?;
        let result = request(
            Arc::clone(&store),
            transport,
            Request {
                peer: &peer,
                identity: &identity,
                path: "/api/federation/v1/message",
                body: json!({"protocol":PROTOCOL,"envelope":envelope}),
                now: now as i64,
                budget: &budget,
            },
        )
        .await
        .and_then(|value| {
            core::validate(
                "studio_api__federation__models__MessageReceipt",
                &value,
                "receipt",
            )?;
            if value["messageId"] != id || value["room"] != item["room"] {
                return Err(Error::Conflict("receipt identity mismatch"));
            }
            Ok(value)
        });
        let result = match result {
            Ok(value) => value,
            Err(error) => {
                let peer_id_owned = peer_id.to_owned();
                let id_owned = id.clone();
                store
                    .transaction_with(move |tx| {
                        if let Some(mut latest) =
                            Repo::record(tx, Table::Outbox, &id_owned, &peer_id_owned)?
                            && latest["status"] == "queued"
                        {
                            let attempts = latest["attempts"]
                                .as_u64()
                                .ok_or(Error::Invalid("outbox.attempts"))?
                                .saturating_add(1)
                                .min(16);
                            latest["attempts"] = json!(attempts);
                            latest["nextAt"] =
                                json!(now + (1_u64 << attempts.min(8)).min(300) as f64);
                            latest["error"] =
                                json!("Federation transport did not return a verified receipt");
                            Repo::put(tx, Table::Outbox, &id_owned, &peer_id_owned, &latest)?;
                        }
                        Ok::<_, Error>(())
                    })
                    .await?;
                return Err(error);
            }
        };
        let peer_id_owned = peer_id.to_owned();
        let peer_for_ack = peer.clone();
        let id_for_ack = id.clone();
        let hash_for_ack = hash.clone();
        let result_for_ack = result.clone();
        store
            .transaction_with(move |tx| {
                let state = snapshot(tx, &peer_id_owned, "", "", now)?;
                state.authorize(false)?;
                if state.peer.public_key != peer_for_ack.public_key {
                    return Err(Error::Conflict("peer key changed"));
                }
                if effects::acknowledge_outbox(
                    tx,
                    &peer_id_owned,
                    &json!({"id":id_for_ack,"hash":hash_for_ack}),
                    now,
                    true,
                )? {
                    let mut latest = Repo::record(tx, Table::Outbox, &id_for_ack, &peer_id_owned)?
                        .ok_or(Error::Conflict("outbox removed"))?;
                    latest["receipt"] = result_for_ack.clone();
                    Repo::put(tx, Table::Outbox, &id_for_ack, &peer_id_owned, &latest)?;
                }
                Ok::<_, Error>(())
            })
            .await?;
        delivered += 1;
    }
    if budget.remaining().is_err() {
        return Ok(delivered);
    }
    let peer_id_owned = peer_id.to_owned();
    let peer_for_acks = peer.clone();
    let acks = store
        .transaction_with(move |tx| {
            let state = snapshot(tx, &peer_id_owned, "", "", now)?;
            state.authorize(false)?;
            if state.peer.public_key != peer_for_acks.public_key {
                return Err(Error::Conflict("peer key changed"));
            }
            Ok::<_, Error>(
                Repo::batch(tx, Table::Inbox, &peer_id_owned, now)?
                    .into_iter()
                    .map(|(id, item)| json!({"id":id,"hash":item["hash"]}))
                    .collect::<Vec<_>>(),
            )
        })
        .await?;
    let response = request(
        Arc::clone(&store),
        transport,
        Request {
            peer: &peer,
            identity: &identity,
            path: "/api/federation/v1/pull",
            body: json!({"protocol":PROTOCOL,"acks":acks}),
            now: now as i64,
            budget: &budget,
        },
    )
    .await?;
    effects::reconcile(&store, peer_id, &peer.public_key, &response, now).await?;
    Ok(delivered)
}

/// Local message producer used by the message dispatcher and user-room action.
pub async fn send_message(
    store: Arc<RuntimeStore>,
    sender: &str,
    room_id: &str,
    text: &str,
    key: &str,
    now: f64,
) -> Result<Value> {
    let store_for_read = Arc::clone(&store);
    let identity =
        tokio::task::spawn_blocking(move || transport::load_federation_identity(&store_for_read))
            .await
            .map_err(|_| Error::Unavailable("federation/identity-read-worker"))??;
    let identity = Arc::new(identity);
    let sender = sender.to_owned();
    let room_id = room_id.to_owned();
    let text = text.to_owned();
    let key = key.to_owned();
    store
        .transaction_with(move |tx| {
            effects::check_identity(tx, identity.as_ref())?;
            send_message_in_transaction(tx, &sender, &room_id, &text, &key, now)
        })
        .await
}

/// The Python `/api/messages` path sends a user message as the local lead.
pub async fn user_message(
    store: Arc<RuntimeStore>,
    room_id: &str,
    text: &str,
    key: &str,
    now: f64,
) -> Result<Value> {
    let text = crate::event_write::python_strip(text);
    if text.is_empty() || text.chars().count() > core::MAX_MESSAGE || key.len() > 4096 {
        return Err(Error::Invalid("message"));
    }
    let store_for_read = Arc::clone(&store);
    let room_id_for_read = room_id.to_owned();
    let sender = tokio::task::spawn_blocking(move || {
        store_for_read.read_transaction(|connection| {
            let room = Repo::record(connection, Table::Room, &room_id_for_read, "")?.ok_or(
                StoreError::SchemaMismatch("shared room is unavailable".to_owned()),
            )?;
            let sender = room
                .get("localParticipants")
                .and_then(Value::as_array)
                .and_then(|participants| {
                    participants.iter().find_map(|person| {
                        (person.get("role").and_then(Value::as_str) == Some("lead"))
                            .then(|| person.get("id").and_then(Value::as_str))
                            .flatten()
                    })
                })
                .ok_or(StoreError::SchemaMismatch(
                    "the local lead is not a room participant".to_owned(),
                ))?;
            Ok::<_, StoreError>(sender.to_owned())
        })
    })
    .await
    .map_err(|_| Error::Unavailable("federation/room-read-worker"))??;
    send_message(store, &sender, room_id, text, key, now).await
}

/// Queue a federation room message in the caller's owning transaction. This is
/// used by provider callbacks, whose operation receipt and federation outbox
/// must commit atomically. It reads only the public namespace from the stored
/// identity row; signing material is loaded later by the delivery pass.
pub fn send_message_in_transaction(
    tx: &Transaction<'_>,
    sender: &str,
    room_id: &str,
    text: &str,
    key: &str,
    now: f64,
) -> Result<Value> {
    let text = crate::event_write::python_strip(text);
    if text.is_empty() || text.chars().count() > core::MAX_MESSAGE || key.len() > 4096 {
        return Err(Error::Invalid("message"));
    }
    let state_id: String = tx
        .query_row(
            "SELECT json_extract(record,'$.stateId') FROM runtime_federation_identity WHERE id='local'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(StoreError::from)?
        .ok_or(Error::Unavailable("federation-credentials/private-key"))?;
    let namespace =
        uuid::Uuid::parse_str(&state_id).map_err(|_| Error::Invalid("identity.stateId"))?;
    let message_id = uuid::Uuid::new_v5(&namespace, key.as_bytes()).to_string();
    let mut room =
        Repo::record(tx, Table::Room, room_id, "")?.ok_or(Error::Forbidden("room unavailable"))?;
    let peer = room["peerId"]
        .as_str()
        .ok_or(Error::Invalid("room.peerId"))?
        .to_owned();
    snapshot(tx, &peer, room_id, "", now)?.authorize(false)?;
    if room["status"] != "approved"
        || room["localApproved"] != true
        || room["remoteApproved"] != true
        || !room["localMembers"]
            .as_array()
            .is_some_and(|members| members.iter().any(|v| v == sender))
    {
        return Err(Error::Forbidden("sender is not an approved room member"));
    }
    let sender_agent = crate::event_write::checked_agent(tx, sender)?;
    let existing: Option<(String, String, String, String)> = tx
        .query_row(
            "SELECT room,sender,text,deliveries FROM runtime_chat_messages WHERE id=?1",
            [&message_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(StoreError::from)?;
    if let Some((prior_room, prior_sender, prior_text, raw)) = existing {
        if (
            prior_room.as_str(),
            prior_sender.as_str(),
            prior_text.as_str(),
        ) != (room_id, sender, text)
        {
            return Err(Error::Conflict("message identity has different content"));
        }
        let deliveries: Value = serde_json::from_str(&raw).map_err(StoreError::from)?;
        let object = deliveries
            .as_object()
            .ok_or(Error::Invalid("chat.deliveries"))?;
        let status = if !object.is_empty() && object.values().all(|v| v == "delivered") {
            "delivered"
        } else {
            "queued"
        };
        return Ok(json!({"id":message_id,"room":room_id,"deliveries":deliveries,"status":status}));
    }
    let person = room
        .get("localParticipants")
        .and_then(Value::as_array)
        .and_then(|people| people.iter().find(|person| person["id"] == sender));
    let name = person
        .and_then(|person| person.get("name"))
        .cloned()
        .filter(|value| !value.is_null())
        .or_else(|| {
            if sender_agent.extra.get("isLead") == Some(&json!(true)) {
                sender_agent
                    .extra
                    .get("name")
                    .cloned()
                    .or_else(|| Some(json!("Lead")))
            } else {
                None
            }
        })
        .unwrap_or(Value::Null);
    let sequence = room
        .get("sequence")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(Error::Invalid("room.sequence"))?;
    let payload =
        json!({"sender":sender,"senderName":name,"text":text,"created":now,"sequence":sequence});
    let item = json!({"id":message_id,"kind":"chat-message","room":room_id,"payload":payload,"created":now,"status":"queued","attempts":0,"nextAt":0,"error":null});
    let item = core::decide_queue(
        Repo::record(tx, Table::Outbox, &message_id, &peer)?.as_ref(),
        item,
        Repo::queue_size(tx, &peer)?,
    )?;
    let deliveries = json!({format!("remote:{peer}"):"queued"});
    tx.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES(?1,?2,?3,?4,?5,?6)", params![message_id, room_id, sender, text, now, transport::canonical_json(&deliveries)?]).map_err(StoreError::from)?;
    let event = json!({"room":room_id,"message_id":message_id,"sender":sender,"sender_name":name,"text":text,"untrusted_remote":false,"authority":"room-data-only"});
    for member in room["localMembers"]
        .as_array()
        .ok_or(Error::Invalid("room.localMembers"))?
    {
        let member = member.as_str().ok_or(Error::Invalid("room.localMembers"))?;
        if member == sender {
            continue;
        }
        let agent = AgentRepository::get(tx, &AgentId::from(member))?
            .ok_or(Error::Invalid("room.localMembers"))?;
        if !crate::event_write::truthy(agent.extra.get("deletedAt")) {
            crate::event_write::enqueue_event(
                tx,
                &agent,
                format!("federation-local:{room_id}:{message_id}:{member}"),
                "agent_message",
                transport::canonical_json(&event)?,
                now,
            )?;
        }
    }
    Repo::put(tx, Table::Outbox, &message_id, &peer, &item)?;
    room["sequence"] = json!(sequence);
    room["updated"] = json!(now);
    effects::save_room(tx, &peer, &room)?;
    Ok(json!({"id":message_id,"room":room_id,"deliveries":deliveries,"status":"queued"}))
}

#[cfg(test)]
mod tests {
    use super::delivery_deadlines_by_peer;
    use serde_json::json;
    use studio_runtime_store::RuntimeStore;

    #[tokio::test]
    async fn next_delivery_deadline_ignores_unapproved_peers_and_nonqueued_rows() {
        let directory = tempfile::tempdir().expect("state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("runtime store");
        store
            .transaction(|tx| {
                for (peer_id, status) in [("approved", "approved"), ("pending", "pending")] {
                    tx.execute(
                        "INSERT INTO runtime_federation_peers(id,record) VALUES (?1,?2)",
                        rusqlite::params![peer_id, json!({"id":peer_id,"status":status}).to_string()],
                    )?;
                }
                for (id, peer, status, due) in [
                    ("future", "approved", "queued", 52.0),
                    ("earlier-but-unapproved", "pending", "queued", 1.0),
                    ("done", "approved", "sent", 0.0),
                ] {
                    tx.execute(
                        "INSERT INTO runtime_federation_outbox(id,peer_id,record) VALUES (?1,?2,?3)",
                        rusqlite::params![id, peer, json!({"status":status,"nextAt":due}).to_string()],
                    )?;
                }
                Ok(())
            })
            .await
            .expect("fixture rows commit");
        assert_eq!(
            delivery_deadlines_by_peer(&store).expect("deadline query"),
            vec![("approved".to_owned(), 52.0)]
        );
    }
}
