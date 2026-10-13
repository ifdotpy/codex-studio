//! Durable user terminal records and output history.

use rusqlite::{Connection, OptionalExtension, params, types::Value as SqlValue};

use crate::records::{Table, TerminalRecord};
use crate::schema::{SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};

const TABLES: &[TableRequirement] = &[
    TableRequirement {
        name: "user_terminals",
        columns: &["id", "record", "output", "offset"],
    },
    TableRequirement {
        name: "user_terminal_receipts",
        columns: &["id", "signature", "result"],
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
    tables: TABLES,
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
            if !row.output.is_empty() && Self::archive_start(transaction, id)?.is_none() {
                Self::append_output(transaction, id, row.offset, &row.output)?;
            }
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
        let table = Table::<TerminalRecord>::new(connection);
        let Some(record) = table.get(&id.to_owned())? else {
            return Ok(None);
        };
        connection
            .query_row(
                "SELECT output,offset FROM user_terminals WHERE id=?1",
                [id],
                |row| {
                    Ok(TerminalRow {
                        id: record.id().as_str().to_owned(),
                        record,
                        output: row.get(0)?,
                        offset: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
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
        table.put_with_columns(
            typed,
            &[
                ("output", SqlValue::Text(output.to_owned())),
                ("offset", SqlValue::Integer(offset)),
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

    pub fn receipt(connection: &Connection, id: &str) -> StoreResult<Option<(String, String)>> {
        connection
            .query_row(
                "SELECT signature,result FROM user_terminal_receipts WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn save_receipt(
        transaction: &rusqlite::Transaction<'_>,
        id: &str,
        signature: &str,
        result: &str,
    ) -> StoreResult<()> {
        transaction.execute(
            "INSERT INTO user_terminal_receipts(id,signature,result) VALUES (?1,?2,?3)",
            params![id, signature, result],
        )?;
        Ok(())
    }

    pub fn update_receipt(
        transaction: &rusqlite::Transaction<'_>,
        id: &str,
        result: &str,
    ) -> StoreResult<()> {
        transaction.execute(
            "UPDATE user_terminal_receipts SET result=?1 WHERE id=?2",
            params![result, id],
        )?;
        Ok(())
    }

    pub fn archive_chunks(
        connection: &Connection,
        id: &str,
        start: i64,
        end: i64,
        limit: i64,
    ) -> StoreResult<Vec<(i64, i64, String)>> {
        let mut statement = connection.prepare(
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
    use rusqlite::Connection;

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
}
