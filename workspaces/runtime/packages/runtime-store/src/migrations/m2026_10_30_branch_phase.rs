use crate::StoreResult;
use rusqlite::{Transaction, params};
use serde::Deserialize;
use studio_runtime_schema::workspace_operation::OperationPhase;

pub fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    if !crate::schema::table_exists(tx, "runtime_workspace_operations")? {
        return Ok(());
    }
    let blocked = serde_json::to_string(&OperationPhase::LocalMutation)?;
    tx.execute("UPDATE runtime_workspace_operations SET record=json_object('phase',json(?1),'legacyRecord',json(record)) WHERE id LIKE 'branch:%' AND json_valid(record) AND json_type(record)!='object'", [&blocked])?;
    tx.execute("UPDATE runtime_workspace_operations SET record=json_object('phase',json(?1),'legacyRecord',record) WHERE id LIKE 'branch:%' AND NOT json_valid(record)", [&blocked])?;
    let mut query = tx.prepare("SELECT id,json_type(record,'$.phase'),json_extract(record,'$.phase'),json_type(record,'$.legacyPhase'),json_type(record,'$.legacyPhaseCollision') FROM runtime_workspace_operations WHERE id LIKE 'branch:%' AND json_valid(record) AND json_type(record)='object'")?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let kind: Option<String> = row.get(1)?;
        let phase: Option<String> = if kind.as_deref() == Some("text") {
            row.get(2)?
        } else {
            None
        };
        let legacy: Option<String> = row.get(3)?;
        let collision: Option<String> = row.get(4)?;
        if phase.as_deref().is_some_and(|phase| {
            OperationPhase::deserialize(
                serde::de::value::StrDeserializer::<serde::de::value::Error>::new(phase),
            )
            .is_ok()
        }) {
            continue;
        }
        if kind.is_some() && legacy.is_some() && collision.is_some() {
            tx.execute("UPDATE runtime_workspace_operations SET record=json_set(record,'$.legacyPhaseCollision',json_array(json_extract(record,'$.legacyPhaseCollision'),json_extract(record,'$.phase')),'$.phase',json(?2)) WHERE id=?1", params![id, blocked])?;
        } else if kind.is_some() && legacy.is_some() {
            tx.execute("UPDATE runtime_workspace_operations SET record=json_set(record,'$.legacyPhaseCollision',json_quote(json_extract(record,'$.phase')),'$.phase',json(?2)) WHERE id=?1", params![id, blocked])?;
        } else if kind.is_some() {
            tx.execute("UPDATE runtime_workspace_operations SET record=json_set(record,'$.legacyPhase',json_quote(json_extract(record,'$.phase')),'$.phase',json(?2)) WHERE id=?1", params![id, blocked])?;
        } else {
            tx.execute("UPDATE runtime_workspace_operations SET record=json_set(record,'$.phase',json(?2)) WHERE id=?1", params![id, blocked])?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn existing_phase_evidence_is_not_overwritten() {
        let mut db = Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_workspace_operations(id TEXT PRIMARY KEY,record TEXT NOT NULL); INSERT INTO runtime_workspace_operations VALUES('branch:collision','{\"phase\":{\"new\":1},\"legacyPhase\":{\"old\":2}}'),('branch:both','{\"phase\":{\"new\":1},\"legacyPhase\":{\"old\":2},\"legacyPhaseCollision\":{\"older\":3}}'),('branch:null','null'),('branch:broken','{');").expect("fixtures");
        let tx = db.transaction().expect("transaction");
        apply(&tx).expect("migration");
        for (id, expected) in [
            (
                "branch:collision",
                r#"{"phase":"local_mutation","legacyPhase":{"old":2},"legacyPhaseCollision":{"new":1}}"#,
            ),
            (
                "branch:both",
                r#"{"phase":"local_mutation","legacyPhase":{"old":2},"legacyPhaseCollision":[{"older":3},{"new":1}]}"#,
            ),
            (
                "branch:null",
                r#"{"phase":"local_mutation","legacyRecord":null}"#,
            ),
            (
                "branch:broken",
                r#"{"phase":"local_mutation","legacyRecord":"{"}"#,
            ),
        ] {
            let raw: String = tx
                .query_row(
                    "SELECT record FROM runtime_workspace_operations WHERE id=?1",
                    [id],
                    |r| r.get(0),
                )
                .expect("row");
            assert_eq!(raw, expected);
        }
    }

    #[test]
    fn missing_and_object_phases_are_blocked_and_evidence_is_preserved() {
        let mut db = Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_workspace_operations(id TEXT PRIMARY KEY,record TEXT NOT NULL); INSERT INTO runtime_workspace_operations VALUES('branch:missing','{\"signature\":\"s\"}'),('branch:object','{\"phase\":{\"completed\":null},\"signature\":\"s\"}'),('branch:known','{\"phase\":\"completed\",\"signature\":\"s\"}'),('checkpoint:x','{\"phase\":{}}');").expect("fixtures");
        let tx = db.transaction().expect("transaction");
        apply(&tx).expect("migration");
        apply(&tx).expect("repeat");
        for (id, expected) in [
            (
                "branch:missing",
                r#"{"signature":"s","phase":"local_mutation"}"#,
            ),
            (
                "branch:object",
                r#"{"phase":"local_mutation","signature":"s","legacyPhase":{"completed":null}}"#,
            ),
            ("branch:known", r#"{"phase":"completed","signature":"s"}"#),
            ("checkpoint:x", r#"{"phase":{}}"#),
        ] {
            let raw: String = tx
                .query_row(
                    "SELECT record FROM runtime_workspace_operations WHERE id=?1",
                    [id],
                    |row| row.get(0),
                )
                .expect("record");
            assert_eq!(raw, expected);
        }
    }
}
