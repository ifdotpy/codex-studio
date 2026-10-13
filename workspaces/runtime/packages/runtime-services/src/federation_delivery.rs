//! Bounded federation delivery pass. Network work runs outside store transactions.
use crate::federation_effects::{self as effects, snapshot};
use crate::federation_messaging::{
    self as core, FederationMessageError as Error, PROTOCOL, Result,
};
use crate::remote_transport::{self as transport, RemoteIdentity};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use studio_runtime_store::federation_messaging::{
    FederationMessagingRepository as Repo, FederationTable as Table,
};
use studio_runtime_store::{AgentId, AgentRepository, RuntimeStore, StoreError};
const PASS_TIMEOUT: Duration = Duration::from_secs(8);

/// A single absolute deadline and cancellation signal shared by every exchange.
pub struct PassBudget<'a> {
    deadline: Instant,
    cancelled: &'a AtomicBool,
}
impl<'a> PassBudget<'a> {
    pub fn new(deadline: Instant, cancelled: &'a AtomicBool) -> Self {
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
pub trait FederationTransport {
    fn exchange(
        &mut self,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
        budget: &PassBudget<'_>,
    ) -> transport::RemoteTransportResult<Vec<u8>>;
}
pub struct HttpsFederationTransport;
impl FederationTransport for HttpsFederationTransport {
    fn exchange(
        &mut self,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
        budget: &PassBudget<'_>,
    ) -> transport::RemoteTransportResult<Vec<u8>> {
        let headers = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect::<Vec<_>>();
        // Blocking HTTPS observes cancellation before/after I/O; its in-flight
        // cancellation latency is bounded by the remaining absolute pass budget.
        let response = transport::SignedHttpClient::exchange(
            "POST",
            url,
            &headers,
            body,
            budget.remaining()?,
            transport::MAX_REQUEST_BYTES,
        )?;
        budget.remaining()?;
        if response.status != 200 {
            return Err(transport::RemoteTransportError::InvalidResponse);
        }
        Ok(response.body)
    }
}
fn request(
    transport: &mut impl FederationTransport,
    peer: &core::Peer,
    identity: &RemoteIdentity,
    path: &str,
    body: Value,
    now: i64,
    budget: &PassBudget<'_>,
) -> Result<Value> {
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
    let response = transport.exchange(&url, &headers, &body, budget)?;
    budget.remaining()?;
    if response.len() > transport::MAX_REQUEST_BYTES {
        return Err(Error::Invalid("response.size"));
    }
    Ok(transport::verify_federation_response(
        &response,
        &peer.public_key,
        Some(&peer.state_id),
        now,
    )?)
}

/// One bounded pass for an approved peer; the runtime timer owns scheduling/cancellation
/// between calls. Failed delivery keeps the same immutable message for safe replay.
pub fn pump_peer(
    store: &RuntimeStore,
    peer_id: &str,
    now: f64,
    transport: &mut impl FederationTransport,
    cancelled: &AtomicBool,
) -> Result<usize> {
    pump_peer_until(
        store,
        peer_id,
        now,
        transport,
        PassBudget::new(Instant::now() + PASS_TIMEOUT, cancelled),
    )
}

pub(crate) fn pump_peer_until(
    store: &RuntimeStore,
    peer_id: &str,
    now: f64,
    transport: &mut impl FederationTransport,
    budget: PassBudget<'_>,
) -> Result<usize> {
    if budget.remaining().is_err() {
        return Ok(0);
    }
    let identity = transport::load_federation_identity(store)?;
    let (peer, queued) = store.transaction_with(|tx| {
        let state = snapshot(tx, peer_id, "", "", now)?;
        state.authorize(false)?;
        effects::check_identity(tx, &identity)?;
        Ok::<_, Error>((state.peer, Repo::batch(tx, Table::Outbox, peer_id, now)?))
    })?;
    let mut delivered = 0;
    for (id, item) in queued {
        if budget.remaining().is_err() {
            return Ok(delivered);
        }
        // Re-check revocation and record identity immediately before each send. A later
        // revocation may race with the network; its receipt cannot revive local state.
        let envelope = effects::outbound(&item, &identity, now as i64)?;
        let hash = core::envelope_hash(&envelope)?;
        store.transaction_with(|tx| {
            let state = snapshot(tx, peer_id, "", "", now)?;
            state.authorize(false)?;
            if state.peer.public_key != peer.public_key {
                return Err(Error::Conflict("peer key changed"));
            }
            let mut latest = Repo::record(tx, Table::Outbox, &id, peer_id)?
                .ok_or(Error::Conflict("outbox removed"))?;
            if latest["status"] != "queued"
                || core::envelope_hash(&effects::outbound(&latest, &identity, now as i64)?)? != hash
            {
                return Err(Error::Conflict("outbox changed"));
            }
            latest["hash"] = json!(hash);
            Repo::put(tx, Table::Outbox, &id, peer_id, &latest)?;
            Ok(())
        })?;
        let result = request(
            transport,
            &peer,
            &identity,
            "/api/federation/v1/message",
            json!({"protocol":PROTOCOL,"envelope":envelope}),
            now as i64,
            &budget,
        )
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
                store.transaction_with(|tx| {
                    if let Some(mut latest) = Repo::record(tx, Table::Outbox, &id, peer_id)?
                        && latest["status"] == "queued"
                    {
                        let attempts = latest["attempts"]
                            .as_u64()
                            .ok_or(Error::Invalid("outbox.attempts"))?
                            .saturating_add(1)
                            .min(16);
                        latest["attempts"] = json!(attempts);
                        latest["nextAt"] = json!(now + (1_u64 << attempts.min(8)).min(300) as f64);
                        latest["error"] =
                            json!("Federation transport did not return a verified receipt");
                        Repo::put(tx, Table::Outbox, &id, peer_id, &latest)?;
                    }
                    Ok::<_, Error>(())
                })?;
                return Err(error);
            }
        };
        store.transaction_with(|tx| {
            let state = snapshot(tx, peer_id, "", "", now)?;
            state.authorize(false)?;
            if state.peer.public_key != peer.public_key {
                return Err(Error::Conflict("peer key changed"));
            }
            if effects::acknowledge_outbox(tx, peer_id, &json!({"id":id,"hash":hash}), now, true)? {
                let mut latest = Repo::record(tx, Table::Outbox, &id, peer_id)?
                    .ok_or(Error::Conflict("outbox removed"))?;
                latest["receipt"] = result.clone();
                Repo::put(tx, Table::Outbox, &id, peer_id, &latest)?;
            }
            Ok::<_, Error>(())
        })?;
        delivered += 1;
    }
    if budget.remaining().is_err() {
        return Ok(delivered);
    }
    let acks = store.transaction_with(|tx| {
        let state = snapshot(tx, peer_id, "", "", now)?;
        state.authorize(false)?;
        if state.peer.public_key != peer.public_key {
            return Err(Error::Conflict("peer key changed"));
        }
        Ok::<_, Error>(
            Repo::batch(tx, Table::Inbox, peer_id, now)?
                .into_iter()
                .map(|(id, item)| json!({"id":id,"hash":item["hash"]}))
                .collect::<Vec<_>>(),
        )
    })?;
    let response = request(
        transport,
        &peer,
        &identity,
        "/api/federation/v1/pull",
        json!({"protocol":PROTOCOL,"acks":acks}),
        now as i64,
        &budget,
    )?;
    effects::reconcile(store, peer_id, &peer.public_key, &response, now)?;
    Ok(delivered)
}

