//! The first turn of a native review child: Python `codex_agent_review.claim`
//! followed by `Runtime.run_native_action` for action `review`.
//!
//! The account worker runs this driver when it wakes a queued child whose
//! `nativeReview.status` is `pending`. The child has no native thread yet, so
//! the driver first starts one with the ordinary thread planner (no Studio
//! tools, `review_model` set to the child model). Then the shared native action
//! owner reserves, prepares settings, and hands off one `review/start` request
//! with `delivery: "inline"`. The provider review result reaches the parent
//! through the ordinary turn completion (`exitedReviewMode` sets `lastAnswer`).
//!
//! Identity rules:
//! - The action request ID is fixed per child and epoch, so a repeated wake
//!   replays the saved receipt and never sends a second `review/start`.
//! - The thread start is saved as `submitted` before the provider write. A
//!   saved `submitted` or `unknown` start is never sent again.
use crate::{
    account_session_runtime::{
        AccountSessionPort, ConnectionScope, ProviderRequest, ProviderResponse, RequestCertainty,
    },
    account_worker::{WorkerError, preparation, retained::RequestActor},
    native_action_ports::{MessageNativeAdmission, NativeActionAcceptance},
    native_actions::{
        self, Acceptance, Action, Admission, Delivery, NativeError, NativeRequest, Outcome, Request,
    },
};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use studio_runtime_providers::Method;
#[cfg(test)]
use studio_runtime_store::path_identity::CanonicalPath;
use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, RuntimeStore, Transaction};

/// Prefix of the native action request ID of a review child's first turn.
pub const REQUEST_PREFIX: &str = "native-review:";
/// Deadline of the review thread start; Python waits for the same native call
/// without a separate limit, and the retained session needs a finite one.
const THREAD_START_TIMEOUT: Duration = Duration::from_secs(60);
/// Agent field that holds the receipt of the review thread start.
const THREAD_RECEIPT: &str = "nativeReviewThread";

/// The queued review child state that Python `codex_agent_review.claim`
/// accepts. A leftover unsubmitted attempt of a refused earlier start does not
/// block the review: it never reached the provider.
pub fn claimable(record: &Value) -> bool {
    let attempt = record.get("startAttempt").unwrap_or(&Value::Null);
    record["nativeReview"]["status"] == "pending"
        && record["status"] == "queued"
        && record["autoWake"] == true
        && !truthy(record.get("inFlight"))
        && !truthy(record.get("deletedAt"))
        && !truthy(record.get("nativeFailureHold"))
        && record
            .get("provider")
            .is_none_or(|provider| provider == "codex")
        && (attempt.is_null()
            || (attempt["submitted"] == false
                && !truthy(attempt.get("turnId"))
                && !truthy(attempt.get("observedTurnId"))))
}

/// The durable native action request of this child's review turn.
pub fn request(record: &Value) -> Result<Request, NativeError> {
    let id = record["id"]
        .as_str()
        .ok_or(NativeError::Invalid("agent.id"))?;
    let epoch = record["epoch"]
        .as_i64()
        .ok_or(NativeError::Invalid("agent.epoch"))?;
    let account = record
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    Ok(Request {
        request_id: format!("{REQUEST_PREFIX}{id}:{epoch}"),
        agent: id.to_owned(),
        action: Action::Review,
        context: json!({"accountKey":account,"epoch":epoch}),
    })
}

/// The start attempt identity `native_actions::execute_provider_prepared`
/// assigns to this request; failure notices use it as their marker.
fn attempt_id(request: &Request) -> String {
    uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("native-action:{}", request.request_id).as_bytes(),
    )
    .to_string()
}

fn truthy(value: Option<&Value>) -> bool {
    studio_runtime_store::truthy(value)
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

/// Whether the account worker should run this driver for the agent now.
pub(crate) async fn candidate(
    store: &Arc<RuntimeStore>,
    agent_id: &str,
    epoch: i64,
    account: &str,
) -> Result<bool, WorkerError> {
    let agent_id = agent_id.to_owned();
    let account = account.to_owned();
    store
        .read_async("runtime-services/native_review_turn", move |tx| {
            // A malformed record is not a review candidate: the ordinary
            // claim owns its visible invalid-record hold.
            let Ok(Some(record)) = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))
            else {
                return Ok(false);
            };
            let value = record.value();
            Ok(value["epoch"].as_i64() == Some(epoch)
                && value
                    .get("accountKey")
                    .and_then(Value::as_str)
                    .unwrap_or("default")
                    == account
                && claimable(&value))
        })
        .await
}

