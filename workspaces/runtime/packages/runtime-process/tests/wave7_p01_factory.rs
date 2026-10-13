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
    assert_eq!(hello["protocol"], 1);
    respond(&mut stream, json!({"protocol":1,"ok":true})).await;
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
    let next = request(&mut stream).await;
    assert_eq!(next["action"], "next");
    respond(&mut stream,json!({"requestId":next["requestId"],"result":{"event":{"sequence":1,"generation":17,"kind":"stdout","payload":"{\"id\":81,\"result\":{}}"},"returnCode":null,"backpressure":false,"stdoutReaderAlive":true,"stderrReaderAlive":true}})).await;
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

mod support;

#[tokio::test]
async fn shutdown_detaches_and_supervised_child_reattaches_at_same_generation() {
    let (state, mut supervisor, python) = tokio::task::spawn_blocking(|| {
        let state = support::DisposableState::new();
        let python = support::python_executable();
        let supervisor = support::SupervisorProcess::start(&state, &python);
        (state, supervisor, python)
    })
    .await
    .unwrap();
    let factory = SupervisorProviderTaskFactory::new(state.identity());
    let handle = ProviderHandle::for_account("retained");
    let launch = || OpenOptions {
        clock: SessionClock::default(),
        launch: ProviderLaunchSpec::new(
            vec![
                python.clone(),
                "-u".into(),
                "-c".into(),
                "import sys\nfor line in sys.stdin: pass".into(),
            ],
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
    let _exit = attachment.shutdown().await;
    assert!(
        support::pid_exists(pid),
        "supervised child must survive backend attachment shutdown"
    );
    let prepared = factory
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
    let _exit = attachment.shutdown().await;
    // Fixture teardown owns the supervisor and its surviving child.
    tokio::task::spawn_blocking(move || drop(supervisor))
        .await
        .unwrap();
}
