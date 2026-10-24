use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tempfile::TempDir;

use super::{
    Clock, CompatibilityEvidence, CompatibilityProbe, CompatibilityRefusal, CompatibilityResult,
    DurableStep, FailPoints, NativeRuntime, NativeRuntimeError,
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

    fn check(
        &self,
        bundle: &Path,
        _platform: NativePlatform,
    ) -> Result<CompatibilityResult, NativeRuntimeError> {
        if !bundle.join(COMPANION).is_file() {
            return Ok(CompatibilityResult::Refused(
                CompatibilityRefusal::CompanionCheck,
            ));
        }
        Ok(CompatibilityResult::Approved(CompatibilityEvidence {
            schema: json!({"methods": ["initialize"]}),
            smoke: json!(true),
            companions: [(COMPANION.to_owned(), json!({"smoke": true}))]
                .into_iter()
                .collect(),
        }))
    }
}

struct RefusingProbe;

impl CompatibilityProbe for RefusingProbe {
    fn version(&self, _executable: &Path) -> Result<String, NativeRuntimeError> {
        Ok("1.2.3".to_owned())
    }

    fn check(
        &self,
        _bundle: &Path,
        _platform: NativePlatform,
    ) -> Result<CompatibilityResult, NativeRuntimeError> {
        Ok(CompatibilityResult::Refused(
            CompatibilityRefusal::ProtocolSchema,
        ))
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
    // The store refuses a root under any link, such as the macOS `/var`.
    let base = studio_runtime_store::path_identity::resolve(temp.path()).expect("resolved root");
    let source = base.join("candidate");
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
    let root = base.join("state");
    (temp, root, binary)
}

fn runtime(root: &Path) -> NativeRuntime {
    NativeRuntime::with_ports(
        root,
        Arc::new(FakeProbe),
        Arc::new(FixedClock),
        Arc::new(super::NoFailPoints),
    )
    .expect("exclusive runtime instance")
}

#[test]
fn state_root_lock_is_exclusive_until_last_clone_drops() {
    let (_temp, root, _binary) = fixture();
    let owner = runtime(&root);
    let clone = owner.clone();
    assert!(matches!(
        NativeRuntime::new(&root, Arc::new(FakeProbe)),
        Err(NativeRuntimeError::StoreAlreadyLocked)
    ));
    drop(owner);
    assert!(matches!(
        NativeRuntime::new(&root, Arc::new(FakeProbe)),
        Err(NativeRuntimeError::StoreAlreadyLocked)
    ));
    drop(clone);
    assert!(NativeRuntime::new(&root, Arc::new(FakeProbe)).is_ok());
}

#[cfg(unix)]
#[test]
fn approval_release_and_recovery_refuse_symlinked_store_paths() {
    use std::os::unix::fs::symlink;

    let (_temp, root, binary) = fixture();
    let runtime = runtime(&root);
    let outside = root.with_file_name("outside-store");
    fs::create_dir_all(&outside).expect("outside directory");
    fs::write(outside.join("keep"), "outside data").expect("outside sentinel");
    let found = candidate(&runtime, &binary);
    let hash = "a".repeat(64);

    symlink(&outside, root.join("native-runtime")).expect("link native runtime dir");
    assert!(matches!(
        runtime.approve(&found, NativePlatform::LinuxX64),
        Err(NativeRuntimeError::UnsafeStorePath)
    ));
    assert!(matches!(
        runtime.release(&hash),
        Err(NativeRuntimeError::UnsafeStorePath)
    ));
    assert!(matches!(
        runtime.recover_retired(),
        Err(NativeRuntimeError::UnsafeStorePath)
    ));
    fs::remove_file(root.join("native-runtime")).expect("remove test symlink");

    fs::create_dir(root.join("native-runtime")).expect("native runtime directory");
    symlink(&outside, root.join("native-runtime/builds")).expect("link builds directory");
    assert!(matches!(
        runtime.approve(&found, NativePlatform::LinuxX64),
        Err(NativeRuntimeError::UnsafeStorePath)
    ));
    assert!(matches!(
        runtime.release(&hash),
        Err(NativeRuntimeError::UnsafeStorePath)
    ));
    assert!(matches!(
        runtime.recover_retired(),
        Err(NativeRuntimeError::UnsafeStorePath)
    ));
    fs::remove_file(root.join("native-runtime/builds")).expect("remove test symlink");

    let builds = root.join("native-runtime/builds");
    fs::create_dir(&builds).expect("real builds directory");
    symlink(&outside, builds.join(".retired-escape")).expect("link retired path outside");
    assert!(matches!(
        runtime.recover_retired(),
        Err(NativeRuntimeError::UnsafeStorePath)
    ));
    assert_eq!(
        fs::read(outside.join("keep")).expect("outside sentinel survives"),
        b"outside data"
    );
    fs::remove_file(builds.join(".retired-escape")).expect("remove retired test symlink");

    let retired = builds.join(".retired-nested");
    let retired_bundle = retired.join("bundle");
    fs::create_dir_all(&retired_bundle).expect("retired bundle fixture");
    symlink(outside.join("keep"), retired_bundle.join("outside-link"))
        .expect("nested retired symlink");
    assert_eq!(runtime.recover_retired().expect("remove retired bundle"), 1);
    assert_eq!(
        fs::read(outside.join("keep")).expect("outside sentinel survives nested cleanup"),
        b"outside data"
    );

    let bundle = builds.join(&hash);
    fs::create_dir(&bundle).expect("fake bundle directory");
    symlink(outside.join("keep"), bundle.join("codex")).expect("link executable outside");
    fs::write(bundle.join(COMPANION), "companion").expect("fake companion");
    assert!(matches!(
        runtime.release(&hash),
        Err(NativeRuntimeError::UnsafeStorePath)
    ));
    assert_eq!(
        fs::read(outside.join("keep")).expect("outside sentinel survives"),
        b"outside data"
    );
    fs::remove_dir_all(&bundle).expect("remove fake bundle");

    let status_path = root.join("native-runtime/status.json");
    symlink(outside.join("keep"), &status_path).expect("link status outside");
    assert!(matches!(
        runtime.check(
            DiscoveryInputs {
                pinned_path: Some(binary.clone()),
                ..DiscoveryInputs::default()
            },
            NativePlatform::LinuxX64,
        ),
        Err(NativeRuntimeError::UnsafeStorePath)
    ));
    fs::remove_file(status_path).expect("remove status test symlink");

    let alias = root.with_file_name("state-root-alias");
    symlink(&root, &alias).expect("link state root");
    assert!(matches!(
        NativeRuntime::new(alias, Arc::new(FakeProbe)),
        Err(NativeRuntimeError::UnsafeStorePath)
    ));
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
        CanonicalPath::existing(&native).expect("canonical binary")
    );
}

