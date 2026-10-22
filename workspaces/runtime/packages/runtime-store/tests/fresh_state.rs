use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde_json::{Value, json};
use studio_runtime_store::{RuntimeStore, StoreError};

#[tokio::test]
async fn open_recreates_the_task_indexes_rust_readers_rely_on() {
    let state_directory = temporary_state("task-index");
    let state = state_directory.path();
    drop(
        RuntimeStore::open(Some(state))
            .await
            .expect("Rust creates an empty state"),
    );
    let index_exists = || {
        Connection::open(state.join("canvas.sqlite3"))
            .expect("canvas opens")
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                 WHERE type='index' AND name='runtime_task_agent_created_id')",
                [],
                |row| row.get::<_, bool>(0),
            )
            .expect("catalog reads")
    };
    assert!(index_exists());
    Connection::open(state.join("canvas.sqlite3"))
        .expect("canvas opens")
        .execute_batch("DROP INDEX runtime_task_agent_created_id")
        .expect("index drops");
    assert!(!index_exists());
    drop(
        RuntimeStore::open(Some(state))
            .await
            .expect("Rust reopens the state"),
    );
    assert!(index_exists(), "open recreates the index");
}

#[tokio::test]
async fn partial_database_set_is_refused_without_changing_existing_files() {
    let state_directory = temporary_state("partial");
    let state = state_directory.path();
    let canvas = state.join("canvas.sqlite3");
    fs::write(&canvas, b"partial database sentinel").expect("sentinel writes");
    let before = filesystem_snapshot(state);

    let error = match RuntimeStore::open(Some(state)).await {
        Ok(_) => panic!("partial state must be refused"),
        Err(error) => error,
    };
    assert!(
        matches!(error, StoreError::SchemaMismatch(message) if message.contains("partial state database") && message.contains("canvas.sqlite3"))
    );
    assert_eq!(filesystem_snapshot(state), before);
    assert!(!state.join("runtime.lock").exists());
    assert!(!state.join("analytics.sqlite3").exists());
}

#[tokio::test]
async fn foreign_files_are_refused_without_creating_any_state_files() {
    let state_directory = temporary_state("foreign");
    let state = state_directory.path();
    let foreign = state.join("notes.txt");
    fs::write(foreign, b"keep this file byte-for-byte").expect("foreign sentinel writes");
    let before = filesystem_snapshot(state);

    let error = match RuntimeStore::open(Some(state)).await {
        Ok(_) => panic!("foreign state-directory entries must be refused"),
        Err(error) => error,
    };
    assert!(matches!(error, StoreError::SchemaMismatch(message)
        if message.contains("unrecognized entry") && message.contains("notes.txt")));
    assert_eq!(filesystem_snapshot(state), before);
    assert!(!state.join("runtime.lock").exists());
}

#[tokio::test]
async fn serving_open_claims_the_state_and_a_foreign_owner_is_refused_before_writes() {
    use studio_runtime_store::state_owner::{STATE_OWNER_FILE, StateOwnerClaim};
    let state_directory = temporary_state("serving-owner");
    let state = state_directory.path();
    let first = StateOwnerClaim {
        backend_build: "a".repeat(64),
    };
    let store = RuntimeStore::open_claiming(Some(state), &first)
        .await
        .expect("a serving Rust open claims a fresh state");
    drop(store);
    let marker: Value =
        serde_json::from_slice(&fs::read(state.join(STATE_OWNER_FILE)).expect("marker"))
            .expect("marker JSON");
    assert_eq!(
        marker,
        json!({"runtime":"rust","formatVersion":1,"backendBuild":"a".repeat(64)})
    );
    let update = StateOwnerClaim {
        backend_build: "b".repeat(64),
    };
    drop(
        RuntimeStore::open_claiming(Some(state), &update)
            .await
            .expect("a later Rust build with the same state format opens its state"),
    );

    let foreign_directory = temporary_state("foreign-owner");
    let foreign = foreign_directory.path();
    let prior = json!({"runtime":"rust","formatVersion":2,"backendBuild":"c".repeat(64)});
    fs::write(foreign.join(STATE_OWNER_FILE), prior.to_string()).expect("foreign marker");
    let before = filesystem_snapshot(foreign);
    let refused = RuntimeStore::open_claiming(Some(foreign), &first).await;
    assert!(matches!(refused, Err(StoreError::SchemaMismatch(message))
        if message.contains(STATE_OWNER_FILE)));
    let after: BTreeMap<_, _> = filesystem_snapshot(foreign)
        .into_iter()
        .filter(|(path, _)| !path.ends_with("runtime.lock"))
        .collect();
    let before: BTreeMap<_, _> = before
        .into_iter()
        .filter(|(path, _)| !path.ends_with("runtime.lock"))
        .collect();
    assert_eq!(after, before, "a refused claim creates no database");
}

