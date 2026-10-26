#![allow(clippy::duplicate_mod)]

use super::*;
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering as AtomicOrdering},
    },
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

fn isolated_tempdir() -> tempfile::TempDir {
    prepared_python::disposable_state_dir()
}

struct MutableConnections(Mutex<Vec<ProviderProfile>>, tokio::sync::watch::Sender<u64>);

impl MutableConnections {
    fn new(profiles: Vec<ProviderProfile>) -> Self {
        let (changes, _) = tokio::sync::watch::channel(0);
        Self(Mutex::new(profiles), changes)
    }

    fn replace(&self, profiles: Vec<ProviderProfile>) {
        *self.0.lock().expect("connection fixture lock") = profiles;
        self.1.send_modify(|generation| *generation += 1);
    }
}

impl ActiveProviderConnections for MutableConnections {
    fn snapshot(&self) -> Vec<ProviderProfile> {
        self.0.lock().expect("connection fixture lock").clone()
    }

    fn subscribe_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.1.subscribe()
    }
}

struct DisabledStore;

impl ProviderVersionStore for DisabledStore {
    fn upstream_enabled(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<bool, ProviderVersionsError>> + Send + '_>> {
        Box::pin(std::future::ready(Ok(false)))
    }

    fn persist_warnings<'a>(
        &'a self,
        _: &'a [String],
        _: &'a [Value],
    ) -> Pin<Box<dyn Future<Output = Result<(), ProviderVersionsError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

struct RecoveringStore {
    unavailable: AtomicBool,
    reads: AtomicUsize,
    completed_passes: AtomicUsize,
}

impl ProviderVersionStore for RecoveringStore {
    fn upstream_enabled(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<bool, ProviderVersionsError>> + Send + '_>> {
        self.reads.fetch_add(1, AtomicOrdering::SeqCst);
        Box::pin(std::future::ready(
            if self.unavailable.load(AtomicOrdering::SeqCst) {
                Err(ProviderVersionsError::PreferenceRead)
            } else {
                Ok(true)
            },
        ))
    }

    fn persist_warnings<'a>(
        &'a self,
        _: &'a [String],
        _: &'a [Value],
    ) -> Pin<Box<dyn Future<Output = Result<(), ProviderVersionsError>> + Send + 'a>> {
        Box::pin(async move {
            self.completed_passes.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        })
    }
}

struct DropObservedStore(Mutex<Option<tokio::sync::oneshot::Sender<()>>>);

impl Drop for DropObservedStore {
    fn drop(&mut self) {
        if let Ok(sender) = self.0.get_mut()
            && let Some(sender) = sender.take()
        {
            let _ignored = sender.send(());
        }
    }
}

impl ProviderVersionStore for DropObservedStore {
    fn upstream_enabled(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<bool, ProviderVersionsError>> + Send + '_>> {
        Box::pin(std::future::ready(Ok(false)))
    }

    fn persist_warnings<'a>(
        &'a self,
        _: &'a [String],
        _: &'a [Value],
    ) -> Pin<Box<dyn Future<Output = Result<(), ProviderVersionsError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> f64 {
        200_000.0
    }
}

struct MutableClock(AtomicU64);

impl Clock for MutableClock {
    fn now(&self) -> f64 {
        self.0.load(AtomicOrdering::SeqCst) as f64
    }
}

struct CountingProbe {
    calls: AtomicUsize,
    observed: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}

impl VersionProbe for CountingProbe {
    fn probe(
        &self,
        profile: &ProviderProfile,
        _: Instant,
    ) -> Result<VersionFacts, ProviderPortError> {
        self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        if let Some(sender) = &self.observed {
            let _ignored = sender.send(profile.connection_id.clone().unwrap_or_default());
        }
        Ok(VersionFacts {
            running_version: Some("0.153.4".into()),
            installed_version: Some("0.153.4".into()),
            ..VersionFacts::default()
        })
    }
}

struct CountingFetch(AtomicUsize);

