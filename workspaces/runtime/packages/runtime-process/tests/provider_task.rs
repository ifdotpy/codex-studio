#![cfg(unix)]
#![allow(clippy::unwrap_used)]
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use studio_runtime_process::{provider_seam::*, provider_task::SupervisorProviderTaskFactory, *};
use studio_runtime_providers::{DecodedJournalPayload, Frame, RequestId, decode_journal_entry};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};
mod support;
use support::*;

struct Peer(BufReader<UnixStream>);
impl Peer {
    async fn read(&mut self) -> Option<Value> {
        let mut line = String::new();
        let count = self.0.read_line(&mut line).await.unwrap();
        (count != 0).then(|| serde_json::from_str(&line).unwrap())
    }
    async fn request(&mut self, action: &str) -> Value {
        let value = self
            .read()
            .await
            .expect("adapter disconnected unexpectedly");
        assert_eq!(value["action"], action);
        value
    }
    async fn send(&mut self, value: Value) {
        self.0
            .get_mut()
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn result(&mut self, request: &Value, result: Value) {
        self.send(json!({"requestId":request["requestId"], "result":result}))
            .await;
    }
    async fn error(&mut self, request: &Value, error: &str) {
        self.send(json!({"requestId":request["requestId"], "error":error}))
            .await;
    }
    async fn next_event(&mut self, cursor: u64, sequence: u64, kind: &str, payload: Value) {
        let next = self.request("next").await;
        assert_eq!(next["cursor"], cursor);
        self.result(&next, event(sequence, kind, payload)).await;
    }
}
fn event(sequence: u64, kind: &str, payload: Value) -> Value {
    json!({"event":{"sequence":sequence,"generation":1,"kind":kind,"payload":payload.to_string()}, "returnCode":null,"backpressure":false,"stdoutReaderAlive":true,"stderrReaderAlive":true})
}
fn empty() -> Value {
    json!({"event":null,"returnCode":null,"backpressure":false,"stdoutReaderAlive":true,"stderrReaderAlive":true})
}
fn options(state: &DisposableState) -> OpenOptions {
    OpenOptions {
        clock: SessionClock::default(),
        launch: ProviderLaunchSpec::new(
            vec!["unused-fake-child".into()],
            BTreeMap::new(),
            state.0.clone(),
        ),
        initialize_params: json!({}),
        expected_generation: None,
    }
}
fn frame(value: Value) -> Frame {
    match decode_journal_entry(0, 1, value.to_string().into_bytes()).payload {
        DecodedJournalPayload::Frame(frame) => frame,
        _ => panic!("not a frame"),
    }
}
fn write(operation: &str, value: Value) -> (AdapterCommand, Receipt) {
    AdapterCommand::write(PreparedWrite::new(OperationId(operation.into()), &frame(value)).unwrap())
}
async fn open_peer(state: &DisposableState) -> (ProviderAttachment, Peer) {
    let listener = UnixListener::bind(state.socket()).unwrap();
    let factory: Arc<dyn ProviderTaskFactory> =
        Arc::new(SupervisorProviderTaskFactory::new(state.identity()));
    let attachment = factory
        .open(
            ProviderHandle::for_account("test"),
            options(state),
            Shutdown::default(),
        )
        .await
        .unwrap();
    let (socket, _) = listener.accept().await.unwrap();
    let mut peer = Peer(BufReader::new(socket));
    assert_eq!(peer.read().await.unwrap()["protocol"], 1);
    peer.send(json!({"ok":true})).await;
    let status = peer.request("status").await;
    peer.error(&status, "Unknown supervisor handle").await;
    let open = peer.request("open").await;
    peer.result(&open, json!({"resumed":false,"initResult":null,"acknowledged":0,"sequence":1000,"generation":1,"returnCode":null,"pid":123})).await;
    (attachment, peer)
}

#[tokio::test]
async fn invalid_initialization_refused_before_socket_open() {
    let state = DisposableState::new();
    let listener = UnixListener::bind(state.socket()).unwrap();
    let factory = SupervisorProviderTaskFactory::new(state.identity());
    let mut invalid = options(&state);
    invalid.initialize_params = json!({"oversized":"x".repeat(1024 * 1024)});
    assert!(matches!(
        factory
            .open(
                ProviderHandle::for_account("test"),
                invalid,
                Shutdown::default()
            )
            .await,
        Err(AdapterError::InvalidConfiguration)
    ));
    let mut accept = Box::pin(listener.accept());
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(accept.as_mut().poll(cx).is_pending()))
            .await
    );
}