#[tokio::test]
async fn desktop_rust_state_owner_marker_is_accepted_before_database_creation() {
    let state_directory = temporary_state("desktop-rust-owner");
    let state = state_directory.path();
    let owner = state.join("rust-state-owner.json");
    fs::write(
        &owner,
        br#"{"runtime":"rust","formatVersion":1,"backendBuild":"0123456789abcdef"}"#,
    )
    .expect("desktop owner marker writes");
    let canvas_log = state.join("canvas.log");
    fs::write(&canvas_log, b"desktop startup log\n").expect("desktop log opens before spawn");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for file in [&owner, &canvas_log] {
            fs::set_permissions(file, fs::Permissions::from_mode(0o600))
                .expect("desktop startup files are private");
        }
    }

    let store = RuntimeStore::open(Some(state))
        .await
        .expect("desktop's preclaimed fresh state opens in the Rust server");
    drop(store);
    assert!(state.join("canvas.sqlite3").is_file());
    assert!(owner.is_file());
    assert!(canvas_log.is_file());
}

#[tokio::test]
async fn python_startup_artifacts_are_accepted_for_a_fresh_state() {
    let state_directory = temporary_state("python-lock");
    let state = state_directory.path();
    let lock = state.join("runtime.lock");
    fs::write(&lock, b"python startup lock").expect("Python lock sentinel writes");
    create_private_directory(&state.join("accounts"));
    create_private_directory(&state.join("multi-server"));

    let store = RuntimeStore::open(Some(state))
        .await
        .expect("Python startup artifacts are allowed entries");
    drop(store);
    assert_eq!(
        fs::read(lock).expect("lock remains readable"),
        b"python startup lock"
    );
    assert!(state.join("canvas.sqlite3").is_file());
    assert!(state.join("analytics.sqlite3").is_file());
}

/// launchd or systemd can start the supervisor before the first backend. The
/// files below are the ones `supervisor_daemon::serve` creates (lease, open WAL
/// journal and socket) plus the launch agent's supervisor.log.
#[cfg(unix)]
#[tokio::test]
async fn running_rust_supervisor_files_are_accepted_and_its_journal_is_kept() {
    use std::os::unix::fs::MetadataExt;

    let state_directory = temporary_state("rsup");
    let state = state_directory.path();
    let journal = studio_runtime_store::supervisor_journal::SupervisorJournal::open(state)
        .expect("supervisor journal opens first");
    journal
        .reserve("server-command:kept", "signature", 1.0)
        .expect("supervisor reserves a handle");
    journal
        .opened("server-command:kept", "signature", 4242, "start")
        .expect("supervisor records the child");
    fs::write(
        state.join("supervisor.lock"),
        br#"{"pid":4242,"startTime":"start"}"#,
    )
    .expect("supervisor lease writes");
    fs::write(state.join("supervisor.log"), b"supervisor started\n")
        .expect("launch agent log writes");
    let listener = std::os::unix::net::UnixListener::bind(state.join("supervisor.sock"))
        .expect("supervisor socket binds");
    for sidecar in ["supervisor.sqlite3-wal", "supervisor.sqlite3-shm"] {
        assert!(
            state.join(sidecar).is_file(),
            "the open journal has its {sidecar} file"
        );
    }
    let journal_inode = fs::metadata(state.join("supervisor.sqlite3"))
        .expect("journal metadata")
        .ino();

    let store = RuntimeStore::open(Some(state))
        .await
        .expect("a fresh state with only supervisor files opens");
    drop(store);

    assert!(state.join("canvas.sqlite3").is_file());
    assert!(state.join("analytics.sqlite3").is_file());
    assert_eq!(
        fs::metadata(state.join("supervisor.sqlite3"))
            .expect("journal metadata")
            .ino(),
        journal_inode,
        "the running supervisor's journal is not replaced"
    );
    assert!(
        journal
            .handle("server-command:kept")
            .expect("journal still reads")
            .is_some(),
        "the supervisor keeps its recorded handle"
    );
    assert!(
        fs::read_dir(state)
            .expect("state entries")
            .filter_map(Result::ok)
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".rust-state-bootstrap-")),
        "the staging directory is removed"
    );
    drop(listener);
    drop(journal);
}

