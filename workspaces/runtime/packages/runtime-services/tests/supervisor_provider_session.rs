#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;
use studio_runtime_process::{
    BackendId, BackendIdentity, HandleRequest, OperationId, ProviderHandle, ProviderLaunchSpec,
    ProviderProcessFactory, SupervisorClient, SupervisorHandle, SupervisorProviderProcessFactory,
    WriteRequest,
};
use studio_runtime_providers::{Frame, Inbound, Method};
use studio_runtime_services::provider_session::{
    ProviderSession, ProviderSessionConfig, ProviderSessionEvent,
};

const SOCKET: &str = "supervisor.sock";
const SOURCE: &str = env!("CARGO_MANIFEST_DIR");
const SUPERVISOR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../apps/server/src/codex_process_supervisor.py"
);

struct DisposableSupervisor {
    root: PathBuf,
    child: Child,
    proxy: Option<Child>,
    owned_children: Vec<u32>,
}

impl DisposableSupervisor {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("sp-{}-{unique}", std::process::id()));
        assert_runtime_state_is_not_used(&root);
        assert!(root.join(SOCKET).as_os_str().as_bytes().len() < 108);
        fs::create_dir_all(&root).unwrap();
        let stderr = File::create(root.join("supervisor.log")).unwrap();
        let child = Command::new(python_executable())
            .arg(SUPERVISOR)
            .arg("--state")
            .arg(&root)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("XDG_CACHE_HOME", root.join("cache"))
            .current_dir(SOURCE)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("start disposable supervisor with checked-out Python server");
        let mut harness = Self {
            root,
            child,
            proxy: None,
            owned_children: Vec::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if harness.root.join(SOCKET).exists()
                && let Ok(mut client) =
                    SupervisorClient::connect(harness.root.join(SOCKET), harness.identity())
                && client.health(Default::default()).is_ok()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "supervisor socket startup timed out"
            );
            assert!(
                harness.child.try_wait().unwrap().is_none(),
                "supervisor exited early"
            );
            thread::yield_now();
        }
        harness
    }

    fn install_drop_proxy(&mut self, mode: &str) {
        fs::rename(
            self.root.join(SOCKET),
            self.root.join("supervisor.actual.sock"),
        )
        .unwrap();
        let proxy_script = self.root.join("drop_proxy.py");
        fs::write(
            &proxy_script,
            r#"import json, os, socket, sys, threading
root, mode = sys.argv[1:]
listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
listener.bind(os.path.join(root, 'supervisor.sock'))
listener.listen()
dropped = False
lock = threading.Lock()
def frame(sock):
 data = bytearray()
 while not data.endswith(b'\n'):
  part = sock.recv(65536)
  if not part: return None
  data.extend(part)
 return bytes(data)
def handle(client):
 global dropped
 upstream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
 try:
  upstream.connect(os.path.join(root, 'supervisor.actual.sock'))
  handshake = frame(client)
  if handshake is None: return
  upstream.sendall(handshake)
  answer = frame(upstream)
  if answer is None: return
  client.sendall(answer)
  while True:
   request = frame(client)
   if request is None: return
   value = json.loads(request)
   upstream.sendall(request)
   response = frame(upstream)
   if response is None: return
   action = value.get('action')
   method = value.get('message', {}).get('method')
   if action == 'next' and json.loads(response).get('result', {}).get('event') is not None:
    with open(os.path.join(root, 'proxy-actions.log'), 'a', encoding='utf-8') as audit:
     audit.write('next-event\n')
   with open(os.path.join(root, 'proxy-actions.log'), 'a', encoding='utf-8') as audit:
    audit.write(str(action) + ' ' + str(method) + '\n')
   should_drop = (mode == 'ack' and action == 'ack' and int(value.get('sequence', 0)) >= 1)
   should_drop = should_drop or (mode == 'write' and action == 'write' and method == 'model/list')
   should_drop = should_drop or (mode == 'status' and action == 'status')
   if should_drop:
    with lock:
     if not dropped:
      dropped = True
      return
   client.sendall(response)
 finally:
  client.close()
  upstream.close()
while True:
 client, _ = listener.accept()
 threading.Thread(target=handle, args=(client,), daemon=True).start()
"#,
        )
        .unwrap();
        let proxy = Command::new(python_executable())
            .arg(&proxy_script)
            .arg(&self.root)
            .arg(mode)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                File::create(self.root.join("drop-proxy.log")).unwrap(),
            ))
            .spawn()
            .expect("start disposable supervisor response proxy");
        self.proxy = Some(proxy);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if self.root.join(SOCKET).exists()
                && let Ok(mut client) =
                    SupervisorClient::connect(self.root.join(SOCKET), self.identity())
                && client.health(Default::default()).is_ok()
            {
                break;
            }
            if let Some(status) = self
                .proxy
                .as_mut()
                .and_then(|proxy| proxy.try_wait().unwrap())
            {
                let log = fs::read_to_string(self.root.join("drop-proxy.log")).unwrap_or_default();
                panic!("drop proxy exited before becoming ready: {status}; {log}");
            }
            assert!(
                Instant::now() < deadline,
                "drop proxy socket startup timed out: supervisor log: {}; proxy log: {}",
                fs::read_to_string(self.root.join("supervisor.log")).unwrap_or_default(),
                fs::read_to_string(self.root.join("drop-proxy.log")).unwrap_or_default()
            );
            thread::yield_now();
        }
    }

    fn identity(&self) -> BackendIdentity {
        BackendIdentity {
            backend_id: BackendId(format!("provider-session-test-{}", std::process::id())),
            state_dir: self.root.clone(),
        }
    }

    fn client(&self) -> SupervisorClient<std::os::unix::net::UnixStream> {
        SupervisorClient::connect(self.root.join(SOCKET), self.identity()).unwrap()
    }

    fn own_child(&mut self, pid: u32) {
        self.owned_children.push(pid);
    }
}