#[tokio::test]
async fn shutdown_interrupts_handshake_and_closes_socket() {
    let state = DisposableState::new();
    let listener = UnixListener::bind(state.socket()).unwrap();
    let factory = SupervisorProviderTaskFactory::new(state.identity());
    let mut attachment = factory
        .open(
            ProviderHandle::for_account("test"),
            options(&state),
            Shutdown::default(),
        )
        .await
        .unwrap();
    let (socket, _) = listener.accept().await.unwrap();
    let mut peer = Peer(BufReader::new(socket));
    peer.read().await.unwrap(); // Actual handshake bytes reached the peer.
    assert!(matches!(
        attachment.shutdown().await.result,
        Err(AdapterError::Cancelled)
    ));
    assert!(peer.read().await.is_none());
}

#[tokio::test]
async fn full_replay_lane_stops_next_but_ack_and_shutdown_remain_live() {
    let state = DisposableState::new();
    let (mut attachment, mut peer) = open_peer(&state).await;
    for cursor in 0..LANE_CAPACITY as u64 {
        peer.next_event(cursor, cursor + 1, "stdout", json!({"method":"notice"}))
            .await;
    }
    // Commands serialize after the final event response. If `next` were issued
    // without capacity, it would appear before this ACK and fail the assertion.
    let (ack, receipt) = AdapterCommand::ack(EventCursor(1));
    attachment
        .commands
        .send(ack)
        .await
        .unwrap_or_else(|_| panic!("closed lane"));
    let request = peer.request("ack").await;
    assert_eq!(attachment.journal.len(), LANE_CAPACITY);
    peer.result(&request, json!({"acknowledged":1})).await;
    assert!(matches!(
        receipt.receive().await,
        CommandReceipt::Acknowledged(EventCursor(1))
    ));
    let exit = attachment.shutdown().await;
    assert!(exit.recovery_needed.is_some());
    assert!(peer.read().await.is_none());
}

#[tokio::test]
async fn stdout_stderr_exit_raw_keep_cursors_and_never_auto_ack() {
    let state = DisposableState::new();
    let (mut attachment, mut peer) = open_peer(&state).await;
    let entries = [
        ("stdout", json!({"method":"notice"})),
        ("stderr", json!({"data":"error"})),
        ("exit", json!({"returnCode":7})),
        ("stdout", json!({"supervisorRaw":"not-json\n"})),
    ];
    for (index, (kind, payload)) in entries.into_iter().enumerate() {
        peer.next_event(index as u64, index as u64 + 1, kind, payload)
            .await;
        let entry = attachment.journal.recv().await.unwrap();
        assert_eq!(entry.cursor.0, index as u64 + 1);
        assert_eq!(entry.generation, Generation(1));
        match index {
            0 => assert!(matches!(entry.payload, JournalPayload::Frame(_))),
            1 => assert!(matches!(entry.payload, JournalPayload::Stderr(_))),
            2 => assert!(matches!(entry.payload, JournalPayload::Exit(_))),
            _ => assert!(
                matches!(entry.payload, JournalPayload::RawStdout { bytes, .. } if bytes == b"not-json\n")
            ),
        }
    }
    // The request after every entry is next, never ACK, including at shutdown.
    peer.request("next").await;
    attachment.shutdown().await;
    assert!(peer.read().await.is_none());
}

#[tokio::test]
async fn duplicate_cursor_is_not_delivered_twice() {
    let state = DisposableState::new();
    let (mut attachment, mut peer) = open_peer(&state).await;
    peer.next_event(0, 1, "stdout", json!({"method":"notice"}))
        .await;
    assert_eq!(
        attachment.journal.recv().await.unwrap().cursor,
        EventCursor(1)
    );
    peer.next_event(1, 1, "stdout", json!({"method":"notice"}))
        .await;
    assert!(attachment.completion().await.result.is_err());
    assert!(attachment.journal.recv().await.is_none());
}

