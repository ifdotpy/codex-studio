use super::*;
fn git(at: &Path, args: &[&str]) -> Vec<u8> {
    super::super::inventory::git(at, args).unwrap()
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
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("stage");
    fs::create_dir_all(source.join("module")).unwrap();
    fs::create_dir(source.join("metadata")).unwrap();
    fs::create_dir(&destination).unwrap();
    let original = b"[core]\n\tbare = false\n";
    fs::write(source.join("config"), original).unwrap();
    std::os::unix::fs::symlink(source.join("config"), source.join("metadata/config")).unwrap();
    let mut plan = Plan::new(&source, false, &destination).unwrap();
    plan.repositories.push(Repository {
        relative: PathBuf::from("module"),
        kind: "submodule",
        included: true,
        cache: false,
        git_directory: Some(fs::canonicalize(&source).unwrap().join("metadata")),
    });
    plan.stage(&destination).unwrap();
    assert_eq!(
        fs::read(source.join("config")).unwrap(),
        original,
        "Source config must remain unchanged"
    );
    assert!(
        !fs::symlink_metadata(destination.join("metadata/config"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}
#[test]
fn review_internal_object_store_cannot_be_dropped() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    fs::create_dir(&source).unwrap();
    git(&source, &["init", "-q"]);
    fs::write(source.join("code"), "base").unwrap();
    commit(&source);
    fs::create_dir(source.join(".worktrees")).unwrap();
    let objects = source.join(".worktrees/object-store");
    fs::rename(source.join(".git/objects"), &objects).unwrap();
    std::os::unix::fs::symlink(
        fs::canonicalize(&objects).unwrap(),
        source.join(".git/objects"),
    )
    .unwrap();
    let plan = Plan::new(&source, false, root.path()).unwrap();
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
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join(".worktrees/notes/empty")).unwrap();
    fs::write(root.path().join(".worktrees/info"), "keep").unwrap();
    fs::write(root.path().join(".worktrees/notes/text"), "keep").unwrap();
    let plan = Plan::new(root.path(), false, root.path().parent().unwrap()).unwrap();
    plan.admit().unwrap();
    assert!(plan.files().any(|p| p == Path::new(".worktrees/info")));
    assert!(
        plan.files()
            .any(|p| p == Path::new(".worktrees/notes/text"))
    );
}
#[test]
fn review_stage_preserves_xattrs_for_copies_directories_and_links() {
    use rustix::fs::{XattrFlags, lgetxattr, lsetxattr};
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&destination).unwrap();
    fs::create_dir(source.join("dir")).unwrap();
    fs::write(source.join("dir/file"), "data").unwrap();
    std::os::unix::fs::symlink("dir/file", source.join("link")).unwrap();
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
        lsetxattr(source.join(name), attr, b"attribute", XattrFlags::empty()).unwrap();
    }
    let mut plan = Plan::new(&source, false, &destination).unwrap();
    for e in &mut plan.entries {
        e.cloneable = false;
    }
    plan.stage(&destination).unwrap();
    for name in paths {
        let mut b = [0u8; 64];
        let len = lgetxattr(destination.join(name), attr, &mut b).unwrap();
        assert_eq!(&b[..len], b"attribute");
    }
}
#[test]
fn review_source_git_never_lazy_fetches_missing_blobs() {
    let root = tempfile::tempdir().unwrap();
    let remote = root.path().join("remote");
    fs::create_dir(&remote).unwrap();
    git(&remote, &["init", "-q"]);
    fs::write(remote.join("payload"), "fixture payload").unwrap();
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
            source.to_str().unwrap(),
        ],
    );
    fs::write(source.join("payload"), "fixture payload").unwrap();
    let before = fs::read_dir(source.join(".git/objects/pack"))
        .unwrap()
        .count();
    let plan = Plan::new(&source, false, root.path()).unwrap();
    let after = fs::read_dir(source.join(".git/objects/pack"))
        .unwrap()
        .count();
    assert_eq!(after, before, "Size reads must not fetch source objects");
    assert!(plan.admit().is_err(), "Missing blob size must be unknown");
}

#[test]
fn review2_stage_rejects_nested_destination_link() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("input");
    let destination = root.path().join("stage");
    let original = root.path().join("original");
    fs::create_dir_all(source.join("dir")).unwrap();
    fs::create_dir(&destination).unwrap();
    fs::create_dir(&original).unwrap();
    fs::write(source.join("dir/new"), "new").unwrap();
    std::os::unix::fs::symlink(&original, destination.join("dir")).unwrap();
    let plan = Plan::new(&source, false, &destination).unwrap();
    let _ = plan.stage(&destination);
    assert!(
        !original.join("new").exists(),
        "Staging must never follow a replaced destination"
    );
}
#[test]
fn review2_config_swap_never_edits_source() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("stage");
    fs::create_dir_all(source.join("metadata")).unwrap();
    fs::create_dir(source.join("module")).unwrap();
    fs::create_dir(&destination).unwrap();
    let original = b"[core]\n\tbare = false\n";
    fs::write(source.join("metadata/config"), original).unwrap();
    let mut plan = Plan::new(&source, false, &destination).unwrap();
    plan.repositories.push(Repository {
        relative: PathBuf::from("module"),
        kind: "submodule",
        included: true,
        cache: false,
        git_directory: Some(fs::canonicalize(&source).unwrap().join("metadata")),
    });
    let copied = destination.join("metadata/config");
    let original_path = source.join("metadata/config");
    REVIEW_CONFIG_SWAP.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            fs::rename(&copied, copied.with_extension("old")).unwrap();
            std::os::unix::fs::symlink(original_path, copied).unwrap();
        }))
    });
    let _ = plan.stage(&destination);
    assert_eq!(fs::read(source.join("metadata/config")).unwrap(), original);
}
#[test]
fn review2_refuses_source_object_links() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    fs::create_dir(&source).unwrap();
    git(&source, &["init", "-q"]);
    fs::write(source.join("code"), "base").unwrap();
    commit(&source);
    let objects = source.join("object-store");
    fs::rename(source.join(".git/objects"), &objects).unwrap();
    std::os::unix::fs::symlink(
        fs::canonicalize(&objects).unwrap(),
        source.join(".git/objects"),
    )
    .unwrap();
    let plan = Plan::new(&source, false, root.path()).unwrap();
    assert!(
        plan.admit().is_err(),
        "Layr init must not retain a path into the source object store"
    );
}
#[test]
fn review2_stages_linked_repository_root_mode() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("stage");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&destination).unwrap();
    fs::write(source.join("code"), "base").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o750)).unwrap();
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o700)).unwrap();
    let plan = Plan::new(&source, true, &destination).unwrap();
    plan.stage(&destination).unwrap();
    assert_eq!(fs::metadata(&destination).unwrap().mode() & 0o7777, 0o750);
}
