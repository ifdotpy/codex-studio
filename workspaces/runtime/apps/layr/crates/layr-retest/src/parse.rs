//! Parsers: lcov to line ranges, llvm-cov JSON to function ranges, `layr diff -U0` to hunks.

use crate::record::Ranges;
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};

/// Sort and merge line numbers or ranges into inclusive ranges. Touching ranges merge.
pub fn merge_ranges(mut v: Vec<[u32; 2]>) -> Ranges {
    v.sort_unstable();
    let mut out: Ranges = Vec::new();
    for r in v {
        match out.last_mut() {
            Some(last) if r[0] <= last[1].saturating_add(1) => last[1] = last[1].max(r[1]),
            _ => out.push(r),
        }
    }
    out
}

pub fn lines_to_ranges(lines: impl IntoIterator<Item = u32>) -> Ranges {
    merge_ranges(lines.into_iter().map(|l| [l, l]).collect())
}

pub fn in_ranges(r: &Ranges, line: u32) -> bool {
    r.iter().any(|x| x[0] <= line && line <= x[1])
}

pub fn overlaps(r: &Ranges, lo: u32, hi: u32) -> bool {
    r.iter().any(|x| x[0] <= hi && lo <= x[1])
}

/// Executed lines per file from lcov text (`SF:`, `DA:<line>,<count>`).
pub fn parse_lcov(text: &str) -> BTreeMap<String, Ranges> {
    let mut out = BTreeMap::new();
    let mut file: Option<String> = None;
    let mut hit: Vec<u32> = Vec::new();
    for l in text.lines() {
        if let Some(f) = l.strip_prefix("SF:") {
            file = Some(f.to_string());
            hit.clear();
        } else if let Some(d) = l.strip_prefix("DA:") {
            let mut it = d.split(',');
            let (Some(n), Some(c)) = (it.next(), it.next()) else { continue };
            if let (Ok(n), Ok(c)) = (n.parse::<u32>(), c.parse::<u64>()) {
                if c > 0 {
                    hit.push(n);
                }
            }
        } else if l == "end_of_record" {
            if let Some(f) = file.take() {
                // Generic copies can repeat a file; merge them.
                let mut all: Vec<[u32; 2]> = out.remove(&f).unwrap_or_default();
                all.extend(lines_to_ranges(hit.drain(..)));
                out.insert(f, merge_ranges(all));
            }
        }
    }
    out
}

#[derive(Deserialize)]
struct Export {
    data: Vec<ExportData>,
}
#[derive(Deserialize)]
struct ExportData {
    #[serde(default)]
    functions: Vec<Func>,
}
#[derive(Deserialize)]
struct Func {
    name: String,
    filenames: Vec<String>,
    /// `[line start, col start, line end, col end, count, file id, expanded file id, kind]`
    regions: Vec<[u64; 8]>,
}

/// The functions of one binary with their line spans per file. It does not depend on the test,
/// so it is built once per binary; `classify` then needs only the names of the functions that a
/// test executed.
#[derive(Default)]
pub struct FuncTable(Vec<(String, Vec<(String, [u32; 2])>)>);

/// Function spans from the `llvm-cov export` JSON. Only code regions (kind 0) count. Spans in
/// files for which `keep` is false are dropped.
pub fn parse_function_table(json: &[u8], keep: impl Fn(&str) -> bool) -> anyhow::Result<FuncTable> {
    let e: Export = serde_json::from_slice(json)?;
    let mut out = Vec::new();
    for f in e.data.iter().flat_map(|d| &d.functions) {
        let mut per_file: BTreeMap<u64, [u32; 2]> = BTreeMap::new();
        for r in f.regions.iter().filter(|r| r[7] == 0 && r[5] == r[6]) {
            let s = per_file.entry(r[5]).or_insert([u32::MAX, 0]);
            s[0] = s[0].min(r[0] as u32);
            s[1] = s[1].max(r[2] as u32);
        }
        let spans: Vec<(String, [u32; 2])> = per_file
            .into_iter()
            .filter_map(|(id, span)| f.filenames.get(id as usize).filter(|n| keep(n)).map(|n| (n.clone(), span)))
            .collect();
        if !spans.is_empty() {
            out.push((f.name.clone(), spans));
        }
    }
    Ok(FuncTable(out))
}

