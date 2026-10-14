use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tempfile::TempDir;

use super::{
    Clock, CompatibilityProbe, DurableStep, FailPoints, NativeRuntime, NativeRuntimeError,
};
use crate::provider_binaries::{BinaryCandidate, CandidateStatus, DiscoveryInputs, NativePlatform};

const COMPANION: &str = "codex-code-mode-host";

struct FakeProbe;

impl CompatibilityProbe for FakeProbe {
    fn version(&self, executable: &Path) -> Result<String, NativeRuntimeError> {
        let bytes = fs::read(executable)?;
        Ok(String::from_utf8_lossy(&bytes)
            .strip_prefix("version=")
            .unwrap_or("1.2.3")
            .trim()
            .to_owned())
    }

    fn check(&self, bundle: &Path, _platform: NativePlatform) -> Result<Value, NativeRuntimeError> {
        if !bundle.join(COMPANION).is_file() {
            return Err(NativeRuntimeError::Incompatible);
        }
        Ok(json!({"schema": {"methods": ["initialize"]}, "smoke": true}))
    }
}

struct FixedClock;

impl Clock for FixedClock {
    fn unix_seconds(&self) -> Result<f64, NativeRuntimeError> {
        Ok(1234.5)
    }
}

struct OneShot {
    target: Mutex<Option<(DurableStep, bool)>>,
}

impl OneShot {
    fn new(step: DurableStep, after: bool) -> Self {
        Self {
            target: Mutex::new(Some((step, after))),
        }
    }

    fn hit(&self, step: DurableStep, after: bool) -> Result<(), NativeRuntimeError> {
        let mut target = self
            .target
            .lock()
            .map_err(|_| NativeRuntimeError::Poisoned)?;
        if *target == Some((step, after)) {
            *target = None;
            return Err(NativeRuntimeError::FailPoint("injected crash"));
        }
        Ok(())
    }
}

impl FailPoints for OneShot {
    fn before(&self, step: DurableStep) -> Result<(), NativeRuntimeError> {
        self.hit(step, false)
    }

    fn after(&self, step: DurableStep) -> Result<(), NativeRuntimeError> {
        self.hit(step, true)
    }
}

fn fixture() -> (TempDir, PathBuf, PathBuf) {
    let temp = TempDir::new().expect("temporary root");
    let source = temp.path().join("candidate");
    fs::create_dir(&source).expect("candidate directory");
    let binary = source.join("codex");
    fs::write(&binary, "version=1.2.3\n").expect("fake executable bytes");
    fs::write(source.join(COMPANION), "harmless companion\n").expect("fake companion bytes");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).expect("binary mode");
        fs::set_permissions(source.join(COMPANION), fs::Permissions::from_mode(0o700))
            .expect("companion mode");
    }
    let root = temp.path().join("state");
    (temp, root, binary)
}

fn runtime(root: &Path) -> NativeRuntime {
    NativeRuntime::with_ports(
        root,
        Arc::new(FakeProbe),
        Arc::new(FixedClock),
        Arc::new(super::NoFailPoints),
    )
}

fn candidate(runtime: &NativeRuntime, binary: &Path) -> BinaryCandidate {
    runtime
        .discover(
            DiscoveryInputs {
                pinned_path: Some(binary.to_owned()),
                ..DiscoveryInputs::default()
            },
            NativePlatform::LinuxX64,
        )
        .expect("candidate discovery")
        .into_iter()
        .next()
        .expect("candidate row")
}

#[test]
fn discovery_resolves_npm_package_launcher_to_native_binary() {
    let (_temp, root, _binary) = fixture();
    let runtime = runtime(&root);
    let package = root.join("node_modules/@openai/codex");
    let launcher = package.join("bin/codex.js");
    let native = package
        .join("vendor")
        .join(NativePlatform::LinuxX64.target_triple())
        .join("bin/codex");
    fs::create_dir_all(launcher.parent().expect("launcher directory"))
        .expect("create npm launcher directory");
    fs::create_dir_all(native.parent().expect("native binary directory"))
        .expect("create native binary directory");
    fs::write(package.join("package.json"), r#"{"name":"@openai/codex"}"#)
        .expect("write npm package metadata");
    fs::write(&launcher, "fixture launcher; never executed\n").expect("write launcher fixture");
    fs::write(&native, "version=1.2.3\n").expect("write native fixture");
    fs::write(
        native.with_file_name("codex-code-mode-host"),
        "fixture companion\n",
    )
    .expect("write native companion");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&native, fs::Permissions::from_mode(0o700))
            .expect("native fixture executable mode");
        fs::set_permissions(
            native.with_file_name("codex-code-mode-host"),
            fs::Permissions::from_mode(0o700),
        )
        .expect("native companion executable mode");
    }
    let found = runtime
        .discover(
            DiscoveryInputs {
                pinned_path: Some(launcher),
                ..DiscoveryInputs::default()
            },
            NativePlatform::LinuxX64,
        )
        .expect("discover npm native binary");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].status, CandidateStatus::Discovered);
    assert_eq!(
        found[0].path,
        native.canonicalize().expect("canonical binary")
    );
}

