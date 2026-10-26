use std::path::PathBuf;

use serde_json::Value;
use studio_runtime_services::terminal_driver::{history_output, listing, output};
use studio_runtime_store::{RuntimeStore, TerminalsRepository};

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Tables {
    user_terminals: Vec<(String, String, String, i64)>,
    user_terminal_output: Vec<(String, i64, i64, String)>,
    user_terminal_receipts: Vec<(String, String, String)>,
    user_terminal_event_cursor: Vec<(String, i64)>,
    user_terminal_decoder_state: Vec<(String, Vec<u8>)>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputCase {
    offset: i64,
    value: Value,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryCase {
    offset: i64,
    limit: i64,
    value: Value,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadCorpus {
    listing: Value,
    output: Vec<OutputCase>,
    history: Vec<HistoryCase>,
    tables: Tables,
    #[serde(rename = "sizes")]
    _sizes: Vec<Value>,
    #[serde(rename = "restart")]
    _restart: Value,
    #[serde(rename = "events")]
    _events: Vec<Value>,
    #[serde(rename = "large")]
    _large: Vec<Value>,
}

async fn seed(store: &RuntimeStore, tables: Tables) {
    store
        .transaction(move |tx| {
            for (id, record, output, offset) in tables.user_terminals {
                tx.execute(
                    "INSERT INTO user_terminals(id,record,output,offset) VALUES(?1,?2,?3,?4)",
                    rusqlite::params![id, record, output, offset],
                )?;
            }
            for (terminal, start, end, text) in tables.user_terminal_output {
                tx.execute(
                    "INSERT INTO user_terminal_output VALUES(?1,?2,?3,?4)",
                    rusqlite::params![terminal, start, end, text],
                )?;
            }
            for (id, signature, result) in tables.user_terminal_receipts {
                tx.execute(
                    "INSERT INTO user_terminal_receipts VALUES(?1,?2,?3)",
                    rusqlite::params![id, signature, result],
                )?;
            }
            for (terminal, sequence) in tables.user_terminal_event_cursor {
                tx.execute(
                    "INSERT INTO user_terminal_event_cursor VALUES(?1,?2)",
                    rusqlite::params![terminal, sequence],
                )?;
            }
            for (terminal, pending) in tables.user_terminal_decoder_state {
                tx.execute(
                    "INSERT INTO user_terminal_decoder_state VALUES(?1,?2)",
                    rusqlite::params![terminal, pending],
                )?;
            }
            Ok(())
        })
        .await
        .expect("seed frozen terminal rows");
}

fn python_table_rows(connection: &rusqlite::Connection, table: &str) -> Vec<Vec<Value>> {
    // The Python oracle predates the Rust-owned output_generation column.
    let columns = if table == "user_terminals" {
        "id,record,output,offset"
    } else {
        "*"
    };
    let sql = format!("SELECT {columns} FROM \"{table}\" ORDER BY rowid");
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

fn assert_python_rows(store: &RuntimeStore, expected: &Tables) {
    let connection = store.test_reader().expect("read Rust state");
    for (table, rows) in [
        ("user_terminals", serde_json::json!(expected.user_terminals)),
        (
            "user_terminal_receipts",
            serde_json::json!(expected.user_terminal_receipts),
        ),
        (
            "user_terminal_output",
            serde_json::json!(expected.user_terminal_output),
        ),
        (
            "user_terminal_event_cursor",
            serde_json::json!(expected.user_terminal_event_cursor),
        ),
        (
            "user_terminal_decoder_state",
            serde_json::json!(expected.user_terminal_decoder_state),
        ),
    ] {
        assert_eq!(
            serde_json::json!(python_table_rows(&connection, table)),
            rows,
            "stored terminal table {table} differs from the frozen corpus"
        );
    }
}

async fn run_corpus() {
    let temp_root = std::env::var_os("CS_TERMINAL_TEST_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&temp_root).expect("tmpfs test state directory");
    let root = tempfile::Builder::new()
        .prefix("terminal-corpus-")
        .tempdir_in(&temp_root)
        .expect("Python-written terminal state");
    let escaped_root = serde_json::to_string(&root.path()).expect("JSON fixture path");
    let fixture = include_str!("terminal_read_corpus.json")
        .replace("$ROOT", &escaped_root[1..escaped_root.len() - 1]);
    let oracle: ReadCorpus = serde_json::from_str(&fixture).expect("frozen terminal corpus");
    let store = std::sync::Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("Rust opens Python-written state"),
    );

    seed(&store, oracle.tables.clone()).await;

    assert_eq!(listing(&store).await.expect("Rust listing"), oracle.listing);
    for case in &oracle.output {
        assert_eq!(
            output(&store, "terminal-corpus", case.offset)
                .await
                .expect("Rust output"),
            case.value,
            "complete output return differs for offset {}",
            case.offset
        );
    }
    for case in &oracle.history {
        assert_eq!(
            history_output(
                std::sync::Arc::clone(&store),
                "terminal-corpus",
                case.offset,
                case.limit
            )
            .await
            .expect("Rust history output"),
            case.value,
            "complete history return differs at offset {}",
            case.offset
        );
    }
    let row = TerminalsRepository::get(&store.test_reader().expect("read"), "terminal-corpus")
        .expect("Rust row")
        .expect("Python-written terminal row");
    assert_eq!(
        row.record.value(),
        &serde_json::from_str::<Value>(&oracle.tables.user_terminals[0].1)
            .expect("recorded terminal JSON")
    );
    assert_eq!(row.output, "alpha-Ж🙂-omega");
    assert_python_rows(&store, &oracle.tables);
}

#[tokio::test]
async fn python_written_terminal_corpus_matches_fast_rust_reads_and_rows() {
    run_corpus().await;
}
