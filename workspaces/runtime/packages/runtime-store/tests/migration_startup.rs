use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};
use studio_runtime_schema::studio_errors::{
    MigrationPhase, MigrationRollbackOutcome, MigrationTarget,
};
use studio_runtime_store::{RuntimeStore, StoreError};

#[tokio::test]
async fn sync_schema_is_ready_before_model_preferences_migrate() {
    let state = tempfile::tempdir().expect("state");
    let store = RuntimeStore::open(Some(state.path())).await.expect("fresh");
    store.transaction(|tx| {
        studio_runtime_store::SyncDocumentsRepository::put_preferences(tx, "server", r#"{"collection":"uiPreferences","id":"server","value":{"fields":{"[\"studio-model-whitelist-v1\"]":{"value":true,"timestamp":1,"writer":"test"}}}}"#)?;
        Ok(())
    }).await.expect("legacy preferences");
    drop(store);
    let db = Connection::open(state.path().join("canvas.sqlite3")).expect("canvas");
    db.execute_batch(
        "DROP TABLE durable_rows; DELETE FROM sync_entity_meta WHERE key='migration_catalog';",
    )
    .expect("older completed schema");
    drop(db);
    drop(
        RuntimeStore::open(Some(state.path()))
            .await
            .expect("schema is prepared before model migration"),
    );
}

#[tokio::test]
async fn migration_startup_upgrades_registry_before_store_is_available() {
    let state = tempfile::tempdir().expect("state");
    let store = RuntimeStore::open(Some(state.path())).await.expect("fresh");
    drop(store);
    std::fs::create_dir(state.path().join("accounts")).expect("accounts");
    let path = state.path().join("accounts/registry.json");
    std::fs::write(&path, json!({"version":1,"defaultAccountKey":"default","accounts":{"default":{"id":"default","home":"/fixture","label":"Fixture","source":"fixture","status":"ready"}},"logins":{}}).to_string()).expect("old registry");
    let store = RuntimeStore::open(Some(state.path()))
        .await
        .expect("upgrade");
    let bytes = std::fs::read(&path).expect("registry");
    let value: Value = serde_json::from_slice(&bytes).expect("JSON");
    assert_eq!(value["version"], 2);
    assert_eq!(value["accounts"]["default"]["provider"], "codex");
    drop(store);
    let reopened = RuntimeStore::open(Some(state.path()))
        .await
        .expect("restart");
    assert_eq!(std::fs::read(path).expect("registry"), bytes);
    drop(reopened);
}

#[tokio::test]
async fn migration_startup_analytics_failure_rolls_back_the_entire_target() {
    let state = tempfile::tempdir().expect("state");
    let store = RuntimeStore::open(Some(state.path())).await.expect("fresh");
    drop(store);
    let db = Connection::open(state.path().join("analytics.sqlite3")).expect("analytics");
    db.execute_batch("DELETE FROM analytics_meta WHERE key IN ('providerIdentityFormat','migration_catalog'); DROP TABLE analytics_cost_rows; CREATE VIEW analytics_cost_rows AS SELECT 'blocked' AS id;").expect("failure fixture");
    assert!(RuntimeStore::open(Some(state.path())).await.is_err());
    let marker: Option<String> = db
        .query_row(
            "SELECT value FROM analytics_meta WHERE key='providerIdentityFormat'",
            [],
            |row| row.get(0),
        )
        .optional()
        .expect("marker");
    assert_eq!(
        marker, None,
        "provider changes must roll back with aggregate failure"
    );
}

#[tokio::test]
async fn migration_startup_current_registry_is_never_repaired_or_written() {
    let state = tempfile::tempdir().expect("state");
    drop(RuntimeStore::open(Some(state.path())).await.expect("fresh"));
    std::fs::create_dir(state.path().join("accounts")).expect("accounts");
    let registry = state.path().join("accounts/registry.json");
    let bytes = br#"{"version":2,"defaultAccountKey":"default","accounts":{"default":{"id":"default","home":"/fixture","label":"Fixture","source":"fixture","status":"ready"}},"logins":{}}"#;
    std::fs::write(&registry, bytes).expect("malformed current registry");
    let analytics = Connection::open(state.path().join("analytics.sqlite3")).expect("analytics");
    analytics
        .execute(
            "DELETE FROM analytics_meta WHERE key='providerIdentityFormat'",
            [],
        )
        .expect("pending analytics");
    let error = RuntimeStore::open(Some(state.path()))
        .await
        .err()
        .expect("preflight refuses current missing provider");
    assert!(matches!(
        error,
        StoreError::Migration {
            target: MigrationTarget::AccountRegistry,
            phase: MigrationPhase::Validate,
            rollback: MigrationRollbackOutcome::NotRequired,
            ..
        }
    ));
    assert_eq!(std::fs::read(registry).expect("source"), bytes);
    assert_eq!(
        analytics
            .query_row(
                "SELECT count(*) FROM analytics_meta WHERE key='providerIdentityFormat'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .expect("pending target"),
        0,
        "preflight precedes data writes"
    );
}

#[tokio::test]
async fn migration_startup_invalid_later_source_prevents_earlier_data_writes() {
    let state = tempfile::tempdir().expect("state");
    drop(RuntimeStore::open(Some(state.path())).await.expect("fresh"));
    let analytics = Connection::open(state.path().join("analytics.sqlite3")).expect("analytics");
    analytics
        .execute(
            "DELETE FROM analytics_meta WHERE key='providerIdentityFormat'",
            [],
        )
        .expect("old analytics");
    let canvas = Connection::open(state.path().join("canvas.sqlite3")).expect("canvas");
    canvas.execute_batch("UPDATE sync_entities SET payload=json_set(payload,'$.value.rateLimitsByAccount',17) WHERE collection='workspace' AND id='current'").expect("invalid pending source");
    let error = RuntimeStore::open(Some(state.path()))
        .await
        .err()
        .expect("preflight refusal");
    assert!(matches!(
        error,
        StoreError::Migration {
            target: MigrationTarget::Canvas,
            phase: MigrationPhase::Validate,
            rollback: MigrationRollbackOutcome::NotRequired,
            ..
        }
    ));
    assert_eq!(
        analytics
            .query_row(
                "SELECT count(*) FROM analytics_meta WHERE key='providerIdentityFormat'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .expect("marker"),
        0
    );
}

#[tokio::test]
async fn migration_startup_malformed_retired_sources_preserve_pending_analytics() {
    for table in ["wait", "callback"] {
        let state = tempfile::tempdir().expect("state");
        drop(RuntimeStore::open(Some(state.path())).await.expect("fresh"));
        let analytics =
            Connection::open(state.path().join("analytics.sqlite3")).expect("analytics");
        analytics
            .execute(
                "DELETE FROM analytics_meta WHERE key='providerIdentityFormat'",
                [],
            )
            .expect("pending analytics");
        let canvas = Connection::open(state.path().join("canvas.sqlite3")).expect("canvas");
        canvas.execute_batch(match table {
            "wait" => "DROP TABLE runtime_usage_resumes; CREATE TABLE runtime_usage_resumes(id TEXT PRIMARY KEY, agent TEXT NOT NULL,record TEXT NOT NULL); INSERT INTO runtime_usage_resumes(id,agent,record) VALUES('bad','a','not-json')",
            "callback" => "CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL); INSERT INTO runtime_operation_receipts(id,signature,result) VALUES('bad','provider-callback','not-json')",
            _ => unreachable!("fixture source"),
        }).expect("malformed source");
        let error = RuntimeStore::open(Some(state.path()))
            .await
            .err()
            .expect("preflight refusal");
        assert!(matches!(
            error,
            StoreError::Migration {
                phase: MigrationPhase::Validate,
                rollback: MigrationRollbackOutcome::NotRequired,
                ..
            }
        ));
        assert_eq!(
            analytics
                .query_row(
                    "SELECT count(*) FROM analytics_meta WHERE key='providerIdentityFormat'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .expect("marker"),
            0
        );
    }
}

#[test]
fn migration_supervisor_refuses_missing_response_before_schema_writes() {
    use studio_runtime_store::supervisor_journal::{JournalError, SupervisorJournal};
    let state = tempfile::tempdir().expect("state");
    drop(SupervisorJournal::open(state.path()).expect("fresh supervisor"));
    let db = Connection::open(state.path().join("supervisor.sqlite3")).expect("journal");
    db.execute_batch("ALTER TABLE operations DROP COLUMN response")
        .expect("malformed empty journal");
    let before: i64 = db
        .pragma_query_value(None, "schema_version", |row| row.get(0))
        .expect("schema revision");
    let error = SupervisorJournal::open(state.path())
        .err()
        .expect("refused journal");
    assert!(
        matches!(error,JournalError::Migration(source) if matches!(*source,StoreError::Migration{target:MigrationTarget::Supervisor,phase:MigrationPhase::Validate,rollback:MigrationRollbackOutcome::NotRequired,..}))
    );
    assert_eq!(
        db.pragma_query_value(None, "schema_version", |row| row.get::<_, i64>(0))
            .expect("schema revision"),
        before
    );
}

#[tokio::test]
async fn migration_provider_preflight_identifies_actual_analytics_target_and_catalog_id() {
    for marker in [true, false] {
        let state = tempfile::tempdir().expect("state");
        drop(RuntimeStore::open(Some(state.path())).await.expect("fresh"));
        let db = Connection::open(state.path().join("analytics.sqlite3")).expect("analytics");
        if marker {
            db.execute(
                "UPDATE analytics_meta SET value='future' WHERE key='providerIdentityFormat'",
                [],
            )
            .expect("unknown marker");
        } else {
            db.execute("INSERT INTO analytics_agents(id,record) VALUES('invalid','{\"id\":\"invalid\",\"provider\":\"future\"}')",[]).expect("unknown current provider");
        }
        let error = RuntimeStore::open(Some(state.path()))
            .await
            .err()
            .expect("provider refusal");
        assert!(matches!(
            error,
            StoreError::Migration {
                id: "2026-10-26-analytics-provider",
                target: MigrationTarget::Analytics,
                phase: MigrationPhase::Validate,
                ..
            }
        ));
    }
}

fn turn(thread: &str) -> Value {
    json!({"id":format!("a:{thread}:turn"),"agentId":"a","rootId":"a","at":1,"threadId":thread,"turnId":"turn","startedAt":1,"finishedAt":null,"firstOutputAt":null,"status":"running"})
}

#[tokio::test]
async fn migration_terminal_empty_source_is_complete_before_new_turns_arrive() {
    use studio_runtime_store::analytics_capture::{
        AnalyticsCaptureRepository as Repo, CaptureTable,
    };
    let state = tempfile::tempdir().expect("state");
    let store = RuntimeStore::open(Some(state.path())).await.expect("fresh");
    let db = Connection::open(state.path().join("analytics.sqlite3")).expect("analytics");
    let checkpoint: String = db
        .query_row(
            "SELECT value FROM analytics_meta WHERE key='terminalErrorRepairV1'",
            [],
            |row| row.get(0),
        )
        .expect("empty source has a completed checkpoint");
    let completed: Value = serde_json::from_str(&checkpoint).expect("checkpoint JSON");
    assert_eq!(
        completed,
        json!({"cursor":0,"end":0,"repaired":0,"ambiguous":0})
    );
    let record = turn("thread");
    let stored = record.clone();
    store
        .analytics_transaction::<_, StoreError>(move |tx, _| {
            Repo::write(tx, CaptureTable::Turns, &stored)
        })
        .await
        .expect("new turn after migration");
    store
        .transaction(|tx| {
            let notice = json!({"turnId":"turn","threadId":"thread","nativeError":{"message":"later notice"},"at":3});
            tx.execute(
                "INSERT INTO runtime_items(id,agent,record,created) VALUES('a:native-notice:error:turn','a',?1,3)",
                [notice.to_string()],
            )?;
            Ok(())
        })
        .await
        .expect("new notice after migration");
    drop(store);
    for _ in 0..2 {
        drop(
            RuntimeStore::open(Some(state.path()))
                .await
                .expect("restart"),
        );
        assert_eq!(
            Repo::read(&db, CaptureTable::Turns, "a:thread:turn").expect("turn"),
            Some(record.clone()),
            "completed migration must not capture later turns"
        );
        let current: String = db
            .query_row(
                "SELECT value FROM analytics_meta WHERE key='terminalErrorRepairV1'",
                [],
                |row| row.get(0),
            )
            .expect("same checkpoint");
        assert_eq!(current, checkpoint);
    }
}