/// Local message producer used by the message dispatcher and user-room action.
pub fn send_message(
    store: &RuntimeStore,
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
    let identity = transport::load_federation_identity(store)?;
    let namespace =
        uuid::Uuid::parse_str(identity.id()).map_err(|_| Error::Invalid("identity.stateId"))?;
    let message_id = uuid::Uuid::new_v5(&namespace, key.as_bytes()).to_string();
    store.transaction_with(|tx| {
        effects::check_identity(tx,&identity)?;
        let mut room=Repo::record(tx,Table::Room,room_id,"")?.ok_or(Error::Forbidden("room unavailable"))?;
        let peer=room["peerId"].as_str().ok_or(Error::Invalid("room.peerId"))?.to_owned();
        snapshot(tx,&peer,room_id,"",now)?.authorize(false)?;
        if room["status"]!="approved" || room["localApproved"]!=true || room["remoteApproved"]!=true || !room["localMembers"].as_array().is_some_and(|a|a.iter().any(|v|v==sender)) {return Err(Error::Forbidden("sender is not an approved room member"));}
        let sender_agent=crate::event_write::checked_agent(tx,sender)?;
        let existing:Option<(String,String,String,String)>=tx.query_row("SELECT room,sender,text,deliveries FROM runtime_chat_messages WHERE id=?1",[&message_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(StoreError::from)?;
        if let Some((prior_room,prior_sender,prior_text,raw))=existing {
            if (prior_room.as_str(),prior_sender.as_str(),prior_text.as_str())!=(room_id,sender,text) {return Err(Error::Conflict("message identity has different content"));}
            let deliveries:Value=serde_json::from_str(&raw).map_err(StoreError::from)?;
            let object=deliveries.as_object().ok_or(Error::Invalid("chat.deliveries"))?;
            let status=if !object.is_empty() && object.values().all(|v|v=="delivered") {"delivered"} else {"queued"};
            return Ok(json!({"id":message_id,"room":room_id,"deliveries":deliveries,"status":status}));
        }
        let person=room.get("localParticipants").and_then(Value::as_array).and_then(|a|a.iter().find(|p|p["id"]==sender));
        let name=person.and_then(|p|p.get("name")).cloned().filter(|v|!v.is_null()).or_else(||if sender_agent.extra.get("isLead")==Some(&json!(true)) {sender_agent.extra.get("name").cloned().or_else(||Some(json!("Lead")))} else {None}).unwrap_or(Value::Null);
        let sequence=room.get("sequence").and_then(Value::as_u64).unwrap_or(0).checked_add(1).ok_or(Error::Invalid("room.sequence"))?;
        let payload=json!({"sender":sender,"senderName":name,"text":text,"created":now,"sequence":sequence});
        let item=json!({"id":message_id,"kind":"chat-message","room":room_id,"payload":payload,"created":now,"status":"queued","attempts":0,"nextAt":0,"error":null});
        let item=core::decide_queue(Repo::record(tx,Table::Outbox,&message_id,&peer)?.as_ref(),item,Repo::queue_size(tx,&peer)?)?;
        let deliveries=json!({format!("remote:{peer}"):"queued"});
        tx.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES(?1,?2,?3,?4,?5,?6)",params![message_id,room_id,sender,text,now,transport::canonical_json(&deliveries)?]).map_err(StoreError::from)?;
        let event=json!({"room":room_id,"message_id":message_id,"sender":sender,"sender_name":name,"text":text,"untrusted_remote":false,"authority":"room-data-only"});
        for member in room["localMembers"].as_array().ok_or(Error::Invalid("room.localMembers"))? {
            let member=member.as_str().ok_or(Error::Invalid("room.localMembers"))?;
            if member==sender {continue;}
            let agent=AgentRepository::get(tx,&AgentId::from(member))?.ok_or(Error::Invalid("room.localMembers"))?;
            if !crate::event_write::truthy(agent.extra.get("deletedAt")) {crate::event_write::enqueue_event(tx,&agent,format!("federation-local:{room_id}:{message_id}:{member}"),"agent_message",transport::canonical_json(&event)?,now)?;}
        }
        Repo::put(tx,Table::Outbox,&message_id,&peer,&item)?;
        room["sequence"]=json!(sequence);room["updated"]=json!(now);effects::save_room(tx,&peer,&room)?;
        Ok(json!({"id":message_id,"room":room_id,"deliveries":deliveries,"status":"queued"}))
    })
}
