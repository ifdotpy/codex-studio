//! Commands that change or report the working content of a line: status, add, rm, mv, apply,
//! commit, restore, reset, stash, clean.

use super::{exit, here, matches, Here};
use crate::args::{parse, Spec};
use crate::changes::{self, Change};
use crate::ctx::Ctx;
use crate::diff;
use crate::fsutil;
use crate::ids;
use crate::model::{Line, Record, StateKind};
use crate::repo::{conflict_open, skip_path, Repo, Status};
use crate::revs;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

fn xy(c: &Change) -> char {
    match (&c.old, &c.new) {
        (None, Some(_)) => 'A',
        (Some(_), None) => 'D',
        _ if c.old_path.is_some() => 'R',
        (Some(o), Some(n)) if o.ftype != n.ftype => 'T',
        _ => 'M',
    }
}

fn conflict_code(kind: &str) -> &'static str {
    match kind {
        "add/add" => "AA",
        "modify/delete" => "UD",
        "delete/modify" => "DU",
        "rename/delete" => "AU",
        _ => "UU",
    }
}

pub fn status(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .flag("-s|--short")
        .flag("-b|--branch")
        .optional("--porcelain")
        .optional("-u|--untracked-files")
        .optional("--ignored")
        .flag("-z")
        .flag("--long")
        .flag("-v|--verbose")
        .flag("--no-renames");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let specs = h.specs(&a.rest())?;
    let (st, _scan, _itemp, _index) = h.repo.status(&line)?;
    let porcelain = a.has("--porcelain");
    let short = a.has("-s") || porcelain;
    let untracked_mode = a.get("-u").map(|s| if s.is_empty() { "all" } else { s }).unwrap_or("normal").to_string();
    let show_ignored = a.has("--ignored");
    let nul = a.has("-z");
    let end = if nul { "\0" } else { "\n" };
    let path_of = |p: &str| -> String {
        if porcelain || nul {
            p.to_string()
        } else {
            h.rel(p)
        }
    };
    let conflicted: BTreeSet<String> = st.conflicts.iter().map(|c| c.path.clone()).collect();

    if short {
        if a.has("-b") {
            out!(ctx, "## {}{}", line.name, end);
        }
        let mut rows: BTreeMap<String, (char, char, Option<String>)> = BTreeMap::new();
        for c in &st.staged {
            if !matches(&specs, &c.path) {
                continue;
            }
            rows.insert(c.path.clone(), (xy(c), ' ', c.old_path.clone()));
        }
        for c in &st.unstaged {
            if !matches(&specs, &c.path) {
                continue;
            }
            let e = rows.entry(c.path.clone()).or_insert((' ', ' ', None));
            e.1 = xy(c);
        }
        for c in &st.conflicts {
            if !matches(&specs, &c.path) {
                continue;
            }
            let code = conflict_code(&c.kind);
            let b = code.as_bytes();
            rows.insert(c.path.clone(), (b[0] as char, b[1] as char, None));
        }
        for (p, (x, y, from)) in &rows {
            match from {
                Some(f) if nul => out!(ctx, "{x}{y} {}\0{}\0", path_of(p), path_of(f)),
                Some(f) => out!(ctx, "{x}{y} {} -> {}\n", path_of(f), path_of(p)),
                None => out!(ctx, "{x}{y} {}{}", path_of(p), end),
            }
        }
        if untracked_mode != "no" {
            for u in untracked_list(&st, &untracked_mode) {
                if matches(&specs, u.trim_end_matches('/')) {
                    out!(ctx, "?? {}{}", path_of(&u), end);
                }
            }
        }
        if show_ignored {
            for i in &st.ignored {
                if matches(&specs, i) {
                    out!(ctx, "!! {}{}", path_of(i), end);
                }
            }
        }
        return Ok(0);
    }

    outln!(ctx, "On line {}", line.name);
    let head = h.repo.state(&line.head)?;
    if !head.conflicts.is_empty() && !st.conflicts.is_empty() {
        outln!(ctx, "You have unmerged paths.\n  (fix conflicts and run \"layr commit\")\n");
    }
    let staged: Vec<&Change> = st.staged.iter().filter(|c| matches(&specs, &c.path) && !conflicted.contains(&c.path)).collect();
    let unstaged: Vec<&Change> =
        st.unstaged.iter().filter(|c| matches(&specs, &c.path) && !conflicted.contains(&c.path)).collect();
    let untracked: Vec<String> = if untracked_mode == "no" {
        vec![]
    } else {
        untracked_list(&st, &untracked_mode).into_iter().filter(|u| matches(&specs, u.trim_end_matches('/'))).collect()
    };
    let label = |c: &Change| -> String {
        match xy(c) {
            'A' => format!("new file:   {}", h.rel(&c.path)),
            'D' => format!("deleted:    {}", h.rel(&c.path)),
            'R' => format!("renamed:    {} -> {}", h.rel(c.old_path.as_deref().unwrap_or("")), h.rel(&c.path)),
            'T' => format!("typechange: {}", h.rel(&c.path)),
            _ => format!("modified:   {}", h.rel(&c.path)),
        }
    };
    if !staged.is_empty() {
        outln!(ctx, "Changes to be committed:\n  (use \"layr restore --staged <file>...\" to unstage)");
        for c in &staged {
            outln!(ctx, "\t{}", label(c));
        }
        outln!(ctx);
    }
    if !st.conflicts.is_empty() {
        outln!(ctx, "Unmerged paths:\n  (fix the files, then \"layr add <file>...\" and \"layr commit\")");
        for c in &st.conflicts {
            if matches(&specs, &c.path) {
                let what = match c.kind.as_str() {
                    "content" => "both modified:",
                    "add/add" => "both added:   ",
                    "modify/delete" => "deleted by them:",
                    "delete/modify" => "deleted by us:",
                    _ => "conflict:     ",
                };
                outln!(ctx, "\t{what}   {} ({})", h.rel(&c.path), c.kind);
            }
        }
        outln!(ctx);
    }
    if !unstaged.is_empty() {
        outln!(ctx, "Changes not staged for commit:\n  (use \"layr add <file>...\" to update what will be committed)\n  (use \"layr restore <file>...\" to discard changes in working directory)");
        for c in &unstaged {
            outln!(ctx, "\t{}", label(c));
        }
        outln!(ctx);
    }
    if !untracked.is_empty() {
        outln!(ctx, "Untracked files:\n  (use \"layr add <file>...\" to include in what will be committed)");
        for u in &untracked {
            outln!(ctx, "\t{}", h.rel(u));
        }
        outln!(ctx);
    }
    if show_ignored && !st.ignored.is_empty() {
        outln!(ctx, "Ignored files:");
        for i in &st.ignored {
            outln!(ctx, "\t{}", h.rel(i));
        }
        outln!(ctx);
    }
    if staged.is_empty() {
        if !unstaged.is_empty() {
            outln!(ctx, "no changes added to commit (use \"layr add\" and/or \"layr commit -a\")");
        } else if !untracked.is_empty() {
            outln!(ctx, "nothing added to commit but untracked files present (use \"layr add\" to track)");
        } else if st.conflicts.is_empty() {
            outln!(ctx, "nothing to commit, working tree clean");
        }
    }
    Ok(0)
}

