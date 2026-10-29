//! Stale test selection. The rules are in docs/retest.md.

use crate::deps::Ext;
use crate::parse::{in_ranges, overlaps, FileChange, Hunk};
use crate::record::{FileCov, Ranges, TestRec};

/// Line `l` of the old state in the new state. `None` when a hunk changed that line.
fn map_line(hunks: &[Hunk], l: u32) -> Option<u32> {
    let mut delta = 0i64;
    for h in hunks {
        let (old, new) = (h.old_len as i64, h.new_len as i64);
        if h.old_len == 0 {
            if l > h.old_start {
                delta += new;
            }
        } else if l > h.old_start + h.old_len - 1 {
            delta += new - old;
        } else if l >= h.old_start {
            return None;
        }
    }
    u32::try_from(l as i64 + delta).ok()
}

fn map_ranges(hunks: &[Hunk], r: &Ranges) -> Option<Ranges> {
    r.iter().map(|x| Some([map_line(hunks, x[0])?, map_line(hunks, x[1])?])).collect()
}

/// The entry of a test that stays unchanged, in the lines of the new state.
/// `None` when the test is stale, or when an end of a range was itself changed (run it, do not guess).
pub fn carry(rec: &TestRec, changes: &[FileChange], ext: &Ext) -> Option<TestRec> {
    if is_stale(rec, changes, ext) {
        return None;
    }
    let mut out = TestRec { status: rec.status.clone(), files: Default::default(), deps: rec.deps.clone() };
    for (path, cov) in &rec.files {
        let c = changes.iter().find(|c| &c.old_path == path);
        let (name, hunks) = c.map_or((path, &[][..]), |c| (&c.new_path, &c.hunks[..]));
        let cov = FileCov {
            lines: map_ranges(hunks, &cov.lines)?,
            funcs: map_ranges(hunks, &cov.funcs)?,
            idle: map_ranges(hunks, &cov.idle)?,
        };
        out.files.insert(name.clone(), cov);
    }
    Some(out)
}

/// A test is stale when a change since its record can alter what it executed.
pub fn is_stale(rec: &TestRec, changes: &[FileChange], ext: &Ext) -> bool {
    if rec.status != "pass" || ext.hits(rec) {
        return true;
    }
    changes.iter().any(|c| match rec.files.get(&c.old_path) {
        Some(cov) => c.hunks.iter().any(|h| hunk_hits(cov, h)),
        None => false,
    })
}

fn hunk_hits(cov: &FileCov, h: &Hunk) -> bool {
    if h.old_len > 0 {
        let (lo, hi) = (h.old_start, h.old_start + h.old_len - 1);
        // An executed line changed, or an executed function changed.
        if overlaps(&cov.lines, lo, hi) || overlaps(&cov.funcs, lo, hi) {
            return true;
        }
        // Module level: some changed line is in no function of the file.
        return (lo..=hi).any(|l| !in_ranges(&cov.funcs, l) && !in_ranges(&cov.idle, l));
    }
    // Pure insertion between line `old_start` and `old_start + 1`.
    let (a, b) = (h.old_start, h.old_start + 1);
    let inside = |r: &Vec<[u32; 2]>| r.iter().any(|x| x[0] <= a && b <= x[1]);
    if inside(&cov.funcs) {
        return true;
    }
    if inside(&cov.idle) {
        return false;
    }
    !only_functions_or_comments(&h.added)
}

/// True when the added text is blank lines, line comments, and whole function items.
/// A wrong "false" only makes more tests run. A string with unbalanced brackets can give a wrong "true".
fn only_functions_or_comments(added: &[String]) -> bool {
    let mut depth: i32 = 0;
    for l in added {
        let t = l.trim();
        if t.is_empty() || t.starts_with("//") {
            continue;
        }
        if depth == 0 && !(t.starts_with("#[") || starts_fn(t)) {
            return false;
        }
        for ch in t.chars() {
            match ch {
                '{' | '(' | '[' => depth += 1,
                '}' | ')' | ']' => depth -= 1,
                _ => {}
            }
        }
        // `#[attr]` closes its own bracket on the same line.
        if depth < 0 {
            return false;
        }
    }
    depth == 0
}