#[tokio::test]
async fn lost_write_receipt_is_unknown_and_remaining_commands_are_not_sent() {
    let state = DisposableState::new();
    let (mut attachment, mut peer) = open_peer(&state).await;
    let next = peer.request("next").await;
    let (first, receipt) = write("owned:1", json!({"id":7,"method":"model/list"}));
    attachment
        .commands
        .send(first)
        .await
        .unwrap_or_else(|_| panic!("closed"));
    peer.result(&next, empty()).await;
    let request = peer.request("write").await;
    assert_eq!(request["operationId"], "owned:1");
    let (second, second_receipt) = write("owned:2", json!({"id":8,"method":"model/list"}));
    attachment
        .commands
        .send(second)
        .await
        .unwrap_or_else(|_| panic!("closed"));
    drop(peer); // Acceptance may have happened; its receipt did not arrive.
    assert!(matches!(receipt.receive().await, CommandReceipt::Unknown));
    assert!(matches!(
        second_receipt.receive().await,
        CommandReceipt::NotSent
    ));
    assert!(attachment.completion().await.result.is_err());
    assert!(attachment.commands.is_closed());
}

#[tokio::test]
async fn readiness_requires_real_initialized_receipt_and_stable_identity() {
    let state = DisposableState::new();
    let (mut attachment, mut peer) = open_peer(&state).await;
    let next = peer.request("next").await;
    let (initialize, receipt) = write(
        "caller:init",
        json!({"id":"claude-init","method":"initialize","params":{}}),
    );
    attachment
        .commands
        .send(initialize)
        .await
        .unwrap_or_else(|_| panic!("closed"));
    peer.result(&next, empty()).await;
    let request = peer.request("write").await;
    assert_eq!(request["nativeId"], 1);
    peer.result(
        &request,
        json!({"accepted":true,"duplicate":false,"remoteId":1,"durableMs":0}),
    )
    .await;
    assert!(matches!(
        receipt.receive().await,
        CommandReceipt::Written { .. }
    ));
    peer.next_event(0, 1, "stdout", json!({"id":1,"result":{}}))
        .await;
    let entry = attachment.journal.recv().await.unwrap();
    assert!(
        matches!(entry.payload, JournalPayload::Frame(Frame::Response { id: RequestId::String(id), .. }) if id == "claude-init")
    );
    assert_eq!(
        attachment.status.borrow().initialization,
        InitializationStatus::InitializedWritePending
    );
    let next = peer.request("next").await;
    let (invalid, refused) = write("wrong-initialized-id", json!({"method":"initialized"}));
    attachment
        .commands
        .send(invalid)
        .await
        .unwrap_or_else(|_| panic!("closed"));
    let (initialized, receipt) = write("initialized:account:test", json!({"method":"initialized"}));
    attachment
        .commands
        .send(initialized)
        .await
        .unwrap_or_else(|_| panic!("closed"));
    peer.result(&next, empty()).await;
    assert!(matches!(refused.receive().await, CommandReceipt::NotSent));
    let request = peer.request("write").await;
    assert_eq!(request["operationId"], "initialized:account:test");
    assert_eq!(
        attachment.status.borrow().initialization,
        InitializationStatus::InitializedWritePending
    );
    peer.result(
        &request,
        json!({"accepted":true,"duplicate":false,"remoteId":null,"durableMs":0}),
    )
    .await;
    assert!(matches!(
        receipt.receive().await,
        CommandReceipt::Written { .. }
    ));
    assert_eq!(
        attachment.status.borrow().initialization,
        InitializationStatus::Ready
    );
    assert!(attachment.shutdown().await.recovery_needed.is_none());
}

#[tokio::test]
async fn failed_status_probe_is_not_absence_and_never_opens_child() {
    let state = DisposableState::new();
    let listener = UnixListener::bind(state.socket()).unwrap();
    let factory = SupervisorProviderTaskFactory::new(state.identity());
    let mut attachment = factory
        .open(
            ProviderHandle::for_account("test"),
            options(&state),
            Shutdown::default(),
        )
        .await
        .unwrap();
    let (socket, _) = listener.accept().await.unwrap();
    let mut peer = Peer(BufReader::new(socket));
    peer.read().await.unwrap();
    peer.send(json!({"ok":true})).await;
    let status = peer.request("status").await;
    peer.error(&status, "status probe failed").await;
    assert!(attachment.completion().await.result.is_err());
    assert!(peer.read().await.is_none());
}

