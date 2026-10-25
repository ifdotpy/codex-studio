//! Git patch rendering from caller-supplied bytes and modes. No I/O.

use similar::{capture_diff_slices, get_diff_ratio, group_diff_ops, Algorithm, DiffOp};

/// Files above this bound use binary patch output without content reads.
pub const MAX_TEXT: u64 = 64 << 20;

/// Git treats a NUL in the first 8000 bytes as binary data.
pub fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(8000)].contains(&0)
}

fn git_blob_id(data: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut hash = Sha1::new();
    hash.update(format!("blob {}\0", data.len()).as_bytes());
    hash.update(data);
    hex::encode(hash.finalize())
}

/// One side of a file in a diff.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Side {
    pub path: String,
    pub mode: u32,
    /// The content; empty for a file larger than `MAX_TEXT` (`big`), which counts as
    /// binary and is never read.
    pub data: Vec<u8>,
    pub big: Option<u64>,
}

impl Side {
    pub fn len(&self) -> usize {
        self.big.map(|n| n as usize).unwrap_or(self.data.len())
    }
}

/// Same content. Two big files are never compared (the change list says they changed).
fn same(o: &Side, n: &Side) -> bool {
    o.big.is_none() && n.big.is_none() && o.data == n.data
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct FileDiff {
    pub old: Option<Side>,
    pub new: Option<Side>,
}

impl FileDiff {
    pub fn path(&self) -> &str {
        self.new.as_ref().or(self.old.as_ref()).map(|s| s.path.as_str()).unwrap_or("")
    }
    pub fn is_binary(&self) -> bool {
        self.old.as_ref().map(|s| s.big.is_some() || is_binary(&s.data)).unwrap_or(false)
            || self.new.as_ref().map(|s| s.big.is_some() || is_binary(&s.data)).unwrap_or(false)
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

fn short_blob(data: Option<&Side>) -> String {
    match data {
        Some(s) => git_blob_id(&s.data)[..7].to_string(),
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

pub fn split_lines(d: &[u8]) -> Vec<&[u8]> {
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
