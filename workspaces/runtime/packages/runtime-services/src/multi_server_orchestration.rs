//! Durable owner-paired envelopes. A transaction claims each exact request
//! before transport or execution. An uncertain claim is never executed again.
use std::sync::Arc;

use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use studio_runtime_store::RuntimeStore;

use crate::multi_server_auth::{AccessError, digest, refused, schema, stored, text};
use crate::remote_transport::{MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, canonical_json};

#[derive(Clone, Debug, PartialEq)]
pub enum ExchangeOutcome {
    Applied(Value),
    NotApplied(String),
    Unknown,
}
impl ExchangeOutcome {
    fn to_value(&self, key: &str) -> Value {
        match self {
            Self::Applied(v) => json!({"requestId":key,"outcome":"applied","value":v}),
            Self::NotApplied(e) => json!({"requestId":key,"outcome":"not_applied","error":e}),
            Self::Unknown => json!({"requestId":key,"outcome":"unknown"}),
        }
    }
    fn from_value(value: &Value, key: &str, now: i64) -> Result<Self, AccessError> {
        if text(value, "requestId") != key || value.to_string().len() > MAX_RESPONSE_BYTES {
            return Err(refused(
                502,
                "invalid_receipt",
                "The paired server returned an invalid receipt",
            ));
        }
        if value
            .get("value")
            .and_then(|v| v.get("outputExpiresAt"))
            .and_then(Value::as_f64)
            .is_some_and(|v| v <= now as f64)
        {
            return Ok(Self::Unknown);
        }
        match text(value, "outcome") {
            "applied" => value
                .get("value")
                .cloned()
                .map(Self::Applied)
                .ok_or_else(|| {
                    refused(
                        502,
                        "invalid_receipt",
                        "The paired server returned an invalid receipt",
                    )
                }),
            "not_applied" => value
                .get("error")
                .and_then(Value::as_str)
                .map(|s| Self::NotApplied(s.to_owned()))
                .ok_or_else(|| {
                    refused(
                        502,
                        "invalid_receipt",
                        "The paired server returned an invalid receipt",
                    )
                }),
            "unknown" => Ok(Self::Unknown),
            _ => Err(refused(
                502,
                "invalid_receipt",
                "The paired server returned an invalid receipt",
            )),
        }
    }
}
/// The transport must use the persisted envelope's requestId and return only a
/// response verified against that server's saved credential. It must complete
/// within the supplied deadline; a lost response returns Unknown, never retries.
pub trait PairedTransport: Send + Sync {
    fn exchange(
        &self,
        server: &str,
        envelope: &Value,
        timeout: std::time::Duration,
    ) -> Result<Value, AccessError>;
}
/// Missing effects are refused before claiming a receipt. Implementations must
/// derive actor permissions from the authenticated principal, not the payload.
pub trait OrchestrationEffects: Send + Sync {
    fn supports(&self, action: &str) -> bool;
    fn execute(&self, principal: &str, envelope: &Value) -> Result<ExchangeOutcome, AccessError>;
}

pub struct Envelope {
    key: String,
    action: String,
    payload: Value,
    body: String,
    fingerprint: String,
}
impl Envelope {
    pub fn from_value(value: &Value) -> Result<Self, AccessError> {
        schema("ServerOperationRequest", value)?;
        let body = canonical_json(value)
            .map_err(|_| refused(400, "invalid_request", "Invalid cross-server request"))?;
        if body.len() > MAX_REQUEST_BYTES {
            return Err(refused(
                413,
                "request_size",
                "The remote request exceeds 256 KiB",
            ));
        }
        let action = text(value, "action");
        if matches!(action, "move_validate" | "move_begin")
            && value.get("payload").and_then(|v| v.get("packed")).is_some()
        {
            return Err(refused(
                501,
                "move_payload",
                "Not implemented: requires multi-server-runtime/packed-move-payload",
            ));
        }
        Ok(Self {
            key: text(value, "requestId").to_owned(),
            action: action.to_owned(),
            payload: value["payload"].clone(),
            fingerprint: digest(body.as_bytes()),
            body,
        })
    }
    pub fn value(&self) -> Value {
        json!({"requestId":self.key,"action":self.action,"payload":self.payload})
    }
    fn incoming_signature(&self, principal: &str) -> Result<String, AccessError> {
        Ok(digest(
            canonical_json(&json!([principal, self.action, self.payload]))
                .map_err(|_| refused(400, "invalid_request", "Invalid cross-server request"))?
                .as_bytes(),
        ))
    }
}

