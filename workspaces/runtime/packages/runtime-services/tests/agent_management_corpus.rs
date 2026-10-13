use serde_json::{Value, json};
use std::{path::PathBuf, process::Command};
use studio_runtime_services::agent_management::{
    Action, ManagementError, ManagementSnapshot, Records, decide,
};

fn corpus() -> Vec<Value> {
    let python = std::env::var("CODEX_AGENTS_PYTHON").expect("set isolated CODEX_AGENTS_PYTHON");
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/agent_management/oracle.py");
    let output = Command::new(python)
        .arg(path)
        .output()
        .expect("Python oracle starts");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("oracle emits corpus")
}

fn apply(
    mut records: Records,
    writes: &[studio_runtime_services::agent_management::RowWrite],
    cancel: &[String],
) -> Records {
    for write in writes {
        let rows = records.tables.get_mut(&write.table).expect("source table");
        let row = rows
            .iter_mut()
            .find(|row| row["id"] == write.record["id"])
            .expect("existing row");
        *row = write.record.clone();
    }
    if let Some(events) = records.tables.get_mut("runtime_events") {
        for event in events {
            if event["status"] == "pending" && cancel.iter().any(|id| event["agent"] == *id) {
                event["status"] = json!("cancelled");
            }
        }
    }
    records
}

#[test]
fn python_lifecycle_decisions_and_complete_stored_rows_match() {
    let cases = corpus();
    assert!(cases.len() >= 35);
    for case in cases {
        let records: Records = serde_json::from_value(case["records"].clone()).expect("records");
        let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
        let snapshot = ManagementSnapshot::from_records(records.clone())
            .expect("checked Python-written snapshot");
        let actual = match decide(&snapshot, &action, case["now"].as_f64().expect("time")) {
            Ok(decision) => {
                json!({"result": decision.result, "records":apply(records, &decision.writes, &decision.cancel_pending)})
            }
            Err(error) => json!({"error":error.to_string(), "records":records}),
        };
        assert_eq!(actual, case["expected"], "case {}", case["name"]);
    }
}

#[test]
fn malformed_stored_lifecycle_identity_is_located() {
    let base = corpus().remove(0)["records"].clone();
    for (field, value) in [
        ("epoch", json!(-1)),
        ("epoch", json!(true)),
        ("epoch", json!("8")),
        ("autoWake", json!("yes")),
        ("parentId", json!(7)),
        ("inFlight", json!([])),
        ("agentArchive", json!({"epoch":8})),
        ("agentArchive", json!({"epoch":-1,"at":2.0})),
        ("deletedAt", json!("timestamp")),
        ("worktreeReady", json!({})),
    ] {
        let mut raw = base.clone();
        raw["tables"]["runtime_agents"][1][field] = value;
        let records: Records = serde_json::from_value(raw).expect("raw JSON rows");
        assert!(
            matches!(ManagementSnapshot::from_records(records), Err(ManagementError::Stored { table, id }) if table == "runtime_agents" && id == "worker"),
            "{field}"
        );
    }
}

#[test]
fn missing_native_and_workspace_effects_produce_no_decision() {
    let base = corpus().remove(0)["records"].clone();
    for (field, value, dependency) in [
        (
            "threadId",
            json!("native-thread"),
            "account-worker/agent-interrupt",
        ),
        ("inFlight", json!(true), "account-worker/agent-interrupt"),
        (
            "remoteWorker",
            json!({"id":"remote"}),
            "cross-server/agent-management",
        ),
        (
            "capacityRetry",
            json!({"id":"retry"}),
            "usage-policy/agent-stop",
        ),
        (
            "imageWorkspaceReady",
            json!(true),
            "workspace-state/agent-lifecycle",
        ),
        (
            "worktreeReady",
            json!(true),
            "workspace-state/agent-lifecycle",
        ),
    ] {
        let mut raw = base.clone();
        raw["tables"]["runtime_agents"][1][field] = value;
        let snapshot =
            ManagementSnapshot::from_records(serde_json::from_value(raw).expect("records"))
                .expect("snapshot");
        let result = decide(
            &snapshot,
            &Action::Delete {
                target: "lead".to_owned(),
            },
            42.0,
        );
        assert!(
            matches!(result, Err(ManagementError::Unavailable(actual)) if actual == dependency),
            "{field}"
        );
    }
}
