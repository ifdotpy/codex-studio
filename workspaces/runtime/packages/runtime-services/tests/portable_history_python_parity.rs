use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use studio_runtime_services::portable_history::{
    ArchiveDescriptor, ExportInput, NativeHistory, PortableHistoryError, export_history,
    history_context, read_history, validate_history,
};
use studio_runtime_store::RuntimeStore;

#[test]
fn frozen_archive_writer_and_context_are_read_without_loss() {
    let root = isolated_test_root();
    let cases = frozen_archives(root.path());
    assert_eq!(cases.len(), 10, "all archive behavior families are present");
    for case in &cases {
        let name = case.name.as_str();
        let state_root = case.root.as_path();
        let descriptor = ArchiveDescriptor::from_value(
            serde_json::to_value(&case.descriptor).expect("descriptor JSON"),
        )
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
            case.context.as_str(),
            "context excerpt differs from Python for {name}"
        );
    }
    let archive_case = cases
        .iter()
        .find(|case| case.name == "native-pages")
        .expect("tamper witness archive");
    let descriptor = ArchiveDescriptor::from_value(
        serde_json::to_value(&archive_case.descriptor).expect("descriptor JSON"),
    )
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
        read_history(archive_case.root.as_path(), &descriptor).is_err(),
        "modified archive bytes are refused"
    );
    std::fs::write(path, original).expect("restore synthetic archive");
    assert_invalid_footer_counts(&descriptor);
    assert_invalid_footer_order(&descriptor);
    assert_invalid_content_hash(&descriptor);
    assert_invalid_mutated_archive(&descriptor, |records| {
        records.insert(
            0,
            json!({
                "kind":"studio_item",
                "id":"preheader-item",
                "created":0.5,
                "item":{"id":"preheader-item","role":"user","text":"before header"}
            }),
        );
    });
    assert_invalid_mutated_archive(&descriptor, |records| {
        let turn_page = records
            .iter_mut()
            .find(|record| record["kind"] == "native_turn_page")
            .expect("native turn page");
        turn_page["page"]["data"][0]["threadId"] = Value::Null;
    });
    assert_invalid_mutated_archive(&descriptor, |records| {
        let turn_page = records
            .iter_mut()
            .find(|record| record["kind"] == "native_turn_page")
            .expect("native turn page");
        turn_page["page"]["data"][0]["itemsView"] = json!(false);
    });
    let original = std::fs::read(path).expect("original archive bytes");
    let mut truncated = original.clone();
    assert_eq!(truncated.pop(), Some(b'\n'));
    std::fs::write(path, &truncated).expect("truncate synthetic footer");
    let truncated_descriptor = refreshed_descriptor(&descriptor, &truncated);
    assert!(validate_history(archive_case.root.as_path(), &truncated_descriptor).is_err());
    std::fs::write(path, original).expect("restore synthetic archive");
    let ordinary = cases
        .iter()
        .find(|case| case.name == "ordinary-and-input")
        .expect("input event fixture");
    let path = ordinary.descriptor.path();
    let archive = std::fs::read_to_string(path).expect("Python archive JSONL");
    assert!(
        archive.contains("original input 😀"),
        "full event text retained"
    );
    assert!(archive.contains("日本語.png"), "asset metadata retained");
    let restored = cases
        .iter()
        .find(|case| case.name == "restored-item")
        .expect("restored fixture");
    let restored_context = restored.context.as_str();
    assert!(!restored_context.contains("must not enter excerpt"));
    let native = cases
        .iter()
        .find(|case| case.name == "native-pages")
        .expect("native page fixture");
    assert!(
        native
            .context
            .contains("Python-written native assistant text")
    );
    let item_page = cases
        .iter()
        .find(|case| case.name == "native-item-page")
        .expect("native item-page fixture");
    assert!(
        item_page
            .context
            .contains("Python-written native item-page text")
    );
    let unmaterialized = cases
        .iter()
        .find(|case| case.name == "native-unmaterialized")
        .expect("unmaterialized fixture");
    let unmaterialized_path = unmaterialized.descriptor.path();
    assert!(
        std::fs::read_to_string(unmaterialized_path)
            .expect("unmaterialized archive")
            .contains("thread/turns/list is unavailable before first user message")
    );
    let unmaterialized_descriptor = ArchiveDescriptor::from_value(
        serde_json::to_value(&unmaterialized.descriptor).expect("descriptor JSON"),
    )
    .expect("unmaterialized descriptor");
    assert_invalid_mutated_archive(&unmaterialized_descriptor, |records| {
        let marker = records
            .iter_mut()
            .find(|record| record["kind"] == "native_unmaterialized")
            .expect("unmaterialized marker");
        marker["error"]["message"] = json!("not a validated RPC error");
    });
    let chain = cases
        .iter()
        .find(|case| case.name == "ancestor-chain")
        .expect("ancestor chain fixture");
    assert_eq!(
        chain.descriptor.to_value().expect("descriptor JSON")["counts"]["ancestor"],
        json!(2)
    );
    for name in ["restored-empty-list", "restored-empty-object"] {
        let case = cases
            .iter()
            .find(|case| case.name == name)
            .expect("empty restored fixture");
        assert!(
            case.context
                .as_str()
                .contains(if name == "restored-empty-list" {
                    "empty list stays in excerpt"
                } else {
                    "empty object stays in excerpt"
                })
        );
    }
}

