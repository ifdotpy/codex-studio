//! Diff output in git formats: patches, `--stat`, `--numstat`, `--name-status`, `--check`.

use crate::changes::Change;
use crate::fsutil::{self, FType, Meta};
use anyhow::Result;
use similar::{capture_diff_slices, get_diff_ratio, group_diff_ops, Algorithm, DiffOp};
use std::path::Path;

/// One side of a file in a diff.
#[derive(Clone)]
pub struct Side {
    pub path: String,
    pub mode: u32,
    /// The content; empty for a file larger than `fsutil::MAX_TEXT` (`big`), which counts as
    /// binary and is never read.
    pub data: Vec<u8>,
    pub big: Option<u64>,
}

impl Side {
    fn len(&self) -> usize {
        self.big.map(|n| n as usize).unwrap_or(self.data.len())
    }
}

/// Same content. Two big files are never compared (the change list says they changed).
fn same(o: &Side, n: &Side) -> bool {
    o.big.is_none() && n.big.is_none() && o.data == n.data
}

pub struct FileDiff {
    pub old: Option<Side>,
    pub new: Option<Side>,
}

impl FileDiff {
    pub fn path(&self) -> &str {
        self.new.as_ref().or(self.old.as_ref()).map(|s| s.path.as_str()).unwrap_or("")
    }
    pub fn is_binary(&self) -> bool {
        self.old.as_ref().map(|s| s.big.is_some() || fsutil::is_binary(&s.data)).unwrap_or(false)
            || self.new.as_ref().map(|s| s.big.is_some() || fsutil::is_binary(&s.data)).unwrap_or(false)
    }
    pub fn status(&self) -> char {
        match (&self.old, &self.new) {
            (None, Some(_)) => 'A',
            (Some(_), None) => 'D',
            (Some(o), Some(n)) if o.path != n.path => 'R',
            (Some(o), Some(n)) if (o.mode & 0o170000) != (n.mode & 0o170000) => 'T',
            _ => 'M',
        }
    }
    /// Similarity percent for a rename (100 when content is equal).
    pub fn similarity(&self) -> u32 {
        match (&self.old, &self.new) {
            (Some(o), Some(n)) => {
                if same(o, n) {
                    return 100;
                }
                if o.big.is_some() || n.big.is_some() {
                    return 0;
                }
                let (a, b) = (split_lines(&o.data), split_lines(&n.data));
                let ops = capture_diff_slices(Algorithm::Myers, &a, &b);
                (get_diff_ratio(&ops, a.len(), b.len()) * 100.0) as u32
            }
            _ => 0,
        }
    }
    pub fn counts(&self) -> (usize, usize) {
        if self.is_binary() {
            return (0, 0);
        }
        let empty: Vec<u8> = Vec::new();
        let o = self.old.as_ref().map(|s| &s.data).unwrap_or(&empty);
        let n = self.new.as_ref().map(|s| &s.data).unwrap_or(&empty);
        let (ol, nl) = (split_lines(o), split_lines(n));
        let ops = capture_diff_slices(Algorithm::Myers, &ol, &nl);
        let mut a = 0;
        let mut r = 0;
        for op in &ops {
            match op {
                DiffOp::Insert { new_len, .. } => a += new_len,
                DiffOp::Delete { old_len, .. } => r += old_len,
                DiffOp::Replace { old_len, new_len, .. } => {
                    a += new_len;
                    r += old_len;
                }
                DiffOp::Equal { .. } => {}
            }
        }
        (a, r)
    }
    pub fn display_path(&self) -> String {
        match (&self.old, &self.new) {
            (Some(o), Some(n)) if o.path != n.path => format!("{} => {}", o.path, n.path),
            _ => self.path().to_string(),
        }
    }
}

pub fn side_of(root: &Path, path: &str, meta: &Meta) -> Result<Side> {
    let big = (meta.ftype == FType::File && meta.size > fsutil::MAX_TEXT).then_some(meta.size);
    let data = match meta.ftype {
        _ if big.is_some() => Vec::new(),
        FType::File | FType::Symlink => fsutil::entry_bytes(root, path)?,
        _ => Vec::new(),
    };
    Ok(Side { path: path.to_string(), mode: meta.git_mode(), data, big })
}

/// Build file diffs for a change list between two roots.
pub fn file_diffs(old_root: &Path, new_root: &Path, changes: &[Change]) -> Result<Vec<FileDiff>> {
    let mut v = Vec::new();
    for c in changes {
        let old = match &c.old {
            Some(m) if m.ftype != FType::Dir => Some(side_of(old_root, c.src(), m)?),
            _ => None,
        };
        let new = match &c.new {
            Some(m) if m.ftype != FType::Dir => Some(side_of(new_root, &c.path, m)?),
            _ => None,
        };
        if old.is_none() && new.is_none() {
            continue;
        }
        v.push(FileDiff { old, new });
    }
    Ok(v)
}

