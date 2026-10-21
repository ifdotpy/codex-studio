//! The one owner of host filesystem path identity.
//!
//! Two absolute host paths name the same location when their resolved forms
//! are equal. A stored path and a canonicalized path of one folder can differ
//! as text: on macOS `/var` and `/tmp` are links to `/private/var` and
//! `/private/tmp`. Every identity check between a stored path and a resolved
//! path must use [`same`]; a text comparison refuses a valid folder.
//!
//! [`resolve`] canonicalizes the longest existing prefix of the path, which
//! resolves every link in it. A remainder that does not exist yet is appended
//! as written. That remainder may hold only normal names: a `.` or `..`, a
//! dangling link, or an entry that cannot be read gives `None`, so the
//! resolved path never points outside the location the link targets give. A
//! containment check must resolve the root and the candidate with this same
//! rule, then use [`Path::starts_with`].
//!
//! The rule is for host paths only. A path that a VM guest owns does not name
//! a host location and must not be resolved on the host.

use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

/// The resolved form of an absolute host path, or `None` when the path is
/// relative or a part of it cannot be resolved safely.
#[must_use]
pub fn resolve(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut missing = Vec::new();
    let mut current = path;
    loop {
        match current.canonicalize() {
            Ok(resolved) => {
                return Some(
                    missing
                        .iter()
                        .rev()
                        .fold(resolved, |path, name| path.join(name)),
                );
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                // A dangling link exists as an entry; its target is unknown.
                if current.symlink_metadata().is_ok() {
                    return None;
                }
                let mut components = current.components();
                match components.next_back() {
                    Some(Component::Normal(name)) => missing.push(name.to_owned()),
                    _ => return None,
                }
                current = components.as_path();
            }
            Err(_) => return None,
        }
    }
}

/// Whether two absolute host paths name the same location under the rule of
/// this module. Equal text is the same location without a filesystem read.
#[must_use]
pub fn same(left: &Path, right: &Path) -> bool {
    left == right || resolve(left).is_some_and(|left| resolve(right) == Some(left))
}

/// [`same`] for paths stored as text.
#[must_use]
pub fn same_text(left: &str, right: &str) -> bool {
    same(Path::new(left), Path::new(right))
}

/// The one string form of an absolute path, without a filesystem read:
/// no trailing separator, no `.` component and no repeated separator. Use it
/// where a stored path must be compared as text and no read is allowed, such
/// as inside a gate. It does not resolve links, so it is not [`resolve`]. A
/// relative path or a path with `..` gives `None`, because the meaning of
/// `..` depends on the filesystem.
#[must_use]
pub fn lexical(path: &Path) -> Option<PathBuf> {
    (path.is_absolute() && !path.components().any(|c| c == Component::ParentDir))
        .then(|| path.components().collect())
}

/// [`lexical`] for paths stored as text; `None` also for a result that is not
/// UTF-8.
#[must_use]
pub fn lexical_text(path: &str) -> Option<String> {
    lexical(Path::new(path)).and_then(|path| path.to_str().map(str::to_owned))
}

#[cfg(all(test, unix))]
mod tests {
    use super::{resolve, same, same_text};
    use std::os::unix::fs::symlink;
    use std::path::Path;

    #[test]
    fn a_linked_folder_and_its_target_are_the_same_location() {
        let directory = tempfile::tempdir().expect("temporary folder");
        let root = directory.path().canonicalize().expect("canonical root");
        let target = root.join("target");
        std::fs::create_dir(&target).expect("target folder");
        let link = root.join("link");
        symlink(&target, &link).expect("link to the target");

        assert!(same(&link, &target));
        assert!(
            same(&link.join("work"), &target.join("work")),
            "missing child"
        );
        assert!(
            same(&link.join("a/b"), &target.join("a/b")),
            "missing children"
        );
        assert_eq!(resolve(&link.join("a/b")), Some(target.join("a/b")));
        assert!(!same(&link.join("a"), &target.join("b")));
        assert!(!same(&link, &root));
    }

    #[test]
    fn a_missing_remainder_never_leaves_the_resolved_prefix() {
        let directory = tempfile::tempdir().expect("temporary folder");
        let root = directory.path().canonicalize().expect("canonical root");
        let allowed = root.join("allowed");
        std::fs::create_dir(&allowed).expect("allowed folder");

        assert_eq!(resolve(&allowed.join("missing/../../outside")), None);
        assert_eq!(resolve(Path::new("relative/path")), None);
        let dangling = allowed.join("dangling");
        symlink(root.join("elsewhere"), &dangling).expect("dangling link");
        assert_eq!(resolve(&dangling.join("file")), None, "dangling link");
        assert!(!same(&dangling.join("file"), &root.join("elsewhere/file")));
        // Unresolvable paths are the same only as equal text.
        assert!(same(&dangling, &dangling));
    }

    /// The macOS default temporary folder is under the `/var` link. This is
    /// the path pair that made thread receipts stale on macOS.
    #[test]
    fn the_default_temporary_folder_matches_its_canonical_form() {
        let directory = tempfile::tempdir().expect("temporary folder");
        let canonical = directory.path().canonicalize().expect("canonical folder");
        let written = directory.path().to_str().expect("UTF-8 temporary folder");
        assert!(same_text(written, canonical.to_str().expect("UTF-8")));
        assert!(same(&directory.path().join("new"), &canonical.join("new")));
    }

    /// 2026-10-21: a worktree cwd was stored with a trailing slash.
    #[test]
    fn lexical_gives_one_form_without_a_read() {
        use super::lexical_text;
        for spelling in ["/a/b", "/a/b/", "/a//b", "/a/./b/", "//a/b"] {
            assert_eq!(
                lexical_text(spelling).as_deref(),
                Some("/a/b"),
                "{spelling}"
            );
        }
        assert_eq!(lexical_text("/").as_deref(), Some("/"));
        assert_eq!(lexical_text("a/b"), None);
        assert_eq!(lexical_text("/a/../b"), None);
    }
}
