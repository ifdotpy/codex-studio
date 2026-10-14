//! Production monitor adapter over the retained account worker session.
//!
//! Native command intents are committed by P15 before this adapter calls the
//! broker. The broker owns request receipts; this layer maps their certainty and
//! result into P15 receipts and never retries `Unknown`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use studio_runtime_providers::Method;
use studio_runtime_services::account_session_runtime::{
    AccountSessionError, AccountSessionPort, NotSentReason as BrokerNotSentReason, ProviderRequest,
    ProviderResponse, RequestCertainty, SessionReceipt,
};
use studio_runtime_services::account_worker::retained::RequestActor;
use studio_runtime_services::monitor_driver::{MonitorNative, NotSentReason, Submission};
use studio_runtime_services::monitor_event_queue;
use studio_runtime_services::monitor_policy::{
    Effect, MonitorError, MonitorSnapshot, Operation, ReattachProof,
};
use studio_runtime_services::monitor_runtime_wiring::{Delivery, InputReply, MonitorPort};
use studio_runtime_services::rules_engine::RuleCheck;
use studio_runtime_services::rules_tick::{
    CheckReconciliation, CheckSubmission, RuleMonitor, RulesTickError,
};
use studio_runtime_store::RuntimeStore;

pub(crate) struct RetainedMonitorPort {
    store: Arc<RuntimeStore>,
    session: Arc<dyn AccountSessionPort>,
    input_replies: Mutex<BTreeMap<String, InputReply>>,
}

impl RetainedMonitorPort {
    pub(crate) fn new(store: Arc<RuntimeStore>, session: Arc<dyn AccountSessionPort>) -> Self {
        Self {
            store,
            session,
            input_replies: Mutex::new(BTreeMap::new()),
        }
    }

    fn request(
        &self,
        operation: &Operation,
        request_operation: String,
        method: Method,
        params: Value,
        deadline: Instant,
        actor: bool,
    ) -> Result<SessionReceipt, AccountSessionError> {
        let scope = self
            .session
            .current_scope(&operation.account_key)
            .ok_or(AccountSessionError::Refused("No live provider session"))?;
        if scope.account != operation.account_key {
            return Err(AccountSessionError::Refused(
                "Provider account scope changed",
            ));
        }
        Ok(self.session.provider_request(ProviderRequest {
            account: operation.account_key.clone(),
            operation: request_operation,
            actor: actor.then(|| RequestActor {
                agent: operation.agent.clone(),
                epoch: operation.epoch,
            }),
            expected_scope: Some(scope),
            method,
            params,
            deadline,
        })?)
    }

    fn submission(receipt: SessionReceipt, operation: &str) -> Submission {
        if receipt.operation != operation {
            return Submission::Unknown;
        }
        match receipt.certainty {
            RequestCertainty::Written => Submission::Written,
            RequestCertainty::Unknown => Submission::Unknown,
            RequestCertainty::NotSent => Submission::NotSent {
                reason: match receipt.not_sent_reason {
                    Some(BrokerNotSentReason::NoLiveAccountWorker)
                    | Some(BrokerNotSentReason::ExpectedScopeMismatch) => {
                        NotSentReason::SessionMissing
                    }
                    _ => NotSentReason::Transient,
                },
                error: "Provider proves the monitor request was not sent".into(),
            },
        }
    }
}

impl MonitorNative for RetainedMonitorPort {
    fn submit(&self, operation: &Operation, effect: &Effect) -> Submission {
        let Effect::Native {
            identity,
            method,
            params,
        } = effect
        else {
            return Submission::NotSent {
                reason: NotSentReason::Transient,
                error: "Invalid monitor native effect".into(),
            };
        };
        let Some(method) = Method::from_wire(method) else {
            return Submission::NotSent {
                reason: NotSentReason::Transient,
                error: "Unknown monitor provider method".into(),
            };
        };
        let timeout = if method == Method::CommandExec {
            params["timeoutMs"]
                .as_u64()
                .unwrap_or(3_600_000)
                .saturating_add(60_000)
                .min(3_660_000)
        } else {
            30_000
        };
        let result = self.request(
            operation,
            identity.clone(),
            method,
            params.clone(),
            Instant::now() + Duration::from_millis(timeout),
            true,
        );
        let receipt = match result {
            Ok(receipt) => receipt,
            Err(AccountSessionError::Unknown) => return Submission::Unknown,
            Err(error) => {
                return Submission::NotSent {
                    reason: NotSentReason::SessionMissing,
                    error: error.to_string(),
                };
            }
        };
        if receipt.operation != *identity {
            return Submission::Unknown;
        }
        if receipt.certainty != RequestCertainty::Written {
            return Self::submission(receipt, identity);
        }
        if method == Method::CommandExec {
            let id = params["processId"].as_str().unwrap_or_default();
            if id.is_empty() {
                return Submission::Unknown;
            }
            let (code, error) = match receipt.result {
                Some(ProviderResponse::Reply(result)) => (
                    result["exitCode"].as_i64(),
                    result["error"].as_str().map(str::to_owned),
                ),
                Some(ProviderResponse::Rejected(result)) => (None, Some(result.to_string())),
                None => return Submission::Unknown,
            };
            if code.is_none() && error.as_deref().is_none_or(str::is_empty) {
                return Submission::Unknown;
            }
            if monitor_event_queue::enqueue_exit(
                &self.store,
                id,
                operation,
                &format!("provider-result:{identity}"),
                code,
                error.as_deref(),
                studio_runtime_services::event_write::unix_time().unwrap_or(0.0),
            )
            .is_err()
            {
                return Submission::Unknown;
            }
        } else if method == Method::CommandExecWrite || method == Method::CommandExecResize {
            let ticket = identity.rsplit(':').next().unwrap_or(identity).to_owned();
            let reply = match receipt.result {
                Some(ProviderResponse::Reply(value)) => InputReply::Acknowledged(value),
                Some(ProviderResponse::Rejected(value)) => InputReply::Rejected(value.to_string()),
                None => InputReply::Unknown("Input acknowledgement is pending".into()),
            };
            self.input_replies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(ticket, reply);
        }
        Submission::Written
    }
}