impl FuncTable {
    /// Function line ranges per file: `(executed, idle)`. A range is idle only when no copy of the
    /// function ran. `ran` holds the names of the functions with a count above 0.
    pub fn classify(&self, ran: &HashSet<String>) -> BTreeMap<String, (Ranges, Ranges)> {
        let mut spans: BTreeMap<&str, BTreeMap<[u32; 2], bool>> = BTreeMap::new();
        for (name, list) in &self.0 {
            let hit = ran.contains(name);
            for (file, span) in list {
                *spans.entry(file).or_default().entry(*span).or_insert(false) |= hit;
            }
        }
        spans
            .into_iter()
            .map(|(file, m)| {
                let ran: Ranges = m.iter().filter(|(_, x)| **x).map(|(s, _)| *s).collect();
                let idle: Ranges = m.iter().filter(|(_, x)| !**x).map(|(s, _)| *s).collect();
                (file.to_string(), (ran, idle))
            })
            .collect()
    }
}

/// Names of the functions that ran: `llvm-profdata show --covered --all-functions` prints one
/// name per line (only functions with a count above 0).
pub fn parse_covered(text: &str) -> HashSet<String> {
    text.lines().filter(|l| !l.is_empty()).map(str::to_string).collect()
}

/// One hunk of `diff -U0`, on the old side. `old_len == 0` means a pure insertion after `old_start`.
#[derive(Debug, PartialEq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_len: u32,
    pub added: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub struct FileChange {
    /// Path in the baseline state.
    pub old_path: String,
    /// Path in the new state; differs after a rename.
    pub new_path: String,
    pub hunks: Vec<Hunk>,
}

