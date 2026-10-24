//! Manual worker recovery and tool release through the retained account owner.
use crate::account_session_runtime::{
    AccountSessionPort, ConnectionScope, ProviderRequest, ProviderResponse, RequestCertainty,
};
use crate::account_worker::retained::RequestActor;
use crate::managed_host_tools::{
    Actor, AgentAncestry, DeliveryCertainty, DispatchedOperation, Error, OperationResult,
    agent_ancestry,
};
use rusqlite::Transaction;
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_providers::Method;
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, EventWriteRepository, RuntimeStore, truthy,
};

const TOOL_MEMORY_LOSS: &str = "JavaScript code cells and MCP tool memory end when the native session closes. The transcript and receipts stay. No user input is replayed.";
/// Operation ID prefix reserved for an explicitly authorized native tool reset.
pub(crate) const MANUAL_RELEASE_PREFIX: &str = "manual-release:";

/// Authority captured by one explicit lead request. The account worker checks
/// this record again in the transaction that applies recovered state.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AuthorizedRecoveryRequest {
    pub lead: Value,
    pub target: Value,
    pub request_id: String,
    pub signature: String,
}

pub(crate) struct AuthorizedRecoveryEffects {
    pub store: crate::recovery_store::RecoveryStore,
    pub authorization: AuthorizedRecoveryRequest,
}
impl crate::recovery::RecoveryEffects for AuthorizedRecoveryEffects {
    fn apply<'a>(
        &'a mut self,
        expected: &'a crate::recovery_policy::RecoverySnapshot,
        scope: &'a ConnectionScope,
        action: crate::recovery::RecoveryAction,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<crate::recovery::RecoveryEffectResult, crate::recovery::RecoveryError>,
    > {
        self.store.apply_authorized(
            expected,
            scope,
            action,
            crate::recovery_store::RecoveryAdmission::ManualSettleOnly(self.authorization.clone()),
        )
    }
}

fn stable(record: &Value) -> Value {
    let mut record = record.clone();
    if let Some(fields) = record.as_object_mut() {
        fields.retain(|key, _| {
            !studio_runtime_store::agent_volatile::is_volatile_field(key)
                && key != "nativeRelease"
                && key != "connectionRecovery"
        });
    }
    record
}

fn authorize(tx: &Transaction<'_>, actor: &Value, target: &str) -> Result<(Value, Value), Error> {
    let lead_id = actor["id"].as_str().ok_or(Error::Stale)?;
    let lead = AgentRepository::get(tx, &AgentId::from(lead_id))?
        .ok_or(Error::Stale)?
        .value();
    if lead["epoch"] != actor["epoch"]
        || lead["isLead"] != true
        || lead["autoWake"] != true
        || truthy(lead.get("deletedAt"))
        || truthy(lead.get("deleted"))
        || truthy(lead.get("remoteOrigin"))
        || truthy(lead.get("executionMove"))
    {
        return Err(Error::Stale);
    }
    let worker = AgentRepository::get(tx, &AgentId::from(target))?
        .ok_or(Error::Invalid("Unknown managed agent"))?
        .value();
    if worker["isLead"] == true
        || target == lead_id
        || worker["rootId"] != lead["rootId"]
        || agent_ancestry(tx, lead_id, target)? != AgentAncestry::Descendant
        || truthy(worker.get("deletedAt"))
        || truthy(worker.get("deleted"))
        || truthy(worker.get("archivedAt"))
        || truthy(worker.get("archived"))
        || truthy(worker.get("remoteOrigin"))
        || truthy(worker.get("executionMove"))
    {
        return Err(Error::Invalid("Manage only a local live descendant worker"));
    }
    Ok((lead, worker))
}