#[tokio::test]
async fn unverifiable_retained_child_refused_before_open() {
    let state = DisposableState::new();
    let listener = UnixListener::bind(state.socket()).unwrap();
    let factory = SupervisorProviderTaskFactory::new(state.identity());
    let mut attachment = factory
        .open(
            ProviderHandle::for_account("test"),
            options(&state),
            Shutdown::default(),
        )
        .await
        .unwrap();
    let (socket, _) = listener.accept().await.unwrap();
    let mut peer = Peer(BufReader::new(socket));
    peer.read().await.unwrap();
    peer.send(json!({"ok":true})).await;
    let status = peer.request("status").await;
    peer.result(&status, json!({"pid":u32::MAX,"sequence":0,"acknowledged":0,"returnCode":null,"backpressure":false})).await;
    assert!(matches!(
        attachment.completion().await.result,
        Err(AdapterError::UnverifiableChild)
    ));
    assert!(peer.read().await.is_none());
}

#[tokio::test]
async fn shutdown_completes_with_full_commands_and_outstanding_sender_permit() {
    let state = DisposableState::new();
    let (mut attachment, mut peer) = open_peer(&state).await;
    peer.request("next").await; // Task is inside a cancellable RPC.
    let sender = attachment.commands.clone();
    let reserved = sender.reserve().await.unwrap();
    let mut receipts = Vec::new();
    for _ in 1..LANE_CAPACITY {
        let (ack, receipt) = AdapterCommand::ack(EventCursor(0));
        attachment
            .commands
            .send(ack)
            .await
            .unwrap_or_else(|_| panic!("closed"));
        receipts.push(receipt);
    }
    assert_eq!(attachment.commands.capacity(), 0);
    attachment.shutdown().await;
    for receipt in receipts {
        assert!(matches!(receipt.receive().await, CommandReceipt::NotSent));
    }
    drop(reserved);
    assert!(peer.read().await.is_none());
}

#[tokio::test]
async fn cancelling_shutdown_future_keeps_owned_join_handle() {
    let state = DisposableState::new();
    let (mut attachment, mut peer) = open_peer(&state).await;
    peer.request("next").await;
    // Poll completion while actor is blocked in RPC, then discard that wait.
    let mut completion = Box::pin(attachment.completion());
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(
            completion.as_mut().poll(cx).is_pending()
        ))
        .await
    );
    drop(completion);
    assert!(matches!(
        attachment.shutdown().await.result,
        Err(AdapterError::Cancelled)
    ));
    assert!(peer.read().await.is_none());
}

