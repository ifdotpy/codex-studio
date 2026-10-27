//! Diff output in git formats: patches, `--stat`, `--numstat`, `--name-status`, `--check`.

use crate::changes::Change;
use crate::fsutil::{self, FType, Meta};
use anyhow::Result;
use similar::{capture_diff_slices, Algorithm, DiffOp};

use layr_model::diff::split_lines;
pub use layr_model::diff::{patch, FileDiff, Side};

pub fn side_of(root: &(impl fsutil::Src + ?Sized), path: &str, meta: &Meta) -> Result<Side> {
    let big = (meta.ftype == FType::File && meta.size > fsutil::MAX_TEXT).then_some(meta.size);
    let data = match meta.ftype {
        _ if big.is_some() => Vec::new(),
        FType::File | FType::Symlink => fsutil::entry_bytes(root, path)?,
        _ => Vec::new(),
    };
    Ok(Side { path: path.to_string(), mode: meta.git_mode(), data, big })
}

/// Build file diffs for a change list between two roots.
pub fn file_diffs(
    old_root: &(impl fsutil::Src + ?Sized),
    new_root: &(impl fsutil::Src + ?Sized),
    changes: &[Change],
) -> Result<Vec<FileDiff>> {
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
