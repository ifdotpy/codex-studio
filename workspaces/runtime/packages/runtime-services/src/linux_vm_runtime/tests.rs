#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// What the fake helper does once it is launched.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Boot {
    /// The guest provisions, answers and has a synchronized clock.
    Healthy,
    /// The helper process exits before its control socket.
    Exits,
    /// The kernel of the new boot panics.
    Panics,
}

#[derive(Default)]
struct Guest {
    calls: Mutex<Vec<(String, Value)>>,
    /// The `layr.health` answer; `None` is a ready layr.
    layr: Mutex<Option<Value>>,
}

/// Serve the fake guest of `state` on its `guest.sock`.
fn serve_guest(state: &Path, guest: Arc<Guest>) {
    drop(fs::remove_file(state.join("guest.sock")));
    serve(state.join("guest.sock"), move |method, params| {
        guest
            .calls
            .lock()
            .unwrap()
            .push((method.to_owned(), params.clone()));
        match method {
            "health" => json!({"result": {"service": "ok"}}),
            "exec" => json!({"result": {"exitCode": 0}}),
            "layr.health" => json!({"result": guest.layr.lock().unwrap().clone()
                .unwrap_or_else(|| json!({"state": "ready"}))}),
            "share.configure" => json!({"result": {"configured": true}}),
            "project.ensure" => {
                json!({"result": {"projectId": params["projectId"], "state": "ready"}})
            }
            "share.status" => json!({"result": {"state": "ready"}}),
            "share.name" => json!({"result": {"name": params["name"]}}),
            _ => json!({"error": {"code": "invalid_params", "message": "Unknown method"}}),
        }
    });
}

struct FakeLauncher {
    boot: Mutex<Boot>,
    launches: AtomicUsize,
    /// `host.stop` requests of every helper.
    stops: Arc<AtomicUsize>,
    guest: Arc<Guest>,
}

/// The port of the share bridge that the fake helper reports.
const BRIDGE_PORT: u64 = 50_123;

struct FakeHelper {
    exits: bool,
}

impl RunningHelper for FakeHelper {
    fn exit(&mut self) -> Option<String> {
        self.exits.then(|| "exit status: 1".to_owned())
    }

    fn detach(self: Box<Self>) {}
}

/// Answer each request line of `socket` with `answer(method, params)`.
fn serve(socket: PathBuf, answer: impl Fn(&str, &Value) -> Value + Send + 'static) {
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let request: Value = serde_json::from_str(&line).unwrap();
            let mut response = answer(request["method"].as_str().unwrap(), &request["params"]);
            response["id"] = request["id"].clone();
            let mut stream = stream;
            drop(writeln!(stream, "{response}"));
        }
    });
}

impl HelperLauncher for FakeLauncher {
    fn launch(
        &self,
        _helper: &Path,
        state: &Path,
        _log: fs::File,
    ) -> io::Result<Box<dyn RunningHelper>> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        let boot = *self.boot.lock().unwrap();
        if boot == Boot::Exits {
            return Ok(Box::new(FakeHelper { exits: true }));
        }
        let console = if boot == Boot::Panics {
            "[    3.140000] Kernel panic - not syncing: corrupted stack end detected inside scheduler\n"
        } else {
            "STUDIO_PROVISION_STAGE: codex\nSTUDIO_PROVISION_READY\n"
        };
        fs::write(state.join(CONSOLE_FILE), console).unwrap();
        // `host.stop` ends this helper: its sockets go away, so the VM is stopped.
        let stops = Arc::clone(&self.stops);
        let folder = state.to_path_buf();
        serve(state.join(CONTROL_SOCKET), move |method, _| match method {
            "host.status" => json!({"result": {"state": "running", "pid": 4242,
                "share": {"state": "listening", "port": BRIDGE_PORT}}}),
            "host.stop" => {
                stops.fetch_add(1, Ordering::SeqCst);
                drop(fs::remove_file(folder.join(CONTROL_SOCKET)));
                drop(fs::remove_file(folder.join("guest.sock")));
                json!({"result": {"state": "stopping"}})
            }
            _ => json!({"error": {"code": "invalid_params", "message": "unknown"}}),
        });
        serve_guest(state, Arc::clone(&self.guest));
        Ok(Box::new(FakeHelper { exits: false }))
    }
}

/// The Mac mount table and `mount_smbfs` of the Finder views.
#[derive(Default)]
struct FakeMounts {
    table: Mutex<Vec<crate::linux_vm_share::MountEntry>>,
    /// Source, folder and password of each mount.
    mounted: Mutex<Vec<(String, PathBuf, String)>>,
    failure: Mutex<Option<String>>,
}

