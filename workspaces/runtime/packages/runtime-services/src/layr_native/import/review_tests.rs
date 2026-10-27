use super::*;
fn git(at: &Path, args: &[&str]) -> Vec<u8> {
    super::super::inventory::git(at, args).expect("fixture operation succeeds")
}
fn commit(at: &Path) {
    git(at, &["add", "."]);
    git(
        at,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-qm",
            "base",
        ],
    );
}
#[test]
fn review_submodule_config_symlink_never_writes_source() {
    let root = tempfile::tempdir().expect("fixture operation succeeds");
    let source = root.path().join("source");
    let destination = root.path().join("stage");
    fs::create_dir_all(source.join("module")).expect("fixture operation succeeds");
    fs::create_dir(source.join("metadata")).expect("fixture operation succeeds");
    fs::create_dir(&destination).expect("fixture operation succeeds");
    let original = b"[core]\n\tbare = false\n";
    fs::write(source.join("config"), original).expect("fixture operation succeeds");
    std::os::unix::fs::symlink(source.join("config"), source.join("metadata/config"))
        .expect("fixture operation succeeds");
    let mut plan = Plan::new(&source, false, &destination).expect("fixture operation succeeds");
    plan.repositories.push(Repository {
        relative: PathBuf::from("module"),
        kind: "submodule",
        included: true,
        cache: false,
        git_directory: Some(
            CanonicalPath::existing(&source)
                .map(CanonicalPath::into_path_buf)
                .expect("fixture operation succeeds")
                .join("metadata"),
        ),
    });
    plan.stage(&destination)
        .expect("fixture operation succeeds");
    assert_eq!(
        fs::read(source.join("config")).expect("fixture operation succeeds"),
        original,
        "Source config must remain unchanged"
    );
    assert!(
        !fs::symlink_metadata(destination.join("metadata/config"))
            .expect("fixture operation succeeds")
            .file_type()
            .is_symlink()
    );
}
#[test]
fn review_internal_object_store_cannot_be_dropped() {
    let root = tempfile::tempdir().expect("fixture operation succeeds");
    let source = root.path().join("source");
    fs::create_dir(&source).expect("fixture operation succeeds");
    git(&source, &["init", "-q"]);
    fs::write(source.join("code"), "base").expect("fixture operation succeeds");
    commit(&source);
    fs::create_dir(source.join(".worktrees")).expect("fixture operation succeeds");
    let objects = source.join(".worktrees/object-store");
    fs::rename(source.join(".git/objects"), &objects).expect("fixture operation succeeds");
    std::os::unix::fs::symlink(
        CanonicalPath::existing(&objects)
            .map(CanonicalPath::into_path_buf)
            .expect("fixture operation succeeds"),
        source.join(".git/objects"),
    )
    .expect("fixture operation succeeds");
    let plan = Plan::new(&source, false, root.path()).expect("fixture operation succeeds");
    assert!(
        plan.admit().is_err()
            || plan
                .entries
                .iter()
                .any(|e| e.relative.starts_with(".worktrees/object-store") && e.metadata.is_file()),
        "Admitted object bytes must enter the selection"
    );
}
#[test]
fn review_worktrees_ordinary_data_enters_main() {
    let root = tempfile::tempdir().expect("fixture operation succeeds");
    fs::create_dir_all(root.path().join(".worktrees/notes/empty"))
        .expect("fixture operation succeeds");
    fs::write(root.path().join(".worktrees/info"), "keep").expect("fixture operation succeeds");
    fs::write(root.path().join(".worktrees/notes/text"), "keep")
        .expect("fixture operation succeeds");
    let plan = Plan::new(
        root.path(),
        false,
        root.path().parent().expect("fixture operation succeeds"),
    )
    .expect("fixture operation succeeds");
    plan.admit().expect("fixture operation succeeds");
    assert!(plan.files().any(|p| p == Path::new(".worktrees/info")));
    assert!(
        plan.files()
            .any(|p| p == Path::new(".worktrees/notes/text"))
    );
}
#[test]
fn review_stage_preserves_xattrs_for_copies_directories_and_links() {
    use rustix::fs::{XattrFlags, lgetxattr, lsetxattr};
    let root = tempfile::tempdir().expect("fixture operation succeeds");
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir(&source).expect("fixture operation succeeds");
    fs::create_dir(&destination).expect("fixture operation succeeds");
    fs::create_dir(source.join("dir")).expect("fixture operation succeeds");
    fs::write(source.join("dir/file"), "data").expect("fixture operation succeeds");
    std::os::unix::fs::symlink("dir/file", source.join("link"))
        .expect("fixture operation succeeds");
    let attr = if cfg!(target_os = "macos") {
        "com.studio.fixture"
    } else {
        "user.studio.fixture"
    };
    let mut paths = vec!["dir", "dir/file"];
    if lsetxattr(source.join("link"), attr, b"attribute", XattrFlags::empty()).is_ok() {
        paths.push("link");
    }
    for name in &paths {
        lsetxattr(source.join(name), attr, b"attribute", XattrFlags::empty())
            .expect("fixture operation succeeds");
    }
    let mut plan = Plan::new(&source, false, &destination).expect("fixture operation succeeds");
    for e in &mut plan.entries {
        e.cloneable = false;
    }
    plan.stage(&destination)
        .expect("fixture operation succeeds");
    for name in paths {
        let mut b = [0u8; 64];
        let len =
            lgetxattr(destination.join(name), attr, &mut b).expect("fixture operation succeeds");
        assert_eq!(&b[..len], b"attribute");
    }
}
#[test]
fn review_source_git_never_lazy_fetches_missing_blobs() {
    let root = tempfile::tempdir().expect("fixture operation succeeds");
    let remote = root.path().join("remote");
    fs::create_dir(&remote).expect("fixture operation succeeds");
    git(&remote, &["init", "-q"]);
    fs::write(remote.join("payload"), "fixture payload").expect("fixture operation succeeds");
    commit(&remote);
    git(&remote, &["config", "uploadpack.allowFilter", "true"]);
    let source = root.path().join("source");
    let url = format!("file://{}", remote.display());
    git(
        root.path(),
        &[
            "clone",
            "--filter=blob:none",
            "--no-checkout",
            &url,
            source.to_str().expect("fixture operation succeeds"),
        ],
    );
    fs::write(source.join("payload"), "fixture payload").expect("fixture operation succeeds");
    let before = fs::read_dir(source.join(".git/objects/pack"))
        .expect("fixture operation succeeds")
        .count();
    let plan = Plan::new(&source, false, root.path()).expect("fixture operation succeeds");
    let after = fs::read_dir(source.join(".git/objects/pack"))
        .expect("fixture operation succeeds")
        .count();
    assert_eq!(after, before, "Size reads must not fetch source objects");
    assert!(plan.admit().is_err(), "Missing blob size must be unknown");
}