fn short_blob(data: Option<&Side>) -> String {
    match data {
        Some(s) => fsutil::git_blob_id(&s.data)[..7].to_string(),
        None => "0000000".to_string(),
    }
}

/// The `index` line of a patch. A big file has no blob id here (it is not read), so the line
/// is left out, as for any patch without a full index.
fn index_line(o: Option<&Side>, n: Option<&Side>, mode: Option<u32>) -> String {
    if o.map(|x| x.big.is_some()).unwrap_or(false) || n.map(|x| x.big.is_some()).unwrap_or(false) {
        return String::new();
    }
    match mode {
        Some(m) => format!("index {}..{} {:06o}\n", short_blob(o), short_blob(n), m),
        None => format!("index {}..{}\n", short_blob(o), short_blob(n)),
    }
}

fn quote_path(p: &str) -> String {
    if p.chars().any(|c| c == '"' || c == '\\' || c.is_control() || !c.is_ascii()) {
        let mut s = String::from("\"");
        for b in p.bytes() {
            match b {
                b'"' => s.push_str("\\\""),
                b'\\' => s.push_str("\\\\"),
                b'\n' => s.push_str("\\n"),
                b'\t' => s.push_str("\\t"),
                0x20..=0x7e => s.push(b as char),
                _ => s.push_str(&format!("\\{:03o}", b)),
            }
        }
        s.push('"');
        s
    } else {
        p.to_string()
    }
}

fn ab(prefix: &str, p: &str) -> String {
    quote_path(&format!("{prefix}{p}"))
}

/// A full git patch for one file.
pub fn patch(fd: &FileDiff, context: usize) -> String {
    let mut s = String::new();
    let opath = fd.old.as_ref().map(|x| x.path.clone()).unwrap_or_else(|| fd.path().to_string());
    let npath = fd.new.as_ref().map(|x| x.path.clone()).unwrap_or_else(|| fd.path().to_string());
    s.push_str(&format!("diff --git {} {}\n", ab("a/", &opath), ab("b/", &npath)));
    match (&fd.old, &fd.new) {
        (None, Some(n)) => {
            s.push_str(&format!("new file mode {:06o}\n", n.mode));
            s.push_str(&index_line(None, Some(n), None));
        }
        (Some(o), None) => {
            s.push_str(&format!("deleted file mode {:06o}\n", o.mode));
            s.push_str(&index_line(Some(o), None, None));
        }
        (Some(o), Some(n)) => {
            if o.mode != n.mode {
                s.push_str(&format!("old mode {:06o}\nnew mode {:06o}\n", o.mode, n.mode));
            }
            if o.path != n.path {
                s.push_str(&format!("similarity index {}%\n", fd.similarity()));
                s.push_str(&format!("rename from {}\nrename to {}\n", quote_path(&o.path), quote_path(&n.path)));
                if same(o, n) && o.mode == n.mode {
                    return s;
                }
            }
            if !same(o, n) {
                s.push_str(&index_line(Some(o), Some(n), (o.mode == n.mode).then_some(n.mode)));
            } else {
                return s;
            }
        }
        (None, None) => return s,
    }
    if fd.is_binary() {
        let a = if fd.old.is_some() { ab("a/", &opath) } else { "/dev/null".into() };
        let b = if fd.new.is_some() { ab("b/", &npath) } else { "/dev/null".into() };
        s.push_str(&format!("Binary files {a} and {b} differ\n"));
        return s;
    }
    let empty: Vec<u8> = Vec::new();
    let od = fd.old.as_ref().map(|x| &x.data).unwrap_or(&empty);
    let nd = fd.new.as_ref().map(|x| &x.data).unwrap_or(&empty);
    if od.is_empty() && nd.is_empty() {
        return s;
    }
    s.push_str(&format!("--- {}\n", if fd.old.is_some() { ab("a/", &opath) } else { "/dev/null".into() }));
    s.push_str(&format!("+++ {}\n", if fd.new.is_some() { ab("b/", &npath) } else { "/dev/null".into() }));
    s.push_str(&hunks(od, nd, context));
    s
}

fn split_lines(d: &[u8]) -> Vec<&[u8]> {
    let mut v = Vec::new();
    let mut start = 0;
    for (i, b) in d.iter().enumerate() {
        if *b == b'\n' {
            v.push(&d[start..=i]);
            start = i + 1;
        }
    }
    if start < d.len() {
        v.push(&d[start..]);
    }
    v
}