impl Drop for DisposableSupervisor {
    fn drop(&mut self) {
        if let Some(proxy) = self.proxy.as_mut()
            && proxy.try_wait().unwrap().is_none()
        {
            let _ignored = proxy.kill();
            let _ignored = proxy.wait();
        }
        for pid in &self.owned_children {
            let _ignored = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
        if self.child.try_wait().unwrap().is_none() {
            let _ignored = self.child.kill();
        }
        let _ignored = self.child.wait();
        let _ignored = fs::remove_dir_all(&self.root);
    }
}

fn assert_runtime_state_is_not_used(path: &Path) {
    let configured = std::env::var_os("CODEX_AGENTS_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".local/state/codex-agents"))
        });
    if let Some(configured) = configured {
        assert_ne!(path, configured);
        assert!(!path.starts_with(configured));
    }
}

fn python_executable() -> String {
    let output = Command::new("python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .expect("python3 must be available for supervisor integration tests");
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn launch_spec(workspace: &Path, audit: &Path, burst: bool) -> ProviderLaunchSpec {
    let script = if burst {
        r#"import json, sys
for line in sys.stdin:
 m=json.loads(line); method=m.get('method')
 if method=='initialize': print(json.dumps({'id':m['id'],'result':{'ok':True}}),flush=True)
 elif method=='burst':
  for n in range(300): print(json.dumps({'method':'test/burst','params':{'n':n}}),flush=True)
  print(json.dumps({'id':m['id'],'result':{'ok':True}}),flush=True)
 elif 'id' in m: print(json.dumps({'id':m['id'],'result':{'ok':True}}),flush=True)
"#
    } else {
        r#"import json, os, sys
for line in sys.stdin:
 m=json.loads(line); method=m.get('method')
 with open(os.environ['AUDIT_PATH'],'a',encoding='utf-8') as f: f.write(str(method)+'\n')
 if method=='initialize':
  print(json.dumps({'method':'test/duringInitialize','params':{'ready':False}}),flush=True)
  print(json.dumps({'id':m['id'],'result':{'ok':True}}),flush=True)
 elif method=='initialized' and 'INITIALIZED_BARRIER' in os.environ:
  with open(os.environ['INITIALIZED_BARRIER'],'w',encoding='utf-8') as f: f.write('x')
 elif method=='trigger': print(json.dumps({'method':'test/backlog','params':{'ready':True}}),flush=True)
 elif 'id' in m: print(json.dumps({'id':m['id'],'result':{'method':method}}),flush=True)
"#
    };
    let python = python_executable();
    let environment = BTreeMap::from([
        ("PYTHONUNBUFFERED".into(), "1".into()),
        ("AUDIT_PATH".into(), audit.to_string_lossy().into_owned()),
        ("PATH".into(), "/usr/bin".into()),
        ("HOME".into(), workspace.to_string_lossy().into_owned()),
        (
            "CODEX_HOME".into(),
            workspace.join("codex-home").to_string_lossy().into_owned(),
        ),
        (
            "CLAUDE_CONFIG_DIR".into(),
            workspace
                .join("claude-config")
                .to_string_lossy()
                .into_owned(),
        ),
    ]);
    ProviderLaunchSpec::new(
        vec![python, "-u".into(), "-c".into(), script.into()],
        environment,
        workspace.to_path_buf(),
    )
}

fn uncertain_write_spec(workspace: &Path, audit: &Path) -> ProviderLaunchSpec {
    let script = r#"import json, os, sys
for line in sys.stdin:
 m=json.loads(line); method=m.get('method')
 if method=='initialize': print(json.dumps({'id':m['id'],'result':{'ok':True}}),flush=True)
 elif method=='model/list':
  with open(os.environ['AUDIT_PATH'],'a',encoding='utf-8') as f: f.write(method+'\n')
  print(json.dumps({'id':m['id'],'result':{'ok':True}}),flush=True)
"#;
    ProviderLaunchSpec::new(
        vec![python_executable(), "-u".into(), "-c".into(), script.into()],
        BTreeMap::from([
            ("PYTHONUNBUFFERED".into(), "1".into()),
            ("AUDIT_PATH".into(), audit.to_string_lossy().into_owned()),
            ("PATH".into(), "/usr/bin".into()),
            ("HOME".into(), workspace.to_string_lossy().into_owned()),
            (
                "CODEX_HOME".into(),
                workspace.join("codex-home").to_string_lossy().into_owned(),
            ),
            (
                "CLAUDE_CONFIG_DIR".into(),
                workspace
                    .join("claude-config")
                    .to_string_lossy()
                    .into_owned(),
            ),
        ]),
        workspace.to_path_buf(),
    )
}

fn factory(
    harness: &DisposableSupervisor,
    handle: ProviderHandle,
    spec: ProviderLaunchSpec,
) -> SupervisorProviderProcessFactory {
    let spec = Arc::new(Mutex::new(spec));
    let resolver_spec = Arc::clone(&spec);
    let expected = handle.clone();
    SupervisorProviderProcessFactory::new(harness.identity(), move |actual: &ProviderHandle| {
        if actual != &expected {
            return Err("unexpected provider handle".into());
        }
        Ok(resolver_spec
            .lock()
            .map_err(|_| "launch resolver poisoned")?
            .clone())
    })
}

fn connect(factory: &impl ProviderProcessFactory, handle: &ProviderHandle) -> ProviderSession {
    connect_with_config(factory, handle, ProviderSessionConfig::new())
}

fn connect_with_config(
    factory: &impl ProviderProcessFactory,
    handle: &ProviderHandle,
    mut config: ProviderSessionConfig,
) -> ProviderSession {
    let process = factory.start_or_reattach(handle).unwrap();
    config.request_timeout_ms = 5_000;
    ProviderSession::connect(process, config).unwrap()
}

fn await_notification(session: &mut ProviderSession, expected: &str) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let (event, cursor) = session.receive_event().unwrap();
        match event {
            ProviderSessionEvent::Inbound(Inbound::Notification(Frame::UnknownMethod(note)))
                if note.method == expected =>
            {
                return cursor.expect("supervisor frame must include its journal cursor");
            }
            ProviderSessionEvent::OrphanReply { .. } => {
                if let Some(cursor) = cursor {
                    session.acknowledge_handled(cursor).unwrap();
                }
            }
            _ => {}
        }
    }
    panic!("notification {expected} was not delivered");
}

