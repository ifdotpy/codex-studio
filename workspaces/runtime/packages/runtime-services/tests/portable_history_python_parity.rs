use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use studio_runtime_services::portable_history::{
    ArchiveDescriptor, ExportInput, NativeHistory, PortableHistoryError, export_history,
    history_context, read_history, validate_history,
};
use studio_runtime_store::RuntimeStore;

#[test]
fn python_archive_writer_and_context_are_read_without_loss() {
    let root = isolated_test_root();
    let output = Command::new(python_executable())
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/portable_history/producer.py"))
        .arg(server_source())
        .arg(root.path())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("prepared Python archive producer starts");
    assert!(
        output.status.success(),
        "Python archive producer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cases: Vec<Value> = serde_json::from_slice(&output.stdout).expect("Python corpus JSON");
    assert_eq!(cases.len(), 5, "all archive behavior families are present");
    for case in &cases {
        let name = case["name"].as_str().expect("case name");
        let state_root = Path::new(case["root"].as_str().expect("archive root"));
        let descriptor = ArchiveDescriptor::from_value(case["descriptor"].clone())
            .unwrap_or_else(|error| panic!("Rust rejected Python descriptor {name}: {error}"));
        let validated = validate_history(state_root, &descriptor)
            .unwrap_or_else(|error| panic!("Rust rejected Python archive {name}: {error}"));
        assert_eq!(validated, descriptor, "descriptor round trip for {name}");
        let mut archive = read_history(state_root, &descriptor)
            .unwrap_or_else(|error| panic!("Rust read failed for {name}: {error}"));
        let mut contents = String::new();
        archive
            .read_to_string(&mut contents)
            .unwrap_or_else(|error| panic!("Rust archive read failed for {name}: {error}"));
        assert!(
            contents.ends_with("\n"),
            "Python JSONL footer read for {name}"
        );
        assert_eq!(
            history_context(state_root, &descriptor)
                .unwrap_or_else(|error| panic!("Rust context failed for {name}: {error}")),
            case["context"].as_str().expect("Python context"),
            "context excerpt differs from Python for {name}"
        );
    }
    let archive_case = cases
        .iter()
        .find(|case| case["name"] == "native-pages")
        .expect("tamper witness archive");
    let descriptor = ArchiveDescriptor::from_value(archive_case["descriptor"].clone())
        .expect("Python descriptor");
    let path = descriptor.path();
    let original = std::fs::read(path).expect("archive bytes");
    let mut tampered = original.clone();
    let original_text = b"Python-written native assistant text";
    let changed_text = b"PythoN-written native assistant text";
    let start = tampered
        .windows(original_text.len())
        .position(|window| window == original_text)
        .expect("native text in archive");
    tampered[start..start + original_text.len()].copy_from_slice(changed_text);
    std::fs::write(path, tampered).expect("tamper synthetic archive");
    assert!(
        read_history(
            Path::new(archive_case["root"].as_str().expect("archive root")),
            &descriptor
        )
        .is_err(),
        "modified archive bytes are refused"
    );
    std::fs::write(path, original).expect("restore synthetic archive");
    let ordinary = cases
        .iter()
        .find(|case| case["name"] == "ordinary-and-input")
        .expect("input event fixture");
    let path = ordinary["descriptor"]["path"].as_str().expect("path");
    let archive = std::fs::read_to_string(path).expect("Python archive JSONL");
    assert!(
        archive.contains("original input 😀"),
        "full event text retained"
    );
    assert!(archive.contains("日本語.png"), "asset metadata retained");
    let restored = cases
        .iter()
        .find(|case| case["name"] == "restored-item")
        .expect("restored fixture");
    let restored_context = restored["context"].as_str().expect("context");
    assert!(!restored_context.contains("must not enter excerpt"));
    let native = cases
        .iter()
        .find(|case| case["name"] == "native-pages")
        .expect("native page fixture");
    assert!(
        native["context"]
            .as_str()
            .expect("context")
            .contains("Python-written native assistant text")
    );
}

#[test]
fn rust_writer_reads_python_state_and_refusal_publishes_nothing() {
    let root = isolated_test_root();
    let output = Command::new(python_executable())
        .arg(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/portable_history/store_producer.py"),
        )
        .arg(server_source())
        .arg(root.path())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("prepared Python state producer starts");
    assert!(
        output.status.success(),
        "Python state producer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state: Value = serde_json::from_slice(&output.stdout).expect("Python state identity");
    let agent_id = state["agent"].as_str().expect("agent ID").to_owned();
    let epoch = state["epoch"].as_i64().expect("agent epoch");
    let store = RuntimeStore::open(Some(root.path())).expect("Rust opens Python state");
    let mut input = ExportInput {
        agent_id: agent_id.clone(),
        account_key: "default".to_owned(),
        epoch,
        thread_id: Some("native-thread".to_owned()),
        source_settings: state["settings"].clone(),
        source_connection_generation: Some(12),
        transfer_id: "rust-transfer-1".to_owned(),
        native_history: None,
    };
    let mut stale = input.clone();
    stale.epoch += 1;
    assert!(
        matches!(
            export_history(&store, stale),
            Err(PortableHistoryError::SourceChanged)
        ),
        "stale source refused"
    );
    let mut stale_settings = input.clone();
    stale_settings.source_settings["role"] = json!("changed-after-capture");
    assert!(
        matches!(
            export_history(&store, stale_settings),
            Err(PortableHistoryError::SourceChanged)
        ),
        "changed source settings refused"
    );
    let mut stale_provider_settings = input.clone();
    stale_provider_settings.source_settings["provider"] = json!("codex");
    assert!(
        matches!(
            export_history(&store, stale_provider_settings),
            Err(PortableHistoryError::SourceChanged)
        ),
        "changed account-transfer provider settings refused"
    );
    assert!(
        !root.path().join("portable-history").exists(),
        "source refusal performs no filesystem write"
    );

    assert!(
        export_history(&store, input.clone()).is_err(),
        "missing native capture refused"
    );
    assert!(
        !root.path().join("portable-history").exists(),
        "native refusal performs no filesystem write"
    );

    input.native_history = Some(NativeHistory {
        account_key: "default".to_owned(),
        thread_id: "native-thread".to_owned(),
        epoch,
        settings: input.source_settings.clone(),
        connection_generation: 12,
        records: vec![
            json!({"kind":"native_thread","thread":{"id":"native-thread"}}),
            json!({
                "kind":"native_turn_page",
                "request":{"threadId":"native-thread","limit":100,"sortDirection":"asc","itemsView":"full"},
                "page":{"data":[{
                    "id":"native-turn-1",
                    "threadId":"native-thread",
                    "itemsView":"full",
                    "items":[{
                        "id":"native-item-1",
                        "type":"agentMessage",
                        "threadId":"native-thread",
                        "turnId":"native-turn-1",
                        "text":"Rust captured native history"
                    }]
                }],"nextCursor":null}
            }),
        ],
        checked_empty_proof: None,
    });

    let descriptor = export_history(&store, input.clone()).expect("Rust publishes archive");
    let validated = validate_history(root.path(), &descriptor).expect("Rust reads its archive");
    assert_eq!(validated, descriptor);
    let mut archive_reader = read_history(root.path(), &descriptor).expect("complete archive read");
    let mut contents = String::new();
    archive_reader
        .read_to_string(&mut contents)
        .expect("archive UTF-8 text");
    let archive = std::fs::read_to_string(descriptor.path()).expect("archive JSONL");
    assert_eq!(
        contents, archive,
        "public read returns complete archive bytes"
    );
    assert!(archive.contains("Python-written full transcript 😀"));
    assert!(archive.contains("Python-written original input"));
    assert!(archive.contains("archive-fixture \\\"日本語.png"));
    let context = history_context(root.path(), &descriptor).expect("bounded context");
    assert!(
        !context.contains("restored content must stay out of context"),
        "restored items are excluded from the historical excerpt"
    );
    input.native_history = None;
    let retry = export_history(&store, input).expect("published winner is retry safe");
    assert_eq!(retry, descriptor);
}

fn python_executable() -> PathBuf {
    std::env::var_os("CODEX_AGENTS_PYTHON")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("python3"))
}

fn isolated_test_root() -> tempfile::TempDir {
    if let Some(parent) = std::env::var_os("STUDIO_PORTABLE_HISTORY_TEST_ROOT") {
        let parent = PathBuf::from(parent);
        std::fs::create_dir_all(&parent).expect("isolated test root exists");
        tempfile::Builder::new()
            .prefix("portable-history-")
            .tempdir_in(parent)
            .expect("isolated tmpfs test directory")
    } else {
        tempfile::tempdir().expect("isolated corpus root")
    }
}

fn server_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src")
}
