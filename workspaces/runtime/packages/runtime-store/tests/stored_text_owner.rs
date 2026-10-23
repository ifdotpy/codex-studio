//! Every source file that names an owned stored-text table is listed here with
//! the reason it may do so. A column owned by `studio_runtime_store::stored_text`
//! can hold a compressed BLOB, so a new reader must decode through that module;
//! this check makes each new mention a reviewed decision.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Files under `workspaces/runtime/{packages,apps}/*/src` that name
/// `runtime_item_fulltext`, and why. Paths are relative to `workspaces/runtime`.
const FULLTEXT_MENTIONS: &[(&str, &str)] = &[
    (
        "packages/runtime-store/src/stored_text.rs",
        "format owner; names its owned column",
    ),
    (
        "packages/runtime-store/src/repositories/transcript.rs",
        "reader and writer of body: TranscriptRepository::full_text and put_*",
    ),
    (
        "packages/runtime-store/src/stored_text_compaction.rs",
        "compaction pass: converts plain values with compare and set and read-back checks",
    ),
    (
        "packages/runtime-services/src/stored_text_compaction.rs",
        "tests that insert plain rows and read them through StoredText",
    ),
    ("packages/runtime-store/src/feature_schema.rs", "table DDL"),
    (
        "packages/runtime-store/src/repositories/claude_retry.rs",
        "schema requirement: table and columns exist",
    ),
    (
        "packages/runtime-store/src/repositories/context_repair.rs",
        "schema requirement: table and columns exist",
    ),
    (
        "packages/runtime-store/src/repositories/notification_dispatch.rs",
        "schema requirement: table and columns exist",
    ),
    (
        "packages/runtime-services/src/context_repair.rs",
        "schema requirement: table exists",
    ),
    (
        "packages/runtime-services/src/transcript.rs",
        "`body IS NOT NULL` existence probe, which is true for both stored forms",
    ),
    (
        "packages/runtime-services/src/notification_dispatch.rs",
        "test fixture DDL and a test reader that decodes through StoredText",
    ),
    (
        "packages/runtime-services/src/account_worker/tests.rs",
        "test readers that decode through StoredText",
    ),
    (
        "packages/runtime-services/src/account_worker/tests/crash_matrix.rs",
        "test reader that decodes through StoredText",
    ),
];

fn runtime_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn rust_sources(directory: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            rust_sources(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

#[test]
fn every_source_that_names_the_fulltext_table_is_a_reviewed_reader() {
    let root = runtime_root();
    let mut sources = Vec::new();
    for group in ["packages", "apps"] {
        let Ok(crates) = std::fs::read_dir(root.join(group)) else {
            continue;
        };
        for entry in crates {
            rust_sources(
                &entry.expect("crate entry").path().join("src"),
                &mut sources,
            );
        }
    }
    assert!(!sources.is_empty(), "runtime sources are found");
    let mut actual = BTreeSet::new();
    for source in sources {
        let text = std::fs::read_to_string(&source).expect("source reads");
        if text.contains("runtime_item_fulltext") {
            let relative = source
                .strip_prefix(&root)
                .expect("source under the runtime root")
                .to_string_lossy()
                .replace('\\', "/");
            actual.insert(relative);
        }
    }
    let expected = FULLTEXT_MENTIONS
        .iter()
        .map(|(path, _)| (*path).to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        actual, expected,
        "a source that names runtime_item_fulltext must read `body` only through \
         TranscriptRepository::full_text or studio_runtime_store::stored_text, and \
         must be listed in FULLTEXT_MENTIONS with its reason"
    );
}
