//! Deterministic interleavings between host preparation and a project writer.
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_services::{host_project_runtime::PreparedHost, projects::ProjectsError};
use studio_runtime_store::{AgentRecord, AgentRepository, ProjectsRepository, RuntimeStore};

async fn fixture() -> (tempfile::TempDir, Arc<RuntimeStore>) {
    let root = tempfile::tempdir().expect("state");
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
    let cwd = root.path().to_string_lossy().into_owned();
    store
        .transaction(move |tx| {
            for id in ["lead", "other"] {
                AgentRepository::put(
                    tx,
                    &AgentRecord::from_value(json!({
                        "id":id, "rootId":id, "epoch":0, "status":"idle", "cwd":cwd,
                        "isLead":true, "autoWake":true,
                    }))?,
                )?;
            }
            ProjectsRepository::put_project(
                tx,
                &json!({
                    "id":"project", "path":cwd, "name":"Before", "created":0,
                }),
            )
        })
        .await
        .expect("seed");
    (root, store)
}

async fn write_prepared(store: &RuntimeStore, prepared: PreparedHost) -> Result<(), ProjectsError> {
    // This is the host preparation contract used by project writers: recheck
    // inside the writer, then update the current project row.
    store
        .transaction_with(move |tx| {
            prepared.recheck(tx)?;
            let mut project = ProjectsRepository::get(tx, "project")?.expect("project").1;
            project["name"] = json!("After");
            ProjectsRepository::put_project(tx, &project)?;
            Ok(())
        })
        .await
}

async fn mutate(store: &RuntimeStore, patch: Value) {
    store
        .transaction(move |tx| {
            let mut agent = AgentRepository::get(tx, &"other".into())?
                .expect("agent")
                .value();
            for (key, value) in patch.as_object().expect("patch") {
                agent[key] = value.clone();
            }
            AgentRepository::put(tx, &AgentRecord::from_value(agent)?)
        })
        .await
        .expect("intervening agent write");
}

#[tokio::test]
async fn project_write_survives_other_agents_execution_churn() {
    let (_root, store) = fixture().await;
    for status in ["starting", "running", "waiting", "interrupted", "idle"] {
        let prepared = PreparedHost::load(&store).await.expect("prepare");
        // A different task commits while the operation is outside the writer.
        let concurrent_store = store.clone();
        tokio::spawn(async move {
            mutate(
                &concurrent_store,
                json!({
                    "status":status, "epoch":7, "autoWake":false,
                    "startAttempt":{"id":status,"epoch":7,"connectionGeneration":9}
                }),
            )
            .await;
        })
        .await
        .expect("agent task");
        write_prepared(&store, prepared)
            .await
            .expect("project write");
    }
    let db = store.test_reader().expect("reader");
    assert_eq!(
        ProjectsRepository::get(&db, "project")
            .expect("read")
            .expect("project")
            .1["name"],
        "After"
    );
}

#[tokio::test]
async fn project_write_refuses_changed_membership_paths_and_liveness() {
    for patch in [
        json!({"path":"/changed"}),
        json!({"cwd":"/changed"}),
        json!({"deletedAt":1}),
        json!({"archivedAt":1}),
        json!({"archived":true}),
        json!({"status":"archived"}),
    ] {
        let (_root, store) = fixture().await;
        let prepared = PreparedHost::load(&store).await.expect("prepare");
        mutate(&store, patch.clone()).await;
        assert!(
            matches!(
                write_prepared(&store, prepared).await,
                Err(ProjectsError::Conflict(_))
            ),
            "{patch}"
        );
        let db = store.test_reader().expect("reader");
        assert_eq!(
            ProjectsRepository::get(&db, "project")
                .expect("read")
                .expect("project")
                .1["name"],
            "Before"
        );
    }
    for sql in [
        "DELETE FROM runtime_agents WHERE id='other'",
        "INSERT INTO runtime_agents(id,record) SELECT 'new', json_set(record,'$.id','new','$.rootId','new') FROM runtime_agents WHERE id='other'",
        "UPDATE runtime_projects SET record=json_set(record,'$.name','Concurrent')",
        "DELETE FROM runtime_projects",
    ] {
        let (_root, store) = fixture().await;
        let prepared = PreparedHost::load(&store).await.expect("prepare");
        store
            .transaction(move |tx| {
                tx.execute(sql, [])?;
                Ok(())
            })
            .await
            .expect("conflict");
        assert!(
            matches!(
                write_prepared(&store, prepared).await,
                Err(ProjectsError::Conflict(_))
            ),
            "{sql}"
        );
    }
}