impl UpstreamFetch for CountingFetch {
    fn fetch(
        &self,
        _: &str,
        _: Option<&str>,
        _: Instant,
    ) -> Result<UpstreamReply, ProviderPortError> {
        self.0.fetch_add(1, AtomicOrdering::SeqCst);
        Err(ProviderPortError)
    }
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
#[allow(clippy::disallowed_methods)]
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
        launched_binary: None,
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
fn failed_release_attempt_uses_bounded_retry_after_a_stale_cache() {
    let entry = CacheEntry {
        version: Some("2.1.9".into()),
        etag: None,
        fetched_at: Some(100_000.0),
        last_attempt_at: 100_000.0 + UPSTREAM_TTL_SECONDS,
    };
    let failed_at = entry.last_attempt_at;
    assert_eq!(
        next_attempt_at(&entry),
        failed_at + FAILURE_RETRY_SECONDS as f64
    );
    assert!(!attempt_due(
        &entry,
        failed_at + FAILURE_RETRY_SECONDS as f64 - 1.0
    ));
    assert!(attempt_due(
        &entry,
        failed_at + FAILURE_RETRY_SECONDS as f64
    ));
}

#[test]
fn failed_monitor_pass_schedules_bounded_retry_and_idle_disabled_pass_disarms() {
    let monotonic_now = tokio::time::Instant::now();
    assert_eq!(
        next_check_deadline(true, false, &Default::default(), 200_000.0, monotonic_now,),
        Some(monotonic_now + Duration::from_secs(FAILURE_RETRY_SECONDS))
    );
    assert_eq!(
        next_check_deadline(false, false, &Default::default(), 200_000.0, monotonic_now,),
        None
    );
}

#[test]
fn full_profile_equality_tracks_session_and_profile_changes() {
    let profile = ProviderProfile {
        account_key: "connected".into(),
        provider: "codex".into(),
        launched_binary: None,
        configured_binary: None,
        server_identity: "server-a".into(),
        connection_id: Some("connection-a".into()),
    };
    let mut reconnected = profile.clone();
    reconnected.connection_id = Some("connection-b".into());
    assert_ne!(profile, reconnected);
    reconnected.connection_id = profile.connection_id.clone();
    reconnected.server_identity = "server-b".into();
    assert_ne!(profile, reconnected);
    reconnected.server_identity = profile.server_identity.clone();
    reconnected.configured_binary = Some("/configured/codex".into());
    assert_ne!(profile, reconnected);
    reconnected.configured_binary = None;
    reconnected.provider = "claude".into();
    assert_ne!(profile, reconnected);
}

#[cfg(unix)]
#[test]
fn claude_version_probe_uses_launch_binary_and_reports_configured_binary_separately() {
    use super::SystemVersionProbe;
    use crate::provider_versions::VersionProbe;
    use std::{
        os::unix::fs::PermissionsExt,
        time::{Duration, Instant},
    };

    let temporary = tempfile::tempdir().expect("version probe fixture");
    let launched = temporary.path().join("claude-launched");
    let configured = temporary.path().join("claude-configured");
    for (path, version) in [(&launched, "2.1.100"), (&configured, "2.1.200")] {
        std::fs::write(path, format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n"))
            .expect("write fake version binary");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("make fake version binary executable");
    }
    let profile = ProviderProfile {
        account_key: "claude-profile".into(),
        provider: "claude".into(),
        launched_binary: Some(launched.to_string_lossy().into_owned()),
        configured_binary: Some(configured.to_string_lossy().into_owned()),
        server_identity: "server".into(),
        connection_id: Some("generation-1".into()),
    };
    let facts = SystemVersionProbe
        .probe(&profile, Instant::now() + Duration::from_secs(5))
        .expect("both Claude executable versions");
    assert_eq!(facts.running_version, None);
    assert_eq!(facts.installed_version.as_deref(), Some("2.1.100"));
    assert_eq!(facts.configured_version.as_deref(), Some("2.1.200"));
    assert_eq!(facts.available_version.as_deref(), Some("2.1.200"));
}

#[tokio::test]
#[allow(clippy::disallowed_methods)]
async fn provider_update_notice_durable_failure_restart_and_replay_matrix() {
    let temp = isolated_tempdir();
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
    let store = Arc::new(
        RuntimeStore::open(Some(&rust_state))
            .await
            .expect("open Python-written state"),
    );
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
        let trigger = trigger.to_owned();
        store.transaction(move |transaction| {
            transaction.execute_batch(&trigger)?;
            Ok(())
        })
    };
    let read_notice_state = || {
        let connection = store.test_reader().expect("read notice state");
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
        .await.expect("before-write trigger installed");
    let connected_accounts = vec!["connected".to_owned()];
    assert!(
        adapter
            .persist_warnings(&connected_accounts, &warnings)
            .await
            .is_err()
    );
    store.transaction(|transaction| transaction.execute_batch("DROP TRIGGER provider_notice_before_insert; DROP TRIGGER provider_notice_before_update;").map_err(Into::into))
        .await.expect("before-write trigger removed");
    assert_eq!(
        read_notice_state(),
        initial,
        "failure before the write is quiescent"
    );

    warn_write("CREATE TRIGGER provider_notice_after_insert AFTER INSERT ON sync_entities WHEN NEW.collection='workspace' AND NEW.id='current' BEGIN SELECT RAISE(ABORT,'after durable notice write'); END; CREATE TRIGGER provider_notice_after_update AFTER UPDATE ON sync_entities WHEN NEW.collection='workspace' AND NEW.id='current' BEGIN SELECT RAISE(ABORT,'after durable notice write'); END;")
        .await.expect("after-write trigger installed");
    assert!(
        adapter
            .persist_warnings(&connected_accounts, &warnings)
            .await
            .is_err()
    );
    store.transaction(|transaction| transaction.execute_batch("DROP TRIGGER provider_notice_after_insert; DROP TRIGGER provider_notice_after_update;").map_err(Into::into))
        .await.expect("after-write trigger removed");
    assert_eq!(
        read_notice_state(),
        initial,
        "transaction rolls back after write failure"
    );

    let disconnected_warning = json!({
        "id":"provider-version:disconnected",
        "accountKey":"disconnected",
        "provider":"codex",
        "version":"0.153.2",
        "baseline":"0.153.4",
        "message":"Disconnected account warning remains until that connection is observed again.",
        "at":100_000.0
    });
    adapter
        .persist_warnings(
            &["disconnected".into()],
            std::slice::from_ref(&disconnected_warning),
        )
        .await
        .expect("disconnected account warning seeded");
    adapter
        .persist_warnings(&connected_accounts, &warnings)
        .await
        .expect("connected notices commit");
    let written = read_notice_state().expect("workspace entity created");
    let written_payload: Value = serde_json::from_str(&written.1).expect("sync entity JSON");
    assert_eq!(
        written_payload["value"]["nativeNotices"]
            .as_array()
            .map(Vec::len),
        Some(4)
    );
    assert_eq!(
        written_payload["value"]["nativeNotices"][0], disconnected_warning,
        "a pass for a connected account retains the disconnected account's warning"
    );
    assert_eq!(
        written_payload["value"]["nativeNotices"][1],
        baseline_warning
    );
    assert_eq!(written_payload["value"]["nativeNotices"][2], first);
    assert_eq!(
        written_payload["value"]["nativeNotices"][3],
        upstream_warning
    );
    let written_notices = written_payload["value"]["nativeNotices"]
        .as_array()
        .expect("notice array");
    let oracle_warnings = expected["warnings"]
        .as_array()
        .expect("oracle warnings")
        .iter()
        .filter(|warning| warning["accountKey"] == "connected")
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(&written_notices[1..], oracle_warnings.as_slice());
    adapter
        .persist_warnings(&connected_accounts, &warnings)
        .await
        .expect("same version replay");
    assert_eq!(
        read_notice_state(),
        Some(written.clone()),
        "same-version replay does not write"
    );

    drop(adapter);
    drop(store);
    let restarted = Arc::new(
        RuntimeStore::open(Some(&rust_state))
            .await
            .expect("restart Python-written state"),
    );
    let restarted_adapter = SyncProviderVersionStore(restarted.clone());
    let replay_with_new_time = json!({"id":"provider-update:connected","accountKey":"connected","provider":"codex","version":"0.153.5","baseline":"0.153.4","message":"reworded notice for same version","at":300_000.0,"level":"info"});
    let replayed_warnings = vec![
        json!({"id":"provider-version:connected","accountKey":"connected","provider":"codex","version":"0.153.3","baseline":"0.153.4","message":"reworded baseline warning","at":300_000.0}),
        replay_with_new_time,
        json!({"id":"provider-update:connected:upstream","accountKey":"connected","provider":"codex","version":"0.154.0","baseline":"0.153.4","message":"reworded upstream warning","at":300_000.0,"level":"info"}),
    ];
    restarted_adapter
        .persist_warnings(&connected_accounts, &replayed_warnings)
        .await
        .expect("restart replay");
    assert_eq!(
        restarted.test_reader().expect("read after restart").query_row(
            "SELECT seq,payload FROM sync_entities WHERE collection='workspace' AND id='current' AND deleted=0", [],
            |row| Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?)),
        ).expect("replayed row"),
        written,
        "replay preserves original notice timestamp and sequence",
    );
    let changed = json!({"id":"provider-update:connected","accountKey":"connected","provider":"codex","version":"0.155.0","baseline":"0.153.4","message":"release 0.155.0 available","at":400_000.0,"level":"info"});
    restarted_adapter
        .persist_warnings(
            &connected_accounts,
            &[
                replayed_warnings[0].clone(),
                changed.clone(),
                replayed_warnings[2].clone(),
            ],
        )
        .await
        .expect("changed content under same id");
    let changed_row: (i64, String) = restarted.test_reader().expect("read changed state").query_row(
        "SELECT seq,payload FROM sync_entities WHERE collection='workspace' AND id='current' AND deleted=0", [],
        |row| Ok((row.get(0)?,row.get(1)?)),
    ).expect("changed workspace row");
    assert!(changed_row.0 > written.0);
    let changed_payload: Value = serde_json::from_str(&changed_row.1).expect("changed entity JSON");
    assert_eq!(
        changed_payload["value"]["nativeNotices"][0],
        disconnected_warning
    );
    assert_eq!(
        changed_payload["value"]["nativeNotices"][1],
        baseline_warning
    );
    assert_eq!(changed_payload["value"]["nativeNotices"][2], changed);
    assert_eq!(
        changed_payload["value"]["nativeNotices"][3],
        upstream_warning
    );
}