#[test]
#[cfg(target_os = "linux")]
fn provider_session_defers_initialization_notifications_and_reattaches_backlog() {
    let mut harness = DisposableSupervisor::new();
    let workspace = harness.root.join("provider-workspace");
    fs::create_dir_all(&workspace).unwrap();
    let audit = workspace.join("methods.log");
    let initialized_barrier = workspace.join("initialized.fifo");
    assert!(
        Command::new("mkfifo")
            .arg(&initialized_barrier)
            .status()
            .unwrap()
            .success()
    );
    let handle = ProviderHandle::for_account("session-test");
    let mut spec = launch_spec(&workspace, &audit, false);
    spec.environment.insert(
        "INITIALIZED_BARRIER".into(),
        initialized_barrier.to_string_lossy().into_owned(),
    );
    let launch = Arc::new(Mutex::new(spec.clone()));
    let resolver_launch = Arc::clone(&launch);
    let expected = handle.clone();
    let factory = SupervisorProviderProcessFactory::new(
        harness.identity(),
        move |actual: &ProviderHandle| {
            if actual != &expected {
                return Err("unexpected provider handle".into());
            }
            Ok(resolver_launch
                .lock()
                .map_err(|_| "launch resolver poisoned")?
                .clone())
        },
    );

    let mut first = connect(&factory, &handle);
    let cursor = await_notification(&mut first, "test/duringInitialize");
    assert!(
        first.handled_cursor() < cursor,
        "consumer event cannot be acked before settlement"
    );
    // Runtime state committed this notification, but its ACK has not reached
    // the supervisor when the process connection is lost.
    launch.lock().unwrap().handled_cursor = cursor;
    // `connect` becomes ready before its asynchronous writer flushes
    // `initialized`. Wait for the child to process that write before injecting
    // the trigger, so the audit sequence has an observable barrier.
    let mut initialized_signal = File::open(&initialized_barrier).unwrap();
    let mut signal = [0];
    std::io::Read::read_exact(&mut initialized_signal, &mut signal).unwrap();
    let mut client = harness.client();
    let status = client
        .status(HandleRequest {
            handle: SupervisorHandle(handle.as_str().to_owned()),
            extra: BTreeMap::new(),
        })
        .unwrap();
    harness.own_child(status.pid);
    first.close().unwrap();

    client
        .write(WriteRequest {
            handle: SupervisorHandle(handle.as_str().to_owned()),
            operation_id: OperationId("provider-test-trigger".into()),
            native_id: None,
            message: json!({"method":"trigger"}),
            extra: BTreeMap::new(),
        })
        .unwrap();
    // Launcher-only environment drift is normalized from the retained child.
    {
        let mut launch = launch.lock().unwrap();
        launch
            .environment
            .insert("PATH".into(), "/changed/path".into());
        launch
            .environment
            .insert("CODEX_AGENTS_BACKEND_ID".into(), "new-backend".into());
    }
    let resumed = factory.start_or_reattach(&handle).unwrap();
    assert!(
        resumed.acknowledged_cursor() >= cursor,
        "persisted handled cursor must be installed before the reader starts"
    );
    let mut config = ProviderSessionConfig::new();
    config.request_timeout_ms = 5_000;
    let mut second = ProviderSession::connect(resumed, config).unwrap();
    assert_eq!(second.generation(), 1);
    let backlog_cursor = await_notification(&mut second, "test/backlog");
    second.acknowledge_handled(backlog_cursor).unwrap();
    assert!(second.handled_cursor() >= backlog_cursor);
    second.close().unwrap();

    let third = connect(&factory, &handle);
    assert_eq!(third.generation(), 1);
    third.close().unwrap();

    let methods = fs::read_to_string(audit).unwrap();
    assert_eq!(
        methods.lines().collect::<Vec<_>>(),
        ["initialize", "initialized", "trigger"]
    );
}

