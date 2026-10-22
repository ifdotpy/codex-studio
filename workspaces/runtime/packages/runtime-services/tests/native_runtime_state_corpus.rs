use std::sync::Arc;
use std::{fs, path::Path};

use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use tempfile::TempDir;

struct NoProbe;

impl studio_runtime_services::native_runtime::CompatibilityProbe for NoProbe {
    fn version(
        &self,
        _executable: &std::path::Path,
    ) -> Result<String, studio_runtime_services::native_runtime::NativeRuntimeError> {
        unreachable!("state corpus never probes an executable")
    }

    fn check(
        &self,
        _bundle: &std::path::Path,
        _platform: studio_runtime_services::provider_binaries::NativePlatform,
    ) -> Result<
        studio_runtime_services::native_runtime::CompatibilityResult,
        studio_runtime_services::native_runtime::NativeRuntimeError,
    > {
        unreachable!("state corpus never approves a candidate")
    }
}

struct FixtureProbe;

impl studio_runtime_services::native_runtime::CompatibilityProbe for FixtureProbe {
    fn version(
        &self,
        executable: &Path,
    ) -> Result<String, studio_runtime_services::native_runtime::NativeRuntimeError> {
        let bytes = fs::read(executable)?;
        Ok(String::from_utf8_lossy(&bytes)
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("version="))
            .unwrap_or("1.2.3")
            .to_owned())
    }

    fn check(
        &self,
        bundle: &Path,
        _platform: studio_runtime_services::provider_binaries::NativePlatform,
    ) -> Result<
        studio_runtime_services::native_runtime::CompatibilityResult,
        studio_runtime_services::native_runtime::NativeRuntimeError,
    > {
        use studio_runtime_services::native_runtime::{
            CompatibilityEvidence, CompatibilityRefusal, CompatibilityResult,
        };
        if !bundle.join("codex-code-mode-host").is_file() {
            return Ok(CompatibilityResult::Refused(
                CompatibilityRefusal::CompanionCheck,
            ));
        }
        Ok(CompatibilityResult::Approved(CompatibilityEvidence {
            schema: json!({"methods": ["initialize"]}),
            smoke: json!(true),
            companions: BTreeMap::from([(
                "codex-code-mode-host".to_owned(),
                json!({"smoke": true}),
            )]),
        }))
    }
}

#[test]
#[allow(clippy::disallowed_methods)]
fn rust_reads_the_complete_status_record_written_by_python() {
    let temp = TempDir::new().expect("isolated state root");
    let root = studio_runtime_store::path_identity::resolve(temp.path()).expect("resolved root");
    let expected: BTreeMap<String, Value> =
        serde_json::from_str(include_str!("native_runtime_state.json"))
            .expect("frozen status records");
    for (name, record) in &expected {
        let directory = root.join(name).join("native-runtime");
        fs::create_dir_all(&directory).expect("status directory");
        fs::write(
            directory.join("status.json"),
            serde_json::to_vec(record).expect("status JSON"),
        )
        .expect("write status fixture");
    }
    for (name, record) in &expected {
        let runtime = studio_runtime_services::native_runtime::NativeRuntime::new(
            root.join(name),
            Arc::new(NoProbe),
        )
        .expect("exclusive corpus runtime");
        let actual = runtime
            .read_state()
            .expect("Rust state reader")
            .expect("Python-created status file");
        assert_eq!(actual.record(), record, "complete Python state case {name}");
    }

    let state_root = root.join("rotation");
    let native_runtime = studio_runtime_services::native_runtime::NativeRuntime::new(
        &state_root,
        Arc::new(FixtureProbe),
    )
    .expect("exclusive rotation runtime");
    let candidate = root.join("candidate");
    fs::create_dir(&candidate).expect("candidate directory");
    let executable = candidate.join("codex");
    fs::write(&executable, "version=1.2.3\nfixture only\n").expect("harmless executable fixture");
    fs::write(
        candidate.join("codex-code-mode-host"),
        "fixture companion\n",
    )
    .expect("harmless companion fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .expect("fixture executable mode");
        fs::set_permissions(
            candidate.join("codex-code-mode-host"),
            fs::Permissions::from_mode(0o700),
        )
        .expect("fixture companion mode");
    }
    let first_state = native_runtime
        .check(
            studio_runtime_services::provider_binaries::DiscoveryInputs {
                pinned_path: Some(executable.clone()),
                ..Default::default()
            },
            studio_runtime_services::provider_binaries::NativePlatform::LinuxX64,
        )
        .expect("discover and approve harmless fixture on Python-written state");
    assert_eq!(
        first_state.record()["accounts"]["default"]["rotation"]["id"],
        "op-a"
    );
    assert_eq!(first_state.record()["futurePythonField"]["preserve"][2], 8);
    let old_record = first_state
        .selected()
        .expect("first approved executable")
        .clone();
    let old_hash = old_record["bundleSha256"]
        .as_str()
        .expect("first bundle hash")
        .to_owned();
    let old_path =
        Path::new(old_record["path"].as_str().expect("first executable path")).to_owned();
    let live_session = native_runtime
        .retain_selected()
        .expect("retain approved Python-state executable");

    fs::write(&executable, "version=2.0.0\nfixture only\n").expect("changed candidate content");
    let second_state = native_runtime
        .check(
            studio_runtime_services::provider_binaries::DiscoveryInputs {
                pinned_path: Some(executable),
                ..Default::default()
            },
            studio_runtime_services::provider_binaries::NativePlatform::LinuxX64,
        )
        .expect("approve changed candidate content");
    let new_record = second_state
        .selected()
        .expect("second approved executable")
        .clone();
    native_runtime
        .switch(&old_record)
        .expect("switch back to retained executable");
    native_runtime
        .switch(&new_record)
        .expect("switch to new executable");
    assert!(matches!(
        native_runtime.release(&old_hash),
        Err(studio_runtime_services::native_runtime::NativeRuntimeError::InUse)
    ));
    assert!(old_path.is_file(), "live session keeps the old executable");
    drop(live_session);
    assert!(
        native_runtime
            .release(&old_hash)
            .expect("release unused executable")
    );

    let persisted = native_runtime
        .read_state()
        .expect("reread Rust state")
        .expect("saved state");
    assert_eq!(persisted.record()["selected"], new_record);
    assert_eq!(
        persisted.record()["accounts"],
        second_state.record()["accounts"]
    );
    assert_eq!(persisted.record()["status"], "ready");
}