#[tokio::test]
async fn rust_writer_reads_frozen_state_and_refusal_publishes_nothing() {
    let root = isolated_test_root();
    let state: FrozenState =
        serde_json::from_str(include_str!("portable_history/fixtures/state.json"))
            .expect("frozen state identity");
    for (name, sql) in [
        (
            "canvas.sqlite3",
            include_str!("portable_history/fixtures/canvas.sql"),
        ),
        (
            "analytics.sqlite3",
            include_str!("portable_history/fixtures/analytics.sql"),
        ),
    ] {
        rusqlite::Connection::open(root.path().join(name))
            .expect("fixture database opens")
            .execute_batch(&format!(
                "PRAGMA journal_mode=WAL; {}",
                sql.replace("<fixture-root>", &root.path().to_string_lossy())
            ))
            .expect("frozen Python state loads");
    }
    let agent_id = state.agent;
    let epoch = state.epoch;
    let store = RuntimeStore::open(Some(root.path()))
        .await
        .expect("Rust opens Python state");
    let base_input = || ExportInput {
        agent_id: agent_id.clone(),
        account_key: "default".to_owned(),
        epoch,
        thread_id: Some("native-thread".to_owned()),
        source_settings: state.settings.clone(),
        source_connection_generation: Some(12),
        transfer_id: "rust-transfer-1".to_owned(),
        native_history: None,
    };
    let input_with_records = |transfer_id: &str, records: Vec<Value>| {
        let mut input = base_input();
        input.transfer_id = transfer_id.to_owned();
        input.native_history = Some(NativeHistory {
            account_key: "default".to_owned(),
            thread_id: "native-thread".to_owned(),
            epoch,
            settings: input.source_settings.clone(),
            connection_generation: 12,
            records: Box::new(records.into_iter().map(Ok::<_, PortableHistoryError>)),
            checked_empty_proof: None,
        });
        input
    };
    let mut stale = base_input();
    stale.epoch += 1;
    assert!(
        matches!(
            export_history(&store, stale).await,
            Err(PortableHistoryError::SourceChanged)
        ),
        "stale source refused"
    );
    let mut stale_settings = base_input();
    stale_settings.source_settings["role"] = json!("changed-after-capture");
    assert!(
        matches!(
            export_history(&store, stale_settings).await,
            Err(PortableHistoryError::SourceChanged)
        ),
        "changed source settings refused"
    );
    let mut stale_provider_settings = base_input();
    stale_provider_settings.source_settings["provider"] = json!("codex");
    assert!(
        matches!(
            export_history(&store, stale_provider_settings).await,
            Err(PortableHistoryError::SourceChanged)
        ),
        "changed account-transfer provider settings refused"
    );
    assert!(
        !root.path().join("portable-history").exists(),
        "source refusal performs no filesystem write"
    );

    assert!(
        export_history(&store, base_input()).await.is_err(),
        "missing native capture refused"
    );
    assert!(
        !root.path().join("portable-history").exists(),
        "native refusal performs no filesystem write"
    );

    let native_records = vec![
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
    ];

    for (label, bad_thread, bad_view) in [
        ("null threadId", Some(Value::Null), None),
        ("wrong threadId type", Some(json!(false)), None),
        ("null itemsView", None, Some(Value::Null)),
        ("wrong itemsView type", None, Some(json!(17))),
    ] {
        let mut records = native_records.clone();
        let page = &mut records[1]["page"]["data"][0];
        if let Some(value) = bad_thread {
            page["threadId"] = value;
        }
        if let Some(value) = bad_view {
            page["itemsView"] = value;
        }
        assert!(
            matches!(
                export_history(
                    &store,
                    input_with_records(&format!("invalid-native-{label}"), records)
                )
                .await,
                Err(PortableHistoryError::InvalidArchive(_))
            ),
            "native {label} is refused"
        );
    }
    let proofless = input_with_records(
        "proofless-unmaterialized",
        vec![
            json!({"kind":"native_thread","thread":{"id":"native-thread"}}),
            json!({"kind":"native_unmaterialized","threadId":"native-thread"}),
        ],
    );
    assert!(
        matches!(
            export_history(&store, proofless).await,
            Err(PortableHistoryError::InvalidArchive(
                "native unmaterialized evidence"
            ))
        ),
        "proofless unmaterialized marker is refused"
    );
    let padding = "x".repeat(64 * 1024 * 1024 + 1);
    let oversized = input_with_records(
        "oversized-native-record",
        vec![json!({
            "kind":"native_thread",
            "thread":{"id":"native-thread","padding":padding}
        })],
    );
    assert!(matches!(
        export_history(&store, oversized).await,
        Err(PortableHistoryError::LimitExceeded {
            limit: "record byte",
            record_index,
            actual,
            maximum,
        }) if record_index > 0 && actual > maximum && maximum == 64 * 1024 * 1024
    ));

    let descriptor = export_history(
        &store,
        input_with_records("rust-transfer-1", native_records),
    )
    .await
    .expect("Rust publishes archive");
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
    assert!(context.contains("empty afterRestore fixture 1"));
    assert!(context.contains("empty afterRestore fixture 2"));
    let retry = export_history(&store, base_input())
        .await
        .expect("published winner is retry safe");
    assert_eq!(retry, descriptor);
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

fn refreshed_descriptor(descriptor: &ArchiveDescriptor, bytes: &[u8]) -> ArchiveDescriptor {
    let mut value = descriptor.to_value().expect("descriptor JSON");
    value["bytes"] = json!(bytes.len() as u64);
    value["sha256"] = json!(studio_runtime_store::sha256_hex(bytes));
    ArchiveDescriptor::from_value(value).expect("refreshed descriptor")
}

fn assert_invalid_mutated_archive(
    descriptor: &ArchiveDescriptor,
    mutate: impl FnOnce(&mut Vec<Value>),
) {
    let original = std::fs::read(descriptor.path()).expect("original archive");
    let mut lines: Vec<Value> = String::from_utf8(original.clone())
        .expect("JSONL UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("archive JSON line"))
        .collect();
    let mut footer = lines.pop().expect("footer line");
    mutate(&mut lines);
    let mut counts = std::collections::BTreeMap::<String, u64>::new();
    for record in &lines {
        let kind = record["kind"].as_str().expect("record kind");
        *counts.entry(kind.to_owned()).or_default() += 1;
    }
    footer["counts"] = serde_json::to_value(&counts).expect("footer record counts");
    let mut content = Vec::new();
    for record in lines {
        serde_json::to_writer(&mut content, &record).expect("encode content line");
        content.push(b'\n');
    }
    footer["contentSha256"] = json!(studio_runtime_store::sha256_hex(&content));
    serde_json::to_writer(&mut content, &footer).expect("encode footer");
    content.push(b'\n');
    std::fs::write(descriptor.path(), &content).expect("write mutated archive");
    let mut descriptor_value = descriptor.to_value().expect("descriptor JSON");
    descriptor_value["counts"] = serde_json::to_value(counts).expect("descriptor counts");
    descriptor_value["bytes"] = json!(content.len() as u64);
    descriptor_value["sha256"] = json!(studio_runtime_store::sha256_hex(&content));
    let updated = ArchiveDescriptor::from_value(descriptor_value).expect("updated descriptor");
    let root = descriptor
        .path()
        .parent()
        .and_then(Path::parent)
        .expect("state root");
    let result = validate_history(root, &updated);
    std::fs::write(descriptor.path(), original).expect("restore original archive");
    assert!(result.is_err(), "mutated archive must be refused");
}

fn assert_invalid_footer_counts(descriptor: &ArchiveDescriptor) {
    let original = std::fs::read(descriptor.path()).expect("original archive");
    let mut lines: Vec<Value> = String::from_utf8(original.clone())
        .expect("JSONL UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("archive JSON line"))
        .collect();
    let mut footer = lines.pop().expect("footer");
    footer["counts"]["header"] = json!(2);
    let mut bytes = Vec::new();
    for line in lines.into_iter().chain(std::iter::once(footer)) {
        serde_json::to_writer(&mut bytes, &line).expect("encode footer count mutation");
        bytes.push(b'\n');
    }
    std::fs::write(descriptor.path(), &bytes).expect("write footer count mutation");
    let updated = refreshed_descriptor(descriptor, &bytes);
    let root = descriptor
        .path()
        .parent()
        .and_then(Path::parent)
        .expect("state root");
    let result = validate_history(root, &updated);
    std::fs::write(descriptor.path(), original).expect("restore original archive");
    assert!(result.is_err(), "footer count mismatch must be refused");
}

fn assert_invalid_content_hash(descriptor: &ArchiveDescriptor) {
    let original = std::fs::read(descriptor.path()).expect("original archive");
    let mut lines: Vec<Value> = String::from_utf8(original.clone())
        .expect("JSONL UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("archive JSON line"))
        .collect();
    let footer = lines.pop().expect("footer");
    let turn_page = lines
        .iter_mut()
        .find(|record| record["kind"] == "native_turn_page")
        .expect("native turn page");
    turn_page["page"]["data"][0]["items"][0]["text"] = json!("changed with a refreshed descriptor");
    let mut bytes = Vec::new();
    for record in lines.into_iter().chain(std::iter::once(footer)) {
        serde_json::to_writer(&mut bytes, &record).expect("encode content hash mutation");
        bytes.push(b'\n');
    }
    std::fs::write(descriptor.path(), &bytes).expect("write content hash mutation");
    let updated = refreshed_descriptor(descriptor, &bytes);
    let root = descriptor
        .path()
        .parent()
        .and_then(Path::parent)
        .expect("state root");
    let result = validate_history(root, &updated);
    std::fs::write(descriptor.path(), original).expect("restore original archive");
    assert!(
        result.is_err(),
        "footer content hash mismatch must be refused"
    );
}

fn assert_invalid_footer_order(descriptor: &ArchiveDescriptor) {
    let original = std::fs::read(descriptor.path()).expect("original archive");
    let mut lines: Vec<Value> = String::from_utf8(original.clone())
        .expect("JSONL UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("archive JSON line"))
        .collect();
    let footer = lines.pop().expect("footer");
    let mut bytes = Vec::new();
    serde_json::to_writer(&mut bytes, &footer).expect("encode first footer");
    bytes.push(b'\n');
    for line in lines {
        serde_json::to_writer(&mut bytes, &line).expect("encode record after footer");
        bytes.push(b'\n');
    }
    std::fs::write(descriptor.path(), &bytes).expect("write footer order mutation");
    let updated = refreshed_descriptor(descriptor, &bytes);
    let root = descriptor
        .path()
        .parent()
        .and_then(Path::parent)
        .expect("state root");
    let result = validate_history(root, &updated);
    std::fs::write(descriptor.path(), original).expect("restore original archive");
    assert!(result.is_err(), "record after footer must be refused");
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenState {
    agent: String,
    epoch: i64,
    settings: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenCase {
    name: String,
    descriptor: ArchiveDescriptor,
    context: String,
    archives: Vec<FrozenArchive>,
    #[serde(skip)]
    root: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenArchive {
    descriptor: RelocatedDescriptor,
    records: Vec<FrozenRecord>,
    counts: BTreeMap<String, u64>,
}

#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum FrozenRecord {
    Line { line: String },
    Ancestor { ancestor: String },
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RelocatedDescriptor {
    version: u64,
    path: PathBuf,
    sha256: String,
    bytes: u64,
    counts: BTreeMap<String, u64>,
    source: studio_runtime_services::portable_history::HistorySource,
    transfer_id: String,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RelocatedRecord<'a> {
    Ancestor {
        archive: &'a ArchiveDescriptor,
    },
    Footer {
        counts: &'a BTreeMap<String, u64>,
        #[serde(rename = "contentSha256")]
        content_sha256: String,
    },
}

fn frozen_archives(root: &Path) -> Vec<FrozenCase> {
    let mut cases: Vec<FrozenCase> =
        serde_json::from_str(include_str!("portable_history/fixtures/archives.json"))
            .expect("frozen Python archive corpus");
    for case in &mut cases {
        case.root = root.join(&case.name);
        let directory = case.root.join("portable-history");
        std::fs::create_dir_all(&directory).expect("fixture archive directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            const PRIVATE_ARCHIVE_DIRECTORY_MODE: u32 = 0o700;
            std::fs::set_permissions(
                &directory,
                std::fs::Permissions::from_mode(PRIVATE_ARCHIVE_DIRECTORY_MODE),
            )
            .expect("private archive fixture directory");
        }
        let mut descriptors = BTreeMap::<String, ArchiveDescriptor>::new();
        for archive in &mut case.archives {
            let filename = archive
                .descriptor
                .path
                .file_name()
                .expect("archive filename")
                .to_str()
                .expect("UTF-8 archive filename")
                .to_owned();
            let mut bytes = Vec::new();
            for record in &archive.records {
                match record {
                    FrozenRecord::Line { line } => bytes.extend_from_slice(line.as_bytes()),
                    FrozenRecord::Ancestor { ancestor } => {
                        serde_json::to_writer(
                            &mut bytes,
                            &RelocatedRecord::Ancestor {
                                archive: descriptors
                                    .get(ancestor)
                                    .expect("earlier ancestor fixture"),
                            },
                        )
                        .expect("relocate ancestor record");
                    }
                }
                bytes.push(b'\n');
            }
            let content_sha256 = studio_runtime_store::sha256_hex(&bytes);
            serde_json::to_writer(
                &mut bytes,
                &RelocatedRecord::Footer {
                    counts: &archive.counts,
                    content_sha256,
                },
            )
            .expect("relocated archive footer");
            bytes.push(b'\n');
            archive.descriptor.path = directory.join(&filename);
            archive.descriptor.bytes = bytes.len() as u64;
            archive.descriptor.sha256 = studio_runtime_store::sha256_hex(&bytes);
            let mut file =
                tempfile::NamedTempFile::new_in(&directory).expect("private archive fixture");
            std::io::Write::write_all(&mut file, &bytes).expect("write relocated archive");
            file.persist(&archive.descriptor.path)
                .expect("publish archive fixture");
            let descriptor: ArchiveDescriptor = serde_json::from_value(
                serde_json::to_value(&archive.descriptor).expect("relocated descriptor JSON"),
            )
            .expect("relocated descriptor");
            descriptors.insert(filename, descriptor);
        }
        case.descriptor = descriptors
            .remove(
                case.descriptor
                    .path()
                    .file_name()
                    .expect("case archive filename")
                    .to_str()
                    .expect("UTF-8 case filename"),
            )
            .expect("case archive fixture");
        let descriptor: RelocatedDescriptor = serde_json::from_value(
            serde_json::to_value(&case.descriptor).expect("descriptor JSON"),
        )
        .expect("typed relocated descriptor");
        case.context = case
            .context
            .replace("<archive-path>", &descriptor.path.to_string_lossy())
            .replace("<archive-sha256>", &descriptor.sha256);
    }
    cases
}