#[test]
fn stderr_and_exit_journal_events_wait_for_consumer_acknowledgement() {
    let harness = DisposableSupervisor::new();
    let workspace = harness.root.join("provider-workspace");
    fs::create_dir_all(&workspace).unwrap();
    let handle = ProviderHandle::for_account("stderr-exit-settlement");
    let script = r#"import json,sys
for line in sys.stdin:
 m=json.loads(line); method=m.get('method')
 if method=='initialize': print(json.dumps({'id':m['id'],'result':{'ok':True}}),flush=True)
 elif method=='initialized':
  print('diagnostic',file=sys.stderr,flush=True)
  sys.exit(7)
"#;
    let factory = factory(
        &harness,
        handle.clone(),
        ProviderLaunchSpec::new(
            vec![python_executable(), "-u".into(), "-c".into(), script.into()],
            BTreeMap::from([
                ("PATH".into(), "/usr/bin".into()),
                ("HOME".into(), workspace.to_string_lossy().into_owned()),
                (
                    "CODEX_HOME".into(),
                    workspace.join("codex-home").to_string_lossy().into_owned(),
                ),
                (
                    "CLAUDE_CONFIG_DIR".into(),
                    workspace
                        .join("claude-config")
                        .to_string_lossy()
                        .into_owned(),
                ),
            ]),
            workspace,
        ),
    );
    let mut session = connect(&factory, &handle);
    let (stderr, stderr_cursor) = session.receive_event().unwrap();
    let stderr_cursor = stderr_cursor.unwrap();
    assert!(matches!(
        stderr,
        ProviderSessionEvent::Inbound(Inbound::Notification(Frame::UnknownMethod(ref frame)))
            if frame.method == "codex/supervisorEvent"
                && frame.fields["params"]["kind"] == "stderr"
    ));
    let (exit, exit_cursor) = session.receive_event().unwrap();
    let exit_cursor = exit_cursor.unwrap();
    assert!(matches!(
        exit,
        ProviderSessionEvent::Inbound(Inbound::Notification(Frame::UnknownMethod(ref frame)))
            if frame.method == "codex/supervisorEvent"
                && frame.fields["params"]["kind"] == "exit"
    ));
    assert_eq!(session.handled_cursor(), 1);
    session.acknowledge_handled(exit_cursor).unwrap();
    assert_eq!(
        session.handled_cursor(),
        1,
        "cursor 3 cannot skip stderr cursor 2"
    );
    session.acknowledge_handled(stderr_cursor).unwrap();
    assert_eq!(session.handled_cursor(), exit_cursor);
    session.close().unwrap();
}

