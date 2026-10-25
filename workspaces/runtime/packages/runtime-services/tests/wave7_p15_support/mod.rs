use serde_json::json;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use studio_runtime_services::{
    monitor_driver::{MonitorNative, NotSentReason, Submission},
    monitor_policy::{
        Effect, MonitorError, MonitorSnapshot, Operation, ReattachOutcome, ReattachProof,
    },
    monitor_runtime_wiring::{Delivery, InputReply, MonitorPort},
    rules_engine::RuleCheck,
    rules_tick::{CheckReconciliation, CheckSubmission, RuleMonitor, RulesTickError},
};
#[derive(Default)]
pub struct Port {
    pub deliveries: Mutex<VecDeque<Delivery>>,
    pub submitted: Mutex<Vec<Effect>>,
    pub probes: Mutex<Vec<(String, i64, String)>>,
    pub starts: Mutex<Vec<(String, f64)>>,
    pub fail_ack: Mutex<bool>,
    pub acknowledged: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    pub unknown: Mutex<bool>,
    pub not_sent: Mutex<bool>,
    pub not_sent_reason: Mutex<NotSentReason>,
    pub rule_available: Mutex<bool>,
    pub availability_overrides: Mutex<VecDeque<bool>>,
    pub rule_checks: Mutex<Vec<RuleCheck>>,
    pub reconcile_not_submitted: Mutex<bool>,
}
impl MonitorNative for Port {
    fn submit<'a>(
        &'a self,
        _: &'a Operation,
        effect: &'a Effect,
    ) -> futures_util::future::BoxFuture<'a, Submission> {
        Box::pin(async move {
            self.submitted.lock().expect("submit").push(effect.clone());
            if *self.unknown.lock().expect("unknown") {
                Submission::Unknown
            } else if *self.not_sent.lock().expect("not sent") {
                Submission::NotSent {
                    reason: *self.not_sent_reason.lock().expect("not sent reason"),
                    error: "injected not sent".into(),
                }
            } else {
                Submission::Written
            }
        })
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
    fn launch<'a>(
        &'a self,
        check: &'a RuleCheck,
    ) -> futures_util::future::BoxFuture<'a, CheckSubmission> {
        Box::pin(async move {
            self.rule_checks
                .lock()
                .expect("rule checks")
                .push(check.clone());
            CheckSubmission::Accepted
        })
    }
    fn reconcile<'a>(
        &'a self,
        _: &'a RuleCheck,
    ) -> futures_util::future::BoxFuture<'a, CheckReconciliation> {
        Box::pin(async move {
            if *self
                .reconcile_not_submitted
                .lock()
                .expect("rule reconciliation")
            {
                CheckReconciliation::NotSubmitted
            } else {
                CheckReconciliation::Unknown
            }
        })
    }
    fn cancel<'a>(
        &'a self,
        _: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<(), RulesTickError>> {
        Box::pin(async { Ok(()) })
    }
}
impl MonitorPort for Port {
    fn start_monitor<'a>(
        &'a self,
        id: &'a str,
        now: f64,
    ) -> futures_util::future::BoxFuture<'a, Result<(), MonitorError>> {
        Box::pin(async move {
            self.starts
                .lock()
                .expect("monitor starts")
                .push((id.to_owned(), now));
            Ok(())
        })
    }

    fn next(&self) -> futures_util::future::BoxFuture<'_, Result<Option<Delivery>, MonitorError>> {
        Box::pin(async move { Ok(self.deliveries.lock().expect("queue").front().cloned()) })
    }
    fn acknowledge<'a>(
        &'a self,
        _: &'a Delivery,
    ) -> futures_util::future::BoxFuture<'a, Result<(), MonitorError>> {
        Box::pin(async move {
            if std::mem::take(&mut *self.fail_ack.lock().expect("ack")) {
                return Err(MonitorError::Refused("injected ACK loss"));
            }
            self.deliveries.lock().expect("queue").pop_front();
            if let Some(acknowledged) = self.acknowledged.lock().expect("acknowledged").as_ref() {
                acknowledged.add_permits(1);
            }
            Ok(())
        })
    }
    fn reattach<'a>(
        &'a self,
        snapshot: &'a MonitorSnapshot,
    ) -> futures_util::future::BoxFuture<'a, Result<ReattachOutcome, MonitorError>> {
        Box::pin(async move {
            Ok(ReattachOutcome::Running(ReattachProof {
                operation: snapshot.operation().expect("operation").clone(),
                operation_id: format!("monitor:{}", snapshot.id()),
                native_id: Some(42),
                current_connection: "restarted".into(),
            }))
        })
    }
    fn await_exit(&self, _: &MonitorSnapshot) -> Result<(), MonitorError> {
        Ok(())
    }
    fn probe<'a>(
        &'a self,
        id: &'a str,
        generation: i64,
        command: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<(), MonitorError>> {
        Box::pin(async move {
            self.probes
                .lock()
                .expect("probes")
                .push((id.into(), generation, command.into()));
            Ok(())
        })
    }
    fn input_reply<'a>(
        &'a self,
        _: &'a Operation,
        _: &'a str,
    ) -> futures_util::future::BoxFuture<'a, InputReply> {
        Box::pin(async { InputReply::Acknowledged(json!({})) })
    }
}