fn approve(runtime: &NativeRuntime, binary: &Path) -> Value {
    runtime
        .check(
            DiscoveryInputs {
                pinned_path: Some(binary.to_owned()),
                ..DiscoveryInputs::default()
            },
            NativePlatform::LinuxX64,
        )
        .expect("candidate check")
        .selected()
        .expect("approved candidate selected")
        .clone()
}

#[test]
fn discovery_approval_switch_and_release_keep_a_live_session_bundle() {
    let (_temp, root, binary) = fixture();
    let runtime = runtime(&root);
    let first = approve(&runtime, &binary);
    assert_eq!(first["validatedAt"], 1234.5);
    let lease = runtime.retain_selected().expect("retain first executable");
    let old_hash = lease.bundle_hash().to_owned();
    let old_path = lease.path().to_owned();

    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let session = std::thread::spawn(move || {
        ready_tx.send(()).expect("signal active session");
        release_rx.recv().expect("wait for session close");
        drop(lease);
    });
    ready_rx.recv().expect("concurrent session owns executable");

    fs::write(&binary, "version=2.0.0\n").expect("replace candidate bytes");
    let second = approve(&runtime, &binary);
    assert_ne!(second["bundleSha256"], old_hash);
    assert_eq!(
        runtime
            .read_state()
            .expect("state")
            .expect("state exists")
            .selected(),
        Some(&second)
    );
    assert!(matches!(
        runtime.release(&old_hash),
        Err(NativeRuntimeError::InUse)
    ));
    assert!(
        old_path.is_file(),
        "live session executable remains present"
    );

    release_tx.send(()).expect("allow session close");
    session
        .join()
        .expect("session dropped its executable lease");
    assert!(runtime.release(&old_hash).expect("release idle bundle"));
    assert!(!old_path.exists());
}

#[test]
fn reads_python_shaped_state_without_losing_rows_or_extensions() {
    let (_temp, root, _binary) = fixture();
    let path = root.join("native-runtime/status.json");
    fs::create_dir_all(path.parent().expect("status parent")).expect("status directory");
    let python_record = json!({
        "status": "ready",
        "checkedAt": 7.5,
        "selected": null,
        "candidates": [{"path": "/fixture/codex", "status": "rejected", "error": "bad"}],
        "accounts": {
            "default": {"status": "waiting", "version": null, "pid": null,
                "targetVersion": "1.2.3", "reason": "busy", "updatedAt": 4.0,
                "rotation": {"id": "op-a", "phase": "closing", "old": {"pid": 42}}}
        },
        "futurePythonField": {"preserve": [null, false, 8]}
    });
    fs::write(
        &path,
        serde_json::to_vec(&python_record).expect("Python record encoding"),
    )
    .expect("Python record");
    let state = runtime(&root)
        .read_state()
        .expect("Rust reads state")
        .expect("Python-written state exists");
    assert_eq!(state.record(), &python_record);
    assert_eq!(
        state.accounts().expect("Python accounts object")["default"]["rotation"]["old"]["pid"],
        42
    );
}

#[test]
fn release_rename_crash_is_recoverable_and_idempotent() {
    for after in [false, true] {
        let (_temp, root, binary) = fixture();
        let normal = runtime(&root);
        let approved = approve(&normal, &binary);
        let old_hash = approved["bundleSha256"]
            .as_str()
            .expect("approved bundle hash")
            .to_owned();
        let old_path = PathBuf::from(approved["path"].as_str().expect("approved path"));
        fs::write(&binary, "version=2.0.0\n").expect("new candidate");
        let newer = approve(&normal, &binary);
        normal.switch(&newer).expect("switch selected bundle");
        let crash = NativeRuntime::with_ports(
            &root,
            Arc::new(FakeProbe),
            Arc::new(FixedClock),
            Arc::new(OneShot::new(DurableStep::RetiredBundleRename, after)),
        );
        assert!(matches!(
            crash.release(&old_hash),
            Err(NativeRuntimeError::FailPoint(_))
        ));
        assert_eq!(old_path.exists(), !after);
        if after {
            assert_eq!(crash.recover_retired().expect("finish cleanup"), 1);
        } else {
            assert!(crash.release(&old_hash).expect("replay release"));
        }
        assert_eq!(crash.recover_retired().expect("replay cleanup"), 0);
    }
}

