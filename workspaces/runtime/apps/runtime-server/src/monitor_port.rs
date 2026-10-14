//! Account-worker monitor transport remains explicitly unavailable until its owner lands.

use studio_runtime_services::monitor_driver::{MonitorNative, NotSentReason, Submission};
use studio_runtime_services::monitor_policy::{
    Effect, MonitorError, MonitorSnapshot, Operation, ReattachProof,
};
use studio_runtime_services::monitor_runtime;
use studio_runtime_services::monitor_runtime_wiring::{Delivery, InputReply, MonitorPort};
use studio_runtime_services::rules_engine::RuleCheck;
use studio_runtime_services::rules_tick::{
    CheckReconciliation, CheckSubmission, RuleMonitor, RulesTickError,
};

pub(crate) struct UnavailableMonitorPort;

fn unavailable() -> MonitorError {
    MonitorError::Unavailable(monitor_runtime::SESSION_DEPENDENCY)
}

impl MonitorNative for UnavailableMonitorPort {
    fn submit(&self, _operation: &Operation, _effect: &Effect) -> Submission {
        Submission::NotSent {
            reason: NotSentReason::SessionMissing,
            error: format!(
                "Not implemented: requires {}",
                monitor_runtime::SESSION_DEPENDENCY
            ),
        }
    }
}

impl RuleMonitor for UnavailableMonitorPort {
    fn available(&self) -> bool {
        false
    }

    fn launch(&self, _check: &RuleCheck) -> CheckSubmission {
        CheckSubmission::NotSubmitted {
            error: "Not implemented: requires monitor-driver/rule-checks".to_owned(),
        }
    }

    fn reconcile(&self, _check: &RuleCheck) -> CheckReconciliation {
        CheckReconciliation::Unknown
    }

    fn cancel(&self, _monitor_id: &str) -> Result<(), RulesTickError> {
        Err(RulesTickError::MonitorUnavailable)
    }
}

impl MonitorPort for UnavailableMonitorPort {
    fn next(&self) -> Result<Option<Delivery>, MonitorError> {
        Err(unavailable())
    }

    fn acknowledge(&self, _delivery: &Delivery) -> Result<(), MonitorError> {
        Err(unavailable())
    }

    fn reattach(&self, _snapshot: &MonitorSnapshot) -> Result<Option<ReattachProof>, MonitorError> {
        Err(unavailable())
    }

    fn probe(&self, _id: &str, _generation: i64, _command: &str) -> Result<(), MonitorError> {
        Err(unavailable())
    }

    fn input_reply(&self, _operation: &Operation, _ticket: &str) -> InputReply {
        InputReply::Rejected(format!(
            "Not implemented: requires {}",
            monitor_runtime::SESSION_DEPENDENCY
        ))
    }
}