impl ShareMountTool for FakeMounts {
    fn entries(&self) -> Result<Vec<crate::linux_vm_share::MountEntry>, String> {
        Ok(self.table.lock().unwrap().clone())
    }

    fn mount(&self, source: &str, destination: &Path, password: &str) -> Result<(), String> {
        if let Some(failure) = self.failure.lock().unwrap().clone() {
            return Err(format!("The authenticated SMB mount failed: {failure}"));
        }
        self.mounted.lock().unwrap().push((
            source.to_owned(),
            destination.to_path_buf(),
            password.to_owned(),
        ));
        self.table
            .lock()
            .unwrap()
            .push(crate::linux_vm_share::MountEntry {
                source: source.to_owned(),
                path: destination.to_string_lossy().into_owned(),
                options: ["smbfs", "nodev", "nosuid", "read-only"]
                    .map(str::to_owned)
                    .to_vec(),
            });
        Ok(())
    }

    fn unmount(&self, path: &Path) -> Result<(), String> {
        self.table
            .lock()
            .unwrap()
            .retain(|entry| Path::new(&entry.path) != path);
        Ok(())
    }
}

struct EnoughSpace;

impl GuestSpaceProbe for EnoughSpace {
    fn free_bytes(&self, _path: &Path) -> io::Result<u64> {
        Ok(u64::MAX)
    }
}

/// The Mac booted 500 seconds ago.
fn recent_boot() -> Option<f64> {
    Some(now() - 500.0)
}

struct Fixture {
    _temp: tempfile::TempDir,
    state: PathBuf,
    studio: PathBuf,
    launcher: Arc<FakeLauncher>,
    shares: Arc<FakeMounts>,
    runtime: Arc<VmRuntime>,
}

impl Fixture {
    fn new(boot: Boot) -> Self {
        // The platform temporary folder keeps the socket paths below the 104-byte limit.
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("s");
        fs::create_dir(&state).unwrap();
        fs::write(state.join(CONFIG_FILE), "{}").unwrap();
        let helper = temp.path().join("studio-linux-vm");
        fs::write(&helper, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
        let launcher = Arc::new(FakeLauncher {
            boot: Mutex::new(boot),
            launches: AtomicUsize::new(0),
            stops: Arc::default(),
            guest: Arc::default(),
        });
        let studio = temp.path().join("Studio");
        let shares = Arc::new(FakeMounts::default());
        let runtime = VmRuntime::with_space(
            VmRuntimeParts {
                control: Arc::new(crate::linux_vm_guest::UnixVmControlRpc::new(&state)),
                guest: Arc::new(crate::linux_vm_guest::UnixGuestRpc::new(&state)),
                launcher: Arc::clone(&launcher) as Arc<dyn HelperLauncher>,
                boot_time: recent_boot,
                supported: true,
                studio_root: Some(studio.clone()),
                shares: Arc::clone(&shares) as Arc<dyn ShareMountTool>,
                helper,
                state_dir: state.clone(),
            },
            Arc::new(EnoughSpace),
        );
        Self {
            _temp: temp,
            state,
            studio,
            launcher,
            shares,
            runtime,
        }
    }

    fn intent(&self, value: Value) {
        fs::write(self.state.join(INTENT_FILE), value.to_string()).unwrap();
    }

    fn json(&self, name: &str) -> Value {
        serde_json::from_str(&fs::read_to_string(self.state.join(name)).unwrap()).unwrap()
    }

    fn launches(&self) -> usize {
        self.launcher.launches.load(Ordering::SeqCst)
    }

    fn stops(&self) -> usize {
        self.launcher.stops.load(Ordering::SeqCst)
    }

    /// What the next launched helper boots.
    fn next_boot(&self, boot: Boot) {
        *self.launcher.boot.lock().unwrap() = boot;
    }

    fn guest_calls(&self, method: &str) -> Vec<Value> {
        self.launcher
            .guest
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.0 == method)
            .map(|call| call.1.clone())
            .collect()
    }

    /// A running helper whose guest kernel panicked.
    async fn panicked(boot: Boot) -> Self {
        let fixture = Self::new(Boot::Panics);
        fixture.runtime.start().await.unwrap_err();
        assert_eq!(fixture.runtime.status().unwrap()["state"], "failed");
        fixture.next_boot(boot);
        fixture
    }
}