/// Run one review child's first turn. Every failure is saved on the child
/// before this returns; the result is only for the caller's log.
pub(crate) async fn run(
    store: Arc<RuntimeStore>,
    resources: &preparation::PreparationResources,
    agent_id: &str,
    epoch: i64,
) -> Result<(), NativeError> {
    for _ in 0..2 {
        match run_once(Arc::clone(&store), resources, agent_id, epoch).await {
            Err(NativeError::Unavailable(preparation::CATALOG_PENDING_REASON)) => {
                let record = read_agent(&store, agent_id).await?;
                let account = record
                    .get("accountKey")
                    .and_then(Value::as_str)
                    .unwrap_or("default")
                    .to_owned();
                resources
                    .model_catalog
                    .refreshed(&account, &resources.account_home, "codex")
                    .await;
            }
            result => return result,
        }
    }
    Ok(())
}

async fn run_once(
    store: Arc<RuntimeStore>,
    resources: &preparation::PreparationResources,
    agent_id: &str,
    epoch: i64,
) -> Result<(), NativeError> {
    let record = read_agent(&store, agent_id).await?;
    if record["epoch"] != epoch || !claimable(&record) {
        return Ok(());
    }
    let request = request(&record)?;
    let Some(port) = resources.account_session.clone() else {
        // A host without a provider session leaves the review queued.
        return Err(NativeError::Unavailable("provider-session/native-request"));
    };
    let admission_agent = agent_id.to_owned();
    let admission = store
        .read_async(
            "runtime-services/native_review_turn_admission",
            move |connection| {
                crate::message_admission::MessageIngressAdmission::read(
                    connection,
                    &admission_agent,
                )
                .map_err(|_| NativeError::Unavailable("budget-policy/native-action"))
            },
        )
        .await?;
    let admission = admission
        .prepare()
        .await
        .map_err(|_| NativeError::Unavailable("budget-policy/native-action"))?;
    let gate: Arc<dyn Admission> = Arc::new(MessageNativeAdmission::new(Arc::new(admission)));
    let preparation = ReviewPreparation {
        store: Arc::clone(&store),
        resources: resources.clone(),
        gate: Arc::clone(&gate),
    };
    let result = native_actions::execute_provider_prepared(
        &store,
        port.as_ref(),
        &request,
        now(),
        gate,
        Arc::new(ReviewAcceptance),
        &preparation,
    )
    .await;
    finish(&store, &request, result).await
}

async fn read_agent(store: &Arc<RuntimeStore>, agent_id: &str) -> Result<Value, NativeError> {
    let agent_id = agent_id.to_owned();
    store
        .read_async("runtime-services/native_review_turn", move |tx| {
            AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                .map(|record| record.value())
                .ok_or(NativeError::Changed)
        })
        .await
}

/// A refusal that leaves the review queued for a later wake, as Python's
/// dispatcher skips a child while capacity, budget or the session are missing.
fn transient(error: &NativeError) -> bool {
    matches!(
        error,
        NativeError::Unavailable(_)
            | NativeError::Invalid(
                "review/global-capacity" | "review/team-capacity" | "budget" | "workspace"
            )
    )
}

