//! Stale test selection. The rules are in docs/retest.md.

use crate::deps::Ext;
use crate::parse::{in_ranges, overlaps, FileChange, Hunk};
use crate::record::{FileCov, Ranges, TestRec};
use std::collections::BTreeMap;

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

/// Files that cargo reads besides Rust source: a change there can add a test target or break the build.
fn is_build_input(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.starts_with(".cargo/")
        || path.contains("/.cargo/")
        || matches!(name, "Cargo.toml" | "Cargo.lock" | "build.rs" | "rust-toolchain" | "rust-toolchain.toml" | "nextest.toml")
}

/// True when the change since the baseline cannot add, remove or break a test, so no build is needed
/// (given that no recorded test is stale): only blank lines and line comments of Rust files changed,
/// plus files that cargo does not read. `name_status` is the output of `layr diff --name-status`.
/// Any other change to a Rust file, a new, deleted or renamed Rust file, and any build input
/// (`Cargo.toml`, `Cargo.lock`, `build.rs`, cargo and nextest config) needs the build and the test list.
/// A change in a function that no test ran still needs the build: it may not compile.
pub fn no_build_needed(name_status: &str, changes: &[FileChange]) -> bool {
    let comment = |l: &String| l.trim().is_empty() || l.trim_start().starts_with("//");
    name_status.lines().filter(|l| !l.is_empty()).all(|l| {
        let mut f = l.split('\t');
        let status = f.next().unwrap_or_default();
        let paths: Vec<&str> = f.collect();
        if paths.iter().any(|p| is_build_input(p)) {
            return false;
        }
        if !paths.iter().any(|p| p.ends_with(".rs")) {
            return true;
        }
        status == "M"
            && changes
                .iter()
                .find(|c| paths.contains(&c.old_path.as_str()))
                .is_some_and(|c| c.hunks.iter().all(|h| h.added.iter().chain(&h.removed).all(comment)))
    })
}

/// Changed files of bin targets that no test record knows. A test can start such a bin and stop it
/// with SIGKILL: LLVM then writes no profile, so the code is in no record and a change of it would
/// select no test. `bins` are the source files of the bin targets, relative to the line root.
/// Files that are new in the diff are not in `changes`, so they are not listed.
pub fn unrecorded_bin_changes<'a>(bins: &[String], changes: &'a [FileChange], tests: &BTreeMap<String, TestRec>) -> Vec<&'a str> {
    changes
        .iter()
        .map(|c| c.old_path.as_str())
        .filter(|p| bins.iter().any(|b| b == p) && !tests.values().any(|t| t.files.contains_key(*p)))
        .collect()
}