#[tokio::test]
async fn a_person_starts_a_created_vm_and_waits_for_a_guest_with_a_synchronized_clock() {
    let fixture = Fixture::new(Boot::Healthy);
    let status = fixture.runtime.start().await.unwrap();
    assert_eq!(
        (status["state"].clone(), status["health"]["service"].clone()),
        (json!("running"), json!("ok"))
    );
    assert_eq!(fixture.launches(), 1);
    let intent = fixture.json(INTENT_FILE);
    assert_eq!(
        (intent["state"].clone(), intent["by"].clone()),
        (json!("running"), json!("start"))
    );
    assert!(intent.get("restart").is_none());
    let port = fixture.json(SHARE_BRIDGE_FILE)["port"].as_u64().unwrap();
    assert!((1024..=65535).contains(&port));
    let health = fixture.json(HEALTH_FILE);
    assert_eq!(health["helperPid"], 4242);
    assert!(health["healthyAt"].is_f64());
    let calls = fixture.launcher.guest.calls.lock().unwrap().clone();
    let clock = calls
        .iter()
        .find(|(method, _)| method == "exec")
        .expect("the clock check runs in the guest");
    assert!(clock.1["argv"].to_string().contains("NTPSynchronized"));
    // A running VM is not started again.
    assert_eq!(fixture.runtime.resume().await.unwrap(), "running");
    fixture.runtime.ensure_for_chat().await.unwrap();
    assert_eq!(fixture.launches(), 1);
}

#[tokio::test]
async fn a_vm_that_ran_before_this_mac_boot_starts_again_with_a_record() {
    let fixture = Fixture::new(Boot::Healthy);
    fixture.intent(json!({"state": "running", "by": "start", "at": now() - 1000.0}));
    // A VM chat needs the guest: the Mac restarted under the VM.
    fixture.runtime.ensure_for_chat().await.unwrap();
    assert_eq!(fixture.launches(), 1);
    let record = fixture.json(RESTARTS_FILE)["restarts"][0].clone();
    assert_eq!(
        (
            record["kind"].clone(),
            record["outcome"].clone(),
            record["stage"].clone()
        ),
        (json!("host_restart"), json!("restarted"), json!("done"))
    );
    let intent = fixture.json(INTENT_FILE);
    assert_eq!(
        (intent["state"].clone(), intent["by"].clone()),
        (json!("running"), json!("watchdog"))
    );
    assert!(intent.get("restart").is_none(), "{intent}");
}

#[tokio::test]
async fn a_vm_stopped_in_this_boot_or_on_purpose_stays_stopped() {
    for intent in [
        json!({"state": "running", "by": "start", "at": now() - 100.0}),
        json!({"state": "stopped", "by": "stop", "at": now() - 1000.0}),
    ] {
        let fixture = Fixture::new(Boot::Healthy);
        fixture.intent(intent.clone());
        assert_eq!(
            fixture.runtime.resume().await.unwrap(),
            "stopped",
            "{intent}"
        );
        let refused = fixture.runtime.ensure_for_chat().await.unwrap_err();
        assert!(refused.to_string().contains("linux-vm start"), "{refused}");
        assert_eq!(fixture.launches(), 0);
        assert_eq!(fixture.json(INTENT_FILE), intent);
    }
    // Without a created VM nothing starts.
    let fixture = Fixture::new(Boot::Healthy);
    fs::remove_file(fixture.state.join(CONFIG_FILE)).unwrap();
    fixture.intent(json!({"state": "running", "by": "start", "at": now() - 1000.0}));
    assert_eq!(fixture.runtime.resume().await.unwrap(), "absent");
    assert_eq!(fixture.launches(), 0);
}

#[tokio::test]
async fn the_restart_limits_and_the_maintenance_lock_hold_an_automatic_start() {
    let fixture = Fixture::new(Boot::Healthy);
    fixture.intent(json!({"state": "running", "by": "start", "at": now() - 1000.0}));
    let restarts: Vec<Value> = (1..=4)
        .map(|hour| json!({"id": hour.to_string(), "at": now() - 3600.0 * f64::from(hour)}))
        .collect();
    fs::write(
        fixture.state.join(RESTARTS_FILE),
        json!({"restarts": restarts}).to_string(),
    )
    .unwrap();
    assert_eq!(fixture.runtime.resume().await.unwrap(), "limited");
    assert_eq!(
        fixture.json(RESTARTS_FILE)["held"]["rule"],
        "4 automatic restarts in 24 hours"
    );
    fs::write(
        fixture.state.join(RESTARTS_FILE),
        json!({"restarts": []}).to_string(),
    )
    .unwrap();
    let maintenance = lock_file(&fixture.state.join(MAINTENANCE_LOCK)).unwrap();
    rustix::fs::flock(&maintenance, rustix::fs::FlockOperation::LockExclusive).unwrap();
    assert_eq!(fixture.runtime.resume().await.unwrap(), "busy");
    drop(maintenance);
    assert_eq!(fixture.launches(), 0);
    assert_eq!(fixture.runtime.resume().await.unwrap(), "restarted");
}

