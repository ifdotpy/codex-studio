#![allow(clippy::unwrap_used)]

use crate::managed_host_tools::{
    Actor, ChildCreationPort, ComplaintPort, ContextPort, Error, InterruptPort, OperationResult,
    PostCommitPort, ReviewPort, SearchPort, SendPort, StatusPort, WakeSignal, WatchMonitorPort,
};
use crate::provider_callbacks::{
    CallbackContext, CallbackDecision, CallbackError, ManagedToolPortsConfig,
    ManagedToolRuntimePorts, dispatch_with_managed_tools,
};
use rusqlite::{Transaction, params};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use studio_runtime_providers::{Frame, Method, RequestFrame, RequestId, ResponseBody};
use studio_runtime_store::path_identity::CanonicalPath;
use studio_runtime_store::{RuntimeStore, StoreError};

const BASE_AGENT: &str = "{\"accountKey\": \"default\", \"executionSettingsAccountKey\": \"default\", \"provider\": \"codex\", \"yoloMode\": true, \"prompt\": \"fixture\", \"cwd\": \"@cwd\", \"role\": \"orchestrator\", \"needsTitle\": false, \"subagentConcurrencyVersion\": 2, \"maxAgentsExplicit\": false, \"model\": \"gpt-6-astra\", \"effort\": \"medium\", \"fastMode\": false, \"daybreakEnabled\": false, \"cyberAccessProgram\": \"standard\", \"concurrency\": 32, \"maxAgents\": 64, \"tokenBudget\": null, \"status\": \"running\", \"autoWake\": true, \"epoch\": 3, \"inFlight\": false, \"turnEpoch\": 3, \"usageResumeEnabled\": true, \"tokensUsed\": 0, \"compactions\": 0, \"compactionsObservedOnly\": false, \"contextUsage\": null, \"events\": 0, \"error\": null, \"profileId\": null, \"profileInstructions\": \"\", \"tail\": \"\", \"worktree\": false, \"workspaceMode\": \"worktree\", \"executionMode\": \"native\", \"hostProjectPath\": \"@cwd\", \"layrReady\": false, \"workspaceBackend\": null, \"workspaceModeExplicit\": false, \"workspaceInGit\": false, \"worktreeReady\": false, \"environment\": \"host\", \"imageWorkspace\": false, \"imageWorkspaceReady\": false, \"imageWorkspacePhase\": null, \"imageWorkspaceRepo\": null, \"imageWorkspaceSubpath\": \".\", \"imageWorkspaceBaseRef\": null, \"imageWorkspaceHasGit\": false, \"imageWorkspaceCreatedAt\": null, \"imageWorkspaceError\": null, \"worktreeWarning\": null, \"workerBaseRef\": null, \"workerBaseCommit\": null, \"workerBaseBehindMain\": null, \"workerBaseMainRef\": null, \"workerDefaults\": {\"model\": \"gpt-6-luna\", \"effort\": \"high\", \"fastMode\": false, \"daybreakEnabled\": false, \"delegationDepth\": 1}, \"reviewDefaults\": {\"model\": null, \"effort\": null}, \"agentMode\": \"multi\", \"agentModeRevision\": 0, \"agentModeSupported\": true}";
const FIXTURE_MONITOR: &str = "{\"activityAt\": 19.0, \"activityGeneration\": 0, \"agent\": \"bf2eeb40-0cc8-44ef-a4e8-aa321b4d7331\", \"bytes\": 0, \"command\": \"printf fixture\", \"created\": 19.0, \"cwd\": \"@cwd\", \"epoch\": 3, \"error\": \"Server restarted. Command outcome unknown; not rerun.\", \"exitCode\": null, \"id\": \"6a1cbd51-e035-54ce-af5c-7fd9850e3065\", \"interactive\": false, \"livenessCommand\": \"\", \"log\": \"@cwd/monitor-logs/6a1cbd51-e035-54ce-af5c-7fd9850e3065.log\", \"operation\": {\"accountKey\": \"default\", \"agent\": \"bf2eeb40-0cc8-44ef-a4e8-aa321b4d7331\", \"connectionId\": \"connection\", \"epoch\": 3, \"requestId\": \"managed-monitor\"}, \"outputEventCount\": 0, \"outputEventIds\": [], \"outputEventSequence\": 0, \"outputPatterns\": [], \"reattachRecovery\": {\"accountKey\": \"default\", \"epoch\": 3, \"status\": \"not_reattachable\", \"startedAt\": 19.0}, \"requestId\": \"managed-monitor\", \"ruleId\": null, \"stallTimeoutSeconds\": 1800, \"stallWakeGeneration\": -1, \"status\": \"lost\", \"successExitCodes\": [0], \"tail\": \"\", \"timeout_ms\": 3600000, \"turnId\": \"turn-0\", \"wakeOn\": \"exit\"}";
const LEAD: &str = "bf2eeb40-0cc8-44ef-a4e8-aa321b4d7331";
const WORKER: &str = "415ccbb7-cd04-444e-8599-af5c52da8e5e";
const FOREIGN: &str = "787d59cc-071a-4918-990a-cb1dffba4e2f";

struct Template {
    value: Value,
}

/// Rust-built fixture: a lead, its worker and a foreign lead, the managed tool
/// calls the tests replay, and one queued spawn reservation.
fn template() -> &'static Template {
    static TEMPLATE: OnceLock<Template> = OnceLock::new();
    TEMPLATE.get_or_init(|| {
        let scratch = tempfile::tempdir().unwrap();
        let cwd = scratch.path().to_string_lossy().into_owned();
        let base: Value = serde_json::from_str(&BASE_AGENT.replace("@cwd", &cwd)).unwrap();
        let actor = |index: usize, id: &str, name: &str, parent: Option<&str>, lead: bool| {
            let mut record = base.clone();
            record["id"] = json!(id);
            record["name"] = json!(name);
            record["isLead"] = json!(lead);
            record["parentId"] = json!(parent);
            record["rootId"] = json!(parent.unwrap_or(id));
            record["threadId"] = json!(format!("thread-{index}"));
            record["turnId"] = json!(format!("turn-{index}"));
            record["created"] = json!(1_792_510_147.0 + index as f64);
            record
        };
        let value = json!({
            "agents": [
                actor(0, LEAD, "Callback lead", None, true),
                actor(1, WORKER, "Callback worker", Some(LEAD), false),
                actor(2, FOREIGN, "Other team", None, true),
            ],
            "monitor": serde_json::from_str::<Value>(&FIXTURE_MONITOR.replace("@cwd", &cwd)).unwrap(),
            "managedToolCases": [{"actor": "bf2eeb40-0cc8-44ef-a4e8-aa321b4d7331", "tool": "orchestration_send", "arguments": {"agent_id": "415ccbb7-cd04-444e-8599-af5c52da8e5e", "text": "descendant input"}, "callId": "managed-0"}, {"actor": "415ccbb7-cd04-444e-8599-af5c52da8e5e", "tool": "orchestration_send", "arguments": {"agent_id": "parent", "text": "parent alias"}, "callId": "managed-1"}, {"actor": "415ccbb7-cd04-444e-8599-af5c52da8e5e", "tool": "orchestration_send", "arguments": {"agent_id": "bf2eeb40-0cc8-44ef-a4e8-aa321b4d7331", "text": "parent id"}, "callId": "managed-2"}, {"actor": "415ccbb7-cd04-444e-8599-af5c52da8e5e", "tool": "orchestration_send", "arguments": {"agent_id": "broadcast", "text": "team notice"}, "callId": "managed-3"}, {"actor": "415ccbb7-cd04-444e-8599-af5c52da8e5e", "tool": "orchestration_send", "arguments": {"agent_id": "787d59cc-071a-4918-990a-cb1dffba4e2f", "text": "peer refusal"}, "callId": "managed-4"}, {"actor": "415ccbb7-cd04-444e-8599-af5c52da8e5e", "tool": "orchestration_complaint", "arguments": {"action": "submit", "text": "Need help"}, "callId": "managed-5"}, {"actor": "bf2eeb40-0cc8-44ef-a4e8-aa321b4d7331", "tool": "orchestration_complaint", "arguments": {"action": "read", "complaint_id": "ccba5086-ee65-562b-9703-fd2fc708bad4"}, "callId": "managed-6"}, {"actor": "bf2eeb40-0cc8-44ef-a4e8-aa321b4d7331", "tool": "orchestration_complaint", "arguments": {"action": "respond", "complaint_id": "ccba5086-ee65-562b-9703-fd2fc708bad4", "text": "Investigating", "status": "in_progress"}, "callId": "managed-7"}, {"actor": "bf2eeb40-0cc8-44ef-a4e8-aa321b4d7331", "tool": "orchestration_context", "arguments": {"topic": "monitor", "id": "6a1cbd51-e035-54ce-af5c-7fd9850e3065"}, "callId": "managed-9"}, {"actor": "bf2eeb40-0cc8-44ef-a4e8-aa321b4d7331", "tool": "orchestration_cancel_monitor", "arguments": {"monitor_id": "6a1cbd51-e035-54ce-af5c-7fd9850e3065"}, "callId": "managed-10"}],
            "managedRootDeletedRefusal": {"actor": "415ccbb7-cd04-444e-8599-af5c52da8e5e", "tool": "orchestration_complaint", "callId": "deleted-root", "arguments": {"action": "read"}},
            "reservations": [{"record": {"id": "thread-0:spawn:request-1", "agent": "bf2eeb40-0cc8-44ef-a4e8-aa321b4d7331", "threadId": "thread-0", "callId": "alias-1", "tool": "orchestration_spawn", "signature": "c91f8a50f288534226b47d6abda036631ba03b0ad5aae43ad0b8e1fe6431fc12", "rpcId": 1, "turnId": "turn-0", "epoch": 3, "accountKey": "default", "connectionId": "connection", "created": 10.0, "updated": 10.0, "readOnly": false, "timing": {"reservationBeganAt": 49476990541458_i64, "reservationLockedAt": 49476991477012_i64, "reservationEndedAt": 49476991698409_i64}, "request_id": "request-1", "stage": "queued", "outcome": "pending", "cancelRequested": false}}],
        });
        Template {
            value,
        }
    })
}

