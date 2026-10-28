use super::*;
use std::fs;

async fn store_with(records: &[Value]) -> (tempfile::TempDir, Arc<RuntimeStore>) {
    let directory = tempfile::tempdir().expect("state directory");
    let store = Arc::new(
        RuntimeStore::open(Some(directory.path()))
            .await
            .expect("runtime store opens"),
    );
    let records = records.to_vec();
    store
        .transaction_with(move |tx| -> std::result::Result<(), StoreError> {
            for record in records {
                AgentRepository::put(tx, &AgentRecord::from_value(record)?)?;
            }
            Ok(())
        })
        .await
        .expect("fixture records");
    (directory, store)
}

async fn stored(store: &RuntimeStore, id: &str) -> Value {
    runtime::agent_value(store, id).await.expect("stored chat")
}

fn wt_lead() -> Value {
    json!({"provider":"claude","id":"wt-lead","rootId":"wt-lead","epoch":0,"status":"idle",
        "autoWake":true,"isLead":true,"workspaceMode":"worktree","cwd":"/fixture/project",
        "threadId":"thread-kept"})
}

fn fake_effects(_: &Path, _: &Value, target: &Value) -> Result<Value> {
    let line = if target["isLead"] == true {
        "main".to_owned()
    } else {
        format!(
            "agent-{}",
            studio_runtime_store::sha256_hex(target["id"].as_str().unwrap_or_default().as_bytes())
        )
    };
    Ok(
        json!({"cwd":format!("/store/projects/project/lines/{line}"),"layrProjectId":"project",
        "layrLine":line,"layrStateId":"exact-state","layrReady":true,"layrStoreRoot":"/store"}),
    )
}

fn no_effects(_: &Path, _: &Value, _: &Value) -> Result<Value> {
    panic!("a refused or repeated switch must not run filesystem effects")
}

async fn with_event(store: &RuntimeStore, status: &'static str) {
    store
        .transaction_with(move |tx| -> std::result::Result<(), StoreError> {
            tx.execute(
                "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES('event','wt-lead','child_result','{}',?1,1,0)",
                [status],
            )
            .map_err(StoreError::from)?;
            Ok(())
        })
        .await
        .expect("fixture event");
}

#[tokio::test]
async fn an_uncertain_past_delivery_does_not_block_the_switch_in_place() {
    // The switch keeps the thread, so the event and its unknown outcome stay as they are.
    let (_directory, store) = store_with(&[wt_lead()]).await;
    with_event(&store, "uncertain").await;
    let result = switch_chat_with(Arc::clone(&store), "wt-lead", fake_effects)
        .await
        .expect("an uncertain past delivery is not in flight");
    assert_eq!(result["workspaceMode"], "layr-native");
}

#[tokio::test]
async fn input_still_in_flight_blocks_the_switch() {
    let (_directory, store) = store_with(&[wt_lead()]).await;
    with_event(&store, "dispatching").await;
    let error = switch_chat_with(Arc::clone(&store), "wt-lead", no_effects)
        .await
        .expect_err("input in flight refuses the switch");
    assert!(error.to_string().contains("Finish pending input first"));
}

#[tokio::test]
async fn switching_refuses_during_a_turn_and_keeps_the_wt_record() {
    let mut lead = wt_lead();
    lead["status"] = json!("running");
    lead["inFlight"] = json!(true);
    let (_directory, store) = store_with(&[lead]).await;
    let error = switch_chat_with(Arc::clone(&store), "wt-lead", no_effects)
        .await
        .expect_err("a running turn refuses the switch");
    assert!(
        error
            .to_string()
            .contains("Finish the active or queued turn")
    );
    assert_eq!(stored(&store, "wt-lead").await["workspaceMode"], "worktree");
}

#[tokio::test]
async fn a_leftover_prepare_attempt_does_not_block_an_idle_chat() {
    // prepareAttempt names the last prepare. It stays after the turn ends.
    let mut lead = wt_lead();
    lead["prepareAttempt"] = json!("attempt-from-an-earlier-turn");
    let (_directory, store) = store_with(&[lead]).await;
    let result = switch_chat_with(Arc::clone(&store), "wt-lead", fake_effects)
        .await
        .expect("an idle chat with a past prepare switches");
    assert_eq!(result["workspaceMode"], "layr-native");
}

