//! Native maintenance intents. Admission, hand-off and settlement each commit
//! separately; a committed hand-off is never automatically resent after restart.
use crate::workspace_operations::ProviderTicket;
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{future::Future, pin::Pin, sync::Arc};
use studio_runtime_store::{
    python_json_with_separators, AgentRecord, AgentRepository, ContextRepairRepository,
    RuntimeStore, StoreError,
};
use thiserror::Error;

const MAX_ID: usize = 200;
const MAX_BYTES: usize = 1024 * 1024;
pub type Result<T> = std::result::Result<T, NativeError>;
pub type NativeFuture<'a> = Pin<Box<dyn Future<Output = Result<TransportReceipt>> + Send + 'a>>;

#[derive(Debug, Error)]
pub enum NativeError {
    #[error("Invalid native action object at {0}")]
    Invalid(&'static str),
    #[error("Invalid stored object {table}/{id} at {location}")]
    InvalidStored {
        table: &'static str,
        id: String,
        location: String,
    },
    #[error("This action request ID belongs to different input")]
    Conflict,
    #[error("The native action belongs to an earlier account, thread or epoch")]
    Changed,
    #[error("Native action awaits its exact native receipt")]
    Unknown,
    #[error("Not implemented: requires {0}")]
    Unavailable(&'static str),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("Invalid stored native action JSON")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Compact,
    Review,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub request_id: String,
    pub agent: String,
    pub action: Action,
    pub context: Value,
}
impl Request {
    fn signature(&self) -> Result<String> {
        if self.request_id.is_empty() || self.request_id.len() > MAX_ID {
            return Err(NativeError::Invalid("requestId"));
        }
        bounded(&self.context, "context")?;
        let signature = json!({"agent":self.agent,"action":self.action,"context":self.context});
        Ok(python_json_with_separators(&signature, ",", ":"))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionReceipt {
    pub request_id: String,
    pub agent_id: String,
    pub action: Action,
    pub account_key: String,
    pub thread_id: String,
    pub epoch: i64,
    pub attempt_id: String,
    pub accepted_at: f64,
    pub status: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Pending {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Unknown {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Failed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Acknowledged {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
    },
}

/// Checked once from Python-written agent and receipt records, including legacy
/// pending rows. The stored attempt remains the authority for transport scope.
pub struct Snapshot {
    agent: Value,
    receipt: AdmissionReceipt,
    outcome: Outcome,
}
impl Snapshot {
    pub fn from_records(agent: Value, receipt: Value, outcome: Value) -> Result<Self> {
        bounded(&agent, "agent")?;
        checked_agent(&agent)?;
        let receipt: AdmissionReceipt = decode(receipt, "receipt")?;
        let outcome = decode(outcome, "outcome")?;
        if receipt.status != "accepted"
            || !receipt.accepted_at.is_finite()
            || receipt.epoch < 0
            || receipt.request_id.is_empty()
            || receipt.request_id.len() > MAX_ID
            || receipt.attempt_id.is_empty()
            || receipt.thread_id.is_empty()
            || receipt.account_key.is_empty()
            || agent["id"] != receipt.agent_id
        {
            return Err(NativeError::Invalid("receipt.identity"));
        }
        Ok(Self {
            agent,
            receipt,
            outcome,
        })
    }
    pub fn receipt(&self) -> &AdmissionReceipt {
        &self.receipt
    }
    pub fn outcome(&self) -> &Outcome {
        &self.outcome
    }
    fn current(&self) -> Result<()> {
        if self.agent["epoch"] != self.receipt.epoch
            || self.agent.get("accountKey").unwrap_or(&json!("default"))
                != &self.receipt.account_key
            || self.agent["threadId"] != self.receipt.thread_id
            || self.agent["startAttempt"]["id"] != self.receipt.attempt_id
            || self.agent["startAttempt"]["actionRequestId"] != self.receipt.request_id
            || self.agent["startAttempt"]["action"] != json!(self.receipt.action)
            || self.agent["startAttempt"]["actionIdentity"]
                != json!({"accountKey":self.receipt.account_key,"threadId":self.receipt.thread_id,"epoch":self.receipt.epoch})
            || self.agent["autoWake"] != true
            || crate::event_write::truthy(self.agent.get("deletedAt"))
        {
            return Err(NativeError::Changed);
        }
        Ok(())
    }
}

/// The owner checks workspace, budget, pending settings and review capacity in
/// this transaction. No provider, filesystem work or publication is permitted.
pub trait Admission {
    fn check(&self, tx: &Transaction<'_>, agent: &Value, action: Action) -> Result<()>;
}

pub fn reserve(
    tx: &Transaction<'_>,
    request: &Request,
    attempt: &str,
    now: f64,
    gate: &impl Admission,
) -> Result<Snapshot> {
    let signature = request.signature()?;
    if let Some((saved, receipt, outcome)) = row(tx, &request.request_id)? {
        if saved != signature {
            return Err(NativeError::Conflict);
        }
        return Snapshot::from_records(agent(tx, &request.agent)?, receipt, outcome);
    }
    let mut agent = agent(tx, &request.agent)?;
    checked_agent(&agent)?;
    if attempt.is_empty() || attempt.len() > MAX_ID || !now.is_finite() {
        return Err(NativeError::Invalid("attempt"));
    }
    crate::context_repair::assert_available(&agent).map_err(|_| NativeError::Unknown)?;
    let review_worker = request.action == Action::Review
        && agent["nativeReview"]["status"] == "pending"
        && agent["status"] == "queued";
    if agent["autoWake"] != true
        || crate::event_write::truthy(agent.get("inFlight"))
        || crate::event_write::truthy(agent.get("deletedAt"))
        || (!review_worker
            && matches!(
                agent["status"].as_str(),
                Some("queued" | "starting" | "running" | "approval" | "compacting")
            ))
    {
        return Err(NativeError::Invalid("agent.status"));
    }
    if request.action == Action::Review
        && (agent.get("provider").is_some_and(|v| v != "codex")
            || crate::event_write::truthy(agent.get("daybreakEnabled")))
    {
        return Err(NativeError::Invalid("agent.provider"));
    }
    let identity = json!({"accountKey":agent.get("accountKey").unwrap_or(&json!("default")),"threadId":agent["threadId"],"epoch":agent["epoch"]});
    let context = request
        .context
        .as_object()
        .ok_or(NativeError::Invalid("context"))?;
    if context.iter().any(|(k, v)| identity.get(k) != Some(v)) {
        return Err(NativeError::Changed);
    }
    let receipt = json!({"requestId":request.request_id,"agentId":request.agent,"action":request.action,
        "accountKey":identity["accountKey"],"threadId":identity["threadId"],"epoch":identity["epoch"],
        "attemptId":attempt,"acceptedAt":now,"status":"accepted"});
    let checked =
        Snapshot::from_records(agent.clone(), receipt.clone(), json!({"status":"pending"}))?;
    gate.check(tx, &agent, request.action)?;
    agent["status"] = json!("starting");
    agent["inFlight"] = json!(true);
    agent["turnEpoch"] = agent["epoch"].clone();
    agent["startAttempt"] = json!({"id":attempt,"epoch":agent["epoch"],"events":[],"action":request.action,
        "submitted":false,"created":now,"actionRequestId":request.request_id,"actionIdentity":identity});
    if review_worker {
        agent["startAttempt"]["reviewTarget"] = agent["nativeReview"]["target"].clone();
        agent["nativeReview"]["status"] = json!("started");
        agent["nativeReview"]["startedAt"] = json!(now);
    }
    tx.execute("INSERT INTO runtime_native_action_receipts(id,signature,receipt,outcome) VALUES(?1,?2,?3,?4)",
        params![request.request_id,signature,receipt.to_string(),json!({"status":"pending"}).to_string()])?;
    AgentRepository::put(tx, &AgentRecord::from_value(agent.clone())?)?;
    Ok(Snapshot { agent, ..checked })
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NativeRequest {
    pub ticket: ProviderTicket,
    pub method: String,
    pub params: Value,
}
#[derive(Clone, Debug, PartialEq)]
pub enum Delivery {
    Written(Value),
    NotSent,
    Unknown,
}
#[derive(Clone, Debug, PartialEq)]
pub struct TransportReceipt {
    pub ticket: ProviderTicket,
    pub delivery: Delivery,
}
/// One retained account session only. Written contains the native response;
/// transport-write completion without that response must return Unknown.
pub trait RetainedSession: Send + Sync {
    fn request<'a>(
        &'a self,
        request: &'a NativeRequest,
        deadline: tokio::time::Instant,
    ) -> NativeFuture<'a>;
}

/// Called only after ordinary thread/settings preparation. The supplied ticket
/// must come from the retained owner. Committing this transaction is the hand-off
/// boundary: restart may inspect or settle it, but cannot call claim again.
pub fn claim(tx: &Transaction<'_>, id: &str, ticket: &ProviderTicket) -> Result<NativeRequest> {
    let mut snapshot = load(tx, id)?;
    snapshot.current()?;
    if !matches!(snapshot.outcome, Outcome::Pending { .. })
        || snapshot.agent["startAttempt"]["submitted"] == true
    {
        return Err(NativeError::Unknown);
    }
    let r = &snapshot.receipt;
    if ticket.operation != r.attempt_id
        || ticket.agent != r.agent_id
        || ticket.epoch != r.epoch as u64
        || ticket.account != r.account_key
        || ticket.thread.as_deref() != Some(&r.thread_id)
        || ticket.request != r.request_id
        || ticket.connection.is_empty()
        || ticket.generation == 0
    {
        return Err(NativeError::Changed);
    }
    let mut params = json!({"threadId":r.thread_id});
    let method = match r.action {
        Action::Compact => "thread/compact/start",
        Action::Review => {
            params["target"] = snapshot.agent["startAttempt"]
                .get("reviewTarget")
                .cloned()
                .unwrap_or(json!({"type":"uncommittedChanges"}));
            params["delivery"] = json!("inline");
            "review/start"
        }
    };
    let request = NativeRequest {
        ticket: ticket.clone(),
        method: method.into(),
        params,
    };
    snapshot.agent["startAttempt"]["submitted"] = json!(true);
    snapshot.agent["startAttempt"]["accountKey"] = json!(ticket.account);
    snapshot.agent["startAttempt"]["threadId"] = json!(ticket.thread);
    snapshot.agent["startAttempt"]["connectionId"] = json!(ticket.connection);
    snapshot.agent["startAttempt"]["nativeRequest"] = json!(request);
    AgentRepository::put(tx, &AgentRecord::from_value(snapshot.agent)?)?;
    write_outcome(tx, id, &Outcome::Unknown { error: None })?;
    Ok(request)
}

/// Persist result and provider-owned lifecycle effects atomically. The effect
/// writer is the existing account-worker acceptance path, never a second actor.
pub trait Acceptance {
    fn apply(
        &self,
        tx: &Transaction<'_>,
        request: &NativeRequest,
        delivery: &Delivery,
    ) -> Result<()>;
}
pub fn settle(
    tx: &Transaction<'_>,
    id: &str,
    receipt: &TransportReceipt,
    effects: &impl Acceptance,
) -> Result<Outcome> {
    let snapshot = load(tx, id)?;
    snapshot.current()?;
    let expected: NativeRequest = decode(
        snapshot.agent["startAttempt"]["nativeRequest"].clone(),
        "agent.startAttempt.nativeRequest",
    )?;
    if expected.ticket != receipt.ticket {
        return Err(NativeError::Changed);
    }
    let outcome = match &receipt.delivery {
        Delivery::Written(result) => {
            bounded(result, "response")?;
            Outcome::Acknowledged {
                result: Some(result.clone()),
            }
        }
        Delivery::NotSent => Outcome::Failed { error: None },
        Delivery::Unknown => Outcome::Unknown { error: None },
    };
    if snapshot.outcome == outcome {
        return Ok(outcome);
    }
    if !matches!(snapshot.outcome, Outcome::Unknown { .. }) {
        return Err(NativeError::Conflict);
    }
    if !matches!(receipt.delivery, Delivery::Unknown) {
        effects.apply(tx, &expected, &receipt.delivery)?;
    }
    write_outcome(tx, id, &outcome)?;
    Ok(outcome)
}

/// Calls only an already committed intent. On timeout/error/cancellation its
/// stored Unknown remains intact; publication belongs after settlement commit.
pub async fn send(
    session: &impl RetainedSession,
    request: &NativeRequest,
    deadline: tokio::time::Instant,
) -> Result<TransportReceipt> {
    if tokio::time::Instant::now() >= deadline {
        return Ok(TransportReceipt {
            ticket: request.ticket.clone(),
            delivery: Delivery::NotSent,
        });
    }
    match tokio::time::timeout_at(deadline, session.request(request, deadline)).await {
        Ok(Ok(receipt)) if receipt.ticket == request.ticket => Ok(receipt),
        _ => Ok(TransportReceipt {
            ticket: request.ticket.clone(),
            delivery: Delivery::Unknown,
        }),
    }
}

/// Account-owner caller. All SQLite work is off the async executor and a claim
/// commit must succeed before transport hand-off. A crash between these stages
/// leaves Unknown; recovery uses settle with the original exact receipt.
pub async fn execute(
    store: Arc<RuntimeStore>,
    id: String,
    ticket: ProviderTicket,
    session: &impl RetainedSession,
    effects: Arc<impl Acceptance + Send + Sync + 'static>,
    deadline: tokio::time::Instant,
) -> Result<Outcome> {
    let claim_store = store.clone();
    let claim_id = id.clone();
    let request = tokio::task::spawn_blocking(move || {
        claim_store.transaction_with(|tx| claim(tx, &claim_id, &ticket))
    })
    .await
    .map_err(|_| NativeError::Unknown)??;
    let receipt = send(session, &request, deadline).await?;
    tokio::task::spawn_blocking(move || {
        store.transaction_with(|tx| settle(tx, &id, &receipt, effects.as_ref()))
    })
    .await
    .map_err(|_| NativeError::Unknown)?
}

fn agent(tx: &Transaction<'_>, id: &str) -> Result<Value> {
    let value =
        ContextRepairRepository::agent(tx, id)?.ok_or_else(|| NativeError::InvalidStored {
            table: "runtime_agents",
            id: id.to_owned(),
            location: "record missing".into(),
        })?;
    checked_agent(&value).map_err(|error| located(error, "runtime_agents", id))?;
    Ok(value)
}
fn row(tx: &Transaction<'_>, id: &str) -> Result<Option<(String, Value, Value)>> {
    if !crate::sync_policy::table_exists(tx, "runtime_native_action_receipts")? {
        return Err(NativeError::Unavailable(
            "store-bootstrap/native-action-schema",
        ));
    }
    let row = tx
        .query_row(
            "SELECT signature,receipt,outcome FROM runtime_native_action_receipts WHERE id=?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    row.map(|(signature, r, o)| {
        Ok((
            signature,
            serde_json::from_str(&r).map_err(|_| NativeError::InvalidStored {
                table: "runtime_native_action_receipts",
                id: id.to_owned(),
                location: "receipt JSON".into(),
            })?,
            serde_json::from_str(&o).map_err(|_| NativeError::InvalidStored {
                table: "runtime_native_action_receipts",
                id: id.to_owned(),
                location: "outcome JSON".into(),
            })?,
        ))
    })
    .transpose()
}
fn load(tx: &Transaction<'_>, id: &str) -> Result<Snapshot> {
    let (_, receipt, outcome) = row(tx, id)?.ok_or(NativeError::Invalid("receipt.id"))?;
    let agent_id = receipt["agentId"]
        .as_str()
        .ok_or_else(|| NativeError::InvalidStored {
            table: "runtime_native_action_receipts",
            id: id.to_owned(),
            location: "receipt.agentId".into(),
        })?
        .to_owned();
    Snapshot::from_records(agent(tx, &agent_id)?, receipt, outcome)
        .map_err(|error| located(error, "runtime_native_action_receipts", id))
}
fn write_outcome(tx: &Transaction<'_>, id: &str, outcome: &Outcome) -> Result<()> {
    tx.execute(
        "UPDATE runtime_native_action_receipts SET outcome=?1 WHERE id=?2",
        params![
            serde_json::to_string(outcome).map_err(|_| NativeError::Invalid("outcome"))?,
            id
        ],
    )?;
    Ok(())
}
pub(crate) fn bounded(value: &Value, location: &'static str) -> Result<()> {
    if value.to_string().len() > MAX_BYTES {
        return Err(NativeError::Invalid(location));
    }
    Ok(())
}
pub(crate) fn decode<T: serde::de::DeserializeOwned>(
    value: Value,
    location: &'static str,
) -> Result<T> {
    bounded(&value, location)?;
    serde_json::from_value(value).map_err(|_| NativeError::Invalid(location))
}

pub(crate) fn checked_agent(value: &Value) -> Result<AgentRecord> {
    bounded(value, "agent")?;
    crate::context_repair::validate("PersistedAgentRecord", value, "agent").map_err(|_| {
        NativeError::InvalidStored {
            table: "runtime_agents",
            id: value
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("<missing>")
                .to_owned(),
            location: "record".into(),
        }
    })?;
    Ok(AgentRecord::from_value(value.clone())?)
}

fn located(error: NativeError, table: &'static str, id: &str) -> NativeError {
    match error {
        NativeError::Invalid(location) => NativeError::InvalidStored {
            table,
            id: id.to_owned(),
            location: location.to_owned(),
        },
        other => other,
    }
}

/// Native command IDs are scoped to both their task item and owning thread.
/// A process number alone is insufficient after recovery.
pub struct CommandSnapshot {
    task: Value,
    agent: Value,
    process: String,
}
impl CommandSnapshot {
    pub fn from_records(task: Value, agent: Value) -> Result<Self> {
        checked_agent(&agent)?;
        bounded(&task, "command.task")?;
        crate::context_repair::validate("PersistedTaskRecord", &task, "command.task")
            .map_err(|_| NativeError::Invalid("command.task"))?;
        if task["kind"] != "command"
            || task["status"] != "running"
            || task["agent"] != agent["id"]
            || !task["itemId"].is_string()
            || !agent["threadId"].is_string()
        {
            return Err(NativeError::Invalid("command.session"));
        }
        let process = match &task["processId"] {
            Value::String(v) if !v.is_empty() => v.clone(),
            Value::Number(v) if v.as_u64().is_some_and(|v| v > 0) => v.to_string(),
            _ => return Err(NativeError::Invalid("command.processId")),
        };
        Ok(Self {
            task,
            agent,
            process,
        })
    }
    pub fn input(&self, text: &str) -> Result<String> {
        if text.chars().count() > 16_000 || text.contains('\0') {
            return Err(NativeError::Invalid("command.text"));
        }
        Ok(format!(
            "Use your native write_stdin tool for session {}. Send exactly this JSON string as chars: {}.",
            self.process,
            crate::event_write::python_json(&json!(text))
        ))
    }
    pub fn listed(&self, page: &Value) -> Result<bool> {
        bounded(page, "command.page")?;
        let data = page["data"]
            .as_array()
            .filter(|d| d.len() <= 100)
            .ok_or(NativeError::Invalid("command.page.data"))?;
        Ok(data
            .iter()
            .any(|t| t["itemId"] == self.task["itemId"] && t["processId"] == self.process))
    }
    fn current(&self, tx: &Transaction<'_>) -> Result<()> {
        let saved: String = tx.query_row(
            "SELECT record FROM runtime_tasks WHERE id=?1",
            [self.task["id"]
                .as_str()
                .ok_or(NativeError::Invalid("command.id"))?],
            |r| r.get(0),
        )?;
        let task: Value = serde_json::from_str(&saved)?;
        if task != self.task
            || agent(
                tx,
                self.agent["id"]
                    .as_str()
                    .ok_or(NativeError::Invalid("command.agent"))?,
            )? != self.agent
        {
            return Err(NativeError::Changed);
        }
        Ok(())
    }
}

/// Message admission returns the real durable enqueue result in this transaction.
/// The operation key must also identify that enqueue, so response loss is replayable.
pub trait CommandAdmission {
    fn enqueue(
        &self,
        tx: &Transaction<'_>,
        agent: &Value,
        key: &str,
        instruction: &str,
    ) -> Result<Value>;
}
pub fn command_input(
    tx: &Transaction<'_>,
    key: &str,
    snapshot: &CommandSnapshot,
    text: &str,
    admission: &impl CommandAdmission,
) -> Result<Value> {
    use studio_runtime_store::EventWriteRepository;
    if key.is_empty() || key.len() > MAX_ID {
        return Err(NativeError::Invalid("command.requestId"));
    }
    let signature = json!({"task":snapshot.task["id"],"action":"input","text":text}).to_string();
    let id = format!("native-command:{key}");
    if let Some(receipt) = EventWriteRepository::receipt(tx, &id)? {
        if receipt.signature != signature {
            return Err(NativeError::Conflict);
        }
        return Ok(serde_json::from_str(&receipt.result)?);
    }
    snapshot.current(tx)?;
    let result = admission.enqueue(tx, &snapshot.agent, key, &snapshot.input(text)?)?;
    bounded(&result, "command.result")?;
    EventWriteRepository::put_receipt(tx, &id, &signature, &result.to_string())?;
    Ok(result)
}

/// Only the retained owner may supply a matching list response. Persist its
/// exact scope before terminate; the operation receipt never authorizes resend.
pub enum CommandCancelClaim {
    Send(NativeRequest),
    Replay(Value),
}

pub fn command_cancel_claim(
    tx: &Transaction<'_>,
    snapshot: &CommandSnapshot,
    ticket: &ProviderTicket,
    list: &TransportReceipt,
) -> Result<CommandCancelClaim> {
    use studio_runtime_store::EventWriteRepository;
    snapshot.current(tx)?;
    let mut list_ticket = ticket.clone();
    list_ticket.request = format!("{}:list", ticket.request);
    if ticket.agent != snapshot.agent["id"]
        || ticket.thread.as_deref() != snapshot.agent["threadId"].as_str()
        || ticket.epoch
            != snapshot.agent["epoch"]
                .as_u64()
                .ok_or(NativeError::Changed)?
        || ticket.account
            != snapshot
                .agent
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
        || ticket.connection.is_empty()
        || ticket.generation == 0
        || ticket.request.is_empty()
        || ticket.operation != snapshot.task["id"]
        || list.ticket != list_ticket
    {
        return Err(NativeError::Changed);
    }
    let id = format!("native-command:{}", ticket.request);
    let request = NativeRequest {
        ticket: ticket.clone(),
        method: "thread/backgroundTerminals/terminate".into(),
        params: json!({"threadId":ticket.thread,"processId":snapshot.process}),
    };
    let signature = json!({"task":snapshot.task["id"],"action":"cancel"}).to_string();
    if let Some(saved) = EventWriteRepository::receipt(tx, &id)? {
        if saved.signature != signature {
            return Err(NativeError::Conflict);
        }
        let saved_result: Value = serde_json::from_str(&saved.result)?;
        let saved_request: NativeRequest =
            decode(saved_result["request"].clone(), "command.receipt")?;
        if saved_request != request {
            return Err(NativeError::Conflict);
        }
        return match saved_result["status"].as_str() {
            Some("acknowledged") => Ok(CommandCancelClaim::Replay(saved_result["result"].clone())),
            Some("unknown") => Err(NativeError::Unknown),
            _ => Err(NativeError::InvalidStored {
                table: "runtime_operation_receipts",
                id,
                location: "cancel result".into(),
            }),
        };
    }
    let Delivery::Written(page) = &list.delivery else {
        return Err(NativeError::Unknown);
    };
    if !snapshot.listed(page)? {
        return Err(NativeError::Invalid("command.session"));
    }
    EventWriteRepository::put_receipt(
        tx,
        &id,
        &signature,
        &json!({"status":"unknown","request":request}).to_string(),
    )?;
    Ok(CommandCancelClaim::Send(request))
}
pub fn command_cancel_settle(tx: &Transaction<'_>, receipt: &TransportReceipt) -> Result<Value> {
    use studio_runtime_store::EventWriteRepository;
    let id = format!("native-command:{}", receipt.ticket.request);
    let saved = EventWriteRepository::receipt(tx, &id)?.ok_or(NativeError::Changed)?;
    let value: Value = serde_json::from_str(&saved.result)?;
    let request: NativeRequest = decode(value["request"].clone(), "command.receipt")?;
    if request.ticket != receipt.ticket {
        return Err(NativeError::Changed);
    }
    let Delivery::Written(result) = &receipt.delivery else {
        return Err(NativeError::Unknown);
    };
    if result["terminated"] != true {
        return Err(NativeError::Unknown);
    }
    bounded(result, "command.result")?;
    if value["status"] == "acknowledged" {
        return if value["result"] == *result {
            Ok(result.clone())
        } else {
            Err(NativeError::Conflict)
        };
    }
    tx.execute(
        "UPDATE runtime_operation_receipts SET result=?1 WHERE id=?2",
        params![
            json!({"status":"acknowledged","request":request,"result":result}).to_string(),
            id
        ],
    )?;
    Ok(result.clone())
}