#[test]
#[ignore = "thread-based session: detach before the initialized write completes (review 4, A6); replaced by readiness_requires_real_initialized_receipt_and_stable_identity"]
fn claude_initialize_reuses_cached_child_and_deduplicates_initialized() {
    let mut harness = DisposableSupervisor::new();
    let workspace = harness.root.join("provider-workspace");
    fs::create_dir_all(&workspace).unwrap();
    let audit = workspace.join("claude-methods.log");
    let handle = ProviderHandle::for_account("claude-reattach");
    let factory = factory(
        &harness,
        handle.clone(),
        launch_spec(&workspace, &audit, false),
    );
    let mut config = ProviderSessionConfig::new();
    config.claude = true;
    let first = connect_with_config(&factory, &handle, config);
    let status = harness
        .client()
        .status(HandleRequest {
            handle: SupervisorHandle(handle.as_str().to_owned()),
            extra: BTreeMap::new(),
        })
        .unwrap();
    harness.own_child(status.pid);
    first.close().unwrap();
    let mut config = ProviderSessionConfig::new();
    config.claude = true;
    let second = connect_with_config(&factory, &handle, config);
    assert_eq!(second.generation(), 1);
    second.close().unwrap();
    assert_eq!(
        fs::read_to_string(audit)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["initialize", "initialized"]
    );
}

#[test]
fn resumed_child_is_refused_when_status_cannot_be_verified() {
    let mut harness = DisposableSupervisor::new();
    let workspace = harness.root.join("provider-workspace");
    fs::create_dir_all(&workspace).unwrap();
    let handle = ProviderHandle::for_account("unverifiable-status");
    let launch = launch_spec(&workspace, &workspace.join("unused"), false);
    let factory = factory(&harness, handle.clone(), launch);
    let first = connect(&factory, &handle);
    let status = harness
        .client()
        .status(HandleRequest {
            handle: SupervisorHandle(handle.as_str().to_owned()),
            extra: BTreeMap::new(),
        })
        .unwrap();
    harness.own_child(status.pid);
    first.close().unwrap();
    harness.install_drop_proxy("status");
    assert!(matches!(
        factory.start_or_reattach(&handle),
        Err(studio_runtime_process::ProviderFactoryError::Launch)
    ));
}

#[test]
#[cfg(not(target_os = "linux"))]
fn retained_child_environment_change_fails_closed() {
    let mut harness = DisposableSupervisor::new();
    let workspace = harness.root.join("provider-workspace");
    fs::create_dir_all(&workspace).unwrap();
    let handle = ProviderHandle::for_account("non-linux-environment-change");
    let launch = Arc::new(Mutex::new(launch_spec(
        &workspace,
        &workspace.join("unused"),
        false,
    )));
    let resolver_launch = Arc::clone(&launch);
    let expected = handle.clone();
    let factory = SupervisorProviderProcessFactory::new(
        harness.identity(),
        move |actual: &ProviderHandle| {
            if actual != &expected {
                return Err("unexpected provider handle".into());
            }
            Ok(resolver_launch
                .lock()
                .map_err(|_| "launch resolver poisoned")?
                .clone())
        },
    );
    let mut first = connect(&factory, &handle);
    let status = harness
        .client()
        .status(HandleRequest {
            handle: SupervisorHandle(handle.as_str().to_owned()),
            extra: BTreeMap::new(),
        })
        .unwrap();
    harness.own_child(status.pid);
    first.close().unwrap();
    launch
        .lock()
        .unwrap()
        .environment
        .insert("PATH".into(), "/changed/path".into());
    assert!(matches!(
        factory.start_or_reattach(&handle),
        Err(studio_runtime_process::ProviderFactoryError::Launch)
    ));
}