#[test]
fn compatibility_probe_refusal_is_typed_and_named() {
    let (_temp, root, binary) = fixture();
    let runtime = NativeRuntime::with_ports(
        &root,
        Arc::new(RefusingProbe),
        Arc::new(FixedClock),
        Arc::new(super::NoFailPoints),
    )
    .expect("exclusive runtime instance");
    let found = candidate(&runtime, &binary);
    assert!(matches!(
        runtime.approve(&found, NativePlatform::LinuxX64),
        Err(NativeRuntimeError::CompatibilityRefused(
            CompatibilityRefusal::ProtocolSchema
        ))
    ));
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
fn release_rename_crash_is_recoverable_and_idempotent() {
    for (step, after, renamed) in [
        (DurableStep::RetiredBundleRename, false, false),
        (DurableStep::RetiredBundleRename, true, true),
        (DurableStep::RetiredDirectorySync, false, true),
        (DurableStep::RetiredDirectorySync, true, true),
    ] {
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
        drop(normal);
        let crash = NativeRuntime::with_ports(
            &root,
            Arc::new(FakeProbe),
            Arc::new(FixedClock),
            Arc::new(OneShot::new(step, after)),
        )
        .expect("exclusive crash runtime");
        assert!(matches!(
            crash.release(&old_hash),
            Err(NativeRuntimeError::FailPoint(_))
        ));
        assert_eq!(old_path.exists(), !renamed);
        if renamed {
            assert_eq!(crash.recover_retired().expect("finish cleanup"), 1);
        } else {
            assert!(crash.release(&old_hash).expect("replay release"));
        }
        assert_eq!(crash.recover_retired().expect("replay cleanup"), 0);
    }
}

#[test]
fn retired_bundle_removal_crash_is_recoverable_and_idempotent() {
    for (step, after, recovered_count) in [
        (DurableStep::RetiredBundleRemoval, false, 1),
        (DurableStep::RetiredBundleRemoval, true, 0),
        (DurableStep::RetiredDirectoryCleanupSync, false, 0),
        (DurableStep::RetiredDirectoryCleanupSync, true, 0),
    ] {
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
        drop(normal);
        let interrupted = NativeRuntime::with_ports(
            &root,
            Arc::new(FakeProbe),
            Arc::new(FixedClock),
            Arc::new(OneShot::new(step, after)),
        )
        .expect("exclusive crash runtime");
        assert!(matches!(
            interrupted.release(&old_hash),
            Err(NativeRuntimeError::FailPoint(_))
        ));
        let recovered = interrupted
            .recover_retired()
            .expect("finish or confirm removal");
        assert_eq!(recovered, recovered_count);
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
        (DurableStep::StatusDirectorySync, false, true),
        (DurableStep::StatusDirectorySync, true, true),
    ] {
        let (_temp, root, binary) = fixture();
        let normal = runtime(&root);
        let first = approve(&normal, &binary);
        fs::write(&binary, "version=2.0.0\n").expect("second candidate");
        let approved = approve(&normal, &binary);
        normal.switch(&first).expect("select first bundle");
        let prior = normal
            .read_state()
            .expect("prior state read")
            .expect("prior state exists")
            .record()
            .clone();
        assert_ne!(prior["selected"], approved);
        let mut next = prior.clone();
        next["selected"] = approved.clone();
        drop(normal);
        let crash = NativeRuntime::with_ports(
            &root,
            Arc::new(FakeProbe),
            Arc::new(FixedClock),
            Arc::new(OneShot::new(step, after)),
        )
        .expect("exclusive crash runtime");
        assert!(matches!(
            crash.switch(&approved),
            Err(NativeRuntimeError::FailPoint(_))
        ));
        drop(crash);
        let reopened_runtime = runtime(&root);
        let reopened = reopened_runtime
            .read_state()
            .expect("restart state read")
            .expect("restart state exists");
        assert_eq!(
            reopened.record(),
            if expected_new { &next } else { &prior },
            "{step:?}, after={after}"
        );
        let selected_path = Path::new(
            reopened.selected().expect("published selection")["path"]
                .as_str()
                .expect("selected path"),
        );
        assert!(
            selected_path.is_file(),
            "published executable survives interruption"
        );
        reopened_runtime.switch(&approved).expect("safe replay");
        assert_eq!(
            reopened_runtime
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
    for (step, after) in [
        (DurableStep::BundleDirectoryRename, false),
        (DurableStep::BundleDirectoryRename, true),
        (DurableStep::BundleDirectorySync, false),
        (DurableStep::BundleDirectorySync, true),
    ] {
        let (_temp, root, binary) = fixture();
        let crash = NativeRuntime::with_ports(
            &root,
            Arc::new(FakeProbe),
            Arc::new(FixedClock),
            Arc::new(OneShot::new(step, after)),
        )
        .expect("exclusive crash runtime");
        let interrupted = crash
            .check(
                DiscoveryInputs {
                    pinned_path: Some(binary.clone()),
                    ..DiscoveryInputs::default()
                },
                NativePlatform::LinuxX64,
            )
            .expect("rejected sync interruption is recorded");
        assert!(interrupted.selected().is_none());
        drop(crash);
        let retry = runtime(&root);
        let approved = retry
            .check(
                DiscoveryInputs {
                    pinned_path: Some(binary.clone()),
                    ..DiscoveryInputs::default()
                },
                NativePlatform::LinuxX64,
            )
            .expect("retry after durable directory sync")
            .selected()
            .expect("approved bundle selected")
            .clone();
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
        )
        .expect("exclusive crash runtime");
        let found = candidate(&interrupted, &binary);
        assert!(matches!(
            interrupted.approve(&found, NativePlatform::LinuxX64),
            Err(NativeRuntimeError::FailPoint(_))
        ));
        assert!(
            interrupted
                .read_state()
                .expect("no selected state")
                .is_none()
        );
        drop(interrupted);
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

use studio_runtime_store::path_identity::CanonicalPath;
