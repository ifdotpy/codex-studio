//! Change lists between two read-only snapshots, from `btrfs send --no-data`.
//!
//! A change list is a candidate list: an entry that kept its inode and path has the changed byte
//! ranges, so a content check reads only those ranges. Other entries are checked in full.

use crate::btrfs;
use crate::fsutil::{self, Meta};
use crate::stream;
use anyhow::Result;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Change {
    /// Path in the child (or in the parent for a delete).
    pub path: String,
    /// Path in the parent when it differs from `path` (a move detected at inode level).
    pub old_path: Option<String>,
    pub old: Option<Meta>,
    pub new: Option<Meta>,
    /// Changed byte ranges when the entry kept its inode and path; `None` means "compare all".
    pub ranges: Option<Vec<(u64, u64)>>,
}

impl Change {
    pub fn src(&self) -> &str {
        self.old_path.as_deref().unwrap_or(&self.path)
    }
    pub fn is_rename(&self) -> bool {
        self.old_path.is_some() && self.old.is_some() && self.new.is_some()
    }
}

/// Raw candidate list from the stream, without content checks.
pub fn candidates(parent: &Path, child: &Path) -> Result<Vec<Change>> {
    let data = btrfs::send_no_data(parent, child)?;
    let cmds = stream::parse(&data)?;
    let entries = stream::entries(&cmds);
    build(parent, child, entries)
}

fn build(parent: &Path, child: &Path, entries: Vec<stream::Entry>) -> Result<Vec<Change>> {
    // Every path the stream touched, in the parent and in the child. `Some(ranges)` when the only
    // entry at the path kept its inode there, so unchanged byte ranges share extents.
    let mut paths: BTreeMap<String, Option<Vec<(u64, u64)>>> = BTreeMap::new();
    let mut moves: Vec<(String, String, Vec<(u64, u64)>)> = Vec::new();
    let mut moved_dirs: Vec<(String, String)> = Vec::new();
    let deleted: HashSet<String> = entries.iter().filter(|e| e.path.is_none()).filter_map(|e| e.origin.clone()).collect();
    let touch = |paths: &mut BTreeMap<String, Option<Vec<(u64, u64)>>>, p: &str, r: Option<Vec<(u64, u64)>>| match paths.get(p) {
        Some(_) => {
            paths.insert(p.to_string(), None);
        }
        None => {
            paths.insert(p.to_string(), r);
        }
    };
    for e in &entries {
        match (&e.origin, &e.path) {
            (Some(o), Some(p)) if o == p => touch(&mut paths, p, Some(e.ranges.clone())),
            (Some(o), Some(p)) => {
                touch(&mut paths, o, None);
                touch(&mut paths, p, None);
                moves.push((o.clone(), p.clone(), e.ranges.clone()));
                let od = fsutil::lmeta(parent, o).map(|m| m.is_dir()).unwrap_or(false);
                let nd = fsutil::lmeta(child, p).map(|m| m.is_dir()).unwrap_or(false);
                if od && nd {
                    moved_dirs.push((o.clone(), p.clone()));
                }
            }
            (Some(o), None) => touch(&mut paths, o, None),
            (None, Some(p)) => {
                touch(&mut paths, p, None);
                if let Some(l) = &e.linked_from {
                    if deleted.contains(l) {
                        moves.push((l.clone(), p.clone(), e.ranges.clone()));
                    }
                }
            }
            (None, None) => {}
        }
    }
    // Content below a moved folder moved too.
    for (o, p) in moved_dirs {
        let mut found: Vec<(String, String)> = Vec::new();
        fsutil::walk(child, &p, &mut |rel, _m| {
            let tail = &rel[p.len() + 1..];
            found.push((format!("{o}/{tail}"), rel.to_string()));
            Ok(true)
        })?;
        for (src, dst) in found {
            if !paths.contains_key(&dst) {
                paths.insert(dst.clone(), None);
                if !paths.contains_key(&src) {
                    paths.insert(src.clone(), None);
                }
                moves.push((src, dst, Vec::new()));
            }
        }
    }
    let mut out: BTreeMap<String, Change> = BTreeMap::new();
    for (p, ranges) in paths {
        let old = fsutil::lmeta(parent, &p);
        let new = fsutil::lmeta(child, &p);
        if old.is_none() && new.is_none() {
            continue;
        }
        out.insert(p.clone(), Change { path: p, old_path: None, old, new, ranges });
    }
    // Moves: the source is gone in the child and the target is new in the parent.
    let mut used: HashSet<String> = HashSet::new();
    for (src, dst, ranges) in moves {
        if used.contains(&src) || used.contains(&dst) {
            continue;
        }
        let ok = match (out.get(&src), out.get(&dst)) {
            (Some(s), Some(d)) => {
                s.new.is_none() && d.old.is_none() && s.old.as_ref().map(|m| m.ftype) == d.new.as_ref().map(|m| m.ftype)
            }
            _ => false,
        };
        if !ok {
            continue;
        }
        let s = out.remove(&src).unwrap();
        let d = out.get_mut(&dst).unwrap();
        d.old_path = Some(src.clone());
        d.old = s.old;
        d.ranges = Some(ranges);
        used.insert(src);
        used.insert(dst);
    }
    Ok(out.into_values().collect())
}

/// True when the entry changed in a way git would see: type, content, link target or the
/// executable bit of a file.
pub fn really_changed(parent: &Path, child: &Path, c: &Change) -> Result<bool> {
    let (o, n) = match (&c.old, &c.new) {
        (Some(o), Some(n)) => (o, n),
        _ => return Ok(true),
    };
    if c.old_path.is_some() {
        return Ok(true);
    }
    if o.ftype != n.ftype {
        return Ok(true);
    }
    if o.is_file() && o.executable() != n.executable() {
        return Ok(true);
    }
    if o.is_dir() {
        return Ok(false);
    }
    Ok(!fsutil::same_entry(parent, c.src(), child, &c.path, c.ranges.as_deref())?)
}

