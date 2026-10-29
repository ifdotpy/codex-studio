//! Native maintenance intents. Admission, hand-off and settlement each commit
//! separately; a committed hand-off is never automatically resent after restart.
use crate::account_session_runtime::SessionOutcome as ReceiptOutcome;
use crate::account_session_runtime::{
    AccountSessionError, AccountSessionPort, ProviderRequest, ProviderResponse, RequestCertainty,
    SessionReceipt,
};
use crate::account_worker::retained::RequestActor;
use crate::event_write::settle_request;
use crate::workspace_operations::ProviderTicket;
use rusqlite::Transaction;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};
use studio_runtime_providers::ProviderKind;
use studio_runtime_providers::{Method, RequestId};
use studio_runtime_schema::persistence::{
    NativeSettingsStatus, OutboxKind, OutboxStatus, OwnerKind, RequestIdentity,
};
use studio_runtime_store::outbox::{Outbox, OutboxRow};
use studio_runtime_store::request_results::{
    Admission as RequestAdmission, AdmittedRequest, RequestResults,
};
use studio_runtime_store::{
    AgentRecord, AgentRepository, ContextRepairRepository, RuntimeStore, StoreError,
    python_json_with_separators,
};
use thiserror::Error;

const MAX_ID: usize = 200;
const MAX_BYTES: usize = 1024 * 1024;
const NATIVE_ACTION_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_NOT_SENT_RETRIES: usize = 1;
pub type Result<T> = std::result::Result<T, NativeError>;
pub type NativeFuture<'a> = Pin<Box<dyn Future<Output = Result<TransportReceipt>> + Send + 'a>>;
pub type PreparationFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

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
    #[error("Request id expired")]
    Expired,
    #[error("Not implemented: requires {0}")]
    Unavailable(&'static str),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("Invalid stored native action JSON")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

impl From<studio_runtime_store::request_results::RequestResultError> for NativeError {
    fn from(error: studio_runtime_store::request_results::RequestResultError) -> Self {
        use studio_runtime_store::request_results::RequestResultError;
        match error {
            RequestResultError::Conflict { .. } => Self::Conflict,
            error @ (RequestResultError::Expired
            | RequestResultError::Sql(_)
            | RequestResultError::Json(_)
            | RequestResultError::Identity(_)) => Self::Store(error.into()),
        }
    }
}
impl From<studio_runtime_schema::persistence::RequestIdentityError> for NativeError {
    fn from(error: studio_runtime_schema::persistence::RequestIdentityError) -> Self {
        Self::Store(StoreError::RequestResult(error.into()))
    }
}
impl From<studio_runtime_store::outbox::OutboxError> for NativeError {
    fn from(error: studio_runtime_store::outbox::OutboxError) -> Self {
        use studio_runtime_store::outbox::OutboxError;
        match error {
            OutboxError::Conflict(_) => Self::Conflict,
            error @ (OutboxError::Sql(_) | OutboxError::Json(_)) => Self::Store(error.into()),
        }
    }
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
    pub status: studio_runtime_schema::persistence::NativeActionAdmissionStatus,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
struct NativeActionIntent {
    admitted: AdmittedRequest,
    receipt: AdmissionReceipt,
    outcome: Outcome,
    idle: Option<IdleActionProvenance>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IdleActionProvenance {
    pub episode_id: String,
    pub before: IdleActionBefore,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdleActionBefore {
    pub status: studio_runtime_store::AgentStatus,
    pub start_attempt: Value,
    pub turn_epoch: Option<i64>,
}

pub fn idle_provenance(
    connection: &rusqlite::Connection,
    request_id: &str,
) -> Result<Option<IdleActionProvenance>> {
    Ok(
        Outbox::get::<NativeActionIntent>(connection, &action_outbox_id(request_id))?
            .and_then(|row| row.payload.idle),
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CommandCancelIntent {
    admitted: AdmittedRequest,
    request: NativeRequest,
}

#[derive(Serialize, Deserialize)]
struct CommandCancelReply {
    request: NativeRequest,
    result: Value,
}

fn action_outbox_id(root: &str) -> String {
    json!(["native-action", root]).to_string()
}
fn command_outbox_id(root: &str) -> String {
    json!(["native-command", format!("native-command:{root}")]).to_string()
}

fn request_identity(namespace: &str, root: &str) -> Result<RequestIdentity> {
    Ok(RequestIdentity::new(namespace, &[], root, &[])?)
}

fn admit(
    tx: &Transaction<'_>,
    namespace: &str,
    root: &str,
    signature: &str,
    now_ms: i64,
) -> Result<AdmittedRequest> {
    match RequestResults::admit(tx, &request_identity(namespace, root)?, signature, now_ms)? {
        RequestAdmission::Fresh(admitted) => Ok(admitted),
        RequestAdmission::Replay(_) => Err(NativeError::Conflict),
    }
}

fn saved_reply<R: serde::de::DeserializeOwned>(
    connection: &rusqlite::Connection,
    namespace: &str,
    root: &str,
) -> Result<Option<(String, R)>> {
    let raw = if namespace == "native-command" {
        format!("native-command:{root}")
    } else {
        root.to_owned()
    };
    Ok(RequestResults::read(
        connection,
        &raw,
        namespace,
        &[],
        root,
        &[],
        chrono::Utc::now().timestamp_millis(),
    )?
    .map(|saved| (saved.signature, saved.reply)))
}

#[derive(Serialize, Deserialize)]
struct NativeActionReply {
    receipt: AdmissionReceipt,
    outcome: Outcome,
    replayed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
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
        Self::checked(
            agent,
            decode(receipt, "receipt")?,
            decode(outcome, "outcome")?,
        )
    }
    fn checked(agent: Value, receipt: AdmissionReceipt, outcome: Outcome) -> Result<Self> {
        bounded(&agent, "agent")?;
        checked_agent(&agent)?;
        if !receipt.accepted_at.is_finite()
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
pub trait Admission: Send + Sync + 'static {
    fn check(&self, tx: &Transaction<'_>, agent: &Value, action: Action) -> Result<()>;
    fn idle_episode_id(&self) -> Option<&str> {
        None
    }
}

pub fn reserve(
    tx: &Transaction<'_>,
    request: &Request,
    attempt: &str,
    now: f64,
    gate: &dyn Admission,
) -> Result<Snapshot> {
    let signature = request.signature()?;
    if let Some((saved, receipt, outcome)) = row(tx, &request.request_id)? {
        if saved != signature {
            return Err(NativeError::Conflict);
        }
        return Snapshot::checked(agent(tx, &request.agent)?, receipt, outcome);
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
        && (agent
            .get("provider")
            .is_some_and(|v| v != ProviderKind::Codex.key())
            || crate::event_write::truthy(agent.get("daybreakEnabled")))
    {
        return Err(NativeError::Invalid("agent.provider"));
    }
    let identity = json!({"accountKey":agent.get("accountKey").unwrap_or(&json!("default")),"threadId":agent["threadId"],"epoch":agent["epoch"]});
    let context = request
        .context
        .as_object()
        .ok_or(NativeError::Invalid("context"))?;
    let matches_identity = |expected: &Value| {
        context.iter().all(|(key, value)| {
            expected.get(key).is_some_and(|expected| {
                expected == value
                    || (key == "epoch"
                        && expected.as_i64().is_some_and(|expected| {
                            value.as_f64().is_some_and(|value| {
                                value.is_finite()
                                    && value.fract() == 0.0
                                    && value as i128 == i128::from(expected)
                                    && (value as i128) as f64 == value
                            })
                        }))
            })
        })
    };
    let current_matches = matches_identity(&identity);
    let repair = agent.get("contextRepair").unwrap_or(&Value::Null);
    let source = &repair["source"];
    let repaired_destination = repair["phase"] == "completed"
        && repair["agent"] == request.agent
        && source["id"] == request.agent
        && source["accountKey"] == identity["accountKey"]
        && source["epoch"] == identity["epoch"]
        && source["threadId"].is_string()
        && source["threadId"] != identity["threadId"]
        && repair["newThreadId"] == identity["threadId"]
        && matches_identity(source);
    if !current_matches && !repaired_destination {
        return Err(NativeError::Changed);
    }
    let receipt = AdmissionReceipt {
        request_id: request.request_id.clone(),
        agent_id: request.agent.clone(),
        action: request.action,
        account_key: identity["accountKey"]
            .as_str()
            .ok_or(NativeError::Invalid("accountKey"))?
            .into(),
        thread_id: identity["threadId"]
            .as_str()
            .ok_or(NativeError::Invalid("threadId"))?
            .into(),
        epoch: identity["epoch"]
            .as_i64()
            .ok_or(NativeError::Invalid("epoch"))?,
        attempt_id: attempt.into(),
        accepted_at: now,
        status: studio_runtime_schema::persistence::NativeActionAdmissionStatus::Accepted,
    };
    let checked = Snapshot::checked(agent.clone(), receipt, Outcome::Pending { error: None })?;
    let account = agent
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    if let Some(pending) = agent.get("pendingSettings") {
        let pending_settings = pending
            .as_object()
            .map(|pending| {
                pending
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect::<Vec<_>>()
            })
            .ok_or(NativeError::Invalid("agent.pendingSettings"))?;
        if !pending.is_object()
            || agent
                .get("pendingSettingsAccountKey")
                .and_then(Value::as_str)
                .unwrap_or(account)
                != account
        {
            return Err(NativeError::Invalid("agent.pendingSettingsAccountKey"));
        }
        for (key, value) in pending_settings {
            agent[key] = value;
        }
        if let Some(record) = agent.as_object_mut() {
            record.remove("pendingSettings");
            record.remove("pendingSettingsAccountKey");
        }
    }
    let idle = gate
        .idle_episode_id()
        .map(|episode_id| -> Result<IdleActionProvenance> {
            Ok(IdleActionProvenance {
                episode_id: episode_id.to_owned(),
                before: IdleActionBefore {
                    status: decode(agent["status"].clone(), "idle.before.status")?,
                    start_attempt: agent["startAttempt"].clone(),
                    turn_epoch: decode(agent["turnEpoch"].clone(), "idle.before.turnEpoch")?,
                },
            })
        })
        .transpose()?;
    gate.check(tx, &agent, request.action)?;
    if let Some(episode) = AgentRepository::get(
        tx,
        &studio_runtime_store::AgentId::from(request.agent.as_str()),
    )?
    .and_then(|current| current.extra.get("idleCompactionEpisode").cloned())
    {
        agent["idleCompactionEpisode"] = episode;
    }
    agent["status"] = json!("starting");
    agent["inFlight"] = json!(true);
    agent["turnEpoch"] = agent["epoch"].clone();
    agent["startAttempt"] = json!({"id":attempt,"epoch":agent["epoch"],"events":[],"action":request.action,
        "submitted":false,"created":now,"actionRequestId":request.request_id,"rootRequestId":request.request_id,"actionIdentity":identity});
    if review_worker {
        // Python `codex_agent_review.claim` saves the account on the attempt.
        agent["startAttempt"]["accountKey"] = identity["accountKey"].clone();
        agent["startAttempt"]["reviewTarget"] = agent["nativeReview"]["target"].clone();
        agent["nativeReview"]["status"] = json!("started");
        agent["nativeReview"]["startedAt"] = json!(now);
    }
    let now_ms = (now * 1000.0) as i64;
    let admitted = admit(
        tx,
        "native-action",
        &request.request_id,
        &signature,
        chrono::Utc::now().timestamp_millis(),
    )?;
    Outbox::put(
        tx,
        &OutboxRow {
            id: action_outbox_id(&request.request_id),
            kind: OutboxKind::NativeAction,
            owner_kind: OwnerKind::Agent,
            owner_id: request.agent.clone(),
            request_id: Some(admitted.id().into()),
            signature,
            status: OutboxStatus::Pending,
            account: Some(checked.receipt.account_key.clone()),
            stream_id: None,
            generation: None,
            ordinal: None,
            next_at: Some(now_ms),
            created: admitted.created(),
            payload: NativeActionIntent {
                admitted,
                receipt: checked.receipt.clone(),
                outcome: checked.outcome.clone(),
                idle,
            },
        },
    )?;
    AgentRepository::put(tx, &AgentRecord::from_value(agent.clone())?)?;
    Ok(Snapshot { agent, ..checked })
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NativeRequest {
    pub root_request_id: String,
    pub admitted_request: Option<AdmittedRequest>,
    pub ticket: ProviderTicket,
    pub method: String,
    pub params: Value,
}
/// Reserve the exact provider obligation in the transaction changing its owner.
/// A saved final token is reused without admitting another provider attempt.
pub fn reserve_provider_intent(
    tx: &Transaction<'_>,
    owner: &AdmittedRequest,
    provider_operation: &str,
    mut request: NativeRequest,
) -> Result<NativeRequest> {
    if request.admitted_request.is_none() {
        let method = Method::from_wire(&request.method).ok_or(NativeError::Invalid("method"))?;
        let token = crate::account_worker::retained::reserve_provider_call(
            tx,
            owner,
            crate::account_worker::retained::ProviderCallIntent {
                root_request_id: request.root_request_id.clone(),
                admitted_request: None,
                scope: crate::account_session_runtime::ConnectionScope {
                    account: request.ticket.account.clone(),
                    connection: request.ticket.connection.clone(),
                    generation: request.ticket.generation,
                },
                operation: provider_operation.to_owned(),
                actor: Some(RequestActor {
                    agent: request.ticket.agent.clone(),
                    epoch: i64::try_from(request.ticket.epoch).map_err(|_| NativeError::Changed)?,
                }),
                transfer: None,
                deleted_actor_session: None,
                method,
                params: request.params.clone(),
            },
        )
        .map_err(|error| {
            if let crate::account_worker::WorkerError::Store(error) = error {
                NativeError::Store(error)
            } else {
                NativeError::Unknown
            }
        })?;
        request.admitted_request = Some(token);
    }
    Ok(request)
}

#[derive(Clone, Debug, PartialEq)]
pub enum Delivery {
    Written(Value),
    Rejected(Value),
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
    let intent = Outbox::get::<NativeActionIntent>(tx, &action_outbox_id(id))?
        .ok_or(NativeError::Changed)?
        .payload;
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
    if let Some(idle) = &intent.idle
        && let Some(reason) =
            crate::idle_compaction::handoff_refusal(tx, &snapshot.agent, &idle.episode_id)?
    {
        eprintln!(
            "idle_compaction_refused request={} reason={}",
            r.request_id,
            reason.as_str()
        );
        return Err(NativeError::Invalid(reason.as_str()));
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
    if intent.idle.is_some() {
        params["requestId"] = json!(r.request_id);
        params["idleCompaction"] = snapshot.agent["idleCompactionEpisode"]["cache"].clone();
    }
    let request = NativeRequest {
        root_request_id: snapshot.agent["startAttempt"]["rootRequestId"]
            .as_str()
            .ok_or(NativeError::Invalid("startAttempt.rootRequestId"))?
            .to_owned(),
        admitted_request: snapshot.agent["startAttempt"]
            .get("providerRequest")
            .cloned()
            .map(|value| decode(value, "startAttempt.providerRequest"))
            .transpose()?,
        ticket: ticket.clone(),
        method: method.into(),
        params,
    };
    let owner = intent.admitted;
    let request = reserve_provider_intent(tx, &owner, &ticket.operation, request)?;
    snapshot.agent["startAttempt"]["providerRequest"] = json!(request.admitted_request);
    snapshot.agent["startAttempt"]["submitted"] = json!(true);
    snapshot.agent["startAttempt"]["accountKey"] = json!(ticket.account);
    snapshot.agent["startAttempt"]["threadId"] = json!(ticket.thread);
    snapshot.agent["startAttempt"]["connectionId"] = json!(ticket.connection);
    snapshot.agent["startAttempt"]["connectionGeneration"] = json!(ticket.generation);
    snapshot.agent["startAttempt"]["requestTicket"] = json!(format!(
        "retained:{}:{}",
        ticket.account,
        studio_runtime_store::sha256_hex(ticket.operation.as_bytes())
    ));
    snapshot.agent["startAttempt"]["nativeRequest"] = json!(request);
    AgentRepository::put(tx, &AgentRecord::from_value(snapshot.agent)?)?;
    write_outcome(tx, id, &Outcome::Unknown { error: None })?;
    Ok(request)
}

/// Persist result and provider-owned lifecycle effects atomically. The effect
/// writer is the existing account-worker acceptance path, never a second actor.
pub trait Acceptance: Send + Sync + 'static {
    fn apply(
        &self,
        tx: &Transaction<'_>,
        request: &NativeRequest,
        delivery: &Delivery,
    ) -> Result<()>;
}
/// Runtime preparation and settings updates run after admission commits and
/// before the compact/review hand-off is claimed. Implementations may perform
/// filesystem work and their own separately receipted provider request here.
pub trait Preparation {
    /// Cross-owner repair must finish before the action receipt captures its
    /// account/thread identity. The default is suitable for already prepared
    /// native callers; HTTP adapters may repair through a durable private port.
    fn before_reserve<'a>(
        &'a self,
        _store: &'a RuntimeStore,
        _provider: &'a dyn AccountSessionPort,
        _request: &'a Request,
        _scope: &'a crate::account_session_runtime::ConnectionScope,
    ) -> PreparationFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn prepare<'a>(
        &'a self,
        store: &'a RuntimeStore,
        provider: &'a dyn AccountSessionPort,
        request: &'a Request,
        ticket: &'a ProviderTicket,
        scope: &'a crate::account_session_runtime::ConnectionScope,
    ) -> PreparationFuture<'a>;
}

struct NoopPreparation;
impl Preparation for NoopPreparation {
    fn prepare<'a>(
        &'a self,
        _store: &'a RuntimeStore,
        _provider: &'a dyn AccountSessionPort,
        _request: &'a Request,
        _ticket: &'a ProviderTicket,
        _scope: &'a crate::account_session_runtime::ConnectionScope,
    ) -> PreparationFuture<'a> {
        Box::pin(async { Ok(()) })
    }
}
pub fn settle(
    tx: &Transaction<'_>,
    id: &str,
    receipt: &TransportReceipt,
    effects: &dyn Acceptance,
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
        Delivery::Rejected(result) => {
            bounded(result, "response")?;
            Outcome::Failed { error: None }
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
    if matches!(receipt.delivery, Delivery::NotSent) {
        // Acceptance may release the in-flight owner in this same transaction.
        // Reload after applying it so the retry marker does not overwrite that update.
        let mut agent = agent(tx, &snapshot.receipt.agent_id)?;
        agent["startAttempt"]["submitted"] = json!(false);
        agent["startAttempt"]
            .as_object_mut()
            .map(|attempt| attempt.remove("nativeRequest"));
        AgentRepository::put(tx, &AgentRecord::from_value(agent)?)?;
        write_outcome(tx, id, &Outcome::Pending { error: None })?;
        return Ok(Outcome::Pending { error: None });
    }
    write_outcome(tx, id, &outcome)?;
    Ok(outcome)
}

fn validate_provider_receipt(expected: &NativeRequest, receipt: &SessionReceipt) -> Result<()> {
    let ticket = &expected.ticket;
    if receipt.scope.account != ticket.account
        || receipt.scope.connection != ticket.connection
        || receipt.scope.generation != ticket.generation
        || receipt.operation != ticket.operation
    {
        return Err(NativeError::Changed);
    }
    let method = Method::from_wire(&expected.method).ok_or(NativeError::Invalid("method"))?;
    if let Some(provider_ticket) = receipt.ticket() {
        let expected_id = format!(
            "retained:{}:{}",
            ticket.account,
            studio_runtime_store::sha256_hex(ticket.operation.as_bytes())
        );
        if provider_ticket.generation != ticket.generation
            || provider_ticket.method != method
            || provider_ticket.id != RequestId::String(expected_id)
        {
            return Err(NativeError::Changed);
        }
    }
    Ok(())
}

/// Execute one admitted action through the retained account broker. Every
/// provider call follows a committed claim and is bound to the scope observed
/// before that claim; only a broker-proven NotSent can claim again.
pub async fn execute_provider(
    store: &RuntimeStore,
    port: &dyn AccountSessionPort,
    request: &Request,
    now: f64,
    gate: Arc<dyn Admission>,
    effects: Arc<dyn Acceptance>,
) -> Result<Value> {
    execute_provider_prepared(store, port, request, now, gate, effects, &NoopPreparation).await
}

pub async fn execute_provider_prepared(
    store: &RuntimeStore,
    port: &dyn AccountSessionPort,
    request: &Request,
    now: f64,
    gate: Arc<dyn Admission>,
    effects: Arc<dyn Acceptance>,
    preparation: &impl Preparation,
) -> Result<Value> {
    let replay_request = request.clone();
    let final_reply = store
        .read_async("runtime-services/native_actions", move |tx| {
            if let Some((signature, reply)) =
                saved_reply::<Value>(tx, "native-action", &replay_request.request_id)?
            {
                if signature != replay_request.signature()? {
                    return Err(NativeError::Conflict);
                }
                return Ok(Some(reply));
            }
            Ok(None)
        })
        .await?;
    if let Some(reply) = final_reply {
        return Ok(reply);
    }
    let replay_request = request.clone();
    let existing = store
        .read_async("runtime-services/native_actions", move |tx| {
            replay(tx, &replay_request)
        })
        .await?;
    if let Some(snapshot) = existing.as_ref()
        && (!matches!(snapshot.outcome, Outcome::Pending { .. })
            || snapshot.agent["startAttempt"]["submitted"] == true)
    {
        return Ok(response(snapshot, true));
    }
    let resume_reserved = existing.is_some();
    let agent_id = request.agent.clone();
    let agent_snapshot = store
        .read_async("runtime-services/native_actions", move |tx| {
            agent(tx, &agent_id)
        })
        .await?;
    let account = agent_snapshot
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_owned();
    let attempt = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("native-action:{}", request.request_id).as_bytes(),
    )
    .to_string();
    for retry in 0..=MAX_NOT_SENT_RETRIES {
        let scope = match port.current_scope(&account) {
            Some(scope) => scope,
            // The account worker exits after its idle deadline. Start it again, as the
            // other account-scoped routes do, instead of refusing the action.
            None => port.ensure_session(&account).await.map_err(|error| {
                if matches!(error, AccountSessionError::Expired) {
                    NativeError::Expired
                } else {
                    NativeError::Unavailable("provider-session/native-request")
                }
            })?,
        };
        if scope.account != account {
            return Err(NativeError::Changed);
        }
        if !resume_reserved {
            preparation
                .before_reserve(store, port, request, &scope)
                .await?;
        }
        if port.current_scope(&account).as_ref() != Some(&scope) {
            return Err(NativeError::Changed);
        }
        let claim_store = Arc::clone(&gate);
        let action = request.clone();
        let attempt_id = attempt.clone();
        let scope_for_claim = scope.clone();
        let reserved = store
            .transaction_with(move |tx| {
                let snapshot = reserve(tx, &action, &attempt_id, now, claim_store.as_ref())?;
                let receipt = snapshot.receipt();
                if receipt.account_key != scope_for_claim.account {
                    return Err(NativeError::Changed);
                }
                let ticket = ProviderTicket {
                    operation: receipt.attempt_id.clone(),
                    agent: receipt.agent_id.clone(),
                    epoch: u64::try_from(receipt.epoch).map_err(|_| NativeError::Changed)?,
                    account: receipt.account_key.clone(),
                    thread: Some(receipt.thread_id.clone()),
                    connection: scope_for_claim.connection.clone(),
                    generation: scope_for_claim.generation,
                    request: receipt.request_id.clone(),
                };
                Ok((receipt.clone(), ticket))
            })
            .await?;
        let (receipt, ticket) = reserved;
        preparation
            .prepare(store, port, request, &ticket, &scope)
            .await?;
        if port.current_scope(&account).as_ref() != Some(&scope) {
            return Err(NativeError::Changed);
        }
        let claim_id = receipt.request_id.clone();
        let claim_ticket = ticket.clone();
        let native_request = store
            .transaction_with(move |tx| claim(tx, &claim_id, &claim_ticket))
            .await?;
        let method =
            Method::from_wire(&native_request.method).ok_or(NativeError::Invalid("method"))?;
        let provider = port
            .provider_request(ProviderRequest {
                root_request_id: native_request.root_request_id.clone(),
                admitted_request: native_request.admitted_request.clone(),
                account: account.clone(),
                operation: ticket.operation.clone(),
                actor: Some(RequestActor {
                    agent: ticket.agent.clone(),
                    epoch: i64::try_from(ticket.epoch).map_err(|_| NativeError::Changed)?,
                }),
                deleted_actor_session: None,
                expected_scope: Some(scope.clone()),
                method,
                params: native_request.params.clone(),
                deadline: Instant::now() + NATIVE_ACTION_TIMEOUT,
            })
            .await;
        let retryable = provider
            .as_ref()
            .is_ok_and(|receipt| receipt.certainty() == RequestCertainty::NotSent);
        let id = request.request_id.clone();
        let settle_effects = Arc::clone(&effects);
        let settled = store
            .transaction_with(move |tx| {
                settle_provider(tx, &id, ticket, provider, settle_effects.as_ref())
            })
            .await?;
        let id = request.request_id.clone();
        let snapshot = store
            .read_async("runtime-services/native_actions", move |tx| load(tx, &id))
            .await?;
        if retryable && retry < MAX_NOT_SENT_RETRIES {
            continue;
        }
        let mut value = response(&snapshot, false);
        value["outcome"] =
            serde_json::to_value(settled).map_err(|_| NativeError::Invalid("outcome"))?;
        return Ok(value);
    }
    Err(NativeError::Unknown)
}

/// Return a previously settled action receipt before an HTTP caller applies
/// readiness preflights that may no longer hold after the original action.
pub fn replay_if_settled(tx: &Transaction<'_>, request: &Request) -> Result<Option<Value>> {
    if let Some((signature, reply)) =
        saved_reply::<Value>(tx, "native-action", &request.request_id)?
    {
        if signature != request.signature()? {
            return Err(NativeError::Conflict);
        }
        return Ok(Some(reply));
    }
    let existing = replay(tx, request)?;
    Ok(existing
        .as_ref()
        .filter(|snapshot| {
            !matches!(snapshot.outcome, Outcome::Pending { .. })
                || snapshot.agent["startAttempt"]["submitted"] == true
        })
        .map(|snapshot| response(snapshot, true)))
}

/// The saved outcome of one admitted native action request.
pub fn outcome(tx: &Transaction<'_>, id: &str) -> Result<Option<Outcome>> {
    Ok(row(tx, id)?.map(|(_, _, outcome)| outcome))
}

/// Settle an admitted request that was never handed to the provider as a
/// definite failure. A claimed (submitted) request keeps its uncertainty.
pub fn fail_pending(tx: &Transaction<'_>, id: &str, error: &str) -> Result<()> {
    let snapshot = load(tx, id)?;
    if !matches!(snapshot.outcome, Outcome::Pending { .. })
        || snapshot.agent["startAttempt"]["submitted"] == true
    {
        return Err(NativeError::Conflict);
    }
    write_outcome(
        tx,
        id,
        &Outcome::Failed {
            error: Some(error.to_owned()),
        },
    )
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod native_action_retry_tests {
    #[test]
    fn receipt_pivot_reads_completed_native_outcome_without_receipt_table() {
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        studio_runtime_store::receipt_schema::create(&db).expect("target schema");
        db.execute_batch(
            "CREATE TABLE runtime_orchestration_settings(id TEXT PRIMARY KEY,record TEXT NOT NULL)",
        )
        .expect("policy");
        let reply = serde_json::json!({"receipt":{"requestId":"old-action","agentId":"agent","action":"compact","accountKey":"account","threadId":"thread","epoch":1,"attemptId":"attempt","acceptedAt":1.0,"status":"accepted"},"outcome":{"status":"acknowledged","result":{"turnId":"original"}},"replayed":false,"result":{"turnId":"original"}});
        let envelope = studio_runtime_store::request_results::ResultEnvelope {
            actor: Some("agent".to_owned()),
            reply,
            references: Vec::<String>::new(),
        };
        db.execute(
            "INSERT INTO request_results VALUES('old-action','signature',?1,?2)",
            rusqlite::params![
                serde_json::to_string(&envelope).expect("reply"),
                chrono::Utc::now().timestamp_millis()
            ],
        )
        .expect("migrated exact response");
        let tx = db.transaction().expect("transaction");
        assert!(
            matches!(super::outcome(&tx, "old-action").expect("new owner"), Some(super::Outcome::Acknowledged { result: Some(ref result) }) if result["turnId"] == "original")
        );
    }
    use super::{NativeRequest, SessionReceipt, validate_provider_receipt};
    use crate::account_session_runtime::SessionOutcome as ReceiptOutcome;
    use crate::{account_session_runtime::ConnectionScope, workspace_operations::ProviderTicket};
    use serde_json::json;
    use studio_runtime_providers::{Method, RequestId, RequestTicket};

    /// The store owns the attempt action names that may have no input events.
    #[test]
    fn action_wire_names_match_the_store_native_action_owner() {
        let names: Vec<_> = [super::Action::Compact, super::Action::Review]
            .into_iter()
            .map(|action| serde_json::to_value(action).expect("action wire name"))
            .collect();
        assert_eq!(
            names,
            studio_runtime_store::NATIVE_ACTION_ATTEMPT_KINDS
                .iter()
                .map(|name| json!(name))
                .collect::<Vec<_>>()
        );
    }

    fn worker_not_sent_receipt(ticket: RequestTicket) -> SessionReceipt {
        SessionReceipt {
            scope: ConnectionScope {
                account: "default".into(),
                connection: "connection".into(),
                generation: 4,
            },
            operation: "attempt".into(),
            outcome: ReceiptOutcome::WorkerNotSent { ticket },
            replayed: false,
        }
    }

    #[test]
    fn correlated_worker_not_sent_receipt_is_validated_for_retry() {
        let request = NativeRequest {
            root_request_id: studio_runtime_schema::persistence::RequestIdentity::issue_root(),
            admitted_request: None,
            ticket: ProviderTicket {
                operation: "attempt".into(),
                agent: "agent".into(),
                epoch: 3,
                account: "default".into(),
                thread: Some("thread".into()),
                connection: "connection".into(),
                generation: 4,
                request: "request".into(),
            },
            method: "thread/compact/start".into(),
            params: json!({"threadId":"thread"}),
        };
        let matching_ticket = RequestTicket {
            id: RequestId::String(format!(
                "retained:default:{}",
                studio_runtime_store::sha256_hex(b"attempt")
            )),
            generation: 4,
            method: Method::from_wire("thread/compact/start").expect("known method"),
        };
        assert!(
            validate_provider_receipt(&request, &worker_not_sent_receipt(matching_ticket)).is_ok()
        );
        let foreign_ticket = RequestTicket {
            id: RequestId::String("foreign-ticket".into()),
            generation: 4,
            method: Method::from_wire("thread/compact/start").expect("known method"),
        };
        assert!(matches!(
            validate_provider_receipt(&request, &worker_not_sent_receipt(foreign_ticket)),
            Err(super::NativeError::Changed)
        ));
    }
}

/// Validate and settle the exact receipt returned by the retained account broker.
pub fn settle_provider(
    tx: &Transaction<'_>,
    id: &str,
    ticket: ProviderTicket,
    provider: std::result::Result<SessionReceipt, AccountSessionError>,
    effects: &dyn Acceptance,
) -> Result<Outcome> {
    let delivery = match &provider {
        Ok(receipt) => {
            let expected = NativeRequest {
                root_request_id: ticket.request.clone(),
                admitted_request: None,
                ticket: ticket.clone(),
                method: match load(tx, id)?.receipt.action {
                    Action::Compact => "thread/compact/start".into(),
                    Action::Review => "review/start".into(),
                },
                params: Value::Null,
            };
            validate_provider_receipt(&expected, receipt)?;
            match &receipt.outcome {
                ReceiptOutcome::Written {
                    response: ProviderResponse::Reply(value),
                    ..
                } => Delivery::Written(value.clone()),
                ReceiptOutcome::Written {
                    response: ProviderResponse::Rejected(value),
                    ..
                } => Delivery::Rejected(value.clone()),
                ReceiptOutcome::WorkerNotSent { .. }
                | ReceiptOutcome::NoLiveAccountWorker
                | ReceiptOutcome::ExpectedScopeMismatch
                | ReceiptOutcome::DeadlineExpired
                | ReceiptOutcome::NotHandedOff => Delivery::NotSent,
                ReceiptOutcome::Unknown { .. } => Delivery::Unknown,
            }
        }
        Err(AccountSessionError::Expired) => return Err(NativeError::Expired),
        Err(_) => Delivery::Unknown,
    };
    settle(tx, id, &TransportReceipt { ticket, delivery }, effects)
}

/// Apply native-action model settings through a separately persisted provider
/// intent. A restart can retry only an exact broker receipt proving NotSent.
pub async fn update_action_settings(
    store: &RuntimeStore,
    port: &dyn AccountSessionPort,
    request: &Request,
    ticket: &ProviderTicket,
    scope: &crate::account_session_runtime::ConnectionScope,
    params: Value,
) -> Result<()> {
    let operation = format!("{}:settings", ticket.operation);
    let settings_ticket = ProviderTicket {
        operation: operation.clone(),
        ..ticket.clone()
    };
    for retry in 0..=MAX_NOT_SENT_RETRIES {
        let action_id = request.request_id.clone();
        let request_agent = request.agent.clone();
        let request_id = request.request_id.clone();
        let ticket_for_tx = ticket.clone();
        let operation_for_tx = operation.clone();
        let request_id_for_record = action_id.clone();
        let scope_snapshot = scope.clone();
        let params_snapshot = params.clone();
        let should_send = store
            .transaction_with(move |tx| {
                let mut agent = agent(tx, &request_agent)?;
                if agent["startAttempt"]["id"] != ticket_for_tx.operation
                    || agent["startAttempt"]["actionRequestId"] != request_id
                    || agent["epoch"].as_u64() != Some(ticket_for_tx.epoch)
                    || agent
                        .get("accountKey")
                        .and_then(Value::as_str)
                        .unwrap_or("default")
                        != ticket_for_tx.account
                    || agent["threadId"].as_str() != ticket_for_tx.thread.as_deref()
                {
                    return Err(NativeError::Changed);
                }
                let saved = agent["startAttempt"].get("modelSettings").cloned();
                let root_request_id = saved
                    .as_ref()
                    .and_then(|saved| saved.get("rootRequestId"))
                    .or_else(|| agent["startAttempt"].get("rootRequestId"))
                    .and_then(Value::as_str)
                    .ok_or(NativeError::Invalid("modelSettings.rootRequestId"))?
                    .to_owned();
                let final_token = saved
                    .as_ref()
                    .and_then(|saved| saved.get("providerRequest"))
                    .cloned()
                    .map(|value| decode(value, "modelSettings.providerRequest"))
                    .transpose()?;
                if let Some(saved) = saved {
                    if saved["operation"] != operation_for_tx
                        || saved["requestId"] != request_id_for_record
                        || saved["scope"] != json!(scope_snapshot)
                        || saved["params"] != params_snapshot
                    {
                        return Err(NativeError::Conflict);
                    }
                    let status: NativeSettingsStatus =
                        decode(saved["status"].clone(), "modelSettings.status")?;
                    match status {
                        NativeSettingsStatus::Acknowledged => return Ok(None),
                        NativeSettingsStatus::NotSent => (),
                        NativeSettingsStatus::Submitted | NativeSettingsStatus::Unknown => {
                            return Err(NativeError::Unknown);
                        }
                        NativeSettingsStatus::Rejected => {
                            return Err(NativeError::Invalid("settings/rejected"));
                        }
                    }
                }
                let owner = Outbox::get::<NativeActionIntent>(tx, &action_outbox_id(&request_id))?
                    .ok_or(NativeError::Changed)?
                    .payload
                    .admitted;
                let prepared = reserve_provider_intent(
                    tx,
                    &owner,
                    &operation_for_tx,
                    NativeRequest {
                        root_request_id: root_request_id.clone(),
                        admitted_request: final_token,
                        ticket: ProviderTicket {
                            operation: operation_for_tx.clone(),
                            ..ticket_for_tx.clone()
                        },
                        method: "thread/settings/update".into(),
                        params: params_snapshot.clone(),
                    },
                )?;
                agent["startAttempt"]["modelSettings"] = json!({
                    "operation": operation_for_tx,
                    "requestId": request_id_for_record,
                    "scope": scope_snapshot,
                    "method": "thread/settings/update",
                    "params": params_snapshot,
                    "status": NativeSettingsStatus::Submitted,
                    "providerRequest":prepared.admitted_request,
                    "rootRequestId":root_request_id,
                });
                AgentRepository::put(tx, &AgentRecord::from_value(agent)?)?;
                Ok(Some(prepared))
            })
            .await?;
        let Some(settings_request) = should_send else {
            return Ok(());
        };

        let method =
            Method::from_wire("thread/settings/update").ok_or(NativeError::Invalid("method"))?;
        let provider = port
            .provider_request(ProviderRequest {
                root_request_id: settings_request.root_request_id.clone(),
                admitted_request: settings_request.admitted_request.clone(),
                account: settings_ticket.account.clone(),
                operation: operation.clone(),
                actor: Some(RequestActor {
                    agent: settings_ticket.agent.clone(),
                    epoch: i64::try_from(settings_ticket.epoch)
                        .map_err(|_| NativeError::Changed)?,
                }),
                deleted_actor_session: None,
                expected_scope: Some(scope.clone()),
                method,
                params: params.clone(),
                deadline: Instant::now() + NATIVE_ACTION_TIMEOUT,
            })
            .await;
        let expected = NativeRequest {
            root_request_id: request.request_id.clone(),
            admitted_request: None,
            ticket: settings_ticket.clone(),
            method: "thread/settings/update".into(),
            params: params.clone(),
        };
        let status = match &provider {
            Ok(receipt) => {
                validate_provider_receipt(&expected, receipt)?;
                match &receipt.outcome {
                    ReceiptOutcome::Written {
                        response: ProviderResponse::Reply(_),
                        ..
                    } => NativeSettingsStatus::Acknowledged,
                    ReceiptOutcome::Written {
                        response: ProviderResponse::Rejected(_),
                        ..
                    } => NativeSettingsStatus::Rejected,
                    ReceiptOutcome::WorkerNotSent { .. }
                    | ReceiptOutcome::NoLiveAccountWorker
                    | ReceiptOutcome::ExpectedScopeMismatch
                    | ReceiptOutcome::DeadlineExpired
                    | ReceiptOutcome::NotHandedOff => NativeSettingsStatus::NotSent,
                    ReceiptOutcome::Unknown { .. } => NativeSettingsStatus::Unknown,
                }
            }
            Err(AccountSessionError::Expired) => return Err(NativeError::Expired),
            Err(_) => NativeSettingsStatus::Unknown,
        };
        let action_id = request.request_id.clone();
        let request_agent = request.agent.clone();
        let operation_for_tx = operation.clone();
        store
            .transaction_with(move |tx| {
                let mut agent = agent(tx, &request_agent)?;
                if agent["startAttempt"]["modelSettings"]["operation"] != operation_for_tx
                    || agent["startAttempt"]["modelSettings"]["requestId"] != action_id
                    || agent["startAttempt"]["modelSettings"]["status"]
                        != json!(NativeSettingsStatus::Submitted)
                {
                    return Err(NativeError::Changed);
                }
                agent["startAttempt"]["modelSettings"]["status"] = json!(status);
                AgentRepository::put(tx, &AgentRecord::from_value(agent)?)?;
                Ok(())
            })
            .await?;
        match status {
            NativeSettingsStatus::Acknowledged => return Ok(()),
            NativeSettingsStatus::NotSent if retry < MAX_NOT_SENT_RETRIES => continue,
            NativeSettingsStatus::NotSent
            | NativeSettingsStatus::Submitted
            | NativeSettingsStatus::Unknown => return Err(NativeError::Unknown),
            NativeSettingsStatus::Rejected => {
                return Err(NativeError::Invalid("settings/rejected"));
            }
        }
    }
    Err(NativeError::Unknown)
}

fn replay(tx: &Transaction<'_>, request: &Request) -> Result<Option<Snapshot>> {
    let Some((signature, receipt, outcome)) = row(tx, &request.request_id)? else {
        return Ok(None);
    };
    if signature != request.signature()? {
        return Err(NativeError::Conflict);
    }
    Snapshot::checked(agent(tx, &request.agent)?, receipt, outcome).map(Some)
}

fn response(snapshot: &Snapshot, replayed: bool) -> Value {
    let mut value = json!({
        "receipt": snapshot.receipt,
        "outcome": snapshot.outcome,
        "replayed": replayed,
    });
    if let Outcome::Acknowledged {
        result: Some(result),
    } = snapshot.outcome()
    {
        value["result"] = result.clone();
    }
    value
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
    effects: Arc<impl Acceptance + 'static>,
    deadline: tokio::time::Instant,
) -> Result<Outcome> {
    let claim_id = id.clone();
    let claim_ticket = ticket.clone();
    let request = store
        .transaction_with(move |tx| claim(tx, &claim_id, &claim_ticket))
        .await?;
    let receipt = send(session, &request, deadline).await?;
    store
        .transaction_with(move |tx| settle(tx, &id, &receipt, effects.as_ref()))
        .await
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
fn row(tx: &Transaction<'_>, id: &str) -> Result<Option<(String, AdmissionReceipt, Outcome)>> {
    if let Some(saved) = Outbox::get::<NativeActionIntent>(tx, &action_outbox_id(id))? {
        if saved.kind != OutboxKind::NativeAction
            || saved.owner_kind != OwnerKind::Agent
            || saved.owner_id != saved.payload.receipt.agent_id
        {
            return Err(NativeError::Changed);
        }
        return Ok(Some((
            saved.signature,
            saved.payload.receipt,
            saved.payload.outcome,
        )));
    }
    Ok(saved_reply::<NativeActionReply>(tx, "native-action", id)?
        .map(|(signature, reply)| (signature, reply.receipt, reply.outcome)))
}
fn load(tx: &Transaction<'_>, id: &str) -> Result<Snapshot> {
    let (_, receipt, outcome) = row(tx, id)?.ok_or(NativeError::Invalid("receipt.id"))?;
    Snapshot::checked(agent(tx, &receipt.agent_id)?, receipt, outcome)
        .map_err(|error| located(error, "runtime_outbox", id))
}
pub(crate) fn hold_unclean_actions(tx: &Transaction<'_>) -> Result<()> {
    let mut query = tx.prepare("SELECT id FROM runtime_outbox WHERE kind=?1 AND status=?2")?;
    let ids = query
        .query_map(
            rusqlite::params![OutboxKind::NativeAction, OutboxStatus::Pending],
            |row| row.get::<_, String>(0),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(query);
    for id in ids {
        let row = Outbox::get::<NativeActionIntent>(tx, &id)?.ok_or(NativeError::Changed)?;
        write_outcome(
            tx,
            &row.payload.receipt.request_id,
            &Outcome::Unknown { error: None },
        )?;
    }
    Ok(())
}

fn write_outcome(tx: &Transaction<'_>, id: &str, outcome: &Outcome) -> Result<()> {
    let Some(mut row) = Outbox::get::<NativeActionIntent>(tx, &action_outbox_id(id))? else {
        return if row(tx, id)?.is_some_and(|(_, _, saved)| saved == *outcome) {
            Ok(())
        } else {
            Err(NativeError::Changed)
        };
    };
    row.payload.outcome = outcome.clone();
    match outcome {
        Outcome::Pending { .. } => {
            row.status = OutboxStatus::Pending;
            row.next_at = Some(chrono::Utc::now().timestamp_millis());
            Outbox::put(tx, &row)?;
        }
        Outcome::Unknown { .. } => {
            row.status = OutboxStatus::Unknown;
            row.next_at = None;
            Outbox::put(tx, &row)?;
        }
        Outcome::Failed { .. } | Outcome::Acknowledged { .. } => {
            let reply = NativeActionReply {
                receipt: row.payload.receipt,
                outcome: outcome.clone(),
                replayed: false,
                result: match outcome {
                    Outcome::Acknowledged { result } => result.clone(),
                    Outcome::Pending { .. } | Outcome::Unknown { .. } | Outcome::Failed { .. } => {
                        None
                    }
                },
            };
            settle_request(tx, &row.payload.admitted, Some(&row.owner_id), &reply)?;
            Outbox::delete_settled(tx, &row.id, &row.signature)?;
        }
    }
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
        other @ NativeError::InvalidStored { .. }
        | other @ NativeError::Conflict
        | other @ NativeError::Changed
        | other @ NativeError::Unknown
        | other @ NativeError::Expired
        | other @ NativeError::Unavailable(_)
        | other @ NativeError::Store(_)
        | other @ NativeError::Json(_)
        | other @ NativeError::Sql(_) => other,
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
            Value::Null
            | Value::Bool(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Array(_)
            | Value::Object(_) => return Err(NativeError::Invalid("command.processId")),
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
    ) -> Result<Box<serde_json::value::RawValue>>;
}
#[derive(Clone, Copy)]
pub enum CommandAction<'a> {
    Input(&'a str),
    Cancel,
}

/// Opaque original public response, independent of current command readiness.
pub struct CommandReply {
    pub response_json: String,
}

/// The task's current owner authorizes disclosure before any result is read.
pub fn replay_command(
    connection: &rusqlite::Connection,
    agent_id: &str,
    task_id: &str,
    root: &str,
    action: CommandAction<'_>,
) -> Result<Option<CommandReply>> {
    use studio_runtime_store::{AgentId, TaskId, TaskRepository};
    let task =
        TaskRepository::get(connection, &TaskId::from(task_id))?.ok_or(NativeError::Changed)?;
    if task.value()["agent"] != agent_id
        || task.value()["kind"] != "command"
        || AgentRepository::get(connection, &AgentId::from(agent_id))?.is_none()
    {
        return Err(NativeError::Changed);
    }
    let signature = match action {
        CommandAction::Input(text) => json!({"task":task_id,"action":"input","text":text}),
        CommandAction::Cancel => json!({"task":task_id,"action":"cancel"}),
    }
    .to_string();
    let saved = match action {
        CommandAction::Input(_) => saved_reply::<Value>(connection, "native-command", root)?,
        CommandAction::Cancel => {
            saved_reply::<CommandCancelReply>(connection, "native-command", root)?
                .map(|(signature, reply)| {
                    if reply.request.ticket.agent != agent_id
                        || reply.request.ticket.operation != task_id
                    {
                        return Err(NativeError::Changed);
                    }
                    Ok((signature, reply.result))
                })
                .transpose()?
        }
    };
    if let Some((saved_signature, reply)) = saved {
        if signature != saved_signature {
            return Err(NativeError::Conflict);
        }
        return Ok(Some(CommandReply {
            response_json: reply.to_string(),
        }));
    }
    if let Some(saved) = Outbox::get::<CommandCancelIntent>(connection, &command_outbox_id(root))? {
        if saved.signature != signature
            || saved.owner_id != task_id
            || saved.payload.request.ticket.agent != agent_id
        {
            return Err(NativeError::Conflict);
        }
        return Err(NativeError::Unknown);
    }
    let now = chrono::Utc::now().timestamp_millis();
    let identity = RequestIdentity::new("native-command", &[], root, &[]).map_err(|_| {
        studio_runtime_schema::persistence::RequestIdentityError::Expired {
            issued_ms: 0,
            expired_before_ms: now,
        }
    })?;
    identity.validate(now, RequestResults::expired_before_ms(connection)?)?;
    Ok(None)
}

pub fn command_input(
    tx: &Transaction<'_>,
    key: &str,
    snapshot: &CommandSnapshot,
    text: &str,
    admission: &impl CommandAdmission,
) -> Result<Box<serde_json::value::RawValue>> {
    use crate::event_write::{CallerAdmission, RequestKey, begin_request};
    let signature = json!({"task":snapshot.task["id"],"action":"input","text":text}).to_string();
    let admitted = match begin_request::<Box<serde_json::value::RawValue>>(
        tx,
        RequestKey::new("native-command", &[], key).raw(&format!("native-command:{key}")),
        &signature,
        chrono::Utc::now().timestamp_millis(),
    )? {
        CallerAdmission::Fresh(admitted) => admitted,
        CallerAdmission::Replay(reply) => return Ok(reply),
    };
    snapshot.current(tx)?;
    let result = admission.enqueue(tx, &snapshot.agent, key, &snapshot.input(text)?)?;
    if result.get().len() > MAX_BYTES {
        return Err(NativeError::Invalid("command.result"));
    }
    settle_request(tx, &admitted, snapshot.agent["id"].as_str(), &result)?;
    Ok(result)
}

/// Only the retained owner may supply a matching list response. Persist its
/// exact scope before terminate; the operation receipt never authorizes resend.
pub enum CommandCancelClaim {
    Send(Box<NativeRequest>),
    Replay(Value),
}

fn same_native_request(left: &NativeRequest, right: &NativeRequest) -> bool {
    left.ticket == right.ticket && left.method == right.method && left.params == right.params
}

pub fn command_cancel_claim(
    tx: &Transaction<'_>,
    snapshot: &CommandSnapshot,
    ticket: &ProviderTicket,
    list: &TransportReceipt,
) -> Result<CommandCancelClaim> {
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
    let id = command_outbox_id(&ticket.request);
    let request = NativeRequest {
        root_request_id: ticket.request.clone(),
        admitted_request: None,
        ticket: ticket.clone(),
        method: "thread/backgroundTerminals/terminate".into(),
        params: json!({"threadId":ticket.thread,"processId":snapshot.process}),
    };
    let signature = json!({"task":snapshot.task["id"],"action":"cancel"}).to_string();
    if let Some((saved_signature, saved)) =
        saved_reply::<CommandCancelReply>(tx, "native-command", &ticket.request)?
    {
        if saved_signature != signature || !same_native_request(&saved.request, &request) {
            return Err(NativeError::Conflict);
        }
        return Ok(CommandCancelClaim::Replay(saved.result));
    }
    if let Some(saved) = Outbox::get::<CommandCancelIntent>(tx, &id)? {
        if saved.signature != signature || !same_native_request(&saved.payload.request, &request) {
            return Err(NativeError::Conflict);
        }
        return Err(NativeError::Unknown);
    }
    let Delivery::Written(page) = &list.delivery else {
        return Err(NativeError::Unknown);
    };
    if !snapshot.listed(page)? {
        return Err(NativeError::Invalid("command.session"));
    }
    let admitted = admit(
        tx,
        "native-command",
        &ticket.request,
        &signature,
        chrono::Utc::now().timestamp_millis(),
    )?;
    let request = reserve_provider_intent(tx, &admitted, &ticket.request, request)?;
    Outbox::put(
        tx,
        &OutboxRow {
            id,
            kind: OutboxKind::NativeAction,
            owner_kind: OwnerKind::Task,
            owner_id: ticket.operation.clone(),
            request_id: Some(admitted.id().into()),
            signature,
            status: OutboxStatus::Unknown,
            account: Some(ticket.account.clone()),
            stream_id: None,
            generation: Some(i64::try_from(ticket.generation).map_err(|_| NativeError::Changed)?),
            ordinal: None,
            next_at: None,
            created: admitted.created(),
            payload: CommandCancelIntent {
                admitted,
                request: request.clone(),
            },
        },
    )?;
    Ok(CommandCancelClaim::Send(Box::new(request)))
}
pub fn command_cancel_settle(tx: &Transaction<'_>, receipt: &TransportReceipt) -> Result<Value> {
    let id = command_outbox_id(&receipt.ticket.request);
    let saved = Outbox::get::<CommandCancelIntent>(tx, &id)?;
    let Some(saved) = saved else {
        let (_, saved) =
            saved_reply::<CommandCancelReply>(tx, "native-command", &receipt.ticket.request)?
                .ok_or(NativeError::Changed)?;
        return if saved.request.ticket == receipt.ticket
            && matches!(&receipt.delivery, Delivery::Written(result) if *result == saved.result)
        {
            Ok(saved.result)
        } else {
            Err(NativeError::Conflict)
        };
    };
    let request = &saved.payload.request;
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
    settle_request(
        tx,
        &saved.payload.admitted,
        Some(&request.ticket.agent),
        &CommandCancelReply {
            request: request.clone(),
            result: result.clone(),
        },
    )?;
    Outbox::delete_settled(tx, &id, &saved.signature)?;
    Ok(result.clone())
}
