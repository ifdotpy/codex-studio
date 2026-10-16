//! Indexed access to reviewer archive candidates and due retries.

use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::{StoreError, StoreResult};

pub struct ReviewerArchiveRepository;

impl ReviewerArchiveRepository {
    pub(crate) fn prepare_indexes(transaction: &Transaction<'_>) -> StoreResult<()> {
        for definition in Self::index_definitions() {
            transaction.execute_batch(&definition.create_sql)?;
        }
        Ok(())
    }

    /// Canonical DDL for the reviewer archive worker's repository-owned indexes.
    #[doc(hidden)]
    pub fn index_definitions() -> Vec<crate::OwnedRuntimeIndexDefinition> {
        let candidate_eligibility = candidate_eligibility("record");
        let retry_eligibility = retry_eligibility("record");
        vec![
            crate::OwnedRuntimeIndexDefinition {
                name: "runtime_agent_reviewer_archive_pending",
                table: "runtime_agents",
                create_sql: format!(
                    "CREATE INDEX IF NOT EXISTS runtime_agent_reviewer_archive_pending \
                     ON runtime_agents(json_extract(record,'$.parentId'),id) \
                     WHERE {candidate_eligibility}"
                ),
            },
            crate::OwnedRuntimeIndexDefinition {
                name: "runtime_agent_reviewer_archive_retry_due",
                table: "runtime_agents",
                create_sql: format!(
                    "CREATE INDEX IF NOT EXISTS runtime_agent_reviewer_archive_retry_due \
                     ON runtime_agents(CASE WHEN json_valid(record) THEN \
                         COALESCE(json_extract(record,'$.reviewArchiveNextAt'),0) END,id) \
                     WHERE {retry_eligibility}"
                ),
            },
            crate::OwnedRuntimeIndexDefinition {
                name: "runtime_event_reviewer_archive_delivery",
                table: "runtime_events",
                create_sql: "CREATE INDEX IF NOT EXISTS runtime_event_reviewer_archive_delivery \
                    ON runtime_events(agent, \
                        CASE WHEN json_valid(text) THEN json_extract(text,'$.agent_id') END,id) \
                    WHERE kind='child_result' AND status='delivered'"
                    .to_owned(),
            },
        ]
    }

