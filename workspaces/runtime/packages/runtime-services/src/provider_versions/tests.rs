use super::*;
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};
use studio_runtime_store::RuntimeStore;

#[path = "../../tests/support/prepared_python.rs"]
mod prepared_python;

const CASES: &str = include_str!("../../tests/provider_versions/cases.json");
const NOTICE_CASES: &str = include_str!("../../tests/turn_lifecycle/notification_cases.json");

struct PythonFixture {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl PythonFixture {
    fn new(scratch: &std::path::Path) -> Self {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let python = prepared_python::prepared_python(&source);
        let mut child = Command::new(python)
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/turn_lifecycle/oracle.py"))
            .arg(scratch)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Python fixture oracle starts");
        Self {
            input: child.stdin.take().expect("oracle stdin"),
            output: BufReader::new(child.stdout.take().expect("oracle stdout")),
            child,
        }
    }

    fn call(&mut self, value: Value) -> Value {
        writeln!(self.input, "{value}").expect("oracle command writes");
        self.input.flush().expect("oracle command flushes");
        let mut line = String::new();
        self.output
            .read_line(&mut line)
            .expect("oracle response reads");
        let response: Value = serde_json::from_str(&line).expect("oracle response JSON");
        assert!(response.get("error").is_none(), "{}", response["error"]);
        response["result"].clone()
    }
}

impl Drop for PythonFixture {
    fn drop(&mut self) {
        let _ignored = writeln!(self.input, "{{\"command\":\"close\"}}");
        let _ignored = self.input.flush();
        let _ignored = self.child.wait();
    }
}

#[test]
fn version_parse_and_comparison_match_the_real_python_functions() {
    let cases: Vec<[String; 2]> = serde_json::from_str(CASES).expect("version corpus");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
    let python = prepared_python::prepared_python(&source);
    let oracle = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/provider_versions/oracle.py");
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/provider_versions/cases.json");
    let output = Command::new(python)
        .arg(oracle)
        .arg(corpus)
        .output()
        .expect("Python oracle launches");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: Vec<[bool; 3]> = serde_json::from_slice(&output.stdout).expect("oracle JSON");
    let actual = cases
        .iter()
        .map(|[installed, available]| {
            [
                parse_key(installed).is_some(),
                parse_key(available).is_some(),
                compare_version_text(installed, available)
                    .is_some_and(|order| order == Ordering::Less),
            ]
        })
        .collect::<Vec<_>>();
    assert_eq!(
        actual, expected,
        "provider version corpus differs from Python"
    );
    assert_eq!(cases.len(), 20);
}

#[test]
fn update_warning_projection_preserves_python_ids_and_messages() {
    let profile = ProviderProfile {
        account_key: "default".into(),
        provider: "codex".into(),
        configured_binary: None,
        server_identity: "server-1".into(),
        connection_id: Some("connection-1".into()),
    };
    let upstream = "0.154.0".to_owned();
    let row = diagnostic(
        &profile,
        VersionFacts {
            running_version: Some("0.153.3".into()),
            installed_version: Some("0.153.3".into()),
            available_version: Some("0.153.5".into()),
            ..VersionFacts::default()
        },
        Some(&upstream),
        123.0,
    );
    assert_eq!(row["status"], "outdated");
    assert_eq!(row["updateNotices"].as_array().map(Vec::len), Some(2));
    let warnings = warning_projection(&row, true);
    assert_eq!(warnings.len(), 3);
    assert_eq!(warnings[0]["id"], "provider-version:default");
    assert_eq!(warnings[1]["id"], "provider-update:default");
    assert_eq!(warnings[2]["id"], "provider-update:default:upstream");
    assert_eq!(warning_projection(&row, false).len(), 2);
}

#[test]
fn cached_release_expiry_matches_the_python_ttl_boundary() {
    let entry = CacheEntry {
        version: Some("2.1.9".into()),
        etag: None,
        fetched_at: Some(100_000.0),
        last_attempt_at: 100_000.0,
    };
    assert_eq!(
        cached_version(&entry, 100_000.0 + UPSTREAM_TTL_SECONDS - 1.0),
        Some("2.1.9")
    );
    assert_eq!(
        cached_version(&entry, 100_000.0 + UPSTREAM_TTL_SECONDS),
        None
    );
    assert!(!attempt_due(&entry, 100_000.0 + UPSTREAM_TTL_SECONDS - 1.0));
    assert!(attempt_due(&entry, 100_000.0 + UPSTREAM_TTL_SECONDS));
}

#[test]
fn active_connection_signature_tracks_server_and_connection_identity() {
    let profile = ProviderProfile {
        account_key: "connected".into(),
        provider: "codex".into(),
        configured_binary: None,
        server_identity: "server-a".into(),
        connection_id: Some("connection-a".into()),
    };
    let mut reconnected = profile.clone();
    reconnected.connection_id = Some("connection-b".into());
    assert_ne!(
        connection_identity(&profile),
        connection_identity(&reconnected)
    );
    reconnected.connection_id = profile.connection_id.clone();
    reconnected.server_identity = "server-b".into();
    assert_ne!(
        connection_identity(&profile),
        connection_identity(&reconnected)
    );
}

#[test]
fn provider_update_notice_durable_failure_restart_and_replay_matrix() {
    let temp = tempfile::tempdir().expect("isolated notice fixture");
    let mut oracle = PythonFixture::new(temp.path());
    let cases: Vec<Value> = serde_json::from_str(NOTICE_CASES).expect("notice seed corpus");
    let rust_state = temp.path().join("rust-state");
    oracle.call(json!({
        "command":"case",
        "module":"notification_dispatch",
        "root":temp.path().join("python-state"),
        "rust":rust_state,
        "case":cases[0]
    }));
    let store = Arc::new(RuntimeStore::open(Some(&rust_state)).expect("open Python-written state"));
    let adapter = SyncProviderVersionStore(store.clone());
    let first = json!({
        "id":"provider-update:connected",
        "accountKey":"connected",
        "provider":"codex",
        "version":"0.153.5",
        "baseline":"0.153.4",
        "message":"A newer installed Codex CLI version is selected for runtime updates, but this connection is still using 0.153.3. Available: 0.153.5.",
        "at":200_000.0,
        "level":"info"
    });
    let upstream_warning = json!({
        "id":"provider-update:connected:upstream",
        "accountKey":"connected",
        "provider":"codex",
        "version":"0.154.0",
        "baseline":"0.153.4",
        "message":"A newer Codex CLI release is available upstream than the version currently in use. Current: 0.153.3 · Available: 0.154.0.",
        "at":200_000.0,
        "level":"info"
    });
    let baseline_warning = json!({
        "id":"provider-version:connected",
        "accountKey":"connected",
        "provider":"codex",
        "version":"0.153.3",
        "baseline":"0.153.4",
        "message":"The running Codex CLI version 0.153.3 is below this repository's lowest recorded tested version (0.153.4). It may work poorly or fail. You can continue at your own risk.",
        "at":200_000.0
    });
    let warnings = vec![
        baseline_warning.clone(),
        first.clone(),
        upstream_warning.clone(),
    ];
    let python_source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
    let python = prepared_python::prepared_python(&python_source);
    let projection_oracle =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/provider_versions/projection_oracle.py");
    let expected_output = Command::new(python)
        .arg(projection_oracle)
        .output()
        .expect("Python provider/sync projection oracle starts");
    assert!(
        expected_output.status.success(),
        "{}",
        String::from_utf8_lossy(&expected_output.stderr)
    );
    let expected: Value = serde_json::from_slice(&expected_output.stdout)
        .expect("Python provider/sync projection oracle JSON");
    let warn_write = |trigger: &str| {
        store.transaction(|transaction| {
            transaction.execute_batch(trigger)?;
            Ok(())
        })
    };
    let read_notice_state = || {
        let connection = store.read().expect("read notice state");
        let raw: Option<(i64, String)> = connection
            .query_row(
                "SELECT seq,payload FROM sync_entities WHERE collection='workspace' AND id='current' AND deleted=0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .expect("read workspace entity");
        raw
    };

    let initial = read_notice_state();
    warn_write("CREATE TRIGGER provider_notice_before_insert BEFORE INSERT ON sync_entities WHEN NEW.collection='workspace' AND NEW.id='current' BEGIN SELECT RAISE(ABORT,'before durable notice write'); END; CREATE TRIGGER provider_notice_before_update BEFORE UPDATE ON sync_entities WHEN NEW.collection='workspace' AND NEW.id='current' BEGIN SELECT RAISE(ABORT,'before durable notice write'); END;")
        .expect("before-write trigger installed");
    assert!(adapter.persist_warnings(&warnings).is_err());
    store.transaction(|transaction| transaction.execute_batch("DROP TRIGGER provider_notice_before_insert; DROP TRIGGER provider_notice_before_update;").map_err(Into::into))
        .expect("before-write trigger removed");
    assert_eq!(
        read_notice_state(),
        initial,
        "failure before the write is quiescent"
    );

    warn_write("CREATE TRIGGER provider_notice_after_insert AFTER INSERT ON sync_entities WHEN NEW.collection='workspace' AND NEW.id='current' BEGIN SELECT RAISE(ABORT,'after durable notice write'); END; CREATE TRIGGER provider_notice_after_update AFTER UPDATE ON sync_entities WHEN NEW.collection='workspace' AND NEW.id='current' BEGIN SELECT RAISE(ABORT,'after durable notice write'); END;")
        .expect("after-write trigger installed");
    assert!(adapter.persist_warnings(&warnings).is_err());
    store.transaction(|transaction| transaction.execute_batch("DROP TRIGGER provider_notice_after_insert; DROP TRIGGER provider_notice_after_update;").map_err(Into::into))
        .expect("after-write trigger removed");
    assert_eq!(
        read_notice_state(),
        initial,
        "transaction rolls back after write failure"
    );

    adapter.persist_warnings(&warnings).expect("notice commits");
    let written = read_notice_state().expect("workspace entity created");
    let written_payload: Value = serde_json::from_str(&written.1).expect("sync entity JSON");
    assert_eq!(
        written_payload["value"]["nativeNotices"]
            .as_array()
            .map(Vec::len),
        Some(3)
    );
    assert_eq!(
        written_payload["value"]["nativeNotices"],
        expected["warnings"]
    );
    assert_eq!(
        written_payload["value"]["nativeNotices"][0],
        baseline_warning
    );
    assert_eq!(written_payload["value"]["nativeNotices"][1], first);
    assert_eq!(
        written_payload["value"]["nativeNotices"][2],
        upstream_warning
    );
    adapter
        .persist_warnings(&warnings)
        .expect("same version replay");
    assert_eq!(
        read_notice_state(),
        Some(written.clone()),
        "same-version replay does not write"
    );

    drop(adapter);
    drop(store);
    let restarted =
        Arc::new(RuntimeStore::open(Some(&rust_state)).expect("restart Python-written state"));
    let restarted_adapter = SyncProviderVersionStore(restarted.clone());
    let replay_with_new_time = json!({"id":"provider-update:connected","accountKey":"connected","provider":"codex","version":"0.153.5","baseline":"0.153.4","message":"reworded notice for same version","at":300_000.0,"level":"info"});
    let replayed_warnings = vec![
        json!({"id":"provider-version:connected","accountKey":"connected","provider":"codex","version":"0.153.3","baseline":"0.153.4","message":"reworded baseline warning","at":300_000.0}),
        replay_with_new_time,
        json!({"id":"provider-update:connected:upstream","accountKey":"connected","provider":"codex","version":"0.154.0","baseline":"0.153.4","message":"reworded upstream warning","at":300_000.0,"level":"info"}),
    ];
    restarted_adapter
        .persist_warnings(&replayed_warnings)
        .expect("restart replay");
    assert_eq!(
        restarted.read().expect("read after restart").query_row(
            "SELECT seq,payload FROM sync_entities WHERE collection='workspace' AND id='current' AND deleted=0", [],
            |row| Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?)),
        ).expect("replayed row"),
        written,
        "replay preserves original notice timestamp and sequence",
    );
    let changed = json!({"id":"provider-update:connected","accountKey":"connected","provider":"codex","version":"0.155.0","baseline":"0.153.4","message":"release 0.155.0 available","at":400_000.0,"level":"info"});
    restarted_adapter
        .persist_warnings(&[
            replayed_warnings[0].clone(),
            changed.clone(),
            replayed_warnings[2].clone(),
        ])
        .expect("changed content under same id");
    let changed_row: (i64, String) = restarted.read().expect("read changed state").query_row(
        "SELECT seq,payload FROM sync_entities WHERE collection='workspace' AND id='current' AND deleted=0", [],
        |row| Ok((row.get(0)?,row.get(1)?)),
    ).expect("changed workspace row");
    assert!(changed_row.0 > written.0);
    let changed_payload: Value = serde_json::from_str(&changed_row.1).expect("changed entity JSON");
    assert_eq!(
        changed_payload["value"]["nativeNotices"][0],
        baseline_warning
    );
    assert_eq!(changed_payload["value"]["nativeNotices"][1], changed);
    assert_eq!(
        changed_payload["value"]["nativeNotices"][2],
        upstream_warning
    );
}