fn funcname(lines: &[&[u8]], before: usize) -> String {
    let mut i = before;
    while i > 0 {
        i -= 1;
        let l = lines[i];
        if let Some(c) = l.first() {
            if c.is_ascii_alphabetic() || *c == b'_' || *c == b'$' {
                let t = String::from_utf8_lossy(l);
                let t = t.trim_end();
                let t: String = t.chars().take(80).collect();
                return format!(" {t}");
            }
        }
    }
    String::new()
}

fn range(start: usize, len: usize) -> String {
    let s = if len == 0 { start } else { start + 1 };
    if len == 1 {
        format!("{s}")
    } else {
        format!("{s},{len}")
    }
}

/// Unified diff hunks in git format.
pub fn hunks(old: &[u8], new: &[u8], context: usize) -> String {
    let ol = split_lines(old);
    let nl = split_lines(new);
    let ops = capture_diff_slices(Algorithm::Myers, &ol, &nl);
    let mut s = String::new();
    for group in group_diff_ops(ops, context) {
        let (first, last) = (&group[0], &group[group.len() - 1]);
        let o_start = first.old_range().start;
        let o_end = last.old_range().end;
        let n_start = first.new_range().start;
        let n_end = last.new_range().end;
        s.push_str(&format!(
            "@@ -{} +{} @@{}\n",
            range(o_start, o_end - o_start),
            range(n_start, n_end - n_start),
            funcname(&ol, o_start)
        ));
        for op in &group {
            let push = |s: &mut String, prefix: char, line: &[u8]| {
                s.push(prefix);
                s.push_str(&String::from_utf8_lossy(line));
                if !line.ends_with(b"\n") {
                    s.push_str("\n\\ No newline at end of file\n");
                }
            };
            match *op {
                DiffOp::Equal { old_index, len, .. } => {
                    for i in old_index..old_index + len {
                        push(&mut s, ' ', ol[i]);
                    }
                }
                DiffOp::Delete { old_index, old_len, .. } => {
                    for i in old_index..old_index + old_len {
                        push(&mut s, '-', ol[i]);
                    }
                }
                DiffOp::Insert { new_index, new_len, .. } => {
                    for i in new_index..new_index + new_len {
                        push(&mut s, '+', nl[i]);
                    }
                }
                DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                    for i in old_index..old_index + old_len {
                        push(&mut s, '-', ol[i]);
                    }
                    for i in new_index..new_index + new_len {
                        push(&mut s, '+', nl[i]);
                    }
                }
            }
        }
    }
    s
}

fn decimal_width(n: usize) -> usize {
    n.to_string().len()
}

fn scale(it: usize, width: usize, max: usize) -> usize {
    if it == 0 {
        return 0;
    }
    if max == 0 {
        return 0;
    }
    1 + (it * (width.saturating_sub(1))) / max
}

/// `--stat` output (git's layout for an 80 column width).
pub fn stat(files: &[FileDiff], width: usize) -> String {
    if files.is_empty() {
        return String::new();
    }
    struct Row {
        name: String,
        add: usize,
        del: usize,
        bin: Option<(usize, usize)>,
    }
    let rows: Vec<Row> = files
        .iter()
        .map(|f| {
            let (a, d) = f.counts();
            let bin = if f.is_binary() {
                Some((f.old.as_ref().map(|s| s.len()).unwrap_or(0), f.new.as_ref().map(|s| s.len()).unwrap_or(0)))
            } else {
                None
            };
            Row { name: f.display_path(), add: a, del: d, bin }
        })
        .collect();
    let max_len = rows.iter().map(|r| r.name.chars().count()).max().unwrap_or(0);
    let max_change = rows.iter().map(|r| r.add + r.del).max().unwrap_or(0);
    let mut number_width = decimal_width(max_change);
    if rows.iter().any(|r| r.bin.is_some()) && number_width < 3 {
        number_width = 3;
    }
    let mut name_width = max_len;
    let mut graph_width = max_change;
    if name_width + number_width + 6 + graph_width > width {
        let lim = (width * 3 / 8).saturating_sub(number_width + 6);
        if graph_width > lim {
            graph_width = lim.max(6);
        }
        if name_width > width.saturating_sub(number_width + 6 + graph_width) {
            name_width = width.saturating_sub(number_width + 6 + graph_width);
        } else {
            graph_width = width.saturating_sub(number_width + 6 + name_width);
        }
    }
    let mut s = String::new();
    let (mut ta, mut td) = (0, 0);
    for r in &rows {
        let mut name = r.name.clone();
        let n = name.chars().count();
        if n > name_width {
            let keep = name_width.saturating_sub(3);
            let tail: String = name.chars().skip(n - keep).collect();
            name = format!("...{tail}");
        }
        let pad = name_width.saturating_sub(name.chars().count());
        if let Some((a, b)) = r.bin {
            s.push_str(&format!(" {}{} | {:>w$} {} -> {} bytes\n", name, " ".repeat(pad), "Bin", a, b, w = number_width));
            continue;
        }
        ta += r.add;
        td += r.del;
        let total = r.add + r.del;
        let (a, d) = if max_change > graph_width {
            let total_scaled = scale(total, graph_width, max_change);
            let a = scale(r.add, graph_width, max_change);
            let d = total_scaled.saturating_sub(a);
            (a, d)
        } else {
            (r.add, r.del)
        };
        s.push_str(&format!(
            " {}{} | {:>w$}{}{}{}\n",
            name,
            " ".repeat(pad),
            total,
            if total > 0 { " " } else { "" },
            "+".repeat(a),
            "-".repeat(d),
            w = number_width
        ));
    }
    s.push_str(&summary(rows.len(), ta, td));
    s
}