async fn fixture() -> (tempfile::TempDir, Arc<RuntimeStore>, CallbackContext) {
    let template = template();
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
    let agents = template.value["agents"].as_array().unwrap().clone();
    let monitor = template.value["monitor"].clone();
    let cwd = json!(directory.path());
    store
        .transaction_with::<_, StoreError>(move |tx| {
            for agent in &agents {
                let mut record = studio_runtime_store::AgentRecord::from_value(agent.clone())
                    .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
                record.extra.insert("cwd".into(), cwd.clone());
                record.extra.insert("hostProjectPath".into(), cwd.clone());
                studio_runtime_store::AgentRepository::put(tx, &record)?;
                crate::agents_create::publish_agent(tx, &record.value())?;
            }
            tx.execute(
                "INSERT INTO runtime_monitors(id,record) VALUES(?1,?2)",
                params![monitor["id"].as_str().unwrap(), monitor.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    store
        .transaction_with::<_, StoreError>(|tx| {
            tx.execute_batch(
                "CREATE TABLE p05_test_intents (kind TEXT NOT NULL, id TEXT NOT NULL, payload TEXT NOT NULL, started INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(kind,id)); CREATE TABLE p05_test_starts (child_id TEXT PRIMARY KEY);",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let actor = &template.value["agents"][0];
    let context = CallbackContext {
        agent: actor["id"].as_str().unwrap().into(),
        account: "default".into(),
        connection: "connection".into(),
        generation: 7,
        epoch: 3,
        thread: "thread-0".into(),
        turn: "turn-0".into(),
    };
    (directory, store, context)
}

async fn fixture_arc() -> (tempfile::TempDir, Arc<RuntimeStore>, CallbackContext) {
    fixture().await
}

fn shared_ports<P>(ports: P) -> Arc<dyn crate::managed_host_tools::Ports<Prepared = ()>>
where
    P: crate::managed_host_tools::Ports<Prepared = ()> + 'static,
{
    Arc::new(ports)
}

#[derive(Default)]
struct PortBundle {
    staged: AtomicUsize,
    released: AtomicUsize,
    skip_release: AtomicBool,
    fail_release: AtomicBool,
    dispatch_status: AtomicBool,
    dispatched: AtomicUsize,
}

impl PortBundle {
    fn stage(
        &self,
        tx: &Transaction<'_>,
        kind: &str,
        request_id: &str,
        value: Value,
    ) -> Result<OperationResult, Error> {
        tx.execute(
            "INSERT OR IGNORE INTO p05_test_intents(kind,id,payload) VALUES(?1,?2,?3)",
            params![kind, request_id, value.to_string()],
        )?;
        self.staged.fetch_add(1, Ordering::SeqCst);
        Ok(OperationResult {
            value,
            signals: vec![WakeSignal::new(kind)],
        })
    }
}

impl ChildCreationPort for PortBundle {
    type Prepared = ();

    fn prepare_work_item(
        &self,
        tx: &Transaction<'_>,
        _: &Actor,
        request_id: &str,
        spec: &Value,
        _: f64,
    ) -> Result<Self::Prepared, Error> {
        let id = format!("child-{request_id}");
        tx.execute(
            "INSERT OR IGNORE INTO p05_test_intents(kind,id,payload) VALUES('spawn',?1,?2)",
            params![id, spec.to_string()],
        )?;
        self.staged.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn recheck_worker(
        &self,
        _: &Transaction<'_>,
        _: &Actor,
        _: &Self::Prepared,
    ) -> Result<(), Error> {
        Ok(())
    }

    fn persisted_record(&self, tx: &Transaction<'_>, _: &Self::Prepared) -> Result<Value, Error> {
        let child_id: String = tx.query_row(
            "SELECT id FROM p05_test_intents WHERE kind='spawn' ORDER BY rowid DESC LIMIT 1",
            [],
            |row| row.get(0),
        )?;
        Ok(
            json!({"id":child_id,"name":"Worker","status":"starting","provider":"codex","cwd":"/isolated","worktree":true}),
        )
    }

    fn wake_signal(&self, _: &Self::Prepared) -> WakeSignal {
        WakeSignal::new("child-start")
    }
}

impl SendPort for PortBundle {
    fn send(
        &self,
        tx: &Transaction<'_>,
        _: &Actor,
        args: &Value,
        request_id: &str,
        _: f64,
    ) -> Result<OperationResult, Error> {
        self.stage(tx, "send", request_id, args.clone())
    }
}

impl InterruptPort for PortBundle {
    fn interrupt_in_transaction(
        &self,
        tx: &Transaction<'_>,
        _: &Actor,
        target_id: &str,
        reason: &str,
        request_id: &str,
        _: f64,
    ) -> Result<OperationResult, Error> {
        self.stage(
            tx,
            "interrupt",
            request_id,
            json!({"targetId":target_id,"reason":reason}),
        )
    }
}

impl StatusPort for PortBundle {
    fn status(
        &self,
        tx: &Transaction<'_>,
        _: &Actor,
        args: &Value,
    ) -> Result<OperationResult, Error> {
        let result = self.stage(tx, "status", "status", args.clone())?;
        Ok(result)
    }
}

impl SearchPort for PortBundle {
    fn search(
        &self,
        tx: &Transaction<'_>,
        _: &Actor,
        args: &Value,
    ) -> Result<OperationResult, Error> {
        self.stage(tx, "search", "search", args.clone())
    }
}

impl ContextPort for PortBundle {
    fn context(
        &self,
        tx: &Transaction<'_>,
        _: &Actor,
        args: &Value,
    ) -> Result<OperationResult, Error> {
        self.stage(tx, "context", "context", args.clone())
    }
}

impl ComplaintPort for PortBundle {
    fn complaint(
        &self,
        tx: &Transaction<'_>,
        _: &Actor,
        args: &Value,
        request_id: &str,
        _: f64,
    ) -> Result<OperationResult, Error> {
        self.stage(tx, "complaint", request_id, args.clone())
    }
}

impl WatchMonitorPort for PortBundle {
    fn watch(
        &self,
        tx: &Transaction<'_>,
        _: &Actor,
        args: &Value,
        request_id: &str,
        _: f64,
    ) -> Result<OperationResult, Error> {
        self.stage(tx, "watch", request_id, args.clone())
    }

    fn monitor(
        &self,
        tx: &Transaction<'_>,
        _: &Actor,
        args: &Value,
        request_id: &str,
        _: f64,
    ) -> Result<OperationResult, Error> {
        self.stage(tx, "monitor", request_id, args.clone())
    }
}

impl ReviewPort for PortBundle {
    fn review(
        &self,
        tx: &Transaction<'_>,
        _: &Actor,
        args: &Value,
        request_id: &str,
        _: f64,
    ) -> Result<OperationResult, Error> {
        self.stage(tx, "review", request_id, args.clone())
    }
}

impl PostCommitPort for PortBundle {
    fn dispatch_managed<'a>(
        &'a self,
        _: Arc<RuntimeStore>,
        _: &Actor,
        tool: &'a str,
        _: &Value,
        _: &str,
        _: f64,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Option<crate::managed_host_tools::DispatchedOperation>, Error>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            if tool != "orchestration_status" || !self.dispatch_status.load(Ordering::SeqCst) {
                return Ok(None);
            }
            self.dispatched.fetch_add(1, Ordering::SeqCst);
            Ok(Some(crate::managed_host_tools::DispatchedOperation {
                result: OperationResult {
                    value: json!({"status":"dispatched"}),
                    signals: vec![WakeSignal::new("event-driver/test-dispatch")],
                },
                certainty: crate::managed_host_tools::DeliveryCertainty::Written,
                applied: Some(true),
            }))
        })
    }

    fn release(&self, signals: &[WakeSignal]) -> Result<(), Error> {
        if self.fail_release.load(Ordering::SeqCst) {
            return Err(Error::Unavailable("test/post-commit-dispatch"));
        }
        if !self.skip_release.load(Ordering::SeqCst) {
            self.released.fetch_add(signals.len(), Ordering::SeqCst);
        }
        Ok(())
    }
}

#[tokio::test]
async fn a_failed_wake_after_the_effect_still_answers_with_the_real_result() {
    let (_root, store, context) = fixture().await;
    let ports = Arc::new(PortBundle::default());
    ports.dispatch_status.store(true, Ordering::SeqCst);
    ports.fail_release.store(true, Ordering::SeqCst);
    let frame = call(
        &context,
        "post-commit-status",
        "orchestration_status",
        json!({}),
    );
    // The effect committed, so the model must not be told it failed.
    let first =
        dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 20.0, ports.clone())
            .await
            .expect("a lost wake does not fail a committed tool call");
    let CallbackDecision::Answer(first) = first else {
        panic!("a committed tool call answers");
    };
    assert!(matches!(&first.body, ResponseBody::Result(value) if value["success"] == true));
    assert_eq!(ports.dispatched.load(Ordering::SeqCst), 1);
    let connection = store.test_reader().unwrap();
    let receipt = studio_runtime_store::system_reads::all(&connection, "runtime_tool_requests")
        .unwrap()
        .into_iter()
        .find(|row| row["callId"] == "post-commit-status")
        .unwrap();
    assert_eq!(receipt["deliveryCertainty"], "written");

    ports.fail_release.store(false, Ordering::SeqCst);
    let replay =
        dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 21.0, ports.clone())
            .await
            .unwrap();
    let CallbackDecision::Answer(replay) = replay else {
        panic!("a replay answers from the durable result");
    };
    assert_eq!(
        replay.body, first.body,
        "the replay rebuilds the same answer"
    );
    assert_eq!(ports.dispatched.load(Ordering::SeqCst), 1);
}

struct ScriptedMonitorPort {
    certainty: crate::managed_host_tools::DeliveryCertainty,
    calls: AtomicUsize,
    tickets: Mutex<Vec<String>>,
    store: Option<Arc<RuntimeStore>>,
}

impl crate::provider_callbacks::MonitorInputPort for ScriptedMonitorPort {
    fn input<'a>(
        &'a self,
        request: crate::provider_callbacks::MonitorInputRequest<'a>,
    ) -> futures_util::future::BoxFuture<'a, Result<Value, Error>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.tickets.lock().unwrap().push(request.ticket.to_owned());
            match self.certainty {
                crate::managed_host_tools::DeliveryCertainty::Written => {
                    Ok(json!({"ticket":request.ticket}))
                }
                crate::managed_host_tools::DeliveryCertainty::NotSent => {
                    Err(Error::Unavailable("test/input-not-sent"))
                }
                crate::managed_host_tools::DeliveryCertainty::Unknown => {
                    let monitor_id = request.monitor_id.to_owned();
                    let ticket = request.ticket.to_owned();
                    self.store
                        .as_ref()
                        .expect("unknown delivery fixture stores its receipt")
                        .transaction_with(move |tx| {
                            let mut record = studio_runtime_store::system_reads::get(
                                tx,
                                "runtime_monitors",
                                &monitor_id,
                            )?
                            .ok_or(Error::Invalid("Unknown test monitor"))?;
                            record["inputAttempt"] = json!({"id":ticket,"status":"pending"});
                            tx.execute(
                                "UPDATE runtime_monitors SET record=?1 WHERE id=?2",
                                rusqlite::params![record.to_string(), monitor_id],
                            )?;
                            Ok::<_, Error>(())
                        })
                        .await?;
                    Err(Error::Unavailable("test/input-unknown"))
                }
            }
        })
    }
}

impl crate::provider_callbacks::MonitorCancelPort for ScriptedMonitorPort {
    fn cancel_monitor<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: f64,
    ) -> futures_util::future::BoxFuture<'a, Result<Value, Error>> {
        Box::pin(async { Ok(Value::Null) })
    }
}

struct ScriptedRetainedMonitorPort {
    certainty: crate::managed_host_tools::DeliveryCertainty,
    calls: AtomicUsize,
    tickets: Mutex<Vec<String>>,
}

impl crate::monitor_driver::MonitorNative for ScriptedRetainedMonitorPort {
    fn submit<'a>(
        &'a self,
        _operation: &'a crate::monitor_policy::Operation,
        effect: &'a crate::monitor_policy::Effect,
    ) -> futures_util::future::BoxFuture<'a, crate::monitor_driver::Submission> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let crate::monitor_policy::Effect::Native { identity, .. } = effect {
                self.tickets.lock().unwrap().push(identity.clone());
            }
            match self.certainty {
                crate::managed_host_tools::DeliveryCertainty::Written => {
                    crate::monitor_driver::Submission::Written
                }
                crate::managed_host_tools::DeliveryCertainty::NotSent => {
                    crate::monitor_driver::Submission::NotSent {
                        reason: crate::monitor_driver::NotSentReason::Transient,
                        error: "test transport did not send".into(),
                    }
                }
                crate::managed_host_tools::DeliveryCertainty::Unknown => {
                    crate::monitor_driver::Submission::Unknown
                }
            }
        })
    }
}

impl crate::rules_tick::RuleMonitor for ScriptedRetainedMonitorPort {
    fn available(&self) -> bool {
        false
    }
    fn launch<'a>(
        &'a self,
        _check: &'a crate::rules_engine::RuleCheck,
    ) -> futures_util::future::BoxFuture<'a, crate::rules_tick::CheckSubmission> {
        Box::pin(async { crate::rules_tick::CheckSubmission::Unknown })
    }
    fn cancel<'a>(
        &'a self,
        _monitor_id: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<(), crate::rules_tick::RulesTickError>> {
        Box::pin(async { Err(crate::rules_tick::RulesTickError::MonitorUnavailable) })
    }
}

impl crate::monitor_runtime_wiring::MonitorPort for ScriptedRetainedMonitorPort {
    fn next<'a>(
        &'a self,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<
            Option<crate::monitor_runtime_wiring::Delivery>,
            crate::monitor_policy::MonitorError,
        >,
    > {
        Box::pin(async { Ok(None) })
    }
    fn acknowledge<'a>(
        &'a self,
        _delivery: &'a crate::monitor_runtime_wiring::Delivery,
    ) -> futures_util::future::BoxFuture<'a, Result<(), crate::monitor_policy::MonitorError>> {
        Box::pin(async { Ok(()) })
    }
    fn reattach<'a>(
        &'a self,
        _snapshot: &'a crate::monitor_policy::MonitorSnapshot,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<crate::monitor_policy::ReattachOutcome, crate::monitor_policy::MonitorError>,
    > {
        Box::pin(async { Ok(crate::monitor_policy::ReattachOutcome::Pending) })
    }
    fn probe<'a>(
        &'a self,
        _id: &'a str,
        _generation: i64,
        _command: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<(), crate::monitor_policy::MonitorError>> {
        Box::pin(async { Ok(()) })
    }
    fn input_reply<'a>(
        &'a self,
        _operation: &'a crate::monitor_policy::Operation,
        ticket: &'a str,
    ) -> futures_util::future::BoxFuture<'a, crate::monitor_runtime_wiring::InputReply> {
        Box::pin(async move {
            crate::monitor_runtime_wiring::InputReply::Acknowledged(json!({"ticket":ticket}))
        })
    }
}

struct NoopLifecyclePorts;

impl crate::agent_lifecycle_runtime::LifecyclePorts for NoopLifecyclePorts {
    fn stage(
        &self,
        _tx: &Transaction<'_>,
        _effect: &crate::agent_lifecycle_runtime::LifecycleEffect,
    ) -> Result<(), crate::agent_management::ManagementError> {
        Ok(())
    }
    fn committed(&self) -> crate::agent_lifecycle_runtime::LifecycleCommitFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}

fn real_monitor_runtime_ports(
    store: Arc<RuntimeStore>,
    fake: Arc<ScriptedRetainedMonitorPort>,
) -> ManagedToolRuntimePorts {
    let monitor: Arc<dyn crate::monitor_runtime_wiring::MonitorPort> = fake;
    let runtime = Arc::new(crate::monitor_runtime_wiring::MonitorRuntime::new(
        store,
        monitor,
        Arc::new(tokio::sync::Notify::new()),
    ));
    crate::provider_callbacks::ManagedToolRuntime::new(runtime, Arc::new(NoopLifecyclePorts))
        .ports()
}

async fn production_managed_ports(
    store: &Arc<RuntimeStore>,
    context: &CallbackContext,
    frame: &Frame,
    monitor: Arc<ScriptedRetainedMonitorPort>,
) -> Arc<dyn crate::managed_host_tools::Ports<Prepared = ()>> {
    shared_ports(
        crate::provider_callbacks::managed_tool_ports_with_runtime(
            store.as_ref(),
            context,
            frame,
            ManagedToolPortsConfig {
                wake: Arc::new(tokio::sync::Notify::new()),
                monitor_dispatch_wake: Arc::new(tokio::sync::Notify::new()),
                runtime_ports: real_monitor_runtime_ports(Arc::clone(store), monitor),
                preparation: None,
            },
        )
        .await
        .unwrap(),
    )
}

#[tokio::test]
async fn production_monitor_adapter_authorizes_before_callback_writes_and_preserves_receipts() {
    for certainty in [
        crate::managed_host_tools::DeliveryCertainty::Written,
        crate::managed_host_tools::DeliveryCertainty::NotSent,
        crate::managed_host_tools::DeliveryCertainty::Unknown,
    ] {
        let (_root, store, context) = fixture_arc().await;
        let fake = Arc::new(ScriptedRetainedMonitorPort {
            certainty,
            calls: AtomicUsize::new(0),
            tickets: Mutex::new(Vec::new()),
        });
        let frame = call(
            &context,
            "production-monitor-input",
            "orchestration_monitor_input",
            json!({"monitor_id":managed_monitor_id(),"text":"hello"}),
        );
        activate_fixture_monitor(&store, &context, &managed_monitor_id()).await;
        let ports = production_managed_ports(&store, &context, &frame, fake.clone()).await;
        let first =
            dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 20.0, ports.clone())
                .await
                .unwrap();
        dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 21.0, ports.clone())
            .await
            .unwrap();
        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            1,
            "first reply: {first:?}"
        );
        let tickets = fake.tickets.lock().unwrap();
        assert_eq!(tickets.len(), 1, "same native ticket must not be resent");
        assert!(tickets[0].contains("production-monitor-input"));
        let row = studio_runtime_store::system_reads::all(
            &store.test_reader().unwrap(),
            "runtime_tool_requests",
        )
        .unwrap()
        .into_iter()
        .find(|row| row["callId"] == "production-monitor-input")
        .unwrap();
        let expected = match certainty {
            crate::managed_host_tools::DeliveryCertainty::Written => "written",
            crate::managed_host_tools::DeliveryCertainty::NotSent => "not_sent",
            crate::managed_host_tools::DeliveryCertainty::Unknown => "unknown",
        };
        assert_eq!(row["deliveryCertainty"], expected);
    }

    // A live but non-owning agent cannot reserve a callback claim or result.
    let (_root, store, _lead_context) = fixture_arc().await;
    let worker = &template().value["agents"][1];
    let worker_context = CallbackContext {
        agent: worker["id"].as_str().unwrap().to_owned(),
        account: "default".into(),
        connection: "connection".into(),
        generation: 7,
        epoch: worker["epoch"].as_u64().unwrap(),
        thread: worker["threadId"].as_str().unwrap().to_owned(),
        turn: worker["turnId"].as_str().unwrap().to_owned(),
    };
    let fake = Arc::new(ScriptedRetainedMonitorPort {
        certainty: crate::managed_host_tools::DeliveryCertainty::Written,
        calls: AtomicUsize::new(0),
        tickets: Mutex::new(Vec::new()),
    });
    let frame = call(
        &worker_context,
        "production-foreign-monitor-input",
        "orchestration_monitor_input",
        json!({"monitor_id":managed_monitor_id(),"text":"no"}),
    );
    let ports = production_managed_ports(&store, &worker_context, &frame, fake.clone()).await;
    let before = callback_intent_snapshot(&store);
    let decision = dispatch_with_managed_tools(
        Arc::clone(&store),
        &worker_context,
        &frame,
        20.0,
        ports.clone(),
    )
    .await;
    assert!(decision.is_err(), "non-owner monitor input must be refused");
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    assert_eq!(callback_intent_snapshot(&store), before);

    // A stale authenticated epoch is refused before tool request/result/alias writes.
    let (_root, store, mut stale_context) = fixture_arc().await;
    stale_context.epoch += 1;
    let fake = Arc::new(ScriptedRetainedMonitorPort {
        certainty: crate::managed_host_tools::DeliveryCertainty::Written,
        calls: AtomicUsize::new(0),
        tickets: Mutex::new(Vec::new()),
    });
    let frame = call(
        &stale_context,
        "production-stale-monitor-input",
        "orchestration_monitor_input",
        json!({"monitor_id":managed_monitor_id(),"text":"no"}),
    );
    let ports = production_managed_ports(&store, &stale_context, &frame, fake.clone()).await;
    let before = callback_intent_snapshot(&store);
    let decision = dispatch_with_managed_tools(
        Arc::clone(&store),
        &stale_context,
        &frame,
        20.0,
        ports.clone(),
    )
    .await;
    assert!(decision.is_err(), "stale monitor input must be refused");
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    assert_eq!(callback_intent_snapshot(&store), before);
}

