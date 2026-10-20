use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;
use serde_json::{Value, json};
use studio_runtime_store::{RuntimeStore, StoreError};

#[tokio::test]
async fn open_recreates_the_task_indexes_rust_readers_rely_on() {
    let state = temporary_state("task-index");
    drop(
        RuntimeStore::open(Some(&state))
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
        RuntimeStore::open(Some(&state))
            .await
            .expect("Rust reopens the state"),
    );
    assert!(index_exists(), "open recreates the index");
    cleanup(state);
}

#[tokio::test]
async fn partial_database_set_is_refused_without_changing_existing_files() {
    let state = temporary_state("partial");
    let canvas = state.join("canvas.sqlite3");
    fs::write(&canvas, b"partial database sentinel").expect("sentinel writes");
    let before = filesystem_snapshot(&state);

    let error = match RuntimeStore::open(Some(&state)).await {
        Ok(_) => panic!("partial state must be refused"),
        Err(error) => error,
    };
    assert!(
        matches!(error, StoreError::SchemaMismatch(message) if message.contains("partial state database") && message.contains("canvas.sqlite3"))
    );
    assert_eq!(filesystem_snapshot(&state), before);
    assert!(!state.join("runtime.lock").exists());
    assert!(!state.join("analytics.sqlite3").exists());
    cleanup(state);
}

#[tokio::test]
async fn foreign_files_are_refused_without_creating_any_state_files() {
    let state = temporary_state("foreign");
    let foreign = state.join("notes.txt");
    fs::write(&foreign, b"keep this file byte-for-byte").expect("foreign sentinel writes");
    let before = filesystem_snapshot(&state);

    let error = match RuntimeStore::open(Some(&state)).await {
        Ok(_) => panic!("foreign state-directory entries must be refused"),
        Err(error) => error,
    };
    assert!(matches!(error, StoreError::SchemaMismatch(message)
        if message.contains("unrecognized entry") && message.contains("notes.txt")));
    assert_eq!(filesystem_snapshot(&state), before);
    assert!(!state.join("runtime.lock").exists());
    cleanup(state);
}

#[tokio::test]
async fn serving_open_claims_the_state_and_a_foreign_owner_is_refused_before_writes() {
    use studio_runtime_store::state_owner::{STATE_OWNER_FILE, StateOwnerClaim};
    let state = temporary_state("serving-owner");
    let first = StateOwnerClaim {
        backend_build: "a".repeat(64),
    };
    let store = RuntimeStore::open_claiming(Some(&state), &first)
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
        RuntimeStore::open_claiming(Some(&state), &update)
            .await
            .expect("a later Rust build with the same state format opens its state"),
    );

    let foreign = temporary_state("foreign-owner");
    let prior = json!({"runtime":"rust","formatVersion":2,"backendBuild":"c".repeat(64)});
    fs::write(foreign.join(STATE_OWNER_FILE), prior.to_string()).expect("foreign marker");
    let before = filesystem_snapshot(&foreign);
    let refused = RuntimeStore::open_claiming(Some(&foreign), &first).await;
    assert!(matches!(refused, Err(StoreError::SchemaMismatch(message))
        if message.contains(STATE_OWNER_FILE)));
    let after: BTreeMap<_, _> = filesystem_snapshot(&foreign)
        .into_iter()
        .filter(|(path, _)| !path.ends_with("runtime.lock"))
        .collect();
    let before: BTreeMap<_, _> = before
        .into_iter()
        .filter(|(path, _)| !path.ends_with("runtime.lock"))
        .collect();
    assert_eq!(after, before, "a refused claim creates no database");
    cleanup(state);
    cleanup(foreign);
}

#[tokio::test]
async fn desktop_rust_state_owner_marker_is_accepted_before_database_creation() {
    let state = temporary_state("desktop-rust-owner");
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

    let store = RuntimeStore::open(Some(&state))
        .await
        .expect("desktop's preclaimed fresh state opens in the Rust server");
    drop(store);
    assert!(state.join("canvas.sqlite3").is_file());
    assert!(owner.is_file());
    assert!(canvas_log.is_file());
    cleanup(state);
}

#[tokio::test]
async fn python_startup_artifacts_are_accepted_for_a_fresh_state() {
    let state = temporary_state("python-lock");
    let lock = state.join("runtime.lock");
    fs::write(&lock, b"python startup lock").expect("Python lock sentinel writes");
    create_private_directory(&state.join("accounts"));
    create_private_directory(&state.join("multi-server"));

    let store = RuntimeStore::open(Some(&state))
        .await
        .expect("Python startup artifacts are allowed entries");
    drop(store);
    assert_eq!(
        fs::read(lock).expect("lock remains readable"),
        b"python startup lock"
    );
    assert!(state.join("canvas.sqlite3").is_file());
    assert!(state.join("analytics.sqlite3").is_file());
    cleanup(state);
}

/// launchd or systemd can start the supervisor before the first backend. The
/// files below are the ones `supervisor_daemon::serve` creates (lease, open WAL
/// journal and socket) plus the launch agent's supervisor.log.
#[cfg(unix)]
#[tokio::test]
async fn running_rust_supervisor_files_are_accepted_and_its_journal_is_kept() {
    use std::os::unix::fs::MetadataExt;

    // A short name keeps the socket path under the macOS 104-byte limit.
    let state = temporary_state("rsup");
    let journal = studio_runtime_store::supervisor_journal::SupervisorJournal::open(&state)
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

    let store = RuntimeStore::open(Some(&state))
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
        fs::read_dir(&state)
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
    cleanup(state);
}

#[tokio::test]
async fn fresh_state_without_supervisor_still_creates_the_supervisor_journal() {
    let state = temporary_state("no-supervisor");
    let store = RuntimeStore::open(Some(&state))
        .await
        .expect("an empty state opens");
    drop(store);
    assert!(state.join("supervisor.sqlite3").is_file());
    cleanup(state);
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
        let state = temporary_state(name);
        for file in files {
            fs::write(state.join(file), b"sentinel").expect("fixture file writes");
        }
        let before = filesystem_snapshot(&state);
        let error = match RuntimeStore::open(Some(&state)).await {
            Ok(_) => panic!("{name}: state must be refused"),
            Err(error) => error,
        };
        assert!(
            matches!(&error, StoreError::SchemaMismatch(text) if text.contains(message)),
            "{name}: unexpected refusal {error}"
        );
        assert_eq!(filesystem_snapshot(&state), before, "{name}: files changed");
        assert!(!state.join("runtime.lock").exists(), "{name}: lock created");
        cleanup(state);
    }
}

fn temporary_state(name: &str) -> PathBuf {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let root = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = root.join(format!(
        "studio-fresh-state-{name}-{}-{}-{}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("test clock is after epoch")
            .as_nanos()
    ));
    if path.exists() {
        fs::remove_dir_all(&path).expect("stale temporary state directory removes");
    }
    fs::create_dir_all(&path).expect("temporary state directory creates");
    path
}

fn cleanup(path: PathBuf) {
    fs::remove_dir_all(path).expect("temporary state directory removes");
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
