use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

async fn fixture() -> (tempfile::TempDir, Arc<RuntimeStore>) {
    let directory = tempfile::Builder::new()
        .prefix("image-workspace-agents-")
        .tempdir()
        .expect("state root creates");
    let schema: Value = serde_json::from_str(include_str!(
        "../../../runtime-store/src/runtime-state-schema.json"
    ))
    .expect("runtime schema fixture parses");
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        let connection =
            rusqlite::Connection::open(directory.path().join(name)).expect("fixture DB opens");
        connection
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
            .expect("fixture pragmas apply");
        let mut statements = schema[name]["sqlite_master"]
            .as_array()
            .expect("schema entries")
            .iter()
            .filter_map(|entry| {
                Some((
                    entry["type"].as_str()?,
                    entry["name"].as_str()?,
                    entry["sql"].as_str()?,
                ))
            })
            .collect::<Vec<_>>();
        statements.sort_by_key(|(kind, _, _)| match *kind {
            "table" => 0,
            "index" => 1,
            "view" => 2,
            _ => 3,
        });
        for (_, object_name, sql) in statements {
            // A virtual table creates its shadow tables itself.
            let exists: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
                    [object_name],
                    |row| row.get(0),
                )
                .expect("schema entry lookup");
            if !exists {
                connection.execute_batch(sql).expect("schema installs");
            }
        }
    }
    let store = RuntimeStore::open(Some(directory.path()))
        .await
        .expect("fixture store opens");
    (directory, Arc::new(store))
}

fn agent(id: &str, fields: Value) -> Value {
    let mut record = json!({
        "id":id,"rootId":id,"name":id,"epoch":3,"turnEpoch":3,"status":"idle",
        "autoWake":true,"created":1_760_000_000.0,"cwd":"/source/project/app",
        "imageWorkspaceRepo":"/source/project","imageWorkspaceSubpath":"app",
        "workspaceMode":"image","workspaceBackend":"asif","environment":"host",
    });
    for (key, value) in fields.as_object().expect("fields") {
        record[key] = value.clone();
    }
    record
}

async fn put_agent(store: &RuntimeStore, record: Value) {
    let agent = AgentRecord::from_value(record).expect("agent record parses");
    store
        .transaction(move |tx| AgentRepository::put(tx, &agent))
        .await
        .expect("agent stores");
}

fn stored(store: &RuntimeStore, id: &str) -> Value {
    AgentRepository::get(&store.test_reader().expect("read"), &AgentId::from(id))
        .expect("agent reads")
        .expect("agent exists")
        .value()
}