struct EligibleAccountRead;

impl crate::provider_callbacks::ManagedCreationAccountRead for EligibleAccountRead {
    fn worker_accounts(
        &self,
    ) -> Result<
        crate::provider_callbacks::ManagedCreationAccountListing,
        crate::provider_callbacks::ManagedCreationAccountError,
    > {
        Ok(crate::provider_callbacks::ManagedCreationAccountListing {
            accounts: vec![crate::provider_callbacks::ManagedCreationAccountCandidate {
                account_key: "primary".into(),
                identity: crate::provider_callbacks::ManagedAccountIdentity {
                    provider: studio_runtime_providers::ProviderKind::Codex,
                    home: None,
                    account_id: None,
                    email: None,
                },
                ready: true,
                can_attempt_native_proof: false,
                disconnected: false,
                deleted: false,
                duplicate: false,
                max_running_turns: studio_runtime_schema::shared_values::ACCOUNT_RUNNING_TURNS_MAX,
                group: None,
            }],
            default_account_groups: Vec::new(),
        })
    }

    fn resolve_identity(
        &self,
        _account_key: &str,
        _parent_account_key: &str,
    ) -> Result<
        crate::provider_callbacks::ManagedAccountIdentity,
        crate::provider_callbacks::ManagedCreationAccountError,
    > {
        unreachable!("implicit account selection uses the listed identity")
    }
}

#[tokio::test]
async fn unavailable_provider_model_list_refuses_without_callback_writes() {
    let (_root, store, _context) = fixture().await;
    let before = callback_intent_snapshot(&store);
    let mut unavailable =
        |_: &str, _: &crate::provider_callbacks::ManagedAccountIdentity, _: &str| {
            Err(
                crate::managed_child_create::ManagedCreationEnvironmentError::Unavailable(
                    "provider-session/model-list",
                ),
            )
        };
    let result = crate::provider_callbacks::select_worker_account(
        crate::provider_callbacks::WorkerAccountSelection {
            accounts: &EligibleAccountRead,
            request: crate::provider_callbacks::WorkerAccountRequest {
                parent_account_key: "primary",
                requested_account_key: None,
                default_account_key: None,
                model: "gpt-5",
                excluded_account_keys: &[],
                project_account_keys: None,
                project_account_groups: None,
                active_agents: &std::collections::HashMap::new(),
            },
            model_available: &mut unavailable,
        },
    );
    assert!(matches!(
        result,
        Err(
            crate::managed_child_create::ManagedCreationEnvironmentError::Unavailable(
                "provider-session/model-list",
            )
        )
    ));
    assert_eq!(callback_intent_snapshot(&store), before);
}

fn callback_intent_snapshot(store: &RuntimeStore) -> Value {
    let connection = store.test_reader().unwrap();
    json!({
        "requests": connection.prepare("SELECT id,record FROM runtime_tool_requests ORDER BY id").unwrap()
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).unwrap()
            .collect::<Result<Vec<_>, _>>().unwrap(),
        "results": connection.prepare("SELECT id,result FROM runtime_tool_results ORDER BY id").unwrap()
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).unwrap()
            .collect::<Result<Vec<_>, _>>().unwrap(),
        "aliases": connection.prepare("SELECT agent,alias,request FROM runtime_tool_request_aliases ORDER BY agent,alias,request").unwrap()
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))).unwrap()
            .collect::<Result<Vec<_>, _>>().unwrap(),
    })
}

fn managed_ports(
    monitor_input: Option<Arc<dyn crate::provider_callbacks::MonitorInputPort>>,
    monitor_cancel: Option<Arc<dyn crate::provider_callbacks::MonitorCancelPort>>,
) -> ManagedToolPortsConfig {
    ManagedToolPortsConfig {
        wake: Arc::new(tokio::sync::Notify::new()),
        monitor_dispatch_wake: Arc::new(tokio::sync::Notify::new()),
        runtime_ports: ManagedToolRuntimePorts {
            status_watch: None,
            child_creation: None,
            monitor_create: None,
            interrupt: None,
            monitor_input,
            monitor_cancel,
            work_action: None,
            late: Default::default(),
        },
        preparation: None,
    }
}