fn untracked_list(st: &Status, mode: &str) -> Vec<String> {
    if mode == "all" {
        return st.untracked.clone();
    }
    let mut out: BTreeSet<String> = BTreeSet::new();
    for f in &st.untracked {
        match st.untracked_dirs.iter().find(|d| f.starts_with(&format!("{d}/"))) {
            Some(d) => {
                out.insert(format!("{d}/"));
            }
            None => {
                out.insert(f.clone());
            }
        }
    }
    out.into_iter().collect()
}

/// Lock the project and save the working content (snapshot rule 2).
pub fn begin_change(ctx: &Ctx, repo: &Repo, line: &Line) -> Result<(crate::store::Lock, Option<String>)> {
    begin(ctx, repo, line, crate::repo::LineOp::Direct)
}

/// `begin_change` for another kind of change (a merge).
pub fn begin(ctx: &Ctx, repo: &Repo, line: &Line, op: crate::repo::LineOp) -> Result<(crate::store::Lock, Option<String>)> {
    repo.authorize_line(ctx, line, op)?;
    let lock = repo.p.lock()?;
    let line = repo.line(&line.id)?;
    let w = repo.working_state(ctx, &line, "before a layr command")?;
    Ok((lock, w))
}

/// Stage paths: copy them from the working snapshot into the staging line. A file that moved
/// in the working folder (same inode number) is moved in the staging line too, so states keep
/// the inode and btrfs reports the move.
fn stage_paths(repo: &Repo, line: &Line, scan: &Path, paths: &BTreeSet<String>) -> Result<()> {
    let stage = repo.ensure_stage(line)?;
    let own = repo.own(line);
    let mut gone: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
    for p in paths {
        if fsutil::lmeta(scan, p).is_none() {
            if let Some(m) = fsutil::lmeta(&stage, p) {
                if m.is_file() || m.ftype == fsutil::FType::Symlink {
                    gone.insert(m.ino, p.clone());
                }
            }
        }
    }
    for p in paths {
        if let (Some(m), None) = (fsutil::lmeta(scan, p), fsutil::lmeta(&stage, p)) {
            if let Some(from) = gone.remove(&m.ino) {
                let t = crate::tree::Tree::open(&stage)?;
                t.mkdirs_for(p, own)?;
                t.rename(&from, p)?;
            }
        }
    }
    for p in paths {
        if fsutil::lmeta(scan, p).is_some() {
            fsutil::copy_entry(scan, p, &stage, p, own)?;
        } else if fsutil::lmeta(&stage, p).is_some() {
            fsutil::remove_entry(&stage, p)?;
            prune_empty(&stage, p);
        }
    }
    Ok(())
}

/// Remove parent folders that became empty (states hold files, as git trees do).
fn prune_empty(root: &Path, rel: &str) {
    if let Ok(t) = crate::tree::Tree::open(root) {
        prune_empty_in(&t, rel);
    }
}

fn prune_empty_in(t: &crate::tree::Tree, rel: &str) {
    let mut comps: Vec<&str> = rel.split('/').collect();
    comps.pop();
    while !comps.is_empty() && t.rmdir(&comps.join("/")) {
        comps.pop();
    }
}

pub fn add(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .flag("-A|--all")
        .flag("-u|--update")
        .flag("-f|--force")
        .flag("-n|--dry-run")
        .flag("-v|--verbose")
        .flag("--no-all")
        .flag("-N|--intent-to-add")
        .flag("-p|--patch")
        .flag("-i|--interactive");
    let a = parse(&spec, args)?;
    if a.has("-p") || a.has("-i") {
        return Err(exit(
            1,
            "interactive add is not supported: write the hunks to a patch file and run 'layr apply --cached <file>'",
        ));
    }
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let mut specs = h.specs(&a.rest())?;
    if specs.is_empty() && !(a.has("-A") || a.has("-u")) {
        return Err(exit(0, "Nothing specified, nothing added.\nhint: Maybe you wanted to say 'layr add .'?"));
    }
    if specs.is_empty() {
        specs = vec![String::new()];
    }
    h.repo.authorize_line(ctx, &line, crate::repo::LineOp::Direct)?;
    let _lock = h.repo.p.lock()?;
    let (st, scan, _it, index) = h.repo.status(&line)?;
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for c in &st.unstaged {
        if matches(&specs, &c.path) {
            paths.insert(c.path.clone());
        }
    }
    if !a.has("-u") {
        for u in &st.untracked {
            if matches(&specs, u) {
                paths.insert(u.clone());
            }
        }
    }
    // Ignored paths named explicitly.
    let mut ignored_named = Vec::new();
    for s in &specs {
        if s.is_empty() {
            continue;
        }
        let in_scan = fsutil::lmeta(&scan.path, s).is_some();
        let in_index = fsutil::lmeta(&index, s).is_some();
        let matched = paths.iter().any(|p| p == s || p.starts_with(&format!("{s}/"))) || s.contains(['*', '?']);
        if !matched {
            if st.ignored.iter().any(|i| i == s || i.starts_with(&format!("{s}/"))) {
                if a.has("-f") {
                    for i in st.ignored.iter().filter(|i| *i == s || i.starts_with(&format!("{s}/"))) {
                        paths.insert(i.clone());
                    }
                } else {
                    ignored_named.push(h.rel(s));
                }
            } else if !in_scan && !in_index {
                return Err(exit(128, format!("fatal: pathspec '{}' did not match any files", h.rel(s))));
            }
        }
    }
    if a.has("-n") {
        for p in &paths {
            let what = if fsutil::lmeta(&scan.path, p).is_some() { "add" } else { "remove" };
            outln!(ctx, "{what} '{}'", h.rel(p));
        }
        return Ok(0);
    }
    stage_paths(&h.repo, &line, &scan.path, &paths)?;
    if a.has("-v") {
        for p in &paths {
            let what = if fsutil::lmeta(&scan.path, p).is_some() { "add" } else { "remove" };
            outln!(ctx, "{what} '{}'", h.rel(p));
        }
    }
    if !ignored_named.is_empty() {
        let mut m = String::from("The following paths are ignored by one of your .gitignore files:\n");
        for i in ignored_named {
            m.push_str(&format!("{i}\n"));
        }
        m.push_str("hint: Use -f if you really want to add them.");
        return Err(exit(1, m));
    }
    Ok(0)
}