#[test]
fn retired_bundle_removal_crash_is_recoverable_and_idempotent() {
    for after in [false, true] {
        let (_temp, root, binary) = fixture();
        let normal = runtime(&root);
        let old = approve(&normal, &binary);
        let old_hash = old["bundleSha256"]
            .as_str()
            .expect("old bundle hash")
            .to_owned();
        fs::write(&binary, "version=2.0.0\n").expect("new candidate bytes");
        let newer = approve(&normal, &binary);
        normal.switch(&newer).expect("select new bundle");
        let interrupted = NativeRuntime::with_ports(
            &root,
            Arc::new(FakeProbe),
            Arc::new(FixedClock),
            Arc::new(OneShot::new(DurableStep::RetiredBundleRemoval, after)),
        );
        assert!(matches!(
            interrupted.release(&old_hash),
            Err(NativeRuntimeError::FailPoint(_))
        ));
        let recovered = interrupted
            .recover_retired()
            .expect("finish or confirm removal");
        assert_eq!(recovered, usize::from(!after));
        assert!(!interrupted.release(&old_hash).expect("release replay"));
    }
}

#[test]
fn status_write_and_rename_fail_points_reopen_and_replay_safely() {
    for (step, after, expected_new) in [
        (DurableStep::StatusRecordWrite, false, false),
        (DurableStep::StatusRecordWrite, true, false),
        (DurableStep::StatusRecordRename, false, false),
        (DurableStep::StatusRecordRename, true, true),
    ] {
        let (_temp, root, binary) = fixture();
        let normal = runtime(&root);
        let approved = approve(&normal, &binary);
        let prior = normal
            .read_state()
            .expect("prior state read")
            .expect("prior state exists")
            .record()
            .clone();
        let crash = NativeRuntime::with_ports(
            &root,
            Arc::new(FakeProbe),
            Arc::new(FixedClock),
            Arc::new(OneShot::new(step, after)),
        );
        assert!(matches!(
            crash.switch(&approved),
            Err(NativeRuntimeError::FailPoint(_))
        ));
        let reopened = runtime(&root)
            .read_state()
            .expect("restart state read")
            .expect("restart state exists");
        if expected_new {
            assert_eq!(reopened.selected(), Some(&approved));
        } else {
            assert_eq!(reopened.record(), &prior);
        }
        runtime(&root).switch(&approved).expect("safe replay");
        assert_eq!(
            runtime(&root)
                .read_state()
                .expect("replayed state read")
                .expect("replayed state exists")
                .selected(),
            Some(&approved)
        );
    }
}

#[test]
fn bundle_rename_fail_points_replay_the_same_approval() {
    for after in [false, true] {
        let (_temp, root, binary) = fixture();
        let crash = NativeRuntime::with_ports(
            &root,
            Arc::new(FakeProbe),
            Arc::new(FixedClock),
            Arc::new(OneShot::new(DurableStep::BundleDirectoryRename, after)),
        );
        let found = candidate(&crash, &binary);
        assert!(matches!(
            crash.approve(&found, NativePlatform::LinuxX64),
            Err(NativeRuntimeError::FailPoint(_))
        ));
        let retry = runtime(&root);
        let approved = approve(&retry, &binary);
        assert!(Path::new(approved["path"].as_str().expect("approved path")).is_file());
        assert_eq!(
            retry
                .read_state()
                .expect("retry state read")
                .expect("retry state exists")
                .selected(),
            Some(&approved)
        );
    }
}

#[test]
fn bundle_staging_write_fail_points_replay_the_same_approval() {
    for after in [false, true] {
        let (_temp, root, binary) = fixture();
        let interrupted = NativeRuntime::with_ports(
            &root,
            Arc::new(FakeProbe),
            Arc::new(FixedClock),
            Arc::new(OneShot::new(DurableStep::BundleStagingWrite, after)),
        );
        let found = candidate(&interrupted, &binary);
        assert!(matches!(
            interrupted.approve(&found, NativePlatform::LinuxX64),
            Err(NativeRuntimeError::FailPoint(_))
        ));
        let retry = runtime(&root);
        let approved = approve(&retry, &binary);
        assert!(Path::new(approved["path"].as_str().expect("approved path")).is_file());
        assert_eq!(
            retry
                .read_state()
                .expect("retry status read")
                .expect("retry status exists")
                .selected(),
            Some(&approved)
        );
    }
}
