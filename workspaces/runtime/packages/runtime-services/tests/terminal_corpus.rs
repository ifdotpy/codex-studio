use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use studio_runtime_services::terminal_driver::{history_output, listing, output};
use studio_runtime_store::{RuntimeStore, TerminalsRepository};

#[path = "support/prepared_python.rs"]
mod prepared_python;

fn run_python_oracle(root: &Path, full: bool) -> Value {
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("isolated oracle home");
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
    let python = prepared_python::prepared_python(&source);
    let result = Command::new(python)
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/terminal_oracle.py"))
        .arg(root)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("TMPDIR", root)
        .env("CODEX_NODE", "/usr/bin/node")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("STUDIO_RUNTIME_CORPUS_FULL", if full { "1" } else { "0" })
        .output()
        .expect("start isolated Python terminal oracle");
    assert!(
        result.status.success(),
        "Python terminal oracle failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).expect("complete Python oracle JSON")
}

fn python_table_rows(connection: &rusqlite::Connection, table: &str) -> Vec<Vec<Value>> {
    let sql = format!("SELECT * FROM \"{table}\" ORDER BY rowid");
    let mut statement = connection
        .prepare(&sql)
        .expect("prepare Python-written table");
    let count = statement.column_count();
    statement
        .query_map([], |row| {
            (0..count)
                .map(|index| {
                    let value: rusqlite::types::Value = row.get(index)?;
                    Ok(match value {
                        rusqlite::types::Value::Null => Value::Null,
                        rusqlite::types::Value::Integer(value) => Value::from(value),
                        rusqlite::types::Value::Real(value) => Value::from(value),
                        rusqlite::types::Value::Text(value) => Value::String(value),
                        rusqlite::types::Value::Blob(value) => {
                            Value::Array(value.into_iter().map(Value::from).collect())
                        }
                    })
                })
                .collect()
        })
        .expect("read Python-written table")
        .map(|row| row.expect("valid Python-written row"))
        .collect()
}

fn assert_python_rows(store: &RuntimeStore, expected: &Value) {
    let connection = store.read().expect("read Rust state");
    for table in [
        "user_terminals",
        "user_terminal_receipts",
        "user_terminal_output",
        "user_terminal_event_cursor",
        "user_terminal_decoder_state",
    ] {
        assert_eq!(
            serde_json::json!(python_table_rows(&connection, table)),
            expected["tables"][table],
            "stored terminal table {table} differs from Python"
        );
    }
}

fn run_corpus(full: bool) {
    let temp_root = Path::new("/dev/shm/cs-rust-terminals-tests");
    std::fs::create_dir_all(temp_root).expect("tmpfs test state directory");
    let root = tempfile::Builder::new()
        .prefix("terminal-corpus-")
        .tempdir_in(temp_root)
        .expect("Python-written terminal state");
    let oracle = run_python_oracle(root.path(), full);
    let store = RuntimeStore::open(Some(root.path())).expect("Rust opens Python-written state");

    assert_eq!(listing(&store).expect("Rust listing"), oracle["listing"]);
    for case in oracle["output"].as_array().expect("Python output cases") {
        assert_eq!(
            output(
                &store,
                "terminal-corpus",
                case["offset"].as_i64().expect("output offset")
            )
            .expect("Rust output"),
            case["value"],
            "complete output return differs for offset {}",
            case["offset"]
        );
    }
    for case in oracle["history"].as_array().expect("Python history cases") {
        assert_eq!(
            history_output(
                &store,
                "terminal-corpus",
                case["offset"].as_i64().expect("history offset"),
                case["limit"].as_i64().expect("history limit")
            )
            .expect("Rust history output"),
            case["value"],
            "complete history return differs for case {case}"
        );
    }
    let row = TerminalsRepository::get(&store.read().expect("read"), "terminal-corpus")
        .expect("Rust row")
        .expect("Python-written terminal row");
    assert_eq!(
        row.record.value(),
        oracle["tables"]["user_terminals"][0][1]
            .as_str()
            .map(|raw| serde_json::from_str::<Value>(raw).expect("Python record JSON"))
            .as_ref()
            .expect("Python record")
    );
    assert_eq!(row.output, "alpha-Ж🙂-omega");
    assert_python_rows(&store, &oracle);
    let mut expected_events = vec![serde_json::json!({
        "kind":"terminal", "terminalId":"terminal-corpus"
    })];
    if full {
        expected_events.push(serde_json::json!({
            "kind":"terminal", "terminalId":"terminal-large"
        }));
    }
    assert_eq!(oracle["events"], serde_json::json!(expected_events));

    let sizes = oracle["sizes"].as_array().expect("dimension cases");
    assert_eq!(sizes.len(), 8);
    assert_eq!(sizes[0]["value"], serde_json::json!({"cols":100,"rows":28}));
    assert_eq!(sizes[1]["value"], serde_json::json!({"cols":2,"rows":1}));
    assert_eq!(
        sizes[2]["value"],
        serde_json::json!({"cols":1000,"rows":500})
    );
    assert!(sizes[3..].iter().all(|case| case.get("error").is_some()));
    assert_eq!(oracle["restart"]["status"], "exited");
    assert_eq!(
        oracle["restart"]["error"],
        "The server restarted. This shell cannot be resumed."
    );

    if full {
        let large = oracle["large"]
            .as_array()
            .expect("full bounded-history corpus");
        assert_eq!(large.len(), 3);
        assert_eq!(large[0]["offset"], 1_048_580);
        assert_eq!(
            large[0]["text"]
                .as_str()
                .expect("retained suffix")
                .chars()
                .count(),
            1_048_576
        );
        assert_eq!(large[1]["text"], "xxxxxxx");
        assert_eq!(large[2]["text"], "xxxx");
    } else {
        assert_eq!(oracle["large"], serde_json::json!([]));
    }
}

#[test]
fn python_written_terminal_corpus_matches_fast_rust_reads_and_rows() {
    run_corpus(false);
}

#[test]
fn python_written_terminal_corpus_matches_full_bounds_and_paging() {
    if std::env::var("STUDIO_RUNTIME_CORPUS_FULL").as_deref() != Ok("1") {
        return;
    }
    run_corpus(true);
}