/// File level changes (folders dropped), confirmed by content. A move keeps `old_path`.
pub fn file_changes(parent: &Path, child: &Path) -> Result<Vec<Change>> {
    let mut out = Vec::new();
    for c in candidates(parent, child)? {
        let is_dir_only =
            c.old.as_ref().map(|m| m.is_dir()).unwrap_or(true) && c.new.as_ref().map(|m| m.is_dir()).unwrap_or(true);
        if is_dir_only {
            continue;
        }
        // A folder replaced by a file or the reverse: report only the non-folder side.
        let mut c = c;
        if c.old.as_ref().map(|m| m.is_dir()).unwrap_or(false) {
            c.old = None;
            c.old_path = None;
        }
        if c.new.as_ref().map(|m| m.is_dir()).unwrap_or(false) {
            c.new = None;
        }
        if c.old.is_none() && c.new.is_none() {
            continue;
        }
        if c.old.is_some() && c.new.is_some() && !really_changed(parent, child, &c)? {
            continue;
        }
        out.push(c);
    }
    Ok(out)
}

/// Split moves into a delete and an add (git shows unstaged moves this way).
pub fn without_moves(v: Vec<Change>) -> Vec<Change> {
    let mut out: BTreeMap<String, Change> = BTreeMap::new();
    for c in v {
        if let Some(src) = c.old_path.clone() {
            out.insert(src.clone(), Change { path: src, old_path: None, old: c.old.clone(), new: None, ranges: None });
            out.insert(
                c.path.clone(),
                Change { path: c.path.clone(), old_path: None, old: None, new: c.new.clone(), ranges: None },
            );
        } else {
            out.insert(c.path.clone(), c);
        }
    }
    out.into_values().collect()
}

/// Pair deleted and added files as moves when the content is equal, then (for small lists)
/// when at least half of the lines are equal, as git's rename detection does.
pub fn with_renames(parent: &Path, child: &Path, v: Vec<Change>) -> Vec<Change> {
    // Files larger than MAX_TEXT are not compared by content.
    let is_file = |m: &Option<Meta>| {
        m.as_ref().map(|x| x.ftype == crate::fsutil::FType::File && x.size <= crate::fsutil::MAX_TEXT).unwrap_or(false)
    };
    let dels: Vec<usize> = v
        .iter()
        .enumerate()
        .filter(|(_, c)| c.new.is_none() && c.old_path.is_none() && is_file(&c.old))
        .map(|(i, _)| i)
        .collect();
    let adds: Vec<usize> = v.iter().enumerate().filter(|(_, c)| c.old.is_none() && is_file(&c.new)).map(|(i, _)| i).collect();
    if dels.is_empty() || adds.is_empty() {
        return v;
    }
    let read = |r: &Path, p: &str| crate::fsutil::read_file(r, p).ok();
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    let mut used_d: HashSet<usize> = HashSet::new();
    let mut used_a: HashSet<usize> = HashSet::new();
    // Exact: same size and same bytes.
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    for &d in &dels {
        by_size.entry(v[d].old.as_ref().unwrap().size).or_default().push(d);
    }
    for &a in &adds {
        let size = v[a].new.as_ref().unwrap().size;
        if let Some(ds) = by_size.get(&size) {
            let ad = match read(child, &v[a].path) {
                Some(x) => x,
                None => continue,
            };
            for &d in ds {
                if !used_d.contains(&d) && read(parent, &v[d].path).as_ref() == Some(&ad) {
                    pairs.push((d, a));
                    used_d.insert(d);
                    used_a.insert(a);
                    break;
                }
            }
        }
    }
    // Similar content, for small lists.
    let rd: Vec<usize> = dels.iter().copied().filter(|d| !used_d.contains(d)).collect();
    let ra: Vec<usize> = adds.iter().copied().filter(|a| !used_a.contains(a)).collect();
    if !rd.is_empty() && !ra.is_empty() && rd.len() * ra.len() <= 400 {
        let mut scored: Vec<(f32, usize, usize)> = Vec::new();
        for &d in &rd {
            let dd = match read(parent, &v[d].path) {
                Some(x) if !crate::fsutil::is_binary(&x) => x,
                _ => continue,
            };
            for &a in &ra {
                let ad = match read(child, &v[a].path) {
                    Some(x) if !crate::fsutil::is_binary(&x) => x,
                    _ => continue,
                };
                let (x, y) = (split(&dd), split(&ad));
                let ops = similar::capture_diff_slices(similar::Algorithm::Myers, &x, &y);
                let r = similar::get_diff_ratio(&ops, x.len(), y.len());
                if r >= 0.5 {
                    scored.push((r, d, a));
                }
            }
        }
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        for (_, d, a) in scored {
            if !used_d.contains(&d) && !used_a.contains(&a) {
                pairs.push((d, a));
                used_d.insert(d);
                used_a.insert(a);
            }
        }
    }
    let mut out: Vec<Change> = Vec::new();
    for (d, a) in &pairs {
        let mut c = v[*a].clone();
        c.old_path = Some(v[*d].path.clone());
        c.old = v[*d].old.clone();
        c.ranges = None;
        out.push(c);
    }
    for (i, c) in v.into_iter().enumerate() {
        if !used_d.contains(&i) && !used_a.contains(&i) {
            out.push(c);
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn split(d: &[u8]) -> Vec<&[u8]> {
    d.split_inclusive(|b| *b == b'\n').collect()
}