impl AuthorizedRecoveryRequest {
    pub(crate) fn current(&self, tx: &Transaction<'_>) -> Result<bool, Error> {
        if EventWriteRepository::receipt(tx, &self.request_id)?
            .is_none_or(|receipt| receipt.signature != self.signature)
        {
            return Ok(false);
        }
        let Some(target) = self.target["id"].as_str() else {
            return Ok(false);
        };
        match authorize(tx, &self.lead, target) {
            Ok((lead, worker)) => {
                Ok(stable(&lead) == stable(&self.lead) && stable(&worker) == stable(&self.target))
            }
            Err(Error::Stale | Error::Invalid(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }
}

/// Assigned work and Studio monitors survive unsubscribe. Native work and
/// input with an unsettled outcome must keep their exact session.
fn blocker(tx: &Transaction<'_>, worker: &Value) -> Result<Option<&'static str>, Error> {
    if truthy(worker.get("inFlight"))
        || truthy(worker.get("turnId"))
        || matches!(
            worker["status"].as_str(),
            Some("queued" | "starting" | "running" | "approval")
        )
        || worker["activeTools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
        || truthy(worker.get("workspaceOperation"))
        || truthy(worker.get("accountTransferId"))
        || truthy(worker.get("nativeToolRefreshId"))
        || matches!(
            worker["contextRepair"]["phase"].as_str(),
            Some("preparing" | "submitted" | "unknown" | "ready")
        )
    {
        return Ok(Some(
            "An active turn or workspace operation holds this session",
        ));
    }
    let id = worker["id"].as_str().ok_or(Error::Stale)?;
    let open: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND (status IN ('reserved','dispatching','uncertain') OR (status='pending' AND epoch=?2)))", rusqlite::params![id, worker["epoch"].as_i64()], |row| row.get(0))?;
    if open {
        return Ok(Some("Pending or unknown input holds this session"));
    }
    for (table, predicate) in [
        (
            "runtime_tasks",
            "json_extract(record,'$.status') IN ('running','starting','pending','unknown')",
        ),
        (
            "runtime_requests",
            "json_extract(record,'$.status') IN ('pending','answering')",
        ),
        (
            "runtime_tool_requests",
            "json_extract(record,'$.stage') IN ('queued','running')",
        ),
    ] {
        let active: bool = tx.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE json_extract(record,'$.agent')=?1 AND ({predicate}))"), [id], |row| row.get(0))?;
        if active {
            return Ok(Some("An active native task or request holds this session"));
        }
    }
    Ok(None)
}

fn result(value: Value) -> DispatchedOperation {
    let uncertain = matches!(value["status"].as_str(), Some("unknown" | "unconfirmed"));
    let refused = matches!(
        value["status"].as_str(),
        Some("blocked" | "skipped" | "superseded" | "history_missing")
    );
    DispatchedOperation {
        result: OperationResult {
            value,
            signals: vec![],
        },
        certainty: if uncertain {
            DeliveryCertainty::Unknown
        } else {
            DeliveryCertainty::Written
        },
        applied: if uncertain { None } else { Some(!refused) },
    }
}

fn release_result(status: &str, reason: Option<&str>) -> Value {
    let mut value = json!({"status":status,"loss":TOOL_MEMORY_LOSS});
    if let Some(reason) = reason {
        value["reason"] = json!(reason);
    }
    value
}

fn read_saved(tx: &Transaction<'_>, id: &str, signature: &str) -> Result<Option<Value>, Error> {
    EventWriteRepository::receipt(tx, id)?
        .map(|receipt| {
            if receipt.signature != signature {
                return Err(Error::Invalid("This request id has different content"));
            }
            serde_json::from_str(&receipt.result)
                .map_err(|_| Error::Invalid("Invalid manual management receipt"))
        })
        .transpose()
}

fn save(tx: &Transaction<'_>, id: &str, signature: &str, value: &Value) -> Result<(), Error> {
    EventWriteRepository::replace_receipt(tx, id, signature, &value.to_string())?;
    Ok(())
}

fn put(tx: &Transaction<'_>, worker: Value) -> Result<(), Error> {
    AgentRepository::put(
        tx,
        &AgentRecord::from_value(worker).map_err(studio_runtime_store::StoreError::from)?,
    )?;
    Ok(())
}

pub(crate) async fn execute(
    store: Arc<RuntimeStore>,
    session: Arc<dyn AccountSessionPort>,
    actor: &Actor,
    args: &Value,
    request_id: &str,
    now: f64,
) -> Result<DispatchedOperation, Error> {
    let recovery = args["action"] == "recover";
    if !recovery && args["action"] != "reset_tools" {
        return Err(Error::Invalid("Unknown manual management action"));
    }
    let reason = if recovery {
        String::new()
    } else {
        args["reason"]
            .as_str()
            .map(str::trim)
            .filter(|reason| (1..=1000).contains(&reason.chars().count()))
            .ok_or(Error::Invalid(
                "Supply a reset reason with 1 to 1000 characters",
            ))?
            .to_owned()
    };
    if request_id.is_empty() || !now.is_finite() || now < 0.0 {
        return Err(Error::Invalid("Invalid manual management request"));
    }
    let actor = actor.record().value();
    let args = args.clone();
    let signature = crate::event_write::request_signature(
        &json!({"actor":actor["id"],"epoch":actor["epoch"],"args":args}),
    );
    let receipt_id = format!(
        "manual-management:{}:{}:{request_id}",
        actor["id"].as_str().ok_or(Error::Stale)?,
        actor["epoch"]
    );
    let id = receipt_id.clone();
    let sig = signature.clone();
    let initial = store.transaction_with(move |tx| {
        let resolved = crate::provider_callbacks::tools::services::manage::resolved_args(tx, &actor, &args).map_err(|_| Error::Invalid("Unknown managed agent"))?;
        let target = resolved["agent_id"].as_str().ok_or(Error::Invalid("Supply agent_id"))?;
        let (lead, mut worker) = authorize(tx, &actor, target)?;
        if let Some(saved) = read_saved(tx, &id, &sig)? { return Ok((None, saved)); }
        let auth = AuthorizedRecoveryRequest { lead: stable(&lead), target: stable(&worker), request_id: id.clone(), signature: sig.clone() };
        if recovery {
            let pending = json!({"status":"unconfirmed","next":"Inspect the worker. Do not replay input with an unknown outcome."});
            save(tx, &id, &sig, &pending)?;
            return Ok((Some((auth, Value::Null)), pending));
        }
        if worker["provider"].as_str().unwrap_or("codex") != "codex" || worker["executionMode"].as_str().is_some_and(|mode| mode != "native") {
            return Err(Error::Unavailable("native-release/local-codex-session"));
        }
        let previous = &worker["nativeRelease"];
        if truthy(previous.get("resetPending")) || (previous["closedAt"].as_f64().is_none() && matches!(previous["phase"].as_str(), Some("unknown" | "unsubscribing" | "checking"))) {
            let value = release_result("unknown", Some("The previous release still needs exact native closure"));
            save(tx, &id, &sig, &value)?;
            return Ok((None, value));
        }
        if let Some(reason) = blocker(tx, &worker)? {
            let value = release_result("blocked", Some(reason));
            save(tx, &id, &sig, &value)?;
            return Ok((None, value));
        }
        if !worker["threadId"].as_str().is_some_and(|thread| !thread.is_empty()) { return Err(Error::Invalid("The worker has no native thread")); }
        let operation = format!("{MANUAL_RELEASE_PREFIX}{}", studio_runtime_store::sha256_hex(id.as_bytes()));
        worker["nativeRelease"] = json!({"id":operation,"phase":"checking","at":now,"threadId":worker["threadId"],"accountKey":worker["accountKey"].as_str().unwrap_or("default"),"targetEpoch":worker["epoch"],"targetRootId":worker["rootId"],"targetParentId":worker["parentId"],"resetPending":true,"resetReason":reason,"resetBy":auth.lead["id"],"resetActorEpoch":auth.lead["epoch"],"authorization":auth});
        put(tx, worker.clone())?;
        let pending = release_result("unknown", Some("The manual release has not returned a result"));
        save(tx, &id, &sig, &pending)?;
        Ok((Some((auth, worker["nativeRelease"].clone())), pending))
    }).await?;
    let Some((auth, mut release)) = initial.0 else {
        return Ok(result(initial.1));
    };
    if recovery {
        let account = auth.target["accountKey"].as_str().unwrap_or("default");
        let value = match session.recover_managed_agent(account, auth.clone()).await {
            Ok(value) => value,
            // The account worker still owns a possible late result. A lost
            // transport response must not finalize its pending receipt.
            Err(_) => return Ok(result(initial.1)),
        };
        return finish_recovery(&store, &auth, value).await.map(result);
    }
    let account = release["accountKey"]
        .as_str()
        .ok_or(Error::Stale)?
        .to_owned();
    let Some(scope) = session.current_scope(&account) else {
        return finish_release(
            &store,
            &auth,
            &release,
            Arc::clone(&session),
            release_result("blocked", Some("The owning account is offline")),
            false,
            false,
        )
        .await;
    };
    release["scope"] = serde_json::to_value(&scope).map_err(|_| Error::Stale)?;
    release["connectionId"] = json!(scope.connection);
    let frozen = release.clone();
    let frozen_auth = auth.clone();
    store
        .transaction_with(move |tx| {
            if !frozen_auth.current(tx)? {
                return Err(Error::Stale);
            }
            let target = frozen_auth.target["id"].as_str().ok_or(Error::Stale)?;
            let mut worker = AgentRepository::get(tx, &AgentId::from(target))?
                .ok_or(Error::Stale)?
                .value();
            if worker["nativeRelease"]["id"] != frozen["id"]
                || worker["nativeRelease"]["phase"] != "checking"
            {
                return Err(Error::Stale);
            }
            worker["nativeRelease"] = frozen;
            put(tx, worker)
        })
        .await?;
    let inspection = inspect(session.as_ref(), &auth, &release, &scope).await;
    let unloaded = match inspection {
        Ok(value) => value,
        Err(reason) => {
            return finish_release(
                &store,
                &auth,
                &release,
                Arc::clone(&session),
                release_result("blocked", Some(reason)),
                false,
                false,
            )
            .await;
        }
    };
    if unloaded {
        return finish_release(
            &store,
            &auth,
            &release,
            Arc::clone(&session),
            release_result("released", None),
            false,
            true,
        )
        .await;
    }
    let auth_before = auth.clone();
    let release_before = release.clone();
    let frozen_scope = scope.clone();
    let admission = store
        .transaction_with(move |tx| {
            if !auth_before.current(tx)? {
                return Err(Error::Stale);
            }
            let target = auth_before.target["id"].as_str().ok_or(Error::Stale)?;
            let mut worker = AgentRepository::get(tx, &AgentId::from(target))?
                .ok_or(Error::Stale)?
                .value();
            if worker["nativeRelease"]["id"] != release_before["id"]
                || worker["nativeRelease"]["phase"] != "checking"
                || blocker(tx, &worker)?.is_some()
            {
                return Err(Error::Stale);
            }
            worker["nativeRelease"]["phase"] = json!("unsubscribing");
            worker["nativeRelease"]["submittedAt"] = json!(now);
            worker["nativeRelease"]["scope"] =
                serde_json::to_value(frozen_scope).map_err(|_| Error::Stale)?;
            put(tx, worker)
        })
        .await;
    if admission.is_err() {
        return finish_release(
            &store,
            &auth,
            &release,
            Arc::clone(&session),
            release_result(
                "blocked",
                Some("The lead or worker changed during inspection"),
            ),
            false,
            false,
        )
        .await;
    }
    let outcome = request(
        session.as_ref(),
        &auth,
        &release,
        &scope,
        Method::ThreadUnsubscribe,
        "unsubscribe",
        json!({"threadId":release["threadId"]}),
    )
    .await;
    let (status, closed) = match outcome {
        Ok(body)
            if matches!(
                body["status"].as_str(),
                Some("unsubscribed" | "notSubscribed" | "notLoaded")
            ) =>
        {
            ("released", body["status"] == "notLoaded")
        }
        _ => ("unknown", false),
    };
    finish_release(
        &store,
        &auth,
        &release,
        Arc::clone(&session),
        release_result(status, None),
        true,
        closed,
    )
    .await
}

pub(crate) async fn finish_recovery(
    store: &RuntimeStore,
    auth: &AuthorizedRecoveryRequest,
    value: Value,
) -> Result<Value, Error> {
    let auth = auth.clone();
    store
        .transaction_with(move |tx| {
            let saved = read_saved(tx, &auth.request_id, &auth.signature)?.ok_or(Error::Stale)?;
            // The account worker can finish after the callback caller disappears.
            // Its committed result, not a retry's transport state, remains the receipt.
            if saved["manualRecoveryComplete"] == true {
                return Ok(saved);
            }
            let mut value = if value.is_object() {
                value
            } else {
                json!({"status":"unconfirmed"})
            };
            value["manualRecoveryComplete"] = json!(true);
            save(tx, &auth.request_id, &auth.signature, &value)?;
            Ok(value)
        })
        .await
}

async fn request(
    session: &dyn AccountSessionPort,
    auth: &AuthorizedRecoveryRequest,
    release: &Value,
    scope: &ConnectionScope,
    method: Method,
    suffix: &str,
    params: Value,
) -> Result<Value, &'static str> {
    let operation = format!(
        "{}:{suffix}",
        release["id"].as_str().ok_or("Invalid release identity")?
    );
    let receipt = session
        .provider_request(ProviderRequest {
            account: scope.account.clone(),
            operation: operation.clone(),
            actor: Some(RequestActor {
                agent: auth.target["id"]
                    .as_str()
                    .ok_or("Invalid worker identity")?
                    .into(),
                epoch: auth.target["epoch"]
                    .as_i64()
                    .ok_or("Invalid worker epoch")?,
            }),
            expected_scope: Some(scope.clone()),
            deleted_actor_session: None,
            method,
            params,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(10),
        })
        .await
        .map_err(|_| "The native result is not confirmed")?;
    if receipt.operation != operation
        || receipt.scope != *scope
        || receipt.certainty != RequestCertainty::Written
        || !receipt
            .ticket
            .is_some_and(|ticket| ticket.method == method && ticket.generation == scope.generation)
        || session.current_scope(&scope.account).as_ref() != Some(scope)
    {
        return Err("The native result is not confirmed");
    }
    match receipt.result {
        Some(ProviderResponse::Reply(value)) => Ok(value),
        _ => Err("The native result is not confirmed"),
    }
}

async fn inspect(
    session: &dyn AccountSessionPort,
    auth: &AuthorizedRecoveryRequest,
    release: &Value,
    scope: &ConnectionScope,
) -> Result<bool, &'static str> {
    let thread = &release["threadId"];
    let body = request(
        session,
        auth,
        release,
        scope,
        Method::ThreadRead,
        "thread",
        json!({"threadId":thread,"includeTurns":false}),
    )
    .await?;
    if body["thread"]["id"] != *thread {
        return Err("The native thread identity changed");
    }
    match body["thread"]["status"]["type"].as_str() {
        Some("notLoaded") => return Ok(true),
        Some("idle") if !truthy(body["thread"]["status"].get("activeFlags")) => {}
        _ => return Err("The native thread is active or its status is unknown"),
    }
    for (method, suffix, params) in [
        (
            Method::ThreadBackgroundTerminalsList,
            "terminals",
            json!({"threadId":thread,"limit":1}),
        ),
        (Method::ThreadQueueList, "queue", json!({"threadId":thread})),
    ] {
        let body = request(session, auth, release, scope, method, suffix, params).await?;
        if !body["data"].as_array().is_some_and(Vec::is_empty) || truthy(body.get("nextCursor")) {
            return Err("Native commands or input still hold this session");
        }
    }
    session
        .callback_barrier(
            &scope.account,
            scope,
            std::time::Instant::now() + std::time::Duration::from_secs(10),
        )
        .await
        .map_err(|_| "The native notification barrier is not confirmed")?;
    Ok(false)
}

async fn finish_release(
    store: &RuntimeStore,
    auth: &AuthorizedRecoveryRequest,
    release: &Value,
    session: Arc<dyn AccountSessionPort>,
    mut value: Value,
    submitted: bool,
    closed: bool,
) -> Result<DispatchedOperation, Error> {
    let auth = auth.clone();
    let release = release.clone();
    let now = release["at"].as_f64().ok_or(Error::Stale)?;
    let value = store
        .transaction_with(move |tx| {
            let target = auth.target["id"].as_str().ok_or(Error::Stale)?;
            let mut worker = AgentRepository::get(tx, &AgentId::from(target))?
                .ok_or(Error::Stale)?
                .value();
            if worker["nativeRelease"]["id"] != release["id"] {
                return Err(Error::Stale);
            }
            let current = auth.current(tx)?
                && (release["scope"].is_null()
                    || serde_json::from_value::<ConnectionScope>(release["scope"].clone())
                        .ok()
                        .is_some_and(|scope| {
                            session.current_scope(&scope.account).as_ref() == Some(&scope)
                        }));
            if !current {
                value = release_result(
                    if submitted { "unknown" } else { "blocked" },
                    Some("The lead or worker identity changed"),
                );
            }
            let closed = closed && current;
            let confirmed_close = closed || worker["nativeRelease"]["closedAt"].as_f64().is_some();
            worker["nativeRelease"]["phase"] = value["status"].clone();
            worker["nativeRelease"]["resetPending"] = json!(submitted && !confirmed_close);
            if closed {
                worker["nativeRelease"]["closedAt"] = json!(now);
            }
            put(tx, worker)?;
            save(tx, &auth.request_id, &auth.signature, &value)?;
            Ok(value)
        })
        .await?;
    Ok(result(value))
}

/// The retained account owner calls this immediately before native submission.
/// A plain actor-bound unsubscribe has no authority without this durable intent.
pub(crate) fn authorize_release(
    tx: &Transaction<'_>,
    actor: &RequestActor,
    scope: &ConnectionScope,
    operation: &str,
    params: &Value,
) -> Result<bool, Error> {
    let Some(worker) = AgentRepository::get(tx, &AgentId::from(actor.agent.as_str()))? else {
        return Ok(false);
    };
    let worker = worker.value();
    let release = &worker["nativeRelease"];
    let Ok(auth) =
        serde_json::from_value::<AuthorizedRecoveryRequest>(release["authorization"].clone())
    else {
        return Ok(false);
    };
    Ok(auth.target["id"] == actor.agent
        && worker["epoch"].as_i64() == Some(actor.epoch)
        && auth.current(tx)?
        && blocker(tx, &worker)?.is_none()
        && release["phase"] == "unsubscribing"
        && release["resetPending"] == true
        && release["scope"] == serde_json::to_value(scope).map_err(|_| Error::Stale)?
        && release["id"]
            .as_str()
            .is_some_and(|id| operation == format!("{id}:unsubscribe"))
        && params == &json!({"threadId":worker["threadId"]}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_session_runtime::{
        AccountSessionFuture, ConnectionScope, ProviderRequest, ProviderResponse, RequestCertainty,
        SessionReceipt,
    };
    use serde_json::json;
    use std::sync::Mutex;
    use studio_runtime_providers::{Method, RequestId, RequestTicket};
    use studio_runtime_store::{AgentId, AgentRecord, AgentRepository};

    struct Session {
        store: Arc<RuntimeStore>,
        calls: Mutex<Vec<(Method, String)>>,
        unknown: bool,
        unloaded: bool,
        stop_lead: bool,
        change_worker_epoch: bool,
        close_before_reply: bool,
        recover_unknown: bool,
        terminal: bool,
        recoveries: Mutex<Vec<AuthorizedRecoveryRequest>>,
    }
    impl Session {
        fn scope() -> ConnectionScope {
            ConnectionScope {
                account: "default".into(),
                connection: "connection".into(),
                generation: 1,
            }
        }
        fn unsubscribes(&self) -> usize {
            self.calls
                .lock()
                .expect("calls")
                .iter()
                .filter(|(method, _)| *method == Method::ThreadUnsubscribe)
                .count()
        }
    }
    impl AccountSessionPort for Session {
        fn recover_managed_agent<'a>(
            &'a self,
            _: &'a str,
            request: AuthorizedRecoveryRequest,
        ) -> AccountSessionFuture<'a, Value> {
            self.recoveries.lock().expect("recoveries").push(request);
            Box::pin(async move {
                if self.recover_unknown {
                    Err(crate::account_session_runtime::AccountSessionError::Unknown)
                } else {
                    Ok(json!({"status":"unconfirmed"}))
                }
            })
        }
        fn ensure_session<'a>(&'a self, _: &'a str) -> AccountSessionFuture<'a, ConnectionScope> {
            Box::pin(async { Ok(Self::scope()) })
        }
        fn current_scope(&self, _: &str) -> Option<ConnectionScope> {
            Some(Self::scope())
        }
        fn callback_barrier<'a>(
            &'a self,
            _: &'a str,
            _: &'a ConnectionScope,
            _: std::time::Instant,
        ) -> AccountSessionFuture<'a, ()> {
            Box::pin(async { Ok(()) })
        }
        fn provider_request<'a>(
            &'a self,
            request: ProviderRequest,
        ) -> AccountSessionFuture<'a, SessionReceipt> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .expect("calls")
                    .push((request.method, request.operation.clone()));
                if request.method == Method::ThreadUnsubscribe {
                    let actor = request.actor.clone().expect("worker identity");
                    let operation = request.operation.clone();
                    let params = request.params.clone();
                    self.store
                        .transaction_with(move |tx| {
                            assert!(authorize_release(
                                tx,
                                &actor,
                                &Self::scope(),
                                &operation,
                                &params
                            )?);
                            assert!(!authorize_release(
                                tx,
                                &actor,
                                &Self::scope(),
                                "manual-release:another:unsubscribe",
                                &params
                            )?);
                            let mut other_scope = Self::scope();
                            other_scope.generation += 1;
                            assert!(!authorize_release(
                                tx,
                                &actor,
                                &other_scope,
                                &operation,
                                &params
                            )?);
                            assert!(!authorize_release(
                                tx,
                                &actor,
                                &Self::scope(),
                                &operation,
                                &json!({"threadId":"another-thread"})
                            )?);
                            Ok::<_, Error>(())
                        })
                        .await
                        .expect("retained release authority");
                    if self.close_before_reply {
                        self.store
                            .transaction(|tx| {
                                let mut worker =
                                    AgentRepository::get(tx, &AgentId::from("worker-00000000"))?
                                        .expect("worker");
                                let release =
                                    worker.extra.get_mut("nativeRelease").expect("release");
                                release["closedAt"] = json!(11.0);
                                release["resetPending"] = json!(false);
                                AgentRepository::put(tx, &worker)
                            })
                            .await
                            .expect("exact native close before reply");
                    }
                }
                if self.change_worker_epoch && request.method == Method::ThreadQueueList {
                    self.store
                        .transaction(|tx| {
                            let mut worker =
                                AgentRepository::get(tx, &AgentId::from("worker-00000000"))?
                                    .expect("worker");
                            worker.epoch += 1;
                            AgentRepository::put(tx, &worker)
                        })
                        .await
                        .expect("worker epoch changes during inspection");
                }
                if self.stop_lead && request.method == Method::ThreadQueueList {
                    self.store
                        .transaction(|tx| {
                            let mut lead =
                                AgentRepository::get(tx, &AgentId::from("lead-00000000"))?
                                    .expect("lead");
                            lead.epoch += 1;
                            lead.extra.insert("autoWake".into(), json!(false));
                            AgentRepository::put(tx, &lead)
                        })
                        .await
                        .expect("stop lead");
                }
                let body = match request.method {
                    Method::ThreadRead => {
                        json!({"thread":{"id":"worker-thread","status":{"type":if self.unloaded {"notLoaded"} else {"idle"}}}})
                    }
                    Method::ThreadBackgroundTerminalsList if self.terminal => {
                        json!({"data":[{"id":"live-command"}],"nextCursor":null})
                    }
                    Method::ThreadBackgroundTerminalsList | Method::ThreadQueueList => {
                        json!({"data":[],"nextCursor":null})
                    }
                    Method::ThreadUnsubscribe => json!({"status":"unsubscribed"}),
                    method => panic!("unexpected native mutation or read: {method:?}"),
                };
                let unknown = self.unknown && request.method == Method::ThreadUnsubscribe;
                Ok(SessionReceipt {
                    scope: Self::scope(),
                    operation: request.operation.clone(),
                    ticket: Some(RequestTicket {
                        id: RequestId::String(request.operation),
                        generation: 1,
                        method: request.method,
                    }),
                    certainty: if unknown {
                        RequestCertainty::Unknown
                    } else {
                        RequestCertainty::Written
                    },
                    result: (!unknown).then_some(ProviderResponse::Reply(body)),
                    not_sent_reason: None,
                    replayed: false,
                })
            })
        }
    }
    async fn fixture() -> (tempfile::TempDir, Arc<RuntimeStore>, Actor, Session) {
        let directory = tempfile::tempdir().expect("temporary state");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("store"),
        );
        let lead = json!({"id":"lead-00000000","rootId":"lead-00000000","parentId":null,"epoch":1,
            "status":"running","isLead":true,"autoWake":true,"inFlight":true,"threadId":"lead-thread",
            "turnId":"lead-turn","accountKey":"default","provider":"codex"});
        let worker = json!({"id":"worker-00000000","rootId":"lead-00000000","parentId":"lead-00000000","epoch":3,
            "status":"completed","isLead":false,"autoWake":true,"inFlight":false,"threadId":"worker-thread",
            "turnId":null,"accountKey":"default","provider":"codex","executionMode":"native","activeTools":[]});
        let actor = Actor::from_record(&lead, "lead-00000000", 1).expect("actor");
        store
            .transaction(move |tx| {
                for value in [lead, worker] {
                    AgentRepository::put(tx, &AgentRecord::from_value(value)?)?;
                }
                Ok(())
            })
            .await
            .expect("seed team");
        let session = Session {
            store: Arc::clone(&store),
            calls: Mutex::new(Vec::new()),
            unknown: false,
            unloaded: false,
            stop_lead: false,
            change_worker_epoch: false,
            close_before_reply: false,
            recover_unknown: false,
            terminal: false,
            recoveries: Mutex::new(vec![]),
        };
        (directory, store, actor, session)
    }
    fn args() -> Value {
        json!({"action":"reset_tools","agent_id":"worker-00000000","reason":"Release stale tool memory"})
    }
    async fn worker(store: &RuntimeStore) -> Value {
        store
            .read_async("manual-recovery-test", |db| {
                Ok::<_, studio_runtime_store::StoreError>(
                    AgentRepository::get(db, &AgentId::from("worker-00000000"))?
                        .expect("worker")
                        .value(),
                )
            })
            .await
            .expect("worker read")
    }

    #[tokio::test]
    async fn a_stopped_lead_cannot_apply_a_native_recovery_result() {
        use crate::recovery::{
            RecoveryAction, RecoveryEffectResult, RecoveryError, RecoveryOutcome,
        };
        use crate::recovery_policy::RecoverySnapshot;
        use crate::recovery_store::{CompletionWriter, RecoveryStore};
        struct NoCompletion;
        impl CompletionWriter for NoCompletion {
            fn start_held(
                &mut self,
                _: &Transaction<'_>,
                _: &Value,
                _: &str,
                _: &str,
            ) -> Result<(), RecoveryError> {
                panic!("no input change")
            }
            fn complete(
                &mut self,
                _: &Transaction<'_>,
                _: &RecoverySnapshot,
                _: &ConnectionScope,
                _: &RecoveryAction,
            ) -> Result<RecoveryEffectResult, RecoveryError> {
                panic!("no completion")
            }
        }
        let (_directory, store, actor, _session) = fixture().await;
        let (auth, snapshot) = store
            .transaction_with(move |tx| {
                let (lead, target) = authorize(tx, &actor.record().value(), "worker-00000000")?;
                let raw = AgentRepository::get_raw(tx, &AgentId::from("worker-00000000"))?
                    .expect("raw worker");
                save(tx, "guard-test", "test", &json!({"status":"unconfirmed"}))?;
                Ok::<_, Error>((
                    AuthorizedRecoveryRequest {
                        lead,
                        target,
                        request_id: "guard-test".into(),
                        signature: "test".into(),
                    },
                    RecoverySnapshot::from_record(serde_json::from_str(&raw).expect("worker JSON"))
                        .expect("snapshot"),
                ))
            })
            .await
            .expect("capture admission");
        store
            .transaction(|tx| {
                let mut lead =
                    AgentRepository::get(tx, &AgentId::from("lead-00000000"))?.expect("lead");
                lead.epoch += 1;
                lead.extra.insert("autoWake".into(), json!(false));
                AgentRepository::put(tx, &lead)
            })
            .await
            .expect("stop lead");
        let mut effects = RecoveryStore {
            store: Arc::clone(&store),
            connection_current: Arc::new(|_| true),
            completion: Arc::new(Mutex::new(Box::new(NoCompletion))),
            at: 10.0,
            observation_current: Arc::new(|_| None),
        };
        let applied = effects
            .apply_authorized(
                &snapshot,
                &Session::scope(),
                RecoveryAction::RecordCheck {
                    native_state: json!("idle"),
                    turn_status: None,
                },
                crate::recovery_store::RecoveryAdmission::ManualSettleOnly(auth),
            )
            .await
            .expect("guard result");
        assert_eq!(applied.outcome, RecoveryOutcome::Superseded);
        assert!(worker(&store).await["connectionCheck"].is_null());
    }

    #[tokio::test]
    async fn a_new_reset_inspects_a_resumed_thread_instead_of_reusing_old_closure() {
        let (_directory, store, actor, session) = fixture().await;
        store.transaction(|tx| {
            let mut worker = AgentRepository::get(tx, &AgentId::from("worker-00000000"))?.expect("worker");
            worker.extra.insert("nativeRelease".into(), json!({"id":"old-release","phase":"resumed","closedAt":5.0,"resetPending":false,"targetEpoch":3,"threadId":"worker-thread","accountKey":"default"}));
            AgentRepository::put(tx, &worker)
        }).await.expect("native close followed by resume");
        let session = Arc::new(session);
        let checked = execute(store, session.clone(), &actor, &args(), "new-reset", 10.0)
            .await
            .expect("new reset");
        assert_eq!(checked.result.value["status"], "released");
        assert_eq!(
            session.unsubscribes(),
            1,
            "the old session closure does not release the resumed session"
        );
    }

    #[tokio::test]
    async fn a_lost_recovery_response_does_not_overwrite_the_workers_late_result() {
        let (_directory, store, actor, mut session) = fixture().await;
        session.recover_unknown = true;
        let session = Arc::new(session);
        let args = json!({"action":"recover","agent_id":"worker-00000000"});
        let lost = execute(
            Arc::clone(&store),
            session.clone(),
            &actor,
            &args,
            "late-recovery",
            10.0,
        )
        .await
        .expect("unknown response");
        assert_eq!(lost.result.value["status"], "unconfirmed");
        let auth = session.recoveries.lock().expect("recoveries")[0].clone();
        finish_recovery(
            &store,
            &auth,
            json!({"status":"checked","evidence":"exact native result"}),
        )
        .await
        .expect("worker finishes after response loss");
        let replay = execute(store, session.clone(), &actor, &args, "late-recovery", 11.0)
            .await
            .expect("durable late result");
        assert_eq!(replay.result.value["status"], "checked");
        assert_eq!(session.recoveries.lock().expect("recoveries").len(), 1);
        assert!(session.calls.lock().expect("native calls").is_empty());
    }

    #[tokio::test]
    async fn manual_recovery_keeps_uncertain_input_and_reuses_the_request() {
        let (_directory, store, actor, session) = fixture().await;
        store.transaction(|tx| {
            tx.execute("INSERT INTO runtime_events(id,agent,epoch,kind,text,status,created) VALUES('unknown-input','worker-00000000',3,'followup','original input','uncertain',1)", [])?;
            Ok(())
        }).await.expect("uncertain input");
        let session = Arc::new(session);
        let args =
            json!({"action":"recover","agent_id":"worker-00000000","request_id":"exact-recovery"});
        for _ in 0..2 {
            let checked = execute(
                Arc::clone(&store),
                session.clone(),
                &actor,
                &args,
                "exact-recovery",
                10.0,
            )
            .await
            .expect("manual recovery");
            assert_eq!(checked.result.value["status"], "unconfirmed");
        }
        assert_eq!(session.recoveries.lock().expect("recoveries").len(), 1);
        assert!(session.calls.lock().expect("native calls").is_empty());
        let original = store
            .read_async("manual-recovery-test", |db| {
                db.query_row(
                    "SELECT text,status FROM runtime_events WHERE id='unknown-input'",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .map_err(studio_runtime_store::StoreError::from)
            })
            .await
            .expect("original input");
        assert_eq!(original, ("original input".into(), "uncertain".into()));
    }

    #[tokio::test]
    async fn manual_tool_reset_unsubscribes_once_and_reports_unconfirmed_closure() {
        let (_directory, store, actor, session) = fixture().await;
        let session = Arc::new(session);
        let first = execute(
            Arc::clone(&store),
            session.clone(),
            &actor,
            &args(),
            "reset-1",
            10.0,
        )
        .await
        .expect("manual reset");
        assert_eq!(first.result.value["status"], "released");
        assert!(
            first.result.value["loss"]
                .as_str()
                .expect("loss warning")
                .contains("JavaScript")
        );
        assert!(
            first.result.value["loss"]
                .as_str()
                .expect("loss warning")
                .contains("MCP")
        );
        let current = worker(&store).await;
        assert_eq!(current["nativeRelease"]["resetPending"], true);
        assert!(
            current["nativeRelease"]["closedAt"].is_null(),
            "unsubscribe is not closure"
        );
        let replay = execute(
            Arc::clone(&store),
            session.clone(),
            &actor,
            &args(),
            "reset-1",
            11.0,
        )
        .await
        .expect("lost response replay");
        assert_eq!(first.result.value, replay.result.value);
        assert_eq!(session.unsubscribes(), 1);
        let mut changed = args();
        changed["reason"] = json!("Changed request content");
        assert!(
            execute(store, session.clone(), &actor, &changed, "reset-1", 12.0)
                .await
                .is_err()
        );
        assert_eq!(session.unsubscribes(), 1);
    }

    #[tokio::test]
    async fn an_unknown_manual_tool_reset_is_not_sent_again() {
        let (_directory, store, actor, mut session) = fixture().await;
        session.unknown = true;
        let session = Arc::new(session);
        let first = execute(
            Arc::clone(&store),
            session.clone(),
            &actor,
            &args(),
            "unknown-1",
            10.0,
        )
        .await
        .expect("unknown result");
        assert_eq!(
            first.certainty,
            crate::managed_host_tools::DeliveryCertainty::Unknown
        );
        assert_eq!(first.result.value["status"], "unknown");
        for request in ["unknown-1", "unknown-2"] {
            let result = execute(
                Arc::clone(&store),
                session.clone(),
                &actor,
                &args(),
                request,
                11.0,
            )
            .await
            .expect("inspect uncertain release");
            assert_eq!(result.result.value["status"], "unknown");
        }
        assert_eq!(session.unsubscribes(), 1);
        assert_eq!(worker(&store).await["nativeRelease"]["resetPending"], true);
    }

    #[tokio::test]
    async fn manual_tool_reset_rechecks_the_lead_after_native_reads() {
        let (_directory, store, actor, mut session) = fixture().await;
        session.stop_lead = true;
        let session = Arc::new(session);
        let result = execute(
            Arc::clone(&store),
            session.clone(),
            &actor,
            &args(),
            "stopped-lead",
            10.0,
        )
        .await;
        assert!(result.is_err() || result.expect("result").result.value["status"] == "blocked");
        assert_eq!(session.unsubscribes(), 0);
    }

    #[tokio::test]
    async fn manual_tool_reset_refuses_a_changed_worker_epoch_after_inspection() {
        let (_directory, store, actor, mut session) = fixture().await;
        session.change_worker_epoch = true;
        let session = Arc::new(session);
        let checked = execute(
            store,
            session.clone(),
            &actor,
            &args(),
            "changed-worker",
            10.0,
        )
        .await
        .expect("refused reset");
        assert_eq!(checked.result.value["status"], "blocked");
        assert_eq!(session.unsubscribes(), 0);
    }

    #[tokio::test]
    async fn manual_tool_reset_refuses_vm_and_claude_sessions() {
        for (provider, mode) in [("codex", "vm"), ("claude", "native")] {
            let (_directory, store, actor, session) = fixture().await;
            store
                .transaction(move |tx| {
                    let mut worker = AgentRepository::get(tx, &AgentId::from("worker-00000000"))?
                        .expect("worker");
                    worker.extra.insert("provider".into(), json!(provider));
                    worker.extra.insert("executionMode".into(), json!(mode));
                    AgentRepository::put(tx, &worker)
                })
                .await
                .expect("provider fixture");
            let session = Arc::new(session);
            assert!(matches!(
                execute(
                    store,
                    session.clone(),
                    &actor,
                    &args(),
                    "unsupported-reset",
                    10.0
                )
                .await,
                Err(Error::Unavailable("native-release/local-codex-session"))
            ));
            assert!(session.calls.lock().expect("native calls").is_empty());
        }
    }

    #[tokio::test]
    async fn a_native_close_before_unsubscribe_reply_keeps_the_reset_hold_clear() {
        let (_directory, store, actor, mut session) = fixture().await;
        session.close_before_reply = true;
        let session = Arc::new(session);
        let checked = execute(
            Arc::clone(&store),
            session.clone(),
            &actor,
            &args(),
            "close-first",
            10.0,
        )
        .await
        .expect("unsubscribe result");
        assert_eq!(checked.result.value["status"], "released");
        let current = worker(&store).await;
        assert_eq!(current["nativeRelease"]["resetPending"], false);
        assert_eq!(current["nativeRelease"]["closedAt"], 11.0);
    }

    #[tokio::test]
    async fn manual_tool_reset_keeps_native_commands_and_uncertain_input() {
        for native_command in [false, true] {
            let (_directory, store, actor, mut session) = fixture().await;
            session.terminal = native_command;
            if !native_command {
                store.transaction(|tx| {
                    tx.execute("INSERT INTO runtime_events(id,agent,epoch,kind,text,status,created) VALUES('unknown-input','worker-00000000',3,'followup','original input','uncertain',1)", [])?;
                    Ok(())
                }).await.expect("uncertain original input");
            }
            let session = Arc::new(session);
            let result = execute(
                Arc::clone(&store),
                session.clone(),
                &actor,
                &args(),
                "blocked-reset",
                10.0,
            )
            .await
            .expect("blocked reset");
            assert_eq!(result.result.value["status"], "blocked");
            assert_eq!(session.unsubscribes(), 0);
        }
    }

    #[tokio::test]
    async fn a_confirmed_native_closure_needs_no_unsubscribe() {
        let (_directory, store, actor, mut session) = fixture().await;
        session.unloaded = true;
        let session = Arc::new(session);
        let result = execute(
            Arc::clone(&store),
            session.clone(),
            &actor,
            &args(),
            "already-closed",
            10.0,
        )
        .await
        .expect("confirmed closure");
        assert_eq!(result.result.value["status"], "released");
        let current = worker(&store).await;
        assert_eq!(current["nativeRelease"]["resetPending"], false);
        assert_eq!(current["nativeRelease"]["closedAt"], 10.0);
        assert_eq!(session.unsubscribes(), 0);
    }
}
