//! Durable user terminal records and output history.

use rusqlite::{Connection, OptionalExtension, params, types::Value as SqlValue};

use crate::outbox::{Outbox, OutboxRow};
use crate::records::{JsonRecord, Table, TerminalRecord};
use crate::request_results::{Admission, AdmittedRequest, RequestResults, ResultEnvelope};
use crate::schema::{SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};
use serde::{Deserialize, Serialize};
use studio_runtime_schema::persistence::{
    OutboxKind, OutboxStatus, OwnerKind, RequestIdentity, TerminalActionFailure,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalActionReply {
    /// Exact original serialized response body.
    pub response: String,
    pub failure: Option<TerminalActionFailure>,
}
#[derive(Serialize, Deserialize)]
struct TerminalActionIntent {
    admitted: AdmittedRequest,
    reply: TerminalActionReply,
}

fn action_outbox_id(root: &str) -> String {
    serde_json::json!(["terminal-action", root]).to_string()
}

fn now_ms() -> StoreResult<i64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|time| time.as_millis() as i64)
        .map_err(|error| StoreError::SchemaMismatch(error.to_string()))
}
const TABLES: &[TableRequirement] = &[
    TableRequirement {
        name: "user_terminals",
        columns: &["id", "record", "output", "offset", "output_generation"],
    },
    TableRequirement {
        name: "user_terminal_output",
        columns: &["terminal", "start", "end", "text"],
    },
    TableRequirement {
        name: "user_terminal_event_cursor",
        columns: &["terminal", "sequence"],
    },
    TableRequirement {
        name: "user_terminal_decoder_state",
        columns: &["terminal", "pending"],
    },
];

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: TABLES,
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Debug, PartialEq)]
pub struct TerminalRow {
    pub id: String,
    pub record: TerminalRecord,
    pub output: String,
    pub offset: i64,
    /// Durable counter incremented when replacement output moves its end backwards.
    pub output_generation: i64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TerminalsRepository;

impl TerminalsRepository {
    pub fn append_and_retain(
        transaction: &rusqlite::Transaction<'_>,
        id: &str,
        row: &TerminalRow,
        text: &str,
        limit: usize,
    ) -> StoreResult<(String, i64)> {
        if !text.is_empty() {
            Self::append_output(
                transaction,
                id,
                row.offset + row.output.chars().count() as i64,
                text,
            )?;
        }
        let output = row.output.clone() + text;
        let dropped = output.chars().count().saturating_sub(limit);
        Ok((
            output.chars().skip(dropped).collect(),
            row.offset + dropped as i64,
        ))
    }

    pub fn list(connection: &Connection) -> StoreResult<Vec<TerminalRecord>> {
        Table::<TerminalRecord>::new(connection).list()
    }