#[tokio::test]
async fn a_helper_exit_and_a_kernel_panic_fail_the_start() {
    let fixture = Fixture::new(Boot::Exits);
    let error = fixture.runtime.start().await.unwrap_err();
    assert!(
        matches!(&error, VmStartError::Failed(message) if message.contains(HELPER_LOG)),
        "{error}"
    );
    let fixture = Fixture::new(Boot::Panics);
    let error = fixture.runtime.start().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("The Linux VM failed: Kernel panic - not syncing"),
        "{error}"
    );
    assert_eq!(
        fixture.runtime.status().unwrap()["guestFailure"]["kind"],
        "kernel_panic"
    );
    // An automatic restart records its failure and keeps the intent without the restart.
    let fixture = Fixture::new(Boot::Exits);
    fixture.intent(json!({"state": "running", "by": "start", "at": now() - 1000.0}));
    assert_eq!(fixture.runtime.resume().await.unwrap(), "failed");
    let record = fixture.json(RESTARTS_FILE)["restarts"][0].clone();
    assert_eq!(
        (record["outcome"].clone(), record["stage"].clone()),
        (json!("failed"), json!("failed"))
    );
    assert!(fixture.json(INTENT_FILE).get("restart").is_none());
}

#[tokio::test]
async fn an_interrupted_automatic_restart_finishes_or_is_abandoned() {
    for (age, outcome) in [(60.0, "restarted"), (4000.0, "abandoned")] {
        let fixture = Fixture::new(Boot::Healthy);
        let record =
            json!({"id": "r1", "at": now() - age, "kind": "host_restart", "stage": "starting"});
        fs::write(
            fixture.state.join(RESTARTS_FILE),
            json!({"restarts": [record]}).to_string(),
        )
        .unwrap();
        fixture.intent(
            json!({"state": "running", "by": "watchdog", "at": now() - age, "restart": "r1"}),
        );
        assert_eq!(fixture.runtime.resume().await.unwrap(), outcome);
        assert_eq!(
            fixture.json(RESTARTS_FILE)["restarts"][0]["outcome"],
            outcome
        );
        assert!(fixture.json(INTENT_FILE).get("restart").is_none());
        assert_eq!(fixture.launches(), usize::from(outcome == "restarted"));
    }
    // A stop on purpose since the restart began cancels its start.
    let fixture = Fixture::new(Boot::Healthy);
    let error = on_thread({
        let runtime = Arc::clone(&fixture.runtime);
        move || runtime.start_blocking(RESTART_SECONDS, "watchdog", Some("other"))
    })
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error, VmStartError::StoppedOnPurpose);
    assert_eq!(fixture.launches(), 0);
}

/// Python `Watchdog.check`, which 027f3cfea deleted: a kernel panic behind a
/// live helper is a failed VM. The watchdog stops it, keeps the console of the
/// failed boot, and starts it through the normal start.
#[tokio::test]
async fn the_watchdog_restarts_a_failed_guest_and_keeps_its_console() {
    let fixture = Fixture::panicked(Boot::Healthy).await;
    assert_eq!(fixture.runtime.check().await.unwrap(), "restarted");
    assert_eq!((fixture.launches(), fixture.stops()), (2, 1));
    let record = fixture.json(RESTARTS_FILE)["restarts"][0].clone();
    assert_eq!(
        (
            record["kind"].clone(),
            record["outcome"].clone(),
            record["stage"].clone(),
            record["helperPid"].clone()
        ),
        (
            json!("kernel_panic"),
            json!("restarted"),
            json!("done"),
            json!(4242)
        )
    );
    assert_eq!(
        record["reason"],
        "Kernel panic - not syncing: corrupted stack end detected inside scheduler"
    );
    assert!(
        record["consoleTail"]
            .as_str()
            .unwrap()
            .contains("corrupted stack end")
    );
    let copy = fixture.state.join(record["consoleLog"].as_str().unwrap());
    assert!(fs::read_to_string(&copy).unwrap().contains("Kernel panic"));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // The new boot has its own console.
    assert!(
        !fs::read_to_string(fixture.state.join(CONSOLE_FILE))
            .unwrap()
            .contains("Kernel panic")
    );
    let intent = fixture.json(INTENT_FILE);
    assert_eq!(
        (intent["state"].clone(), intent["by"].clone()),
        (json!("running"), json!("watchdog"))
    );
    assert!(intent.get("restart").is_none(), "{intent}");
    assert_eq!(fixture.runtime.status().unwrap()["state"], "running");
    // The next check leaves the running VM alone.
    assert_eq!(fixture.runtime.check().await.unwrap(), "running");
    assert_eq!(fixture.launches(), 2);
}