/// Save the visible end of a review start that produced no native turn.
/// Python `start_error` marks the child failed and tells the parent once.
async fn finish(
    store: &Arc<RuntimeStore>,
    request: &Request,
    result: Result<Value, NativeError>,
) -> Result<(), NativeError> {
    let error = match result {
        Ok(value) if value["outcome"]["status"] == "pending" => {
            NativeError::Unavailable("native-review/not-sent")
        }
        Ok(_) => return Ok(()),
        Err(error) => error,
    };
    let message = match &error {
        NativeError::Unknown => {
            "The native review start outcome is unknown. Studio does not send it again.".to_owned()
        }
        NativeError::Unavailable("native-review/not-sent") => {
            "The native review request was not sent.".to_owned()
        }
        other => format!("Native review could not start: {other}"),
    };
    let request = request.clone();
    let not_sent = matches!(error, NativeError::Unavailable("native-review/not-sent"));
    let keep_queued = transient(&error) && !not_sent;
    store
        .transaction_with(move |tx| {
            let mut agent = AgentRepository::get(tx, &AgentId::from(request.agent.as_str()))?
                .ok_or(NativeError::Changed)?
                .value();
            let reserved = agent["startAttempt"]["actionRequestId"] == request.request_id;
            if !reserved {
                // Nothing was admitted: the child is still the queued review.
                if keep_queued || !claimable(&agent) {
                    return Ok(());
                }
            } else {
                match native_actions::outcome(tx, &request.request_id)? {
                    Some(Outcome::Pending { .. })
                        if agent["startAttempt"]["submitted"] == false
                            && agent["inFlight"] == true => {}
                    Some(Outcome::Unknown { .. }) if agent["startAttempt"]["submitted"] == true => {
                        // The hand-off is uncertain. Keep its owner for recovery;
                        // only show why the review has no turn yet.
                        agent["error"] = json!(message);
                        return AgentRepository::put(tx, &AgentRecord::from_value(agent)?)
                            .map_err(NativeError::from);
                    }
                    _ => return Ok(()),
                }
                native_actions::fail_pending(tx, &request.request_id, &message)?;
                agent = AgentRepository::get(tx, &AgentId::from(request.agent.as_str()))?
                    .ok_or(NativeError::Changed)?
                    .value();
            }
            let before = agent.clone();
            agent["status"] = json!("failed");
            agent["inFlight"] = json!(false);
            agent["error"] = json!(message);
            if agent["startAttempt"].is_object() {
                agent["startAttempt"]["executionOutcome"] = json!("unsent");
            }
            AgentRepository::put(tx, &AgentRecord::from_value(agent.clone())?)?;
            crate::lifecycle_effects::notify_parent_of_start_failure(
                tx,
                &before,
                &agent,
                &attempt_id(&request),
                now(),
            )?;
            Ok::<_, NativeError>(())
        })
        .await?;
    Err(error)
}

/// The shared native action effects, and Python's parent notice when the
/// provider rejects `review/start` (`start_error` with a failed child).
struct ReviewAcceptance;
impl Acceptance for ReviewAcceptance {
    fn apply(
        &self,
        tx: &Transaction<'_>,
        request: &NativeRequest,
        delivery: &Delivery,
    ) -> native_actions::Result<()> {
        let before = AgentRepository::get(tx, &AgentId::from(request.ticket.agent.as_str()))?
            .ok_or(NativeError::Changed)?
            .value();
        NativeActionAcceptance.apply(tx, request, delivery)?;
        if matches!(delivery, Delivery::Rejected(_)) {
            let failed = AgentRepository::get(tx, &AgentId::from(request.ticket.agent.as_str()))?
                .ok_or(NativeError::Changed)?
                .value();
            crate::lifecycle_effects::notify_parent_of_start_failure(
                tx,
                &before,
                &failed,
                &request.ticket.operation,
                now(),
            )?;
        }
        Ok(())
    }
}

struct ReviewPreparation {
    store: Arc<RuntimeStore>,
    resources: preparation::PreparationResources,
    gate: Arc<dyn Admission>,
}

impl native_actions::Preparation for ReviewPreparation {
    fn before_reserve<'a>(
        &'a self,
        _store: &'a RuntimeStore,
        provider: &'a dyn AccountSessionPort,
        request: &'a Request,
        scope: &'a ConnectionScope,
    ) -> native_actions::PreparationFuture<'a> {
        Box::pin(self.ensure_thread(provider, request, scope))
    }

    fn prepare<'a>(
        &'a self,
        _store: &'a RuntimeStore,
        provider: &'a dyn AccountSessionPort,
        request: &'a Request,
        ticket: &'a crate::workspace_operations::ProviderTicket,
        scope: &'a ConnectionScope,
    ) -> native_actions::PreparationFuture<'a> {
        Box::pin(async move {
            // Python `codex_native_action_settings.ensure` before `review/start`.
            let prepared = preparation::prepare_native_action(
                Arc::clone(&self.store),
                &self.resources,
                &request.agent,
                &ticket.operation,
                ticket.generation,
            )
            .await
            .map_err(worker_error)?;
            if Some(prepared.thread_id.as_str()) != ticket.thread.as_deref() {
                return Err(NativeError::Changed);
            }
            let mut params = prepared.settings;
            if let Some(params) = params.as_object_mut() {
                params.remove("cwd");
                params.remove("cyberAccessProgram");
            }
            native_actions::update_action_settings(
                &self.store,
                provider,
                request,
                ticket,
                scope,
                params,
            )
            .await
        })
    }
}