fn profile(account: &str, provider: &str, connection: &str) -> ProviderProfile {
    ProviderProfile {
        account_key: account.into(),
        provider: provider.into(),
        launched_binary: None,
        configured_binary: None,
        server_identity: format!("server-{account}"),
        connection_id: Some(connection.into()),
    }
}

#[tokio::test]
async fn preference_read_error_fails_closed_without_an_upstream_request() {
    let temporary = isolated_tempdir();
    let root = temporary.path().join("state");
    let mut oracle = PythonFixture::new(temporary.path());
    let cases: Vec<Value> = serde_json::from_str(NOTICE_CASES).expect("notice seed corpus");
    oracle.call(json!({
        "command":"case",
        "module":"notification_dispatch",
        "root":temporary.path().join("python-state"),
        "rust":root,
        "case":cases[0]
    }));
    std::fs::create_dir_all(&root).expect("isolated state directory exists");
    let runtime = Arc::new(
        RuntimeStore::open(Some(&root))
            .await
            .expect("isolated store opens"),
    );
    runtime
        .transaction(|transaction| {
            transaction.execute_batch("DROP TABLE sync_entities")?;
            Ok(())
        })
        .await
        .expect("preference source made unreadable");
    let store = runtime_provider_version_store(runtime);
    assert!(
        matches!(
            store.upstream_enabled().await,
            Err(ProviderVersionsError::Store(_))
        ),
        "preference storage errors remain typed"
    );

    let fetch = Arc::new(CountingFetch(AtomicUsize::new(0)));
    let monitor = ProviderVersionMonitor::start(
        store,
        None,
        temporary.path().join("cache.json"),
        Arc::new(CountingProbe {
            calls: AtomicUsize::new(0),
            observed: None,
        }),
        fetch.clone(),
        Arc::new(FixedClock),
    )
    .await
    .expect("monitor starts despite unreadable preferences");
    assert!(monitor.wait_until_checked(Duration::from_secs(3)).await);
    assert_eq!(fetch.0.load(AtomicOrdering::SeqCst), 0);
    monitor.shutdown().await;
}