/// The restart limits, `maintenance.lock` and the client lock hold the
/// restart of a failed guest before its stop.
#[tokio::test]
async fn the_restart_of_a_failed_guest_waits_for_the_limits_and_the_locks() {
    let fixture = Fixture::panicked(Boot::Healthy).await;
    fs::write(
        fixture.state.join(RESTARTS_FILE),
        json!({"restarts": [{"id": "r0", "at": now() - 300.0}]}).to_string(),
    )
    .unwrap();
    assert_eq!(fixture.runtime.check().await.unwrap(), "limited");
    let held = fixture.json(RESTARTS_FILE)["held"].clone();
    assert_eq!(held["rule"], "one automatic restart in 10 minutes");
    assert!(held["reason"].as_str().unwrap().starts_with("Kernel panic"));
    fs::write(
        fixture.state.join(RESTARTS_FILE),
        json!({"restarts": []}).to_string(),
    )
    .unwrap();
    for name in [MAINTENANCE_LOCK, CLIENT_LOCK] {
        let lock = lock_file(&fixture.state.join(name)).unwrap();
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive).unwrap();
        assert_eq!(fixture.runtime.check().await.unwrap(), "busy", "{name}");
    }
    assert_eq!((fixture.launches(), fixture.stops()), (1, 0));
    assert_eq!(fixture.json(RESTARTS_FILE)["restarts"], json!([]));
    assert_eq!(fixture.runtime.check().await.unwrap(), "restarted");
}

/// A restart whose start a backend stop interrupted after the launch is
/// closed when the guest answers, so a later stop of the guest keeps the VM
/// stopped.
#[tokio::test]
async fn an_answering_guest_closes_an_interrupted_restart() {
    let fixture = Fixture::new(Boot::Healthy);
    fixture.runtime.start().await.unwrap();
    fs::write(
        fixture.state.join(RESTARTS_FILE),
        json!({"restarts": [{"id": "r1", "at": now() - 60.0, "kind": "kernel_panic", "stage": "starting"}]})
            .to_string(),
    )
    .unwrap();
    fixture
        .intent(json!({"state": "running", "by": "watchdog", "at": now() - 60.0, "restart": "r1"}));
    assert_eq!(fixture.runtime.check().await.unwrap(), "running");
    let record = fixture.json(RESTARTS_FILE)["restarts"][0].clone();
    assert_eq!(
        (record["outcome"].clone(), record["stage"].clone()),
        (json!("restarted"), json!("done"))
    );
    assert!(fixture.json(INTENT_FILE).get("restart").is_none());
    assert_eq!(fixture.launches(), 1);
}

