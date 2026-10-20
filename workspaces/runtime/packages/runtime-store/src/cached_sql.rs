//! Statement-cached forms of `Connection::execute` and `Connection::query_row`.
//!
//! The plain forms compile their SQL on every call. A writer statement on a
//! table with many expression indexes and triggers costs far more to compile
//! than to run, so hot paths with fixed SQL text use these forms, which reuse
//! the connection's prepared-statement cache ([`STATEMENT_CACHE_CAPACITY`]).
//! SQL text built per call (variable `IN` lists) keeps the plain forms.

use rusqlite::{Connection, Params, Row};

/// Prepared statements kept per connection; unit: statements. Above the
/// distinct fixed SQL texts one connection runs on its hot paths, so the LRU
/// does not evict them between notifications.
pub const STATEMENT_CACHE_CAPACITY: usize = 256;

pub trait CachedSql {
    /// [`Connection::execute`] through the prepared-statement cache.
    fn execute_cached<P: Params>(&self, sql: &str, params: P) -> rusqlite::Result<usize>;
    /// [`Connection::query_row`] through the prepared-statement cache.
    fn query_row_cached<T, P: Params, F: FnOnce(&Row<'_>) -> rusqlite::Result<T>>(
        &self,
        sql: &str,
        params: P,
        row: F,
    ) -> rusqlite::Result<T>;
}

impl CachedSql for Connection {
    fn execute_cached<P: Params>(&self, sql: &str, params: P) -> rusqlite::Result<usize> {
        self.prepare_cached(sql)?.execute(params)
    }

    fn query_row_cached<T, P: Params, F: FnOnce(&Row<'_>) -> rusqlite::Result<T>>(
        &self,
        sql: &str,
        params: P,
        row: F,
    ) -> rusqlite::Result<T> {
        self.prepare_cached(sql)?.query_row(params, row)
    }
}