#[tokio::test]
async fn preference_read_failure_retries_without_using_stale_enablement() {
    let temporary = isolated_tempdir();
    let store = Arc::new(RecoveringStore {
        unavailable: AtomicBool::new(false),
        reads: AtomicUsize::new(0),
        completed_passes: AtomicUsize::new(0),
    });
    let fetch = Arc::new(CountingFetch(AtomicUsize::new(0)));
    let clock = Arc::new(MutableClock(AtomicU64::new(200_000)));
    let connections = Arc::new(MutableConnections::new(vec![profile(
        "connected",
        "codex",
        "connection-1",
    )]));
    let monitor = ProviderVersionMonitor::start(
        store.clone(),
        Some(connections),
        temporary.path().join("cache.json"),
        Arc::new(CountingProbe {
            calls: AtomicUsize::new(0),
            observed: None,
        }),
        fetch.clone(),
        clock.clone(),
    )
    .await
    .expect("monitor starts with provider updates enabled");
    assert!(monitor.wait_until_checked(Duration::from_secs(3)).await);
    assert_eq!(fetch.0.load(AtomicOrdering::SeqCst), RELEASES.len());
    tokio::time::timeout(Duration::from_secs(3), async {
        while store.completed_passes.load(AtomicOrdering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("initial pass persists its warning snapshot");

    clock
        .0
        .fetch_add(FAILURE_RETRY_SECONDS + 1, AtomicOrdering::SeqCst);
    store.unavailable.store(true, AtomicOrdering::SeqCst);
    monitor.wake();
    tokio::time::timeout(Duration::from_secs(3), async {
        while store.completed_passes.load(AtomicOrdering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("targeted wake observes the transient preference read error");
    assert_eq!(
        fetch.0.load(AtomicOrdering::SeqCst),
        RELEASES.len(),
        "unknown preference state must not make an upstream request"
    );

    store.unavailable.store(false, AtomicOrdering::SeqCst);
    monitor.wake();
    tokio::time::timeout(Duration::from_secs(3), async {
        while store.completed_passes.load(AtomicOrdering::SeqCst) < 3
            || fetch.0.load(AtomicOrdering::SeqCst) < RELEASES.len() * 2
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a successful preference read resumes upstream checks");
    monitor.shutdown().await;
}

#[tokio::test]
async fn dropping_idle_monitor_releases_store_and_worker_ownership() {
    let temporary = isolated_tempdir();
    let (dropped, released) = tokio::sync::oneshot::channel();
    let monitor = ProviderVersionMonitor::start(
        Arc::new(DropObservedStore(Mutex::new(Some(dropped)))),
        None,
        temporary.path().join("cache.json"),
        Arc::new(CountingProbe {
            calls: AtomicUsize::new(0),
            observed: None,
        }),
        Arc::new(CountingFetch(AtomicUsize::new(0))),
        Arc::new(FixedClock),
    )
    .await
    .expect("monitor starts");
    assert!(monitor.wait_until_checked(Duration::from_secs(3)).await);

    drop(monitor);
    tokio::time::timeout(Duration::from_secs(3), released)
        .await
        .expect("idle worker does not retain the monitor")
        .expect("store drop signal");
}

#[tokio::test]
async fn unsupported_provider_is_projected_without_launching_a_probe() {
    let temporary = isolated_tempdir();
    let connections = Arc::new(MutableConnections::new(vec![profile(
        "other",
        "unsupported",
        "connection-1",
    )]));
    let probe = Arc::new(CountingProbe {
        calls: AtomicUsize::new(0),
        observed: None,
    });
    let monitor = ProviderVersionMonitor::start(
        Arc::new(DisabledStore),
        Some(connections),
        temporary.path().join("cache.json"),
        probe.clone(),
        Arc::new(CountingFetch(AtomicUsize::new(0))),
        Arc::new(FixedClock),
    )
    .await
    .expect("monitor starts");
    assert!(monitor.wait_until_checked(Duration::from_secs(3)).await);
    assert_eq!(probe.calls.load(AtomicOrdering::SeqCst), 0);
    assert_eq!(monitor.status()["providers"][0]["status"], "unknown");
    monitor.shutdown().await;
}

#[tokio::test]
async fn profile_change_notification_starts_a_new_probe_without_waiting_for_cache_expiry() {
    let temporary = isolated_tempdir();
    let connections = Arc::new(MutableConnections::new(vec![profile(
        "connected",
        "codex",
        "connection-1",
    )]));
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let monitor = ProviderVersionMonitor::start(
        Arc::new(DisabledStore),
        Some(connections.clone()),
        temporary.path().join("cache.json"),
        Arc::new(CountingProbe {
            calls: AtomicUsize::new(0),
            observed: Some(sender),
        }),
        Arc::new(CountingFetch(AtomicUsize::new(0))),
        Arc::new(FixedClock),
    )
    .await
    .expect("monitor starts");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), receiver.recv())
            .await
            .expect("initial probe timeout")
            .expect("initial probe"),
        "connection-1"
    );
    connections.replace(vec![profile("connected", "codex", "connection-2")]);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), receiver.recv())
            .await
            .expect("profile-change probe timeout")
            .expect("profile-change probe"),
        "connection-2"
    );
    monitor.shutdown().await;
}