    pub fn get(connection: &Connection, id: &str) -> StoreResult<Option<TerminalRow>> {
        let snapshot = connection
            .query_row(
                "SELECT record,output,offset,output_generation FROM user_terminals WHERE id=?1",
                [id],
                |row| {
                    Ok((
                        row.get::<_, rusqlite::types::Value>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((record_value, output, offset, output_generation)) = snapshot else {
            return Ok(None);
        };
        let SqlValue::Text(encoded) = record_value else {
            return Err(StoreError::InvalidStoredObject {
                table: TerminalRecord::TABLE,
                id: id.to_owned(),
            });
        };
        let record = crate::records::decode_record::<TerminalRecord>(connection, id, &encoded)?;
        Ok(Some(TerminalRow {
            id: record.id().as_str().to_owned(),
            record,
            output,
            offset,
            output_generation,
        }))
    }

    pub fn put(
        transaction: &rusqlite::Transaction<'_>,
        id: &str,
        typed: &TerminalRecord,
        output: &str,
        offset: i64,
    ) -> StoreResult<()> {
        let table = Table::<TerminalRecord>::new(transaction);
        table.get(typed.id())?;
        if typed.id().as_str() != id {
            return Err(StoreError::SchemaMismatch(
                "terminal record identity does not match its key".to_owned(),
            ));
        }
        // This is the single reset-counter owner. Retention moves the start
        // forward without reducing the end and therefore keeps the generation.
        let generation = Self::get(transaction, id)?.map_or(0, |previous| {
            previous.output_generation
                + i64::from(
                    offset + (output.chars().count() as i64)
                        < previous.offset + (previous.output.chars().count() as i64),
                )
        });
        table.put_with_columns(
            typed,
            &[
                ("output", SqlValue::Text(output.to_owned())),
                ("offset", SqlValue::Integer(offset)),
                ("output_generation", SqlValue::Integer(generation)),
            ],
        )?;
        Ok(())
    }

    pub fn update_record(
        transaction: &rusqlite::Transaction<'_>,
        id: &str,
        record: &TerminalRecord,
    ) -> StoreResult<()> {
        if record.id().as_str() != id {
            return Err(StoreError::SchemaMismatch(
                "terminal record identity does not match its key".to_owned(),
            ));
        }
        let table = Table::<TerminalRecord>::new(transaction);
        if table.get(&id.to_owned())?.is_some() {
            table.update(record)?;
        }
        Ok(())
    }

    pub fn delete(transaction: &rusqlite::Transaction<'_>, id: &str) -> StoreResult<()> {
        transaction.execute("DELETE FROM user_terminal_output WHERE terminal=?1", [id])?;
        transaction.execute(
            "DELETE FROM user_terminal_event_cursor WHERE terminal=?1",
            [id],
        )?;
        transaction.execute(
            "DELETE FROM user_terminal_decoder_state WHERE terminal=?1",
            [id],
        )?;
        transaction.execute("DELETE FROM user_terminals WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn action_result(
        connection: &Connection,
        id: &str,
    ) -> StoreResult<Option<(String, TerminalActionReply)>> {
        if let Some(saved) = Outbox::get::<TerminalActionIntent>(connection, &action_outbox_id(id))
            .map_err(StoreError::from)?
        {
            if saved.kind != OutboxKind::TerminalAction
                || !matches!(saved.owner_kind, OwnerKind::Terminal | OwnerKind::Request)
            {
                return Err(StoreError::SchemaMismatch(
                    "terminal action owner changed".into(),
                ));
            }
            return Ok(Some((saved.signature, saved.payload.reply)));
        }
        Ok(
            RequestResults::read(connection, id, "terminal-action", &[], id, &[], now_ms()?)?
                .map(|saved| (saved.signature, saved.reply)),
        )
    }

    pub fn reserve_action(
        tx: &rusqlite::Transaction<'_>,
        terminal: &str,
        id: &str,
        signature: &str,
        response: &str,
        failure: Option<TerminalActionFailure>,
    ) -> StoreResult<()> {
        let identity = RequestIdentity::new("terminal-action", &[], id, &[])
            .map_err(|error| StoreError::RequestResult(error.into()))?;
        let admitted = match RequestResults::admit(tx, &identity, signature, now_ms()?)
            .map_err(StoreError::from)?
        {
            Admission::Fresh(admitted) => admitted,
            Admission::Replay(_) => {
                return Err(StoreError::SchemaMismatch(
                    "terminal action already completed".into(),
                ));
            }
        };
        Outbox::put(
            tx,
            &OutboxRow {
                id: action_outbox_id(id),
                kind: OutboxKind::TerminalAction,
                owner_kind: OwnerKind::Terminal,
                owner_id: terminal.into(),
                request_id: Some(admitted.id().into()),
                signature: signature.into(),
                status: OutboxStatus::Unknown,
                account: None,
                stream_id: None,
                generation: None,
                ordinal: None,
                next_at: None,
                created: admitted.created(),
                payload: TerminalActionIntent {
                    admitted,
                    reply: TerminalActionReply {
                        response: response.into(),
                        failure,
                    },
                },
            },
        )
        .map_err(StoreError::from)
    }

    pub fn settle_action(
        tx: &rusqlite::Transaction<'_>,
        id: &str,
        response: &str,
        failure: Option<TerminalActionFailure>,
    ) -> StoreResult<()> {
        let Some(mut saved) = Outbox::get::<TerminalActionIntent>(tx, &action_outbox_id(id))
            .map_err(StoreError::from)?
        else {
            return Ok(());
        };
        RequestResults::settle(
            tx,
            &saved.payload.admitted,
            &ResultEnvelope {
                actor: None,
                reply: TerminalActionReply {
                    response: response.into(),
                    failure,
                },
                references: vec![saved.owner_id.clone()],
            },
        )
        .map_err(StoreError::from)?;
        if failure == Some(TerminalActionFailure::Disconnected) {
            saved.payload.reply = TerminalActionReply {
                response: response.into(),
                failure,
            };
            Outbox::put(tx, &saved).map_err(StoreError::from)?;
        } else {
            Outbox::delete_settled(tx, &saved.id, &saved.signature).map_err(StoreError::from)?;
        }
        Ok(())
    }

    pub fn archive_chunks(
        connection: &Connection,
        id: &str,
        start: i64,
        end: i64,
        limit: i64,
    ) -> StoreResult<Vec<(i64, i64, String)>> {
        let mut statement = connection.prepare_cached(
            "SELECT start,end,text FROM user_terminal_output \
             WHERE terminal=?1 AND end>?2 AND start<?3 ORDER BY start LIMIT ?4",
        )?;
        let rows = statement.query_map(params![id, start, end, limit], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn archive_start(connection: &Connection, id: &str) -> StoreResult<Option<i64>> {
        connection
            .query_row(
                "SELECT start FROM user_terminal_output WHERE terminal=?1 ORDER BY start LIMIT 1",
                [id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn append_output(
        transaction: &rusqlite::Transaction<'_>,
        id: &str,
        start: i64,
        text: &str,
    ) -> StoreResult<()> {
        let previous: Option<(i64, i64, String)> = transaction.query_row(
            "SELECT start,end,text FROM user_terminal_output WHERE terminal=?1 ORDER BY start DESC LIMIT 1",
            [id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        let mut text = text;
        let mut start = start;
        if let Some((chunk_start, end, mut previous)) =
            previous.filter(|(_, end, value)| *end == start && value.chars().count() < 65_536)
        {
            let count = text.chars().count().min(65_536 - previous.chars().count());
            let added = text.chars().take(count).collect::<String>();
            previous.push_str(&added);
            transaction.execute(
                "UPDATE user_terminal_output SET end=?1,text=?2 WHERE terminal=?3 AND start=?4",
                params![end + count as i64, previous, id, chunk_start],
            )?;
            start += count as i64;
            text = &text[added.len()..];
        }
        for chunk in text.chars().collect::<Vec<_>>().chunks(65_536) {
            let value = chunk.iter().collect::<String>();
            transaction.execute(
                "INSERT INTO user_terminal_output(terminal,start,end,text) VALUES (?1,?2,?3,?4)",
                params![id, start, start + chunk.len() as i64, value],
            )?;
            start += chunk.len() as i64;
        }
        Ok(())
    }

    pub fn event_cursor(connection: &Connection, id: &str) -> StoreResult<Option<i64>> {
        connection
            .query_row(
                "SELECT sequence FROM user_terminal_event_cursor WHERE terminal=?1",
                [id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn decoder_pending(connection: &Connection, id: &str) -> StoreResult<Option<Vec<u8>>> {
        connection
            .query_row(
                "SELECT pending FROM user_terminal_decoder_state WHERE terminal=?1",
                [id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn save_decoder_pending(
        transaction: &rusqlite::Transaction<'_>,
        id: &str,
        pending: &[u8],
    ) -> StoreResult<()> {
        transaction.execute(
            "INSERT INTO user_terminal_decoder_state(terminal,pending) VALUES (?1,?2) \
             ON CONFLICT(terminal) DO UPDATE SET pending=excluded.pending",
            params![id, pending],
        )?;
        Ok(())
    }

    pub fn save_event_cursor(
        transaction: &rusqlite::Transaction<'_>,
        id: &str,
        sequence: i64,
    ) -> StoreResult<()> {
        transaction.execute(
            "INSERT INTO user_terminal_event_cursor(terminal,sequence) VALUES (?1,?2) \
             ON CONFLICT(terminal) DO UPDATE SET sequence=excluded.sequence",
            params![id, sequence],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn receipt_pivot_terminal_unknown_is_not_ttl_data_and_scope_does_not_collide() {
        use studio_runtime_schema::persistence::{
            OutboxKind, REQUEST_RESULT_TTL_MS, RequestIdentity,
        };
        let mut db = Connection::open_in_memory().expect("database");
        crate::receipt_schema::create(&db).expect("schema");
        db.execute_batch(
            "CREATE TABLE runtime_orchestration_settings(id TEXT PRIMARY KEY,record TEXT NOT NULL)",
        )
        .expect("policy");
        let root = RequestIdentity::issue_root();
        let now = super::now_ms().expect("clock");
        let tx = db.transaction().expect("transaction");
        TerminalsRepository::reserve_action(
            &tx,
            "terminal",
            &root,
            "signature",
            "{\"delivery\":\"uncertain\"}",
            None,
        )
        .expect("reserve before dispatch");
        tx.execute("INSERT INTO runtime_outbox(id,kind,owner_kind,owner_id,signature,status,payload,created) VALUES(?1,'native_action','agent','agent','other','unknown','{}',?2)",rusqlite::params![json!(["native-action",root]).to_string(),now]).expect("same external root in another family");
        assert!(
            crate::outbox::Outbox::pending::<serde_json::Value>(
                &tx,
                OutboxKind::TerminalAction,
                i64::MAX,
                10
            )
            .expect("pending")
            .is_empty()
        );
        crate::request_results::RequestResults::gc(&tx, now + REQUEST_RESULT_TTL_MS + 1)
            .expect("advance expiry");
        assert!(
            TerminalsRepository::action_result(&tx, &root)
                .expect("unknown remains")
                .is_some()
        );
        assert!(
            TerminalsRepository::reserve_action(&tx, "terminal", &root, "changed", "{}", None)
                .is_err()
        );
        TerminalsRepository::settle_action(&tx, &root, "{\"delivery\":\"sent\"}", None)
            .expect("late exact native result");
        assert_eq!(
            tx.query_row("SELECT count(*) FROM runtime_outbox", [], |row| row
                .get::<_, i64>(0))
                .expect("only other family remains"),
            1
        );
        assert_eq!(
            TerminalsRepository::action_result(&tx, &root)
                .expect("exact replay")
                .expect("result")
                .1
                .response,
            "{\"delivery\":\"sent\"}"
        );
        crate::request_results::RequestResults::gc(&tx, now + REQUEST_RESULT_TTL_MS + 1)
            .expect("expire final result");
        assert!(
            TerminalsRepository::reserve_action(&tx, "terminal", &root, "signature", "{}", None)
                .is_err(),
            "expiry never permits another native write"
        );
    }

    #[test]
    fn receipt_pivot_terminal_replays_original_reply_without_old_table() {
        let db = rusqlite::Connection::open_in_memory().expect("database");
        crate::receipt_schema::create(&db).expect("target schema");
        db.execute_batch(
            "CREATE TABLE runtime_orchestration_settings(id TEXT PRIMARY KEY,record TEXT NOT NULL)",
        )
        .expect("policy");
        let reply = serde_json::json!({"ok":true,"delivery":"sent"});
        let envelope = crate::request_results::ResultEnvelope {
            actor: None,
            reply: super::TerminalActionReply {
                response: reply.to_string(),
                failure: None,
            },
            references: Vec::<String>::new(),
        };
        db.execute(
            "INSERT INTO request_results VALUES('imported-terminal','signature',?1,?2)",
            rusqlite::params![
                serde_json::to_string(&envelope).expect("envelope"),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_millis() as i64
            ],
        )
        .expect("imported final response");
        assert_eq!(
            super::TerminalsRepository::action_result(&db, "imported-terminal").expect("replay"),
            Some((
                "signature".to_owned(),
                super::TerminalActionReply {
                    response: reply.to_string(),
                    failure: None
                }
            ))
        );
    }
    use rusqlite::{Connection, functions::FunctionFlags};
    use serde_json::json;

    use super::TerminalsRepository;

    #[test]
    fn output_deltas_fill_the_previous_partial_chunk() {
        let mut database = Connection::open_in_memory().expect("memory database");
        database.execute_batch("CREATE TABLE user_terminal_output(terminal TEXT,start INTEGER,end INTEGER,text TEXT)").expect("archive table");
        for (start, text) in [(0, "a"), (1, "b"), (2, "c"), (3, "d")] {
            let transaction = database.transaction().expect("transaction");
            TerminalsRepository::append_output(&transaction, "term", start, text).expect("append");
            transaction.commit().expect("commit");
        }
        let (start, end, text): (i64, i64, String) = database
            .query_row(
                "SELECT start,end,text FROM user_terminal_output WHERE terminal='term'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("single packed history row");
        assert_eq!((start, end, text.as_str()), (0, 4, "abcd"));
    }

    #[test]
    fn terminal_record_and_output_are_read_from_one_snapshot() {
        let directory = tempfile::tempdir().expect("owned test directory");
        let path = directory.path().join("snapshot.sqlite3");
        let database = Connection::open(&path).expect("temporary SQLite database opens");
        database
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE terminal_backing(id TEXT PRIMARY KEY,record TEXT,output TEXT,offset INTEGER);
                 CREATE VIEW user_terminals AS SELECT id,record_and_advance(record) AS record,output,offset,0 AS output_generation FROM terminal_backing;",
            )
            .expect("WAL-backed fixture creates");
        let record = json!({"id":"terminal","agent":"a","title":"Terminal","cwd":"/tmp","status":"running","created":1});
        database
            .execute(
                "INSERT INTO terminal_backing VALUES('terminal',?1,'old',0)",
                [record.to_string()],
            )
            .expect("initial terminal is stored");
        let writer = Connection::open(&path).expect("concurrent writer opens");
        database
            .create_scalar_function(
                "record_and_advance",
                1,
                FunctionFlags::SQLITE_UTF8,
                move |context| {
                    let old: String = context.get(0)?;
                    writer.execute(
                        "UPDATE terminal_backing SET record=json_set(record,'$.status','exited'),output='new',offset=3 WHERE id='terminal'",
                        [],
                    )?;
                    Ok(old)
                },
            )
            .expect("concurrent commit trigger registers");

        let row = TerminalsRepository::get(&database, "terminal")
            .expect("terminal snapshot reads")
            .expect("terminal exists");
        assert_eq!(
            row.record.get("status").and_then(serde_json::Value::as_str),
            Some("running")
        );
        assert_eq!((row.output.as_str(), row.offset), ("old", 0));

        drop(database);
    }
}
inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "terminals",
        requirements: &REQUIREMENTS,
    }
}