/// Python `remount_registered` at the end of `ensure_running`: each start
/// mounts the registered Finder views again with the private SMB password.
#[tokio::test]
async fn each_start_mounts_the_registered_project_views_again() {
    let fixture = Fixture::new(Boot::Healthy);
    let project = fixture.state.join("layr-projects/p1");
    fs::create_dir_all(&project).unwrap();
    let view = fixture.studio.join("app");
    fs::write(
        project.join("mount.json"),
        json!({"projectId": "p1", "name": "app", "path": view, "port": 1}).to_string(),
    )
    .unwrap();
    fixture.runtime.start().await.unwrap();
    let credential = fixture.json("smb-credential.json");
    let password = credential["password"].as_str().unwrap().to_owned();
    assert_eq!(password.len(), 64);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(fixture.state.join("smb-credential.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fixture.shares.mounted.lock().unwrap().clone(),
        vec![(
            format!("//studio-view@127.0.0.1:{BRIDGE_PORT}/app"),
            view.clone(),
            password.clone()
        )]
    );
    assert_eq!(
        fixture.guest_calls("share.configure"),
        vec![json!({"password": password})]
    );
    assert_eq!(
        fixture.guest_calls("project.ensure"),
        vec![json!({"projectId": "p1"})]
    );
    assert_eq!(
        fixture.guest_calls("share.name"),
        vec![json!({"projectId": "p1", "name": "app"})]
    );
    let saved: Value =
        serde_json::from_str(&fs::read_to_string(project.join("mount.json")).unwrap()).unwrap();
    assert_eq!(
        saved,
        json!({"projectId": "p1", "name": "app", "path": view, "port": BRIDGE_PORT})
    );
    assert!(!project.join("mount-error.json").exists());

    // A failed view keeps its reason; the VM keeps running.
    fixture.shares.table.lock().unwrap().clear();
    *fixture.shares.failure.lock().unwrap() = Some("server rejected the connection".into());
    fixture.runtime.start().await.unwrap();
    let report: Value =
        serde_json::from_str(&fs::read_to_string(project.join("mount-error.json")).unwrap())
            .unwrap();
    assert_eq!(report["projectId"], "p1");
    assert!(
        report["error"]
            .as_str()
            .unwrap()
            .contains("server rejected the connection"),
        "{report}"
    );
    assert_eq!(fixture.launches(), 1);
}

/// Python `Client.stop`, which 027f3cfea deleted: a person's stop ends the
/// helper and saves its intent, so no automatic start follows.
#[tokio::test]
async fn a_stop_on_purpose_keeps_the_vm_stopped() {
    let fixture = Fixture::new(Boot::Healthy);
    fixture.runtime.start().await.unwrap();
    let stopped = fixture.runtime.stop().await.unwrap();
    assert_eq!(Value::Object(stopped), json!({"state": "stopped"}));
    assert_eq!(fixture.stops(), 1);
    assert_eq!(fixture.runtime.status().unwrap()["state"], "stopped");
    let intent = fixture.json(INTENT_FILE);
    assert_eq!(
        (intent["state"].clone(), intent["by"].clone()),
        (json!("stopped"), json!("stop"))
    );
    assert_eq!(fixture.runtime.check().await.unwrap(), "stopped");
    assert!(fixture.runtime.ensure_for_chat().await.is_err());
    assert_eq!(fixture.launches(), 1);

    // A stop of a stopped VM ends a pending automatic restart.
    fixture.intent(
        json!({"state": "running", "by": "watchdog", "at": now() - 1000.0, "restart": "r1"}),
    );
    fixture.runtime.stop().await.unwrap();
    let intent = fixture.json(INTENT_FILE);
    assert_eq!(intent["state"], "stopped");
    assert!(intent.get("restart").is_none(), "{intent}");
    assert_eq!(fixture.stops(), 1);
    assert_eq!(fixture.runtime.check().await.unwrap(), "stopped");
    assert_eq!(fixture.launches(), 1);
}

/// Python `Client._guest_failure` and `Watchdog._unresponsive`, which
/// 027f3cfea deleted: a guest that answered in this boot and then gave no
/// answer for 180 seconds over 3 probes is unresponsive. It is shown and
/// recorded as an episode, never restarted, and one answer ends the episode.
#[tokio::test]
async fn an_unanswered_guest_is_shown_and_recorded_but_never_restarted() {
    let fixture = Fixture::new(Boot::Healthy);
    fixture.runtime.start().await.unwrap();
    // The guest answered in this boot, then two probes failed over 200 seconds.
    let mut health = fixture.json(HEALTH_FILE);
    for (field, value) in [
        ("failingSince", json!(now() - 200.0)),
        ("lastFailureAt", json!(now() - 30.0)),
        ("lastProbeAt", json!(now() - 30.0)),
        ("failures", json!(2)),
    ] {
        health[field] = value;
    }
    fs::write(fixture.state.join(HEALTH_FILE), health.to_string()).unwrap();
    fs::remove_file(fixture.state.join("guest.sock")).unwrap();
    assert_eq!(fixture.runtime.check().await.unwrap(), "unresponsive");
    let status = fixture.runtime.status().unwrap();
    assert_eq!(
        (
            status["state"].clone(),
            status["helperState"].clone(),
            status["guestFailure"]["kind"].clone()
        ),
        (json!("unresponsive"), json!("running"), json!("unanswered"))
    );
    assert!(
        status["error"]
            .as_str()
            .unwrap()
            .starts_with("The guest has not answered for")
    );
    // A second check keeps the one open episode.
    assert_eq!(fixture.runtime.check().await.unwrap(), "unresponsive");
    let episodes = fixture.json(RESTARTS_FILE)["unresponsive"].clone();
    assert_eq!(episodes.as_array().unwrap().len(), 1, "{episodes}");
    assert_eq!(episodes[0]["helperPid"], 4242);
    assert!(episodes[0].get("endedAt").is_none());
    assert_eq!(
        fixture.runtime.status().unwrap()["watchdog"]["lastUnresponsive"]["helperPid"],
        4242
    );
    assert_eq!((fixture.launches(), fixture.stops()), (1, 0));

    // One answer ends the episode.
    serve_guest(&fixture.state, Arc::clone(&fixture.launcher.guest));
    assert_eq!(fixture.runtime.check().await.unwrap(), "running");
    assert!(fixture.json(RESTARTS_FILE)["unresponsive"][0]["endedAt"].is_f64());
    assert_eq!(fixture.json(HEALTH_FILE)["failures"], 0);
    assert_eq!(fixture.runtime.status().unwrap()["state"], "running");
}

/// Python `Watchdog.probe_layr` and `layr_fault`: the guest answers while
/// LAYR does not. After 3 failed `layr.health` answers over 120 seconds the
/// status shows the fault; the VM keeps running and is not restarted.
#[tokio::test]
async fn a_layr_outage_behind_an_answering_guest_is_shown_not_restarted() {
    let fixture = Fixture::new(Boot::Healthy);
    fixture.runtime.start().await.unwrap();
    *fixture.launcher.guest.layr.lock().unwrap() = Some(json!({"state": "unavailable"}));
    let mut health = fixture.json(HEALTH_FILE);
    health["layr"] = json!({"failingSince": now() - 150.0, "lastFailureAt": now() - 70.0,
        "lastProbeAt": now() - 70.0, "failures": 2, "error": "layr state unavailable"});
    fs::write(fixture.state.join(HEALTH_FILE), health.to_string()).unwrap();
    assert_eq!(fixture.runtime.check().await.unwrap(), "running");
    let status = fixture.runtime.status().unwrap();
    assert_eq!(status["state"], "running");
    assert_eq!(status["layrFault"]["kind"], "layr_unavailable");
    assert!(
        status["error"]
            .as_str()
            .unwrap()
            .starts_with("LAYR in the VM has not answered for"),
        "{status:?}"
    );
    // At most one layr probe in 60 seconds.
    assert_eq!(fixture.runtime.check().await.unwrap(), "running");
    assert_eq!(fixture.guest_calls("layr.health").len(), 1);
    assert_eq!((fixture.launches(), fixture.stops()), (1, 0));
}

/// The chats of a fake runtime store.
#[derive(Default)]
struct FakeNotices {
    chats: Vec<String>,
    posted: Mutex<Vec<Map<String, Value>>>,
}

impl VmChatNotices for FakeNotices {
    fn chats(&self) -> futures_util::future::BoxFuture<'_, Result<Vec<String>, String>> {
        Box::pin(async move { Ok(self.chats.clone()) })
    }

    fn post(
        &self,
        record: Map<String, Value>,
    ) -> futures_util::future::BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            self.posted.lock().unwrap().push(record);
            Ok(())
        })
    }
}