/// A checked constructor is the only path from Python-written SQL rows into a
/// delivery decision. The raw result is validated against the envelope identity.
pub struct DeliverySnapshot {
    state: String,
    signature: String,
    server: Option<String>,
    result: Option<ExchangeOutcome>,
    attempts: u64,
}
impl DeliverySnapshot {
    pub fn from_record(
        table: &'static str,
        id: &str,
        raw: &str,
        now: i64,
    ) -> Result<Self, AccessError> {
        let value = stored("PersistedServerDelivery", table, id, raw)?;
        if text(&value, "id") != id {
            return Err(refused(
                503,
                "invalid_record",
                "Stored delivery identity differs from its row",
            ));
        }
        let body = value.get("body").and_then(Value::as_str);
        let signature = value
            .get("signature")
            .and_then(Value::as_str)
            .or_else(|| value.get("fingerprint").and_then(Value::as_str))
            .map(str::to_owned)
            .or_else(|| body.map(|b| digest(b.as_bytes())))
            .ok_or_else(|| refused(503, "invalid_record", "Stored delivery has no identity"))?;
        let result = value
            .get("result")
            .and_then(Value::as_str)
            .map(|s| {
                serde_json::from_str::<Value>(s)
                    .map_err(|_| {
                        refused(503, "invalid_record", "Stored delivery result is invalid")
                    })
                    .and_then(|v| ExchangeOutcome::from_value(&v, id, now))
            })
            .transpose()?;
        if text(&value, "state") == "complete" && result.is_none() {
            return Err(refused(
                503,
                "invalid_record",
                "Completed delivery has no receipt",
            ));
        }
        Ok(Self {
            state: text(&value, "state").to_owned(),
            signature,
            server: value
                .get("server")
                .and_then(Value::as_str)
                .map(str::to_owned),
            result,
            attempts: value.get("attempts").and_then(Value::as_u64).unwrap_or(0),
        })
    }
}
pub enum DeliveryDecision {
    Execute,
    Return(ExchangeOutcome),
}
pub fn decide_delivery(
    snapshot: Option<&DeliverySnapshot>,
    signature: &str,
    server: Option<&str>,
    outgoing: bool,
) -> Result<DeliveryDecision, AccessError> {
    let Some(snapshot) = snapshot else {
        return Ok(DeliveryDecision::Execute);
    };
    if snapshot.signature != signature || snapshot.server.as_deref() != server {
        return Err(refused(
            409,
            "request_conflict",
            "This request id has different content",
        ));
    }
    if snapshot.state == "complete" {
        return Ok(DeliveryDecision::Return(
            snapshot.result.clone().ok_or_else(|| {
                refused(503, "invalid_record", "Completed delivery has no receipt")
            })?,
        ));
    }
    if (!outgoing && snapshot.state == "waiting")
        || (outgoing && snapshot.state == "queued" && snapshot.attempts == 0)
    {
        return Ok(DeliveryDecision::Execute);
    }
    Ok(DeliveryDecision::Return(ExchangeOutcome::Unknown))
}

