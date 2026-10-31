//! Target DDL consumed by the dated migration and disposable contract callers.
//! Creating this schema alone does not perform the receipt-family cutover.
use rusqlite::Connection;
use studio_runtime_schema::persistence::{
    IncidentKind, OutboxKind, OutboxStatus, OwnerKind, enum_check,
};

pub fn create(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(&format!(r#"
        CREATE TABLE IF NOT EXISTS request_results (
            id TEXT PRIMARY KEY, signature TEXT NOT NULL, result TEXT NOT NULL, created INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS request_results_created ON request_results(created,id);
        CREATE INDEX IF NOT EXISTS request_results_tool_actor_created
            ON request_results(json_extract(result,'$.actor'),created DESC,id)
            WHERE json_extract(result,'$.actor') IS NOT NULL;
        CREATE TABLE IF NOT EXISTS runtime_outbox (
            id TEXT PRIMARY KEY, kind TEXT NOT NULL {kind}, owner_kind TEXT NOT NULL {owner},
            owner_id TEXT NOT NULL, request_id TEXT, signature TEXT NOT NULL,
            status TEXT NOT NULL {status}, account TEXT, stream_id TEXT, generation INTEGER,
            ordinal INTEGER, next_at INTEGER, payload TEXT NOT NULL, created INTEGER NOT NULL,
            CHECK((status!='{unknown}' AND status!='{unrecognized}') OR next_at IS NULL));
        CREATE INDEX IF NOT EXISTS runtime_outbox_pending ON runtime_outbox(kind,status,next_at,id) WHERE status='{pending}';
        CREATE INDEX IF NOT EXISTS runtime_outbox_owner ON runtime_outbox(owner_kind,owner_id,kind,id);
        CREATE INDEX IF NOT EXISTS runtime_outbox_account ON runtime_outbox(account,kind,status,ordinal,id);
        CREATE INDEX IF NOT EXISTS runtime_outbox_stream ON runtime_outbox(stream_id,generation,ordinal,id);
        CREATE INDEX IF NOT EXISTS runtime_outbox_stream_ordered ON runtime_outbox(stream_id,kind,ordinal,id);
        CREATE INDEX IF NOT EXISTS runtime_outbox_request ON runtime_outbox(request_id,id) WHERE request_id IS NOT NULL;
        CREATE TABLE IF NOT EXISTS runtime_error_incidents (
            id TEXT PRIMARY KEY, kind TEXT NOT NULL {incident}, owner_kind TEXT NOT NULL {owner},
            owner_id TEXT NOT NULL, created INTEGER NOT NULL, last_seen INTEGER NOT NULL,
            dismissed_at INTEGER, payload TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS runtime_error_incidents_created ON runtime_error_incidents(created DESC,id);
        CREATE INDEX IF NOT EXISTS runtime_error_incidents_owner ON runtime_error_incidents(owner_kind,owner_id,created DESC,id);
        CREATE INDEX IF NOT EXISTS runtime_error_incidents_open ON runtime_error_incidents(last_seen DESC,id) WHERE dismissed_at IS NULL;
        CREATE TABLE IF NOT EXISTS runtime_account_state (
            account TEXT PRIMARY KEY, next_generation INTEGER NOT NULL, connection_generation INTEGER,
            connection_id TEXT, journal_ack INTEGER NOT NULL, recovery_required INTEGER NOT NULL,
            streams TEXT NOT NULL, limits_observation TEXT, start_failure TEXT);
    "#,
        kind=enum_check::<OutboxKind>("kind"), owner=enum_check::<OwnerKind>("owner_kind"),
        status=enum_check::<OutboxStatus>("status"), incident=enum_check::<IncidentKind>("kind"),
        pending=OutboxStatus::Pending.as_str(),unknown=OutboxStatus::Unknown.as_str(),unrecognized=OutboxStatus::Unrecognized.as_str(),
    ))?;
    install_dismissed_index(connection)
}

pub(crate) fn install_dismissed_index(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch("CREATE INDEX IF NOT EXISTS runtime_error_incidents_dismissed ON runtime_error_incidents(id,dismissed_at) WHERE dismissed_at IS NOT NULL")
}

#[cfg(test)]
mod tests {
    use super::*;
    use strum::VariantNames;
    #[test]
    fn dismissal_scan_uses_partial_covering_index() {
        let db = Connection::open_in_memory().expect("fresh");
        create(&db).expect("DDL");
        let plan: String = db.query_row("EXPLAIN QUERY PLAN SELECT id,dismissed_at FROM runtime_error_incidents WHERE dismissed_at IS NOT NULL", [], |row| row.get(3)).expect("plan");
        assert!(plan.contains("runtime_error_incidents_dismissed"), "{plan}");
    }
    #[test]
    fn receipt_target_schema_checks_and_indexes_are_live() {
        let mut db = Connection::open_in_memory().expect("fresh");
        create(&db).expect("DDL");
        let tx = db.transaction().expect("migration tx");
        crate::receipt_migration_contract::prepare_schema(&tx).expect("migration helper");
        tx.commit().expect("commit");
        for (table, column, variants) in [
            ("runtime_outbox", "kind", OutboxKind::VARIANTS),
            ("runtime_outbox", "owner_kind", OwnerKind::VARIANTS),
            ("runtime_outbox", "status", OutboxStatus::VARIANTS),
            ("runtime_error_incidents", "kind", IncidentKind::VARIANTS),
            ("runtime_error_incidents", "owner_kind", OwnerKind::VARIANTS),
        ] {
            let ddl: String = db
                .query_row(
                    "SELECT sql FROM sqlite_schema WHERE name=?1",
                    [table],
                    |row| row.get(0),
                )
                .expect("live schema");
            for variant in variants {
                assert!(
                    ddl.contains(&format!("'{variant}'")),
                    "{table}.{column}: {variant}"
                );
                let sql = if table == "runtime_outbox" {
                    format!(
                        "INSERT INTO {table}(id,kind,owner_kind,owner_id,signature,status,payload,created) VALUES('witness','provider_call','agent','owner','sig','pending','{{}}',1)"
                    )
                } else {
                    format!(
                        "INSERT INTO {table}(id,kind,owner_kind,owner_id,created,last_seen,payload) VALUES('witness','http_failure','agent','owner',1,1,'{{}}')"
                    )
                };
                db.execute(&sql, []).expect("valid base row");
                db.execute(
                    &format!("UPDATE {table} SET {column}=?1 WHERE id='witness'"),
                    [variant],
                )
                .expect("every variant admitted");
                assert!(
                    db.execute(
                        &format!("UPDATE {table} SET {column}='bogus' WHERE id='witness'"),
                        []
                    )
                    .is_err()
                );
                db.execute(&format!("DELETE FROM {table} WHERE id='witness'"), [])
                    .expect("clear witness");
            }
        }
        for (sql, index) in [
            (
                "SELECT id FROM request_results WHERE created<=0 ORDER BY created,id LIMIT 500",
                "request_results_created",
            ),
            (
                "SELECT id FROM runtime_outbox WHERE kind='provider_call' AND status='pending' AND next_at<=0 ORDER BY next_at,id LIMIT 5",
                "runtime_outbox_pending",
            ),
            (
                "SELECT id FROM runtime_outbox WHERE owner_kind='agent' AND owner_id='a' AND kind='provider_call' ORDER BY id",
                "runtime_outbox_owner",
            ),
            (
                "SELECT id FROM runtime_outbox WHERE account='a' AND kind='provider_call' AND status='pending' ORDER BY ordinal,id",
                "runtime_outbox_account",
            ),
            (
                "SELECT id FROM runtime_outbox WHERE stream_id='s' AND generation=1 ORDER BY ordinal,id",
                "runtime_outbox_stream",
            ),
            (
                "SELECT id,payload,generation FROM runtime_outbox WHERE stream_id='s' AND kind='journal_entry' ORDER BY ordinal,id LIMIT 65",
                "runtime_outbox_stream_ordered",
            ),
            (
                "SELECT id FROM runtime_outbox WHERE request_id='r' ORDER BY id",
                "runtime_outbox_request",
            ),
        ] {
            let plan: String = db
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .expect("plan")
                .query_map([], |row| row.get::<_, String>(3))
                .expect("steps")
                .collect::<rusqlite::Result<Vec<_>>>()
                .expect("plan rows")
                .join("; ");
            assert!(plan.contains(index), "{plan}");
            if index == "runtime_outbox_stream_ordered" {
                assert!(!plan.contains("TEMP B-TREE"), "{plan}");
            }
        }
    }
}
