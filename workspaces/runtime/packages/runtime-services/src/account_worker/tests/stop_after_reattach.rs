//! Stop ends the provider work of the agent's turn after its account worker
//! reattached to the same provider process (live agents m1-merge-main 4964fc10
//! and u1-agent-ui b91c79c8, 22 October 2026). A lost provider connection
//! interrupted both agents; the replacement worker attached to the same
//! app-server generation with a new connection id and continued their turns.
//! A lead then stopped them. The stop bound its turn/interrupt to the old
//! connection id; the session refused that scope before it wrote anything, so
//! the turns ran on and their `cargo nextest` and `just test-fast` commands ran
//! for 15 more minutes. The Codex app-server also keeps a thread's unified exec
//! processes after an interrupt, so the stop must end them too.
use super::*;
use crate::account_session_runtime::{
    AccountSessionError, AccountSessionRuntime, NotSentReason, ProviderRequest, ProviderResponse,
    RequestCertainty, RetainedAccountSession, SessionFuture, SessionReceipt,
};
use crate::account_worker::retained::{
    PortError, RequestResult, RetainedSessionPort, SessionRequest,
};
use crate::agent_management::Action;
use crate::lifecycle_effects::ProductionLifecyclePorts;
use crate::recovery_history::ConnectionScope;

/// The registry broker's request path (`dispatch_request` in the runtime
/// server): a request bound to another connection is not sent.
struct WorkerSession {
    port: RetainedSessionPort,
}

impl RetainedAccountSession for WorkerSession {
    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        (self.port.scope().account == account).then(|| self.port.scope().clone())
    }

    fn request(&self, request: ProviderRequest) -> SessionFuture {
        let port = self.port.clone();
        Box::pin(async move {
            let scope = port.scope().clone();
            if request
                .expected_scope
                .as_ref()
                .is_some_and(|expected| expected != &scope)
            {
                return Ok(SessionReceipt {
                    scope: request.expected_scope.unwrap_or(scope),
                    operation: request.operation,
                    ticket: None,
                    certainty: RequestCertainty::NotSent,
                    result: None,
                    not_sent_reason: Some(NotSentReason::ExpectedScopeMismatch),
                    replayed: false,
                });
            }
            let receipt = port
                .request(SessionRequest {
                    scope,
                    expected_scope: request.expected_scope,
                    operation: request.operation,
                    actor: request.actor,
                    transfer: None,
                    deleted_actor_session: request.deleted_actor_session,
                    method: request.method,
                    params: request.params,
                    deadline: request.deadline,
                })
                .await
                .map_err(|error| match error {
                    PortError::Unknown => AccountSessionError::Unknown,
                    _ => AccountSessionError::Refused("Provider request was refused"),
                })?;
            let (certainty, result) = match receipt.result {
                RequestResult::Reply(value) => (
                    RequestCertainty::Written,
                    Some(ProviderResponse::Reply(value)),
                ),
                RequestResult::Rejected(value) => (
                    RequestCertainty::Written,
                    Some(ProviderResponse::Rejected(value)),
                ),
                RequestResult::NotSent => (RequestCertainty::NotSent, None),
                RequestResult::Unknown => (RequestCertainty::Unknown, None),
            };
            Ok(SessionReceipt {
                scope: receipt.scope,
                operation: receipt.operation,
                ticket: Some(receipt.ticket),
                certainty,
                result,
                not_sent_reason: (certainty == RequestCertainty::NotSent)
                    .then_some(NotSentReason::WorkerNotSent),
                replayed: receipt.replayed,
            })
        })
    }
}

/// The production account session over the replacement worker.
fn account_session(state: &State, port: RetainedSessionPort) -> Arc<AccountSessionRuntime> {
    let home = state.root.join("codex-home");
    std::fs::create_dir_all(&home).unwrap();
    let registry_path = state.root.join("accounts/registry.json");
    std::fs::create_dir_all(registry_path.parent().unwrap()).unwrap();
    std::fs::write(
        &registry_path,
        serde_json::to_vec(&json!({
            "version":1,
            "defaultAccountKey":"default",
            "accounts":{"default":{
                "id":"default","home":home,"label":"Codex","source":"Codex CLI",
                "accountId":"fixture-account","status":"signedOut"
            }},
            "logins":{},"deleteReceipts":{}
        }))
        .unwrap(),
    )
    .unwrap();
    Arc::new(AccountSessionRuntime::new(
        Arc::new(crate::account_driver::AccountDriver::open(&registry_path).unwrap()),
        Some(Arc::new(WorkerSession { port })),
        tokio::runtime::Handle::current(),
        state.root.join("accounts"),
        home.clone(),
        home,
    ))
}

/// The lifecycle action that ends the agent's work.
#[derive(Clone, Copy, PartialEq)]
enum Ending {
    Stop,
    Delete,
}