#[test]
fn review2_stage_rejects_nested_destination_link() {
    let root = tempfile::tempdir().expect("fixture operation succeeds");
    let source = root.path().join("input");
    let destination = root.path().join("stage");
    let original = root.path().join("original");
    fs::create_dir_all(source.join("dir")).expect("fixture operation succeeds");
    fs::create_dir(&destination).expect("fixture operation succeeds");
    fs::create_dir(&original).expect("fixture operation succeeds");
    fs::write(source.join("dir/new"), "new").expect("fixture operation succeeds");
    std::os::unix::fs::symlink(&original, destination.join("dir"))
        .expect("fixture operation succeeds");
    let plan = Plan::new(&source, false, &destination).expect("fixture operation succeeds");
    drop(plan.stage(&destination));
    assert!(
        !original.join("new").exists(),
        "Staging must never follow a replaced destination"
    );
}
#[test]
fn review2_config_swap_never_edits_source() {
    let root = tempfile::tempdir().expect("fixture operation succeeds");
    let source = root.path().join("source");
    let destination = root.path().join("stage");
    fs::create_dir_all(source.join("metadata")).expect("fixture operation succeeds");
    fs::create_dir(source.join("module")).expect("fixture operation succeeds");
    fs::create_dir(&destination).expect("fixture operation succeeds");
    let original = b"[core]\n\tbare = false\n";
    fs::write(source.join("metadata/config"), original).expect("fixture operation succeeds");
    let mut plan = Plan::new(&source, false, &destination).expect("fixture operation succeeds");
    plan.repositories.push(Repository {
        relative: PathBuf::from("module"),
        kind: "submodule",
        included: true,
        cache: false,
        git_directory: Some(
            CanonicalPath::existing(&source)
                .map(CanonicalPath::into_path_buf)
                .expect("fixture operation succeeds")
                .join("metadata"),
        ),
    });
    let copied = destination.join("metadata/config");
    let original_path = source.join("metadata/config");
    REVIEW_CONFIG_SWAP.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            fs::rename(&copied, copied.with_extension("old")).expect("fixture operation succeeds");
            std::os::unix::fs::symlink(original_path, copied).expect("fixture operation succeeds");
        }))
    });
    drop(plan.stage(&destination));
    assert_eq!(
        fs::read(source.join("metadata/config")).expect("fixture operation succeeds"),
        original
    );
}
#[test]
fn review2_refuses_source_object_links() {
    let root = tempfile::tempdir().expect("fixture operation succeeds");
    let source = root.path().join("source");
    fs::create_dir(&source).expect("fixture operation succeeds");
    git(&source, &["init", "-q"]);
    fs::write(source.join("code"), "base").expect("fixture operation succeeds");
    commit(&source);
    let objects = source.join("object-store");
    fs::rename(source.join(".git/objects"), &objects).expect("fixture operation succeeds");
    std::os::unix::fs::symlink(
        CanonicalPath::existing(&objects)
            .map(CanonicalPath::into_path_buf)
            .expect("fixture operation succeeds"),
        source.join(".git/objects"),
    )
    .expect("fixture operation succeeds");
    let plan = Plan::new(&source, false, root.path()).expect("fixture operation succeeds");
    assert!(
        plan.admit().is_err(),
        "Layr init must not retain a path into the source object store"
    );
}
#[test]
fn review2_stages_linked_repository_root_mode() {
    let root = tempfile::tempdir().expect("fixture operation succeeds");
    let source = root.path().join("source");
    let destination = root.path().join("stage");
    fs::create_dir(&source).expect("fixture operation succeeds");
    fs::create_dir(&destination).expect("fixture operation succeeds");
    fs::write(source.join("code"), "base").expect("fixture operation succeeds");
    fs::set_permissions(&source, fs::Permissions::from_mode(0o750))
        .expect("fixture operation succeeds");
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o700))
        .expect("fixture operation succeeds");
    let plan = Plan::new(&source, true, &destination).expect("fixture operation succeeds");
    plan.stage(&destination)
        .expect("fixture operation succeeds");
    assert_eq!(
        fs::metadata(&destination)
            .expect("fixture operation succeeds")
            .mode()
            & 0o7777,
        0o750
    );
}
