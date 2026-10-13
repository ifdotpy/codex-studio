//! Read-only validation of the open access records and SQL delivery projections.
use crate::{StateCheckFailure, StateCheckTable, StoreError, StoreResult};
use rusqlite::Connection;
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) fn check(connection: &Connection) -> StoreResult<Vec<StateCheckTable>> {
    let mut result = Vec::new();
    for (table, projection) in [
        ("runtime_access_clients", "record"),
        ("runtime_access_discovered", "record"),
        ("runtime_access_invites", "record"),
        ("runtime_access_receipts", "record"),
        (
            "runtime_server_inbox",
            "json_object('id',id,'signature',signature,'state',state,'result',result,'created_at',created_at,'completed_at',completed_at)",
        ),
        (
            "runtime_server_outbox",
            "json_object('id',id,'server',server,'body',body,'state',state,'result',result,'fingerprint',fingerprint,'attempts',attempts,'created_at',created_at,'completed_at',completed_at)",
        ),
    ] {
        let mut report = StateCheckTable {
            table,
            row_count: 0,
            rejected_count: 0,
            failures: Vec::new(),
        };
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1 AND type='table')",
            [table],
            |r| r.get(0),
        )?;
        if exists {
            let mut counts = BTreeMap::new();
            let mut statement = connection.prepare(&format!("SELECT {projection} FROM {table}"))?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                report.row_count += 1;
                let error = match row
                    .get::<_, String>(0)
                    .ok()
                    .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                {
                    Some(value) => studio_runtime_schema::first_error(
                        studio_runtime_schema::table_schema(table).ok_or_else(|| {
                            StoreError::SchemaValidation(format!("no storage schema for {table}"))
                        })?,
                        &value,
                    )
                    .map_err(StoreError::SchemaValidation)?,
                    None => Some((String::new(), "JSON".to_owned())),
                };
                if let Some(error) = error {
                    report.rejected_count += 1;
                    *counts.entry(error).or_insert(0) += 1;
                }
            }
            report.failures = counts
                .into_iter()
                .map(|((pointer, keyword), count)| StateCheckFailure {
                    pointer,
                    keyword,
                    count,
                })
                .collect();
        }
        result.push(report);
    }
    Ok(result)
}
