#![cfg(unix)]
#![allow(clippy::unwrap_used)]
use serde_json::{Value, json};
use std::{collections::BTreeMap, future::Future, task::Poll};
use studio_runtime_process::{
    BackendId, BackendIdentity, Generation, ProviderHandle, ProviderLaunchSpec,
    provider_seam::{JournalPayload, OpenOptions, ProviderTaskFactory, SessionClock, Shutdown},
    provider_task::SupervisorProviderTaskFactory,
};
use studio_runtime_providers::{Frame, RequestId};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

async fn request(stream: &mut BufReader<UnixStream>) -> Value {
    let mut line = String::new();
    stream.read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}
async fn respond(stream: &mut BufReader<UnixStream>, value: Value) {
    stream
        .get_mut()
        .write_all(format!("{value}\n").as_bytes())
        .await
        .unwrap();
}
fn options() -> OpenOptions {
    OpenOptions {
        clock: SessionClock::default(),
        launch: ProviderLaunchSpec::new(
            vec!["scripted".into()],
            BTreeMap::new(),
            "/var/tmp".into(),
        )
        .with_inherited_cwd(),
        expected_generation: None,
    }
}

#[tokio::test(start_paused = true)]
async fn two_phase_open_gates_replay_until_worker_installs_mapping() {
    let root = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(root.path().join("supervisor.sock")).unwrap();
    let factory = SupervisorProviderTaskFactory::new(BackendIdentity {
        backend_id: BackendId("fixture".into()),
        state_dir: root.path().into(),
    });
    let handle = ProviderHandle::for_account("fixture");
    let prepare = tokio::spawn(async move {
        factory
            .prepare(handle, options(), Shutdown::default())
            .await
            .unwrap()
    });
    let (socket, _) = listener.accept().await.unwrap();
    let mut stream = BufReader::new(socket);
    let hello = request(&mut stream).await;
    assert_eq!(hello["protocol"], 2);
    assert_eq!(hello["stateDir"], root.path().to_string_lossy().as_ref());
    assert_eq!(hello["backendId"], "fixture");
    respond(&mut stream, json!({"protocol":2,"ok":true})).await;
    let status = request(&mut stream).await;
    respond(
        &mut stream,
        json!({"requestId":status["requestId"],"error":"Unknown supervisor handle"}),
    )
    .await;
    let open = request(&mut stream).await;
    assert!(open["cwd"].is_null());
    respond(&mut stream,json!({"requestId":open["requestId"],"result":{"resumed":false,"initResult":null,"acknowledged":0,"sequence":1,"generation":17,"returnCode":null,"pid":1}})).await;
    let prepared = prepare.await.unwrap();
    assert_eq!(prepared.generation(), Generation(17));
    let mut read = Box::pin(request(&mut stream));
    assert!(
        std::future::poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx).is_pending())).await,
        "no replay before recovery mapping"
    );
    drop(read);
    let mut wrong = options();
    wrong.expected_generation = Some(Generation(16));
    assert!(
        prepared
            .open(
                ProviderHandle::for_account("fixture"),
                wrong,
                Shutdown::default()
            )
            .await
            .is_err()
    );
    let mut recovered = options();
    recovered.expected_generation = Some(Generation(17));
    recovered.launch.retained_response_ids.insert(81, 42);
    let mut attachment = prepared
        .open(
            ProviderHandle::for_account("fixture"),
            recovered,
            Shutdown::default(),
        )
        .await
        .unwrap();
    let mut repeated = options();
    repeated.expected_generation = Some(Generation(17));
    assert!(
        prepared
            .open(
                ProviderHandle::for_account("fixture"),
                repeated,
                Shutdown::default()
            )
            .await
            .is_err()
    );
    let (event_socket, _) = listener.accept().await.unwrap();
    let mut event_stream = BufReader::new(event_socket);
    let event_hello = request(&mut event_stream).await;
    assert_eq!(event_hello["protocol"], 2);
    assert_eq!(
        event_hello["stateDir"],
        root.path().to_string_lossy().as_ref()
    );
    assert_eq!(event_hello["backendId"], "fixture");
    respond(&mut event_stream, json!({"protocol":2,"ok":true})).await;
    let next = request(&mut event_stream).await;
    assert_eq!(next["action"], "nextBatch");
    assert_eq!(next["cursor"], 0);
    respond(&mut event_stream,json!({"requestId":next["requestId"],"result":{"events":[{"sequence":1,"generation":17,"kind":"stdout","payload":{"id":81,"result":{}}}],"returnCode":null,"backpressure":false,"stdoutReaderAlive":true,"stderrReaderAlive":true}})).await;
    let entry = attachment.journal.recv().await.unwrap();
    match entry.payload {
        JournalPayload::Frame(Frame::Response { id, .. }) => {
            assert_eq!(id, RequestId::Number(42))
        }
        _ => panic!("replayed response"),
    }
    attachment.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn cancelled_prepare_never_returns_a_usable_generation() {
    let root = tempfile::tempdir().unwrap();
    let factory = SupervisorProviderTaskFactory::new(BackendIdentity {
        backend_id: BackendId("fixture".into()),
        state_dir: root.path().into(),
    });
    let stop = Shutdown::default();
    stop.cancel();
    assert!(
        factory
            .prepare(ProviderHandle::for_account("fixture"), options(), stop)
            .await
            .is_err()
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::process_support as support;

// Retained child environment verification is implemented only on Linux.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn shutdown_detaches_and_another_backend_reattaches_at_same_generation() {
    let (state, mut supervisor) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-process/tests/wave7_p01_factory",
        || {
            let state = support::DisposableState::new();
            let supervisor = support::SupervisorProcess::start(&state);
            (state, supervisor)
        },
    )
    .await
    .unwrap();
    let factory = SupervisorProviderTaskFactory::new(state.identity());
    let handle = ProviderHandle::for_account("retained");
    let launch = || OpenOptions {
        clock: SessionClock::default(),
        launch: ProviderLaunchSpec::new(
            support::shell_command("printf 'ready\\n' >&2; while IFS= read -r line; do :; done"),
            support::child_environment(&state.0),
            state.0.clone(),
        ),
        expected_generation: None,
    };
    let prepared = factory
        .prepare(handle.clone(), launch(), Shutdown::default())
        .await
        .unwrap();
    let generation = prepared.generation();
    let mut opts = launch();
    opts.expected_generation = Some(generation);
    let mut attachment = prepared
        .open(handle.clone(), opts, Shutdown::default())
        .await
        .unwrap();
    let pid = attachment.status.borrow().info.as_ref().unwrap().pid;
    supervisor.own_child(pid);
    // Receiving an entry proves the event connection was attached as well.
    let replay_cursor = attachment.journal.recv().await.unwrap().cursor;
    let exit = attachment.shutdown().await;
    assert!(exit.cleanup_error.is_none(), "{:?}", exit.cleanup_error);
    assert!(
        support::pid_exists(pid),
        "supervised child must survive backend attachment shutdown"
    );
    let mut identity = state.identity();
    identity.backend_id.0.push_str("-restarted");
    let restarted_factory = SupervisorProviderTaskFactory::new(identity);
    let prepared = restarted_factory
        .prepare(handle.clone(), launch(), Shutdown::default())
        .await
        .unwrap();
    assert_eq!(prepared.generation(), generation);
    let mut opts = launch();
    opts.expected_generation = Some(generation);
    let mut attachment = prepared
        .open(handle, opts, Shutdown::default())
        .await
        .unwrap();
    let info = attachment.status.borrow().info.clone().unwrap();
    assert!(info.resumed);
    assert_eq!(info.pid, pid);
    assert_eq!(info.generation, generation);
    assert_eq!(
        attachment.journal.recv().await.unwrap().cursor,
        replay_cursor
    );
    let exit = attachment.shutdown().await;
    assert!(exit.cleanup_error.is_none(), "{:?}", exit.cleanup_error);
    // Fixture teardown owns the supervisor and its surviving child.
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-process/tests/wave7_p01_factory",
        move || drop(supervisor),
    )
    .await
    .unwrap();
}