pub fn rm(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("--cached").flag("-r").flag("-f|--force").flag("-q|--quiet").flag("-n|--dry-run");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let specs = h.specs(&a.rest())?;
    if specs.is_empty() {
        return Err(exit(128, "usage: layr rm [--cached] [-r] [-f] <path>..."));
    }
    let (_lock, _w) = begin_change(ctx, &h.repo, &line)?;
    let line = h.repo.line(&line.id)?;
    let stage = h.repo.ensure_stage(&line)?;
    let w = h.repo.working(&line);
    let mut removed = Vec::new();
    for s in &specs {
        let m = fsutil::lmeta(&stage, s);
        match m {
            None => return Err(exit(128, format!("fatal: pathspec '{}' did not match any files", h.rel(s)))),
            Some(m) if m.is_dir() && !a.has("-r") => {
                return Err(exit(128, format!("fatal: not removing '{}' recursively without -r", h.rel(s))))
            }
            _ => {}
        }
        if a.has("-n") {
            removed.push(s.clone());
            continue;
        }
        let mut files = Vec::new();
        if fsutil::lmeta(&stage, s).map(|m| m.is_dir()).unwrap_or(false) {
            fsutil::walk(&stage, s, &mut |rel, m| {
                if !m.is_dir() {
                    files.push(rel.to_string());
                }
                Ok(true)
            })?;
        } else {
            files.push(s.clone());
        }
        fsutil::remove_entry(&stage, s)?;
        if !a.has("--cached") {
            for f in &files {
                fsutil::remove_entry(&w, f)?;
            }
            if crate::tree::Tree::open(&w)?.stat(s).map(|m| m.is_dir()).unwrap_or(false) {
                crate::tree::Tree::open(&w)?.rmdir(s);
            }
        }
        removed.extend(files);
    }
    if !a.has("-q") {
        for r in removed {
            outln!(ctx, "rm '{}'", h.rel(&r));
        }
    }
    Ok(0)
}

pub fn mv(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("-f|--force").flag("-n|--dry-run").flag("-k").flag("-v|--verbose");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let rest = h.specs(&a.rest())?;
    if rest.len() < 2 {
        return Err(exit(128, "usage: layr mv <source>... <destination>"));
    }
    let (_lock, _w) = begin_change(ctx, &h.repo, &line)?;
    let line = h.repo.line(&line.id)?;
    let w = h.repo.working(&line);
    let dst = rest.last().unwrap().clone();
    let dst_is_dir = fsutil::lmeta(&w, &dst).map(|m| m.is_dir()).unwrap_or(false);
    let mut moves = Vec::new();
    for src in &rest[..rest.len() - 1] {
        if fsutil::lmeta(&w, src).is_none() {
            return Err(exit(128, format!("fatal: bad source, source={}, destination={}", h.rel(src), h.rel(&dst))));
        }
        let name = src.rsplit('/').next().unwrap_or(src);
        let target = if dst_is_dir {
            if dst.is_empty() {
                name.to_string()
            } else {
                format!("{dst}/{name}")
            }
        } else {
            dst.clone()
        };
        if fsutil::lmeta(&w, &target).is_some() && !a.has("-f") {
            return Err(exit(128, format!("fatal: destination exists, source={}, destination={}", h.rel(src), h.rel(&target))));
        }
        moves.push((src.clone(), target));
    }
    if a.has("-n") {
        for (s, t) in &moves {
            outln!(ctx, "Renaming {} to {}", h.rel(s), h.rel(t));
        }
        return Ok(0);
    }
    let tree = crate::tree::Tree::open(&w)?;
    for (s, t) in &moves {
        tree.mkdirs_for(t, h.repo.own(&line))?;
        tree.rename(s, t)?;
    }
    // Stage both sides of each move.
    let scan = h.repo.scan(&line)?;
    let mut paths = BTreeSet::new();
    for (s, t) in &moves {
        {
            let root = &scan.path;
            let mut push = |p: &str| {
                paths.insert(p.to_string());
            };
            push(s);
            push(t);
            if fsutil::lmeta(root, t).map(|m| m.is_dir()).unwrap_or(false) {
                fsutil::walk(root, t, &mut |rel, _| {
                    paths.insert(rel.to_string());
                    Ok(true)
                })?;
            }
        }
        let stage = h.repo.ensure_stage(&line)?;
        if fsutil::lmeta(&stage, s).map(|m| m.is_dir()).unwrap_or(false) {
            fsutil::remove_entry(&stage, s)?;
        }
    }
    stage_paths(&h.repo, &line, &scan.path, &paths)?;
    if a.has("-v") {
        for (s, t) in &moves {
            outln!(ctx, "Renaming {} to {}", h.rel(s), h.rel(t));
        }
    }
    Ok(0)
}