/// `(id, kind, text)` of every queued event for `agent`, in id order.
fn events(store: &RuntimeStore, agent: &str) -> Vec<(String, String, String)> {
    let db = store.test_reader().expect("read");
    let mut statement = db
        .prepare("SELECT id,kind,text FROM runtime_events WHERE agent=?1 ORDER BY id")
        .expect("event query");
    statement
        .query_map([agent], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("events")
        .collect::<Result<Vec<_>, _>>()
        .expect("event rows")
}

#[tokio::test]
async fn a_mounted_turn_writes_its_path_back_and_sends_the_ready_notice_once() {
    let (_directory, store) = fixture().await;
    put_agent(
        &store,
        agent(
            "mounted",
            json!({"imageWorkspace":true,"imageWorkspaceReady":true,"imageWorkspacePhase":"ready",
                "imageWorkspaceCreatedAt":1_760_000_100.0}),
        ),
    )
    .await;
    async fn write(store: &RuntimeStore) -> Result<Option<Value>, EventWriteError> {
        store
            .transaction_with(|tx| {
                record_mounted(
                    tx,
                    "mounted",
                    Path::new("/mnt/mounted/repo/app"),
                    "/mnt/mounted",
                    1_760_000_200.0,
                )
            })
            .await
    }
    let first = write(&store).await.expect("write-back").expect("agent");
    assert_eq!(first["cwd"], "/mnt/mounted/repo/app");
    assert_eq!(first["imageWorkspaceMount"], "/mnt/mounted");
    assert_eq!(
        first["status"], "queued",
        "the idle agent wakes for its notice"
    );
    assert_eq!(
        first["imageWorkspaceNoticeSent"],
        "image-workspace-ready:mounted"
    );
    assert_eq!(stored(&store, "mounted"), first);
    let expected = "[Studio image workspace ready] Path: /mnt/mounted/repo/app. This is a copy of /source/project taken at 2025-10-09T08:55:00Z, including uncommitted changes. Write access is enabled.";
    assert_eq!(
        events(&store, "mounted"),
        vec![(
            "image-workspace-ready:mounted".to_owned(),
            "followup".to_owned(),
            expected.to_owned()
        )]
    );
    let second = write(&store).await.expect("repeat").expect("agent");
    assert_eq!(second, first, "a repeated turn changes nothing");
    assert_eq!(
        events(&store, "mounted").len(),
        1,
        "the notice goes out once"
    );
}

static BASE_CALLS: AtomicUsize = AtomicUsize::new(0);
static COPY_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

fn base_after_one_check(_: &Path) -> Result<Value, ImageWorkspaceError> {
    Ok(if BASE_CALLS.fetch_add(1, Ordering::SeqCst) == 0 {
        json!({"state":"building","version":"v-1","error":null})
    } else {
        json!({"state":"ready","version":"v-1","error":null})
    })
}

fn create_copy(_: &Path, agent_id: &str) -> Result<Value, ImageWorkspaceError> {
    let root = COPY_ROOT
        .lock()
        .expect("copy root")
        .clone()
        .expect("copy root set");
    let mount = root.join(agent_id);
    std::fs::create_dir_all(mount.join("repo/app"))?;
    Ok(json!({"mount":mount,"path":mount.join("repo")}))
}

fn never_remove(agent_id: &str) {
    panic!("the ready copy of {agent_id} must stay");
}

#[tokio::test]
async fn a_read_only_agent_gets_its_copy_once_the_base_is_ready() {
    let (directory, store) = fixture().await;
    *COPY_ROOT.lock().expect("copy root") = Some(directory.path().join("mnt"));
    put_agent(
        &store,
        agent(
            "reader",
            json!({"imageWorkspace":true,"imageWorkspaceReady":false,"imageWorkspacePhase":"read_only",
                "branch":"main"}),
        ),
    )
    .await;
    assert_eq!(
        read_only_agents(&store.test_reader().expect("test reader")).expect("scan"),
        vec!["reader"]
    );
    let steps = Steps {
        base: base_after_one_check,
        create: create_copy,
        remove: never_remove,
        poll: Duration::from_millis(1),
    };
    let Completion::Ready(ready) = complete(&store, "reader", steps).await.expect("completion")
    else {
        panic!("the copy becomes ready");
    };
    assert_eq!(
        BASE_CALLS.load(Ordering::SeqCst),
        2,
        "the base is polled until ready"
    );
    let mount = directory.path().join("mnt/reader");
    let cwd = mount.join("repo/app");
    assert_eq!(ready["cwd"], json!(cwd));
    assert_eq!(ready["imageWorkspaceMount"], json!(mount));
    assert_eq!(ready["imageWorkspaceReady"], true);
    assert_eq!(ready["imageWorkspacePhase"], "ready");
    assert_eq!(ready["imageWorkspaceBaseState"], "ready");
    assert_eq!(
        ready["status"], "queued",
        "the idle agent wakes for its notice"
    );
    assert!(ready["branch"].is_null());
    assert!(ready["imageWorkspaceCreatedAt"].as_f64().is_some());
    assert_eq!(stored(&store, "reader"), ready);
    let queued = events(&store, "reader");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].0, "image-workspace-ready:reader");
    assert!(queued[0].2.starts_with(&format!(
        "[Studio image workspace ready] Path: {}. This is a copy of /source/project taken at ",
        cwd.display()
    )));
    assert!(
        read_only_agents(&store.test_reader().expect("test reader"))
            .expect("scan")
            .is_empty()
    );
    assert_eq!(
        complete(&store, "reader", steps).await.expect("repeat"),
        Completion::Superseded,
        "a finished agent is not completed twice"
    );
}

