#![cfg(unix)]
use std::{collections::BTreeMap, time::Duration};
use studio_runtime_process::{
    Generation, ProviderHandle, ProviderLaunchSpec, provider_seam::*,
    provider_stdio::StdioProviderTaskFactory,
};
use tokio::time::Instant;

const PROVIDER_STDERR_COMPLETION_TIMEOUT: Duration = Duration::from_secs(10);
/// Bounds real-process progress in the independent exit witness; unit: wall time.
#[cfg(target_os = "linux")]
const PROVIDER_EXIT_HANG_GUARD: Duration = Duration::from_secs(10);

#[cfg(target_os = "linux")]
#[tokio::test]
async fn killed_provider_is_reaped_with_full_journal_and_unobserved_write() {
    use studio_runtime_process::{
        OperationId, child_priority::ChildPriority, provider_direct::DirectProviderTaskFactory,
    };
    use studio_runtime_providers::{Frame, Method, RequestFrame, RequestId, ResponseBody};
    use tokio::sync::oneshot;

    let root = tempfile::tempdir().expect("isolated provider cwd");
    // Emit one known reply and enough frames to block journal delivery, then
    // stop reading or answering. This is a bounded fixture, not an output flood.
    let script = format!(
        r#"printf '{{"id":7,"result":{{}}}}\n'; remaining={LANE_CAPACITY}; while [ "$remaining" -gt 0 ]; do printf '{{"method":"ready"}}\n'; remaining=$((remaining - 1)); done; while IFS= read -r line; do :; done"#
    );
    let handle = ProviderHandle::for_account("exit-with-full-lanes");
    let launch = ProviderLaunchSpec::new(
        crate::process_support::shell_command(&script),
        BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
        root.path().to_path_buf(),
    );
    let options = OpenOptions {
        clock: SessionClock::default(),
        expected_generation: Some(Generation(1)),
        launch: launch.clone(),
    };
    let shutdown = Shutdown::default();
    let factory = DirectProviderTaskFactory::new(ChildPriority::Normal, None)
        .prepare(handle.clone(), options, &shutdown)
        .expect("direct launch prepared");
    let mut attachment = factory
        .open(
            handle,
            OpenOptions {
                clock: SessionClock::default(),
                expected_generation: Some(Generation(1)),
                launch,
            },
            shutdown,
        )
        .await
        .expect("persistent provider starts");
    let child_pid = pid(&attachment);
    tokio::time::timeout(PROVIDER_EXIT_HANG_GUARD, async {
        let frame = Frame::Request(RequestFrame {
            id: RequestId::Number(7),
            method: Method::ModelList,
            params: None,
            fields: Default::default(),
        });
        let (reply, written) = oneshot::channel();
        let (_observation, observed) = oneshot::channel();
        attachment
            .commands
            .send(AdapterCommand::Write {
                write: PreparedWrite::new(OperationId("held-write".into()), &frame).expect("frame"),
                reply,
                observed,
            })
            .await
            .expect("write admitted");
        assert!(matches!(
            written.await.expect("retained receipt"),
            CommandReceipt::Written { .. }
        ));
        while attachment.journal.len() < LANE_CAPACITY {
            tokio::task::yield_now().await;
        }
        let owned_pid = rustix::process::Pid::from_raw(child_pid as i32).expect("owned pid");
        rustix::process::kill_process(owned_pid, rustix::process::Signal::KILL)
            .expect("kill fixture");
        // Do not consume the journal or acknowledge the write. Only the owner
        // observing wait() can close status and finish this attachment.
        assert!(attachment.status.changed().await.is_err());
        let exit = attachment.completion().await;
        assert!(matches!(
            exit.result,
            Err(AdapterError::Supervisor(
                studio_runtime_process::SupervisorError::ConnectionLost
            ))
        ));
        assert!(exit.cleanup_error.is_none());
        assert_reaped(child_pid);
        let known = attachment
            .journal
            .recv()
            .await
            .expect("known reply retained");
        assert!(matches!(
            known.payload,
            JournalPayload::Frame(Frame::Response {
                id: RequestId::Number(7),
                body: ResponseBody::Result(_),
                ..
            })
        ));
    })
    .await
    .expect("exit independent of full lanes");
}

