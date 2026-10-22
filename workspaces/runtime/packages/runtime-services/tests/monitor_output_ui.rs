#![allow(clippy::unwrap_used)]
use serde_json::{Value, json};
use studio_runtime_services::{
    monitor_policy::MonitorError, monitor_runtime, process_controls, sync_views, task_output,
};
use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore, Transaction};

#[test]
fn monitor_budget_and_task_file_survive_renderer_projections() {
    let budget = json!({"totalSuppressedMatches":1234,"paused":true,"tokens":0.5});
    let mut raw = json!({"id":"monitor","agent":"owner","created":1,"status":"running","outputPatterns":["ready"],"outputWakeBudget":budget});
    raw["outputWakeBudget"]["noticeSent"] = json!(true);
    let projected = sync_views::project_persisted_record("monitor", &raw).unwrap();
    assert_eq!(projected["outputWakeBudget"], budget);
    assert_eq!(projected["outputPatterns"], json!(["ready"]));
    let process = process_controls::project("monitor", &raw, &json!({"id":"owner"}), None);
    assert_eq!(process["outputWakeBudget"], budget);
    for collection in ["task", "taskDetail"] {
        let task = sync_views::project_persisted_record(
            collection,
            &json!({"id":"task","outputFile":"logs/out.txt"}),
        )
        .unwrap();
        assert_eq!(task["outputFile"], "logs/out.txt");
    }
    let process = process_controls::project(
        "claude_task",
        &json!({"id":"task","outputFile":"logs/out.txt"}),
        &json!({"id":"owner"}),
        None,
    );
    assert_eq!(process["outputFile"], "logs/out.txt");
    assert_eq!(process["capabilities"]["readOutput"], true);
}

fn file_fixture(root: &std::path::Path, path: &str) -> (Value, Value) {
    (
        json!({"id":"task","agent":"owner","itemId":"task:abc","outputFile":path}),
        json!({"id":"owner","provider":"claude","cwd":root.to_string_lossy()}),
    )
}
#[test]
fn task_output_is_bounded_and_checks_owner() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("output"),
        vec![b'x'; task_output::MAX_TASK_OUTPUT_BYTES as usize + 100],
    )
    .unwrap();
    let (task, owner) = file_fixture(root.path(), "output");
    let result = task_output::read(&task, &owner, "owner").unwrap();
    assert_eq!(
        result["text"].as_str().unwrap().len(),
        task_output::MAX_TASK_OUTPUT_BYTES as usize
    );
    assert_eq!(result["truncated"], true);
    assert!(task_output::read(&task, &owner, "other").is_err());
    let mut deleted = owner.clone();
    deleted["deletedAt"] = json!(1);
    assert!(task_output::read(&task, &deleted, "owner").is_err());
    std::fs::write(root.path().join("output"), "complete output").unwrap();
    let result = task_output::read(&task, &owner, "owner").unwrap();
    assert_eq!(result["text"], "complete output");
    assert_eq!(result["truncated"], false);
}
#[cfg(unix)]
#[test]
fn task_output_refuses_outside_paths_and_symlink_escapes() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "not for the task").unwrap();
    symlink(outside.path().join("secret"), root.path().join("link")).unwrap();
    symlink(outside.path(), root.path().join("directory")).unwrap();
    for path in [
        outside.path().join("secret").to_string_lossy().into_owned(),
        "../secret".into(),
        "link".into(),
        "directory/secret".into(),
        ".".into(),
    ] {
        let (task, owner) = file_fixture(root.path(), &path);
        assert!(task_output::read(&task, &owner, "owner").is_err(), "{path}");
    }
}

struct Workspace;
impl monitor_runtime::WorkspaceAdmission for Workspace {
    fn check(&self, _: &Transaction<'_>, _: &AgentRecord) -> Result<(), MonitorError> {
        Ok(())
    }
}
#[tokio::test]
async fn confirmed_restart_is_atomic_replay_safe_and_waits_for_exit() {
    let root = tempfile::tempdir().unwrap();
    let store = RuntimeStore::open(Some(root.path())).await.unwrap();
    let cwd = root.path().to_string_lossy().into_owned();
    let first = store.transaction_with(move |tx| {
        AgentRepository::put(tx, &AgentRecord::from_value(json!({"id":"owner","rootId":"owner","epoch":2,"cwd":cwd,"autoWake":true,"yoloMode":true,"status":"idle"}))?)?;
        monitor_runtime::create_in_transaction(tx, &monitor_runtime::MonitorAdmission {
            actor_id:"owner", epoch:2, request_id:"first", request:&json!({"command":"printf test","output_patterns":["test"]}), log_root:"/logs"
        }, &Workspace, 1.0)
    }).await.unwrap();
    let original = first["id"].as_str().unwrap().to_owned();
    let old = original.clone();
    store.transaction(move |tx| {
        tx.execute("UPDATE runtime_monitors SET record=json_set(record,'$.status','running') WHERE id=?", [&old])?;
        Ok(())
    }).await.unwrap();
    let body = json!({"id":original,"agent":"owner","epoch":2,"requestId":"confirmed","accountKey":"default","outputPatterns":["test complete"]});
    let request = body.clone();
    let new = store
        .transaction_with(move |tx| {
            monitor_runtime::restart_in_transaction(tx, &request, &Workspace, "/logs", 2.0)
        })
        .await
        .unwrap();
    assert_eq!(new["command"], "printf test");
    assert_eq!(new["outputPatterns"], json!(["test complete"]));
    let db = store.test_reader().unwrap();
    assert!(!monitor_runtime::restart_ready(&db, &new).unwrap());
    assert!(
        studio_runtime_services::monitor_driver::pending_start_ids(&store)
            .await
            .unwrap()
            .is_empty()
    );
    let request = body.clone();
    let replay = store
        .transaction_with(move |tx| {
            monitor_runtime::restart_in_transaction(tx, &request, &Workspace, "/logs", 3.0)
        })
        .await
        .unwrap();
    assert_eq!(new, replay);
    let mut changed = body;
    changed["outputPatterns"] = json!(["different"]);
    assert!(
        store
            .transaction_with(move |tx| monitor_runtime::restart_in_transaction(
                tx, &changed, &Workspace, "/logs", 4.0
            ))
            .await
            .is_err()
    );
    let old = original.clone();
    store.transaction(move |tx| { tx.execute("UPDATE runtime_monitors SET record=json_set(record,'$.status','cancelled') WHERE id=?", [&old])?; Ok(()) }).await.unwrap();
    assert!(monitor_runtime::restart_ready(&db, &new).unwrap());
    assert_eq!(
        studio_runtime_services::monitor_driver::pending_start_ids(&store)
            .await
            .unwrap(),
        vec![new["id"].as_str().unwrap()]
    );
    let old = original;
    store
        .transaction(move |tx| {
            tx.execute(
                "UPDATE runtime_monitors SET record=json_set(record,'$.status','lost') WHERE id=?",
                [&old],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(!monitor_runtime::restart_ready(&db, &new).unwrap());
}
