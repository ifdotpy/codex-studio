//! Closed historical state fixtures shared by the media and authentication witnesses.
use std::{fs, path::Path};

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Databases {
    #[serde(rename = "canvas.sqlite3")]
    canvas: String,
    #[serde(rename = "analytics.sqlite3")]
    analytics: String,
}

const DATABASE_NAMES: [&str; 2] = ["canvas.sqlite3", "analytics.sqlite3"];

pub(crate) fn write_state(root: &Path, fixture: &str) {
    let databases: Databases = serde_json::from_str(fixture).expect("frozen database SQL");
    for (name, sql) in DATABASE_NAMES
        .into_iter()
        .zip([databases.canvas, databases.analytics])
    {
        let path = root.join(name);
        if path.exists() {
            fs::remove_file(&path).expect("replace closed fixture database");
        }
        let database = rusqlite::Connection::open(path).expect("fixture database");
        studio_runtime_store::sqlite_durability::configure_writer(&database)
            .expect("fixture database durability");
        database
            .execute_batch(&sql)
            .expect("restore frozen database");
    }
}

pub(crate) fn state_bytes(fixture: &str) -> [Vec<u8>; 2] {
    let root = tempfile::tempdir().expect("state fixture directory");
    write_state(root.path(), fixture);
    DATABASE_NAMES.map(|name| fs::read(root.path().join(name)).expect("closed state database"))
}