/// Read a file named by the caller with the caller's permissions.
/// Read at most `limit` bytes; more is an error (the service holds the input in memory).
pub fn read_limited(r: &mut dyn std::io::Read, limit: u64, what: &str) -> Result<Vec<u8>> {
    let mut v = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(r, limit + 1), &mut v)?;
    if v.len() as u64 > limit {
        anyhow::bail!("{what} is larger than {} MiB", limit >> 20);
    }
    Ok(v)
}

/// A file named by the caller (a patch, a message, records), read with the caller's
/// permissions, or stdin for `-`.
pub fn read_caller_file(ctx: &Ctx, path: &str, limit: u64) -> Result<Vec<u8>> {
    if path == "-" {
        return read_limited(&mut std::io::stdin(), limit, "the input");
    }
    let p = if path.starts_with('/') { std::path::PathBuf::from(path) } else { ctx.cwd.join(path) };
    let mut f = crate::privs::as_caller_fs(ctx, || Ok(std::fs::File::open(&p)?))?;
    read_limited(&mut f, limit, path)
}

pub fn apply(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .flag("--cached")
        .flag("--index")
        .flag("--check")
        .flag("-R|--reverse")
        .flag("-v|--verbose")
        .flag("--stat")
        .flag("--numstat")
        .flag("--3way|-3")
        .value("-p")
        .flag("--recount")
        .flag("--unidiff-zero");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let files = a.rest();
    let patch: Vec<u8> = if files.is_empty() || files[0] == "-" {
        read_caller_file(ctx, "-", fsutil::MAX_TEXT)?
    } else {
        let mut v = Vec::new();
        for f in &files {
            v.extend(read_caller_file(ctx, f, fsutil::MAX_TEXT)?);
        }
        v
    };
    let mut flags: Vec<String> = Vec::new();
    for f in ["-R", "--recount", "--unidiff-zero", "-v"] {
        if a.has(f) {
            flags.push(f.into());
        }
    }
    if let Some(p) = a.get("-p") {
        flags.push(format!("-p{p}"));
    }
    if a.has("--stat") || a.has("--numstat") {
        let mode = if a.has("--stat") { "--stat" } else { "--numstat" };
        let out = run_git_apply(Path::new("/"), &patch, &[mode.to_string()], ctx.caller.uid)?;
        ctx.print(&out);
        return Ok(0);
    }
    let to_stage = a.has("--cached") || a.has("--index");
    let to_work = !a.has("--cached");
    let (_lock, _w) = begin_change(ctx, &h.repo, &line)?;
    let line = h.repo.line(&line.id)?;
    let own = h.repo.own(&line);
    // The staging line is private to the service (applied as root, then given to the owner);
    // the working folder belongs to the owner, so the patch is applied there as the owner.
    let mut targets: Vec<(std::path::PathBuf, u32)> = Vec::new();
    if to_stage {
        targets.push((h.repo.ensure_stage(&line)?, 0));
    }
    if to_work {
        targets.push((h.repo.working(&line), line.owner));
    }
    // Check all targets first, then apply.
    for (t, uid) in &targets {
        let mut f = flags.clone();
        f.push("--check".into());
        run_git_apply(t, &patch, &f, *uid)?;
    }
    if a.has("--check") {
        return Ok(0);
    }
    let touched = patch_paths(&patch);
    for (t, uid) in &targets {
        run_git_apply(t, &patch, &flags, *uid)?;
        if *uid == 0 {
            if let Some((u, g)) = own {
                for p in &touched {
                    if let Ok(full) = fsutil::safe_join(t, p) {
                        if full.symlink_metadata().is_ok() {
                            fsutil::chown_nofollow(&full, u, g)?;
                        }
                    }
                }
            }
        }
    }
    Ok(0)
}

fn patch_paths(patch: &[u8]) -> Vec<String> {
    let mut v = Vec::new();
    for l in String::from_utf8_lossy(patch).lines() {
        for pre in ["+++ b/", "--- a/"] {
            if let Some(p) = l.strip_prefix(pre) {
                v.push(p.trim().to_string());
            }
        }
    }
    v.sort();
    v.dedup();
    v
}