pub fn summary(files: usize, add: usize, del: usize) -> String {
    let mut s = format!(" {} file{} changed", files, if files == 1 { "" } else { "s" });
    if add > 0 || del == 0 {
        s.push_str(&format!(", {} insertion{}(+)", add, if add == 1 { "" } else { "s" }));
    }
    if del > 0 || add == 0 {
        s.push_str(&format!(", {} deletion{}(-)", del, if del == 1 { "" } else { "s" }));
    }
    s.push('\n');
    s
}

pub fn shortstat(files: &[FileDiff]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let (mut a, mut d) = (0, 0);
    for f in files {
        let (x, y) = f.counts();
        a += x;
        d += y;
    }
    summary(files.len(), a, d)
}

pub fn numstat(files: &[FileDiff]) -> String {
    let mut s = String::new();
    for f in files {
        if f.is_binary() {
            s.push_str(&format!("-\t-\t{}\n", f.display_path()));
        } else {
            let (a, d) = f.counts();
            s.push_str(&format!("{a}\t{d}\t{}\n", f.display_path()));
        }
    }
    s
}

pub fn name_status(files: &[FileDiff]) -> String {
    let mut s = String::new();
    for f in files {
        match f.status() {
            'R' => s.push_str(&format!(
                "R{:03}\t{}\t{}\n",
                f.similarity(),
                f.old.as_ref().unwrap().path,
                f.new.as_ref().unwrap().path
            )),
            c => s.push_str(&format!("{c}\t{}\n", f.path())),
        }
    }
    s
}

pub fn name_only(files: &[FileDiff]) -> String {
    files.iter().map(|f| format!("{}\n", f.path())).collect()
}

/// Whitespace errors and conflict markers in added lines (`--check`).
pub fn check(files: &[FileDiff]) -> String {
    let mut s = String::new();
    for f in files {
        if f.is_binary() {
            continue;
        }
        let empty: Vec<u8> = Vec::new();
        let od = f.old.as_ref().map(|x| &x.data).unwrap_or(&empty);
        let nd = match &f.new {
            Some(n) => &n.data,
            None => continue,
        };
        let ol = split_lines(od);
        let nl = split_lines(nd);
        let ops = capture_diff_slices(Algorithm::Myers, &ol, &nl);
        for op in &ops {
            let (start, len) = match *op {
                DiffOp::Insert { new_index, new_len, .. } => (new_index, new_len),
                DiffOp::Replace { new_index, new_len, .. } => (new_index, new_len),
                _ => continue,
            };
            for i in start..start + len {
                let raw = String::from_utf8_lossy(nl[i]);
                let line = raw.trim_end_matches('\n').trim_end_matches('\r');
                let mut problems = Vec::new();
                if line.ends_with(' ') || line.ends_with('\t') {
                    problems.push("trailing whitespace");
                }
                let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
                if indent.contains(" \t") {
                    problems.push("space before tab in indent");
                }
                if line.starts_with("<<<<<<< ") || line.starts_with(">>>>>>> ") || line == "=======" {
                    problems.push("leftover conflict marker");
                }
                if !problems.is_empty() {
                    s.push_str(&format!("{}:{}: {}.\n+{}\n", f.path(), i + 1, problems.join(", "), line));
                }
            }
        }
    }
    s
}