async fn ending_after_worker_reattach_ends_the_turn_work(ending: Ending) {
    let state = State::new().await;
    let mut first = Peer::new();
    let mut first_worker = first.worker(state.bridge.clone()).await;
    let start = state.start(&mut first, &first_worker).await;
    first.accepted(&start).await;
    let cursor = first
        .frame(
            json!({"method":"item/started","params":{"threadId":"fake-thread-0",
            "turnId":"turn","item":{"id":"exec-1","type":"commandExecution",
            "command":"cargo nextest run","status":"inProgress","processId":"42"}}}),
        )
        .await;
    first.acknowledged(cursor).await;
    let submitted = state.record().await["startAttempt"]["connectionId"].clone();
    first_worker.shutdown().await.unwrap();

    // The Studio connection is lost while the supervisor keeps the provider
    // process, its turn, and the command running.
    state
        .bridge
        .run(|store| async move {
            scheduler::lifecycle_transaction_with(&store, |tx| {
                store::disconnect_provider_generation(tx, "default", 1, false)?;
                Ok::<_, WorkerError>(())
            })
            .await
        })
        .await
        .unwrap();
    assert_eq!(state.record().await["status"], "interrupted");

    // A replacement worker attaches to the same provider generation under a
    // new connection id and continues the turn.
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    worker_barrier(&worker).await;
    let live = worker.retained_session().scope().clone();
    let record = state.record().await;
    assert_eq!(record["status"], "running", "{record}");
    assert_eq!(record["inFlight"], true, "{record}");
    assert_eq!(record["turnId"], "turn", "{record}");
    assert_eq!(
        record["connectionRecovery"]["outcome"], "active",
        "{record}"
    );
    assert_eq!(
        record["startAttempt"]["connectionGeneration"],
        live.generation
    );
    assert_eq!(record["startAttempt"]["connectionId"], submitted);
    assert_ne!(
        submitted,
        json!(live.connection),
        "the connection id changed"
    );

    let ports = Arc::new(ProductionLifecyclePorts::new(
        Arc::clone(&state.bridge.store),
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
        account_session(&state, worker.retained_session()),
        None,
    ));
    let target = state.agent.clone();
    let action = match ending {
        Ending::Stop => Action::Stop {
            target,
            descendants: false,
            reason: None,
            sender: None,
            sender_epoch: None,
        },
        Ending::Delete => Action::Delete { target },
    };
    let mut stop = tokio::spawn(async move { ports.execute(&action, 20.0).await });

    let interrupt = tokio::select! {
        wire = peer.next_write() => wire,
        result = &mut stop => panic!("the stop finished without a provider request: {result:?}"),
    };
    assert_eq!(interrupt.value["method"], "turn/interrupt");
    assert_eq!(
        interrupt.value["params"],
        json!({"threadId":"fake-thread-0","turnId":"turn"})
    );
    let _ignored = interrupt.receipt.send(CommandReceipt::Written {
        remote_id: None,
        duplicate: false,
    });
    peer.reply(&interrupt.value, json!({})).await;

    // The interrupt leaves the thread's exec processes running; the stop ends them.
    let clean = tokio::select! {
        wire = peer.next_write() => wire,
        result = &mut stop => panic!("the stop finished without ending the turn's commands: {result:?}"),
    };
    assert_eq!(clean.value["method"], "thread/backgroundTerminals/clean");
    assert_eq!(clean.value["params"], json!({"threadId":"fake-thread-0"}));
    let _ignored = clean.receipt.send(CommandReceipt::Written {
        remote_id: None,
        duplicate: false,
    });
    peer.reply(&clean.value, json!({})).await;
    stop.await.unwrap().expect("the action commits");

    let record = state.record().await;
    assert_eq!(record["status"], "paused", "{record}");
    assert_eq!(
        studio_runtime_store::truthy(record.get("deletedAt")),
        ending == Ending::Delete,
        "{record}"
    );
    let receipt = &record["startAttempt"]["interrupt"];
    assert_eq!(receipt["outcome"], "accepted", "{record}");
    assert_eq!(receipt["turnId"], "turn", "{record}");
    assert!(peer.writes.try_recv().is_err(), "each request is sent once");
    worker.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_after_worker_reattach_interrupts_the_turn_and_ends_its_commands() {
    ending_after_worker_reattach_ends_the_turn_work(Ending::Stop).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_after_worker_reattach_interrupts_the_turn_and_ends_its_commands() {
    ending_after_worker_reattach_ends_the_turn_work(Ending::Delete).await;
}

/// The production account session while no account worker is attached.
fn detached_account_session(state: &State) -> Arc<AccountSessionRuntime> {
    let home = state.root.join("codex-home");
    std::fs::create_dir_all(&home).unwrap();
    let registry_path = state.root.join("accounts/registry.json");
    std::fs::create_dir_all(registry_path.parent().unwrap()).unwrap();
    std::fs::write(
        &registry_path,
        serde_json::to_vec(&json!({
            "version":1,
            "defaultAccountKey":"default",
            "accounts":{"default":{
                "id":"default","home":home,"label":"Codex","source":"Codex CLI",
                "accountId":"fixture-account","status":"signedOut"
            }},
            "logins":{},"deleteReceipts":{}
        }))
        .unwrap(),
    )
    .unwrap();
    Arc::new(AccountSessionRuntime::new(
        Arc::new(crate::account_driver::AccountDriver::open(&registry_path).unwrap()),
        None,
        tokio::runtime::Handle::current(),
        state.root.join("accounts"),
        home.clone(),
        home,
    ))
}

/// The owner ends the agent while its Studio connection is lost and no worker
/// has reattached yet. The provider process keeps the turn and its command
/// running. The worker that reattaches to that provider generation must end
/// them once, and must not continue or restart the stopped agent.
async fn ending_before_worker_reattach_ends_the_turn_work(ending: Ending) {
    let state = State::new().await;
    let mut first = Peer::new();
    let mut first_worker = first.worker(state.bridge.clone()).await;
    let start = state.start(&mut first, &first_worker).await;
    first.accepted(&start).await;
    let cursor = first
        .frame(
            json!({"method":"item/started","params":{"threadId":"fake-thread-0",
            "turnId":"turn","item":{"id":"exec-1","type":"commandExecution",
            "command":"cargo nextest run","status":"inProgress","processId":"42"}}}),
        )
        .await;
    first.acknowledged(cursor).await;
    first_worker.shutdown().await.unwrap();
    state
        .bridge
        .run(|store| async move {
            scheduler::lifecycle_transaction_with(&store, |tx| {
                store::disconnect_provider_generation(tx, "default", 1)?;
                Ok::<_, WorkerError>(())
            })
            .await
        })
        .await
        .unwrap();
    assert_eq!(state.record().await["status"], "interrupted");

    // No worker is attached: the action commits without a provider request.
    let ports = Arc::new(ProductionLifecyclePorts::new(
        Arc::clone(&state.bridge.store),
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
        detached_account_session(&state),
        None,
    ));
    let target = state.agent.clone();
    let action = match ending {
        Ending::Stop => Action::Stop {
            target,
            descendants: false,
            reason: None,
            sender: None,
            sender_epoch: None,
        },
        Ending::Delete => Action::Delete { target },
    };
    ports
        .execute(&action, 20.0)
        .await
        .expect("the action commits");
    let stopped = state.record().await;
    assert_eq!(stopped["status"], "paused", "{stopped}");

    // A worker reattaches to the same provider generation, where the turn runs.
    // Its startup barrier must not count the first worker's finished pass.
    let fresh_pass = || {
        state
            .bridge
            .faults
            .startup_recovery_done
            .store(false, std::sync::atomic::Ordering::SeqCst)
    };
    fresh_pass();
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let interrupt = tokio::select! {
        wire = peer.next_write() => wire,
        () = worker_barrier(&worker) => panic!(
            "the reattached worker left the stopped turn running: {}",
            state.record().await["disconnectRecovery"]
        ),
    };
    assert_eq!(
        interrupt.value["method"], "turn/interrupt",
        "{}",
        interrupt.value
    );
    assert_eq!(
        interrupt.value["params"],
        json!({"threadId":"fake-thread-0","turnId":"turn"})
    );
    let _ignored = interrupt.receipt.send(CommandReceipt::Written {
        remote_id: None,
        duplicate: false,
    });
    peer.reply(&interrupt.value, json!({})).await;
    let clean = peer.next_write().await;
    assert_eq!(clean.value["method"], "thread/backgroundTerminals/clean");
    assert_eq!(clean.value["params"], json!({"threadId":"fake-thread-0"}));
    let _ignored = clean.receipt.send(CommandReceipt::Written {
        remote_id: None,
        duplicate: false,
    });
    peer.reply(&clean.value, json!({})).await;
    worker_barrier(&worker).await;

    let record = state.record().await;
    assert_eq!(record["status"], "paused", "{record}");
    assert!(
        !studio_runtime_store::truthy(record.get("autoWake")),
        "the stopped agent does not wake: {record}"
    );
    assert_eq!(
        record["disconnectRecovery"]["stopInterrupt"]["outcome"], "accepted",
        "{record}"
    );
    assert!(peer.writes.try_recv().is_err(), "each request is sent once");
    worker.shutdown().await.unwrap();

    // A later worker of the same generation finds the turn already ended.
    fresh_pass();
    let mut next = Peer::new();
    let mut next_worker = next.worker(state.bridge.clone()).await;
    worker_barrier(&next_worker).await;
    assert!(
        next.writes.try_recv().is_err(),
        "an ended turn is not interrupted again"
    );
    next_worker.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_before_worker_reattach_interrupts_the_turn_and_ends_its_commands() {
    ending_before_worker_reattach_ends_the_turn_work(Ending::Stop).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_before_worker_reattach_interrupts_the_turn_and_ends_its_commands() {
    ending_before_worker_reattach_ends_the_turn_work(Ending::Delete).await;
}
