use serde_json::json;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, mpsc};
use studio_runtime_services::{
    monitor_driver::{MonitorNative, Submission},
    monitor_policy::{Effect, MonitorError, MonitorSnapshot, Operation, ReattachProof},
    monitor_runtime_wiring::{Delivery, InputReply, MonitorPort},
    rules_engine::RuleCheck,
    rules_tick::{CheckReconciliation, CheckSubmission, RuleMonitor, RulesTickError},
};
#[derive(Default)]
pub struct Port {
    pub deliveries: Mutex<VecDeque<Delivery>>,
    pub submitted: Mutex<Vec<Effect>>,
    pub probes: Mutex<Vec<(String, i64, String)>>,
    pub fail_ack: Mutex<bool>,
    pub fail_ack_gate: Mutex<Option<(Arc<tokio::sync::Notify>, mpsc::Receiver<()>)>>,
    pub unknown: Mutex<bool>,
    pub not_sent: Mutex<bool>,
    pub rule_available: Mutex<bool>,
    pub availability_overrides: Mutex<VecDeque<bool>>,
    pub rule_checks: Mutex<Vec<RuleCheck>>,
    pub reconcile_not_submitted: Mutex<bool>,
}
impl MonitorNative for Port {
    fn submit(&self, _: &Operation, effect: &Effect) -> Submission {
        self.submitted.lock().expect("submit").push(effect.clone());
        if *self.unknown.lock().expect("unknown") {
            Submission::Unknown
        } else if *self.not_sent.lock().expect("not sent") {
            Submission::NotSent {
                error: "injected not sent".into(),
            }
        } else {
            Submission::Written
        }
    }
}
impl RuleMonitor for Port {
    fn available(&self) -> bool {
        self.availability_overrides
            .lock()
            .expect("rule availability")
            .pop_front()
            .unwrap_or_else(|| *self.rule_available.lock().expect("rule availability"))
    }
    fn launch(&self, check: &RuleCheck) -> CheckSubmission {
        self.rule_checks
            .lock()
            .expect("rule checks")
            .push(check.clone());
        CheckSubmission::Accepted
    }
    fn reconcile(&self, _: &RuleCheck) -> CheckReconciliation {
        if *self
            .reconcile_not_submitted
            .lock()
            .expect("rule reconciliation")
        {
            CheckReconciliation::NotSubmitted
        } else {
            CheckReconciliation::Unknown
        }
    }
    fn cancel(&self, _: &str) -> Result<(), RulesTickError> {
        Ok(())
    }
}
impl MonitorPort for Port {
    fn next(&self) -> Result<Option<Delivery>, MonitorError> {
        Ok(self.deliveries.lock().expect("queue").front().cloned())
    }
    fn acknowledge(&self, _: &Delivery) -> Result<(), MonitorError> {
        if let Some((entered, release)) = self.fail_ack_gate.lock().expect("ack gate").take() {
            entered.notify_one();
            release.recv().expect("test releases failed ACK barrier");
        }
        if std::mem::take(&mut *self.fail_ack.lock().expect("ack")) {
            return Err(MonitorError::Refused("injected ACK loss"));
        }
        self.deliveries.lock().expect("queue").pop_front();
        Ok(())
    }
    fn reattach(&self, snapshot: &MonitorSnapshot) -> Result<Option<ReattachProof>, MonitorError> {
        Ok(Some(ReattachProof {
            operation: snapshot.operation().expect("operation").clone(),
            operation_id: format!("monitor:{}", snapshot.id()),
            native_id: 42,
            current_connection: "restarted".into(),
        }))
    }
    fn probe(&self, id: &str, generation: i64, command: &str) -> Result<(), MonitorError> {
        self.probes
            .lock()
            .expect("probes")
            .push((id.into(), generation, command.into()));
        Ok(())
    }
    fn input_reply(&self, _: &Operation, _: &str) -> InputReply {
        InputReply::Acknowledged(json!({}))
    }
}
