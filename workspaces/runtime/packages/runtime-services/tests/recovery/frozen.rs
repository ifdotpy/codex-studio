//! Restore reviewable historical SQLite seeds without a Python interpreter.
use std::path::Path;

pub(crate) fn database(root: &Path, name: &str, sql: &str) {
    std::fs::create_dir_all(root).expect("create frozen state directory");
    let connection = rusqlite::Connection::open(root.join(name)).expect("create frozen database");
    connection
        .execute_batch(sql)
        .expect("restore frozen schema and rows");
}

pub(crate) fn state(root: &Path, canvas: &str, analytics: &str) {
    database(root, "canvas.sqlite3", canvas);
    database(root, "analytics.sqlite3", analytics);
}
