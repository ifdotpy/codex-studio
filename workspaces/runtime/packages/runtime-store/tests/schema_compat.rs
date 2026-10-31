use std::path::Path;

use rusqlite::Connection;
use serde_json::{Value, json};
use studio_runtime_schema::studio_errors::{
    MigrationPhase, MigrationRollbackOutcome, MigrationTarget,
};
use studio_runtime_store::{
    AgentId, QueueEvent, QueueEventId, QueueEventRepository, RuntimeStore, StoreError,
};

const LIVE_SCHEMA: &str = include_str!("fixtures/live-state-schema.json");

fn usage_rows(db: &Connection) -> Vec<Value> {
    db.prepare("SELECT seq,id,agent,root,thread,turn,at,record FROM analytics_usage ORDER BY seq")
        .expect("usage snapshot")
        .query_map([], |row| {
            Ok(json!([
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, f64>(6)?,
                row.get::<_, String>(7)?
            ]))
        })
        .expect("usage rows")
        .collect::<Result<_, _>>()
        .expect("usage facts")
}

#[tokio::test]
async fn provider_identity_cutover_never_repairs_current_ownerless_or_invalid_usage() {
    use studio_runtime_store::{
        analytics_capture::{AnalyticsCaptureRepository, CaptureTable},
        cost_aggregate,
    };

    let root = temporary_state();
    let store = RuntimeStore::open(Some(root.path()))
        .await
        .expect("current state");
    for provider in [
        None,
        Some(Value::Null),
        Some(json!("future")),
        Some(json!(7)),
    ] {
        if provider.is_some() {
            store
                .analytics_transaction::<_, StoreError>(|tx, _| {
                    AnalyticsCaptureRepository::write(
                        tx,
                        CaptureTable::Agents,
                        &json!({"id":"current-agent","provider":"codex"}),
                    )
                })
                .await
                .expect("known owner cannot override invalid explicit provenance");
        }
        let mut record = json!({"id":"current-orphan","agentId":"current-agent","rootId":"root","threadId":"thread","at":1,"responseId":"current-response","model":"m","last":{"inputTokens":5,"outputTokens":1},"total":{}});
        if let Some(provider) = provider {
            record["provider"] = provider;
        }
        let copied = record.clone();
        assert!(
            store
                .analytics_transaction::<_, StoreError>(move |tx, _| {
                    AnalyticsCaptureRepository::write(tx, CaptureTable::Usage, &copied)
                })
                .await
                .is_err(),
            "invalid or absent identity cannot become a normal write"
        );
        store.analytics_transaction::<_,StoreError>(move |tx,_| {
            tx.execute("INSERT INTO analytics_usage(id,agent,root,thread,at,record) VALUES('current-orphan','current-agent','root','thread',1,?1)", [record.to_string()])?;
            assert!(cost_aggregate::recorded_responses(tx,&["current-agent".into()]).is_err());
            assert!(AnalyticsCaptureRepository::usage_inventory(tx,"current-agent").is_err());
            assert!(AnalyticsCaptureRepository::usage_provenance(tx,"current-agent",Some("thread"),"current-response").is_err());
            tx.execute("DELETE FROM analytics_usage WHERE id='current-orphan'",[])?;
            Ok(())
        }).await.expect("strict ordinary readers");
    }
    store.analytics_transaction::<_,StoreError>(|tx,_| {
        tx.execute("DELETE FROM analytics_agents WHERE id='current-agent'",[])?;
        tx.execute("INSERT INTO analytics_usage(id,agent,root,thread,at,record) VALUES('late-orphan','current-agent','root','thread',2,?1)",
            [json!({"id":"late-orphan","agentId":"current-agent","rootId":"root","threadId":"thread","at":2,"model":"m","last":{"inputTokens":5,"outputTokens":1},"total":{}}).to_string()])?;
        Ok(())
    }).await.expect("source appearing after cutover");
    let path = root.path().join("analytics.sqlite3");
    let before = usage_rows(&Connection::open(&path).expect("current source"));
    drop(store);
    let reopened = RuntimeStore::open(Some(root.path()))
        .await
        .expect("current format reopens");
    let db = Connection::open(&path).expect("reopened source");
    assert_eq!(
        usage_rows(&db),
        before,
        "dated conversion never runs on current rows"
    );
    assert!(AnalyticsCaptureRepository::usage_inventory(&db, "current-agent").is_err());
    drop(reopened);
}