fn failed_base(_: &Path) -> Result<Value, ImageWorkspaceError> {
    Ok(json!({"state":"failed","version":null,"error":"base clone failed"}))
}

fn unreachable_create(_: &Path, _: &str) -> Result<Value, ImageWorkspaceError> {
    panic!("no copy without a base");
}

#[tokio::test]
async fn an_explicit_image_request_holds_when_its_base_fails_and_tells_the_lead() {
    let (_directory, store) = fixture().await;
    put_agent(&store, agent("lead", json!({}))).await;
    put_agent(
        &store,
        agent(
            "explicit",
            json!({"imageWorkspace":true,"imageWorkspacePhase":"read_only","parentId":"lead",
                "rootId":"lead","workspaceModeExplicit":true,"worktree":true}),
        ),
    )
    .await;
    let steps = Steps {
        base: failed_base,
        create: unreachable_create,
        remove: never_remove,
        poll: Duration::from_millis(1),
    };
    let Completion::Failed(held) = complete(&store, "explicit", steps)
        .await
        .expect("completion")
    else {
        panic!("the request fails");
    };
    let notice = "[Studio workspace error] The requested image workspace failed: base clone failed. The worker remains read-only in the selected folder. Studio will not switch it to a worktree or shared folder.";
    assert_eq!(held["imageWorkspacePhase"], "failed");
    assert_eq!(held["imageWorkspaceBaseState"], "failed");
    assert_eq!(held["imageWorkspaceError"], "base clone failed");
    assert_eq!(held["imageWorkspace"], true);
    assert_eq!(held["workspaceMode"], "image");
    assert_eq!(held["worktree"], false);
    assert_eq!(held["status"], "paused");
    assert_eq!(held["autoWake"], false);
    assert_eq!(held["error"], notice);
    assert_eq!(
        held["imageWorkspaceNoticeSent"],
        "image-workspace-error:explicit"
    );
    let lead = events(&store, "lead");
    assert_eq!(lead.len(), 1);
    assert_eq!(lead[0].0, "child:explicit:image-workspace-error");
    assert_eq!(lead[0].1, "child_result");
    let payload: Value = serde_json::from_str(&lead[0].2).expect("payload");
    assert_eq!(payload["agent_id"], "explicit");
    assert_eq!(payload["result"], notice);
}

static REMOVED: AtomicUsize = AtomicUsize::new(0);

fn ready_base(_: &Path) -> Result<Value, ImageWorkspaceError> {
    Ok(json!({"state":"ready","version":"v-1","error":null}))
}

fn failed_copy(_: &Path, _: &str) -> Result<Value, ImageWorkspaceError> {
    Err(ImageWorkspaceError::Backend("clone failed".to_owned()))
}

fn count_remove(_: &str) {
    REMOVED.fetch_add(1, Ordering::SeqCst);
}

