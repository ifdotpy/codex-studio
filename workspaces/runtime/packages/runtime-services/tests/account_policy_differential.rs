#[path = "support/prepared_python.rs"]
mod prepared_python_support;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use rusqlite::Connection;
use serde_json::{Value, json};
use studio_runtime_services::account_policy::AccountPolicyState;
use studio_runtime_store::account_state;

const REGISTRY: &[u8] = br#"{"version":1,"defaultAccountKey":"default","accounts":{"default":{"id":"default","home":"/disposable/codex","label":"Codex","source":"Codex CLI","status":"signedOut"}},"logins":{}}"#;

#[test]
fn python_registry_writers_and_sqlite_limits_reach_the_rust_read_models() {
    let output = Command::new(python_executable())
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/account_policy_producers.py"))
        .arg(server_source())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env(
            "CODEX_NODE",
            std::env::var_os("CODEX_NODE").unwrap_or_default(),
        )
        .output()
        .expect("run Python producers with isolated interpreter");
    assert!(
        output.status.success(),
        "Python producer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let corpus: Value = serde_json::from_slice(&output.stdout).expect("producer output JSON");

    let account_cases = corpus["accounts"].as_array().expect("registry states");
    for case in account_cases {
        let name = case["name"].as_str().expect("case name");
        let registry = case["registry"].as_str().expect("registry bytes");
        let state = AccountPolicyState::from_startup(Some(registry.as_bytes()), [])
            .unwrap_or_else(|error| panic!("product registry state {name} refused: {error}"));
        let snapshot = state.accounts();
        let actual = json!({
            "accounts": snapshot.accounts,
            "archivedAccounts": snapshot.archived_accounts,
            "defaultAccountKey": snapshot.default_account_key,
            "logins": snapshot.logins,
            "supportsDisconnect": true,
            "supportsDelete": true
        });
        assert_eq!(
            actual, case["python"],
            "registry projection differs for {name}"
        );
    }
    for case in corpus["invalidRegistries"]
        .as_array()
        .expect("invalid states")
    {
        let name = case["name"].as_str().expect("case name");
        let registry = case["registry"].as_str().expect("registry bytes");
        assert!(
            AccountPolicyState::from_startup(Some(registry.as_bytes()), []).is_err(),
            "strict parser accepted {name}"
        );
    }

    let limit_cases = corpus["limits"]
        .as_array()
        .expect("persisted SQLite states");
    for case in limit_cases {
        let name = case["name"].as_str().expect("case name");
        let account_key = case["accountKey"].as_str().expect("account key");
        let payload = case["payload"].as_str().expect("real sync entity payload");
        let workspace: Value = serde_json::from_str(payload).expect("workspace payload JSON");
        let connection = Connection::open_in_memory().expect("SQLite memory database");
        connection
            .execute(
                "CREATE TABLE sync_entities(collection TEXT,id TEXT,payload TEXT,deleted INTEGER)",
                [],
            )
            .expect("declared table");
        connection
            .execute(
                "INSERT INTO sync_entities VALUES('workspace','current',?1,0)",
                [payload],
            )
            .expect("insert producer row");
        let read = account_state::limits_snapshots(&connection).expect("store repository");
        if name == "restore-invalid-default-valid-secondary" {
            assert_eq!(read.diagnostics.len(), 1, "default diagnostic for {name}");
            assert_eq!(read.diagnostics[0].account_key, "default");
        } else {
            assert!(
                read.diagnostics.is_empty(),
                "workspace diagnostic for {name}"
            );
        }
        let read_diagnostic_accounts = read
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.account_key.clone())
            .collect::<Vec<_>>();
        let state = AccountPolicyState::from_store(Some(REGISTRY), read)
            .unwrap_or_else(|error| panic!("service rejected real limits state {name}: {error}"));
        let actual = state.limits(account_key);
        if case["python"].is_null() {
            assert!(actual["data"].is_null(), "Python rejected cache {name}");
            assert!(
                !state.limits_diagnostics().is_empty(),
                "rejection diagnostic for {name}"
            );
        } else {
            assert_eq!(
                actual, case["python"],
                "limits response projection differs for {name}"
            );
        }
        let actual_map = rust_restored_map(&state, &workspace);
        let expected_map = &case["pythonRestored"];
        if name == "restore-invalid-default-valid-secondary" {
            assert_eq!(read_diagnostic_accounts, ["default"]);
        } else {
            assert!(read_diagnostic_accounts.is_empty());
        }
        assert_eq!(
            actual_map, *expected_map,
            "whole restored map differs for {name}"
        );
    }
    let recovery = &corpus["usageResumes"];
    let connection = Connection::open_in_memory().expect("SQLite memory database");
    connection.execute_batch("CREATE TABLE runtime_usage_resumes(id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL)").expect("Python writer table");
    for state in recovery["states"].as_array().expect("writer states") {
        connection
            .execute(
                "INSERT INTO runtime_usage_resumes VALUES(?1,?2,?3)",
                [
                    state["id"].as_str().expect("id"),
                    state["agent"].as_str().expect("agent"),
                    state["record"].as_str().expect("record"),
                ],
            )
            .expect("insert Python usage_resume_save row");
    }
    let waiting = account_state::authentication_recovery_account_keys(
        &connection,
        recovery["now"].as_f64().expect("injected clock"),
    )
    .expect("read recovery keys");
    assert_eq!(waiting, vec!["default"]);
    assert_eq!(
        recovery["states"].as_array().expect("resume states").len(),
        4,
        "producer covers old, recent, non-auth and cancelled states"
    );
    assert!(
        account_cases.len() >= 20,
        "account writer branch corpus incomplete"
    );
    assert!(
        limit_cases.len() >= 5,
        "limits persistence branch corpus incomplete"
    );
}

fn rust_restored_map(state: &AccountPolicyState, payload: &Value) -> Value {
    let mut result = serde_json::Map::new();
    let map = payload["value"]["rateLimitsByAccount"].as_object();
    let mut keys = map
        .map(|map| map.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    if map.is_none_or(|map| !map.contains_key("default"))
        && payload["value"]["rateLimits"].is_object()
    {
        keys.push("default".to_owned());
    }
    for key in keys {
        let snapshot = state.limits(&key);
        if snapshot.get("data").is_some_and(|data| !data.is_null()) {
            result.insert(key, snapshot);
        }
    }
    Value::Object(result)
}

fn server_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src")
}

fn python_executable() -> &'static Path {
    static PYTHON: OnceLock<PathBuf> = OnceLock::new();
    PYTHON
        .get_or_init(|| prepared_python_support::prepared_python(&server_source()))
        .as_path()
}