// A changed launch command reattaches a running child started with an earlier
// command it lists, and still refuses one started with any other command. The
// running child keeps its own command; a child started later gets the new one.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
#[allow(
    clippy::disallowed_methods,
    reason = "test fixture; `which` and `kill` exit at once"
)]
async fn running_child_started_with_a_previous_command_reattaches() {
    use studio_runtime_process::{EventKind, HandleRequest, HealthRequest, SupervisorHandle};
    use studio_runtime_store::supervisor_journal::launch_signature;
    let (state, mut supervisor) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-process/tests/wave7_p01_factory",
        || {
            let state = support::DisposableState::new();
            let supervisor = support::SupervisorProcess::start(&state);
            (state, supervisor)
        },
    )
    .await
    .unwrap();
    let state = std::sync::Arc::new(state);
    let factory = SupervisorProviderTaskFactory::new(state.identity());
    let handle = ProviderHandle::for_account("retained");
    // A non-platform executable that waits on stdin: macOS hides the launch
    // environment of platform binaries such as /bin/sh, so their reattach is
    // unverifiable.
    let node = std::process::Command::new("which")
        .arg("node")
        .output()
        .unwrap();
    assert!(node.status.success());
    let node = String::from_utf8(node.stdout).unwrap().trim().to_owned();
    let command = |extra: &[&str]| {
        [node.as_str(), "-e", "process.stdin.resume()"]
            .into_iter()
            .chain(extra.iter().copied())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let launch = |command: Vec<String>, reattach_commands: Vec<Vec<String>>| {
        let mut launch = ProviderLaunchSpec::new(
            command,
            support::child_environment(&state.0),
            state.0.clone(),
        );
        launch.reattach_commands = reattach_commands;
        OpenOptions {
            clock: SessionClock::default(),
            launch,
            expected_generation: None,
        }
    };
    let attach = |command: Vec<String>, reattach_commands: Vec<Vec<String>>| {
        let factory = &factory;
        let handle = handle.clone();
        let options = launch(command.clone(), reattach_commands.clone());
        let mut reopened = launch(command, reattach_commands);
        async move {
            let prepared = factory
                .prepare(handle.clone(), options, Shutdown::default())
                .await?;
            reopened.expected_generation = Some(prepared.generation());
            prepared.open(handle, reopened, Shutdown::default()).await
        }
    };
    // The launch signature that Health reports for the handle.
    let running_signature = || {
        let state = state.clone();
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-process/tests/wave7_p01_factory",
            move || {
                let health = support::connect_client(&state)
                    .health(HealthRequest::default())
                    .unwrap();
                health
                    .handles
                    .into_iter()
                    .find(|handle| handle.id == "account:retained")
                    .map(|handle| (handle.pid, handle.signature))
            },
        )
    };
    let signature = |command: &[String]| {
        launch_signature(
            command,
            &support::child_environment(&state.0),
            state.0.to_str(),
        )
    };
    let previous = command(&[]);
    let current = command(&["current"]);
    let mut attachment = attach(previous.clone(), Vec::new()).await.unwrap();
    let started = attachment.status.borrow().info.clone().unwrap();
    supervisor.own_child(started.pid);
    let _exit = attachment.shutdown().await;

    assert!(
        attach(current.clone(), Vec::new()).await.is_err(),
        "a running child started with an unlisted command is refused"
    );
    let mut attachment = attach(current.clone(), vec![previous.clone()])
        .await
        .unwrap();
    let info = attachment.status.borrow().info.clone().unwrap();
    assert!(info.resumed);
    assert_eq!(info.pid, started.pid);
    assert_eq!(info.generation, started.generation);
    let _exit = attachment.shutdown().await;
    assert_eq!(
        running_signature().await.unwrap(),
        Some((Some(started.pid), signature(&previous))),
        "the reattached child keeps its own command and is not restarted"
    );

    // After that child exits, the next child starts with the current command.
    {
        let state = state.clone();
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-process/tests/wave7_p01_factory",
            move || {
                let killed = std::process::Command::new("kill")
                    .args(["-KILL", &started.pid.to_string()])
                    .status()
                    .unwrap();
                assert!(killed.success());
                let mut client = support::connect_client(&state);
                let handle = SupervisorHandle("account:retained".into());
                support::poll_event(
                    &mut client,
                    &handle,
                    studio_runtime_process::EventCursor(0),
                    |event| event.kind == EventKind::Exit,
                );
                let status = client
                    .status(HandleRequest {
                        handle,
                        extra: Default::default(),
                    })
                    .unwrap();
                assert!(status.return_code.is_some());
            },
        )
        .await
        .unwrap();
    }
    let mut attachment = attach(current.clone(), vec![previous]).await.unwrap();
    let restarted = attachment.status.borrow().info.clone().unwrap();
    assert!(!restarted.resumed);
    assert_ne!(restarted.pid, started.pid);
    supervisor.own_child(restarted.pid);
    let _exit = attachment.shutdown().await;
    assert_eq!(
        running_signature().await.unwrap(),
        Some((Some(restarted.pid), signature(&current)))
    );
    // Fixture teardown owns the supervisor and its surviving child.
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-process/tests/wave7_p01_factory",
        move || drop(supervisor),
    )
    .await
    .unwrap();
}