#[test]
fn stop_unblocks_a_provider_reader_blocked_by_a_full_output_channel() {
    let mut harness = DisposableSupervisor::new();
    harness.install_drop_proxy("none");
    let workspace = harness.root.join("provider-workspace");
    fs::create_dir_all(&workspace).unwrap();
    let handle = ProviderHandle::for_account("blocked-reader");
    let factory = factory(
        &harness,
        handle.clone(),
        launch_spec(&workspace, &workspace.join("unused"), true),
    );
    let mut process = factory.start_or_reattach(&handle).unwrap();
    let (_reader, mut writer) = process.split().unwrap();
    writer
        .write_all(b"{\"id\":1,\"method\":\"initialize\"}\n")
        .unwrap();
    writer.flush().unwrap();
    writer
        .write_all(b"{\"id\":2,\"method\":\"burst\"}\n")
        .unwrap();
    writer.flush().unwrap();
    let mut client = harness.client();
    let status = client
        .status(HandleRequest {
            handle: SupervisorHandle(handle.as_str().to_owned()),
            extra: BTreeMap::new(),
        })
        .unwrap();
    let pid = status.pid;
    harness.own_child(pid);
    // The proxy sees the 65th event query only after the adapter has queued the
    // previous 64 events into its 64-slot output queue. That is the observable
    // barrier proving the next output is blocked by full capacity.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let actions =
            fs::read_to_string(harness.root.join("proxy-actions.log")).unwrap_or_default();
        if actions.lines().filter(|line| *line == "next-event").count() >= 65 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "adapter did not reach its full output queue barrier"
        );
        thread::yield_now();
    }
    let started = Instant::now();
    process.stop().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "stop left a blocked reader behind"
    );
    assert!(
        writer.write_all(b"{}\n").is_err(),
        "stop must unblock and reject writers"
    );
    let other_backend = BackendIdentity {
        backend_id: BackendId("provider-stop-lease-check".into()),
        state_dir: harness.root.clone(),
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut lease_released = false;
    while Instant::now() < deadline {
        if let Ok(mut client) = SupervisorClient::connect(&harness.root, other_backend.clone())
            && client
                .status(HandleRequest {
                    handle: SupervisorHandle(handle.as_str().to_owned()),
                    extra: BTreeMap::new(),
                })
                .is_ok()
        {
            lease_released = true;
            break;
        }
        thread::yield_now();
    }
    assert!(
        lease_released,
        "stop must release the supervisor backend lease"
    );
}

#[test]
fn stop_fences_reader_from_reconnecting_after_cancellation() {
    let mut harness = DisposableSupervisor::new();
    let workspace = harness.root.join("provider-workspace");
    fs::create_dir_all(&workspace).unwrap();
    let handle = ProviderHandle::for_account("stop-reconnect-fence");
    let factory = factory(
        &harness,
        handle.clone(),
        launch_spec(&workspace, &workspace.join("unused"), false),
    );
    let mut process = factory.start_or_reattach(&handle).unwrap();
    let mut client = harness.client();
    let status = client
        .status(HandleRequest {
            handle: SupervisorHandle(handle.as_str().to_owned()),
            extra: BTreeMap::new(),
        })
        .unwrap();
    harness.own_child(status.pid);
    harness.install_drop_proxy("none");
    process.stop().unwrap();
    let actions = fs::read_to_string(harness.root.join("proxy-actions.log")).unwrap_or_default();
    assert!(
        !actions.lines().any(|line| line.starts_with("status ")),
        "cancelled reader must not establish a replacement connection"
    );
}