#[tokio::test]
async fn rust_open_adds_analytics_limits_index_to_existing_schema_without_it() {
    let state_directory = temporary_state();
    let state = state_directory.path();
    create_from_live_schema(state);
    let analytics_path = state.join("analytics.sqlite3");
    let db = Connection::open(&analytics_path).expect("analytics fixture opens");
    db.execute("DROP INDEX analytics_limits_account_at", [])
        .expect("fixture index drops");
    drop(db);

    let store = RuntimeStore::open(Some(state))
        .await
        .expect("Rust opens existing analytics schema");
    drop(store);

    let db = Connection::open(&analytics_path).expect("analytics fixture reopens");
    let exists: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name='analytics_limits_account_at')",
            [],
            |row| row.get(0),
        )
        .expect("index lookup succeeds");
    assert!(
        exists,
        "Rust open creates the missing analytics limits index"
    );
    drop(db);
}

#[tokio::test]
async fn missing_required_objects_are_typed_and_name_the_object() {
    for (sql, expected) in [
        ("DROP TABLE runtime_tasks", "runtime_tasks"),
        (
            "DROP TABLE runtime_agents; CREATE TABLE runtime_agents(id TEXT PRIMARY KEY)",
            "runtime_agents",
        ),
        (
            "DROP TABLE runtime_tasks; CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY)",
            "runtime_tasks",
        ),
    ] {
        let state_directory = temporary_state();
        let state = state_directory.path();
        create_from_live_schema(state);
        Connection::open(state.join("canvas.sqlite3"))
            .expect("database opens")
            .execute_batch(sql)
            .expect("negative case mutates disposable schema");
        let before = catalog(&state.join("canvas.sqlite3"));
        let error = match RuntimeStore::open(Some(state)).await {
            Ok(_) => panic!("incompatible required schema was accepted"),
            Err(error) => error,
        };
        assert!(
            matches!(error, StoreError::SchemaObjectMismatch { ref object, .. } if object == expected),
            "expected typed refusal naming {expected}, got {error:?}"
        );
        if expected == "runtime_agents" {
            assert!(
                matches!(error, StoreError::SchemaObjectMismatch { ref reason, .. } if reason.contains("record")),
                "expected missing record refusal, got {error:?}"
            );
        }
        assert_eq!(before, catalog(&state.join("canvas.sqlite3")));
    }
}

#[tokio::test]
async fn renamed_sync_table_with_equivalent_structure_opens() {
    let state_directory = temporary_state();
    let state = state_directory.path();
    create_from_live_schema(state);
    let canvas = state.join("canvas.sqlite3");
    let initial_store = RuntimeStore::open(Some(state))
        .await
        .expect("fixture prepares on open");
    drop(initial_store);
    let identity = Connection::open(&canvas)
        .expect("fixture DB opens")
        .query_row("SELECT id FROM sync_identity", [], |row| {
            row.get::<_, String>(0)
        })
        .expect("fixture has sync identity");
    Connection::open(&canvas)
        .expect("fixture DB opens")
        .execute_batch(
            "ALTER TABLE sync_identity RENAME TO saved_sync_identity;
             ALTER TABLE saved_sync_identity RENAME TO sync_identity;",
        )
        .expect("rename round trip preserves the table structure");

    let store = RuntimeStore::open(Some(state))
        .await
        .expect("structurally equivalent table opens");
    assert_eq!(
        store
            .test_reader()
            .expect("reader opens")
            .query_row("SELECT id FROM sync_identity", [], |row| row
                .get::<_, String>(0))
            .expect("identity remains readable"),
        identity
    );
    drop(store);
}