pub struct OrchestrationDriver {
    store: Arc<RuntimeStore>,
    transport: Arc<dyn PairedTransport>,
    effects: Arc<dyn OrchestrationEffects>,
}
impl OrchestrationDriver {
    pub fn new(
        store: Arc<RuntimeStore>,
        transport: Arc<dyn PairedTransport>,
        effects: Arc<dyn OrchestrationEffects>,
    ) -> Self {
        Self {
            store,
            transport,
            effects,
        }
    }
    pub fn exchange(
        &self,
        server: &str,
        action: &str,
        payload: &Value,
        request_id: &str,
        now: i64,
    ) -> Result<ExchangeOutcome, AccessError> {
        let envelope = Envelope::from_value(
            &json!({"requestId":request_id,"action":action,"payload":payload}),
        )?;
        let decision=self.store.transaction_with(|tx| {
            require_paired(tx,server)?;
            if retired(tx,&envelope.key,"out",&envelope.fingerprint,Some(server))? {return Ok(DeliveryDecision::Return(ExchangeOutcome::Unknown));}
            let saved=outbox(tx,&envelope.key,now)?;
            let decision=decide_delivery(saved.as_ref(),&envelope.fingerprint,Some(server),true)?;
            if matches!(decision,DeliveryDecision::Execute) {
                tx.execute("INSERT INTO runtime_server_outbox(id,server,body,state,created_at,fingerprint,attempts) VALUES(?1,?2,?3,'running',?4,?5,1) ON CONFLICT(id) DO UPDATE SET state='running',attempts=attempts+1",params![envelope.key,server,envelope.body,now,envelope.fingerprint])?;
            }
            Ok::<_,AccessError>(decision)
        })?;
        if let DeliveryDecision::Return(result) = decision {
            return Ok(result);
        }
        // The claim is committed before crossing the transport boundary.
        let result = match self.transport.exchange(
            server,
            &envelope.value(),
            std::time::Duration::from_secs(20),
        ) {
            Ok(value) => ExchangeOutcome::from_value(&value, &envelope.key, now)
                .unwrap_or(ExchangeOutcome::Unknown),
            Err(_) => ExchangeOutcome::Unknown,
        };
        self.finish(
            "outbox",
            &envelope.key,
            &envelope.fingerprint,
            Some(server),
            &result,
            now,
        )?;
        Ok(result)
    }
    pub fn receive(
        &self,
        principal: &str,
        value: &Value,
        now: i64,
    ) -> Result<ExchangeOutcome, AccessError> {
        let envelope = Envelope::from_value(value)?;
        if !self.effects.supports(&envelope.action) {
            return Err(refused(
                501,
                "orchestration_effect",
                "Not implemented: requires multi-server-runtime/orchestration-effects",
            ));
        }
        let signature = envelope.incoming_signature(principal)?;
        let decision=self.store.transaction_with(|tx| {
            require_paired(tx,principal)?;
            if retired(tx,&envelope.key,"in",&signature,None)? {return Ok(DeliveryDecision::Return(ExchangeOutcome::Unknown));}
            let saved=inbox(tx,&envelope.key,now)?;
            let decision=decide_delivery(saved.as_ref(),&signature,None,false)?;
            if matches!(decision,DeliveryDecision::Execute) {
                if saved.is_some() {
                    tx.execute("UPDATE runtime_server_inbox SET state='running' WHERE id=?1 AND state='waiting'", [&envelope.key])?;
                } else {
                    tx.execute("INSERT INTO runtime_server_inbox(id,signature,state,result,created_at,principal,actor) VALUES(?1,?2,'running',NULL,?3,?4,?5)",params![envelope.key,signature,now,principal,envelope.payload.get("actor").and_then(Value::as_str)])?;
                }
            }
            Ok::<_,AccessError>(decision)
        })?;
        if let DeliveryDecision::Return(result) = decision {
            return Ok(result);
        }
        let result = self
            .effects
            .execute(principal, &envelope.value())
            .unwrap_or(ExchangeOutcome::Unknown);
        if result.to_value(&envelope.key).to_string().len() > MAX_RESPONSE_BYTES {
            return Ok(ExchangeOutcome::Unknown);
        }
        self.finish("inbox", &envelope.key, &signature, None, &result, now)?;
        Ok(result)
    }
    fn finish(
        &self,
        table: &str,
        key: &str,
        signature: &str,
        server: Option<&str>,
        result: &ExchangeOutcome,
        now: i64,
    ) -> Result<(), AccessError> {
        if *result == ExchangeOutcome::Unknown {
            return Ok(());
        }
        self.store.transaction_with(|tx| {
            let saved=if table=="outbox"{outbox(tx,key,now)?}else{inbox(tx,key,now)?};
            let snapshot=saved.ok_or_else(||refused(409,"receipt_missing","The delivery claim is missing"))?;
            if snapshot.signature!=signature || snapshot.server.as_deref()!=server || snapshot.state!="running" { return Err(refused(409,"stale_receipt","The delivery receipt does not match its claim")); }
            tx.execute(&format!("UPDATE runtime_server_{table} SET state='complete',result=?1,completed_at=?2 WHERE id=?3"),params![result.to_value(key).to_string(),now,key])?;
            Ok(())
        })
    }
}
fn require_paired(tx: &Transaction<'_>, id: &str) -> Result<(), AccessError> {
    let raw: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_access_clients WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    let value = raw
        .map(|s| stored("PersistedAccessClient", "runtime_access_clients", id, &s))
        .transpose()?
        .ok_or_else(|| refused(403, "client_revoked", "The server is not paired"))?;
    if text(&value, "id") != id
        || text(&value, "status") != "paired"
        || text(&value, "kind") != "server"
    {
        return Err(refused(403, "client_revoked", "The server is not paired"));
    }
    Ok(())
}
fn outbox(
    tx: &Transaction<'_>,
    key: &str,
    now: i64,
) -> Result<Option<DeliverySnapshot>, AccessError> {
    let raw:Option<String>=tx.query_row("SELECT json_object('id',id,'server',server,'body',body,'state',state,'result',result,'fingerprint',fingerprint,'attempts',attempts,'created_at',created_at,'completed_at',completed_at) FROM runtime_server_outbox WHERE id=?1",[key],|r|r.get(0)).optional()?;
    raw.map(|s| DeliverySnapshot::from_record("runtime_server_outbox", key, &s, now))
        .transpose()
}
fn inbox(
    tx: &Transaction<'_>,
    key: &str,
    now: i64,
) -> Result<Option<DeliverySnapshot>, AccessError> {
    let raw:Option<String>=tx.query_row("SELECT json_object('id',id,'signature',signature,'state',state,'result',result,'created_at',created_at,'completed_at',completed_at) FROM runtime_server_inbox WHERE id=?1",[key],|r|r.get(0)).optional()?;
    raw.map(|s| DeliverySnapshot::from_record("runtime_server_inbox", key, &s, now))
        .transpose()
}
fn retired(
    tx: &Transaction<'_>,
    key: &str,
    direction: &str,
    signature: &str,
    server: Option<&str>,
) -> Result<bool, AccessError> {
    let row = tx
        .query_row(
            "SELECT signature,server FROM runtime_server_retired WHERE id=?1 AND direction=?2",
            params![key, direction],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    if let Some((stored_signature, stored_server)) = row {
        if stored_signature != signature || stored_server.as_deref() != server {
            return Err(refused(
                409,
                "request_conflict",
                "This request id has different content",
            ));
        }
        return Ok(true);
    }
    Ok(false)
}
