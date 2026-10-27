//! Shared file history and line attribution for native and browser callers.
use crate::StateRec;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, BinaryHeap, HashMap};

/// Walk source states in CLI date/identity order, comparing each to its first parent.
/// Callers supply verified metadata and the paths changed in each immutable state.
/// The predicate supports the CLI's pathspecs as well as an exact browser path.
pub fn file_history(
    starts: &[String],
    excluded: &BTreeSet<String>,
    first_parent: bool,
    mut state: impl FnMut(&str) -> Result<Option<StateRec>>,
    mut changed_paths: impl FnMut(&StateRec) -> Result<Vec<String>>,
    mut matches: impl FnMut(&str) -> bool,
    mut accept: impl FnMut(StateRec) -> Result<bool>,
) -> Result<()> {
    let mut seen = BTreeSet::new();
    let mut heap = BinaryHeap::new();
    for id in starts.iter().filter(|id| !excluded.contains(*id)) {
        if let Some(s) = state(id)?.filter(|s| !s.kind.auxiliary()) {
            heap.push((s.time, s.id));
        }
    }
    while let Some((_, id)) = heap.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let Some(s) = state(&id)? else { continue };
        for parent in s.parents.iter().take(if first_parent { 1 } else { usize::MAX }) {
            if !excluded.contains(parent) && !seen.contains(parent) {
                if let Some(p) = state(parent)?.filter(|s| !s.kind.auxiliary()) {
                    heap.push((p.time, p.id));
                }
            }
        }
        if changed_paths(&s)?.iter().any(|p| matches(p)) && !accept(s)? {
            break;
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct FileVersion {
    /// None denotes the CLI working content.
    pub state: Option<StateRec>,
    pub bytes: Vec<u8>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlameLine {
    pub line: usize,
    pub state: Option<String>,
    pub text: String,
    /// True only for lines attributed to a root state.
    pub boundary: bool,
    /// Index into the supplied versions, for native presentation metadata.
    pub version: usize,
}

/// Attribute newest file lines backwards through supplied first-parent blob versions.
/// Equal lines use the same `similar` diff algorithm as the CLI. Versions are newest first.
/// Callers bound memory when obtaining blobs, and stop at a missing/deleted file.
pub fn blame(versions: &[FileVersion]) -> Result<Vec<BlameLine>> {
    ensure!(!versions.is_empty(), "no such path");
    let newest = String::from_utf8_lossy(&versions[0].bytes);
    let lines: Vec<_> = newest.split_inclusive('\n').collect();
    let mut owner = vec![0; lines.len()];
    let mut map: Vec<_> = (0..lines.len()).map(Some).collect();
    for vi in 1..versions.len() {
        let newer = String::from_utf8_lossy(&versions[vi - 1].bytes);
        let older = String::from_utf8_lossy(&versions[vi].bytes);
        let diff = similar::TextDiff::from_lines(&older, &newer);
        let mut new_to_old = HashMap::new();
        for op in diff.ops() {
            if let similar::DiffOp::Equal { old_index, new_index, len } = op {
                for k in 0..*len {
                    new_to_old.insert(new_index + k, old_index + k);
                }
            }
        }
        for (i, pos) in map.iter_mut().enumerate() {
            if let Some(m) = *pos {
                match new_to_old.get(&m) {
                    Some(o) => {
                        *pos = Some(*o);
                        owner[i] = vi;
                    }
                    None => *pos = None,
                }
            }
        }
    }
    Ok(lines
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let version = owner[i];
            let state = versions[version].state.as_ref();
            BlameLine {
                line: i + 1,
                state: state.map(|s| s.id.clone()),
                text: text.trim_end_matches('\n').into(),
                boundary: version == versions.len() - 1 && state.is_some_and(|s| s.parents.is_empty()),
                version,
            }
        })
        .collect())
}