#[tokio::test]
async fn missing_automatic_unique_index_is_named_and_left_untouched() {
    let state_directory = temporary_state();
    let state = state_directory.path();
    create_from_live_schema(state);
    let canvas = state.join("canvas.sqlite3");
    Connection::open(&canvas)
        .expect("fixture DB opens")
        .execute_batch(
            "DROP TABLE sync_documents;
             CREATE TABLE sync_documents (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 scope TEXT NOT NULL,
                 id TEXT NOT NULL,
                 payload TEXT NOT NULL,
                 deleted INTEGER NOT NULL DEFAULT 0
             );",
        )
        .expect("table has matching columns but lacks its unique constraint");
    let before = catalog(&canvas);

    let error = match RuntimeStore::open(Some(state)).await {
        Ok(_) => panic!("sync_documents without UNIQUE(scope,id) was accepted"),
        Err(error) => error,
    };
    assert!(
        matches!(&error, StoreError::Migration {
            id: "2026-10-20-agent-storage",
            target: MigrationTarget::Canvas,
            phase: MigrationPhase::Validate,
            rollback: MigrationRollbackOutcome::NotRequired,
            source,
            rollback_error: None,
        } if matches!(source.as_ref(), StoreError::SchemaObjectMismatch { database, object, reason }
            if database == "canvas.sqlite3" && object == "sync_documents" && reason.contains("UNIQUE/PRIMARY KEY"))),
        "constraint error should identify sync_documents, got {error:?}"
    );
    assert_eq!(catalog(&canvas), before);
}

#[tokio::test]
async fn unicode_altered_table_column_is_named_and_left_untouched() {
    let state_directory = temporary_state();
    let state = state_directory.path();
    create_from_live_schema(state);
    let canvas = state.join("canvas.sqlite3");
    let altered_table = format!(
        "DROP TABLE sync_identity; CREATE TABLE sync_identity(id TEXT{}PRIMARY KEY);",
        '\u{00a0}'
    );
    Connection::open(&canvas)
        .expect("fixture DB opens")
        .execute_batch(&altered_table)
        .expect("SQLite accepts the altered type declaration");
    let before = catalog(&canvas);

    let error = match RuntimeStore::open(Some(state)).await {
        Ok(_) => panic!("incompatible sync_identity column is accepted"),
        Err(error) => error,
    };
    assert!(
        matches!(&error, StoreError::Migration {
            id: "2026-10-20-agent-storage",
            target: MigrationTarget::Canvas,
            phase: MigrationPhase::Validate,
            rollback: MigrationRollbackOutcome::NotRequired,
            source,
            rollback_error: None,
        } if matches!(source.as_ref(), StoreError::SchemaObjectMismatch { database, object, reason }
            if database == "canvas.sqlite3" && object == "sync_identity" && reason.contains("column id"))),
        "table error should name sync_identity.id, got {error:?}"
    );
    assert_eq!(catalog(&canvas), before);
}