#[tokio::test]
async fn fresh_state_without_supervisor_still_creates_the_supervisor_journal() {
    let state_directory = temporary_state("no-supervisor");
    let state = state_directory.path();
    let store = RuntimeStore::open(Some(state))
        .await
        .expect("an empty state opens");
    drop(store);
    assert!(state.join("supervisor.sqlite3").is_file());
}

#[tokio::test]
async fn supervisor_files_do_not_hide_partial_or_foreign_state() {
    let cases: [(&str, &[&str], &str); 3] = [
        (
            "one-core-database",
            &["supervisor.sqlite3", "canvas.sqlite3"],
            "partial state database",
        ),
        (
            "stray-wal",
            &["supervisor.lock", "supervisor.sqlite3-wal"],
            "supervisor.sqlite3-wal",
        ),
        (
            "foreign-file",
            &["supervisor.sqlite3", "supervisor.lock", "notes.txt"],
            "notes.txt",
        ),
    ];
    for (name, files, message) in cases {
        let state_directory = temporary_state(name);
        let state = state_directory.path();
        for file in files {
            fs::write(state.join(file), b"sentinel").expect("fixture file writes");
        }
        let before = filesystem_snapshot(state);
        let error = match RuntimeStore::open(Some(state)).await {
            Ok(_) => panic!("{name}: state must be refused"),
            Err(error) => error,
        };
        assert!(
            matches!(&error, StoreError::SchemaMismatch(text) if text.contains(message)),
            "{name}: unexpected refusal {error}"
        );
        assert_eq!(filesystem_snapshot(state), before, "{name}: files changed");
        assert!(!state.join("runtime.lock").exists(), "{name}: lock created");
    }
}

/// A random suffix keeps parallel test processes apart. The short prefix keeps
/// `supervisor.sock` under the macOS 104-byte socket path limit, also below a
/// long per-user TMPDIR such as /var/folders/<id>/T.
fn temporary_state(name: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("sfs-{name}-"))
        .tempdir()
        .expect("temporary state directory creates")
}

fn filesystem_snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, directory: &Path, snapshot: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(directory).expect("directory entries read") {
            let path = entry.expect("directory entry reads").path();
            let relative = path
                .strip_prefix(root)
                .expect("entry remains under root")
                .to_path_buf();
            let metadata = fs::symlink_metadata(&path).expect("entry metadata reads");
            if metadata.file_type().is_dir() {
                snapshot.insert(relative, Vec::new());
                visit(root, &path, snapshot);
            } else {
                snapshot.insert(relative, fs::read(path).expect("file bytes read"));
            }
        }
    }

    let mut snapshot = BTreeMap::new();
    visit(root, root, &mut snapshot);
    snapshot
}

fn create_private_directory(path: &Path) {
    fs::create_dir(path).expect("Python startup directory creates");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .expect("Python startup directory is owner-only");
    }
}

#[tokio::test]
async fn rust_fresh_state_catalog_is_deterministic_and_survives_reopen() {
    fn catalog(root: &Path) -> BTreeMap<String, Vec<(String, String, String)>> {
        ["canvas.sqlite3", "analytics.sqlite3"]
            .into_iter()
            .map(|name| {
                let db = Connection::open(root.join(name)).expect("fresh database opens");
                let mut statement = db.prepare(
                    "SELECT type,name,sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type,name",
                ).expect("fresh catalog query prepares");
                let objects = statement.query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                }).expect("fresh catalog query runs")
                  .collect::<Result<Vec<_>, _>>().expect("fresh catalog reads");
                assert!(!objects.is_empty(), "{name} has a schema");
                let journal: String = db.pragma_query_value(None, "journal_mode", |row| row.get(0))
                    .expect("journal mode reads");
                assert_eq!(journal, "wal");
                (name.to_owned(), objects)
            }).collect()
    }
    let first = temporary_state("catalog-first");
    let second = temporary_state("catalog-second");
    drop(
        RuntimeStore::open(Some(first.path()))
            .await
            .expect("first fresh state opens"),
    );
    let snapshot = catalog(first.path());
    drop(
        RuntimeStore::open(Some(second.path()))
            .await
            .expect("second fresh state opens"),
    );
    assert_eq!(
        catalog(second.path()),
        snapshot,
        "fresh roots have the same catalog"
    );
    drop(
        RuntimeStore::open(Some(first.path()))
            .await
            .expect("fresh state reopens"),
    );
    assert_eq!(
        catalog(first.path()),
        snapshot,
        "reopen preserves the full catalog"
    );
}
