#![allow(clippy::duplicate_mod, clippy::unwrap_used)]

#[path = "../../tests/support/prepared_python.rs"]
mod prepared_python;

use crate::managed_host_tools::{
    Actor, ChildCreationPort, ComplaintPort, ContextPort, Error, InterruptPort, OperationResult,
    PostCommitPort, ReviewPort, SearchPort, SendPort, StatusPort, WakeSignal, WatchMonitorPort,
};
use crate::provider_callbacks::{
    CallbackContext, CallbackDecision, CallbackError, ManagedToolPortsConfig,
    ManagedToolRuntimePorts, dispatch_with_managed_tools,
};
use rusqlite::types::ValueRef;
use rusqlite::{Transaction, params};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::Command,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use studio_runtime_providers::{Frame, Method, RequestFrame, RequestId, ResponseBody};
use studio_runtime_store::{RuntimeStore, StoreError};

struct Template {
    _scratch: tempfile::TempDir,
    value: Value,
}

fn template() -> &'static Template {
    static TEMPLATE: OnceLock<Template> = OnceLock::new();
    TEMPLATE.get_or_init(|| {
        let scratch = tempfile::tempdir().unwrap();
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let source = manifest.join("../../apps/server/src");
        let oracle = manifest.join("tests/provider_callbacks/oracle.py");
        let output = Command::new(prepared_python::prepared_python(&source))
            .arg(oracle)
            .arg(scratch.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "oracle: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Template {
            _scratch: scratch,
            value: serde_json::from_slice(&output.stdout).unwrap(),
        }
    })
}

async fn fixture() -> (tempfile::TempDir, Arc<RuntimeStore>, CallbackContext) {
    let template = template();
    let directory = tempfile::tempdir().unwrap();
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(
            PathBuf::from(template.value["template"].as_str().unwrap()).join(name),
            directory.path().join(name),
        )
        .unwrap();
    }
    let store = Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
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
async fn post_commit_dispatch_saves_receipt_before_reply_and_replay_does_not_dispatch_again() {
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
    assert!(
        dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 20.0, ports.clone())
            .await
            .is_err()
    );
    assert_eq!(ports.dispatched.load(Ordering::SeqCst), 1);
    let connection = store.read().unwrap();
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
    assert!(matches!(replay, CallbackDecision::Answer(_)));
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
        Result<Option<crate::monitor_policy::ReattachProof>, crate::monitor_policy::MonitorError>,
    > {
        Box::pin(async { Ok(None) })
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

fn production_managed_ports(
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
            },
        )
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
        let ports = production_managed_ports(&store, &context, &frame, fake.clone());
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
            &store.read().unwrap(),
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
    let ports = production_managed_ports(&store, &worker_context, &frame, fake.clone());
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
    let ports = production_managed_ports(&store, &stale_context, &frame, fake.clone());
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
            default_account_key: None,
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
            }],
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
            parent_account_key: "primary",
            requested_account_key: None,
            default_account_key: None,
            model: "gpt-5",
            excluded_account_keys: &[],
            project_account_keys: None,
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
    let connection = store.read().unwrap();
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
        },
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
            &store.read().unwrap(),
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
        .read()
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
    let connection = store.read().unwrap();
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