    /// Completed reviewers that can be newly scheduled. The partial index
    /// excludes historical markers using the same predicate as this query.
    pub fn candidates(connection: &Connection) -> StoreResult<Vec<(String, String)>> {
        let eligibility = candidate_eligibility("a.record");
        let mut statement = connection.prepare(&format!(
            "SELECT a.id,a.record FROM runtime_agents AS a \
             INDEXED BY runtime_agent_reviewer_archive_pending \
             WHERE {eligibility} ORDER BY a.id"
        ))?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Earliest delivered child-result identity for the given parent and child.
    /// Malformed event JSON remains ineligible, as it was in the Rust parser.
    pub fn child_result_id(
        connection: &Connection,
        parent_id: &str,
        child_id: &str,
    ) -> StoreResult<Option<String>> {
        connection
            .query_row(
                "SELECT id FROM runtime_events INDEXED BY runtime_event_reviewer_archive_delivery \
                 WHERE id LIKE 'child:%' AND kind='child_result' AND status='delivered' \
                   AND agent=?1 \
                   AND CASE WHEN json_valid(text) \
                       THEN json_extract(text,'$.agent_id') END=?2 \
                 ORDER BY id LIMIT 1",
                rusqlite::params![parent_id, child_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Durable archive markers whose retry deadline has elapsed.
    pub fn due_retries(connection: &Connection, now: f64) -> StoreResult<Vec<(String, String)>> {
        let eligibility = retry_eligibility("record");
        let scheduled = "CASE WHEN json_valid(record) THEN \
            json_extract(record,'$.reviewArchiveScheduled') IS NOT NULL ELSE 0 END";
        let deadline = "CASE WHEN json_valid(record) THEN \
            COALESCE(json_extract(record,'$.reviewArchiveNextAt'),0) END";
        let mut statement = connection.prepare(&format!(
            "SELECT id,record FROM runtime_agents INDEXED BY runtime_agent_reviewer_archive_retry_due \
             WHERE {eligibility} AND {scheduled} \
               AND {deadline}<=?1 ORDER BY id"
        ))?;
        statement
            .query_map([now], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }
}

/// Owns the candidate index predicate and its consuming query predicate.
fn candidate_eligibility(record: &str) -> String {
    format!(
        "CASE WHEN json_valid({record}) THEN ( \
         json_extract({record},'$.role')='reviewer' \
         AND json_extract({record},'$.status')='completed' \
         AND {} AND {} \
         AND (json_type({record},'$.reviewArchiveScheduled') IS NOT 'text' \
              OR json_extract({record},'$.reviewArchiveScheduled')='') \
         ) ELSE 0 END",
        falsey(record, "$.agentArchive"),
        falsey(record, "$.reviewArchiveError"),
    )
}

/// Owns the durable retry index predicate and its consuming query predicate.
fn retry_eligibility(record: &str) -> String {
    format!(
        "CASE WHEN json_valid({record}) THEN ( \
         json_extract({record},'$.role')='reviewer' \
         AND json_extract({record},'$.status')='completed' \
         AND {} \
         AND json_extract({record},'$.reviewArchiveScheduled') IS NOT NULL \
         ) ELSE 0 END",
        falsey(record, "$.agentArchive"),
    )
}

/// Mirrors `studio_runtime_store::truthy` for JSON values. All JSON1 calls are
/// behind CASE so malformed persisted records are excluded without an error.
fn falsey(record: &str, path: &str) -> String {
    format!(
        "(json_type({record},'{path}') IS NULL \
         OR json_type({record},'{path}')='null' \
         OR json_type({record},'{path}')='false' \
         OR (json_type({record},'{path}') IN ('integer','real') \
             AND json_extract({record},'{path}')=0) \
         OR (json_type({record},'{path}')='text' \
             AND json_extract({record},'{path}')='') \
         OR (json_type({record},'{path}')='array' \
             AND json_array_length({record},'{path}')=0) \
         OR (json_type({record},'{path}')='object' \
             AND json_extract({record},'{path}')='{{}}'))"
    )
}

#[cfg(test)]
mod tests {
    use super::{
        ReviewerArchiveRepository as Repository, candidate_eligibility, retry_eligibility,
    };
    use crate::Connection;
    use serde_json::{Value, json};

    fn database() -> Connection {
        let mut connection = Connection::open_in_memory().expect("memory database opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);\
                 CREATE TABLE runtime_events(\
                    id TEXT PRIMARY KEY,agent TEXT NOT NULL,kind TEXT NOT NULL,text TEXT NOT NULL,\
                    status TEXT NOT NULL,created REAL NOT NULL,epoch INTEGER NOT NULL,\
                    turn_id TEXT,error TEXT);\
                 ",
            )
            .expect("archive tables create");
        let transaction = connection.transaction().expect("index transaction starts");
        Repository::prepare_indexes(&transaction).expect("archive indexes create");
        transaction.commit().expect("index transaction commits");
        connection
    }

    fn put_agent(connection: &Connection, id: &str, extra: Value) {
        let mut record = json!({"id":id,"role":"reviewer","status":"completed",
            "parentId":"parent","rootId":"parent","epoch":1});
        for (key, value) in extra.as_object().expect("extra object") {
            record[key] = value.clone();
        }
        connection
            .execute(
                "INSERT INTO runtime_agents(id,record) VALUES (?1,?2)",
                rusqlite::params![id, record.to_string()],
            )
            .expect("agent inserts");
    }

    #[test]
    fn candidate_predicate_matches_every_falsey_json_type() {
        let connection = database();
        let falsey_values = [
            Value::Null,
            json!(false),
            json!(0),
            json!(0.0),
            json!(""),
            json!([]),
            json!({}),
        ];
        for (index, value) in falsey_values.iter().enumerate() {
            put_agent(
                &connection,
                &format!("eligible-{index}"),
                json!({
                    "agentArchive":value,
                    "reviewArchiveError":value,
                    "reviewArchiveScheduled":""
                }),
            );
        }
        put_agent(&connection, "eligible-missing-markers", json!({}));
        connection
            .execute(
                "INSERT INTO runtime_agents(id,record) VALUES (?1,?2)",
                [
                    "eligible-whitespace-object",
                    r#"{"id":"eligible-whitespace-object","role":"reviewer","status":"completed","parentId":"parent","agentArchive":{ }}"#,
                ],
            )
            .expect("whitespace empty object inserts");
        for (index, scheduled) in [
            Value::Null,
            json!(false),
            json!(0),
            json!(""),
            json!([]),
            json!({}),
        ]
        .iter()
        .enumerate()
        {
            put_agent(
                &connection,
                &format!("eligible-scheduled-marker-{index}"),
                json!({"reviewArchiveScheduled":scheduled}),
            );
        }
        for (index, value) in [
            json!(true),
            json!(1),
            json!("archived"),
            json!([1]),
            json!({"reason":"archived"}),
            json!("x".repeat(1024 * 1024)),
        ]
        .iter()
        .enumerate()
        {
            put_agent(
                &connection,
                &format!("excluded-archive-{index}"),
                json!({
                    "agentArchive":value
                }),
            );
            put_agent(
                &connection,
                &format!("excluded-error-{index}"),
                json!({
                    "reviewArchiveError":value
                }),
            );
        }
        put_agent(
            &connection,
            "excluded-scheduled",
            json!({
                "reviewArchiveScheduled":"child:already-scheduled"
            }),
        );
        connection
            .execute(
                "INSERT INTO runtime_agents(id,record) VALUES ('malformed','{bad json')",
                [],
            )
            .expect("malformed record inserts");

        let candidates = Repository::candidates(&connection).expect("candidates load");
        let ids = candidates.into_iter().map(|(id, _)| id).collect::<Vec<_>>();
        assert_eq!(ids.len(), falsey_values.len() + 1 + 1 + 6);
        assert!(ids.iter().any(|id| id == "eligible-missing-markers"));
        assert!(ids.iter().any(|id| id == "eligible-whitespace-object"));
        assert!(ids.iter().all(|id| !id.starts_with("excluded-")));
        assert!(!ids.iter().any(|id| id == "malformed"));
    }

    #[test]
    fn indexed_candidate_and_retry_queries_have_no_agent_table_scan() {
        let connection = database();
        let candidates = format!(
            "EXPLAIN QUERY PLAN SELECT a.id,a.record FROM runtime_agents AS a \
             INDEXED BY runtime_agent_reviewer_archive_pending WHERE {} ORDER BY a.id",
            candidate_eligibility("a.record")
        );
        let details = connection
            .prepare(&candidates)
            .expect("candidate query prepares")
            .query_map([], |row| row.get::<_, String>(3))
            .expect("candidate plan loads")
            .collect::<Result<Vec<_>, _>>()
            .expect("candidate plan rows load")
            .join(" ");
        assert!(
            details.contains("runtime_agent_reviewer_archive_pending"),
            "{details}"
        );
        assert!(
            details.contains("USING INDEX runtime_agent_reviewer_archive_pending"),
            "{details}"
        );

        let retries = format!(
            "EXPLAIN QUERY PLAN SELECT id,record FROM runtime_agents \
             INDEXED BY runtime_agent_reviewer_archive_retry_due WHERE {} \
             AND CASE WHEN json_valid(record) \
                 THEN json_extract(record,'$.reviewArchiveScheduled') IS NOT NULL ELSE 0 END \
             AND CASE WHEN json_valid(record) \
                 THEN COALESCE(json_extract(record,'$.reviewArchiveNextAt'),0) END<=?1 \
             ORDER BY id",
            retry_eligibility("record")
        );
        let details = connection
            .prepare(&retries)
            .expect("retry query prepares")
            .query_map([10.0], |row| row.get::<_, String>(3))
            .expect("retry plan loads")
            .collect::<Result<Vec<_>, _>>()
            .expect("retry plan rows load")
            .join(" ");
        assert!(
            details.contains("runtime_agent_reviewer_archive_retry_due"),
            "{details}"
        );
        assert!(
            details.contains("USING INDEX runtime_agent_reviewer_archive_retry_due"),
            "{details}"
        );

        let event_lookup = "EXPLAIN QUERY PLAN SELECT id FROM runtime_events \
            INDEXED BY runtime_event_reviewer_archive_delivery \
            WHERE id LIKE 'child:%' AND kind='child_result' AND status='delivered' \
              AND agent=?1 AND CASE WHEN json_valid(text) \
                  THEN json_extract(text,'$.agent_id') END=?2 ORDER BY id LIMIT 1";
        let details = connection
            .prepare(event_lookup)
            .expect("event query prepares")
            .query_map(["parent", "child"], |row| row.get::<_, String>(3))
            .expect("event plan loads")
            .collect::<Result<Vec<_>, _>>()
            .expect("event plan rows load")
            .join(" ");
        assert!(
            details.contains("runtime_event_reviewer_archive_delivery"),
            "{details}"
        );
        assert!(!details.contains("SCAN runtime_events"), "{details}");
    }

    #[test]
    fn malformed_event_payload_does_not_match_child_result_lookup() {
        let connection = database();
        connection
            .execute(
                "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) \
                 VALUES ('child:bad','parent','child_result','{bad json','delivered',1,1)",
                [],
            )
            .expect("malformed event inserts");
        assert_eq!(
            Repository::child_result_id(&connection, "parent", "child")
                .expect("lookup handles malformed payload"),
            None
        );
    }

    #[test]
    fn large_valid_event_payload_keeps_its_stable_child_identity() {
        let connection = database();
        let text = json!({"agent_id":"child","result":"x".repeat(1024 * 1024)}).to_string();
        connection
            .execute(
                "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) \
                 VALUES ('child:large','parent','child_result',?1,'delivered',1,1)",
                [text],
            )
            .expect("large child result inserts");
        assert_eq!(
            Repository::child_result_id(&connection, "parent", "child")
                .expect("lookup handles large payload"),
            Some("child:large".to_owned())
        );
    }
}
