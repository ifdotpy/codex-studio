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
    account_session_over(state, Some(Arc::new(WorkerSession { port })))
}

/// The production account session over `session`, or without one.
fn account_session_over(
    state: &State,
    session: Option<Arc<dyn RetainedAccountSession>>,
) -> Arc<AccountSessionRuntime> {
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
        session,
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
    answer_adopted_turn_check(&mut peer).await;
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
    account_session_over(state, None)
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
                store::disconnect_provider_generation(tx, "default", 1, false)?;
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

/// The registry broker while the account has no live worker: it has no
/// current scope, and it sends nothing.
struct NoLiveWorker;

impl RetainedAccountSession for NoLiveWorker {
    fn current_scope(&self, _account: &str) -> Option<ConnectionScope> {
        None
    }

    fn request(&self, request: ProviderRequest) -> SessionFuture {
        Box::pin(async move {
            Ok(SessionReceipt {
                scope: request.expected_scope.unwrap_or(ConnectionScope {
                    account: request.account,
                    connection: String::new(),
                    generation: 0,
                }),
                operation: request.operation,
                ticket: None,
                certainty: RequestCertainty::NotSent,
                result: None,
                not_sent_reason: Some(NotSentReason::NoLiveAccountWorker),
                replayed: false,
            })
        })
    }
}

/// Where the stop finds no worker for its interrupt.
#[derive(Clone, Copy)]
enum StopWindow {
    /// The running server's account session: the stop's request is bound to
    /// a scope that has no live worker, so its outcome stays unknown.
    NoLiveWorker,
    /// No account session at all: the request is proven unsent.
    NoSession,
}

/// The account worker ends while the process supervisor keeps the provider,
/// so the turn stays running and has no disconnect receipt. The owner stops
/// the agent before a worker reattaches: no worker can send the stop's
/// interrupt. The worker that reattaches to the same provider generation must
/// still end that turn and its commands once, under the stop's own request
/// identities.
async fn stop_while_a_retained_provider_has_no_worker_ends_its_turn_once(window: StopWindow) {
    let state = State::new().await;
    let mut first = Peer::supervised();
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
    worker_barrier(&first_worker).await;
    first_worker.shutdown().await.unwrap();
    let live = state.record().await;
    assert_eq!(
        live["status"], "running",
        "the retained turn runs on: {live}"
    );
    assert!(
        live.get("disconnectRecovery").is_none_or(Value::is_null),
        "{live}"
    );

    let session = match window {
        StopWindow::NoLiveWorker => account_session_over(&state, Some(Arc::new(NoLiveWorker))),
        StopWindow::NoSession => detached_account_session(&state),
    };
    let ports = Arc::new(ProductionLifecyclePorts::new(
        Arc::clone(&state.bridge.store),
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
        session,
        None,
    ));
    ports
        .execute(
            &Action::Stop {
                target: state.agent.clone(),
                descendants: false,
                reason: None,
                sender: None,
                sender_epoch: None,
            },
            20.0,
        )
        .await
        .expect("the stop commits");
    let stopped = state.record().await;
    assert_eq!(stopped["status"], "paused", "{stopped}");
    let staged = stopped["startAttempt"]["interrupt"].clone();
    assert_eq!(
        staged["outcome"],
        match window {
            StopWindow::NoLiveWorker => "unknown",
            StopWindow::NoSession => "unsent",
        },
        "{stopped}"
    );

    let fresh_pass = || {
        state
            .bridge
            .faults
            .startup_recovery_done
            .store(false, std::sync::atomic::Ordering::SeqCst)
    };
    fresh_pass();
    let mut peer = Peer::supervised();
    peer.cursor = cursor;
    let mut worker = peer.worker(state.bridge.clone()).await;
    let interrupt = tokio::select! {
        wire = peer.next_write() => wire,
        () = worker_barrier(&worker) => {
            let record = state.record().await;
            panic!(
                "the reattached worker left the stopped turn running: status {} inFlight {} interrupt {}",
                record["status"], record["inFlight"], record["startAttempt"]["interrupt"]
            )
        }
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
    let settled = &record["startAttempt"]["interrupt"];
    assert_eq!(settled["outcome"], "accepted", "{record}");
    assert_eq!(
        settled["requestTicket"], staged["requestTicket"],
        "the stop's own request identity: {record}"
    );
    assert!(peer.writes.try_recv().is_err(), "each request is sent once");
    worker.shutdown().await.unwrap();

    // A later worker of the same generation finds the turn already ended.
    fresh_pass();
    let mut next = Peer::supervised();
    next.cursor = cursor;
    let mut next_worker = next.worker(state.bridge.clone()).await;
    worker_barrier(&next_worker).await;
    assert!(
        next.writes.try_recv().is_err(),
        "an ended turn is not interrupted again"
    );
    next_worker.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_while_a_retained_provider_has_no_live_worker_interrupts_its_turn_once() {
    stop_while_a_retained_provider_has_no_worker_ends_its_turn_once(StopWindow::NoLiveWorker).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_without_an_account_session_interrupts_a_retained_turn_once() {
    stop_while_a_retained_provider_has_no_worker_ends_its_turn_once(StopWindow::NoSession).await;
}

/// The stop's interrupt reaches the provider, but its reply is lost when the
/// account worker ends and the supervisor keeps the provider. The stop's
/// outcome stays unknown. The worker that reattaches must not send that
/// interrupt again; it sends only the command cleanup that never left Studio.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_interrupt_with_a_lost_reply_is_not_sent_again_after_reattach() {
    let state = State::new().await;
    let mut first = Peer::supervised();
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
    worker_barrier(&first_worker).await;

    let ports = Arc::new(ProductionLifecyclePorts::new(
        Arc::clone(&state.bridge.store),
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
        account_session(&state, first_worker.retained_session()),
        None,
    ));
    let action = Action::Stop {
        target: state.agent.clone(),
        descendants: false,
        reason: None,
        sender: None,
        sender_epoch: None,
    };
    let mut stop = tokio::spawn(async move { ports.execute(&action, 20.0).await });
    let interrupt = tokio::select! {
        wire = first.next_write() => wire,
        result = &mut stop => panic!("the stop finished without a provider request: {result:?}"),
    };
    assert_eq!(interrupt.value["method"], "turn/interrupt");
    let _ignored = interrupt.receipt.send(CommandReceipt::Written {
        remote_id: None,
        duplicate: false,
    });
    // The worker ends before the reply arrives; the provider stays retained.
    first_worker.shutdown().await.unwrap();
    stop.await.unwrap().expect("the stop commits");
    let stopped = state.record().await;
    assert_eq!(stopped["status"], "paused", "{stopped}");
    assert_eq!(
        stopped["startAttempt"]["interrupt"]["outcome"], "unknown",
        "{stopped}"
    );
    let mut interrupts = 1;
    let mut cleanups = 0;
    while let Ok(wire) = first.writes.try_recv() {
        match wire.value["method"].as_str() {
            Some("turn/interrupt") => interrupts += 1,
            Some("thread/backgroundTerminals/clean") => cleanups += 1,
            _ => {}
        }
    }

    state
        .bridge
        .faults
        .startup_recovery_done
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let mut peer = Peer::supervised();
    peer.cursor = cursor;
    let mut worker = peer.worker(state.bridge.clone()).await;
    loop {
        let wire = tokio::select! {
            wire = peer.next_write() => wire,
            () = worker_barrier(&worker) => break,
        };
        match wire.value["method"].as_str() {
            Some("turn/interrupt") => interrupts += 1,
            Some("thread/backgroundTerminals/clean") => cleanups += 1,
            _ => {}
        }
        let _ignored = wire.receipt.send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        });
        peer.reply(&wire.value, json!({})).await;
    }
    while let Ok(wire) = peer.writes.try_recv() {
        match wire.value["method"].as_str() {
            Some("turn/interrupt") => interrupts += 1,
            Some("thread/backgroundTerminals/clean") => cleanups += 1,
            _ => {}
        }
    }
    assert_eq!(interrupts, 1, "a written interrupt is never sent again");
    assert_eq!(cleanups, 1, "the command cleanup is sent once");
    let record = state.record().await;
    assert_eq!(record["status"], "paused", "{record}");
    assert_eq!(
        record["startAttempt"]["interrupt"]["outcome"], "unknown",
        "a lost reply stays unknown: {record}"
    );
    worker.shutdown().await.unwrap();
}