fn hunk_hits(cov: &FileCov, h: &Hunk) -> bool {
    if h.old_len > 0 {
        let (lo, hi) = (h.old_start, h.old_start + h.old_len - 1);
        // An executed line changed, or an executed function changed.
        if overlaps(&cov.lines, lo, hi) || overlaps(&cov.funcs, lo, hi) {
            return true;
        }
        // Module level: some changed line is in no function of the file. Blank lines and line
        // comments there change nothing.
        let blank_or_comment = |l: &String| l.trim().is_empty() || l.trim_start().starts_with("//");
        if !(h.removed.is_empty() && h.added.is_empty()) && h.removed.iter().chain(&h.added).all(blank_or_comment) {
            return false;
        }
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

    fn rs_diff(path: &str, removed: &[&str], added: &[&str]) -> Vec<FileChange> {
        let mut d = format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -5,{} +5,{} @@\n", removed.len(), added.len());
        d.extend(removed.iter().map(|r| format!("-{r}\n")));
        d.extend(added.iter().map(|a| format!("+{a}\n")));
        parse_hunks(&d)
    }

    #[test]
    fn no_build_for_files_that_cargo_does_not_read() {
        assert!(no_build_needed("", &[]));
        assert!(no_build_needed("M\tdocs/a.md\nA\tweb/x.ts\nD\tweb/y.json\n", &[]));
    }

    #[test]
    fn build_for_build_inputs() {
        for p in ["Cargo.toml", "crates/a/Cargo.toml", "Cargo.lock", "a/build.rs", ".cargo/config.toml", "rust-toolchain.toml", ".config/nextest.toml"] {
            assert!(!no_build_needed(&format!("M\t{p}\n"), &[]), "{p}");
        }
    }

    #[test]
    fn build_for_added_deleted_or_renamed_rust_files() {
        for s in ["A\tsrc/new.rs", "D\tsrc/old.rs", "R100\tsrc/a.rs\tsrc/b.rs"] {
            assert!(!no_build_needed(s, &[]), "{s}");
        }
    }

    #[test]
    fn no_build_for_comment_and_blank_changes_only() {
        let c = rs_diff("src/a.rs", &["// old"], &["", "    // new", "/// doc"]);
        assert!(no_build_needed("M\tsrc/a.rs", &c));
        // Code removed, code added, or a block comment: build.
        assert!(!no_build_needed("M\tsrc/a.rs", &rs_diff("src/a.rs", &["let x = 1;"], &["// gone"])));
        assert!(!no_build_needed("M\tsrc/a.rs", &rs_diff("src/a.rs", &[], &["#[test]", "fn t() {}"])));
        assert!(!no_build_needed("M\tsrc/a.rs", &rs_diff("src/a.rs", &[], &["/* c */"])));
        // A modified Rust file with no hunk is not understood: build.
        assert!(!no_build_needed("M\tsrc/a.rs", &[]));
    }

    #[test]
    fn changed_bin_file_that_no_record_knows_is_listed() {
        let bins = vec!["src/bin/sup.rs".to_string(), "src/main.rs".to_string()];
        let tests: BTreeMap<String, TestRec> = [("t".to_string(), rec("pass"))].into();
        let change = |p: &str| rs_diff(p, &["x"], &["y"]);
        // A bin file in no record: listed.
        assert_eq!(unrecorded_bin_changes(&bins, &change("src/bin/sup.rs"), &tests), vec!["src/bin/sup.rs"]);
        // A bin file that a record has (rec() has a.rs): not listed.
        let bins_a = vec!["a.rs".to_string()];
        assert!(unrecorded_bin_changes(&bins_a, &change("a.rs"), &tests).is_empty());
        // A file that is not a bin target, and no change at all: not listed.
        assert!(unrecorded_bin_changes(&bins, &change("src/lib.rs"), &tests).is_empty());
        assert!(unrecorded_bin_changes(&bins, &[], &tests).is_empty());
        // A new file is not in `changes` (parse_hunks leaves it out), so it is not listed.
        let new = parse_hunks("diff --git a/src/main.rs b/src/main.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/main.rs\n@@ -0,0 +1 @@\n+fn main() {}\n");
        assert!(unrecorded_bin_changes(&bins, &new, &tests).is_empty());
    }

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
    fn module_level_comment_changes_are_not_stale() {
        // Line 1 is `use std::fmt;` in the layout above; here the diff says it was a comment.
        let mut c = diff("-1", &["// other"]);
        c[0].hunks[0].removed = vec!["// old".into()];
        assert!(!is_stale(&rec("pass"), &c));
        // Removing a comment line, and replacing a comment with code, are different.
        let mut gone = diff("-1", &[]);
        gone[0].hunks[0].removed = vec!["".into()];
        gone[0].hunks[0].new_len = 0;
        assert!(!is_stale(&rec("pass"), &gone));
        let mut code = diff("-1", &["const J: i32 = 2;"]);
        code[0].hunks[0].removed = vec!["// old".into()];
        assert!(is_stale(&rec("pass"), &code));
        let mut code = diff("-1", &["// c"]);
        code[0].hunks[0].removed = vec!["use std::fmt;".into()];
        assert!(is_stale(&rec("pass"), &code));
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