fn starts_fn(t: &str) -> bool {
    let mut rest = t;
    loop {
        let (w, r) = rest.split_once(' ').unwrap_or((rest, ""));
        let w = w.split('(').next().unwrap_or(w);
        match w {
            "fn" => return true,
            "pub" | "async" | "const" | "unsafe" | "extern" | "\"C\"" => rest = r,
            _ => return false,
        }
        if rest.is_empty() {
            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_hunks;
    use std::collections::BTreeMap;

    fn is_stale(r: &TestRec, c: &[FileChange]) -> bool {
        super::is_stale(r, c, &Ext::default())
    }

    fn carry(r: &TestRec, c: &[FileChange]) -> Option<TestRec> {
        super::carry(r, c, &Ext::default())
    }

    // File layout of the examples:
    //  1 use std::fmt;            module level
    //  2 const K: i32 = 1;        module level
    //  3 (blank)
    //  4 fn f(x: i32) -> i32 {    executed function 4..9
    //  5     match x {
    //  6         1 => 10,
    //  7         _ => 0,
    //  8     }
    //  9 }
    // 10 (blank)
    // 11 fn g() {                 idle function 11..13
    // 12     todo!()
    // 13 }
    fn rec(status: &str) -> TestRec {
        let mut files = BTreeMap::new();
        files.insert("a.rs".to_string(), FileCov { lines: vec![[4, 6], [9, 9]], funcs: vec![[4, 9]], idle: vec![[11, 13]] });
        TestRec { status: status.into(), files, deps: vec![] }
    }

    fn diff(old_start: &str, added: &[&str]) -> Vec<FileChange> {
        let mut d = format!("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ {old_start} +1 @@\n");
        for a in added {
            d.push_str(&format!("+{a}\n"));
        }
        parse_hunks(&d)
    }

    #[test]
    fn line_inserted_in_executed_function_is_stale() {
        // New match arm after line 6.
        assert!(is_stale(&rec("pass"), &diff("-6,0", &["        2 => 20,"])));
    }

    #[test]
    fn new_uncalled_function_is_not_stale() {
        let c = diff("-10,0", &["", "// helper", "fn h() -> i32 {", "    3", "}"]);
        assert!(!is_stale(&rec("pass"), &c));
    }

    #[test]
    fn module_level_changes_are_stale() {
        assert!(is_stale(&rec("pass"), &diff("-2,1", &["const K: i32 = 2;"])));
        assert!(is_stale(&rec("pass"), &diff("-2,0", &["const J: i32 = 2;"])));
        assert!(is_stale(&rec("pass"), &diff("-1,0", &["fn h() {}", "const J: i32 = 2;"])));
    }

    #[test]
    fn changed_executed_line_is_stale() {
        assert!(is_stale(&rec("pass"), &diff("-6", &["        1 => 11,"])));
    }

    #[test]
    fn change_in_idle_function_is_not_stale() {
        assert!(!is_stale(&rec("pass"), &diff("-12", &["    unimplemented!()"])));
        assert!(!is_stale(&rec("pass"), &diff("-12,0", &["    let _a = 1;"])));
    }

    #[test]
    fn insertion_at_function_edges_is_outside_the_function() {
        // After the closing brace of f, and before the header of f.
        assert!(!is_stale(&rec("pass"), &diff("-9,0", &["fn h() {}"])));
        assert!(!is_stale(&rec("pass"), &diff("-3,0", &["fn h() {}"])));
    }

    #[test]
    fn carry_shifts_ranges_and_follows_renames() {
        // Ten lines inserted above everything: f 4..9 becomes 14..19.
        let ins = diff("-0,0", &["fn h() {}"; 10]);
        let mut ins = ins;
        ins[0].hunks[0].new_len = 10;
        ins[0].new_path = "b.rs".into();
        let r = carry(&rec("pass"), &ins).unwrap();
        let cov = &r.files["b.rs"];
        assert_eq!(
            (cov.funcs.clone(), cov.idle.clone(), cov.lines.clone()),
            (vec![[14, 19]], vec![[21, 23]], vec![[14, 16], [19, 19]])
        );
        // An edit inside idle `g` at its first line changes a range end: not carried.
        assert!(carry(&rec("pass"), &diff("-11", &["fn g() {"])).is_none());
        // A stale test is not carried.
        assert!(carry(&rec("pass"), &diff("-6", &["x"])).is_none());
    }

    #[test]
    fn failed_test_and_other_files() {
        assert!(is_stale(&rec("fail"), &[]));
        let mut c = diff("-6", &["x"]);
        c[0].old_path = "b.rs".into();
        c[0].new_path = "b.rs".into();
        assert!(!is_stale(&rec("pass"), &c));
    }
}