#[tokio::test]
async fn an_implicit_image_request_falls_back_to_a_worktree_when_setup_fails() {
    let (_directory, store) = fixture().await;
    put_agent(
        &store,
        agent(
            "implicit",
            json!({"imageWorkspace":true,"imageWorkspacePhase":"read_only",
                "imageWorkspaceHasGit":true,"worktree":false}),
        ),
    )
    .await;
    let steps = Steps {
        base: ready_base,
        create: failed_copy,
        remove: count_remove,
        poll: Duration::from_millis(1),
    };
    let Completion::Failed(fallback) = complete(&store, "implicit", steps)
        .await
        .expect("completion")
    else {
        panic!("the copy fails");
    };
    assert_eq!(
        REMOVED.load(Ordering::SeqCst),
        1,
        "the failed copy is removed"
    );
    assert_eq!(fallback["imageWorkspace"], false);
    assert_eq!(fallback["imageWorkspacePhase"], "fallback");
    assert_eq!(fallback["environment"], "host");
    assert!(fallback["workspaceBackend"].is_null());
    assert_eq!(fallback["workspaceMode"], "worktree");
    assert_eq!(fallback["worktree"], true);
    assert_eq!(
        fallback["status"], "queued",
        "the idle agent wakes for its notice"
    );
    assert_eq!(fallback["autoWake"], true);
    assert_eq!(
        events(&store, "implicit"),
        vec![(
            "image-workspace-fallback:implicit".to_owned(),
            "followup".to_owned(),
            "[Studio workspace fallback] Image workspace setup failed: Workspace backend failed: clone failed. Studio will create a Git worktree.".to_owned()
        )]
    );
    let actor: crate::workspace_ready::WorkspaceActor =
        serde_json::from_value(fallback).expect("actor");
    assert!(
        !actor.image_bound(),
        "the fallback agent prepares a worktree, not an image"
    );
}

#[tokio::test]
async fn startup_resume_selects_only_live_read_only_image_agents() {
    let (_directory, store) = fixture().await;
    let phase = |phase: &str| json!({"imageWorkspace":true,"imageWorkspacePhase":phase});
    put_agent(&store, agent("a-read-only", phase("read_only"))).await;
    put_agent(&store, agent("b-creating", phase("creating"))).await;
    let mut deleted = agent("c-deleted", phase("read_only"));
    deleted["deletedAt"] = json!(1_760_000_300.0);
    put_agent(&store, deleted).await;
    let mut ready = agent("d-ready", phase("read_only"));
    ready["imageWorkspaceReady"] = json!(true);
    put_agent(&store, ready).await;
    put_agent(
        &store,
        agent(
            "e-fallback",
            json!({"imageWorkspace":false,"imageWorkspacePhase":"fallback"}),
        ),
    )
    .await;
    assert_eq!(
        read_only_agents(&store.test_reader().expect("test reader")).expect("scan"),
        vec!["a-read-only"]
    );
}

fn ready_copy(_: &Path, agent_id: &str) -> Result<Value, ImageWorkspaceError> {
    let root = std::env::temp_dir().join(format!("image-workspace-agents-wake-{agent_id}"));
    std::fs::create_dir_all(root.join("repo/app"))?;
    Ok(json!({"mount":root,"path":root.join("repo")}))
}

#[tokio::test]
async fn a_background_completion_wakes_the_scheduler_for_its_notice() {
    let (_directory, store) = fixture().await;
    let id = format!("waker-{}", std::process::id());
    put_agent(
        &store,
        agent(
            &id,
            json!({"imageWorkspace":true,"imageWorkspacePhase":"read_only"}),
        ),
    )
    .await;
    let wake = Arc::new(tokio::sync::Notify::new());
    install_wake(Arc::clone(&wake));
    start_with(
        &store,
        &id,
        Steps {
            base: ready_base,
            create: ready_copy,
            remove: never_remove,
            poll: Duration::from_millis(1),
        },
    );
    tokio::time::timeout(Duration::from_secs(10), wake.notified())
        .await
        .expect("the scheduler is woken after the ready write");
    assert_eq!(stored(&store, &id)["imageWorkspacePhase"], "ready");
    assert_eq!(
        events(&store, &id)[0].0,
        format!("image-workspace-ready:{id}")
    );
    let _cleanup = std::fs::remove_dir_all(
        std::env::temp_dir().join(format!("image-workspace-agents-wake-{id}")),
    );
}