async fn child(script: &str) -> ProviderAttachment {
    StdioProviderTaskFactory {
        deadline: Some(Instant::now() + Duration::from_secs(30)),
        max_frame_bytes: 1024,
        max_output_bytes: Some(4096),
        stderr_log: None,
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

#[tokio::test]
async fn provider_stderr_is_written_to_the_state_directory_app_server_log() {
    use studio_runtime_process::operational_log::OperationalLog;

    let root = tempfile::tempdir().expect("isolated state directory");
    let log = OperationalLog::open(root.path().join("app-server.log"), 1024, 4, 16)
        .expect("operational log");
    let mut attachment = StdioProviderTaskFactory {
        deadline: Some(Instant::now() + Duration::from_secs(30)),
        max_frame_bytes: 1024,
        max_output_bytes: Some(4096),
        stderr_log: Some(log.clone()),
    }
    .open(
        ProviderHandle::for_account("stdio-log-test"),
        OpenOptions {
            clock: SessionClock::default(),
            expected_generation: Some(Generation(1)),
            launch: ProviderLaunchSpec::new(
                vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    r#"printf 'provider diagnostic\n' >&2; printf '\377\n' >&2"#.into(),
                ],
                BTreeMap::new(),
                root.path().to_path_buf(),
            ),
        },
        Shutdown::default(),
    )
    .await
    .expect("child starts");
    let completion =
        tokio::time::timeout(PROVIDER_STDERR_COMPLETION_TIMEOUT, attachment.completion())
            .await
            .expect("provider completion hang guard");
    assert!(completion.cleanup_error.is_none());
    assert_eq!(
        std::fs::read(root.path().join("app-server.log")).expect("app-server log"),
        b"provider diagnostic\n\xff\n"
    );
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
    tokio::time::timeout(Duration::from_secs(10), attachment.journal.recv())
        .await
        .expect("journal deadline")
        .expect("child ready");
    assert!(attachment.shutdown().await.result.is_ok());
    assert_reaped(pid);
}

#[tokio::test]
async fn deadline_stops_and_reaps_child() {
    let mut attachment = child(WAIT).await;
    let pid = pid(&attachment);
    tokio::time::timeout(Duration::from_secs(10), attachment.journal.recv())
        .await
        .expect("journal deadline")
        .expect("child ready");
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
    tokio::time::timeout(Duration::from_secs(10), attachment.journal.recv())
        .await
        .expect("journal deadline")
        .expect("child ready");
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
    // The shell reads exactly one byte, announces progress, then waits on
    // a timer rather than consuming the rest of stdin.
    let mut attachment = child(r#"printf '{"method":"pipe-ready"}\n'; dd bs=1 count=1 of=/dev/null 2>/dev/null; printf '{"method":"stdin-progress"}\n'; exec sleep infinity"#).await;
    let pid = pid(&attachment);
    tokio::time::timeout(Duration::from_secs(10), attachment.journal.recv())
        .await
        .expect("journal deadline")
        .expect("child ready");
    let stdin = std::fs::File::open(format!("/proc/{pid}/fd/0")).expect("owned child stdin");
    let capacity = rustix::pipe::fcntl_getpipe_size(&stdin).expect("pipe capacity");
    const BLOCKED_WRITE_OVERFLOW_BYTES: usize = 4096;
    let frame = Frame::Request(RequestFrame {
        id: RequestId::Number(1),
        method: Method::ModelList,
        params: Some(
            serde_json::json!({"padding":"x".repeat(capacity + BLOCKED_WRITE_OVERFLOW_BYTES)}),
        ),
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
    tokio::time::timeout(Duration::from_secs(10), attachment.journal.recv())
        .await
        .expect("stdin progress deadline")
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
    // Every descendant inherits the lifetime descriptor. EOF after cancellation proves that
    // the whole group exited, including the child announced by its parent.
    let root = tempfile::tempdir().expect("isolated group fixture");
    let code = r#"
        sleep infinity &
        descendant=$!
        printf '{"method":"ready","pid":%s,"group":%s,"descendant":%s}\n' "$$" "$$" "$descendant"
        wait "$descendant"
    "#;
    let factory = StdioProviderTaskFactory {
        deadline: Some(Instant::now() + Duration::from_secs(30)),
        max_frame_bytes: 1024,
        max_output_bytes: Some(4096),
        stderr_log: None,
    };
    // A FIFO carries the lifetime witness independently from the provider
    // journal, so EOF proves every inherited descriptor was closed.
    let fifo = root.path().join("lifetime");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &fifo,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )
    .expect("lifetime FIFO");
    let reader = rustix::fs::open(
        &fifo,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .expect("nonblocking lifetime reader");
    let mut lifetime =
        tokio::net::unix::pipe::Receiver::from_owned_fd(reader).expect("observe lifetime EOF");
    let script = format!("exec 3>\"{}\"; {code}", fifo.display());
    let mut attachment = factory
        .open(
            ProviderHandle::for_account("group-test"),
            OpenOptions {
                clock: SessionClock::default(),
                expected_generation: Some(Generation(1)),
                launch: ProviderLaunchSpec::new(
                    crate::process_support::shell_command(&script),
                    BTreeMap::new(),
                    root.path().to_path_buf(),
                ),
            },
            Shutdown::default(),
        )
        .await
        .expect("group starts");
    let parent = pid(&attachment);
    let mut descendant = None;
    while descendant.is_none() {
        let entry = tokio::time::timeout(Duration::from_secs(10), attachment.journal.recv())
            .await
            .expect("journal deadline")
            .expect("group ready");
        if let JournalPayload::Frame(studio_runtime_providers::Frame::UnknownMethod(frame)) =
            entry.payload
        {
            #[derive(serde::Deserialize)]
            enum ReadyMethod {
                #[serde(rename = "ready")]
                Ready,
            }
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct GroupReady {
                #[serde(rename = "method")]
                _method: ReadyMethod,
                pid: u32,
                group: u32,
                descendant: u32,
            }
            let ready: GroupReady =
                serde_json::from_value(serde_json::to_value(frame.fields).expect("ready fields"))
                    .expect("group ready frame");
            assert_eq!(ready.pid, parent);
            assert_eq!(ready.group, parent);
            descendant = Some(ready.descendant);
        }
    }
    assert!(crate::process_support::pid_exists(
        descendant.expect("descendant pid")
    ));
    assert!(attachment.shutdown().await.result.is_ok());
    assert_reaped(parent);
    let eof = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::io::AsyncReadExt::read(&mut lifetime, &mut [0_u8; 1])
            .await
            .expect("group EOF")
    })
    .await
    .expect("group exit deadline");
    assert_eq!(eof, 0);
}
