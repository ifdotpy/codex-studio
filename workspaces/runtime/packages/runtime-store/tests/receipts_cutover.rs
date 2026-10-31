use rusqlite::Connection;
use studio_runtime_schema::persistence::*;
use studio_runtime_store::RuntimeStore;

#[tokio::test]
async fn receipt_v2_keeps_legacy_body_and_adds_checked_reference_fields() {
    let root = tempfile::tempdir().expect("state");
    let store = RuntimeStore::open(Some(root.path())).await.expect("open");
    let db = Connection::open(root.path().join("canvas.sqlite3")).expect("database");
    let columns: Vec<String> = db
        .prepare("SELECT name FROM pragma_table_info('request_results') ORDER BY cid")
        .expect("schema")
        .query_map([], |row| row.get(0))
        .expect("columns")
        .collect::<rusqlite::Result<_>>()
        .expect("column names");
    for column in ["status", "ref_kind", "ref_id", "ref_rev"] {
        assert!(
            columns.iter().any(|name| name == column),
            "missing {column}"
        );
    }
    drop(db);
    drop(store);
}

fn enum_column<T: strum::VariantNames>(db: &Connection, table: &str, column: &str) {
    let sql: String = db
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1",
            [table],
            |row| row.get(0),
        )
        .expect("actual table DDL");
    assert!(
        sql.contains(&enum_check::<T>(column)),
        "{table}.{column} must consume its enum owner: {sql}"
    );
}

#[tokio::test]
async fn receipt_cutover_dropped_tables_stay_absent_on_second_open() {
    let root = tempfile::tempdir().expect("state");
    for _ in 0..2 {
        drop(RuntimeStore::open(Some(root.path())).await.expect("open"));
        let db = Connection::open(root.path().join("canvas.sqlite3")).expect("database");
        for table in [
            "runtime_operation_receipts",
            "runtime_native_action_receipts",
            "runtime_context_terminal_receipts",
            "user_terminal_receipts",
            "runtime_access_receipts",
            "runtime_tool_results",
            "runtime_tool_request_aliases",
        ] {
            let exists: bool = db
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
                    [table],
                    |row| row.get(0),
                )
                .expect("schema");
            assert!(!exists, "{table} must stay absent");
        }
    }
}

#[tokio::test]
async fn receipt_cutover_enum_checks_match_owners_on_fresh_and_reopened_schema() {
    let root = tempfile::tempdir().expect("state");
    let mut first = None;
    for _ in 0..2 {
        let store = RuntimeStore::open(Some(root.path())).await.expect("open");
        let db = Connection::open(root.path().join("canvas.sqlite3")).expect("database");
        enum_column::<EventKind>(&db, "runtime_events", "kind");
        enum_column::<EventStatus>(&db, "runtime_events", "status");
        enum_column::<MigrationStatus>(&db, "runtime_payload_migrations", "status");
        enum_column::<HoldStatus>(&db, "runtime_agents", "hold_status");
        enum_column::<TitleStatus>(&db, "runtime_agents", "title_status");
        enum_column::<WorkEffectStatus>(&db, "runtime_agents", "vm_launch_status");
        enum_column::<IdleEpisodeOutcome>(&db, "runtime_agents", "idle_episode_outcome");
        enum_column::<WorkStatus>(&db, "runtime_work", "status");
        for column in ["archive_status", "vm_submit_status", "vm_accept_status"] {
            enum_column::<WorkEffectStatus>(&db, "runtime_work", column);
        }
        enum_column::<ToolRequestKind>(&db, "runtime_tool_requests", "kind");
        enum_column::<ToolRequestStatus>(&db, "runtime_tool_requests", "status");
        enum_column::<ExecutionStatus>(&db, "runtime_execution_runs", "status");
        enum_column::<AttemptSubmission>(&db, "runtime_execution_attempts", "submission");
        enum_column::<ExecutionStatus>(&db, "runtime_execution_attempts", "turn_status");
        enum_column::<ExecutionEffectKind>(&db, "runtime_execution_effects", "kind");
        enum_column::<ExecutionEffectStatus>(&db, "runtime_execution_effects", "status");
        enum_column::<ExecutionNodeStatus>(&db, "runtime_execution_nodes", "status");
        enum_column::<ProjectRemovalReason>(&db, "runtime_projects", "removal_reason");
        enum_column::<OutboxKind>(&db, "runtime_outbox", "kind");
        enum_column::<OutboxStatus>(&db, "runtime_outbox", "status");
        enum_column::<OwnerKind>(&db, "runtime_outbox", "owner_kind");
        enum_column::<IncidentKind>(&db, "runtime_error_incidents", "kind");
        enum_column::<OwnerKind>(&db, "runtime_error_incidents", "owner_kind");
        let ddl:Vec<(String,String)>=db.prepare("SELECT name,sql FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").expect("schema").query_map([],|row|Ok((row.get(0)?,row.get(1)?))).expect("rows").collect::<rusqlite::Result<_>>().expect("DDL");
        if let Some(previous) = &first {
            assert_eq!(previous, &ddl);
        } else {
            first = Some(ddl);
        }
        let result = store.transaction(|tx| {
        tx.execute_batch("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES('enum-witness','a','user','','pending',1,0); INSERT INTO runtime_tool_requests(id,record) VALUES('managed-enum','{\"id\":\"managed-enum\",\"kind\":\"managed_tool\",\"stage\":\"queued\",\"agent\":\"a\",\"admitted\":{\"id\":\"request\"}}');").expect("canonical variants");
        assert!(
            tx.execute(
                "UPDATE runtime_events SET kind='bogus' WHERE id='enum-witness'",
                []
            )
            .is_err()
        );
        assert!(tx.execute("UPDATE runtime_tool_requests SET record=json_set(record,'$.stage','bogus') WHERE id='managed-enum'",[]).is_err());
        assert_eq!(
            tx.query_row(
                "SELECT status FROM runtime_tool_requests WHERE id='managed-enum'",
                [],
                |row| row.get::<_, ToolRequestStatus>(0)
            )
            .expect("canonical status codec"),
            ToolRequestStatus::Queued
        );
        Err::<(), _>(studio_runtime_store::StoreError::RecordContract("rollback enum witness"))
        }).await;
        assert!(matches!(
            result,
            Err(studio_runtime_store::StoreError::RecordContract(
                "rollback enum witness"
            ))
        ));
    }
}