fn managed_monitor_id() -> String {
    template().value["managedToolCases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["tool"] == "orchestration_context")
        .unwrap()["arguments"]["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn activate_fixture_monitor(store: &RuntimeStore, context: &CallbackContext, id: &str) {
    let context = context.clone();
    let id = id.to_owned();
    store
        .transaction_with::<_, StoreError>(move |tx| {
            let mut record = studio_runtime_store::system_reads::get(tx, "runtime_monitors", &id)?
                .ok_or(StoreError::RecordContract("missing fixture monitor"))?;
            record["status"] = json!("running");
            record["epoch"] = json!(context.epoch);
            record["interactive"] = json!(true);
            record["operation"] = json!({
                "agent":context.agent,
                "epoch":context.epoch,
                "accountKey":context.account,
                "connectionId":context.connection
            });
            tx.execute(
                "UPDATE runtime_monitors SET record=?1 WHERE id=?2",
                rusqlite::params![record.to_string(), id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn managed_monitor_input_checks_owner_and_preserves_delivery_receipts_on_replay() {
    for certainty in [
        crate::managed_host_tools::DeliveryCertainty::Written,
        crate::managed_host_tools::DeliveryCertainty::NotSent,
        crate::managed_host_tools::DeliveryCertainty::Unknown,
    ] {
        let (_root, store, context) = fixture_arc().await;
        let fake = Arc::new(ScriptedMonitorPort {
            certainty,
            calls: AtomicUsize::new(0),
            tickets: Mutex::new(Vec::new()),
            store: Some(Arc::clone(&store)),
        });
        let frame = call(
            &context,
            "stable-monitor-input",
            "orchestration_monitor_input",
            json!({"monitor_id":managed_monitor_id(),"text":"hello"}),
        );
        let ports = shared_ports(
            crate::provider_callbacks::managed_tool_ports_with_runtime(
                &store,
                &context,
                &frame,
                managed_ports(Some(fake.clone()), Some(fake.clone())),
            )
            .await
            .unwrap(),
        );
        dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 20.0, ports.clone())
            .await
            .unwrap();
        dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 21.0, ports.clone())
            .await
            .unwrap();
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *fake.tickets.lock().unwrap(),
            ["thread-0:stable-monitor-input"]
        );
        let row = studio_runtime_store::system_reads::all(
            &store.test_reader().unwrap(),
            "runtime_tool_requests",
        )
        .unwrap()
        .into_iter()
        .find(|row| row["callId"] == "stable-monitor-input")
        .unwrap();
        let expected = match certainty {
            crate::managed_host_tools::DeliveryCertainty::Written => "written",
            crate::managed_host_tools::DeliveryCertainty::NotSent => "not_sent",
            crate::managed_host_tools::DeliveryCertainty::Unknown => "unknown",
        };
        assert_eq!(row["deliveryCertainty"], expected);
    }

    let (_root, store, _lead_context) = fixture().await;
    let worker = &template().value["agents"][1];
    let worker_context = CallbackContext {
        agent: worker["id"].as_str().unwrap().to_owned(),
        account: "default".into(),
        connection: "connection".into(),
        generation: 7,
        epoch: worker["epoch"].as_u64().unwrap(),
        thread: worker["threadId"].as_str().unwrap().to_owned(),
        turn: worker["turnId"].as_str().unwrap().to_owned(),
    };
    let fake = Arc::new(ScriptedMonitorPort {
        certainty: crate::managed_host_tools::DeliveryCertainty::Written,
        calls: AtomicUsize::new(0),
        tickets: Mutex::new(Vec::new()),
        store: None,
    });
    let before = application_table_snapshot(&store);
    let frame = call(
        &worker_context,
        "foreign-monitor-input",
        "orchestration_monitor_input",
        json!({"monitor_id":managed_monitor_id(),"text":"no"}),
    );
    let ports = shared_ports(
        crate::provider_callbacks::managed_tool_ports_with_runtime(
            &store,
            &worker_context,
            &frame,
            managed_ports(Some(fake.clone()), Some(fake.clone())),
        )
        .await
        .unwrap(),
    );
    assert!(
        dispatch_with_managed_tools(
            Arc::clone(&store),
            &worker_context,
            &frame,
            20.0,
            ports.clone()
        )
        .await
        .is_err()
    );
    assert_eq!(
        fake.calls.load(Ordering::SeqCst),
        0,
        "foreign input reached runtime"
    );
    let after = application_table_snapshot(&store);
    let before_monitor = before
        .iter()
        .find(|row| row.0 == "runtime_monitors")
        .unwrap();
    let after_monitor = after
        .iter()
        .find(|row| row.0 == "runtime_monitors")
        .unwrap();
    assert_eq!(
        before_monitor, after_monitor,
        "foreign input changed monitor state"
    );
}

#[tokio::test]
async fn each_missing_managed_runtime_port_refuses_without_domain_writes() {
    let cases = [
        (
            "orchestration_status",
            json!({}),
            "model-directory/status-and-dispatch-capacity",
        ),
        (
            "orchestration_spawn",
            json!({"agents":[{"name":"W","prompt":"p"}]}),
            "agent-create/managed-environment",
        ),
        (
            "orchestration_interrupt",
            json!({"agent_id":"worker"}),
            "agent-lifecycle/production-ports",
        ),
        (
            "orchestration_monitor",
            json!({"command":"true"}),
            "workspace-state/monitor-admission",
        ),
        (
            "orchestration_monitor_input",
            json!({"monitor_id":managed_monitor_id(),"text":"x"}),
            "monitor-runtime/managed-input",
        ),
        (
            "orchestration_watch",
            json!({"action":"pause","id":"rule"}),
            "monitor-runtime/rule-cancel",
        ),
    ];
    for (index, (tool, args, refusal)) in cases.into_iter().enumerate() {
        let (_root, store, context) = fixture().await;
        let before = application_table_snapshot(&store);
        let frame = call(&context, &format!("missing-port-{index}"), tool, args);
        let ports = shared_ports(
            crate::provider_callbacks::managed_tool_ports_with_runtime(
                &store,
                &context,
                &frame,
                managed_ports(None, None),
            )
            .await
            .unwrap(),
        );
        let decision =
            dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 20.0, ports.clone())
                .await
                .unwrap();
        assert!(
            format!("{decision:?}").contains(refusal),
            "{tool}: {decision:?}"
        );
        assert_eq!(application_table_snapshot(&store), before, "{tool}");
    }
}

fn call(context: &CallbackContext, rpc: &str, tool: &str, args: Value) -> Frame {
    Frame::Request(RequestFrame {
        id: RequestId::String(format!("rpc-{rpc}")),
        method: Method::ItemToolCall,
        params: Some(json!({
            "threadId":context.thread,
            "turnId":context.turn,
            "callId":rpc,
            "tool":tool,
            "arguments":args
        })),
        fields: Default::default(),
    })
}

fn intent_count(store: &RuntimeStore, kind: &str) -> i64 {
    store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM p05_test_intents WHERE kind=?1",
            [kind],
            |row| row.get(0),
        )
        .unwrap()
}

fn application_table_snapshot(store: &RuntimeStore) -> Vec<(String, Vec<String>)> {
    use rusqlite::types::ValueRef;
    let connection = store.test_reader().unwrap();
    let mut tables = connection
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap();
    let names = tables
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let mut snapshot = Vec::new();
    for name in names {
        // Callback ingress necessarily persists its idempotency/result receipt.
        // The tested operation must leave every other row byte-for-byte stable.
        if matches!(
            name.as_str(),
            "runtime_tool_requests" | "runtime_tool_results" | "runtime_tool_request_aliases"
        ) {
            continue;
        }
        let quoted = name.replace('"', "\"\"");
        let statement = connection
            .prepare(&format!("SELECT * FROM \"{quoted}\""))
            .unwrap();
        let columns = statement.column_count();
        drop(statement);
        let ordering = (1..=columns)
            .map(|column| column.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let receipt_filter = if name == "runtime_operation_receipts" {
            " WHERE signature!='provider-callback'"
        } else {
            ""
        };
        let mut statement = connection
            .prepare(&format!(
                "SELECT * FROM \"{quoted}\"{receipt_filter} ORDER BY {ordering}"
            ))
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                let mut values = Vec::new();
                for column in 0..columns {
                    let value = row.get_ref(column)?;
                    values.push(match value {
                        ValueRef::Null => "null".to_owned(),
                        ValueRef::Integer(value) => format!("i:{value}"),
                        ValueRef::Real(value) => format!("r:{value:?}"),
                        ValueRef::Text(value) => format!("t:{}", String::from_utf8_lossy(value)),
                        ValueRef::Blob(value) => format!("b:{value:?}"),
                    });
                }
                Ok(values.join("\u{1f}"))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        snapshot.push((name, rows));
    }
    snapshot
}

async fn dispatch_case(
    store: &Arc<RuntimeStore>,
    seed: &Value,
    expected: &Value,
    args: Option<Value>,
) -> Result<CallbackDecision, CallbackError> {
    let actor = seed["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|actor| actor["id"] == expected["actor"])
        .unwrap();
    let context = CallbackContext {
        agent: actor["id"].as_str().unwrap().to_owned(),
        account: "default".into(),
        connection: "connection".into(),
        generation: 7,
        epoch: actor["epoch"].as_u64().unwrap(),
        thread: actor["threadId"].as_str().unwrap().to_owned(),
        turn: actor["turnId"].as_str().unwrap().to_owned(),
    };
    let frame = call(
        &context,
        expected["callId"].as_str().unwrap(),
        expected["tool"].as_str().unwrap(),
        args.unwrap_or_else(|| expected["arguments"].clone()),
    );
    let ports = shared_ports(
        crate::provider_callbacks::managed_tool_ports(
            store,
            &context,
            &frame,
            std::sync::Arc::new(tokio::sync::Notify::new()),
        )
        .await?,
    );
    dispatch_with_managed_tools(Arc::clone(store), &context, &frame, 20.0, ports).await
}

#[tokio::test]
async fn named_refusals_leave_all_application_tables_unchanged() {
    let descendant = template().value["agents"][1]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let cases = [
        (
            "orchestration_interrupt",
            json!({"agent_id":descendant}),
            "agent-lifecycle/production-ports",
        ),
        (
            "orchestration_status",
            json!({}),
            "model-directory/status-and-dispatch-capacity",
        ),
        (
            "orchestration_watch",
            json!({"action":"list"}),
            "model-directory/status-and-dispatch-capacity",
        ),
        (
            "orchestration_monitor",
            json!({"command":"true"}),
            "workspace-state/monitor-admission",
        ),
        (
            "orchestration_monitor_input",
            json!({"monitor_id":managed_monitor_id(),"text":"input"}),
            "monitor-runtime/managed-input",
        ),
        (
            "orchestration_spawn",
            json!({"agents":[{"name":"Worker","prompt":"Task"}]}),
            "agent-create/managed-environment",
        ),
        (
            "orchestration_review",
            json!({"target":{"type":"uncommittedChanges"}}),
            "agent-create/managed-environment",
        ),
        (
            "orchestration_context",
            json!({"topic":"tools"}),
            "model-context/preparation-resources",
        ),
    ];
    for (index, (tool, args, dependency)) in cases.into_iter().enumerate() {
        let (_root, store, context) = fixture().await;
        let before = application_table_snapshot(&store);
        let rpc = format!("refusal-{index}");
        let frame = call(&context, &rpc, tool, args);
        let wake = std::sync::Arc::new(tokio::sync::Notify::new());
        let ports = shared_ports(
            crate::provider_callbacks::managed_tool_ports(&store, &context, &frame, wake)
                .await
                .unwrap(),
        );
        let decision =
            dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 20.0, ports.clone())
                .await
                .unwrap();
        assert!(
            format!("{decision:?}").contains(dependency),
            "{tool} did not name {dependency}: {decision:?}"
        );
        let after = application_table_snapshot(&store);
        let changed = before
            .iter()
            .zip(&after)
            .filter_map(|((name, old), (_, new))| (old != new).then_some(name.as_str()))
            .collect::<Vec<_>>();
        assert!(changed.is_empty(), "{tool} changed tables: {changed:?}");
    }
}

#[tokio::test]
async fn managed_monitor_create_persists_one_start_intent_and_replays_exactly() {
    let (_root, store, context) = fixture().await;
    let store = Arc::new(store);
    let frame = call(
        &context,
        "managed-monitor-create",
        "orchestration_monitor",
        json!({"command":"printf managed-monitor","wake_on":"exit"}),
    );
    let wake = std::sync::Arc::new(tokio::sync::Notify::new());
    let monitor_port: Arc<dyn crate::monitor_runtime_wiring::MonitorPort> =
        Arc::new(ScriptedRetainedMonitorPort {
            certainty: crate::managed_host_tools::DeliveryCertainty::Written,
            calls: AtomicUsize::new(0),
            tickets: Mutex::new(Vec::new()),
        });
    let monitor = Arc::new(crate::monitor_runtime_wiring::MonitorRuntime::new(
        Arc::clone(&store),
        monitor_port,
        Arc::new(tokio::sync::Notify::new()),
    ));
    let runtime = crate::provider_callbacks::ManagedToolRuntime::with_creation(
        monitor,
        Arc::new(NoopLifecyclePorts),
        Arc::new(CreationEnvironment),
    );
    let ports = shared_ports(
        crate::provider_callbacks::managed_tool_ports_with_bundle(
            &store,
            &context,
            &frame,
            wake,
            Arc::new(tokio::sync::Notify::new()),
            Some(&runtime),
        )
        .await
        .unwrap(),
    );
    let count_before: i64 = store
        .test_reader()
        .unwrap()
        .query_row("SELECT count(*) FROM runtime_monitors", [], |row| {
            row.get(0)
        })
        .unwrap();
    let first =
        dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 20.0, ports.clone())
            .await
            .unwrap();
    assert!(matches!(first, CallbackDecision::Answer(_)));
    let count: i64 = store
        .test_reader()
        .unwrap()
        .query_row("SELECT count(*) FROM runtime_monitors", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, count_before + 1);
    let saved: Value = serde_json::from_str(
        &store
            .test_reader()
            .unwrap()
            .query_row(
                "SELECT record FROM runtime_monitors WHERE json_extract(record, '$.command')=?1",
                ["printf managed-monitor"],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
    )
    .unwrap();
    assert!(matches!(
        saved["status"].as_str(),
        Some("starting" | "approval")
    ));
    assert!(
        saved["requestId"]
            .as_str()
            .is_some_and(|id| id.ends_with(":managed-monitor-create"))
    );

    let actor_record = {
        let read = store.test_reader().unwrap();
        studio_runtime_store::AgentRepository::get(
            &read,
            &studio_runtime_store::AgentId::from(context.agent.as_str()),
        )
        .unwrap()
        .unwrap()
    };
    let actor = Actor::from_record(
        &actor_record.value(),
        &context.agent,
        u64::try_from(actor_record.epoch).unwrap(),
    )
    .unwrap();
    let arguments = match &frame {
        Frame::Request(request) => request.params.as_ref().unwrap()["arguments"].clone(),
        _ => unreachable!(),
    };
    let monitor_create = runtime.ports().monitor_create.unwrap();
    FAIL_MANAGED_WORKSPACE_PREPARATION.with(|fail| fail.set(true));
    let adapter_replay = crate::provider_callbacks::tools::MonitorCreatePort::create(
        monitor_create.as_ref(),
        &store,
        &actor,
        &arguments,
        saved["requestId"].as_str().unwrap(),
        21.0,
    )
    .await
    .unwrap();
    FAIL_MANAGED_WORKSPACE_PREPARATION.with(|fail| fail.set(false));
    assert_eq!(adapter_replay, saved);

    let replay =
        dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 21.0, ports.clone())
            .await
            .unwrap();
    assert!(matches!(replay, CallbackDecision::Answer(_)));
    let replayed_count: i64 = store
        .test_reader()
        .unwrap()
        .query_row("SELECT count(*) FROM runtime_monitors", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(replayed_count, count_before + 1);
    let replayed: Value = serde_json::from_str(
        &store
            .test_reader()
            .unwrap()
            .query_row(
                "SELECT record FROM runtime_monitors WHERE id=?1",
                [saved["id"].as_str().unwrap()],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(replayed, saved);
    assert_eq!(replay, first);
}

#[tokio::test]
async fn send_remote_routing_refusals_write_nothing() {
    let descendant = template().value["agents"][1]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Runtime.dynamic routes movedFrom actors to move_send before target
    // resolution (codex_runtime.py:9048-9049); these ports attach no paired
    // driver, so the forwarded call names the worker-call dependency. For
    // other actors it resolves agent_id as a visible agent ID, so "user" is a
    // too-short ID; remote and federated targets reach chat_message and use
    // federation locally (codex_runtime.py:9858-9877).
    let cases = [
        (
            "moved descendant",
            descendant,
            json!({"remoteOrigin":"source-server","movedFrom":true}),
            "multi-server/worker-call",
        ),
        (
            "remote to user",
            "user".to_owned(),
            json!({"remoteOrigin":"source-server"}),
            "Too short agent ID; use at least 8 characters",
        ),
        (
            "remote to remote target",
            "remote:peer".to_owned(),
            json!({"remoteOrigin":"source-server"}),
            "federation/agent-message",
        ),
        (
            "remote to federated target",
            "federated:peer:room".to_owned(),
            json!({"remoteOrigin":"source-server"}),
            "federation/agent-message",
        ),
    ];
    for (index, (label, target, flags, dependency)) in cases.into_iter().enumerate() {
        let (_root, store, mut context) = fixture().await;
        if label != "moved descendant" {
            let worker = &template().value["agents"][1];
            context.agent = worker["id"].as_str().unwrap().to_owned();
            context.thread = worker["threadId"].as_str().unwrap().to_owned();
            context.turn = worker["turnId"].as_str().unwrap().to_owned();
        }
        let actor_id = context.agent.clone();
        let flags = flags.clone();
        store
            .transaction_with::<_, StoreError>(move |tx| {
                let raw: String = tx.query_row(
                    "SELECT record FROM runtime_agents WHERE id=?1",
                    [&actor_id],
                    |row| row.get(0),
                )?;
                let mut actor: Value = serde_json::from_str(&raw).unwrap();
                for (key, value) in flags.as_object().unwrap() {
                    actor[key] = value.clone();
                }
                tx.execute(
                    "UPDATE runtime_agents SET record=?2 WHERE id=?1",
                    params![actor_id, actor.to_string()],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let before = application_table_snapshot(&store);
        let frame = call(
            &context,
            &format!("remote-send-{index}"),
            "orchestration_send",
            json!({"agent_id":target,"text":"remote send"}),
        );
        let ports = shared_ports(
            crate::provider_callbacks::managed_tool_ports(
                &store,
                &context,
                &frame,
                std::sync::Arc::new(tokio::sync::Notify::new()),
            )
            .await
            .unwrap(),
        );
        let decision =
            dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 20.0, ports.clone())
                .await
                .unwrap();
        assert!(
            format!("{decision:?}").contains(dependency),
            "{label} did not name {dependency}: {decision:?}"
        );
        assert_eq!(
            before,
            application_table_snapshot(&store),
            "{label} changed application state"
        );
    }
}

#[tokio::test]
async fn deleted_team_root_refuses_complaint_without_writes() {
    let (_root, store, _) = fixture().await;
    let seed = &template().value;
    let expected = &seed["managedRootDeletedRefusal"];
    let worker = seed["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["id"] == expected["actor"])
        .unwrap();
    let root_id = worker["rootId"].as_str().unwrap().to_owned();
    store
        .transaction_with::<_, StoreError>(move |tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.deletedAt',20.0) WHERE id=?1",
                [root_id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let before = application_table_snapshot(&store);
    let decision = dispatch_case(&store, seed, expected, Some(expected["arguments"].clone()))
        .await
        .unwrap();
    let CallbackDecision::Answer(reply) = decision else {
        panic!("deleted-root refusal must return a callback answer")
    };
    assert!(matches!(reply.body, ResponseBody::Result(ref value) if value["success"] == false));
    assert_eq!(before, application_table_snapshot(&store));
}

#[tokio::test]
async fn managed_reads_preserve_corrupt_record_identity() {
    let (_root, store, context) = fixture().await;
    let monitor_id = template().value["managedToolCases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["tool"] == "orchestration_context")
        .unwrap()["arguments"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let search = call(
        &context,
        "corrupt-search",
        "orchestration_search",
        json!({"query":"fixture"}),
    );
    let search_ports = shared_ports(
        crate::provider_callbacks::managed_tool_ports(
            &store,
            &context,
            &search,
            std::sync::Arc::new(tokio::sync::Notify::new()),
        )
        .await
        .unwrap(),
    );
    // Prepare the search directory first; the corrupt row is deliberately
    // outside sync so the read reaches its stored-object validation path.
    let agent_id = context.agent.clone();
    store
        .transaction_with::<_, StoreError>(move |tx| {
            tx.execute(
                "INSERT INTO runtime_work(id,record) VALUES('p05-corrupt-work',?1)",
                [
                    json!({"id":"p05-corrupt-work","rootId":agent_id,"title":17,"status":"ready"})
                        .to_string(),
                ],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let search_result = dispatch_with_managed_tools(
        Arc::clone(&store),
        &context,
        &search,
        20.0,
        Arc::clone(&search_ports),
    )
    .await;
    assert!(
        matches!(
            &search_result,
            Err(CallbackError::Store(StoreError::InvalidStoredObject { table, id }))
                if *table == "runtime_work" && id == "p05-corrupt-work"
        ),
        "corrupt work result: {search_result:?}"
    );

    let agent_id = context.agent.clone();
    let monitor_id_for_tx = monitor_id.clone();
    let monitor_id_for_json = monitor_id_for_tx.clone();
    store
        .transaction_with::<_, StoreError>(move |tx| {
            tx.execute("DELETE FROM runtime_work WHERE id='p05-corrupt-work'", [])?;
            tx.execute(
                "UPDATE runtime_monitors SET record=?1 WHERE id=?2",
                [
                    json!({"id":monitor_id_for_json,"agent":agent_id,"command":17}).to_string(),
                    monitor_id_for_tx,
                ],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let context_call = call(
        &context,
        "corrupt-context",
        "orchestration_context",
        json!({"topic":"monitor","id":monitor_id}),
    );
    let context_ports = shared_ports(
        crate::provider_callbacks::managed_tool_ports(
            &store,
            &context,
            &context_call,
            std::sync::Arc::new(tokio::sync::Notify::new()),
        )
        .await
        .unwrap(),
    );
    let context_result = dispatch_with_managed_tools(
        Arc::clone(&store),
        &context,
        &context_call,
        20.0,
        Arc::clone(&context_ports),
    )
    .await;
    assert!(
        matches!(
            &context_result,
        Err(CallbackError::Store(StoreError::InvalidStoredObject { table, id }))
            if *table == "runtime_monitors" && id == &monitor_id
        ),
        "corrupt monitor result: {context_result:?}"
    );
}

#[tokio::test]
async fn managed_durable_writes_rollback_replay_once_and_reject_changed_content() {
    let seed = &template().value;
    let cases = seed["managedToolCases"].as_array().unwrap();
    let matrix = [
        (0, "runtime_events", "INSERT"),
        (0, "runtime_event_meta", "INSERT"),
        (1, "runtime_rooms", "INSERT"),
        (1, "runtime_chat_messages", "INSERT"),
        (5, "runtime_complaints", "INSERT"),
        (5, "runtime_events", "INSERT"),
        (6, "runtime_complaints", "UPDATE"),
        (7, "runtime_complaints", "UPDATE"),
        (7, "runtime_events", "UPDATE"),
        (7, "runtime_events", "INSERT"),
    ];
    for (case_index, table, operation) in matrix {
        for timing in ["BEFORE", "AFTER"] {
            let (_root, store, _) = fixture().await;
            for previous in &cases[..case_index] {
                assert!(matches!(
                    dispatch_case(&store, seed, previous, None).await.unwrap(),
                    CallbackDecision::Answer(_)
                ));
            }
            let trigger = format!("p05_fail_{case_index}_{timing}_{operation}");
            let create_sql = format!(
                "CREATE TRIGGER {trigger} {timing} {operation} ON {table} BEGIN SELECT RAISE(ABORT,'injected {table} {timing} {operation}'); END"
            );
            store
                .transaction_with::<_, StoreError>(move |tx| {
                    tx.execute_batch(&create_sql)?;
                    Ok(())
                })
                .await
                .unwrap();
            let before = application_table_snapshot(&store);
            let expected = &cases[case_index];
            let first_attempt = dispatch_case(&store, seed, expected, None).await;
            let saved_failure = match first_attempt {
                Err(_) => None,
                Ok(CallbackDecision::Answer(reply)) => {
                    assert!(
                        matches!(&reply.body, ResponseBody::Result(result) if result["success"] == false),
                        "{} {timing} {operation} on {table} unexpectedly succeeded",
                        expected["tool"]
                    );
                    Some(reply.body)
                }
                Ok(other) => panic!("unexpected failure decision: {other:?}"),
            };
            let drop_sql = format!("DROP TRIGGER {trigger}");
            store
                .transaction_with::<_, StoreError>(move |tx| {
                    tx.execute_batch(&drop_sql)?;
                    Ok(())
                })
                .await
                .unwrap();
            assert_eq!(
                before,
                application_table_snapshot(&store),
                "partial write: {table}"
            );
            let first = match dispatch_case(&store, seed, expected, None).await.unwrap() {
                CallbackDecision::Answer(reply) => reply,
                _ => panic!("retry must answer"),
            };
            if let Some(saved) = saved_failure {
                assert_eq!(first.body, saved, "failed reply is durable for {table}");
                assert_eq!(
                    before,
                    application_table_snapshot(&store),
                    "failed operation wrote: {table}"
                );
            }
            let committed = application_table_snapshot(&store);
            let replay = dispatch_case(&store, seed, expected, None).await.unwrap();
            let CallbackDecision::Answer(replay) = replay else {
                panic!("replay must return saved answer")
            };
            assert_eq!(first.body, replay.body, "saved answer for {table}");
            assert_eq!(
                committed,
                application_table_snapshot(&store),
                "duplicate effect: {table}"
            );
            let mut changed = expected["arguments"].clone();
            if let Some(text) = changed["text"].as_str() {
                changed["text"] = json!(format!("{text} changed"));
            } else if let Some(id) = changed["monitor_id"].as_str() {
                changed["monitor_id"] = json!(format!("{id}-changed"));
            } else {
                changed["action"] = json!("changed");
            }
            assert!(
                dispatch_case(&store, seed, expected, Some(changed))
                    .await
                    .is_err()
            );
            assert_eq!(
                committed,
                application_table_snapshot(&store),
                "changed identity wrote: {table}"
            );
        }
    }
}

#[tokio::test]
async fn startup_reconciles_reserved_callback_once() {
    let (root, store, _) = fixture().await;
    let seed = &template().value;
    let expected = &seed["managedToolCases"][0];
    store
        .transaction_with::<_, StoreError>(|tx| {
            tx.execute("DELETE FROM runtime_requests", [])?;
            tx.execute("DELETE FROM runtime_operation_receipts", [])?;
            tx.execute_batch("CREATE TRIGGER p05_crash_after_reservation BEFORE INSERT ON runtime_tool_results BEGIN SELECT RAISE(ABORT,'crash after reservation'); END")?;
            Ok(())
        }).await
        .unwrap();
    let application_before = application_table_snapshot(&store);
    assert!(dispatch_case(&store, seed, expected, None).await.is_err());
    store
        .transaction_with::<_, StoreError>(|tx| {
            tx.execute_batch("DROP TRIGGER p05_crash_after_reservation")?;
            Ok(())
        })
        .await
        .unwrap();
    let running_claim: i64 = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM runtime_tool_requests WHERE json_extract(record,'$.callId')='managed-0' AND json_extract(record,'$.tool')='orchestration_send' AND json_extract(record,'$.stage')='running' AND json_extract(record,'$.outcome')='pending'",

            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        running_claim, 1,
        "the durable claim survives the failed result transaction"
    );
    assert_eq!(
        application_table_snapshot(&store),
        application_before,
        "the failed send transaction wrote no application rows"
    );
    drop(store);

    // Reopen the real store and invoke the callback owner's startup recovery.
    let restarted = RuntimeStore::open(Some(root.path())).await.unwrap();
    assert_eq!(
        crate::provider_callbacks::recover_connection(&restarted, "default", "connection", 30.0)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        crate::provider_callbacks::recover_connection(&restarted, "default", "connection", 31.0)
            .await
            .unwrap(),
        0,
        "a second startup pass must not reconcile the same reservation twice"
    );
    assert_eq!(
        application_table_snapshot(&restarted),
        application_before,
        "recovery does not resend an uncertain managed operation"
    );
    let (stage, outcome): (String, String) = restarted
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT json_extract(record,'$.stage'),json_extract(record,'$.outcome') FROM runtime_tool_requests WHERE json_extract(record,'$.callId')='managed-0' AND json_extract(record,'$.tool')='orchestration_send'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (stage.as_str(), outcome.as_str()),
        ("interrupted", "unknown")
    );
}

#[tokio::test]
async fn every_managed_tool_runs_through_provider_callback_dispatch() {
    let (_root, store, context) = fixture().await;
    let ports = Arc::new(PortBundle::default());
    let worker = template().value["agents"][1]["id"].as_str().unwrap();
    let cases = [
        (
            "orchestration_spawn",
            json!({"agents":[{"name":"Worker","prompt":"Task"}]}),
        ),
        (
            "orchestration_send",
            json!({"agent_id":"worker","text":"continue"}),
        ),
        ("orchestration_interrupt", json!({"agent_id":worker})),
        ("orchestration_status", json!({})),
        ("orchestration_search", json!({"query":"needle"})),
        ("orchestration_context", json!({"topic":"plan"})),
        (
            "orchestration_complaint",
            json!({"action":"submit","text":"blocked"}),
        ),
        ("orchestration_watch", json!({"action":"list"})),
        ("orchestration_monitor", json!({"command":"true"})),
        ("orchestration_review", json!({"target":{"kind":"diff"}})),
    ];
    for (index, (tool, args)) in cases.into_iter().enumerate() {
        let rpc = format!("managed-{index}");
        let frame = call(&context, &rpc, tool, args);
        let decision =
            dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 20.0, ports.clone())
                .await
                .unwrap();
        assert!(matches!(decision, CallbackDecision::Answer(_)), "{tool}");
    }
    // The plan topic has a local transactional projection and Interrupt now
    // dispatches post-commit; the other nine operations use the test bundle.
    assert_eq!(ports.staged.load(Ordering::SeqCst), 9);
    assert_eq!(ports.released.load(Ordering::SeqCst), 9);
}

#[tokio::test]
async fn post_commit_wake_failure_answers_and_replays_without_staging_again() {
    let (_root, store, context) = fixture().await;
    let ports = Arc::new(PortBundle::default());
    ports.fail_release.store(true, Ordering::SeqCst);
    let frame = call(
        &context,
        "rollback-status",
        "orchestration_status",
        json!({}),
    );
    let first =
        dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 20.0, ports.clone())
            .await
            .expect("the staged effect committed, so the call answers");
    assert_eq!(ports.staged.load(Ordering::SeqCst), 1);
    assert_eq!(ports.released.load(Ordering::SeqCst), 0);
    assert_eq!(intent_count(&store, "status"), 1);

    ports.fail_release.store(false, Ordering::SeqCst);
    let replay =
        dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 21.0, ports.clone())
            .await
            .unwrap();
    assert_eq!(replay, first, "the replay rebuilds the same answer");
    assert_eq!(ports.staged.load(Ordering::SeqCst), 1);
    assert_eq!(intent_count(&store, "status"), 1);
}

#[tokio::test]
async fn spawn_intent_is_reconciled_after_restart_when_no_signal_was_released() {
    let (root, store, context) = fixture().await;
    let ports = Arc::new(PortBundle::default());
    ports.skip_release.store(true, Ordering::SeqCst);
    let frame = call(
        &context,
        "crash-spawn",
        "orchestration_spawn",
        json!({"agents":[{"name":"Worker","prompt":"Task"}]}),
    );
    dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 20.0, ports.clone())
        .await
        .unwrap();
    assert_eq!(ports.released.load(Ordering::SeqCst), 0);
    assert_eq!(intent_count(&store, "spawn"), 1);
    drop(store);

    let restarted = RuntimeStore::open(Some(root.path())).await.unwrap();
    for _ in 0..2 {
        restarted
            .transaction_with::<_, StoreError>(|tx| {
                tx.execute(
                    "INSERT OR IGNORE INTO p05_test_starts(child_id) SELECT id FROM p05_test_intents WHERE kind='spawn' AND started=0",
                    [],
                )?;
                tx.execute(
                    "UPDATE p05_test_intents SET started=1 WHERE kind='spawn' AND started=0",
                    [],
                )?;
                Ok(())
            }).await
            .unwrap();
    }
    let starts: i64 = restarted
        .test_reader()
        .unwrap()
        .query_row("SELECT count(*) FROM p05_test_starts", [], |row| row.get(0))
        .unwrap();
    assert_eq!(starts, 1);
}

#[tokio::test]
async fn same_request_replays_saved_result_without_a_second_effect() {
    let (_root, store, context) = fixture().await;
    let ports = Arc::new(PortBundle::default());
    let frame = call(
        &context,
        "exact-replay",
        "orchestration_send",
        json!({"agent_id":"worker","text":"continue"}),
    );
    let first =
        dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 20.0, ports.clone())
            .await
            .unwrap();
    let replay =
        dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 21.0, ports.clone())
            .await
            .unwrap();
    let (CallbackDecision::Answer(first), CallbackDecision::Answer(replay)) = (first, replay)
    else {
        panic!("expected saved answers")
    };
    assert_eq!(first.body, replay.body);
    assert_eq!(ports.staged.load(Ordering::SeqCst), 1);
    assert_eq!(ports.released.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn remote_origin_search_and_monitor_context_stay_local() {
    let (_root, store, context) = fixture().await;
    let monitor_id = template().value["managedToolCases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["tool"] == "orchestration_context")
        .unwrap()["arguments"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let monitor_id_for_tx = monitor_id.clone();
    let monitor_id_for_json = monitor_id_for_tx.clone();
    let agent_id = context.agent.clone();
    let agent_id_for_json = agent_id.clone();
    store
        .transaction_with::<_, StoreError>(move |tx| {
            tx.execute(
                "UPDATE runtime_monitors SET record=?1 WHERE id=?2",
                [json!({"id":monitor_id_for_json,"agent":agent_id_for_json,"command":"printf fixture","created":20.0,"status":"running"}).to_string(), monitor_id_for_tx],
            )?;
            let raw: String = tx.query_row(
                "SELECT record FROM runtime_agents WHERE id=?1",
                [&agent_id],
                |row| row.get(0),
            )?;
            let mut actor: Value = serde_json::from_str(&raw).unwrap();
            actor["remoteOrigin"] = json!(true);
            tx.execute(
                "UPDATE runtime_agents SET record=?2 WHERE id=?1",
                params![agent_id, actor.to_string()],
            )?;
            Ok(())
        }).await
        .unwrap();
    for (rpc, tool, args) in [
        (
            "remote-search",
            "orchestration_search",
            json!({"query":"fixture"}),
        ),
        (
            "remote-monitor-context",
            "orchestration_context",
            json!({"topic":"monitor","id":monitor_id}),
        ),
    ] {
        let frame = call(&context, rpc, tool, args);
        let ports = shared_ports(
            crate::provider_callbacks::managed_tool_ports(
                &store,
                &context,
                &frame,
                std::sync::Arc::new(tokio::sync::Notify::new()),
            )
            .await
            .unwrap(),
        );
        let CallbackDecision::Answer(reply) =
            dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 20.0, ports.clone())
                .await
                .unwrap()
        else {
            panic!("remote-origin local read must answer: {tool}")
        };
        assert!(
            matches!(reply.body, ResponseBody::Result(ref value) if value["success"] == true),
            "{tool}: {:?}",
            reply.body
        );
    }
}

#[tokio::test]
async fn queued_managed_tool_request_can_be_cancelled_before_dispatch() {
    let (_root, store, context) = fixture().await;
    let mut queued = template().value["reservations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|reservation| &reservation["record"])
        .find(|record| record["tool"] == "orchestration_spawn")
        .unwrap()
        .clone();
    let request_id = "thread-0:spawn:p05-cancel";
    queued["id"] = json!(request_id);
    queued["request_id"] = json!("p05-cancel");
    let queued_record = queued.to_string();
    let request_id = request_id.to_owned();
    let request_id_for_tx = request_id.clone();
    let agent_id = context.agent.clone();
    store
        .transaction_with(move |tx| {
            tx.execute(
                "INSERT INTO runtime_tool_requests(id,record) VALUES(?1,?2)",
                params![request_id_for_tx, queued_record],
            )?;
            let stored = studio_runtime_store::system_reads::get(
                tx,
                "runtime_tool_requests",
                &request_id_for_tx,
            )?;
            assert_eq!(stored.as_ref(), Some(&queued));
            let result = crate::tool_requests::execute(
                tx,
                &agent_id,
                crate::tool_requests::Action::Cancel(&request_id_for_tx),
                21.0,
            )?;
            Ok::<_, CallbackError>(result)
        })
        .await
        .unwrap();
    let connection = store.test_reader().unwrap();
    let cancelled =
        studio_runtime_store::system_reads::get(&connection, "runtime_tool_requests", &request_id)
            .unwrap()
            .unwrap();
    assert_eq!(cancelled["stage"], "cancelled");
    assert_eq!(cancelled["cancelRequested"], true);
    drop(connection);
    assert_eq!(intent_count(&store, "spawn"), 0);
}

struct AllowWorkspace;
impl crate::monitor_runtime::WorkspaceAdmission for AllowWorkspace {
    fn check(
        &self,
        _: &studio_runtime_store::Transaction<'_>,
        _: &studio_runtime_store::AgentRecord,
    ) -> Result<(), crate::monitor_policy::MonitorError> {
        Ok(())
    }
}

struct CreationEnvironment;
struct FakeVmReviewGuest;
thread_local! {
    static VM_REVIEW_GUEST_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static IMAGE_PROVISION_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
impl crate::linux_vm_guest::GuestRpc for FakeVmReviewGuest {
    fn stream(
        &self,
        method: &str,
        params: &Value,
        request_id: Option<&str>,
        timeout: std::time::Duration,
        _on_frame: &mut dyn FnMut(Value) -> Result<(), crate::linux_vm_guest::GuestError>,
    ) -> Result<Value, crate::linux_vm_guest::GuestError> {
        VM_REVIEW_GUEST_CALLS.with(|calls| calls.set(calls.get() + 1));
        assert_eq!(method, "line.save");
        assert!(params["agentId"].as_str().is_some_and(|id| !id.is_empty()));
        let turn = params["turnId"].as_str().unwrap();
        let key = turn.strip_prefix("review:").unwrap();
        assert_eq!(
            request_id,
            Some(format!("layr-review-state:{key}").as_str())
        );
        assert_eq!(timeout, std::time::Duration::from_secs(130));
        Ok(json!({"stateId":"guest-state-17"}))
    }
}
thread_local! {
    static FAIL_MANAGED_WORKSPACE_PREPARATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
impl crate::managed_child_create::ManagedCreationEnvironment for CreationEnvironment {
    fn supports_image_workspace(&self) -> bool {
        true
    }

    fn guest_rpc(
        &self,
        _: &RuntimeStore,
    ) -> Result<
        Arc<dyn crate::linux_vm_guest::GuestRpc>,
        crate::managed_child_create::ManagedCreationEnvironmentError,
    > {
        Ok(Arc::new(FakeVmReviewGuest))
    }
    fn select_worker_account(
        &self,
        request: crate::provider_callbacks::WorkerAccountRequest<'_>,
    ) -> Result<
        (String, crate::host_worker_create::AccountIdentity),
        crate::managed_child_create::ManagedCreationEnvironmentError,
    > {
        let crate::provider_callbacks::WorkerAccountRequest {
            parent_account_key,
            requested_account_key,
            default_account_key,
            model,
            excluded_account_keys,
            project_account_keys,
            ..
        } = request;
        if requested_account_key
            .is_some_and(|key| default_account_key.is_some_and(|default| default != key))
        {
            return Err(
                crate::managed_child_create::ManagedCreationEnvironmentError::Invalid(
                    "Worker account differs from team defaults",
                ),
            );
        }
        let key = requested_account_key
            .or(default_account_key)
            .unwrap_or_else(|| {
                if model == "gpt-6-luna" {
                    "alternate"
                } else {
                    parent_account_key
                }
            });
        if excluded_account_keys.iter().any(|excluded| excluded == key)
            || project_account_keys.is_some_and(|keys| {
                !keys.is_empty() && !keys.iter().any(|candidate| candidate == key)
            })
        {
            return Err(
                crate::managed_child_create::ManagedCreationEnvironmentError::Invalid(
                    "Select a worker account of this project",
                ),
            );
        }
        let provider = crate::provider_adapter::model_provider(model)
            .unwrap_or(studio_runtime_providers::ProviderKind::Codex)
            .key();
        Ok((
            key.to_owned(),
            crate::host_worker_create::AccountIdentity {
                provider: provider.parse().expect("test provider"),
                home: None,
                account_id: None,
                email: None,
            },
        ))
    }
    fn catalog(&self) -> &dyn crate::host_worker_create::CreationCatalog {
        &Self
    }
    fn validate_worker_choice<'a>(
        &'a self,
        _: &'a RuntimeStore,
        _: &'a str,
        _: &'a crate::host_worker_create::AccountIdentity,
        _: &'a str,
        _: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        (),
                        crate::managed_child_create::ManagedCreationEnvironmentError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move { Ok(()) })
    }
    fn prepare_workspace(
        &self,
        _: &RuntimeStore,
        _: &studio_runtime_store::AgentRecord,
        _: &Value,
        _: &str,
    ) -> Result<
        Box<dyn crate::monitor_runtime::WorkspaceAdmission + Send + Sync>,
        crate::managed_child_create::ManagedCreationEnvironmentError,
    > {
        if FAIL_MANAGED_WORKSPACE_PREPARATION.with(std::cell::Cell::get) {
            return Err(
                crate::managed_child_create::ManagedCreationEnvironmentError::Unavailable(
                    "workspace-state/host-preparation",
                ),
            );
        }
        Ok(Box::new(AllowWorkspace))
    }

    fn provision_image_workspace(
        &self,
        _: &RuntimeStore,
        _: &std::path::Path,
        _: &str,
    ) -> Result<Value, crate::managed_child_create::ManagedCreationEnvironmentError> {
        IMAGE_PROVISION_CALLS.with(|calls| calls.set(calls.get() + 1));
        Ok(json!({"path":"/fake-image/repo","mount":"/fake-image"}))
    }
}
impl crate::host_worker_create::CreationCatalog for CreationEnvironment {
    fn resolve(
        &self,
        _: &str,
        _: &crate::host_worker_create::AccountIdentity,
        model: &str,
        effort: Option<&str>,
        fast_mode: bool,
        daybreak_enabled: bool,
    ) -> Result<crate::host_worker_create::ExecutionSettings, crate::agents_create::CreateError>
    {
        Ok(crate::host_worker_create::ExecutionSettings {
            model: model.to_owned(),
            effort: effort.map(str::to_owned),
            native_effort: effort.unwrap_or("low").to_owned(),
            fast_mode,
            daybreak_enabled,
            cyber_access_program: "standard".to_owned(),
        })
    }
}

#[allow(clippy::disallowed_methods)]
fn run_git_output(directory: &std::path::Path, arguments: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {arguments:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
async fn dispatcher_service_bundle_calls_spawn_and_review_entry_points() {
    let (_root, store, context) = fixture().await;
    let project_path = CanonicalPath::existing(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(4)
            .unwrap(),
    )
    .unwrap();
    let review_target = project_path.join("workspaces/runtime/apps/server");
    let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("test HOME"));
    std::fs::create_dir_all(&home).unwrap();
    let tilde_target = home.join("review-target");
    if std::fs::symlink_metadata(&tilde_target).is_ok() {
        std::fs::remove_file(&tilde_target).expect("remove stale review target link");
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&review_target, &tilde_target).unwrap();
    let review_cwd = "~/review-target".to_owned();
    // A folder outside every Git repository, shared by both runtimes.
    let no_git_root = tempfile::tempdir().unwrap();
    let no_git_folder = CanonicalPath::existing(no_git_root.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let review_directory = CanonicalPath::existing(&review_target)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let base_commit = run_git_output(&project_path, &["rev-parse", "HEAD~1"]);
    let mut actor_value = {
        let read = store.test_reader().unwrap();
        studio_runtime_store::AgentRepository::get(
            &read,
            &studio_runtime_store::AgentId::from(context.agent.as_str()),
        )
        .unwrap()
        .unwrap()
        .value()
    };
    actor_value["maxAgents"] = json!(64);
    actor_value["cwd"] = json!(project_path);
    actor_value["workspaceInGit"] = json!(true);
    actor_value["workspaceMode"] = json!("worktree");
    actor_value["workerDefaults"] = json!({});
    actor_value["accountKey"] = json!("default");
    actor_value["effort"] = json!("low");
    let actor_value_for_tx = actor_value.clone();
    let project_path_for_tx = project_path.clone();
    store
        .transaction_with::<_, StoreError>(move |tx| {
            let actor_record = studio_runtime_store::AgentRecord::from_value(actor_value_for_tx)
                .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
            studio_runtime_store::AgentRepository::put(tx, &actor_record)?;
            studio_runtime_store::ProjectsRepository::put_project(
                tx,
                &json!({"id":project_path_for_tx.clone(),"path":project_path_for_tx,"name":"corpus",
                    "accountKey":"alternate","accountRevision":1,"workerBaseRef":"HEAD~1"}),
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let actor = Actor::from_record(&actor_value, &context.agent, context.epoch).unwrap();

    let run_id = "run:spawn-review-corpus";
    let run = json!({"id":run_id,"agent":actor_value["id"],"accountKey":"default",
        "epoch":context.epoch as i64,"threadId":actor_value.get("threadId").cloned().unwrap_or(Value::Null),
        "turnId":null,"created":10.0,"status":"pending","firstAttemptId":null,
        "rootAttemptId":null,"latestAttemptId":null});
    let actor_id_for_run = actor_value["id"].as_str().unwrap().to_owned();
    let actor_thread_for_run = actor_value
        .get("threadId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let run_record = run.to_string();
    let epoch = context.epoch as i64;
    store.transaction_with::<_, StoreError>(move |tx| {
        tx.execute("INSERT INTO runtime_execution_runs(id,agent,account,epoch,thread,turn,created,record) VALUES(?1,?2,?3,?4,?5,NULL,10,?6)",
            rusqlite::params![run_id, actor_id_for_run, "default", epoch,
                actor_thread_for_run, run_record])?;
        Ok(())
    }).await.unwrap();

    store
        .transaction_with::<_, StoreError>(|tx| {
            studio_runtime_store::ProfileRepository::save(
                tx,
                &studio_runtime_store::ProfileRecord {
                    id: "profile-reviewer".to_owned(),
                    name: "Profile reviewer".to_owned(),
                    role: studio_runtime_store::ProfileRole::Implementer,
                    model: "gpt-6-luna".to_owned(),
                    effort: None,
                    instructions: "profile instructions".to_owned(),
                },
            )?;
            Ok(())
        })
        .await
        .unwrap();

    // These are the exact value, identity and clock types passed by the
    // managed callback service bundle after its durable claim commits.
    let spawned = crate::managed_child_create::spawn_agents(
        &store,
        &actor,
        &json!({"agents":[{"name":"Worker","prompt":"Run the task","workspace":"worktree",
            "account_key":"alternate","profile_id":"profile-reviewer","effort":null}]}),
        "dispatcher-spawn-api",
        20.0,
        &CreationEnvironment,
    )
    .await
    .unwrap();
    assert_eq!(spawned.value["agents"].as_array().unwrap().len(), 1);
    assert_eq!(spawned.value["requestId"], "dispatcher-spawn-api");
    let spawned_child_id = spawned.value["agents"][0]["id"].as_str().unwrap();
    let read = store.test_reader().unwrap();
    let spawned_child = studio_runtime_store::AgentRepository::get(
        &read,
        &studio_runtime_store::AgentId::from(spawned_child_id),
    )
    .unwrap()
    .unwrap()
    .value();
    assert_eq!(spawned_child["accountKey"], "alternate");
    assert_eq!(spawned_child["model"], "gpt-6-luna");
    assert_eq!(spawned_child["effort"], Value::Null);
    assert_eq!(spawned_child["nativeEffort"], "low");
    assert_eq!(spawned_child["profileId"], "profile-reviewer");
    assert_eq!(spawned_child["profileInstructions"], "profile instructions");
    assert_eq!(spawned.value["agents"][0]["baseRef"], "HEAD~1");
    assert_eq!(spawned.value["agents"][0]["baseCommit"], base_commit);
    drop(read);

    let fallback = crate::managed_child_create::spawn_agents(
        &store,
        &actor,
        &json!({"agents":[{"name":"Fallback","prompt":"Use the first available account",
            "model":"gpt-6-luna","workspace":"shared"}]}),
        "dispatcher-spawn-fallback",
        20.5,
        &CreationEnvironment,
    )
    .await
    .unwrap();
    let fallback_id = fallback.value["agents"][0]["id"].as_str().unwrap();
    let fallback_child = studio_runtime_store::AgentRepository::get(
        &store.test_reader().unwrap(),
        &studio_runtime_store::AgentId::from(fallback_id),
    )
    .unwrap()
    .unwrap()
    .value();
    assert_eq!(fallback_child["accountKey"], "alternate");
    assert_eq!(fallback_child["profileId"], Value::Null);
    assert_eq!(fallback_child["profileInstructions"], "");
    // A relative cwd starts at the parent's folder.
    let alternate = crate::managed_child_create::spawn_agents(
        &store,
        &actor,
        &json!({"agents":[{"name":"Alternate","prompt":"Work in the server folder",
            "model":"gpt-6-luna","workspace":"shared","cwd":"workspaces/runtime/apps/server"}]}),
        "dispatcher-spawn-alternate",
        20.75,
        &CreationEnvironment,
    )
    .await
    .expect("a host worker may work in another existing folder");
    let alternate_id = alternate.value["agents"][0]["id"].as_str().unwrap();
    let alternate_child = studio_runtime_store::AgentRepository::get(
        &store.test_reader().unwrap(),
        &studio_runtime_store::AgentId::from(alternate_id),
    )
    .unwrap()
    .unwrap()
    .value();
    assert_eq!(alternate_child["cwd"], json!(review_directory));
    // A workspace worktree, base_ref and another
    // folder: the child keeps the folder as cwd and gets a worktree there.
    let alternate_worktree = crate::managed_child_create::spawn_agents(
        &store,
        &actor,
        &json!({"agents":[{"name":"Alternate worktree",
            "prompt":"Work in a worktree of the server folder","model":"gpt-6-luna",
            "workspace":"worktree","base_ref":"HEAD","cwd":"workspaces/runtime/apps/server"}]}),
        "dispatcher-spawn-alternate-worktree",
        20.76,
        &CreationEnvironment,
    )
    .await
    .expect("a worktree child may start in another repository folder");
    let child_id = alternate_worktree.value["agents"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let actual_child = studio_runtime_store::AgentRepository::get(
        &store.test_reader().unwrap(),
        &studio_runtime_store::AgentId::from(child_id.as_str()),
    )
    .unwrap()
    .unwrap()
    .value();
    assert_eq!(actual_child["worktree"], true);
    assert_eq!(actual_child["cwd"], json!(review_directory));

    // The parent's default worktree mode in a folder without Git uses the
    // shared folder, records the note and the warning, and tells the worker.
    let no_git = crate::managed_child_create::spawn_agents(
        &store,
        &actor,
        &json!({"agents":[{"name":"No Git","prompt":"Work in a folder without Git",
            "model":"gpt-6-luna","cwd":no_git_folder}]}),
        "dispatcher-spawn-no-git",
        20.77,
        &CreationEnvironment,
    )
    .await
    .expect("a default worktree outside Git uses the shared folder");
    let child_id = no_git.value["agents"][0]["id"].as_str().unwrap().to_owned();
    let initial: String = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT text FROM runtime_events WHERE id=?1",
            [format!("{child_id}:initial")],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        !initial.is_empty(),
        "the no-Git child gets an initial message"
    );
    let agents_before_folder_refusals: i64 = store
        .test_reader()
        .unwrap()
        .query_row("SELECT count(*) FROM runtime_agents", [], |row| row.get(0))
        .unwrap();
    for (index, (cwd, message)) in [
        (
            json!("missing-worker-folder"),
            "cwd must be an existing folder",
        ),
        (json!(7), "cwd must be a folder path"),
        (json!("   "), "cwd must be a folder path"),
    ]
    .into_iter()
    .enumerate()
    {
        let refused = crate::managed_child_create::spawn_agents(
            &store,
            &actor,
            &json!({"agents":[{"name":"Refused","prompt":"Work elsewhere","workspace":"shared","cwd":cwd}]}),
            &format!("dispatcher-spawn-folder-refusal-{index}"),
            20.8,
            &CreationEnvironment,
        )
        .await
        .expect_err("an invalid worker folder is refused");
        assert!(refused.to_string().contains(message), "{refused}");
    }
    let git_refusals = [
        json!({"workspace": "worktree", "cwd": no_git_folder}),
        json!({"base_ref": "HEAD", "cwd": no_git_folder}),
        json!({"role": "reviewer", "base_ref": "HEAD"}),
        json!({"workspace": "shared", "base_ref": "HEAD"}),
    ];
    for (index, case) in git_refusals.iter().enumerate() {
        let mut spec = json!({"name":"Refused","prompt":"Work elsewhere"});
        for (key, value) in case.as_object().unwrap() {
            spec[key] = value.clone();
        }
        let refused = crate::managed_child_create::spawn_agents(
            &store,
            &actor,
            &json!({"agents":[spec]}),
            &format!("dispatcher-spawn-git-refusal-{index}"),
            20.9,
            &CreationEnvironment,
        )
        .await
        .expect_err("this Git workspace request is refused");
        assert!(!refused.to_string().is_empty(), "{case}");
    }
    let agents_after_folder_refusals: i64 = store
        .test_reader()
        .unwrap()
        .query_row("SELECT count(*) FROM runtime_agents", [], |row| row.get(0))
        .unwrap();
    assert_eq!(agents_before_folder_refusals, agents_after_folder_refusals);
    let before_workspace_refusal: i64 = store
        .test_reader()
        .unwrap()
        .query_row("SELECT count(*) FROM runtime_agents", [], |row| row.get(0))
        .unwrap();
    assert!(crate::managed_child_create::spawn_agents(
        &store,
        &actor,
        &json!({"agents":[{"name":"Reviewer","role":"reviewer","prompt":"Review","workspace":"shared"}]}),
        "dispatcher-workspace-refusal",
        20.25,
        &CreationEnvironment,
    ).await
    .is_err());
    let after_workspace_refusal: i64 = store
        .test_reader()
        .unwrap()
        .query_row("SELECT count(*) FROM runtime_agents", [], |row| row.get(0))
        .unwrap();
    assert_eq!(before_workspace_refusal, after_workspace_refusal);

    let provision_calls_before = IMAGE_PROVISION_CALLS.get();
    let image_base = crate::managed_child_create::spawn_agents(
        &store,
        &actor,
        &json!({"agents":[{"name":"Image Base","prompt":"Review the requested image base","workspace":"image","base_ref":"HEAD~1"}]}),
        "dispatcher-image-base-ref",
        20.5,
        &CreationEnvironment,
    ).await
    .expect("image base refs resolve to a worker checkout instruction");
    assert_eq!(IMAGE_PROVISION_CALLS.get(), provision_calls_before + 1);
    let image_base_id = image_base.value["agents"][0]["id"].as_str().unwrap();
    let image_base_child = studio_runtime_store::AgentRepository::get(
        &store.test_reader().unwrap(),
        &studio_runtime_store::AgentId::from(image_base_id),
    )
    .unwrap()
    .unwrap()
    .value();
    assert_eq!(image_base_child["imageWorkspaceReady"], true);
    assert_eq!(image_base_child["imageWorkspacePhase"], "ready");
    assert_eq!(image_base_child["cwd"], "/fake-image/repo");
    assert_eq!(image_base.value["agents"][0]["cwd"], "/fake-image/repo");
    assert_eq!(image_base.value["agents"][0]["imageWorkspaceReady"], true);
    let published_image_child: String = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?1",
            [image_base_id],
            |row| row.get(0),
        )
        .unwrap();
    let published_image_child: Value = serde_json::from_str(&published_image_child).unwrap();
    assert_eq!(published_image_child["value"]["cwd"], "/fake-image/repo");
    assert_eq!(published_image_child["value"]["imageWorkspaceReady"], true);
    assert_eq!(image_base_child["imageWorkspaceBaseRef"], "HEAD~1");
    assert_eq!(image_base_child["workerBaseCommit"], base_commit);
    let image_base_event: String = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT text FROM runtime_events WHERE id=?1",
            [format!("{image_base_id}:initial")],
            |row| row.get(0),
        )
        .unwrap();
    assert!(image_base_event.contains("Check out this commit yourself before editing."));

    let reviewed = crate::managed_child_create::request_review(
        &store,
        &actor,
        &json!({"target":{"type":"uncommittedChanges"},"cwd":review_cwd}),
        "dispatcher-review-api",
        21.0,
        &CreationEnvironment,
    )
    .await
    .unwrap_or_else(|error| panic!("review API call: {error:?}"));
    assert_eq!(reviewed.value["requestId"], "dispatcher-review-api");
    assert_eq!(reviewed.value["status"], "queued");
    let child = reviewed.value["agents"][0]["id"].as_str().unwrap();
    let read = store.test_reader().unwrap();
    let child = studio_runtime_store::AgentRepository::get(
        &read,
        &studio_runtime_store::AgentId::from(child),
    )
    .unwrap()
    .unwrap()
    .value();
    assert_eq!(child["cwd"], review_directory);
    assert_eq!(child["nativeReview"]["status"], "pending");
    drop(read);

    actor_value["executionMode"] = json!("vm");
    actor_value["workspaceMode"] = json!("layr");
    actor_value["cwd"] = json!("/home/studio/project");
    actor_value["hostProjectPath"] = json!(project_path);
    actor_value["layrReady"] = json!(true);
    actor_value["layrProjectId"] = json!("project-17");
    actor_value["layrLine"] = json!("main");
    actor_value["provider"] = json!("codex");
    actor_value["isLead"] = json!(true);
    let actor_value_for_vm_tx = actor_value.clone();
    store
        .transaction_with::<_, StoreError>(move |tx| {
            let record = studio_runtime_store::AgentRecord::from_value(actor_value_for_vm_tx)
                .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
            studio_runtime_store::AgentRepository::put(tx, &record)?;
            Ok(())
        })
        .await
        .unwrap();
    let vm_actor = Actor::from_record(&actor_value, &context.agent, context.epoch).unwrap();
    VM_REVIEW_GUEST_CALLS.set(0);
    let before_vm_review: i64 = store
        .test_reader()
        .unwrap()
        .query_row("SELECT count(*) FROM runtime_agents", [], |row| row.get(0))
        .unwrap();
    let vm_review_error = crate::managed_child_create::request_review(
        &store,
        &vm_actor,
        &json!({"target":{"type":"uncommittedChanges"}}),
        "dispatcher-vm-review-api",
        22.0,
        &CreationEnvironment,
    )
    .await
    .expect_err("VM review creation is unavailable until VM workspace preparation exists");
    assert!(matches!(
        vm_review_error,
        crate::managed_child_create::Error::Managed(crate::managed_host_tools::Error::Unavailable(
            "workspace-state/vm-workspace"
        ))
    ));
    let after_vm_review: i64 = store
        .test_reader()
        .unwrap()
        .query_row("SELECT count(*) FROM runtime_agents", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        before_vm_review, after_vm_review,
        "no VM reviewer was enqueued"
    );
    assert_eq!(VM_REVIEW_GUEST_CALLS.get(), 0, "refusal precedes line.save");
    let after_uncertain_review: i64 = store
        .test_reader()
        .unwrap()
        .query_row("SELECT count(*) FROM runtime_agents", [], |row| row.get(0))
        .unwrap();
    assert_eq!(before_vm_review, after_uncertain_review);
    let root_agent_id = context.agent.clone();
    store
        .transaction_with::<_, StoreError>(move |tx| {
            let root = studio_runtime_store::AgentRepository::get(
                tx,
                &studio_runtime_store::AgentId::from(root_agent_id.as_str()),
            )?
            .unwrap();
            let mut root_value = root.value();
            root_value["maxAgents"] = json!(1);
            let root = studio_runtime_store::AgentRecord::from_value(root_value)
                .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
            studio_runtime_store::AgentRepository::put(tx, &root)?;
            Ok(())
        })
        .await
        .unwrap();
    let latest_actor_value = {
        let read = store.test_reader().unwrap();
        studio_runtime_store::AgentRepository::get(
            &read,
            &studio_runtime_store::AgentId::from(context.agent.as_str()),
        )
        .unwrap()
        .unwrap()
        .value()
    };
    let latest_actor =
        Actor::from_record(&latest_actor_value, &context.agent, context.epoch).unwrap();
    let before_capacity: i64 = store
        .test_reader()
        .unwrap()
        .query_row("SELECT count(*) FROM runtime_agents", [], |row| row.get(0))
        .unwrap();
    assert!(crate::managed_child_create::request_review(
        &store,
        &latest_actor,
        &json!({"target":{"type":"uncommittedChanges"},"cwd":format!("{review_cwd}/workspaces/runtime/apps/server")}),
        "dispatcher-capacity-refusal",
        22.0,
        &CreationEnvironment,
    ).await
    .is_err());
    let after_capacity: i64 = store
        .test_reader()
        .unwrap()
        .query_row("SELECT count(*) FROM runtime_agents", [], |row| row.get(0))
        .unwrap();
    assert_eq!(before_capacity, after_capacity);
}

#[tokio::test]
async fn spawn_child_and_first_turn_failpoints_roll_back_the_batch() {
    for failpoint in ["child", "first-turn"] {
        let (_root, store, context) = fixture().await;
        let actor_value = {
            let read = store.test_reader().unwrap();
            studio_runtime_store::AgentRepository::get(
                &read,
                &studio_runtime_store::AgentId::from(context.agent.as_str()),
            )
            .unwrap()
            .unwrap()
            .value()
        };
        let actor = Actor::from_record(&actor_value, &context.agent, context.epoch).unwrap();
        let request_id = format!("spawn-failpoint-{failpoint}");
        let child_id = uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            format!("{request_id}:0").as_bytes(),
        )
        .to_string();
        let failpoint_sql = if failpoint == "child" {
            format!(
                "CREATE TRIGGER reject_child BEFORE INSERT ON runtime_agents WHEN NEW.id='{child_id}' BEGIN SELECT RAISE(ABORT,'child failpoint'); END;"
            )
        } else {
            format!(
                "CREATE TRIGGER reject_initial BEFORE INSERT ON runtime_events WHEN NEW.id='{child_id}:initial' BEGIN SELECT RAISE(ABORT,'first turn failpoint'); END;"
            )
        };
        store
            .transaction_with::<_, StoreError>(move |tx| {
                tx.execute_batch(&failpoint_sql)?;
                Ok(())
            })
            .await
            .unwrap();
        let args =
            json!({"agents":[{"name":"Worker","prompt":"Do the task","workspace":"shared"}]});
        assert!(
            crate::managed_child_create::spawn_agents(
                &store,
                &actor,
                &args,
                &request_id,
                20.0,
                &CreationEnvironment,
            )
            .await
            .is_err(),
            "expected {failpoint} write failure"
        );
        let read = store.test_reader().unwrap();
        assert!(
            studio_runtime_store::AgentRepository::get(
                &read,
                &studio_runtime_store::AgentId::from(child_id.as_str()),
            )
            .unwrap()
            .is_none(),
            "child row survived {failpoint} failure"
        );
        let events: i64 = read
            .query_row(
                "SELECT COUNT(*) FROM runtime_events WHERE id=?1",
                [format!("{child_id}:initial")],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(events, 0, "initial event survived {failpoint} failure");
    }
}

/// A team spawns past its running limit: the extra workers wait queued under
/// `team_concurrency`. A batch past the stored team size is refused with a
/// typed error and creates nothing. Both providers' leads.
#[tokio::test]
async fn spawn_past_the_running_limit_queues_and_past_the_team_size_refuses() {
    for (provider, model) in [("codex", "gpt-6-luna"), ("claude", "claude-haiku-4-5")] {
        let (_root, store, context) = fixture().await;
        let mut lead = {
            let read = store.test_reader().unwrap();
            studio_runtime_store::AgentRepository::get(
                &read,
                &studio_runtime_store::AgentId::from(context.agent.as_str()),
            )
            .unwrap()
            .unwrap()
            .value()
        };
        lead["provider"] = json!(provider);
        lead["model"] = json!(model);
        lead["workerDefaults"] = json!({"model": model});
        let active = studio_runtime_store::AgentIndexedReads::active_team_members(
            &store.test_reader().unwrap(),
            lead["rootId"].as_str().unwrap(),
        )
        .unwrap();
        let running = {
            let read = store.test_reader().unwrap();
            read.query_row(
                "SELECT count(*) FROM runtime_agents WHERE json_extract(record,'$.rootId')=?1 \
                 AND id!=?1 AND json_extract(record,'$.status') IN ('running','starting','approval')",
                [lead["rootId"].as_str().unwrap()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
        };
        lead["concurrency"] = json!(running + 1);
        lead["maxAgents"] = json!(active + 2);
        let stored = lead.clone();
        store
            .transaction_with::<_, StoreError>(move |tx| {
                let record = studio_runtime_store::AgentRecord::from_value(stored)
                    .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
                studio_runtime_store::AgentRepository::put(tx, &record)
            })
            .await
            .unwrap();
        let actor = Actor::from_record(&lead, &context.agent, context.epoch).unwrap();
        let batch = |names: &[&str]| {
            json!({"agents": names.iter().map(|name| json!({
                "name": name, "prompt": "Do the task", "workspace": "shared"
            })).collect::<Vec<_>>()})
        };
        let spawned = crate::managed_child_create::spawn_agents(
            &store,
            &actor,
            &batch(&["One", "Two"]),
            &format!("{provider}-within-team-size"),
            20.0,
            &CreationEnvironment,
        )
        .await
        .unwrap_or_else(|error| panic!("{provider} spawn within the team size: {error:?}"));
        let children = spawned.value["agents"].as_array().unwrap();
        assert_eq!(children.len(), 2, "{provider}");
        let first = children[0]["id"].as_str().unwrap().to_owned();
        store
            .transaction_with::<_, StoreError>(move |tx| {
                let id = studio_runtime_store::AgentId::from(first.as_str());
                let mut child = studio_runtime_store::AgentRepository::get(tx, &id)?
                    .expect("spawned child")
                    .value();
                assert_eq!(child["status"], "queued");
                child["status"] = json!("running");
                let record = studio_runtime_store::AgentRecord::from_value(child)
                    .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
                studio_runtime_store::AgentRepository::put(tx, &record)
            })
            .await
            .unwrap();
        let snapshot = crate::managed_status_watch::model_directory(
            &store,
            &context.agent,
            context.epoch,
            &json!({}),
            21.0,
        )
        .await
        .unwrap();
        let capacity = &snapshot["capacity"];
        assert_eq!(capacity["teamLimit"], running + 1, "{provider}");
        assert_eq!(capacity["queuedCount"], 1, "{provider}");
        assert_eq!(
            capacity["queued"][0]["id"], children[1]["id"],
            "{provider}: the second worker waits for a running slot"
        );
        assert_eq!(
            capacity["queued"][0]["reasons"][0]["reason"], "dispatch_pending",
            "{provider}: status must not infer an account gate from team counts: {capacity}"
        );

        let count = |store: &RuntimeStore| -> i64 {
            store
                .test_reader()
                .unwrap()
                .query_row("SELECT count(*) FROM runtime_agents", [], |row| row.get(0))
                .unwrap()
        };
        let before = count(&store);
        let refused = crate::managed_child_create::spawn_agents(
            &store,
            &actor,
            &batch(&["Three"]),
            &format!("{provider}-past-team-size"),
            22.0,
            &CreationEnvironment,
        )
        .await
        .expect_err("the batch exceeds the stored team size");
        assert!(
            matches!(
                refused,
                crate::managed_child_create::Error::Managed(Error::Invalid(message))
                    if message.contains("active agent limit")
            ),
            "{provider}: {refused:?}"
        );
        assert_eq!(count(&store), before, "{provider}: nothing was created");
    }
}

#[path = "ingress_tests/claim_settlement.rs"]
mod claim_settlement;

/// Records each task action the managed tool hands to the work-action owner.
struct ScriptedWorkPort {
    calls: Mutex<Vec<(String, Value, String)>>,
    outcome: crate::provider_callbacks::WorkActionOutcome,
}

impl crate::provider_callbacks::WorkActionPort for ScriptedWorkPort {
    fn act<'a>(
        &'a self,
        actor_id: &'a str,
        args: &'a Value,
        request_id: &'a str,
    ) -> futures_util::future::BoxFuture<'a, crate::provider_callbacks::WorkActionOutcome> {
        self.calls
            .lock()
            .unwrap()
            .push((actor_id.to_owned(), args.clone(), request_id.to_owned()));
        let outcome = self.outcome.clone();
        Box::pin(async move { outcome })
    }
}

async fn work_ports(
    store: &Arc<RuntimeStore>,
    context: &CallbackContext,
    frame: &Frame,
    port: Arc<ScriptedWorkPort>,
) -> Arc<dyn crate::managed_host_tools::Ports<Prepared = ()>> {
    shared_ports(
        crate::provider_callbacks::managed_tool_ports_with_runtime(
            store.as_ref(),
            context,
            frame,
            ManagedToolPortsConfig {
                wake: Arc::new(tokio::sync::Notify::new()),
                monitor_dispatch_wake: Arc::new(tokio::sync::Notify::new()),
                runtime_ports: ManagedToolRuntimePorts {
                    status_watch: None,
                    child_creation: None,
                    monitor_create: None,
                    interrupt: None,
                    monitor_input: None,
                    monitor_cancel: None,
                    work_action: Some(port),
                    late: Default::default(),
                },
                preparation: None,
            },
        )
        .await
        .unwrap(),
    )
}

fn tool_reply(decision: CallbackDecision) -> (bool, Value) {
    let CallbackDecision::Answer(reply) = decision else {
        panic!("managed callback must answer")
    };
    let ResponseBody::Result(actual) = reply.body else {
        panic!("managed callback result expected")
    };
    let text = actual["contentItems"][0]["text"].as_str().unwrap();
    (
        actual["success"] == true,
        serde_json::from_str(text).unwrap_or_else(|_| json!(text)),
    )
}

#[tokio::test]
async fn task_claim_submit_and_accept_run_through_the_work_action_owner_once() {
    // Live cutover 4: every claim, submit and accept through orchestration_task
    // answered "requires work-board/assignment-and-result-driver".
    let (_root, store, context) = fixture_arc().await;
    let port = Arc::new(ScriptedWorkPort {
        calls: Mutex::new(Vec::new()),
        outcome: crate::provider_callbacks::WorkActionOutcome::Written(json!({
            "id":"task-1","title":"Ship it","owner":"worker","status":"accepted","version":4,
            "blockedBy":[],"description":"not in the brief",
            "archive":{"status":"kept","reason":"The owner has another open task","tasks":["task-2"]}
        })),
    });
    for (rpc, action) in [
        ("task-claim", "claim"),
        ("task-submit", "submit"),
        ("task-accept", "accept"),
    ] {
        let frame = call(
            &context,
            rpc,
            "orchestration_task",
            json!({"action":action,"task_id":"task-1","version":3,"result":"done"}),
        );
        let ports = work_ports(&store, &context, &frame, Arc::clone(&port)).await;
        let first =
            dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 20.0, ports.clone())
                .await
                .unwrap();
        let (success, value) = tool_reply(first);
        assert!(success, "{action}: {value}");
        assert_eq!(
            value,
            json!({"id":"task-1","title":"Ship it","owner":"worker","status":"accepted",
                "version":4,"blockedBy":[],
                "archive":{"status":"kept","reason":"The owner has another open task"},
                "requestId":value["requestId"],
                "detail":{"tool":"orchestration_task","action":"get","task_id":"task-1"}}),
            "{action}: Python task_brief, requestId and detail"
        );
        // A retried callback answers from its receipt and never acts again.
        let (_, replay) = tool_reply(
            dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 21.0, ports)
                .await
                .unwrap(),
        );
        assert_eq!(replay, value, "{action}");
    }
    let calls = port.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 3, "one owner call per action");
    for ((actor, args, request), action) in calls.iter().zip(["claim", "submit", "accept"]) {
        assert_eq!(actor, &context.agent);
        assert_eq!(args["action"], action);
        assert!(!request.is_empty());
    }

    // Python reports the owner's refusal to the model; nothing is applied.
    let refusing = Arc::new(ScriptedWorkPort {
        calls: Mutex::new(Vec::new()),
        outcome: crate::provider_callbacks::WorkActionOutcome::Refused(
            "Only the owner can submit this task".to_owned(),
        ),
    });
    let frame = call(
        &context,
        "task-refused",
        "orchestration_task",
        json!({"action":"submit","task_id":"task-1","version":3,"result":"done"}),
    );
    let ports = work_ports(&store, &context, &frame, refusing).await;
    let (success, value) = tool_reply(
        dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 22.0, ports)
            .await
            .unwrap(),
    );
    assert!(!success);
    assert!(
        value
            .to_string()
            .contains("Only the owner can submit this task"),
        "{value}"
    );
}

#[tokio::test]
async fn lead_archive_and_restore_run_through_the_lifecycle_owner() {
    // Live cutover 4: orchestration_agent_manage answered "requires
    // agent-management/runtime-driver" for every action.
    let (_root, store, context) = fixture_arc().await;
    let worker = template().value["agents"][1]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let worker_for_finish = worker.clone();
    store
        .transaction_with::<_, StoreError>(move |tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.status','completed','$.turnId',NULL,\
                 '$.inFlight',json('false'),'$.lastCompletedTurn','turn-1','$.lastCompletedTurnStatus','completed') \
                 WHERE id=?1",
                [&worker_for_finish],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let manage = |rpc: &'static str, args: Value| {
        let store = Arc::clone(&store);
        let context = context.clone();
        async move {
            let frame = call(&context, rpc, "orchestration_agent_manage", args);
            let ports = shared_ports(
                crate::provider_callbacks::managed_tool_ports(
                    &store,
                    &context,
                    &frame,
                    Arc::new(tokio::sync::Notify::new()),
                )
                .await
                .unwrap(),
            );
            tool_reply(
                dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 30.0, ports)
                    .await
                    .unwrap(),
            )
        }
    };
    let worker_record = |store: &RuntimeStore, id: &str| -> Value {
        studio_runtime_store::AgentRepository::get(
            &store.test_reader().unwrap(),
            &studio_runtime_store::AgentId::from(id),
        )
        .unwrap()
        .unwrap()
        .value()
    };

    let before = application_table_snapshot(&store);
    let (success, value) = manage(
        "manage-archive-unassign",
        json!({"action":"restore","agent_id":worker,"unassign_work":true}),
    )
    .await;
    assert!(!success);
    assert_eq!(
        value,
        json!("unassign_work applies only to archive or archive_finished")
    );
    let (success, value) = manage(
        "manage-archive-no-reason",
        json!({"action":"archive","agent_id":worker}),
    )
    .await;
    assert!(!success);
    assert_eq!(
        value,
        json!("Give an archive reason with 1 to 1000 characters")
    );
    assert_eq!(
        before,
        application_table_snapshot(&store),
        "refusals change no application rows"
    );

    let (success, archived) = manage(
        "manage-archive",
        json!({"action":"archive","agent_id":worker[..8],"reason":"Finished work"}),
    )
    .await;
    assert!(success, "{archived}");
    assert_eq!(archived["status"], "archived", "{archived}");
    let record = worker_record(&store, &worker);
    assert!(record["deletedAt"].is_number(), "{record}");
    assert_eq!(record["agentArchive"]["reason"], "Finished work");
    assert_eq!(
        record["agentArchive"]["cleanupPending"], false,
        "cleanup completes after the archive commits: {record}"
    );
    assert!(record.get("workspaceCleanupIntent").is_none(), "{record}");

    let (success, inspected) = manage(
        "manage-inspect",
        json!({"action":"inspect","agent_id":worker}),
    )
    .await;
    assert!(success, "{inspected}");
    assert_eq!(inspected["canArchive"], false);
    assert_eq!(inspected["blockers"], json!([]));

    let (success, restored) = manage(
        "manage-restore",
        json!({"action":"restore","agent_id":worker,"unassign_work":false}),
    )
    .await;
    assert!(success, "{restored}");
    assert_eq!(restored["status"], "restored", "{restored}");
    let record = worker_record(&store, &worker);
    assert!(record.get("deletedAt").is_none(), "{record}");
    assert_eq!(record["status"], "paused");
    assert_eq!(record["autoWake"], false);
    let (success, batch) = manage(
        "manage-archive-finished",
        json!({"action":"archive_finished","request_id":"finished-batch"}),
    )
    .await;
    assert!(success, "{batch}");
    let report: crate::agent_management_store::ArchiveFinishedResult =
        serde_json::from_value(batch.clone()).expect("typed archive summary");
    assert_eq!(report.archived, 1);
    let (success, replay) = manage(
        "manage-archive-finished",
        json!({"action":"archive_finished","request_id":"finished-batch"}),
    )
    .await;
    assert!(success, "{replay}");
    assert_eq!(
        replay, batch,
        "lost tool reply replays the original batch result"
    );
    assert!(worker_record(&store, &worker)["deletedAt"].is_number());
}

#[tokio::test]
async fn archive_finished_runs_for_codex_and_claude_leads_through_tool_dispatch() {
    // A lead on an older live build got "requires agent-management/archive-finished"
    // although the turn reminder told it to run this action. The tool dispatch is
    // the same for both providers; this keeps each provider's lead on that path.
    for (provider, model) in [("codex", "gpt-6-astra"), ("claude", "claude-opus-5-5")] {
        let (_root, store, context) = fixture_arc().await;
        store
            .transaction_with::<_, StoreError>(move |tx| {
                tx.execute(
                    "UPDATE runtime_agents SET record=json_set(record,'$.provider',?1,'$.model',?2) \
                     WHERE id IN (?3,?4)",
                    params![provider, model, LEAD, WORKER],
                )?;
                tx.execute(
                    "UPDATE runtime_agents SET record=json_set(record,'$.status','completed','$.turnId',NULL,\
                     '$.inFlight',json('false'),'$.lastCompletedTurn','turn-1','$.lastCompletedTurnStatus','completed') \
                     WHERE id=?1",
                    [WORKER],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let frame = call(
            &context,
            "archive-finished-provider",
            "orchestration_agent_manage",
            json!({"action":"archive_finished","request_id":format!("finished-{provider}")}),
        );
        let ports = shared_ports(
            crate::provider_callbacks::managed_tool_ports(
                &store,
                &context,
                &frame,
                Arc::new(tokio::sync::Notify::new()),
            )
            .await
            .unwrap(),
        );
        let (success, value) = tool_reply(
            dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 30.0, ports)
                .await
                .unwrap(),
        );
        assert!(success, "{provider} lead: {value}");
        let report: crate::agent_management_store::ArchiveFinishedResult =
            serde_json::from_value(value.clone()).expect("typed archive summary");
        assert_eq!(report.archived, 1, "{provider} lead: {value}");
        let worker = studio_runtime_store::AgentRepository::get(
            &store.test_reader().unwrap(),
            &studio_runtime_store::AgentId::from(WORKER),
        )
        .unwrap()
        .unwrap()
        .value();
        assert_eq!(worker["provider"], provider);
        assert!(worker["deletedAt"].is_number(), "{provider} lead: {worker}");
    }
}

// Count ancestry SQL separately from fixture setup and unrelated routing work.
thread_local! {
    static ANCESTRY_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[tokio::test]
async fn ancestry_reads_only_the_checked_parent_chain() {
    use super::{AgentAncestry, agent_ancestry};
    let (_directory, store, context) = fixture().await;
    let sender = context.agent;
    let base = template().value["agents"][1].clone();
    let sender_for_setup = sender.clone();
    store
        .transaction_with::<_, StoreError>(move |tx| {
            for (id, parent) in [
                ("chain-middle", sender_for_setup.as_str()),
                ("chain-leaf", "chain-middle"),
            ] {
                let mut record = base.clone();
                record["id"] = json!(id);
                record["rootId"] = json!(sender_for_setup);
                record["parentId"] = json!(parent);
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                    params![id, record.to_string()],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let mut reader = store.test_reader().unwrap();
    reader.trace_v2(
        rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
        Some(|event| {
            if let rusqlite::trace::TraceEvent::Stmt(statement, _) = event
                && statement
                    .sql()
                    .starts_with("SELECT record FROM runtime_agents WHERE id=")
            {
                ANCESTRY_READS.with(|count| count.set(count.get() + 1));
            }
        }),
    );
    let check = |reader: &mut rusqlite::Connection| {
        ANCESTRY_READS.with(|count| count.set(0));
        let tx = reader.transaction().unwrap();
        assert_eq!(
            agent_ancestry(&tx, &sender, "chain-leaf").unwrap(),
            AgentAncestry::Descendant
        );
        assert_eq!(ANCESTRY_READS.with(std::cell::Cell::get), 3);
        // A stopped/tombstoned edge is still part of the stored parent chain.
        assert!(
            super::is_descendant(
                &tx,
                &Actor::from_record(&template().value["agents"][0], &sender, 3).unwrap(),
                "chain-leaf"
            )
            .unwrap()
        );
    };
    check(&mut reader);
    // Malformed unrelated records, including tombstones, must never be decoded.
    // These rows are a bounded deterministic fixture, not a load generator.
    const UNRELATED_AGENTS: usize = 512;
    store.transaction(move |tx| {
        for index in 0..UNRELATED_AGENTS {
            let id = format!("unrelated-{index}");
            let record = json!({"id":id,"deletedAt":if index % 2 == 0 {json!(1.0)} else {Value::Null}});
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES(?1,?2)", params![id, record.to_string()])?;
        }
        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.deletedAt',1.0) WHERE id='chain-middle'", [])?;
        Ok(())
    }).await.unwrap();
    check(&mut reader);
    let tx = reader.transaction().unwrap();
    for alias in [
        "parent",
        "lead",
        "broadcast",
        "all",
        "remote:agent",
        "federated:agent",
    ] {
        ANCESTRY_READS.with(|count| count.set(0));
        assert_eq!(
            agent_ancestry(&tx, &sender, alias).unwrap(),
            AgentAncestry::Unrelated
        );
        assert_eq!(ANCESTRY_READS.with(std::cell::Cell::get), 0);
    }
    assert_eq!(
        agent_ancestry(&tx, &sender, "missing").unwrap(),
        AgentAncestry::Unrelated
    );
    drop(tx);
    drop(reader);
    store.transaction_with::<_, Error>(move |tx| {
    tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.parentId','missing') WHERE id='chain-middle'", []).unwrap();
    assert_eq!(
        agent_ancestry(tx, &sender, "chain-leaf").unwrap(),
        AgentAncestry::Unrelated
    );
    tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.parentId','chain-leaf') WHERE id='chain-middle'", []).unwrap();
    assert_eq!(
        agent_ancestry(tx, &sender, "chain-leaf").unwrap(),
        AgentAncestry::Cycle
    );
    tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.rootId','other-root') WHERE id='chain-middle'", []).unwrap();
    assert_eq!(
        agent_ancestry(tx, &sender, "chain-leaf").unwrap(),
        AgentAncestry::Unrelated
    );
    tx.execute(
        "UPDATE runtime_agents SET record=json_set(record,'$.parentId',42) WHERE id='chain-leaf'",
        [],
    )
    .unwrap();
    assert!(matches!(
        agent_ancestry(tx, &sender, "chain-leaf"),
        Err(Error::Store(_))
    ));
    Ok(())
    }).await.unwrap();
}
