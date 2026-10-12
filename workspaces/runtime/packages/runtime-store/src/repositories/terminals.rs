//! Durable user terminal records and output history.

use rusqlite::{Connection, OptionalExtension, params};

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
    pub record: String,
    pub output: String,
    pub offset: i64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TerminalsRepository;

impl TerminalsRepository {
    pub fn list(connection: &Connection) -> StoreResult<Vec<(String, String)>> {
        let mut statement =
            connection.prepare("SELECT id,record FROM user_terminals ORDER BY rowid")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn get(connection: &Connection, id: &str) -> StoreResult<Option<TerminalRow>> {
        connection
            .query_row(
                "SELECT id,record,output,offset FROM user_terminals WHERE id=?1",
                [id],
                |row| {
                    Ok(TerminalRow {
                        id: row.get(0)?,
                        record: row.get(1)?,
                        output: row.get(2)?,
                        offset: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn put(
        transaction: &rusqlite::Transaction<'_>,
        id: &str,
        record: &str,
        output: &str,
        offset: i64,
    ) -> StoreResult<()> {
        transaction.execute(
            "INSERT INTO user_terminals(id,record,output,offset) VALUES (?1,?2,?3,?4) \
             ON CONFLICT(id) DO UPDATE SET record=excluded.record,output=excluded.output,offset=excluded.offset",
            params![id, record, output, offset],
        )?;
        Ok(())
    }

    pub fn update_record(
        transaction: &rusqlite::Transaction<'_>,
        id: &str,
        record: &str,
    ) -> StoreResult<()> {
        transaction.execute(
            "UPDATE user_terminals SET record=?1 WHERE id=?2",
            params![record, id],
        )?;
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
        let mut position = start;
        for chunk in text.chars().collect::<Vec<_>>().chunks(65_536) {
            let value = chunk.iter().collect::<String>();
            transaction.execute(
                "INSERT INTO user_terminal_output(terminal,start,end,text) VALUES (?1,?2,?3,?4)",
                params![id, position, position + chunk.len() as i64, value],
            )?;
            position += chunk.len() as i64;
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

    pub fn has_archive(connection: &Connection, id: &str) -> StoreResult<bool> {
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM user_terminal_output WHERE terminal=?1)",
                [id],
                |row| row.get(0),
            )
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