#[test]
fn raw_writer_does_not_retry_after_an_unknown_write() {
    let mut harness = DisposableSupervisor::new();
    harness.install_drop_proxy("write");
    let workspace = harness.root.join("provider-workspace");
    fs::create_dir_all(&workspace).unwrap();
    let audit = workspace.join("raw-writer-methods.log");
    let handle = ProviderHandle::for_account("raw-writer-poison");
    let factory = factory(
        &harness,
        handle.clone(),
        uncertain_write_spec(&workspace, &audit),
    );
    let mut process = factory.start_or_reattach(&handle).unwrap();
    let (mut reader, mut writer) = process.split().unwrap();
    writer
        .write_all(b"{\"id\":1,\"method\":\"initialize\"}\n")
        .unwrap();
    writer.flush().unwrap();
    let mut initialize_reply = Vec::new();
    let mut chunk = [0_u8; 1024];
    while !initialize_reply.contains(&b'\n') {
        let count = reader.read(&mut chunk).unwrap();
        assert_ne!(count, 0, "initialize reply must arrive");
        initialize_reply.extend_from_slice(&chunk[..count]);
    }
    writer
        .write_all(b"{\"id\":2,\"method\":\"model/list\"}\n")
        .unwrap();
    assert!(writer.flush().is_err(), "first receipt is unknown");
    assert!(writer.flush().is_err(), "poisoned writer cannot resend");
    assert!(
        writer
            .write_all(b"{\"id\":2,\"method\":\"model/list\"}\n")
            .is_err()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs::metadata(&audit).map_or(true, |metadata| metadata.len() == 0) {
        assert!(
            Instant::now() < deadline,
            "provider did not receive the write"
        );
        thread::yield_now();
    }
    assert_eq!(fs::read_to_string(audit).unwrap().lines().count(), 1);
    let actions = fs::read_to_string(harness.root.join("proxy-actions.log")).unwrap();
    assert_eq!(
        actions
            .lines()
            .filter(|line| *line == "write model/list")
            .count(),
        1
    );
    process.stop().unwrap();
}

#[test]
fn lost_ack_reply_is_resolved_by_reattach_status() {
    let mut harness = DisposableSupervisor::new();
    harness.install_drop_proxy("ack");
    let workspace = harness.root.join("provider-workspace");
    fs::create_dir_all(&workspace).unwrap();
    let handle = ProviderHandle::for_account("ack-drop");
    let factory = factory(
        &harness,
        handle.clone(),
        launch_spec(&workspace, &workspace.join("unused"), false),
    );
    let mut session = connect(&factory, &handle);
    let cursor = await_notification(&mut session, "test/duringInitialize");
    session
        .acknowledge_handled(cursor)
        .expect("status after reconnect confirms the ACK committed");
    assert!(session.handled_cursor() >= cursor);
    session.close().unwrap();
    let actions = fs::read_to_string(harness.root.join("proxy-actions.log")).unwrap();
    assert_eq!(
        actions
            .lines()
            .filter(|line| line.starts_with("ack "))
            .count(),
        1
    );
}

#[test]
fn lost_provider_write_receipt_is_unknown_and_never_retried() {
    let mut harness = DisposableSupervisor::new();
    harness.install_drop_proxy("write");
    let workspace = harness.root.join("provider-workspace");
    fs::create_dir_all(&workspace).unwrap();
    let audit = workspace.join("provider-methods.log");
    let handle = ProviderHandle::for_account("write-drop");
    let factory = factory(
        &harness,
        handle.clone(),
        uncertain_write_spec(&workspace, &audit),
    );
    let mut session = connect(&factory, &handle);
    let ticket = session
        .send_request(Method::ModelList, json!({}), None, false, 5_000)
        .expect("request is submitted once");
    assert_eq!(
        session.receive_event().unwrap().0,
        ProviderSessionEvent::UnknownOutcome { ticket }
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs::metadata(&audit).map_or(true, |metadata| metadata.len() == 0) {
        assert!(
            Instant::now() < deadline,
            "provider did not receive the write"
        );
        thread::yield_now();
    }
    assert_eq!(fs::read_to_string(audit).unwrap().lines().count(), 1);
    let actions = fs::read_to_string(harness.root.join("proxy-actions.log")).unwrap();
    assert_eq!(
        actions
            .lines()
            .filter(|line| *line == "write model/list")
            .count(),
        1
    );
    assert!(
        !actions
            .lines()
            .any(|line| line.starts_with("operationStatus "))
    );
    let _ignored = session.close();
}
