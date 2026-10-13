#![cfg(unix)]
use std::{collections::BTreeMap, time::Duration};
use studio_runtime_process::{
    Generation, ProviderHandle, ProviderLaunchSpec, provider_seam::*,
    provider_stdio::StdioProviderTaskFactory,
};
use tokio::time::Instant;

async fn child(script: &str) -> ProviderAttachment {
    StdioProviderTaskFactory {
        deadline: Instant::now() + Duration::from_secs(30),
        max_frame_bytes: 1024,
        max_output_bytes: 4096,
    }
    .open(
        ProviderHandle::for_account("stdio-test"),
        OpenOptions {
            clock: SessionClock::default(),
            expected_generation: Some(Generation(1)),
            launch: ProviderLaunchSpec::new(
                vec!["/bin/sh".into(), "-c".into(), script.into()],
                BTreeMap::new(),
                "/var/tmp".into(),
            ),
        },
        Shutdown::default(),
    )
    .await
    .expect("child starts")
}
const WAIT: &str = "printf '{\"jsonrpc\":\"2.0\",\"method\":\"ready\"}\\n'; read line";

fn pid(attachment: &ProviderAttachment) -> u32 {
    attachment
        .status
        .borrow()
        .info
        .as_ref()
        .expect("child info")
        .pid
}
fn assert_reaped(pid: u32) {
    #[cfg(target_os = "linux")]
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "child must be reaped"
    );
    #[cfg(not(target_os = "linux"))]
    let _pid = pid;
}

#[tokio::test]
async fn explicit_cancel_stops_and_reaps_child() {
    let mut attachment = child(WAIT).await;
    let pid = pid(&attachment);
    attachment.journal.recv().await.expect("child ready");
    assert!(attachment.shutdown().await.result.is_ok());
    assert_reaped(pid);
}

#[tokio::test]
async fn deadline_stops_and_reaps_child() {
    let mut attachment = child(WAIT).await;
    let pid = pid(&attachment);
    attachment.journal.recv().await.expect("child ready");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(30)).await;
    // Reaping observes an OS process; its grace period uses real time.
    tokio::time::resume();
    assert!(matches!(
        attachment.completion().await.result,
        Err(AdapterError::Timeout)
    ));
    assert_reaped(pid);
}

#[tokio::test]
async fn dropping_caller_future_stops_and_reaps_child() {
    let mut attachment = child(WAIT).await;
    let pid = pid(&attachment);
    attachment.journal.recv().await.expect("child ready");
    let mut status = attachment.status.clone();
    let caller = tokio::spawn(async move {
        let _owned = attachment;
        std::future::pending::<()>().await;
    });
    caller.abort();
    assert!(caller.await.expect_err("caller cancelled").is_cancelled());
    while status.changed().await.is_ok() {}
    assert_reaped(pid);
}

#[tokio::test]
async fn stdout_eof_stops_and_reaps_child() {
    let mut attachment = child("exec 1>&-; read line").await;
    let pid = pid(&attachment);
    assert!(attachment.completion().await.result.is_err());
    assert_reaped(pid);
}

#[tokio::test]
async fn oversized_stdout_stops_and_reaps_child() {
    let mut attachment = child("printf '%2048s' x; read line").await;
    let pid = pid(&attachment);
    assert!(attachment.completion().await.result.is_err());
    assert_reaped(pid);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn stalled_stdin_is_interrupted_by_the_exchange_deadline() {
    use studio_runtime_process::OperationId;
    use studio_runtime_providers::{Frame, Method, RequestFrame, RequestId};
    let mut attachment = child(r#"exec /usr/bin/python3 -c 'import os,signal,fcntl,json; print(json.dumps({"method":"pipe-ready","params":{"capacity":fcntl.fcntl(0,fcntl.F_GETPIPE_SZ)}}),flush=True); os.read(0,1); print("{\"method\":\"stdin-progress\"}",flush=True); signal.pause()'"#).await;
    let pid = pid(&attachment);
    let ready = attachment.journal.recv().await.expect("child ready");
    let JournalPayload::Frame(Frame::UnknownMethod(ready)) = ready.payload else {
        panic!("child reports its pipe capacity");
    };
    let capacity = ready.fields["params"]["capacity"]
        .as_u64()
        .expect("pipe capacity") as usize;
    let frame = Frame::Request(RequestFrame {
        id: RequestId::Number(1),
        method: Method::ModelList,
        params: Some(serde_json::json!({"padding":"x".repeat(capacity + 4096)})),
        fields: Default::default(),
    });
    let write =
        PreparedWrite::new(OperationId("blocked-write".into()), &frame).expect("bounded frame");
    let (command, receipt) = AdapterCommand::write(write);
    attachment
        .commands
        .send(command)
        .await
        .expect("command admitted");
    // One byte has reached the child. It consumes no further input, so the
    // frame, larger than the measured capacity, cannot finish writing.
    attachment
        .journal
        .recv()
        .await
        .expect("stdin write reached the child");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(30)).await;
    // Reaping observes an OS process; its grace period uses real time.
    tokio::time::resume();
    assert!(matches!(
        attachment.completion().await.result,
        Err(AdapterError::Timeout)
    ));
    assert!(matches!(receipt.receive().await, CommandReceipt::Unknown));
    assert_reaped(pid);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancellation_stops_the_entire_owned_process_group() {
    use tokio::{io::AsyncReadExt, net::UnixListener};
    let socket_path =
        std::env::temp_dir().join(format!("catalog-group-{}.sock", std::process::id()));
    let listener = UnixListener::bind(&socket_path).expect("test socket");
    let code = format!(
        "import os,json,signal,socket; connection=socket.socket(socket.AF_UNIX); connection.connect({:?}); child=os.fork(); print(json.dumps({{\"method\":\"ready\",\"pid\":os.getpid(),\"group\":os.getpgrp()}}),flush=True) if child else None; signal.pause()",
        socket_path
    );
    let factory = StdioProviderTaskFactory {
        deadline: Instant::now() + Duration::from_secs(30),
        max_frame_bytes: 1024,
        max_output_bytes: 4096,
    };
    let mut attachment = factory
        .open(
            ProviderHandle::for_account("group-test"),
            OpenOptions {
                clock: SessionClock::default(),
                expected_generation: Some(Generation(1)),
                launch: ProviderLaunchSpec::new(
                    vec!["/usr/bin/python3".into(), "-c".into(), code],
                    BTreeMap::new(),
                    "/var/tmp".into(),
                ),
            },
            Shutdown::default(),
        )
        .await
        .expect("group starts");
    let (mut connection, _) = listener.accept().await.expect("child connected");
    let parent = pid(&attachment);
    let entry = attachment.journal.recv().await.expect("group ready");
    let JournalPayload::Frame(studio_runtime_providers::Frame::UnknownMethod(frame)) =
        entry.payload
    else {
        panic!("ready frame");
    };
    assert_eq!(frame.fields["pid"], parent);
    assert_eq!(frame.fields["group"], parent);
    assert!(attachment.shutdown().await.result.is_ok());
    assert_reaped(parent);
    // Both processes keep this connection open until exit. EOF observes the
    // descendant's termination without racing /proc's transient process state.
    assert_eq!(connection.read(&mut [0_u8; 1]).await.expect("group EOF"), 0);
    std::fs::remove_file(socket_path).expect("remove test socket");
}
