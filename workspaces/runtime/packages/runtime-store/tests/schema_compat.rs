use std::path::Path;

use rusqlite::{Connection, params};
use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRepository, ChatMessageId, MessageRepository, QueueEvent, QueueEventId,
    QueueEventRepository, RuntimeStore, StoreError, TaskId, TaskRepository,
};

const LIVE_SCHEMA: &str = include_str!("fixtures/live-state-schema.json");

#[tokio::test]
async fn live_schema_opens_and_supported_repositories_preserve_the_catalog() {
    let state_directory = temporary_state();
    let state = state_directory.path();
    create_from_live_schema(state);
    seed_records(&state.join("canvas.sqlite3"));
    let derived_before_open = derived_catalog(&state.join("canvas.sqlite3"));

    let store = RuntimeStore::open(Some(state))
        .await
        .expect("schema-only live fixture opens");
    let replaced = replaced_derived_objects(
        &derived_before_open,
        &derived_catalog(&state.join("canvas.sqlite3")),
    );
    assert_eq!(
        replaced,
        vec![
            "trigger:sync_entity_event_DELETE".to_owned(),
            "trigger:sync_entity_event_INSERT".to_owned(),
            "trigger:sync_entity_event_UPDATE".to_owned(),
        ],
        "old event triggers upgrade to atomic portable window maintenance"
    );
    let before = catalog(&state.join("canvas.sqlite3"));
    let analytics_before = catalog(&state.join("analytics.sqlite3"));
    let reader = store.test_reader().expect("reader opens");
    assert!(
        AgentRepository::get(&reader, &AgentId::from("fixture-agent"))
            .expect("agent reads")
            .is_some()
    );
    assert!(
        TaskRepository::get(&reader, &TaskId::from("fixture-task"))
            .expect("task reads")
            .is_some()
    );
    assert!(
        QueueEventRepository::get(&reader, &QueueEventId::from("fixture-event"))
            .expect("event reads")
            .is_none()
    );
    assert!(
        MessageRepository::get(&reader, &ChatMessageId::from("fixture-message"))
            .expect("message reads")
            .is_some()
    );
    drop(reader);

    store
        .transaction(move |tx| {
            let mut agent = AgentRepository::get(tx, &AgentId::from("fixture-agent"))?
                .expect("seeded agent exists");
            agent.extra.insert("rustWrite".into(), json!(true));
            AgentRepository::put(tx, &agent)?;

            let mut task = TaskRepository::get(tx, &TaskId::from("fixture-task"))?
                .expect("seeded task exists");
            task.set("status", json!("running"));
            TaskRepository::put(tx, &task)?;

            QueueEventRepository::put(
                tx,
                &QueueEvent {
                    id: QueueEventId::from("fixture-event"),
                    agent: AgentId::from("fixture-agent"),
                    kind: "work_review".into(),
                    text: "{}".into(),
                    status: "running".into(),
                    created: 5.0,
                    epoch: 0,
                    turn_id: None,
                    error: None,
                },
            )?;

            let mut message = MessageRepository::get(tx, &ChatMessageId::from("fixture-message"))?
                .expect("seeded message exists");
            message.deliveries["rustWrite"] = json!(true);
            MessageRepository::put(tx, &message)?;
            Ok(())
        })
        .await
        .expect("all supported repositories write on the live schema");
    let reader = store.test_reader().expect("reader opens after writes");
    assert_eq!(
        QueueEventRepository::get(&reader, &QueueEventId::from("fixture-event"))
            .expect("written event reads")
            .expect("event exists")
            .status,
        "running"
    );
    drop(reader);
    store
        .verify_schema()
        .expect("required schema remains valid");
    drop(store);

    assert_eq!(before, catalog(&state.join("canvas.sqlite3")));
    assert_eq!(analytics_before, catalog(&state.join("analytics.sqlite3")));
    let reopened = RuntimeStore::open(Some(state))
        .await
        .expect("migrated schema remains openable");
    let reader = reopened
        .test_reader()
        .expect("reader opens after Rust writes");
    assert_eq!(
        AgentRepository::get(&reader, &AgentId::from("fixture-agent"))
            .expect("agent reads")
            .expect("agent exists")
            .value()["rustWrite"],
        true
    );
    drop(reader);
    drop(reopened);
}

