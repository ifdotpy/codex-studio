use crate::{StoreError, StoreResult};
use rusqlite::{Connection, ToSql, Transaction};
use studio_runtime_schema::persistence::{IdleEpisodeOutcome, ServerOperationKind, enum_check};

const LEGACY_IDLE_OUTCOME: &str = "not_one_hour";

fn outbox_schema(db: &Connection) -> StoreResult<String> {
    Ok(db.query_row(
        "SELECT json_extract(value,'$.sql') FROM json_each(?1,'$.\"canvas.sqlite3\".sqlite_master') WHERE json_extract(value,'$.name')='runtime_server_outbox'",
        [crate::fresh_state::SNAPSHOT],
        |row| row.get(0),
    )?)
}

fn rebuild(
    tx: &Transaction<'_>,
    table: &str,
    ddl: &str,
    copy: &str,
    values: &[&dyn ToSql],
) -> StoreResult<()> {
    let objects = tx
        .prepare("SELECT sql FROM sqlite_schema WHERE tbl_name=?1 AND type IN ('index','trigger') AND sql IS NOT NULL ORDER BY type,name")?
        .query_map([table], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    tx.execute_batch(ddl)?;
    tx.execute(copy, values)?;
    tx.execute_batch(&format!(
        "PRAGMA legacy_alter_table=ON;
         ALTER TABLE {table} RENAME TO {table}_before_current_checks;
         ALTER TABLE {table}_current_checks RENAME TO {table};
         PRAGMA legacy_alter_table=OFF;
         DROP TABLE {table}_before_current_checks;"
    ))?;
    for object in objects {
        tx.execute_batch(&object)?;
    }
    Ok(())
}

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    let agent_sql: String = tx.query_row(
        "SELECT sql FROM sqlite_schema WHERE type='table' AND name='runtime_agents'",
        [],
        |row| row.get(0),
    )?;
    if !agent_sql.contains(&enum_check::<IdleEpisodeOutcome>("idle_episode_outcome")) {
        let ddl = crate::agent_columns::table_sql("runtime_agents_current_checks")
            .replace("CREATE TABLE IF NOT EXISTS", "CREATE TABLE");
        rebuild(
            tx,
            "runtime_agents",
            &ddl,
            "INSERT INTO runtime_agents_current_checks(rowid,id,record,hold_status,hold_incident)
             SELECT rowid,id,CASE WHEN json_extract(record,'$.idleCompactionEpisode.outcome')=?1
                 THEN json_set(record,'$.idleCompactionEpisode.outcome',?2) ELSE record END,
                 hold_status,hold_incident FROM runtime_agents ORDER BY rowid;",
            &[
                &LEGACY_IDLE_OUTCOME,
                &IdleEpisodeOutcome::TtlChanged.as_str(),
            ],
        )?;
    }
    let outbox_sql: String = tx.query_row(
        "SELECT sql FROM sqlite_schema WHERE type='table' AND name='runtime_server_outbox'",
        [],
        |row| row.get(0),
    )?;
    if !outbox_sql.contains(&enum_check::<ServerOperationKind>("receipt_kind")) {
        let canonical = outbox_schema(tx)?;
        let tail = canonical
            .strip_prefix("CREATE TABLE runtime_server_outbox")
            .ok_or(StoreError::RecordContract(
                "fresh server outbox schema is invalid",
            ))?;
        rebuild(
            tx,
            "runtime_server_outbox",
            &format!("CREATE TABLE runtime_server_outbox_current_checks{tail}"),
            "INSERT INTO runtime_server_outbox_current_checks(rowid,id,server,body,state,result,attempts,next_at,error,created_at,completed_at,fingerprint,admission)
             SELECT rowid,id,server,body,state,result,attempts,next_at,error,created_at,completed_at,fingerprint,admission FROM runtime_server_outbox ORDER BY rowid;",
            &[],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    #[test]
    fn repairs_current_checks_without_losing_rows_indexes_or_triggers() {
        let mut db = Connection::open_in_memory().unwrap();
        let agent_sql = crate::agent_columns::table_sql("runtime_agents")
            .replace("'ttl_changed'", "'not_one_hour'");
        db.execute_batch(&agent_sql).unwrap();
        let outbox_sql = super::outbox_schema(&db).unwrap();
        let old_sql = outbox_sql
            .replace(",'projects.changed'", "")
            .replace(",'backup.put'", "")
            .replace(",'device_trust'", "");
        db.execute_batch(&old_sql).unwrap();
        db.execute_batch("CREATE TABLE audit(n INTEGER); INSERT INTO audit VALUES(0); CREATE INDEX agent_probe ON runtime_agents(root_id); CREATE TRIGGER agent_probe_update AFTER UPDATE ON runtime_agents BEGIN UPDATE audit SET n=n+1; END; CREATE INDEX outbox_probe ON runtime_server_outbox(state,next_at);").unwrap();
        db.execute("INSERT INTO runtime_agents(rowid,id,record,hold_status,hold_incident) VALUES(42,'old',?1,'invalid','held')", [r#"{"compactions":0,"id":"old","rootId":"old","epoch":0,"status":"idle","accountKey":"default","provider":"codex","idleCompactionEpisode":{"outcome":"not_one_hour"}}"#]).unwrap();
        db.execute("INSERT INTO runtime_server_outbox(rowid,id,server,body,state) VALUES(61,'old','peer','{\"action\":\"event\"}','queued')", []).unwrap();
        let agent = r#"{"compactions":0,"id":"new","rootId":"new","epoch":0,"status":"idle","accountKey":"default","provider":"codex","idleCompactionEpisode":{"outcome":"ttl_changed"}}"#;
        assert!(
            db.execute(
                "INSERT INTO runtime_agents(id,record) VALUES('new',?1)",
                [agent]
            )
            .is_err()
        );
        for action in ["device_trust", "backup.put", "projects.changed"] {
            assert!(db.execute("INSERT INTO runtime_server_outbox(id,server,body,state) VALUES(?1,'peer',json_object('action',?1),'queued')", [action]).is_err());
        }
        db.authorizer(Some(|context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::CreateTable {
                    table_name: "runtime_server_outbox_current_checks"
                }
            ) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))
        .unwrap();
        let tx = db.transaction().unwrap();
        assert!(super::apply(&tx).is_err());
        tx.rollback().unwrap();
        db.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
        assert_eq!(db.query_row("SELECT json_extract(record,'$.idleCompactionEpisode.outcome') FROM runtime_agents WHERE id='old'", [], |r| r.get::<_, String>(0)).unwrap(), "not_one_hour");
        let tx = db.transaction().unwrap();
        super::apply(&tx).unwrap();
        super::apply(&tx).unwrap();
        tx.commit().unwrap();
        let row: (i64, String, String, String) = db.query_row("SELECT rowid,json_extract(record,'$.idleCompactionEpisode.outcome'),hold_status,hold_incident FROM runtime_agents WHERE id='old'", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
        assert_eq!(
            row,
            (42, "ttl_changed".into(), "invalid".into(), "held".into())
        );
        assert_eq!(
            db.query_row("SELECT n FROM audit", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            db.query_row(
                "SELECT rowid FROM runtime_server_outbox WHERE id='old'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            61
        );
        for object in ["agent_probe", "agent_probe_update", "outbox_probe"] {
            assert!(
                db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=?1)",
                    [object],
                    |r| r.get::<_, bool>(0)
                )
                .unwrap(),
                "{object}"
            );
        }
        db.execute(
            "INSERT INTO runtime_agents(id,record) VALUES('new',?1)",
            [agent],
        )
        .unwrap();
        for action in ["device_trust", "backup.put", "projects.changed"] {
            db.execute("INSERT INTO runtime_server_outbox(id,server,body,state) VALUES(?1,'peer',json_object('action',?1),'queued')", [action]).unwrap();
        }
    }
}
