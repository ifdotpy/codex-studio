use crate::StoreResult;
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    tx.execute_batch("UPDATE runtime_agents SET record=json_remove(record, '$.worktreeCleanup', '$.cleanedImageWorkspace')
        WHERE json_valid(record) AND (json_type(record,'$.worktreeCleanup') IS NOT NULL OR json_type(record,'$.cleanedImageWorkspace') IS NOT NULL);
        UPDATE runtime_agents SET record=json_set(record, '$.deletedAt', CASE json_type(record,'$.deletedAt') WHEN 'true' THEN 1 ELSE NULL END)
        WHERE json_valid(record) AND json_type(record,'$.deletedAt') IN ('true','false');
        UPDATE runtime_agents SET record=json_set(record, '$.nativeStatus.error',
        CASE WHEN json_type(record,'$.nativeStatus.error')='object' AND json_type(record,'$.nativeStatus.error.message')='text'
             THEN json_extract(record,'$.nativeStatus.error.message')
             ELSE printf('%s',json(record->'$.nativeStatus.error')) END)
        WHERE json_valid(record) AND json_type(record,'$.nativeStatus.error') NOT IN ('text');
        UPDATE runtime_agents SET record=json_remove(record,'$.nativeStatus.message')
        WHERE json_valid(record) AND json_type(record,'$.nativeStatus.message') NOT IN ('text','null');")?;
    let fields = crate::agent_volatile::AGENT_VOLATILE_FIELDS;
    let present = fields
        .iter()
        .map(|field| format!("json_type(record,'$.{field}') IS NOT NULL"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let has_legacy: bool = tx.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM runtime_agents WHERE json_valid(record) AND ({present}))"
        ),
        [],
        |row| row.get(0),
    )?;
    if has_legacy {
        super::m2026_10_20_agents::migrate_2026_10_20(tx)?;
    }
    let changed = fields
        .iter()
        .map(|field| format!("\"{field}\"='null'"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let needs_materialization: bool = tx.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM runtime_agents a LEFT JOIN runtime_agent_volatile v ON v.id=a.id WHERE v.id IS NULL) OR EXISTS(SELECT 1 FROM runtime_agent_volatile WHERE {changed})"),
        [], |row| row.get(0),
    )?;
    if needs_materialization {
        tx.execute_batch("DROP TRIGGER IF EXISTS runtime_agent_volatile_journal_insert;
            DROP TRIGGER IF EXISTS runtime_agent_volatile_journal_update;
            DROP TRIGGER IF EXISTS runtime_agent_volatile_transcript_insert;
            DROP TRIGGER IF EXISTS runtime_agent_volatile_transcript_update;
            INSERT OR IGNORE INTO runtime_agent_volatile(id) SELECT id FROM runtime_agents;")?;
        let assignments = fields.iter().map(|field| format!("\"{field}\"=NULLIF(\"{field}\",'null')"))
            .collect::<Vec<_>>().join(",");
        tx.execute_batch(&format!("UPDATE runtime_agent_volatile SET {assignments} WHERE {changed}"))?;
    }
    crate::agent_volatile::prepare(tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn removes_retired_workspace_cleanup_fields() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            .unwrap();
        db.execute_batch(crate::agent_volatile::agent_volatile_table_sql())
            .unwrap();
        db.execute("INSERT INTO runtime_agents VALUES('a',?1)", [r#"{"id":"a","rootId":"a","epoch":1,"status":"idle","accountKey":"default","worktreeCleanup":{"phase":"done"},"cleanedImageWorkspace":{}}"#]).unwrap();
        let tx = db.transaction().unwrap();
        apply(&tx).unwrap();
        let count: i64 = tx.query_row("SELECT count(*) FROM runtime_agents WHERE json_type(record,'$.worktreeCleanup') IS NOT NULL OR json_type(record,'$.cleanedImageWorkspace') IS NOT NULL", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 0);
    }
    #[test]
    fn normalizes_boolean_deletion_markers() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            .unwrap();
        db.execute_batch(crate::agent_volatile::agent_volatile_table_sql())
            .unwrap();
        for (id, deleted) in [("true", "true"), ("false", "false")] {
            let record = format!(
                r#"{{"id":"{id}","rootId":"{id}","epoch":1,"status":"idle","accountKey":"default","deletedAt":{deleted}}}"#
            );
            db.execute("INSERT INTO runtime_agents VALUES(?1,?2)", [id, &record])
                .unwrap();
        }
        let tx = db.transaction().unwrap();
        apply(&tx).unwrap();
        assert_eq!(
            crate::AgentRepository::get(&tx, &crate::AgentId::from("true"))
                .unwrap()
                .unwrap()
                .deleted_at,
            Some(1.0)
        );
        assert_eq!(
            crate::AgentRepository::get(&tx, &crate::AgentId::from("false"))
                .unwrap()
                .unwrap()
                .deleted_at,
            None
        );
    }
    #[test]
    fn normalizes_historical_native_status_errors() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            .unwrap();
        db.execute_batch(crate::agent_volatile::agent_volatile_table_sql())
            .unwrap();
        for (id, error) in [
            ("text", r#""message""#),
            ("message", r#"{"message":"readable","other":1}"#),
            ("other", r#"{"code":4}"#),
        ] {
            let record = format!(
                r#"{{"id":"{id}","rootId":"{id}","epoch":1,"status":"idle","accountKey":"default","nativeStatus":{{"error":{error}}}}}"#
            );
            db.execute("INSERT INTO runtime_agents VALUES(?1,?2)", [id, &record])
                .unwrap();
        }
        let tx = db.transaction().unwrap();
        apply(&tx).unwrap();
        for (id, expected) in [
            ("text", "message"),
            ("message", "readable"),
            ("other", r#"{"code":4}"#),
        ] {
            let actual: String = tx.query_row("SELECT json_extract(record,'$.nativeStatus.error') FROM runtime_agents WHERE id=?1", [id], |row| row.get(0)).unwrap();
            assert_eq!(actual, expected);
        }
    }
    #[test]
    fn materializes_missing_volatile_rows_and_moves_old_fields() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            .unwrap();
        db.execute_batch(crate::agent_volatile::agent_volatile_table_sql())
            .unwrap();
        db.execute("INSERT INTO runtime_agents VALUES('a',?1)", [r#"{"id":"a","rootId":"a","epoch":1,"status":"idle","accountKey":"default","events":2}"#]).unwrap();
        let tx = db.transaction().unwrap();
        apply(&tx).unwrap();
        let (events, activity): (Option<String>, Option<String>) = tx
            .query_row(
                "SELECT events,activity FROM runtime_agent_volatile WHERE id='a'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(events.as_deref(), Some("2"));
        assert_eq!(activity, None);
        let agent = crate::AgentRepository::get(&tx, &crate::AgentId::from("a"))
            .unwrap()
            .unwrap();
        assert_eq!(agent.events, Some(2));
    }
}