fn run_git_apply(dir: &Path, patch: &[u8], flags: &[String], uid: u32) -> Result<String> {
    use std::io::Write;
    let mut c = std::process::Command::new("git");
    c.arg("apply").args(flags).arg("-").current_dir(dir);
    let env = vec![
        ("GIT_CEILING_DIRECTORIES".to_string(), dir.parent().unwrap_or(Path::new("/")).to_string_lossy().into_owned()),
        ("GIT_DIR".to_string(), "/nonexistent-layr".to_string()),
    ];
    crate::privs::command_as(&mut c, uid, &env)?;
    c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut ch = c.spawn()?;
    ch.stdin.take().unwrap().write_all(patch)?;
    let out = ch.wait_with_output()?;
    if !out.status.success() {
        return Err(exit(1, String::from_utf8_lossy(&out.stderr).trim().replace("/nonexistent-layr", "").to_string()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn commit(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .value("-m|--message")
        .value("-F|--file")
        .flag("-a|--all")
        .flag("--amend")
        .flag("--allow-empty")
        .flag("--no-verify|-n")
        .flag("-q|--quiet")
        .flag("-v|--verbose")
        .flag("--no-edit")
        .value("--author")
        .flag("-s|--signoff");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let mut message: String = a.all("-m").join("\n\n");
    if let Some(f) = a.get("-F") {
        message = String::from_utf8_lossy(&read_caller_file(ctx, f, fsutil::MAX_TEXT)?).into_owned();
    }
    let head = h.repo.state(&line.head)?;
    if a.has("--amend") {
        // The amended state replaces the head: the line's history changes.
        h.repo.authorize_line(ctx, &line, crate::repo::LineOp::Rewrite)?;
    }
    if a.has("--amend") && message.is_empty() {
        message = head.message.clone();
    }
    let message = message.trim_end().to_string();
    if message.is_empty() {
        return Err(exit(1, "Aborting commit due to empty commit message (use -m <message>)."));
    }
    h.repo.authorize_line(ctx, &line, crate::repo::LineOp::Direct)?;
    let _lock = h.repo.p.lock()?;
    let line = h.repo.line(&line.id)?;
    if a.has("-a") {
        let (st, scan, _it, _index) = h.repo.status(&line)?;
        let paths: BTreeSet<String> = st.unstaged.iter().map(|c| c.path.clone()).collect();
        if !paths.is_empty() {
            stage_paths(&h.repo, &line, &scan.path, &paths)?;
        }
    }
    // The new state is a snapshot of the staging line (or the head when nothing is staged);
    // its changes against the head say whether there is anything to commit.
    let head_root = h.repo.state_root(&line.head)?;
    let parents = if a.has("--amend") { head.parents.clone() } else { vec![line.head.clone()] };
    let src = match h.repo.current_stage(&line)? {
        Some(s) => s,
        None => head_root.clone(),
    };
    let mut state = h.repo.new_state(ctx, &src, StateKind::Commit, parents, &message, Some(&line))?;
    let staged: Vec<Change> = match changes::file_changes(&head_root, &h.repo.p.state_path(&state.id)) {
        Ok(v) => v.into_iter().filter(|c| !skip_path(&c.path)).collect(),
        Err(e) => {
            h.repo.discard_states(std::slice::from_ref(&state));
            return Err(e);
        }
    };
    if staged.is_empty() && !a.has("--allow-empty") && !a.has("--amend") {
        h.repo.discard_states(std::slice::from_ref(&state));
        let (st2, _s2, _i2, _x2) = h.repo.status(&line)?;
        outln!(ctx, "On line {}", line.name);
        if !st2.unstaged.is_empty() {
            outln!(ctx, "Changes not staged for commit:");
            for c in &st2.unstaged {
                outln!(ctx, "\tmodified:   {}", h.rel(&c.path));
            }
            outln!(ctx, "\nno changes added to commit (use \"layr add\" and/or \"layr commit -a\")");
        } else if !st2.untracked.is_empty() {
            outln!(ctx, "nothing added to commit but untracked files present (use \"layr add\" to track)");
        } else {
            outln!(ctx, "nothing to commit, working tree clean");
        }
        return Ok(1);
    }
    if let Some(au) = a.get("--author") {
        if let Some((n, e)) = au.split_once('<') {
            state.author.name = n.trim().to_string();
            state.author.email = e.trim_end_matches('>').trim().to_string();
        }
    }
    // Content conflicts stay listed while the committed file has markers; the commit clears
    // the other kinds.
    let new_root = h.repo.p.state_path(&state.id);
    let mut cleared = Vec::new();
    for c in &head.conflicts {
        if (c.kind == "content" || c.kind == "add/add") && conflict_open(&new_root, c)? {
            state.conflicts.push(c.clone());
        } else {
            cleared.push(c.clone());
        }
    }
    let rec = h.repo.move_head(ctx, "commit", &line, &state.id, vec![state.clone()], None, serde_json::json!({"line": line.id}));
    rec?;
    h.repo.drop_stage(&line)?;
    if !a.has("-q") {
        let root = if parents_empty(&state) { " (root-commit)" } else { "" };
        outln!(ctx, "[{}{} {}] {}", line.name, root, ids::short(&state.id), message.lines().next().unwrap_or(""));
        let staged = changes::with_renames(&head_root, &new_root, staged);
        let fds = diff::file_diffs(&head_root, &new_root, &staged)?;
        ctx.print(&diff::shortstat(&fds));
        for f in &fds {
            match (&f.old, &f.new) {
                (None, Some(n)) => outln!(ctx, " create mode {:06o} {}", n.mode, n.path),
                (Some(o), None) => outln!(ctx, " delete mode {:06o} {}", o.mode, o.path),
                (Some(o), Some(n)) if o.path != n.path => outln!(ctx, " rename {} => {} ({}%)", o.path, n.path, f.similarity()),
                _ => {}
            }
        }
        for c in &cleared {
            outln!(ctx, " resolved conflict: {} ({})", c.path, c.kind);
        }
        if !state.conflicts.is_empty() {
            outln!(ctx, "warning: {} file(s) still have conflict markers; see 'layr status'", state.conflicts.len());
        }
    }
    Ok(0)
}

fn parents_empty(s: &crate::model::StateRec) -> bool {
    s.parents.is_empty()
}

/// Make `target` match `source` for paths matching `specs`, without touching other paths.
/// `keep_untracked`: paths absent from `known` (the previous index) are left alone.
#[allow(clippy::too_many_arguments)]
fn restore_paths(
    repo: &Repo,
    line: &Line,
    source: &Path,
    target_view: &Path,
    target: &Path,
    specs: &[String],
    known: Option<&Path>,
) -> Result<Vec<String>> {
    let own = repo.own(line);
    let mut done = Vec::new();
    for c in changes::without_moves(changes::file_changes(target_view, source)?) {
        if skip_path(&c.path) || !matches(specs, &c.path) {
            continue;
        }
        if c.new.is_none() {
            if let Some(k) = known {
                if fsutil::lmeta(k, &c.path).is_none() {
                    continue;
                }
            }
            fsutil::remove_entry(target, &c.path)?;
        } else {
            fsutil::copy_entry(source, &c.path, target, &c.path, own)?;
        }
        done.push(c.path.clone());
    }
    Ok(done)
}

pub fn restore(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .flag("-S|--staged")
        .flag("-W|--worktree")
        .value("-s|--source")
        .flag("--ours")
        .flag("--theirs")
        .flag("-q|--quiet")
        .flag("--overlay")
        .flag("--no-overlay");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let specs = h.specs(&a.rest())?;
    if specs.is_empty() {
        return Err(exit(128, "fatal: you must specify path(s) to restore"));
    }
    restore_impl(
        ctx,
        &h,
        &line,
        &specs,
        a.has("-S"),
        a.has("-W") || !a.has("-S"),
        a.get("-s"),
        a.has("--ours"),
        a.has("--theirs"),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn restore_impl(
    ctx: &Ctx,
    h: &Here,
    line: &Line,
    specs: &[String],
    staged: bool,
    worktree: bool,
    source: Option<&str>,
    ours: bool,
    theirs: bool,
) -> Result<i32> {
    let (_lock, w) = begin_change(ctx, &h.repo, line)?;
    let line = h.repo.line(&line.id)?;
    let repo = &h.repo;
    if ours || theirs {
        let head = repo.state(&line.head)?;
        let wdir = repo.working(&line);
        let mut n = 0;
        for c in &head.conflicts {
            if !matches(specs, &c.path) {
                continue;
            }
            let sid = if ours { c.ours.clone() } else { c.theirs.clone() };
            let sid = match sid {
                Some(s) => s,
                None => continue,
            };
            let root = repo.state_root(&sid)?;
            if fsutil::lmeta(&root, &c.path).is_some() {
                fsutil::copy_entry(&root, &c.path, &wdir, &c.path, repo.own(&line))?;
            } else {
                fsutil::remove_entry(&wdir, &c.path)?;
            }
            n += 1;
        }
        if n == 0 {
            return Err(exit(1, "error: no conflict matches the given paths"));
        }
        record_working(ctx, repo, &line, "restore", w)?;
        return Ok(0);
    }
    let (index, _it) = repo.index_view(&line)?;
    let head_root = repo.state_root(&line.head)?;
    let src_id = match source {
        Some(r) => Some(revs::resolve(repo, Some(&line), r)?),
        None => None,
    };
    let src_root = match &src_id {
        Some(id) => repo.state_root(id)?,
        None => {
            if staged {
                head_root.clone()
            } else {
                index.clone()
            }
        }
    };
    let mut matched = false;
    if staged {
        let stage = repo.ensure_stage(&line)?;
        let view = repo.temp_snapshot(&stage, true)?;
        let d = restore_paths(repo, &line, &src_root, &view.path, &stage, specs, None)?;
        matched |= !d.is_empty();
    }
    if worktree {
        let scan = repo.scan(&line)?;
        let wdir = repo.working(&line);
        let d = restore_paths(repo, &line, &src_root, &scan.path, &wdir, specs, Some(&index))?;
        matched |= !d.is_empty();
    }
    if !matched {
        let exists =
            specs.iter().any(|s| fsutil::lmeta(&src_root, s).is_some() || fsutil::lmeta(&repo.working(&line), s).is_some());
        if !exists {
            return Err(exit(1, format!("error: pathspec '{}' did not match any file(s) known to layr", h.rel(&specs[0]))));
        }
    }
    if worktree {
        record_working(ctx, repo, &line, "restore", w)?;
    }
    Ok(0)
}

/// Record a change of the working content only (for undo).
pub fn record_working(ctx: &Ctx, repo: &Repo, line: &Line, op: &str, working_before: Option<String>) -> Result<()> {
    let mut rec = Record::new(op);
    let working_after = repo.after_state(ctx, line, op, &mut rec)?;
    rec.lines.insert(
        line.id.clone(),
        crate::model::LineChange {
            before: Some(repo.line_state(line, &line.head)),
            after: Some(repo.line_state(line, &line.head)),
            working_before,
            working_after,
        },
    );
    repo.commit_record(ctx, rec)?;
    Ok(())
}

pub fn reset(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("--soft").flag("--mixed").flag("--hard").flag("--keep").flag("--merge").flag("-q|--quiet");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let line = h.line()?.clone();
    // `reset [<rev>] -- <paths>` unstages.
    let mut pos = a.pos.clone();
    let mut paths = a.paths.clone();
    if !a.dashdash && pos.len() > 1 {
        paths = pos.split_off(1);
    } else if !a.dashdash && pos.len() == 1 && revs::resolve(&h.repo, Some(&line), &pos[0]).is_err() {
        paths = pos.clone();
        pos.clear();
    }
    if !paths.is_empty() {
        let specs = h.specs(&paths)?;
        let src = pos.first().map(|s| s.as_str()).unwrap_or("HEAD");
        return restore_impl(ctx, &h, &line, &specs, true, false, Some(src), false, false);
    }
    let target = revs::resolve(&h.repo, Some(&line), pos.first().map(|s| s.as_str()).unwrap_or("HEAD"))?;
    let (_lock, w) = begin_change(ctx, &h.repo, &line)?;
    let line = h.repo.line(&line.id)?;
    let repo = &h.repo;
    repo.check_move(ctx, &line, &target)?;
    let target_root = repo.state_root(&target)?;
    let mode = if a.has("--soft") {
        "soft"
    } else if a.has("--hard") {
        "hard"
    } else if a.has("--keep") || a.has("--merge") {
        "keep"
    } else {
        "mixed"
    };
    let (index, _it) = repo.index_view(&line)?;
    match mode {
        "soft" => {
            // Keep the index content, now based on the new head.
            let keep = repo.temp_snapshot(&index, true)?;
            repo.drop_stage(&line)?;
            let mut l2 = line.clone();
            l2.head = target.clone();
            let stage = repo.p.stage_path(&line.id);
            crate::btrfs::snapshot(&keep.path, &stage, false)?;
            fsutil::atomic_write(&repo.p.meta_dir().join("stage").join(format!("{}.base", line.id)), target.as_bytes(), 0o600)?;
        }
        "mixed" => repo.drop_stage(&line)?,
        "hard" => {
            repo.drop_stage(&line)?;
            let scan = repo.scan(&line)?;
            let wdir = repo.working(&line);
            // Revert every tracked path; untracked and ignored files stay.
            let own = repo.own(&line);
            for c in changes::without_moves(changes::file_changes(&scan.path, &target_root)?) {
                if skip_path(&c.path) {
                    continue;
                }
                if c.new.is_none() {
                    if fsutil::lmeta(&index, &c.path).is_none() {
                        continue;
                    }
                    fsutil::remove_entry(&wdir, &c.path)?;
                } else {
                    fsutil::copy_entry(&target_root, &c.path, &wdir, &c.path, own)?;
                }
            }
        }
        _ => {
            let (st, _scan, _it2, _idx2) = repo.status(&line)?;
            let wdir = repo.working(&line);
            repo.apply_delta(&repo.state_root(&line.head)?, &target_root, &wdir, repo.own(&line), &st.local_paths(), false)?;
            repo.drop_stage(&line)?;
        }
    }
    repo.move_head(ctx, "reset", &line, &target, vec![], w, serde_json::json!({"mode": mode}))?;
    if mode == "hard" && !a.has("-q") {
        let s = repo.state(&target)?;
        outln!(ctx, "HEAD is now at {} {}", ids::short(&target), s.message.lines().next().unwrap_or(""));
    }
    Ok(0)
}

pub fn stash(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("push");
    let (sub, rest): (&str, Vec<String>) = match sub {
        "push" | "save" | "list" | "show" | "pop" | "apply" | "drop" | "clear" => (sub, args.get(1..).unwrap_or(&[]).to_vec()),
        _ => ("push", args.to_vec()),
    };
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let repo = &h.repo;
    match sub {
        "list" => {
            for (i, s) in repo.p.db.line_states(&line.id, StateKind::Stash)?.iter().enumerate() {
                outln!(ctx, "stash@{{{i}}}: {}", s.message);
            }
            Ok(0)
        }
        "show" => {
            let spec = Spec::new().flag("-p|--patch").flag("--stat").flag("-u|--include-untracked");
            let a = parse(&spec, &rest)?;
            let rev = a.pos.first().cloned().unwrap_or_else(|| "stash@{0}".into());
            let id = revs::resolve(repo, Some(&line), &rev)?;
            let s = repo.state(&id)?;
            let parent = repo.state_root(&s.parents[0])?;
            let root = repo.state_root(&id)?;
            let list = changes::file_changes(&parent, &root)?.into_iter().filter(|c| !skip_path(&c.path)).collect::<Vec<_>>();
            let fds = diff::file_diffs(&parent, &root, &list)?;
            if a.has("-p") {
                for f in &fds {
                    ctx.print(&diff::patch(f, 3));
                }
            } else {
                ctx.print(&diff::stat(&fds, 80));
            }
            Ok(0)
        }
        "drop" | "clear" => {
            repo.authorize_line(ctx, &line, crate::repo::LineOp::Direct)?;
            let _lock = repo.p.lock()?;
            let list = repo.p.db.line_states(&line.id, StateKind::Stash)?;
            let targets: Vec<(usize, String)> = if sub == "clear" {
                list.iter().enumerate().map(|(i, s)| (i, s.id.clone())).collect()
            } else {
                let rev = rest.first().cloned().unwrap_or_else(|| "stash@{0}".into());
                let id = revs::resolve(repo, Some(&line), &rev)?;
                let i = list.iter().position(|s| s.id == id).unwrap_or(0);
                vec![(i, id)]
            };
            for (i, id) in targets {
                let mut rec = Record::new("stash.drop");
                rec.data = serde_json::json!({"state": id, "line": line.id});
                repo.commit_record(ctx, rec)?;
                if sub == "drop" {
                    outln!(ctx, "Dropped stash@{{{i}}} ({})", ids::short(&id));
                }
            }
            Ok(0)
        }
        "pop" | "apply" => {
            let rev = rest.iter().find(|r| !r.starts_with('-')).cloned().unwrap_or_else(|| "stash@{0}".into());
            let id = revs::resolve(repo, Some(&line), &rev)?;
            let s = repo.state(&id)?;
            let (_lock, w) = begin_change(ctx, repo, &line)?;
            let line = repo.line(&line.id)?;
            let scan = repo.scan(&line)?;
            let base = repo.state_root(&s.parents[0])?;
            let theirs = repo.state_root(&id)?;
            let work = repo.temp_work(&scan.path)?;
            let cfg = |k: &str| repo.p.config_value(k).ok().flatten().and_then(|v| v.as_str().map(|s| s.to_string()));
            let out = crate::merge::merge(&crate::merge::Input {
                base: &base,
                ours: &scan.path,
                theirs: &theirs,
                result: &work.path,
                base_id: &s.parents[0],
                ours_id: &line.head,
                theirs_id: &id,
                ours_label: "Updated upstream",
                theirs_label: "Stashed changes",
                config: &cfg,
                run_as: ctx.caller.driver_ids(),
                allow_root: ctx.caller.is_root(),
            })?;
            let result = repo.temp_snapshot(&work.path, true)?;
            let wdir = repo.working(&line);
            let own = repo.own(&line);
            for c in changes::file_changes(&scan.path, &result.path)? {
                if skip_path(&c.path) {
                    continue;
                }
                if let Some(src) = &c.old_path {
                    fsutil::remove_entry(&wdir, src)?;
                }
                if c.new.is_none() {
                    fsutil::remove_entry(&wdir, &c.path)?;
                } else {
                    fsutil::copy_entry(&result.path, &c.path, &wdir, &c.path, own)?;
                }
            }
            for m in &out.messages {
                outln!(ctx, "{m}");
            }
            record_working(ctx, repo, &line, &format!("stash.{sub}"), w)?;
            if !out.conflicts.is_empty() {
                outln!(ctx, "The stash entry is kept in case you need it again.");
                return Ok(1);
            }
            if sub == "pop" {
                let mut rec = Record::new("stash.drop");
                rec.data = serde_json::json!({"state": id, "line": line.id});
                repo.commit_record(ctx, rec)?;
                outln!(ctx, "Dropped {rev} ({})", ids::short(&id));
            }
            Ok(0)
        }
        _ => {
            let spec = Spec::new()
                .value("-m|--message")
                .flag("-u|--include-untracked")
                .flag("-a|--all")
                .flag("-q|--quiet")
                .flag("-k|--keep-index");
            let a = parse(&spec, &rest)?;
            let (_lock, w) = begin_change(ctx, repo, &line)?;
            let line = repo.line(&line.id)?;
            let (st, scan, _it, index) = repo.status(&line)?;
            let untracked = a.has("-u") || a.has("-a");
            let ignored = a.has("-a");
            if st.clean() && (!untracked || st.untracked.is_empty()) && (!ignored || st.ignored.is_empty()) {
                outln!(ctx, "No local changes to save");
                return Ok(0);
            }
            let head = repo.state(&line.head)?;
            let head_root = repo.state_root(&line.head)?;
            let work = repo.temp_work(&head_root)?;
            let own = repo.own(&line);
            let mut paths: BTreeSet<String> = BTreeSet::new();
            for c in st.staged.iter().chain(st.unstaged.iter()) {
                paths.insert(c.path.clone());
                if let Some(o) = &c.old_path {
                    paths.insert(o.clone());
                }
            }
            if untracked {
                paths.extend(st.untracked.iter().cloned());
            }
            if ignored {
                paths.extend(st.ignored.iter().cloned());
            }
            for p in &paths {
                if fsutil::lmeta(&scan.path, p).is_some() {
                    fsutil::copy_entry(&scan.path, p, &work.path, p, own)?;
                } else {
                    fsutil::remove_entry(&work.path, p)?;
                }
            }
            let msg = match a.get("-m") {
                Some(m) => format!("On {}: {m}", line.name),
                None => format!("WIP on {}: {} {}", line.name, ids::short(&line.head), head.message.lines().next().unwrap_or("")),
            };
            let state = repo.new_state(ctx, &work.path, StateKind::Stash, vec![line.head.clone()], &msg, Some(&line))?;
            // Back to the head content, or with --keep-index to the index content (the index
            // stays). New files that went into the stash leave the working folder.
            let keep_index = a.has("-k");
            let target: &Path = if keep_index { &index } else { &head_root };
            let wdir = repo.working(&line);
            let wtree = crate::tree::Tree::open(&wdir)?;
            for p in &paths {
                if fsutil::lmeta(target, p).is_some() {
                    fsutil::copy_entry(target, p, &wdir, p, own)?;
                } else if fsutil::lmeta(&index, p).is_some() || fsutil::lmeta(&head_root, p).is_some() || untracked {
                    fsutil::remove_entry(&wdir, p)?;
                    prune_empty_in(&wtree, p);
                }
            }
            if !keep_index {
                repo.drop_stage(&line)?;
            }
            let mut rec = Record::new("stash");
            rec.states.push(state.clone());
            let working_after = repo.after_state(ctx, &line, "stash", &mut rec)?;
            rec.lines.insert(
                line.id.clone(),
                crate::model::LineChange {
                    before: Some(repo.line_state(&line, &line.head)),
                    after: Some(repo.line_state(&line, &line.head)),
                    working_before: w,
                    working_after,
                },
            );
            repo.commit_record(ctx, rec)?;
            if !a.has("-q") {
                outln!(ctx, "Saved working directory and index state {msg}");
            }
            Ok(0)
        }
    }
}

pub fn clean(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .flag("-n|--dry-run")
        .flag("-f|--force")
        .flag("-d")
        .flag("-x")
        .flag("-X")
        .flag("-q|--quiet")
        .flag("-i|--interactive");
    let a = parse(&spec, args)?;
    if !a.has("-n") && !a.has("-f") {
        return Err(exit(128, "fatal: clean requires -f (force) or -n (dry run)"));
    }
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let specs = h.specs(&a.rest())?;
    let (_lock, w) = if a.has("-n") { (h.repo.p.lock()?, None) } else { begin_change(ctx, &h.repo, &line)? };
    let line = h.repo.line(&line.id)?;
    let (st, scan, _it, index) = h.repo.status(&line)?;
    let targets = clean_targets(&st, &scan.path, &index, a.has("-d"), a.has("-x"), a.has("-X"));
    let mut targets: Vec<String> = targets.into_iter().filter(|t| matches(&specs, t.trim_end_matches('/'))).collect();
    targets.sort();
    let wdir = h.repo.working(&line);
    for t in &targets {
        if a.has("-n") {
            outln!(ctx, "Would remove {}", h.rel(t));
        } else {
            fsutil::remove_entry(&wdir, t.trim_end_matches('/'))?;
            if !a.has("-q") {
                outln!(ctx, "Removing {}", h.rel(t));
            }
        }
    }
    if !a.has("-n") && !targets.is_empty() {
        record_working(ctx, &h.repo, &line, "clean", w)?;
    }
    Ok(0)
}

/// What `clean` removes, as git does. A folder goes whole only when nothing in it is in the
/// index and everything in it is of the kinds being removed: with `-X` an ignored folder, with
/// `-x` any new folder, without them a new folder that holds no ignored files. Otherwise the
/// files go one by one. Without `-d`, files in new folders stay.
fn clean_targets(st: &Status, scan: &Path, index: &Path, dirs: bool, with_ignored: bool, only_ignored: bool) -> BTreeSet<String> {
    let rules = crate::ignore_rules::Rules::new(scan);
    let in_index = |d: &str| fsutil::lmeta(index, d).is_some();
    let folders = |p: &str| -> Vec<String> {
        let comps: Vec<&str> = p.split('/').collect();
        (1..comps.len()).map(|n| comps[..n].join("/")).collect()
    };
    // The topmost folder of `p` that is not in the index.
    let new_folder = |p: &str| folders(p).into_iter().find(|d| !in_index(d));
    let has_ignored = |d: &str| st.ignored.iter().any(|i| i.starts_with(&format!("{d}/")));
    let mut out = BTreeSet::new();
    if !only_ignored {
        for f in &st.untracked {
            match new_folder(f) {
                None => {
                    out.insert(f.clone());
                }
                Some(d) if dirs => {
                    out.insert(if with_ignored || !has_ignored(&d) { format!("{d}/") } else { f.clone() });
                }
                Some(_) => {}
            }
        }
    }
    if with_ignored || only_ignored {
        for i in &st.ignored {
            match new_folder(i) {
                None => {
                    out.insert(i.clone());
                }
                Some(d) if dirs => {
                    let whole = if only_ignored {
                        folders(i).into_iter().find(|f| !in_index(f) && rules.ignored(f, true))
                    } else {
                        Some(d)
                    };
                    out.insert(whole.map(|f| format!("{f}/")).unwrap_or_else(|| i.clone()));
                }
                Some(_) => {}
            }
        }
    }
    // A path inside a folder that goes whole is not listed again.
    let all: Vec<String> = out.iter().cloned().collect();
    out.retain(|p| !all.iter().any(|d| d.ends_with('/') && d != p && p.starts_with(d.as_str())));
    out
}