#[tokio::test]
async fn migration_2026_10_22_drops_python_cost_projection_once() {
    let state_directory = temporary_state();
    let state = state_directory.path();
    create_from_live_schema(state);
    let analytics = state.join("analytics.sqlite3");
    let retired_objects = |path: &Path| {
        catalog(path)
            .into_iter()
            .filter(|(_, name, _)| {
                name.starts_with("analytics_cost_projection_")
                    || name.starts_with("analytics_cost_usage_")
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        retired_objects(&analytics).len(),
        11,
        "fixture has the projection table, index and nine triggers"
    );
    let existing_record = r#"{"id":"existing-usage","agentId":"agent","rootId":"root","at":0.5,"model":"gpt-5","last":{"inputTokens":5,"outputTokens":1}}"#;
    Connection::open(&analytics).expect("analytics fixture opens")
        .execute("INSERT INTO analytics_usage(id,agent,root,at,record) VALUES('existing-usage','agent','root',0.5,?1)", [existing_record])
        .expect("usage fixture runs the original projection triggers");
    let store = RuntimeStore::open(Some(state))
        .await
        .expect("fixture opens");
    assert_eq!(
        Connection::open(&analytics)
            .expect("analytics opens")
            .query_row(
                "SELECT record FROM analytics_usage WHERE id='existing-usage'",
                [],
                |row| row.get::<_, String>(0)
            )
            .expect("original usage survives"),
        existing_record
    );
    assert!(
        retired_objects(&analytics).is_empty(),
        "Python projection objects are removed"
    );
    let record = json!({
        "id": "usage-after-migration", "agentId": "agent", "rootId": "root",
        "threadId": "thread", "turnId": "turn", "at": 1.0, "model": "gpt-5",
        "last": {"inputTokens": 10, "outputTokens": 2, "totalTokens": 12},
        "total": {"inputTokens": 10, "outputTokens": 2, "totalTokens": 12}
    });
    for at in [1.0, 2.0] {
        let mut record = record.clone();
        record["at"] = json!(at);
        store
            .analytics_transaction::<_, StoreError>(move |tx, _| {
                studio_runtime_store::analytics_capture::AnalyticsCaptureRepository::write(
                    tx,
                    studio_runtime_store::analytics_capture::CaptureTable::Usage,
                    &record,
                )
            })
            .await
            .expect("usage insert and update still maintain the Rust aggregate");
    }
    let before = catalog(&analytics);
    let cookie = schema_version(&analytics);
    let aggregate = Connection::open(&analytics)
        .expect("analytics opens")
        .query_row("SELECT count(*) FROM analytics_cost_rows", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("Rust aggregate reads");
    assert_eq!(aggregate, 2, "existing usage backfills alongside new usage");
    drop(store);
    let store = RuntimeStore::open(Some(state))
        .await
        .expect("second open succeeds");
    assert_eq!(
        catalog(&analytics),
        before,
        "second open preserves every schema definition"
    );
    assert_eq!(
        schema_version(&analytics),
        cookie,
        "second open performs no DDL"
    );
    store
        .analytics_transaction::<_, StoreError>(|tx, _| {
            tx.execute(
                "DELETE FROM analytics_usage WHERE id='usage-after-migration'",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("usage deletion has no retired trigger dependency");
    drop(store);
}

#[tokio::test]
async fn cost_projection_migration_rolls_back_if_rust_backfill_fails() {
    let state_directory = temporary_state();
    let state = state_directory.path();
    create_from_live_schema(state);
    let analytics = state.join("analytics.sqlite3");
    Connection::open(&analytics).expect("analytics fixture opens")
        .execute("INSERT INTO analytics_usage(id,agent,root,at,record) VALUES('invalid-cost','agent','root',1.0,?1)",
            [r#"{"last":[]}"#])
        .expect("valid JSON with an invalid token object inserts");
    let before = catalog(&analytics);
    let cookie = schema_version(&analytics);
    assert!(
        RuntimeStore::open(Some(state)).await.is_err(),
        "invalid cost facts refuse open"
    );
    assert_eq!(
        catalog(&analytics),
        before,
        "failed backfill rolls back every projection drop"
    );
    assert_eq!(
        schema_version(&analytics),
        cookie,
        "failed migration commits no DDL"
    );
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
        matches!(error, StoreError::SchemaObjectMismatch { ref object, ref reason, .. }
            if object == "sync_documents" && reason.contains("UNIQUE/PRIMARY KEY")),
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
        matches!(error, StoreError::SchemaObjectMismatch { ref object, ref reason, .. } if object == "sync_identity" && reason.contains("column id")),
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

fn derived_catalog(path: &Path) -> Vec<(String, String, String)> {
    let db = Connection::open(path).expect("fixture DB opens");
    let mut statement = db
        .prepare(
            "SELECT type,name,sql FROM sqlite_master WHERE type IN ('index','trigger') AND (name GLOB 'sync_*' OR name IN ('runtime_event_created_id','runtime_transcript_revisions')) ORDER BY type,name",
        )
        .expect("derived catalog query prepares");
    statement
        .query_map([], |row| {
            let sql: Option<String> = row.get(2)?;
            Ok((row.get(0)?, row.get(1)?, sql.unwrap_or_default()))
        })
        .expect("derived catalog query executes")
        .collect::<Result<Vec<_>, _>>()
        .expect("derived catalog reads")
}

fn replaced_derived_objects(
    before: &[(String, String, String)],
    after: &[(String, String, String)],
) -> Vec<String> {
    before
        .iter()
        .filter_map(|(kind, name, sql)| {
            after
                .iter()
                .find(|(after_kind, after_name, _)| {
                    kind == after_kind && name.eq_ignore_ascii_case(after_name)
                })
                .filter(|(_, _, after_sql)| sql != after_sql)
                .map(|(kind, name, _)| format!("{kind}:{name}"))
        })
        .collect()
}

fn sync_seed_rows(path: &Path) -> Vec<String> {
    let db = Connection::open(path).expect("fixture DB opens");
    db.prepare(
        "SELECT json_array('identity',id) FROM sync_identity
         UNION ALL SELECT json_array('generation',id,value) FROM sync_generation
         UNION ALL SELECT json_array('meta',key,value) FROM sync_entity_meta
         UNION ALL SELECT json_array('document',seq,scope,id,payload,deleted) FROM sync_documents
         UNION ALL SELECT json_array('entity',collection,id,seq,hash,payload,deleted) FROM sync_entities
         UNION ALL SELECT json_array('version',seq,scope,hash,deleted,updated) FROM sync_versions
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
async fn sync_schema_bootstrap_is_idempotent() {
    let rust_state_directory = temporary_state();
    let rust_state = rust_state_directory.path();
    create_from_live_schema(rust_state);
    drop_sync_family(&rust_state.join("canvas.sqlite3"));
    let store = RuntimeStore::open(Some(rust_state))
        .await
        .expect("Rust bootstraps an empty sync family");
    store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO sync_documents(scope,id,payload,deleted) VALUES('kept','doc','{}',0)",
                [],
            )?;
            tx.execute(
                "INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) VALUES('kept','entity',1,'h','{}',0)",
                [],
            )?;
            Ok(())
        }).await
        .expect("sentinel sync rows insert");
    let before_second_raw_catalog = catalog(&rust_state.join("canvas.sqlite3"));
    let before_second_seeds = sync_seed_rows(&rust_state.join("canvas.sqlite3"));
    let before_second_schema_version = schema_version(&rust_state.join("canvas.sqlite3"));
    drop(store);
    let store = RuntimeStore::open(Some(rust_state))
        .await
        .expect("second store open succeeds");
    assert_eq!(
        sync_seed_rows(&rust_state.join("canvas.sqlite3")),
        before_second_seeds
    );
    assert_eq!(
        catalog(&rust_state.join("canvas.sqlite3")),
        before_second_raw_catalog,
        "second open leaves every stored sqlite_master definition byte-identical"
    );
    assert_eq!(
        schema_version(&rust_state.join("canvas.sqlite3")),
        before_second_schema_version,
        "second open does not bump the schema cookie"
    );
    drop(store);
}

#[tokio::test]
async fn sync_bootstrap_repairs_missing_objects_in_the_committed_live_fixture() {
    let state_directory = temporary_state();
    let state = state_directory.path();
    create_from_live_schema(state);
    let canvas = state.join("canvas.sqlite3");
    let db = Connection::open(&canvas).expect("fixture DB opens");
    db.execute_batch(
        "DROP INDEX sync_entities_tombstone_order;
         DROP TABLE sync_versions;
         DROP TRIGGER sync_entity_event_UPDATE;
         CREATE TRIGGER syncXwatchYprotect_rows BEFORE DELETE ON runtime_tasks BEGIN SELECT RAISE(ABORT,'retain task'); END;
         INSERT INTO runtime_tasks(id,record) VALUES('protected-task','{}');
         DELETE FROM sync_entity_meta WHERE key='entity_tombstone_count';
         INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) VALUES('event','seeded-tombstone',11,'h','{}',1);
         INSERT INTO sync_documents(scope,id,payload,deleted) VALUES('kept','existing','{\"x\":1}',0);
         INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) VALUES('kept','existing',9,'kept-hash','{\"x\":1}',0);",
    )
    .expect("fixture loses only lazily created objects");
    drop(db);

    let store = RuntimeStore::open(Some(state))
        .await
        .expect("partially prepared fixture opens");
    let protected_trigger: i64 = store
        .test_reader()
        .expect("reader opens")
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name='syncXwatchYprotect_rows'",
            [],
            |row| row.get(0),
        )
        .expect("unrelated trigger count reads");
    assert_eq!(protected_trigger, 1);
    let blocked_delete = store
        .transaction(move |tx| {
            tx.execute("DELETE FROM runtime_tasks WHERE id='protected-task'", [])?;
            Ok(())
        })
        .await;
    assert!(blocked_delete.is_err(), "unrelated task protection remains");
    store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) VALUES('bootstrapped-event','fixture-agent','test','{}','queued',1.0,0,NULL,NULL)",
                [],
            )?;
            tx.execute(
                "INSERT INTO runtime_agents(id,record) VALUES('revision-agent','{}')",
                [],
            )?;
            Ok(())
        }).await
        .expect("registered sync functions and transcript triggers run on writes");
    let db = store.test_reader().expect("reader opens");
    assert_eq!(
        db.query_row(
            "SELECT value FROM sync_entity_meta WHERE key='entity_tombstone_count'",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("missing tombstone count is seeded from rows"),
        "1"
    );
    assert_eq!(
        db.query_row(
            "SELECT payload FROM sync_documents WHERE id='existing'",
            [],
            |row| row.get::<_, String>(0)
        )
        .expect("existing document remains"),
        "{\"x\":1}"
    );
    assert_eq!(
        db.query_row(
            "SELECT hash FROM sync_entities WHERE id='existing'",
            [],
            |row| row.get::<_, String>(0)
        )
        .expect("existing entity remains"),
        "kept-hash"
    );
    assert!(
        db.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='index' AND name='sync_entities_tombstone_order'",
            [],
            |_| Ok(())
        )
        .is_ok()
    );
    let payload: String = db
        .query_row(
            "SELECT payload FROM sync_entities WHERE collection='event' AND id='bootstrapped-event'",
            [],
            |row| row.get(0),
        )
        .expect("runtime event trigger projects the inserted event");
    assert_eq!(
        serde_json::from_str::<Value>(&payload).expect("event projection is JSON")["id"],
        "bootstrapped-event"
    );
    assert_eq!(
        db.query_row(
            "SELECT revision FROM runtime_transcript_revisions WHERE agent='revision-agent'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("transcript revision trigger records agent change"),
        1
    );
    assert!(
        db.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='sync_versions'",
            [],
            |_| Ok(())
        )
        .is_ok()
    );
    drop(db);
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
        matches!(error, StoreError::SchemaObjectMismatch { ref object, .. } if object == "sync_entities"),
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

fn seed_records(path: &Path) {
    let db = Connection::open(path).expect("canvas opens");
    db.execute(
        "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
        params!["fixture-agent", r#"{"id":"fixture-agent","rootId":"fixture-agent","epoch":0,"status":"idle","futureField":{"x":true}}"#],
    )
    .expect("agent fixture inserts");
    db.execute(
        "INSERT INTO runtime_tasks(id,record) VALUES(?1,?2)",
        params!["fixture-task", r#"{"id":"fixture-task","agent":"fixture-agent","kind":"command","status":"completed","created":1.0}"#],
    )
    .expect("runtime task fixture inserts");
    db.execute(
        "INSERT INTO runtime_work(id,record) VALUES(?1,?2)",
        params!["fixture-work", r#"{"id":"fixture-work","rootId":"fixture-agent","status":"ready","title":"t","created":1.0,"updated":2.0,"version":1,"futureField":true}"#],
    )
    .expect("work-board fixture inserts");
    db.execute(
        "INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES('fixture-message','fixture-room','fixture-agent','hello',1.0,'{}')",
        [],
    )
    .expect("message fixture inserts");
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

fn schema_version(path: &Path) -> i64 {
    Connection::open(path)
        .expect("database opens")
        .pragma_query_value(None, "schema_version", |row| row.get(0))
        .expect("schema cookie reads")
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
