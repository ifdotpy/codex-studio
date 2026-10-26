//! Compare each change version's patch against its own first parent.
//!
//! Callers authenticate state and parent bindings, then supply file bytes and Git modes.
//! Use `from_changes` when the caller has rename information. `from_trees` compares paths
//! without guessing renames. Both use the same patch renderer as the native CLI.
use crate::{
    diff::{self, FileDiff, Side},
    StateRec,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A file or symlink. Directories are implicit. Modes are Git modes, for example 0o100644.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct File {
    pub mode: u32,
    pub bytes: Vec<u8>,
}

pub type Tree = BTreeMap<String, File>;

/// An authenticated version and the content of its own first parent (empty for a root).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Version {
    pub state: StateRec,
    pub parent: Tree,
    pub tree: Tree,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileResult {
    pub path: String,
    /// Whether the file patch was added, removed, or modified between versions.
    pub status: String,
    /// Lines added and removed in the patch comparison, including patch metadata.
    pub added: usize,
    pub removed: usize,
    pub interdiff: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Output {
    pub change_id: String,
    pub from: String,
    pub to: String,
    /// The complete CLI interdiff, including its original version headers.
    pub diff: String,
    pub files: Vec<FileResult>,
}

fn changes(version: &Version) -> Vec<FileDiff> {
    let paths: BTreeSet<_> = version.parent.keys().chain(version.tree.keys()).collect();
    paths
        .into_iter()
        .filter_map(|path| {
            if path == ".git" || path.starts_with(".git/") {
                return None;
            }
            let old = version.parent.get(path);
            let new = version.tree.get(path);
            if old.zip(new).is_some_and(|(a, b)| a.mode == b.mode && a.bytes == b.bytes) {
                return None;
            }
            let side = |file: &File| Side {
                path: path.clone(),
                mode: file.mode,
                big: (file.mode & 0o170000 == 0o100000 && file.bytes.len() as u64 > diff::MAX_TEXT)
                    .then_some(file.bytes.len() as u64),
                data: if file.mode & 0o170000 == 0o100000 && file.bytes.len() as u64 > diff::MAX_TEXT {
                    vec![]
                } else {
                    file.bytes.clone()
                },
            };
            Some(FileDiff { old: old.map(side), new: new.map(side) })
        })
        .collect()
}

/// Compute an interdiff from authenticated versions and caller-supplied trees.
/// The labels are normally `v1`, `v2`, and so on. Base-only files stay out of the result.
pub fn from_trees(change_id: &str, from: &str, to: &str, old: &Version, new: &Version) -> Output {
    from_changes(change_id, from, to, &changes(old), &changes(new))
}

/// Compute an interdiff from two version patches, each against its own parent.
/// Rename information comes from the caller. This is the CLI entry point.
pub fn from_changes(change_id: &str, from: &str, to: &str, old: &[FileDiff], new: &[FileDiff]) -> Output {
    let patches = |files: &[FileDiff]| -> BTreeMap<String, String> {
        files.iter().map(|file| (file.path().to_owned(), diff::patch(file, 3))).collect()
    };
    let a = patches(old);
    let b = patches(new);
    let old_patch: String = old.iter().map(|file| diff::patch(file, 3)).collect();
    let new_patch: String = new.iter().map(|file| diff::patch(file, 3)).collect();
    let delta = |a: &str, b: &str| similar::TextDiff::from_lines(a, b).unified_diff().header(from, to).to_string();
    let paths: BTreeSet<_> = a.keys().chain(b.keys()).collect();
    let files = paths
        .into_iter()
        .filter_map(|path| {
            let before = a.get(path).map(String::as_str).unwrap_or("");
            let after = b.get(path).map(String::as_str).unwrap_or("");
            if before == after {
                return None;
            }
            let text = similar::TextDiff::from_lines(before, after);
            let mut added = 0;
            let mut removed = 0;
            for change in text.iter_all_changes() {
                match change.tag() {
                    similar::ChangeTag::Insert => added += 1,
                    similar::ChangeTag::Delete => removed += 1,
                    similar::ChangeTag::Equal => (),
                }
            }
            Some(FileResult {
                path: path.clone(),
                status: if !a.contains_key(path) {
                    "added"
                } else if !b.contains_key(path) {
                    "removed"
                } else {
                    "modified"
                }
                .into(),
                added,
                removed,
                interdiff: delta(before, after),
            })
        })
        .collect();
    Output { change_id: change_id.into(), from: from.into(), to: to.into(), diff: delta(&old_patch, &new_patch), files }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn side(path: &str, mode: u32, bytes: &[u8]) -> Side {
        Side { path: path.into(), mode, data: bytes.to_vec(), big: None }
    }
    #[test]
    fn patch_format_keeps_modes_binary_renames_and_missing_newlines() {
        let file = FileDiff { old: None, new: Some(side("a", 0o100755, b"hello")) };
        assert_eq!(diff::patch(&file,3), "diff --git a/a b/a\nnew file mode 100755\nindex 0000000..b6fc4c6\n--- /dev/null\n+++ b/a\n@@ -0,0 +1 @@\n+hello\n\\ No newline at end of file\n");
        let rename = FileDiff { old: Some(side("old", 0o100644, b"a\n")), new: Some(side("new", 0o100644, b"a\n")) };
        assert_eq!(diff::patch(&rename, 3), "diff --git a/old b/new\nsimilarity index 100%\nrename from old\nrename to new\n");
        let binary = FileDiff { old: None, new: Some(side("b", 0o100644, b"\0")) };
        assert!(diff::patch(&binary, 3).ends_with("Binary files /dev/null and b/b differ\n"));
    }
    #[test]
    fn same_patch_has_no_interdiff_and_amendment_has_file_counts() {
        let a = FileDiff { old: None, new: Some(side("feature", 0o100644, b"one\n")) };
        assert!(from_changes("c", "v2", "v3", std::slice::from_ref(&a), std::slice::from_ref(&a)).files.is_empty());
        let b = FileDiff { old: None, new: Some(side("feature", 0o100644, b"one\ntwo\n")) };
        let result = from_changes("c", "v1", "v2", &[a], &[b]);
        assert_eq!(result.files.len(), 1);
        assert!(result.files[0].added > 0 && result.files[0].removed > 0);
        assert!(result.diff.contains("+two"));
    }
}