#[tokio::test]
async fn a_switched_lead_records_layr_fields_keeps_its_thread_and_workers_inherit() {
    let (_directory, store) = store_with(&[wt_lead()]).await;
    let result = switch_chat_with(Arc::clone(&store), "wt-lead", fake_effects)
        .await
        .expect("idle lead switches");
    assert_eq!(result["workspaceMode"], "layr-native");
    assert_eq!(result["layrLine"], "main");
    let lead = stored(&store, "wt-lead").await;
    assert_eq!(lead["workspaceMode"], "layr-native");
    assert_eq!(lead["executionMode"], "native");
    assert_eq!(lead["layrReady"], true);
    assert_eq!(lead["layrProjectId"], "project");
    assert_eq!(lead["layrStateId"], "exact-state");
    assert_eq!(lead["hostProjectPath"], "/fixture/project");
    assert_eq!(lead["cwd"], "/store/projects/project/lines/main");
    assert_eq!(lead["threadId"], "thread-kept");
    assert_eq!(lead["worktree"], false);
    // A lost response is answered by the saved association without another effect.
    assert_eq!(
        switch_chat_with(Arc::clone(&store), "wt-lead", no_effects)
            .await
            .expect("repeat"),
        result
    );
    assert_eq!(
        crate::managed_child_create::default_workspace("implementer", false, &lead),
        "layr-native"
    );
    let mut child = json!({"id":"new-worker","role":"implementer","isLead":false});
    configure_child(&mut child, &lead, &lead);
    assert_eq!(child["workspaceMode"], "layr-native");
    assert_eq!(child["hostProjectPath"], "/fixture/project");
    assert_eq!(child["layrStoreRoot"], "/store");
}

#[tokio::test]
async fn a_worker_switches_only_after_its_parent() {
    let worker = json!({"provider":"codex","id":"wt-worker","rootId":"wt-lead","parentId":"wt-lead",
        "epoch":0,"status":"idle","autoWake":true,"isLead":false,"role":"implementer",
        "workspaceMode":"worktree","worktree":true,"worktreeReady":true,
        "cwd":"/fixture/project-worktrees/wt-worker","threadId":"worker-thread"});
    let (_directory, store) = store_with(&[wt_lead(), worker]).await;
    let error = switch_chat_with(Arc::clone(&store), "wt-worker", no_effects)
        .await
        .expect_err("a WT parent refuses");
    assert!(error.to_string().contains("Switch the parent chat"));
    switch_chat_with(Arc::clone(&store), "wt-lead", fake_effects)
        .await
        .expect("lead switches");
    switch_chat_with(Arc::clone(&store), "wt-worker", fake_effects)
        .await
        .expect("worker switches");
    let worker = stored(&store, "wt-worker").await;
    assert_eq!(worker["workspaceMode"], "layr-native");
    assert_eq!(worker["hostProjectPath"], "/fixture/project");
    assert!(
        worker["layrLine"]
            .as_str()
            .is_some_and(|line| line.starts_with("agent-"))
    );
    assert_eq!(worker["threadId"], "worker-thread");
}

fn git(directory: &Path, args: &[&str]) -> String {
    let mut argv = vec![
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.test",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "init.defaultBranch=main",
    ];
    argv.extend(args);
    let output = crate::bounded_child::run(crate::bounded_child::Request {
        program: std::ffi::OsStr::new("git"),
        git_directory: Some(directory.as_os_str()),
        args: &argv,
        environment: &[],
        input: None,
        stdout_limit: 64 * 1024,
        stderr_limit: 64 * 1024,
        timeout: std::time::Duration::from_secs(60),
    })
    .unwrap_or_else(|error| panic!("git {args:?}: {error:?}"));
    String::from_utf8(output.stdout.bytes).expect("UTF-8 Git output")
}