fn managed_snapshot(store: &RuntimeStore, tables: &Value) -> Value {
    let connection = store.read().unwrap();
    let mut snapshot = serde_json::Map::new();
    for name in tables.as_object().unwrap().keys() {
        let quoted = name.replace('"', "\"\"");
        let column_names = connection
            .prepare(&format!("PRAGMA table_info(\"{quoted}\")"))
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let statement = connection
            .prepare(&format!("SELECT * FROM \"{quoted}\""))
            .unwrap();
        let columns = statement.column_count();
        drop(statement);
        let ordering = (1..=columns)
            .map(|column| column.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let mut statement = connection
            .prepare(&format!("SELECT * FROM \"{quoted}\" ORDER BY {ordering}"))
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                let mut values = Vec::new();
                for (column, column_name) in column_names.iter().enumerate().take(columns) {
                    let mut value = if name == "sync_entities" && column_name == "seq" {
                        json!(0)
                    } else {
                        match row.get_ref(column)? {
                        ValueRef::Null => Value::Null,
                        ValueRef::Integer(value) => json!(value),
                        ValueRef::Real(value) => json!(value),
                        ValueRef::Text(value) => {
                            let text = String::from_utf8_lossy(value);
                            serde_json::from_str(&text).unwrap_or_else(|_| json!(text.as_ref()))
                        }
                        ValueRef::Blob(value) => json!({
                            "blobHex":value.iter().map(|byte|format!("{byte:02x}")).collect::<String>()
                        }),
                        }
                    };
                    // Python's callback harness updates unrelated model
                    // context fields while resolving calls. The event timing
                    // and agent serverId are fixture-specific. Preserve every
                    // other row and projection field.
                    if name == "runtime_agents"
                        && column_name == "record"
                        && let Some(record) = value.as_object_mut()
                    {
                        record.remove("concurrency");
                        record.remove("deliveredMode");
                    }
                    if name == "runtime_event_meta"
                        && column_name == "record"
                        && let Some(record) = value.as_object_mut()
                    {
                        record.remove("timing");
                    }
                    if name == "sync_entities"
                        && column_name == "payload"
                        && let Some(payload) = value.as_object_mut()
                        && payload["collection"] == "agent"
                        && let Some(record) = payload["value"].as_object_mut()
                    {
                        record.remove("concurrency");
                        record.remove("deliveredMode");
                        record.remove("serverId");
                    }
                    values.push(value);
                }
                if name == "sync_entities" && values.first() == Some(&json!("agent")) {
                    // hash commits the full projection, including the
                    // fixture-specific agent serverId omitted above.
                    values[3] = json!("normalized-agent-projection-hash");
                }
                Ok(Value::Array(values))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        snapshot.insert(name.clone(), json!(rows));
    }
    Value::Object(snapshot)
}

async fn dispatch_python_case(
    store: &Arc<RuntimeStore>,
    oracle: &Value,
    expected: &Value,
    args: Option<Value>,
) -> Result<CallbackDecision, CallbackError> {
    let actor = oracle["agents"]
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
    let ports = shared_ports(crate::provider_callbacks::managed_tool_ports(
        store,
        &context,
        &frame,
        std::sync::Arc::new(tokio::sync::Notify::new()),
    )?);
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
            json!({"topic":"profiles"}),
            "model-context/requested-topic",
        ),
    ];
    for (index, (tool, args, dependency)) in cases.into_iter().enumerate() {
        let (_root, store, context) = fixture().await;
        let before = application_table_snapshot(&store);
        let rpc = format!("refusal-{index}");
        let frame = call(&context, &rpc, tool, args);
        let wake = std::sync::Arc::new(tokio::sync::Notify::new());
        let ports = shared_ports(
            crate::provider_callbacks::managed_tool_ports(&store, &context, &frame, wake).unwrap(),
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
        .unwrap(),
    );
    let count_before: i64 = store
        .read()
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
        .read()
        .unwrap()
        .query_row("SELECT count(*) FROM runtime_monitors", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, count_before + 1);
    let saved: Value = serde_json::from_str(
        &store
            .read()
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
        let read = store.read().unwrap();
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
        .read()
        .unwrap()
        .query_row("SELECT count(*) FROM runtime_monitors", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(replayed_count, count_before + 1);
    let replayed: Value = serde_json::from_str(
        &store
            .read()
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
async fn send_remote_routing_refusals_match_python_and_write_nothing() {
    let descendant = template().value["agents"][1]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Runtime.dynamic routes movedFrom actors to move_send before target
    // resolution (codex_runtime.py:9048-9049). Ordinary remote actors reach
    // chat_message, where user uses send_to_user and remote/federated targets
    // use federation locally (codex_runtime.py:9858-9877).
    let cases = [
        (
            "moved descendant",
            descendant,
            json!({"remoteOrigin":"source-server","movedFrom":true}),
            "multi-server-runtime/move-send",
        ),
        (
            "remote to user",
            "user".to_owned(),
            json!({"remoteOrigin":"source-server"}),
            "Only the lead can contact the user. Send your message to your lead.",
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
async fn managed_tools_match_python_rows_events_and_sync_projection() {
    let (_root, store, _context) = fixture().await;
    let oracle = &template().value;
    let agents = oracle["agents"].as_array().unwrap();
    let wake = std::sync::Arc::new(tokio::sync::Notify::new());
    for expected in oracle["managedToolCases"].as_array().unwrap() {
        let actor = agents
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
        if expected["remoteOrigin"] == true {
            let agent_id = context.agent.clone();
            store
                .transaction_with::<_, StoreError>(move |tx| {
                    tx.execute(
                        "UPDATE runtime_agents SET record=json_set(record,'$.remoteOrigin','source-server') WHERE id=?1",
                        [agent_id],
                    )?;
                    Ok(())
                }).await
                .unwrap();
        }
        let frame = call(
            &context,
            expected["callId"].as_str().unwrap(),
            expected["tool"].as_str().unwrap(),
            expected["arguments"].clone(),
        );
        let ports = shared_ports(
            crate::provider_callbacks::managed_tool_ports(&store, &context, &frame, wake.clone())
                .unwrap(),
        );
        let decision =
            dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 20.0, ports.clone())
                .await
                .unwrap();
        let CallbackDecision::Answer(reply) = decision else {
            panic!("managed callback must answer: {}", expected["tool"])
        };
        let ResponseBody::Result(actual) = reply.body else {
            panic!("managed callback result expected")
        };
        assert_eq!(
            actual["success"], expected["result"]["success"],
            "success for {}",
            expected["tool"]
        );
        let actual_text = actual["contentItems"][0]["text"].as_str().unwrap();
        let expected_text = expected["result"]["contentItems"][0]["text"]
            .as_str()
            .unwrap();
        if actual["success"] == true {
            assert_eq!(
                serde_json::from_str::<Value>(actual_text).unwrap(),
                serde_json::from_str::<Value>(expected_text).unwrap(),
                "tool value for {}",
                expected["tool"]
            );
        } else if expected["tool"] != "orchestration_send" || expected["result"]["success"] == true
        {
            assert_eq!(
                actual_text, expected_text,
                "tool refusal for {}",
                expected["tool"]
            );
        }
        let actual_rows = managed_snapshot(&store, &expected["rows"]);
        let differences = expected["rows"]
            .as_object()
            .unwrap()
            .iter()
            .filter_map(|(table, expected_rows)| {
                let actual_rows = actual_rows[table].as_array().unwrap();
                (actual_rows != expected_rows.as_array().unwrap()).then(|| {
                    let expected_rows = expected_rows.as_array().unwrap();
                    let mismatch = actual_rows
                        .iter()
                        .zip(expected_rows)
                        .find(|(actual, expected)| actual != expected);
                    let detail = mismatch.map(|(actual, expected)| {
                        if actual.as_array().is_some_and(|row| row.len() > 1)
                            && expected.as_array().is_some_and(|row| row.len() > 1)
                        {
                            let actual_row = actual.as_array().unwrap();
                            let expected_row = expected.as_array().unwrap();
                            if table == "sync_entities" {
                                return format!("actual={actual_row:?}; expected={expected_row:?}");
                            }
                            if table == "sync_entity_meta" {
                                return format!("actual={actual_row:?}; expected={expected_row:?}");
                            }
                            let fields = match (&actual_row[1], &expected_row[1]) {
                                (Value::Object(actual), Value::Object(expected)) => actual
                                    .keys()
                                    .chain(expected.keys())
                                    .collect::<std::collections::BTreeSet<_>>()
                                    .into_iter()
                                    .filter(|key| actual.get(*key) != expected.get(*key))
                                    .cloned()
                                    .collect::<Vec<_>>(),
                                _ => vec!["row-value".to_owned()],
                            };
                            format!("id={:?} fields={fields:?}", actual_row[0])
                        } else {
                            format!("actual={actual:?} expected={expected:?}")
                        }
                    });
                    format!("{table}: {detail:?}")
                })
            })
            .collect::<Vec<_>>();
        assert!(differences.is_empty(), "full rows differ: {differences:?}");
    }
}

#[tokio::test]
async fn deleted_team_root_refuses_complaint_without_writes() {
    let (_root, store, _) = fixture().await;
    let oracle = &template().value;
    let expected = &oracle["managedRootDeletedRefusal"];
    assert_eq!(expected["result"]["success"], false);
    assert_eq!(expected["before"], expected["rows"]);
    let worker = oracle["agents"]
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
    let decision = dispatch_python_case(
        &store,
        oracle,
        expected,
        Some(expected["arguments"].clone()),
    )
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
    let oracle = &template().value;
    let cases = oracle["managedToolCases"].as_array().unwrap();
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
                    dispatch_python_case(&store, oracle, previous, None)
                        .await
                        .unwrap(),
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
            let first_attempt = dispatch_python_case(&store, oracle, expected, None).await;
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
            let first = match dispatch_python_case(&store, oracle, expected, None)
                .await
                .unwrap()
            {
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
            let replay = dispatch_python_case(&store, oracle, expected, None)
                .await
                .unwrap();
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
                dispatch_python_case(&store, oracle, expected, Some(changed))
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
    let oracle = &template().value;
    let expected = &oracle["managedToolCases"][0];
    store
        .transaction_with::<_, StoreError>(|tx| {
            tx.execute("DELETE FROM runtime_requests", [])?;
            tx.execute("DELETE FROM runtime_operation_receipts", [])?;
            tx.execute_batch("CREATE TRIGGER p05_crash_after_reservation BEFORE INSERT ON runtime_tool_results BEGIN SELECT RAISE(ABORT,'crash after reservation'); END")?;
            Ok(())
        }).await
        .unwrap();
    let application_before = application_table_snapshot(&store);
    assert!(
        dispatch_python_case(&store, oracle, expected, None)
            .await
            .is_err()
    );
    store
        .transaction_with::<_, StoreError>(|tx| {
            tx.execute_batch("DROP TRIGGER p05_crash_after_reservation")?;
            Ok(())
        })
        .await
        .unwrap();
    let running_claim: i64 = store
        .read()
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
        .read()
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
async fn post_commit_failure_replays_saved_result_before_callback_reply() {
    let (_root, store, context) = fixture().await;
    let ports = Arc::new(PortBundle::default());
    ports.fail_release.store(true, Ordering::SeqCst);
    let frame = call(
        &context,
        "rollback-status",
        "orchestration_status",
        json!({}),
    );
    assert!(
        dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 20.0, ports.clone())
            .await
            .is_err()
    );
    assert_eq!(ports.staged.load(Ordering::SeqCst), 1);
    assert_eq!(ports.released.load(Ordering::SeqCst), 0);
    assert_eq!(intent_count(&store, "status"), 1);

    ports.fail_release.store(false, Ordering::SeqCst);
    let replay =
        dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 21.0, ports.clone())
            .await
            .unwrap();
    assert!(matches!(replay, CallbackDecision::Answer(_)));
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
        .read()
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
    let connection = store.read().unwrap();
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
        parent_account_key: &str,
        requested_account_key: Option<&str>,
        default_account_key: Option<&str>,
        model: &str,
        excluded_account_keys: &[String],
        project_account_keys: Option<&[String]>,
    ) -> Result<
        (String, crate::host_worker_create::AccountIdentity),
        crate::managed_child_create::ManagedCreationEnvironmentError,
    > {
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
    fn validate_worker_choice(
        &self,
        _: &RuntimeStore,
        _: &str,
        _: &crate::host_worker_create::AccountIdentity,
        _: &str,
        _: &str,
    ) -> Result<(), crate::managed_child_create::ManagedCreationEnvironmentError> {
        Ok(())
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
    ) -> Result<std::path::PathBuf, crate::managed_child_create::ManagedCreationEnvironmentError>
    {
        IMAGE_PROVISION_CALLS.with(|calls| calls.set(calls.get() + 1));
        Ok(std::path::PathBuf::from("/fake-image/repo"))
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

fn normalize_actor_ids(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if object.contains_key("actorId") {
                object.insert("actorId".to_owned(), json!("normalized-actor-id"));
            }
            for nested in object.values_mut() {
                normalize_actor_ids(nested);
            }
        }
        Value::Array(values) => {
            for nested in values {
                normalize_actor_ids(nested);
            }
        }
        _ => {}
    }
}

fn normalize_persisted_ids_and_times(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, nested) in object.iter_mut() {
                if matches!(
                    key.as_str(),
                    "id" | "actorId"
                        | "agent"
                        | "agentId"
                        | "parentId"
                        | "rootId"
                        | "runId"
                        | "attemptId"
                        | "turnId"
                        | "referenceId"
                ) {
                    *nested = json!("normalized-opaque-id");
                } else if matches!(key.as_str(), "created" | "updated" | "timestamp") {
                    *nested = json!(0);
                } else {
                    normalize_persisted_ids_and_times(nested);
                }
            }
        }
        Value::Array(values) => {
            for nested in values {
                normalize_persisted_ids_and_times(nested);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn dispatcher_service_bundle_calls_spawn_and_review_entry_points() {
    let (_root, store, context) = fixture().await;
    let project_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .unwrap()
        .canonicalize()
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
    let review_directory = review_target
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let server_source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
    let oracle = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../apps/server/tests/spawn-review-child-oracle.py");
    let mut python = Command::new(prepared_python::prepared_python(&server_source));
    python.env("PYTHONDONTWRITEBYTECODE", "1");
    python.env("HOME", &home);
    python
        .arg(&oracle)
        .arg(review_target.ancestors().nth(4).unwrap())
        .arg(&review_cwd);
    let output = prepared_python::output_with_timeout(
        &mut python,
        &oracle,
        std::time::Duration::from_secs(180),
    )
    .expect("Python child creation corpus completes within its bound");
    assert!(
        output.status.success(),
        "Python child creation corpus: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: Value = serde_json::from_slice(&output.stdout).expect("Python corpus JSON");
    let mut actor_value = {
        let read = store.read().unwrap();
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
    assert_eq!(spawned.value, expected["spawn"]["response"]);
    let spawned_child_id = spawned.value["agents"][0]["id"].as_str().unwrap();
    let read = store.read().unwrap();
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
    assert_eq!(
        spawned.value["agents"][0]["baseCommit"],
        expected["base"]["baseCommit"]
    );
    let mut expected_spawn_child = expected["spawn"]["child"].clone();
    let mut actual_spawn_child = spawned_child.clone();
    normalize_persisted_ids_and_times(&mut expected_spawn_child);
    normalize_persisted_ids_and_times(&mut actual_spawn_child);
    assert_eq!(
        actual_spawn_child, expected_spawn_child,
        "complete spawn child row"
    );
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
        &store.read().unwrap(),
        &studio_runtime_store::AgentId::from(fallback_id),
    )
    .unwrap()
    .unwrap()
    .value();
    assert_eq!(fallback_child["accountKey"], "alternate");
    assert_eq!(fallback_child["profileId"], Value::Null);
    assert_eq!(fallback_child["profileInstructions"], "");
    let mut expected_fallback = expected["fallback"]["child"].clone();
    let mut actual_fallback = fallback_child;
    normalize_persisted_ids_and_times(&mut expected_fallback);
    normalize_persisted_ids_and_times(&mut actual_fallback);
    assert_eq!(
        actual_fallback, expected_fallback,
        "complete implicit fallback child row"
    );
    let read = store.read().unwrap();
    let event = read.query_row("SELECT json_object('id',id,'agent',agent,'kind',kind,'text',text,'status',status,'created',created,'epoch',epoch,'turn_id',turn_id,'error',error) FROM runtime_events WHERE id=?1", [format!("{spawned_child_id}:initial")], |row| row.get::<_, String>(0)).unwrap();
    let event: Value = serde_json::from_str(&event).unwrap();
    let effects = read.query_row("SELECT COALESCE(json_group_array(json(record)), '[]') FROM runtime_execution_effects WHERE reference=?1", [spawned_child_id], |row| row.get::<_, String>(0)).unwrap();
    let effects: Value = serde_json::from_str(&effects).unwrap();
    let nodes = read.query_row("SELECT COALESCE(json_group_array(json(record)), '[]') FROM runtime_execution_nodes WHERE json_extract(record,'$.agentId')=?1", [spawned_child_id], |row| row.get::<_, String>(0)).unwrap();
    let nodes: Value = serde_json::from_str(&nodes).unwrap();
    for (actual, expected_value, label) in [
        (
            event,
            expected["spawn"]["event"].clone(),
            "spawn initial event",
        ),
        (
            effects,
            expected["spawn"]["effects"].clone(),
            "spawn execution effects",
        ),
        (
            nodes,
            expected["spawn"]["nodes"].clone(),
            "spawn execution nodes",
        ),
    ] {
        let mut actual = actual;
        let mut expected_value = expected_value;
        normalize_persisted_ids_and_times(&mut actual);
        normalize_persisted_ids_and_times(&mut expected_value);
        assert_eq!(actual, expected_value, "complete {label} row(s)");
    }
    drop(read);
    let before_workspace_refusal: i64 = store
        .read()
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
        .read()
        .unwrap()
        .query_row("SELECT count(*) FROM runtime_agents", [], |row| row.get(0))
        .unwrap();
    assert_eq!(before_workspace_refusal, after_workspace_refusal);
    assert_eq!(expected["workspaceRefused"], true);

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
        &store.read().unwrap(),
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
        .read()
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
    assert_eq!(
        image_base_child["workerBaseCommit"],
        expected["base"]["baseCommit"]
    );
    let image_base_event: String = store
        .read()
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
    let mut expected_review_response = expected["review"]["response"].clone();
    normalize_actor_ids(&mut expected_review_response);
    let mut actual_review_response = reviewed.value.clone();
    normalize_actor_ids(&mut actual_review_response);
    assert_eq!(actual_review_response, expected_review_response);
    let child = reviewed.value["agents"][0]["id"].as_str().unwrap();
    let read = store.read().unwrap();
    let child = studio_runtime_store::AgentRepository::get(
        &read,
        &studio_runtime_store::AgentId::from(child),
    )
    .unwrap()
    .unwrap()
    .value();
    assert_eq!(child["cwd"], review_directory);
    assert_eq!(child["nativeReview"]["status"], "pending");
    let mut expected_review_child = expected["review"]["child"].clone();
    let mut actual_review_child = child.clone();
    normalize_persisted_ids_and_times(&mut expected_review_child);
    normalize_persisted_ids_and_times(&mut actual_review_child);
    assert_eq!(
        actual_review_child, expected_review_child,
        "complete review child row"
    );
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
        .read()
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
        .read()
        .unwrap()
        .query_row("SELECT count(*) FROM runtime_agents", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        before_vm_review, after_vm_review,
        "no VM reviewer was enqueued"
    );
    assert_eq!(VM_REVIEW_GUEST_CALLS.get(), 0, "refusal precedes line.save");
    let after_uncertain_review: i64 = store
        .read()
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
        let read = store.read().unwrap();
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
        .read()
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
        .read()
        .unwrap()
        .query_row("SELECT count(*) FROM runtime_agents", [], |row| row.get(0))
        .unwrap();
    assert_eq!(before_capacity, after_capacity);
    assert_eq!(
        expected["capacity"]["countBefore"],
        expected["capacity"]["countAfter"]
    );
    assert!(expected["capacity"]["error"].as_str().is_some());
}

#[tokio::test]
async fn spawn_child_and_first_turn_failpoints_roll_back_the_batch() {
    for failpoint in ["child", "first-turn"] {
        let (_root, store, context) = fixture().await;
        let actor_value = {
            let read = store.read().unwrap();
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
        let read = store.read().unwrap();
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

#[path = "ingress_tests/claim_settlement.rs"]
mod claim_settlement;