/// Python `drop_providers` and `post_notices`: each VM chat with a live turn
/// in the dead boot shows how the automatic restart ended, once.
#[tokio::test]
async fn a_restart_of_a_failed_guest_tells_each_chat_with_a_live_turn_once() {
    let fixture = Fixture::panicked(Boot::Healthy).await;
    let notices = Arc::new(FakeNotices {
        chats: vec!["chat-1".into()],
        ..FakeNotices::default()
    });
    fixture
        .runtime
        .attach_chat_notices(Arc::clone(&notices) as Arc<dyn VmChatNotices>);
    assert_eq!(fixture.runtime.check().await.unwrap(), "restarted");
    let posted = notices.posted.lock().unwrap().clone();
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0]["chats"], json!(["chat-1"]));
    assert_eq!(posted[0]["outcome"], "restarted");
    let text = notices::notice_text(&posted[0]);
    assert!(
        text.starts_with(
            "The Linux VM stopped working (Kernel panic - not syncing: corrupted stack end \
             detected inside scheduler). Studio restarted it at "
        ),
        "{text}"
    );
    assert!(text.ends_with("review it before you continue."), "{text}");
    assert_eq!(fixture.json(RESTARTS_FILE)["restarts"][0]["noticed"], true);
    let summary = fixture.runtime.status().unwrap()["watchdog"].clone();
    assert_eq!(
        (
            summary["restartsInLastDay"].clone(),
            summary["lastRestart"]["outcome"].clone()
        ),
        (json!(1), json!("restarted"))
    );
    assert_eq!(fixture.runtime.check().await.unwrap(), "running");
    assert_eq!(notices.posted.lock().unwrap().len(), 1);
}