/// A running child started before a launch variable existed lacks it. The
/// Claude bridges of 69637aa6e were started without
/// `STUDIO_CLAUDE_CREDENTIAL_IDENTITY`, which a later launch adds. Such a child
/// is still the provider of this launch: it reattaches with its own
/// environment and is not restarted. A child whose environment differs in any
/// other way stays unverifiable.
#[tokio::test]
#[allow(
    clippy::disallowed_methods,
    reason = "test fixture; `which` exits at once"
)]
async fn running_child_started_before_a_launch_variable_existed_reattaches() {
    use studio_runtime_process::HealthRequest;
    use studio_runtime_store::supervisor_journal::launch_signature;
    let (state, mut supervisor) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-process/tests/wave7_p01_factory",
        || {
            let state = support::DisposableState::new();
            let supervisor = support::SupervisorProcess::start(&state);
            (state, supervisor)
        },
    )
    .await
    .unwrap();
    let state = std::sync::Arc::new(state);
    let factory = SupervisorProviderTaskFactory::new(state.identity());
    let handle = ProviderHandle::for_account("retained");
    let node = std::process::Command::new("which")
        .arg("node")
        .output()
        .unwrap();
    assert!(node.status.success());
    let node = String::from_utf8(node.stdout).unwrap().trim().to_owned();
    let command = vec![node, "-e".to_owned(), "process.stdin.resume()".to_owned()];
    let previous = support::child_environment(&state.0);
    let with = |name: &str, value: &str| {
        let mut environment = previous.clone();
        environment.insert(name.to_owned(), value.to_owned());
        environment
    };
    let launch = |environment: BTreeMap<String, String>| OpenOptions {
        clock: SessionClock::default(),
        launch: ProviderLaunchSpec::new(command.clone(), environment, state.0.clone()),
        expected_generation: None,
    };
    let attach = |environment: BTreeMap<String, String>| {
        let factory = &factory;
        let handle = handle.clone();
        let options = launch(environment.clone());
        let mut reopened = launch(environment);
        async move {
            let prepared = factory
                .prepare(handle.clone(), options, Shutdown::default())
                .await?;
            reopened.expected_generation = Some(prepared.generation());
            prepared.open(handle, reopened, Shutdown::default()).await
        }
    };
    let mut attachment = attach(previous.clone()).await.unwrap();
    let started = attachment.status.borrow().info.clone().unwrap();
    supervisor.own_child(started.pid);
    let _exit = attachment.shutdown().await;

    let mut attachment = attach(with(
        "STUDIO_CLAUDE_CREDENTIAL_IDENTITY",
        "claude:a@example.test",
    ))
    .await
    .unwrap();
    let info = attachment.status.borrow().info.clone().unwrap();
    assert!(info.resumed);
    assert_eq!(info.pid, started.pid);
    assert_eq!(info.generation, started.generation);
    let _exit = attachment.shutdown().await;
    let running = {
        let state = state.clone();
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-process/tests/wave7_p01_factory",
            move || {
                support::connect_client(&state)
                    .health(HealthRequest::default())
                    .unwrap()
                    .handles
                    .into_iter()
                    .find(|handle| handle.id == "account:retained")
                    .map(|handle| (handle.pid, handle.signature))
            },
        )
        .await
        .unwrap()
    };
    assert_eq!(
        running,
        Some((
            Some(started.pid),
            launch_signature(&command, &previous, state.0.to_str())
        )),
        "the reattached child keeps its own environment"
    );
    // Fixture teardown owns the supervisor and its surviving child.
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-process/tests/wave7_p01_factory",
        move || drop(supervisor),
    )
    .await
    .unwrap();
}