async fn real_info(attachment: &mut ProviderAttachment) -> AttachmentInfo {
    attachment
        .status
        .wait_for(|status| status.info.is_some())
        .await
        .unwrap()
        .info
        .clone()
        .unwrap()
}
async fn real_write(
    attachment: &ProviderAttachment,
    operation: &str,
    value: Value,
) -> CommandReceipt {
    let (command, receipt) = write(operation, value);
    attachment
        .commands
        .send(command)
        .await
        .unwrap_or_else(|_| panic!("closed command lane"));
    receipt.receive().await
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn real_supervisor_incomplete_initialization_recovers_same_child_and_replays_after_ack() {
    let state = DisposableState::new();
    let python = python_executable();
    let mut supervisor = SupervisorProcess::start(&state, &python);
    let script = r#"
import json,sys
for line in sys.stdin:
 m=json.loads(line)
 if m.get('method')=='initialize': print(json.dumps({'id':m['id'],'result':{'ready':True}}),flush=True)
 elif m.get('method')=='initialized':
  print('not-json',flush=True)
  print(json.dumps({'method':'test/initialized'}),flush=True)
"#;
    let launch = ProviderLaunchSpec::new(
        vec![python, "-u".into(), "-c".into(), script.into()],
        BTreeMap::from([("PYTHONUNBUFFERED".into(), "1".into())]),
        state.0.clone(),
    );
    let factory = SupervisorProviderTaskFactory::new(state.identity());
    let handle = ProviderHandle::for_account("recover-init");
    let opts = || OpenOptions {
        clock: SessionClock::default(),
        launch: launch.clone(),
        initialize_params: json!({}),
        expected_generation: None,
    };
    let mut first = factory
        .open(handle.clone(), opts(), Shutdown::default())
        .await
        .unwrap();
    let info = real_info(&mut first).await;
    supervisor.own_child(info.pid);
    let recovery = first.shutdown().await.recovery_needed.unwrap();
    assert_eq!(recovery.handle, handle);
    assert_eq!(recovery.phase, InitializationStatus::AwaitingReply);
    let mut wrong_generation = opts();
    wrong_generation.expected_generation = Some(Generation(info.generation.0 + 1));
    let mut refused = factory
        .open(handle.clone(), wrong_generation, Shutdown::default())
        .await
        .unwrap();
    let refusal = refused.completion().await;
    assert!(matches!(
        refusal.result,
        Err(AdapterError::UnverifiableChild)
    ));
    assert_eq!(
        refusal.recovery_needed.unwrap().generation,
        Some(info.generation)
    );

    let mut second = factory
        .open(handle.clone(), opts(), Shutdown::default())
        .await
        .unwrap();
    let resumed = real_info(&mut second).await;
    assert_eq!(resumed.pid, info.pid);
    assert_eq!(resumed.generation, recovery.generation.unwrap());
    assert_eq!(
        resumed.initialization_recovery.unwrap().phase,
        recovery.phase
    );
    assert!(matches!(
        real_write(
            &second,
            "caller:init",
            json!({"id":"claude-init","method":"initialize","params":{}})
        )
        .await,
        CommandReceipt::Written { .. }
    ));
    let initialize = second.journal.recv().await.unwrap();
    assert!(
        matches!(initialize.payload, JournalPayload::Frame(Frame::Response { id: RequestId::String(id), .. }) if id == "claude-init")
    );
    assert_eq!(
        second.status.borrow().initialization,
        InitializationStatus::InitializedWritePending
    );
    assert!(matches!(
        real_write(
            &second,
            "initialized:account:recover-init",
            json!({"method":"initialized"})
        )
        .await,
        CommandReceipt::Written { .. }
    ));
    let raw = second.journal.recv().await.unwrap();
    assert!(
        matches!(raw.payload, JournalPayload::RawStdout { bytes, .. } if bytes == b"not-json\n")
    );
    let notification = second.journal.recv().await.unwrap();
    assert!(matches!(notification.payload, JournalPayload::Frame(_)));
    // Receiving three entries has not acknowledged any of them.
    assert_eq!(
        second.status.borrow().info.as_ref().unwrap().acknowledged,
        EventCursor(0)
    );
    let (ack, receipt) = AdapterCommand::ack(initialize.cursor);
    second
        .commands
        .send(ack)
        .await
        .unwrap_or_else(|_| panic!("closed"));
    assert!(
        matches!(receipt.receive().await, CommandReceipt::Acknowledged(cursor) if cursor == initialize.cursor)
    );
    assert!(second.shutdown().await.recovery_needed.is_none());
    let mut recovered_options = opts();
    recovered_options.launch.handled_cursor = notification.cursor.0; // Must NOT auto-ACK this hint.
    let mut third = factory
        .open(handle, recovered_options, Shutdown::default())
        .await
        .unwrap();
    let resumed = real_info(&mut third).await;
    assert_eq!(resumed.acknowledged, initialize.cursor);
    assert_eq!(resumed.pid, info.pid);
    // Cached initialize is a control receipt, separate from journal capacity.
    assert!(matches!(
        real_write(
            &third,
            "new-attachment:init",
            json!({"id":"reattach-init","method":"initialize","params":{}})
        )
        .await,
        CommandReceipt::CachedInitialize(_)
    ));
    assert_eq!(third.journal.recv().await.unwrap().cursor, raw.cursor);
    assert_eq!(
        third.journal.recv().await.unwrap().cursor,
        notification.cursor
    );
    assert!(matches!(
        real_write(
            &third,
            "initialized:account:recover-init",
            json!({"method":"initialized"})
        )
        .await,
        CommandReceipt::Written {
            duplicate: true,
            ..
        }
    ));
    assert!(third.shutdown().await.recovery_needed.is_none());
}

#[tokio::test]
async fn lost_ack_receipt_is_unknown_and_does_not_retry() {
    let state = DisposableState::new();
    let (mut attachment, mut peer) = open_peer(&state).await;
    peer.next_event(0, 1, "stderr", json!({"data":"diagnostic"}))
        .await;
    attachment.journal.recv().await.unwrap();
    let next = peer.request("next").await;
    let (command, receipt) = AdapterCommand::ack(EventCursor(1));
    attachment
        .commands
        .send(command)
        .await
        .unwrap_or_else(|_| panic!("closed"));
    peer.result(&next, empty()).await;
    let ack = peer.request("ack").await;
    assert_eq!(ack["sequence"], 1);
    drop(peer);
    assert!(matches!(receipt.receive().await, CommandReceipt::Unknown));
    assert!(attachment.completion().await.result.is_err());
    assert!(attachment.commands.is_closed());
}

#[tokio::test]
async fn caller_losing_write_receipt_closes_attachment() {
    let state = DisposableState::new();
    let (mut attachment, mut peer) = open_peer(&state).await;
    let next = peer.request("next").await;
    let (command, receipt) = write("caller:lost", json!({"method":"notice"}));
    attachment
        .commands
        .send(command)
        .await
        .unwrap_or_else(|_| panic!("closed"));
    peer.result(&next, empty()).await;
    let written = peer.request("write").await;
    drop(receipt);
    peer.result(
        &written,
        json!({"accepted":true,"duplicate":false,"remoteId":null,"durableMs":0}),
    )
    .await;
    assert!(attachment.completion().await.result.is_err());
    assert!(peer.read().await.is_none());
}

#[tokio::test]
async fn shutdown_after_initialize_reply_before_initialized_receipt_reports_recovery() {
    let state = DisposableState::new();
    let (mut attachment, mut peer) = open_peer(&state).await;
    let next = peer.request("next").await;
    let (command, receipt) = write("caller:init", json!({"id":1,"method":"initialize"}));
    attachment
        .commands
        .send(command)
        .await
        .unwrap_or_else(|_| panic!("closed"));
    peer.result(&next, empty()).await;
    let written = peer.request("write").await;
    peer.result(
        &written,
        json!({"accepted":true,"duplicate":false,"remoteId":1,"durableMs":0}),
    )
    .await;
    receipt.receive().await;
    peer.next_event(0, 1, "stdout", json!({"id":1,"result":{}}))
        .await;
    attachment.journal.recv().await.unwrap();
    let next = peer.request("next").await;
    let (command, receipt) = write("initialized:account:test", json!({"method":"initialized"}));
    attachment
        .commands
        .send(command)
        .await
        .unwrap_or_else(|_| panic!("closed"));
    peer.result(&next, empty()).await;
    peer.request("write").await; // Receipt deliberately held.
    let exit = attachment.shutdown().await;
    assert!(matches!(receipt.receive().await, CommandReceipt::Unknown));
    assert_eq!(
        exit.recovery_needed.unwrap().phase,
        InitializationStatus::InitializedWritePending
    );
    assert!(peer.read().await.is_none());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn full_unix_connect_backlog_fails_closed_without_reconnect_or_stranded_task() {
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    struct ListenerChild(std::process::Child);
    impl Drop for ListenerChild {
        fn drop(&mut self) {
            let _ignored = self.0.kill();
            let _ignored = self.0.wait();
        }
    }
    let state = DisposableState::new();
    // listen(0) gives one admission slot on Linux. The stdout line is a bind
    // barrier, not a sleep/window, and the retained socket fills that exact slot.
    let mut child = ListenerChild(Command::new(python_executable()).args(["-u", "-c", "import socket,sys; s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen(0); print('bound',flush=True); sys.stdin.read()"])
        .arg(state.socket()).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap());
    let mut line = String::new();
    std::io::BufReader::new(child.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "bound");
    let _occupant = UnixStream::connect(state.socket()).await.unwrap();
    let factory = SupervisorProviderTaskFactory::new(state.identity());
    let mut attachment = factory
        .open(
            ProviderHandle::for_account("test"),
            options(&state),
            Shutdown::default(),
        )
        .await
        .unwrap();
    // Tokio/mio reports the saturated AF_UNIX connect as WouldBlock on Linux;
    // no retry is scheduled, and completion proves the socket task is gone.
    let exit = attachment.completion().await;
    assert!(
        matches!(exit.result, Err(AdapterError::Supervisor(SupervisorError::Io(ref error))) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    assert!(exit.recovery_needed.is_none());
    assert!(attachment.commands.is_closed());
}

#[cfg(not(target_os = "linux"))]
#[tokio::test]
async fn non_linux_reattach_refuses_even_an_exited_child_before_open() {
    let state = DisposableState::new();
    let listener = UnixListener::bind(state.socket()).unwrap();
    let factory = SupervisorProviderTaskFactory::new(state.identity());
    let mut attachment = factory
        .open(
            ProviderHandle::for_account("test"),
            options(&state),
            Shutdown::default(),
        )
        .await
        .unwrap();
    let (socket, _) = listener.accept().await.unwrap();
    let mut peer = Peer(BufReader::new(socket));
    peer.read().await.unwrap();
    peer.send(json!({"ok":true})).await;
    let request = peer.request("status").await;
    peer.result(
        &request,
        json!({"pid":123,"sequence":0,"acknowledged":0,"returnCode":0,"backpressure":false}),
    )
    .await;
    assert!(matches!(
        attachment.completion().await.result,
        Err(AdapterError::UnverifiableChild)
    ));
    assert!(peer.read().await.is_none());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn real_supervisor_lost_write_receipt_is_sent_once_without_flush_retry() {
    let state = DisposableState::new();
    let python = python_executable();
    let mut supervisor = SupervisorProcess::start(&state, &python);
    let actual = state.0.join("actual.sock");
    std::fs::rename(state.socket(), &actual).unwrap();
    let listener = UnixListener::bind(state.socket()).unwrap();
    let (accepted, seen) = tokio::sync::oneshot::channel();
    let proxy = tokio::spawn(async move {
        let (client, _) = listener.accept().await.unwrap();
        let mut client = Peer(BufReader::new(client));
        let mut backend = Peer(BufReader::new(UnixStream::connect(actual).await.unwrap()));
        loop {
            let request = client.read().await.unwrap();
            let drop_receipt =
                request["action"] == "write" && request["message"]["method"] == "model/list";
            backend.send(request.clone()).await;
            let response = backend.read().await.unwrap();
            if drop_receipt {
                assert_eq!(response["result"]["accepted"], true);
                accepted.send(request).unwrap();
                // The response is lost after actual supervisor acceptance.
                drop(client);
                return listener;
            }
            client.send(response).await;
        }
    });
    let script = "import sys,json\nfor line in sys.stdin:\n m=json.loads(line)\n if 'id' in m: print(json.dumps({'id':m['id'],'result':{}}),flush=True)";
    let launch = ProviderLaunchSpec::new(
        vec![python, "-u".into(), "-c".into(), script.into()],
        BTreeMap::from([("PYTHONUNBUFFERED".into(), "1".into())]),
        state.0.clone(),
    );
    let factory = SupervisorProviderTaskFactory::new(state.identity());
    let mut attachment = factory
        .open(
            ProviderHandle::for_account("write-once"),
            OpenOptions {
                clock: SessionClock::default(),
                launch,
                initialize_params: json!({}),
                expected_generation: None,
            },
            Shutdown::default(),
        )
        .await
        .unwrap();
    let info = real_info(&mut attachment).await;
    supervisor.own_child(info.pid);
    assert!(matches!(
        real_write(&attachment, "init", json!({"id":1,"method":"initialize"})).await,
        CommandReceipt::Written { .. }
    ));
    attachment.journal.recv().await.unwrap();
    assert!(matches!(
        real_write(
            &attachment,
            "initialized:account:write-once",
            json!({"method":"initialized"})
        )
        .await,
        CommandReceipt::Written { .. }
    ));
    assert!(matches!(
        real_write(
            &attachment,
            "caller:exact-id",
            json!({"id":7,"method":"model/list"})
        )
        .await,
        CommandReceipt::Unknown
    ));
    assert_eq!(seen.await.unwrap()["operationId"], "caller:exact-id");
    assert!(attachment.completion().await.result.is_err());
    assert!(attachment.commands.is_closed());
    let listener = proxy.await.unwrap();
    let mut accept = Box::pin(listener.accept());
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(accept.as_mut().poll(cx).is_pending()))
            .await
    );
}
