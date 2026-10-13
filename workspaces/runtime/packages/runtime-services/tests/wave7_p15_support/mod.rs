use serde_json::json;
use std::collections::VecDeque;
use std::sync::Mutex;
use studio_runtime_services::{
    monitor_driver::{MonitorNative, Submission},
    monitor_policy::{Effect, MonitorError, MonitorSnapshot, Operation, ReattachProof},
    monitor_runtime_wiring::{Delivery, InputReply, MonitorPort},
    rules_engine::RuleCheck,
    rules_tick::{CheckSubmission, RuleMonitor, RulesTickError},
};
#[derive(Default)]
pub struct Port {
    pub deliveries: Mutex<VecDeque<Delivery>>,
    pub submitted: Mutex<Vec<Effect>>,
    pub probes: Mutex<Vec<(String, i64, String)>>,
    pub fail_ack: Mutex<bool>,
    pub unknown: Mutex<bool>,
}
impl MonitorNative for Port {
    fn submit(&self, _: &Operation, effect: &Effect) -> Submission {
        self.submitted.lock().expect("submit").push(effect.clone());
        if *self.unknown.lock().expect("unknown") {
            Submission::Unknown
        } else {
            Submission::Accepted
        }
    }
}
impl RuleMonitor for Port {
    fn available(&self) -> bool {
        false
    }
    fn launch(&self, _: &RuleCheck) -> CheckSubmission {
        panic!("no rule launch in fixture")
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
        self.probes.lock().expect("probes").push((id.into(), generation, command.into()));
        Ok(())
    }
    fn input_reply(&self, _: &Operation, _: &str) -> InputReply {
        InputReply::Acknowledged(json!({}))
    }
}