fn parse_span(s: &str) -> Option<(u32, u32)> {
    let s = s.trim_start_matches(['-', '+']);
    match s.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

/// Parse unified diff text made with `-U0`. Files that are new in the diff are left out
/// (no earlier test could execute them).
pub fn parse_hunks(diff: &str) -> Vec<FileChange> {
    let mut out: Vec<FileChange> = Vec::new();
    let mut in_hunk = false;
    let mut cur: Option<FileChange> = None;
    let mk = |p: &str| FileChange { old_path: p.to_string(), new_path: p.to_string(), hunks: Vec::new() };
    for l in diff.lines() {
        if l.starts_with("diff --git ") {
            in_hunk = false;
            out.extend(cur.take());
        } else if in_hunk && !l.starts_with("@@ ") {
            if let (Some(t), Some(h)) = (l.strip_prefix('+'), cur.as_mut().and_then(|c| c.hunks.last_mut())) {
                h.added.push(t.to_string());
            }
        } else if let Some(p) = l.strip_prefix("rename from ") {
            cur = Some(mk(p));
        } else if let Some(p) = l.strip_prefix("rename to ") {
            if let Some(c) = cur.as_mut() {
                c.new_path = p.to_string();
            }
        } else if let Some(p) = l.strip_prefix("--- a/") {
            cur.get_or_insert_with(|| mk(p));
        } else if let Some(h) = l.strip_prefix("@@ ") {
            in_hunk = true;
            let mut it = h.split(' ');
            let (Some(a), Some(b)) = (it.next(), it.next()) else { continue };
            if let (Some((s, n)), Some((_, m)), Some(c)) = (parse_span(a), parse_span(b), cur.as_mut()) {
                c.hunks.push(Hunk { old_start: s, old_len: n, new_len: m, added: Vec::new() });
            }
        }
    }
    out.extend(cur);
    out.retain(|c| !c.hunks.is_empty() || c.old_path != c.new_path);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lcov_to_ranges() {
        let t = "SF:/p/a.rs\nDA:1,3\nDA:2,1\nDA:3,0\nDA:5,2\nDA:6,9\nDA:9,1\nend_of_record\nSF:/p/b.rs\nDA:1,0\nend_of_record\n";
        let m = parse_lcov(t);
        assert_eq!(m["/p/a.rs"], vec![[1, 2], [5, 6], [9, 9]]);
        assert!(m["/p/b.rs"].is_empty());
    }

    #[test]
    fn hunks_from_u0_diff() {
        let d = "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -3,0 +4,2 @@ fn x\n+let a = 1;\n+let b = 2;\n@@ -10 +12 @@\n-old\n+new\n@@ -20,2 +21,0 @@\n-x\n-y\ndiff --git a/new.rs b/new.rs\nnew file mode 100644\n--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1 @@\n+fn n() {}\n";
        let c = parse_hunks(d);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].old_path, "src/a.rs");
        let h = &c[0].hunks;
        assert_eq!((h[0].old_start, h[0].old_len, h[0].new_len, h[0].added.len()), (3, 0, 2, 2));
        assert_eq!((h[1].old_start, h[1].old_len, h[1].added.clone()), (10, 1, vec!["new".to_string()]));
        assert_eq!((h[2].old_start, h[2].old_len), (20, 2));
    }

    #[test]
    fn renames() {
        let d = "diff --git a/o.rs b/n.rs\nsimilarity index 100%\nrename from o.rs\nrename to n.rs\ndiff --git a/p.rs b/q.rs\nrename from p.rs\nrename to q.rs\n--- a/p.rs\n+++ b/q.rs\n@@ -1 +1 @@\n-a\n+b\n";
        let c = parse_hunks(d);
        assert_eq!((c[0].old_path.as_str(), c[0].new_path.as_str(), c[0].hunks.len()), ("o.rs", "n.rs", 0));
        assert_eq!((c[1].old_path.as_str(), c[1].new_path.as_str(), c[1].hunks.len()), ("p.rs", "q.rs", 1));
    }

    #[test]
    fn body_line_that_looks_like_a_header_is_content() {
        let d = "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n--- a/x\n+++ b/x\n";
        let c = parse_hunks(d);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].hunks.len(), 1);
    }

    #[test]
    fn functions_from_json() {
        let j = br#"{"data":[{"functions":[
          {"name":"a","count":2,"filenames":["/p/a.rs"],"regions":[[10,1,12,2,2,0,0,0],[11,5,11,9,2,0,0,0]]},
          {"name":"b","count":0,"filenames":["/p/a.rs"],"regions":[[20,1,25,2,0,0,0,0]]},
          {"name":"c","count":1,"filenames":["/p/a.rs"],"regions":[[30,1,31,2,1,0,0,0],[40,1,41,2,1,0,0,1]]},
          {"name":"a2","count":0,"filenames":["/p/a.rs"],"regions":[[10,1,12,2,0,0,0,0]]},
          {"name":"d","count":5,"filenames":["/dep/x.rs"],"regions":[[1,1,2,2,5,0,0,0]]}
        ]}]}"#;
        let t = parse_function_table(j, |f| f.starts_with("/p/")).unwrap();
        let ran: HashSet<String> = ["a", "c", "d"].map(String::from).into();
        let m = t.classify(&ran);
        assert_eq!(m.len(), 1);
        let (ran, idle) = &m["/p/a.rs"];
        assert_eq!(ran, &vec![[10, 12], [30, 31]]);
        assert_eq!(idle, &vec![[20, 25]]);
    }

    #[test]
    fn covered_function_names() {
        let r = parse_covered("_RNvA\n_RNvC\n\n");
        assert_eq!(r, ["_RNvA", "_RNvC"].map(String::from).into());
    }
}