/// A project with a bare origin and a pushed worker worktree on `codex-agent/wt-worker`.
fn git_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let folder = tempfile::tempdir().expect("fixture folder");
    let base = CanonicalPath::existing(folder.path())
        .map(CanonicalPath::into_path_buf)
        .expect("canonical fixture");
    let (origin, project, tree) = (
        base.join("origin.git"),
        base.join("project"),
        base.join("worktrees/wt-worker"),
    );
    fs::create_dir_all(&project).expect("project");
    git(&base, &["init", "-q", "--bare", "origin.git"]);
    git(&project, &["init", "-q"]);
    fs::write(project.join("code.txt"), "base\n").expect("base file");
    git(&project, &["add", "."]);
    git(&project, &["commit", "-qm", "Base"]);
    git(
        &project,
        &["remote", "add", "origin", origin.to_str().expect("path")],
    );
    git(&project, &["push", "-q", "origin", "main"]);
    git(
        &project,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "codex-agent/wt-worker",
            tree.to_str().expect("path"),
        ],
    );
    fs::write(tree.join("code.txt"), "worker branch\n").expect("worker edit");
    git(&tree, &["commit", "-qam", "Worker"]);
    (folder, project, tree)
}

#[test]
fn uncommitted_or_unpushed_worktrees_refuse_and_pushed_ones_pass() {
    let (_folder, _project, tree) = git_fixture();
    let unpushed = saved_and_pushed(&tree).expect_err("unpushed HEAD refuses");
    assert!(unpushed.to_string().contains("not on a remote branch"));
    git(&tree, &["push", "-q", "origin", "codex-agent/wt-worker"]);
    saved_and_pushed(&tree).expect("committed and pushed work passes");
    fs::write(tree.join("draft.txt"), "unsaved\n").expect("draft");
    let dirty = saved_and_pushed(&tree).expect_err("untracked file refuses");
    assert!(dirty.to_string().contains("uncommitted files"));
}

#[tokio::test]
#[ignore = "requires the vendored Layr host binary and APFS or btrfs"]
async fn real_host_switch_moves_a_lead_to_main_and_a_worker_branch_to_its_line() {
    let binary = std::env::var_os("STUDIO_LAYR_NATIVE_TEST_BINARY")
        .expect("Set STUDIO_LAYR_NATIVE_TEST_BINARY");
    let (_folder, project, tree) = git_fixture();
    git(&tree, &["push", "-q", "origin", "codex-agent/wt-worker"]);
    let mut lead = wt_lead();
    lead["cwd"] = json!(project);
    let worker = json!({"provider":"codex","id":"wt-worker","rootId":"wt-lead","parentId":"wt-lead",
        "epoch":0,"status":"idle","autoWake":true,"isLead":false,"role":"implementer",
        "workspaceMode":"worktree","worktree":true,"worktreeReady":true,"cwd":tree});
    let (directory, store) = store_with(&[lead, worker]).await;
    let state = directory.path();
    fs::create_dir_all(binary_path(state).parent().expect("bin")).expect("bin folder");
    fs::copy(binary, binary_path(state)).expect("managed binary");
    let lead = switch_chat(Arc::clone(&store), "wt-lead")
        .await
        .expect("lead switches");
    let main = PathBuf::from(lead["cwd"].as_str().expect("main"));
    assert_eq!(
        fs::read_to_string(main.join("code.txt")).expect("main"),
        "base\n"
    );
    let worker = switch_chat(Arc::clone(&store), "wt-worker")
        .await
        .expect("worker switches");
    let line = PathBuf::from(worker["cwd"].as_str().expect("line"));
    assert_eq!(
        fs::read_to_string(line.join("code.txt")).expect("line"),
        "worker branch\n"
    );
    assert_eq!(
        switch_chat(Arc::clone(&store), "wt-worker")
            .await
            .expect("repeat"),
        worker
    );
    assert_eq!(
        fs::read_to_string(tree.join("code.txt")).expect("worktree"),
        "worker branch\n",
        "the Git worktree stays unchanged"
    );
}