impl RuleMonitor for RetainedMonitorPort {
    fn available(&self) -> bool {
        false
    }
    fn launch(&self, _check: &RuleCheck) -> CheckSubmission {
        CheckSubmission::NotSubmitted {
            error: "Not implemented: requires rule-driver/monitor-checks".into(),
        }
    }
    fn reconcile(&self, _check: &RuleCheck) -> CheckReconciliation {
        CheckReconciliation::Unknown
    }
    fn cancel(&self, _monitor_id: &str) -> Result<(), RulesTickError> {
        Err(RulesTickError::MonitorUnavailable)
    }
}

impl MonitorPort for RetainedMonitorPort {
    fn next(&self) -> Result<Option<Delivery>, MonitorError> {
        monitor_event_queue::next(&self.store)
    }

    fn acknowledge(&self, delivery: &Delivery) -> Result<(), MonitorError> {
        monitor_event_queue::acknowledge(&self.store, delivery)
    }

    fn reattach(&self, snapshot: &MonitorSnapshot) -> Result<Option<ReattachProof>, MonitorError> {
        let Some(operation) = snapshot.operation() else {
            return Ok(None);
        };
        let operation_id = format!("monitor:{}", snapshot.id());
        let Some(scope) = self.session.current_scope(&operation.account_key) else {
            return Ok(None);
        };
        let receipt = match self.request(
            operation,
            format!("monitor-status:{}", snapshot.id()),
            Method::OperationStatus,
            json!({"operationId":operation_id}),
            Instant::now() + Duration::from_secs(10),
            false,
        ) {
            Ok(receipt) => receipt,
            Err(AccountSessionError::Unknown) => return Ok(None),
            Err(AccountSessionError::Requires(name)) => {
                return Err(MonitorError::Unavailable(name));
            }
            Err(_) => return Ok(None),
        };
        if receipt.certainty != RequestCertainty::Written {
            return Ok(None);
        }
        let Some(ProviderResponse::Reply(result)) = receipt.result else {
            return Ok(None);
        };
        if result["accepted"] != true {
            return Ok(None);
        }
        let Some(native_id) = result["nativeId"].as_u64() else {
            return Ok(None);
        };
        if let Some(response) = result.get("response").filter(|value| !value.is_null()) {
            let response_value = response.get("result").unwrap_or(response);
            let code = response_value["exitCode"].as_i64();
            let error = response_value["error"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| response.get("error").map(Value::to_string));
            if code.is_none() && error.as_deref().is_none_or(str::is_empty) {
                return Ok(None);
            }
            monitor_event_queue::enqueue_exit(
                &self.store,
                snapshot.id(),
                operation,
                &format!("operation-status:{operation_id}"),
                code,
                error.as_deref(),
                studio_runtime_services::event_write::unix_time()
                    .map_err(|_| MonitorError::Refused("System clock unavailable"))?,
            )?;
            return Ok(None);
        }
        Ok(Some(ReattachProof {
            operation: operation.clone(),
            operation_id,
            native_id,
            current_connection: scope.connection,
        }))
    }

    fn probe(&self, _id: &str, _generation: i64, _command: &str) -> Result<(), MonitorError> {
        Err(MonitorError::Unavailable("provider-session/monitor-probe"))
    }

    fn input_reply(&self, _operation: &Operation, ticket: &str) -> InputReply {
        self.input_replies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(ticket)
            .unwrap_or_else(|| InputReply::Unknown("Input acknowledgement is unknown".into()))
    }
}