#[tokio::test]
async fn noncanonical_tombstone_triggers_are_replaced() {
    let state_directory = temporary_state();
    let state = state_directory.path();
    create_from_live_schema(state);
    let trigger = "sync_entity_tombstone_count_update";
    let canonical_store = RuntimeStore::open(Some(state))
        .await
        .expect("canonical schema opens");
    drop(canonical_store);
    let canvas = state.join("canvas.sqlite3");
    let canonical = stored_sql(&canvas, trigger);

    for whitespace_variant in [true, false] {
        let id = if whitespace_variant {
            "whitespace-tombstone"
        } else {
            "unicode-tombstone"
        }
        .to_owned();
        let database = Connection::open(&canvas).expect("fixture DB opens");
        let original = stored_sql(&canvas, trigger);
        let changed = if whitespace_variant {
            original.replace(
                "UPDATE OF deleted,collection",
                "UPDATE OF deleted,   collection",
            )
        } else {
            original.replace(
                "UPDATE OF deleted,collection",
                &format!("UPDATE OF deleted,{}collection", '\u{00a0}'),
            )
        };
        assert_ne!(changed, original, "fixture trigger contains target text");
        database
            .execute_batch(&format!("DROP TRIGGER {trigger}"))
            .expect("original trigger drops");
        database
            .execute_batch(&changed)
            .expect("SQLite accepts the noncanonical trigger");
        drop(database);

        let store = RuntimeStore::open(Some(state))
            .await
            .expect("noncanonical trigger is repaired");
        assert_eq!(stored_sql(&canvas, trigger), canonical);
        let insert_id = id.clone();
        store
            .transaction(move |tx| {
                tx.execute(
                    "INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) VALUES('transcript:test',?1,1,'h','{}',1)",
                    [insert_id.as_str()],
                )?;
                tx.execute(
                    "UPDATE sync_entities SET collection='event' WHERE collection='transcript:test' AND id=?1",
                    [insert_id.as_str()],
                )?;
                Ok(())
            }).await
            .expect("repaired trigger handles collection-only update");
        assert_eq!(
            store
                .test_reader()
                .expect("reader opens")
                .query_row(
                    "SELECT value FROM sync_entity_meta WHERE key='entity_tombstone_count'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("tombstone count reads"),
            "1"
        );
        let delete_id = id.clone();
        store
            .transaction(move |tx| {
                tx.execute(
                    "DELETE FROM sync_entities WHERE id=?1",
                    [delete_id.as_str()],
                )?;
                Ok(())
            })
            .await
            .expect("variant tombstone is removed before the next case");
    }
}

fn drop_sync_family(path: &Path) {
    let db = Connection::open(path).expect("fixture DB opens");
    let mut statement = db
        .prepare(
            "SELECT type,name FROM sqlite_master WHERE name LIKE 'sync_%' OR name IN ('runtime_event_created_id','runtime_transcript_revisions') ORDER BY CASE type WHEN 'trigger' THEN 0 WHEN 'index' THEN 1 WHEN 'table' THEN 2 ELSE 3 END",
        )
        .expect("sync objects query prepares");
    let objects = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("sync objects query executes")
        .collect::<Result<Vec<_>, _>>()
        .expect("sync objects read");
    drop(statement);
    for (kind, name) in objects {
        let quote = name.replace('"', "\"\"");
        db.execute_batch(&format!("DROP {kind} \"{quote}\""))
            .expect("sync family object drops in dependency order");
    }
}

fn sync_seed_rows(path: &Path) -> Vec<String> {
    let db = Connection::open(path).expect("fixture DB opens");
    db.prepare(
        "SELECT json_array('identity',id) FROM sync_identity
         UNION ALL SELECT json_array('generation',id,value) FROM sync_generation
         UNION ALL SELECT json_array('meta',key,value) FROM sync_entity_meta WHERE key != 'migration_catalog'
         UNION ALL SELECT json_array('document',seq,scope,id,payload,deleted) FROM sync_documents
         UNION ALL SELECT json_array('entity',collection,id,seq,hash,payload,deleted) FROM sync_entities
         UNION ALL SELECT json_array('event',id,agent,kind,text,status,created,epoch,turn_id,error) FROM runtime_events
         ORDER BY 1",
    ).expect("sync snapshot prepares")
        .query_map([], |row| row.get(0)).expect("sync snapshot reads")
        .collect::<Result<Vec<_>, _>>().expect("sync snapshot rows")
}

#[tokio::test]
async fn current_event_windows_remain_atomic_when_reopened_by_rust() {
    let state_directory = temporary_state();
    let state = state_directory.path();
    create_from_live_schema(state);
    drop(
        RuntimeStore::open(Some(state))
            .await
            .expect("initial store opens"),
    );
    let store = RuntimeStore::open(Some(state))
        .await
        .expect("Rust reopens the current event-window schema");
    let window = include_str!("../src/sync_schema.sql")
        .lines()
        .find_map(|line| line.strip_prefix("-- event-window-size: "))
        .and_then(|size| size.parse::<usize>().ok())
        .expect("generated SQL identifies codex_entity_contracts.EVENT_WINDOW");
    store
        .transaction(move |tx| {
            for index in 0..window + 2 {
                QueueEventRepository::put(
                    tx,
                    &QueueEvent {
                        id: QueueEventId::from(format!("portable-{index:04}")),
                        agent: AgentId::from("fixture-agent"),
                        kind: "user".into(),
                        text: "fixture".into(),
                        status: "delivered".into(),
                        created: index as f64,
                        epoch: 0,
                        turn_id: None,
                        error: None,
                    },
                )?;
            }
            let live: i64 = tx.query_row(
                "SELECT COUNT(*) FROM sync_entities WHERE collection='event' AND deleted=0",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(
                live,
                i64::try_from(window).expect("event window fits SQLite count"),
                "the source writer commits an exact bounded window"
            );
            let distinct: i64 = tx.query_row(
                "SELECT COUNT(DISTINCT seq) FROM sync_entities WHERE collection='event'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(
                distinct,
                i64::try_from(window + 2).expect("event fixture count fits SQLite count"),
                "every event and displacement has its own sequence"
            );
            Ok(())
        })
        .await
        .expect("Rust event writes need only its existing portable projection functions");
    let before = sync_seed_rows(&state.join("canvas.sqlite3"));
    let rollback: Result<(), StoreError> = store
        .transaction(move |tx| {
            tx.execute("DELETE FROM runtime_events WHERE id='portable-0002'", [])?;
            Err(StoreError::SchemaMismatch(
                "intentional fixture rollback".into(),
            ))
        })
        .await;
    assert!(rollback.is_err());
    let reader = store.test_reader().expect("reader opens after rollback");
    assert_eq!(
        before,
        sync_seed_rows(&state.join("canvas.sqlite3")),
        "rollback preserves source, window, and checkpoint together"
    );
    let obsolete: i64 = reader
        .query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name GLOB 'sync_entity_event_window_v2_*'", [], |row| row.get(0))
        .expect("obsolete trigger count reads");
    assert_eq!(obsolete, 0);
    drop(reader);
    drop(store);
}

#[tokio::test]
async fn incompatible_sync_table_is_named_and_left_untouched() {
    let state_directory = temporary_state();
    let state = state_directory.path();
    create_from_live_schema(state);
    let canvas = state.join("canvas.sqlite3");
    let db = Connection::open(&canvas).expect("fixture DB opens");
    db.execute_batch(
        "DROP TABLE sync_entities;
         CREATE TABLE sync_entities(collection TEXT NOT NULL, id TEXT NOT NULL, PRIMARY KEY(collection,id));",
    )
    .expect("incompatible table fixture is created");
    drop(db);
    let before = catalog(&canvas);
    let error = match RuntimeStore::open(Some(state)).await {
        Ok(_) => panic!("incompatible same-name table is accepted during open"),
        Err(error) => error,
    };
    assert!(
        matches!(&error, StoreError::Migration {
            id: "2026-10-20-agent-storage",
            target: MigrationTarget::Canvas,
            phase: MigrationPhase::Validate,
            rollback: MigrationRollbackOutcome::NotRequired,
            source,
            rollback_error: None,
        } if matches!(source.as_ref(), StoreError::SchemaObjectMismatch { database, object, .. }
            if database == "canvas.sqlite3" && object == "sync_entities")),
        "bootstrap should name sync_entities, got {error:?}"
    );
    assert_eq!(catalog(&canvas), before);
}

#[tokio::test]
async fn sync_bootstrap_failure_rolls_back_earlier_ddl_and_seed_rows() {
    let state_directory = temporary_state();
    let state = state_directory.path();
    create_from_live_schema(state);
    let canvas = state.join("canvas.sqlite3");
    drop_sync_family(&canvas);
    let db = Connection::open(&canvas).expect("fixture DB opens");
    db.execute_batch(
        "CREATE TABLE sync_identity (id TEXT PRIMARY KEY);
         CREATE TRIGGER reject_sync_identity BEFORE INSERT ON sync_identity BEGIN SELECT RAISE(ABORT,'fixture failure'); END;",
    )
    .expect("failing seed trigger is created");
    drop(db);
    let before = catalog(&canvas);
    assert!(
        RuntimeStore::open(Some(state)).await.is_err(),
        "identity seed failure should abort open"
    );
    assert_eq!(catalog(&canvas), before);
}

fn create_from_live_schema(root: &Path) {
    let schema: Value = serde_json::from_str(LIVE_SCHEMA).expect("schema-only fixture parses");
    for database in ["canvas.sqlite3", "analytics.sqlite3"] {
        let mut connection = Connection::open(root.join(database)).expect("database file opens");
        studio_runtime_store::sqlite_durability::configure_writer(&connection)
            .expect("WAL and NORMAL are enabled");
        let mut objects = schema[database]["sqlite_master"]
            .as_array()
            .expect("fixture contains SQLite objects")
            .iter()
            .filter_map(|object| {
                let name = object["name"].as_str()?;
                let sql = object["sql"].as_str()?;
                // FTS5 creates these shadow tables as part of CREATE VIRTUAL TABLE.
                if [
                    "runtime_search_next_config",
                    "runtime_search_next_data",
                    "runtime_search_next_docsize",
                    "runtime_search_next_idx",
                ]
                .contains(&name)
                {
                    return None;
                }
                Some((
                    object["type"].as_str().unwrap_or_default().to_owned(),
                    name.to_owned(),
                    sql.to_owned(),
                ))
            })
            .collect::<Vec<_>>();
        objects.sort_by(|left, right| {
            object_order(&left.0)
                .cmp(&object_order(&right.0))
                .then(left.1.cmp(&right.1))
        });
        let transaction = connection.transaction().expect("fixture DDL transaction");
        for (_, _, sql) in objects {
            transaction.execute_batch(&sql).unwrap_or_else(|error| {
                panic!("live fixture DDL failed for {database}: {error}; SQL={sql}")
            });
        }
        if database == "canvas.sqlite3" {
            transaction
                .execute_batch(include_str!("fixtures/receipt-finish-markers.sql"))
                .expect("completed receipt markers");
        }
        transaction
            .pragma_update(None, "user_version", 0)
            .expect("version matches supplied state");
        transaction.commit().expect("fixture catalog commits");
    }
}

fn object_order(kind: &str) -> u8 {
    match kind {
        "table" => 0,
        "index" => 1,
        "trigger" => 2,
        "view" => 3,
        _ => 4,
    }
}

fn catalog(path: &Path) -> Vec<(String, String, Option<String>)> {
    let db = Connection::open(path).expect("database opens");
    let mut statement = db
        .prepare("SELECT type,name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name")
        .expect("catalog query prepares");
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("catalog query executes")
        .collect::<Result<Vec<_>, _>>()
        .expect("catalog reads")
}

fn stored_sql(path: &Path, name: &str) -> String {
    Connection::open(path)
        .expect("database opens")
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='trigger' AND name=?1",
            [name],
            |row| row.get(0),
        )
        .expect("trigger SQL is stored")
}

fn temporary_state() -> tempfile::TempDir {
    tempfile::tempdir().expect("temporary state directory creates")
}

#[tokio::test]
async fn analytics_marker_projection_is_checked_during_store_open() {
    let root = tempfile::tempdir().expect("isolated state");
    drop(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("fresh store"),
    );
    let analytics = Connection::open(root.path().join("analytics.sqlite3")).expect("analytics");
    analytics
        .execute(
            "INSERT OR REPLACE INTO analytics_meta(key,value) VALUES('fileMigrationV1',?1)",
            [r#"{"phase":"pending","phase":"complete","other":[1,2]}"#],
        )
        .expect("marker");
    drop(analytics);
    drop(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("complete marker"),
    );
    let analytics = Connection::open(root.path().join("analytics.sqlite3")).expect("analytics");
    analytics
        .execute(
            "UPDATE analytics_meta SET value=?1 WHERE key='fileMigrationV1'",
            [r#"{"phase":"complete","phase":null}"#],
        )
        .expect("marker");
    drop(analytics);
    let error = match RuntimeStore::open(Some(root.path())).await {
        Ok(_) => panic!("incomplete marker admitted"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("file migration is not complete"));
}
