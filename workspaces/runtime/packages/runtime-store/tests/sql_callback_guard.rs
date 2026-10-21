#![cfg(debug_assertions)]

use std::path::Path;
use studio_runtime_store::{RuntimeStore, StoreError, debug_assert_outside_sql, path_identity};

fn guarded_path_read_panics() -> bool {
    std::panic::catch_unwind(|| path_identity::resolve(Path::new("/"))).is_err()
}

#[tokio::test]
async fn filesystem_owner_refuses_reads_and_writes_and_guard_unwinds() {
    let directory = tempfile::tempdir().expect("temporary state");
    let store = RuntimeStore::open(Some(directory.path()))
        .await
        .expect("store");
    let refused = store
        .read_async("tests/sql-guard", |_| {
            Ok::<_, StoreError>(guarded_path_read_panics())
        })
        .await
        .expect("read callback");
    assert!(
        refused,
        "path identity must refuse filesystem work in a read callback"
    );
    let refused = store
        .transaction(|_| Ok(guarded_path_read_panics()))
        .await
        .expect("write callback");
    assert!(
        refused,
        "path identity must refuse filesystem work in a write callback"
    );
    debug_assert_outside_sql();
    assert!(path_identity::resolve(directory.path()).is_some());
}

#[tokio::test]
async fn stored_path_identities_deserialize_lexically_inside_sql() {
    use path_identity::{CanonicalPath, LexicalPath};
    let directory = tempfile::tempdir().expect("temporary state");
    let store = RuntimeStore::open(Some(directory.path()))
        .await
        .expect("store");
    // No such path has to exist. Loading stored text must not resolve it on
    // today's filesystem, and serialization remains the existing string format.
    let prefix = directory.path().join("stored");
    let expected = prefix.join("link/missing");
    let spelling = format!("{}//link/./missing/", prefix.display());
    let traversal = prefix.join("../other");
    let check = move || {
        let stored = serde_json::json!(spelling);
        let lexical: LexicalPath =
            serde_json::from_value(stored.clone()).expect("lexical identity");
        let canonical: CanonicalPath = serde_json::from_value(stored).expect("stored identity");
        assert_eq!(lexical.as_path(), expected.as_path());
        assert_eq!(
            serde_json::to_value(canonical).expect("stored string"),
            serde_json::json!(expected)
        );
        assert!(serde_json::from_value::<LexicalPath>(serde_json::json!("relative")).is_err());
        assert!(serde_json::from_value::<CanonicalPath>(serde_json::json!(traversal)).is_err());
        Ok::<_, StoreError>(())
    };
    let read_check = check.clone();
    store
        .read_async("tests/stored-path-identity", move |_| read_check())
        .await
        .expect("read callback");
    store
        .transaction(move |_| check())
        .await
        .expect("write callback");
}

#[cfg(unix)]
#[test]
fn path_constructors_preserve_symlink_and_missing_tail_contracts() {
    use path_identity::{CanonicalPath, LexicalPath};
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().expect("temporary paths");
    let root = CanonicalPath::existing(directory.path()).expect("resolved fixture");
    assert!(LexicalPath::exact(root.as_path()).is_some());
    assert!(LexicalPath::exact(&root.join("./child/")).is_none());
    assert_eq!(
        LexicalPath::within(&root, Path::new("."))
            .expect("workspace root")
            .as_path(),
        root.as_path()
    );
    assert_eq!(
        LexicalPath::within(&root, Path::new("child/./nested"))
            .expect("workspace child")
            .as_path(),
        root.join("child/nested")
    );
    assert!(LexicalPath::within(&root, Path::new("../outside")).is_none());
    assert!(LexicalPath::within(&root, root.as_path()).is_none());
    let target = root.join("target/nested");
    std::fs::create_dir_all(&target).expect("target directories");
    let link = root.join("link");
    symlink(&target, &link).expect("linked folder");
    let traversed = link.join("..");
    assert_eq!(
        CanonicalPath::existing(&traversed)
            .expect("strict link parent")
            .as_path(),
        root.join("target")
    );
    assert_eq!(
        CanonicalPath::nonstrict(&traversed)
            .expect("input link parent")
            .as_path(),
        root.join("target")
    );
    assert_eq!(
        CanonicalPath::profile_nonstrict(&traversed)
            .expect("profile link parent")
            .as_path(),
        root.join("target")
    );
    assert_eq!(
        LexicalPath::absolute(&traversed)
            .expect("lexical parent")
            .as_path(),
        root.as_path()
    );
    assert!(CanonicalPath::existing(link.join("new/file")).is_err());
    assert_eq!(
        CanonicalPath::missing_tail(&link.join("new/file"))
            .expect("safe missing tail")
            .as_path(),
        target.join("new/file")
    );
    assert!(CanonicalPath::missing_tail(&link.join("new/../../outside")).is_none());
    assert_eq!(
        CanonicalPath::ancestor_nonstrict(&link.join("new/../file"))
            .expect("profile missing parent")
            .as_path(),
        target.join("file")
    );
    let dangling = root.join("dangling");
    symlink(root.join("missing-target"), &dangling).expect("dangling link");
    assert!(CanonicalPath::missing_tail(&dangling).is_none());
    assert_eq!(
        CanonicalPath::nonstrict(&dangling)
            .expect("nonstrict dangling link")
            .as_path(),
        root.join("missing-target")
    );
    assert_eq!(
        CanonicalPath::weak_missing_tail(&dangling)
            .expect("submission spelling")
            .as_path(),
        dangling
    );
    assert!(!path_identity::same_existing(&dangling, &dangling));
    assert!(path_identity::same(&dangling, &dangling));
}

#[cfg(windows)]
#[test]
fn drive_relative_inputs_cannot_construct_absolute_lexical_identities() {
    assert!(matches!(
        path_identity::LexicalPath::absolute(Path::new("Z:relative")),
        Err(path_identity::AbsolutePathError::RelativePath)
    ));
}