fn worker_error(error: WorkerError) -> NativeError {
    match error {
        WorkerError::PreparationPending(reason)
            if reason.reason
                == studio_runtime_schema::agent_progress::StartWaitReason::ModelCatalog =>
        {
            NativeError::Unavailable(preparation::CATALOG_PENDING_REASON)
        }
        WorkerError::PreparationPending(_) | WorkerError::PreparationAwaitsAgent(_) => {
            NativeError::Unavailable("native-review/preparation")
        }
        _ => NativeError::Invalid("native-review/preparation"),
    }
}

impl ReviewPreparation {
    /// Python `prepare_locked` for a child without a thread: one durable
    /// `thread/start`, reduced by the ordinary thread receipt reducer.
    async fn ensure_thread(
        &self,
        provider: &dyn AccountSessionPort,
        request: &Request,
        scope: &ConnectionScope,
    ) -> native_actions::Result<()> {
        let record = read_agent(&self.store, &request.agent).await?;
        // A thread from an earlier wake (or a not-sent retry of this action)
        // needs no start; the reservation rechecks the review state.
        if record["threadId"].as_str().is_some_and(|id| !id.is_empty()) {
            return Ok(());
        }
        if !claimable(&record) {
            return Err(NativeError::Changed);
        }
        // Python claims the review only after dispatcher admission. Check the
        // same gate before a native thread exists.
        let gate = Arc::clone(&self.gate);
        let agent_id = request.agent.clone();
        self.store
            .transaction_with(move |tx| {
                let current = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                    .ok_or(NativeError::Changed)?
                    .value();
                gate.check(tx, &current, Action::Review)
            })
            .await?;
        let saved = record.get(THREAD_RECEIPT).cloned().unwrap_or(Value::Null);
        if matches!(saved["status"].as_str(), Some("submitted" | "unknown")) {
            return Err(NativeError::Unknown);
        }
        let sequence = saved["sequence"].as_u64().map_or(0, |value| value + 1);
        let epoch = record["epoch"]
            .as_i64()
            .ok_or(NativeError::Invalid("agent.epoch"))?;
        let attempt = format!("native-review-thread:{}:{epoch}:{sequence}", request.agent);
        let prepared = Arc::new(
            preparation::prepare_review_thread(
                Arc::clone(&self.store),
                &self.resources,
                &request.agent,
                &attempt,
                scope.generation,
            )
            .await
            .map_err(worker_error)?,
        );
        let operation = format!("{attempt}:start");
        let receipt = json!({"sequence":sequence,"attempt":prepared.attempt_id(),"operation":operation,
            "generation":scope.generation,"connection":scope.connection,
            "requestDigest":prepared.request_digest(),"status":"submitted"});
        let submitted = Arc::clone(&prepared);
        let agent_id = request.agent.clone();
        let account = scope.account.clone();
        let expected = saved.clone();
        let saved_receipt = receipt.clone();
        self.store
            .transaction_with(move |tx| {
                let mut current = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                    .ok_or(NativeError::Changed)?
                    .value();
                if !claimable(&current)
                    || current["epoch"] != epoch
                    || current
                        .get("accountKey")
                        .and_then(Value::as_str)
                        .unwrap_or("default")
                        != account
                    || current["threadId"]
                        .as_str()
                        .is_some_and(|id| !id.is_empty())
                    || current.get(THREAD_RECEIPT).unwrap_or(&Value::Null) != &expected
                {
                    return Err(NativeError::Changed);
                }
                preparation::review_thread_submitted(&mut current, &submitted)
                    .map_err(|_| NativeError::Invalid("native-review/thread-start"))?;
                current[THREAD_RECEIPT] = saved_receipt;
                AgentRepository::put(tx, &AgentRecord::from_value(current)?)?;
                Ok(())
            })
            .await?;
        let response = provider
            .provider_request(ProviderRequest {
                account: scope.account.clone(),
                operation: operation.clone(),
                actor: Some(RequestActor {
                    agent: request.agent.clone(),
                    epoch,
                }),
                expected_scope: Some(scope.clone()),
                deleted_actor_session: None,
                method: Method::ThreadStart,
                params: prepared.params(),
                deadline: Instant::now() + THREAD_START_TIMEOUT,
            })
            .await;
        let (status, reply) = match response {
            Ok(receipt) if receipt.scope != *scope || receipt.operation != operation => {
                ("unknown", None)
            }
            Ok(receipt) => match (receipt.certainty, receipt.result) {
                (RequestCertainty::Written, Some(ProviderResponse::Reply(value))) => {
                    ("accepted", Some(value))
                }
                (RequestCertainty::Written, Some(ProviderResponse::Rejected(_))) => {
                    ("rejected", None)
                }
                (RequestCertainty::NotSent, None) => ("not_sent", None),
                _ => ("unknown", None),
            },
            Err(_) => ("unknown", None),
        };
        let resources = self
            .resources
            .for_receipt(&self.store, &request.agent, Some(prepared.cwd()))
            .await
            .map_err(|_| NativeError::Unavailable("native-review/filesystem"))?;
        let root = self.store.root().to_path_buf();
        let agent_id = request.agent.clone();
        let accepted = self
            .store
            .transaction_with(move |tx| {
                let mut current = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                    .ok_or(NativeError::Changed)?
                    .value();
                if current[THREAD_RECEIPT]["operation"] != operation
                    || current[THREAD_RECEIPT]["status"] != "submitted"
                {
                    return Err(NativeError::Changed);
                }
                let mut status = status;
                if let Some(reply) = reply.as_ref() {
                    match preparation::accept_review_thread(
                        tx, &resources, &root, &current, &prepared, reply,
                    )
                    .map_err(|_| NativeError::Invalid("native-review/thread-receipt"))?
                    {
                        Some(patched) => current = patched,
                        None => status = "unknown",
                    }
                }
                current[THREAD_RECEIPT]["status"] = json!(status);
                if status == "accepted" {
                    current[THREAD_RECEIPT]["threadId"] = current["threadId"].clone();
                }
                AgentRepository::put(tx, &AgentRecord::from_value(current)?)?;
                Ok(status)
            })
            .await?;
        match accepted {
            "accepted" => Ok(()),
            "rejected" => Err(NativeError::Invalid("native-review/thread-start-rejected")),
            "not_sent" => Err(NativeError::Invalid("native-review/thread-start-not-sent")),
            _ => Err(NativeError::Unknown),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod tests {
    use super::*;
    use crate::account_session_runtime::{AccountSessionFuture, SessionReceipt};
    use std::sync::Mutex;
    use studio_runtime_providers::{RequestId, RequestTicket};

    /// The provider outcome the fake session gives one method.
    #[derive(Clone, Copy, PartialEq)]
    pub(crate) enum Reply {
        Accept,
        Reject,
        Unknown,
    }

    /// A fake retained session that answers the review child's requests.
    pub(crate) struct Session {
        requests: Mutex<Vec<(Method, Value)>>,
        thread_start: Reply,
        review_start: Reply,
    }
    impl Session {
        pub(crate) fn new(thread_start: Reply, review_start: Reply) -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                thread_start,
                review_start,
            })
        }
        fn scope() -> ConnectionScope {
            ConnectionScope {
                account: "default".into(),
                connection: "connection".into(),
                generation: 7,
            }
        }
        pub(crate) fn methods(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|(method, _)| method.as_str().to_owned())
                .collect()
        }
        fn params(&self, method: Method) -> Value {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .find(|(saved, _)| *saved == method)
                .map(|(_, params)| params.clone())
                .unwrap()
        }
    }
    impl AccountSessionPort for Session {
        fn current_scope(&self, _account: &str) -> Option<ConnectionScope> {
            Some(Self::scope())
        }
        fn provider_request<'a>(
            &'a self,
            request: ProviderRequest,
        ) -> AccountSessionFuture<'a, SessionReceipt> {
            Box::pin(async move {
                self.requests
                    .lock()
                    .unwrap()
                    .push((request.method, request.params.clone()));
                let (reply, value) = match request.method {
                    Method::ThreadStart => (
                        self.thread_start,
                        json!({"thread":{"id":"review-thread"},"model":"gpt-6",
                            "sandbox":{"type":"readOnly"},"approvalPolicy":"on-request"}),
                    ),
                    Method::ReviewStart => (
                        self.review_start,
                        json!({"turn":{"id":"review-turn","items":[],"status":"inProgress"},
                            "reviewThreadId":"review-thread"}),
                    ),
                    _ => (Reply::Accept, json!({})),
                };
                let ticket = RequestTicket {
                    generation: 7,
                    id: RequestId::String(format!(
                        "retained:default:{}",
                        studio_runtime_store::sha256_hex(request.operation.as_bytes())
                    )),
                    method: request.method,
                };
                let (certainty, result) = match reply {
                    Reply::Accept => (
                        RequestCertainty::Written,
                        Some(ProviderResponse::Reply(value)),
                    ),
                    Reply::Reject => (
                        RequestCertainty::Written,
                        Some(ProviderResponse::Rejected(
                            json!({"code":-32600,"message":"refused"}),
                        )),
                    ),
                    Reply::Unknown => (RequestCertainty::Unknown, None),
                };
                Ok(SessionReceipt {
                    scope: Self::scope(),
                    operation: request.operation,
                    ticket: Some(ticket),
                    certainty,
                    result,
                    not_sent_reason: None,
                    replayed: false,
                })
            })
        }
    }

    struct Fixture {
        resources: preparation::PreparationResources,
        store: Arc<RuntimeStore>,
        _directory: tempfile::TempDir,
    }

    /// A lead and its queued review child as `orchestration_review` saves it
    /// (the shape of live agent 8388eb70).
    async fn fixture(session: Arc<Session>) -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let cwd = CanonicalPath::existing(directory.path())
            .unwrap()
            .join("repo");
        std::fs::create_dir(&cwd).unwrap();
        let state = directory.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let store = Arc::new(RuntimeStore::open(Some(&state)).await.unwrap());
        let cwd = cwd.to_string_lossy().into_owned();
        let lead = json!({"id":"lead","rootId":"lead","parentId":null,"epoch":0,"status":"running",
            "name":"Lead","cwd":cwd,"isLead":true,"autoWake":true,"inFlight":true,"created":1.0,
            "threadId":"lead-thread","turnId":"lead-turn","accountKey":"default","provider":"codex",
            "model":"gpt-6","effort":"medium","fastMode":false,"concurrency":32});
        let child = json!({"id":"review","rootId":"lead","parentId":"lead","epoch":0,
            "status":"queued","name":"Review","cwd":cwd,"isLead":false,"autoWake":true,
            "inFlight":false,"created":2.0,"threadId":null,"turnId":null,"accountKey":"default",
            "provider":"codex","role":"reviewer","model":"gpt-6","effort":"medium",
            "nativeEffort":"medium","fastMode":false,"yoloMode":false,"worktree":false,
            "workspaceMode":"shared","daybreakEnabled":false,
            "prompt":"Run a native code review: {\"type\": \"uncommittedChanges\"}",
            "nativeReview":{"target":{"type":"uncommittedChanges"},"requestId":"lead:review:r1",
                "actorId":"lead","args":{},"status":"pending","response":{}}});
        store
            .transaction_with(move |tx| {
                AgentRepository::put(tx, &AgentRecord::from_value(lead)?)?;
                AgentRepository::put(tx, &AgentRecord::from_value(child)?)?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .await
            .unwrap();
        let resources = preparation::PreparationResources::bundled(
            String::new(),
            crate::model_catalog::ModelCatalogService::new(),
        )
        .unwrap()
        .with_account_session(Some(session));
        Fixture {
            _directory: directory,
            store,
            resources,
        }
    }

    async fn child(store: &Arc<RuntimeStore>) -> Value {
        read_agent(store, "review").await.unwrap()
    }

    async fn parent_events(store: &Arc<RuntimeStore>) -> Vec<(String, String)> {
        store
            .read_async("test", |tx| {
                let mut statement = tx
                    .prepare("SELECT id,kind FROM runtime_events WHERE agent='lead' ORDER BY id")?;
                let rows = statement
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok::<_, studio_runtime_store::StoreError>(rows)
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_queued_review_child_starts_one_read_only_thread_and_one_review() {
        let session = Session::new(Reply::Accept, Reply::Accept);
        let fixture = fixture(Arc::clone(&session)).await;
        assert!(
            candidate(&fixture.store, "review", 0, "default")
                .await
                .unwrap()
        );
        run(Arc::clone(&fixture.store), &fixture.resources, "review", 0)
            .await
            .unwrap();
        assert_eq!(
            session.methods(),
            ["thread/start", "thread/settings/update", "review/start"]
        );
        // Python `new_thread_params` for a reviewer: no Studio tools, the
        // review model is the child model, and the sandbox is read-only.
        let start = session.params(Method::ThreadStart);
        assert_eq!(start["dynamicTools"], json!([]));
        assert_eq!(start["config"]["review_model"], "gpt-6");
        assert_eq!(start["sandbox"], "read-only");
        assert_eq!(start["approvalPolicy"], "on-request");
        // Python `run_native_action` for action review.
        assert_eq!(
            session.params(Method::ReviewStart),
            json!({"threadId":"review-thread","target":{"type":"uncommittedChanges"},
                "delivery":"inline"})
        );
        let running = child(&fixture.store).await;
        assert_eq!(running["threadId"], "review-thread");
        assert_eq!(running["turnId"], "review-turn");
        assert_eq!(running["status"], "running");
        assert_eq!(running["nativeReview"]["status"], "started");
        assert_eq!(running[THREAD_RECEIPT]["status"], "accepted");
        assert_eq!(running["startAttempt"]["action"], "review");
        assert_eq!(running["startAttempt"]["submitted"], true);
        // A second wake sends nothing: the child is no longer claimable.
        assert!(
            !candidate(&fixture.store, "review", 0, "default")
                .await
                .unwrap()
        );
        run(Arc::clone(&fixture.store), &fixture.resources, "review", 0)
            .await
            .unwrap();
        assert_eq!(session.methods().len(), 3);
    }

    #[tokio::test]
    async fn an_ordinary_claim_leaves_queued_input_to_the_pending_review() {
        let session = Session::new(Reply::Accept, Reply::Accept);
        let fixture = fixture(session).await;
        fixture
            .store
            .transaction_with(|tx| {
                tx.execute(
                    "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) \
                     VALUES('message','review','agent_message','{}','pending',3.0,0)",
                    [],
                )?;
                let claimed =
                    crate::scheduler::claim_preparation_in_transaction(tx, "review", 0, "a", 4.0)
                        .map_err(|error| {
                        studio_runtime_store::StoreError::SchemaMismatch(error.to_string())
                    })?;
                assert!(claimed.is_none(), "the review owns the first turn");
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .await
            .unwrap();
        let status: String = fixture
            .store
            .test_reader()
            .unwrap()
            .query_row(
                "SELECT status FROM runtime_events WHERE id='message'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending");
        assert!(
            candidate(&fixture.store, "review", 0, "default")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn an_unknown_thread_start_fails_the_child_once_and_is_never_resent() {
        let session = Session::new(Reply::Unknown, Reply::Accept);
        let fixture = fixture(Arc::clone(&session)).await;
        assert!(matches!(
            run(Arc::clone(&fixture.store), &fixture.resources, "review", 0).await,
            Err(NativeError::Unknown)
        ));
        let failed = child(&fixture.store).await;
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed[THREAD_RECEIPT]["status"], "unknown");
        assert!(failed["error"].as_str().unwrap().contains("unknown"));
        assert_eq!(parent_events(&fixture.store).await.len(), 1);
        // A later message queues the child again; the uncertain start stays unsent.
        fixture
            .store
            .transaction_with(|tx| {
                let mut agent = AgentRepository::get(tx, &AgentId::from("review"))?
                    .unwrap()
                    .value();
                agent["status"] = json!("queued");
                AgentRepository::put(tx, &AgentRecord::from_value(agent)?)
            })
            .await
            .unwrap();
        assert!(
            run(Arc::clone(&fixture.store), &fixture.resources, "review", 0)
                .await
                .is_err()
        );
        assert_eq!(session.methods(), ["thread/start"]);
        assert_eq!(parent_events(&fixture.store).await.len(), 1);
    }

    #[tokio::test]
    async fn a_rejected_review_start_fails_the_child_and_tells_the_parent() {
        let session = Session::new(Reply::Accept, Reply::Reject);
        let fixture = fixture(Arc::clone(&session)).await;
        run(Arc::clone(&fixture.store), &fixture.resources, "review", 0)
            .await
            .unwrap();
        let failed = child(&fixture.store).await;
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["inFlight"], false);
        let events = parent_events(&fixture.store).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1, "child_result");
        assert!(events[0].0.starts_with("child-stop:review:0:start:"));
        assert_eq!(
            session.methods(),
            ["thread/start", "thread/settings/update", "review/start"]
        );
    }
}
