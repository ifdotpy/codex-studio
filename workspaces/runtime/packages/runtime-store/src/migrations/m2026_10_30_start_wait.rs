use crate::StoreResult;
use rusqlite::{Transaction, params};
use serde::Deserialize;
use studio_runtime_schema::agent_progress::AgentStartWait;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    if !crate::schema::table_exists(tx, "runtime_agents")? {
        return Ok(());
    }
    let mut query = tx.prepare("SELECT id,json_quote(json_extract(record,'$.startWait')) FROM runtime_agents WHERE json_valid(record) AND json_type(record,'$.startWait') IS NOT NULL")?;
    let mut rows = query.query([])?;
    let (mut normalized, mut removed) = (0u64, 0u64);
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let raw: String = row.get(1)?;
        if serde_json::from_str::<AgentStartWait>(&raw).is_ok() {
            continue;
        }
        let wait = serde_json::from_str::<serde_json::Value>(&raw)
            .ok()
            .and_then(|mut value| {
                let reason = value.get("reason")?.as_str()?.to_owned();
                value.as_object_mut()?.retain(|key, value| {
                    key == "reason"
                        || key == "epoch"
                        || (reason == "slot" && (key == "runningTurns" || key == "maxRunningTurns"))
                        || (matches!(reason.as_str(), "pause" | "provider_session")
                            && key == "until"
                            && !value.is_null())
                });
                AgentStartWait::deserialize(value).ok()
            });
        if let Some(wait) = wait {
            tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.startWait',json(?2)) WHERE id=?1", params![id, serde_json::to_string(&wait)?])?;
            normalized += 1;
        } else {
            tx.execute(
                "UPDATE runtime_agents SET record=json_remove(record,'$.startWait') WHERE id=?1",
                [&id],
            )?;
            removed += 1;
        }
    }
    eprintln!("startWait migration: normalized={normalized}, removed={removed}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use serde_json::{Value, json};

    fn migrate(wait: &Value) -> Value {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            .unwrap();
        let row = json!({"id":"a","rootId":"a","epoch":1,"status":"queued","accountKey":"default","provider":"codex","startWait":wait});
        db.execute(
            "INSERT INTO runtime_agents VALUES('a',?1)",
            [row.to_string()],
        )
        .unwrap();
        let tx = db.transaction().unwrap();
        apply(&tx).unwrap();
        let raw: String = tx
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn normalizes_complete_pause() {
        let row = migrate(&json!({"reason":"pause","epoch":1,"until":12.0,"runningTurns":null}));
        assert_eq!(
            row["startWait"],
            json!({"reason":"pause","epoch":1,"until":12.0})
        );
    }

    #[test]
    fn normalizes_complete_provider_session() {
        let row =
            migrate(&json!({"reason":"provider_session","epoch":1,"until":null,"extension":true}));
        assert_eq!(
            row["startWait"],
            json!({"reason":"provider_session","epoch":1})
        );
    }

    #[test]
    fn removes_incomplete_wait() {
        let row = migrate(&json!({"reason":"slot","epoch":1,"runningTurns":null}));
        assert!(row.get("startWait").is_none());
    }

    #[test]
    fn compact_record_preserves_other_member_bytes() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            .unwrap();
        let original = r#"{"id":"a","startWait":{"reason":"pause","epoch":1,"until":2.0,"extension":true},"admission":{"signature":"a\\b","amount":1.0}}"#;
        db.execute("INSERT INTO runtime_agents VALUES('a',?1)", [original])
            .unwrap();
        let tx = db.transaction().unwrap();
        apply(&tx).unwrap();
        let saved: String = tx
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(saved, original.replace(",\"extension\":true", ""));
    }
}