/// The store names the VM chats with a live turn and writes each notice in
/// its transcript; a deleted chat gets none.
#[tokio::test]
async fn the_store_names_the_vm_chats_and_shows_each_restart_notice() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        studio_runtime_store::RuntimeStore::open(Some(directory.path()))
            .await
            .unwrap(),
    );
    let agent = |id: &str, mode: &str, status: &str, deleted: bool| {
        let mut record = json!({"id": id, "rootId": id, "epoch": 1, "status": status,
            "isLead": true, "name": id, "provider": "codex", "accountKey": "default",
            "executionMode": mode, "cwd": "/line"});
        if deleted {
            record["deletedAt"] = json!(10.0);
        }
        record
    };
    let records = [
        agent("vm-running", "vm", "running", false),
        agent("vm-idle", "vm", "completed", false),
        agent("host-running", "native", "running", false),
        agent("vm-deleted", "vm", "running", true),
    ];
    store
        .transaction_with(move |tx| {
            for record in records {
                studio_runtime_store::AgentRepository::put(
                    tx,
                    &studio_runtime_store::AgentRecord::from_value(record).unwrap(),
                )?;
            }
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .unwrap();
    let notices = StoreChatNotices::new(Arc::clone(&store));
    assert_eq!(notices.chats().await.unwrap(), ["vm-running"]);
    let record = json!({"id": "r1", "outcome": "cancelled", "reason": "Kernel panic - not syncing",
        "at": 100.0, "finishedAt": 160.0, "chats": ["vm-running", "vm-deleted", "missing"]});
    let record = record.as_object().unwrap().clone();
    notices.post(record.clone()).await.unwrap();
    let items: Vec<(String, String)> = store
        .read_async("test", |tx| {
            let mut statement = tx.prepare(
                "SELECT agent, json_extract(record,'$.text') FROM runtime_items ORDER BY agent",
            )?;
            let rows = statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<_, studio_runtime_store::StoreError>(rows)
        })
        .await
        .unwrap();
    assert_eq!(
        items,
        [(
            "vm-running".to_owned(),
            "The Linux VM stopped working (Kernel panic - not syncing). It was stopped on \
             purpose, so Studio did not start it again."
                .to_owned()
        )]
    );
}

#[test]
fn the_console_tells_a_kernel_death_and_the_provisioning_state() {
    assert_eq!(
        kernel_death("[    1.000000] Kernel panic - not syncing: VFS\n").map(|death| death.0),
        Some("kernel_panic")
    );
    assert_eq!(
        kernel_death("watchdog: BUG: soft lockup - CPU#0 stuck\n"),
        None
    );
    assert_eq!(
        kernel_death("traps: node[1] general protection fault ip:1\n"),
        None
    );
    assert_eq!(
        kernel_death("BUG: unable to handle page fault\n").map(|death| death.0),
        Some("kernel_error")
    );
    assert_eq!(
        provision_from_log("STUDIO_PROVISION_READY\n")["state"],
        "ready"
    );
    assert_eq!(
        provision_from_log("STUDIO_PROVISION_STAGE: codex\n")["state"],
        "running"
    );
    let failed = provision_from_log(
        "STUDIO_PROVISION_READY\nSTUDIO_PROVISION_ERROR: codex: download failed\n",
    );
    assert_eq!(
        (failed["state"].clone(), failed["cause"].clone()),
        (json!("failed"), json!("codex: download failed"))
    );
}

/// `desktop_vm.py` owns the name of the maintenance lock that the installer checks.
#[test]
fn the_maintenance_lock_name_matches_its_desktop_owner() {
    let owner = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../client/apps/desktop/desktop_vm.py"),
    )
    .unwrap();
    assert!(
        owner.contains(&format!("MAINTENANCE_LOCK = '{MAINTENANCE_LOCK}'")),
        "{owner}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn the_host_boot_time_is_known_on_macos() {
    let boot = host_boot_time().expect("kern.boottime");
    assert!(boot > 1_500_000_000.0 && boot < now(), "{boot}");
}
