//! Reading commands: diff, show, log, rev-parse, merge-base, blame, grep, ls-files.

use super::{exit, here, matches, Here};
use crate::args::{parse, Spec};
use crate::changes::{self, Change};
use crate::ctx::Ctx;
use crate::diff::{self, FileDiff};
use crate::fsutil;
use crate::ids;
use crate::model::{Line, StateKind, StateRec};
use crate::repo::{skip_path, Repo, Temp};
use crate::revs;
use anyhow::{bail, Result};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

fn diff_spec() -> Spec {
    Spec::new()
        .flag("--cached|--staged")
        .flag("--stat")
        .flag("--numstat")
        .flag("--shortstat")
        .flag("--name-only")
        .flag("--name-status")
        .flag("--check")
        .flag("--quiet")
        .flag("--exit-code")
        .flag("--no-color")
        .optional("--color")
        .flag("-p|-u|--patch")
        .flag("--raw")
        .optional("-M|--find-renames")
        .flag("--no-renames")
        .flag("-R")
        .flag("-a|--text")
        .flag("--minimal")
        .flag("--patience")
        .flag("--histogram")
        .flag("--no-ext-diff")
        .flag("--summary")
        .flag("-w|--ignore-all-space")
        .flag("-b|--ignore-space-change")
        .flag("--full-index")
        .value("-U|--unified")
        .optional("--diff-filter")
        .value("--stat-width")
        .optional("--relative")
}

/// An empty folder to diff against a root state.
fn empty_root() -> Result<Temp> {
    let p = std::env::temp_dir().join(format!("layr-empty-{}", ids::new_id()));
    std::fs::create_dir(&p)?;
    Ok(Temp { path: p })
}

/// All files of a root as additions.
fn all_files(root: &Path) -> Result<Vec<Change>> {
    let mut v = Vec::new();
    fsutil::walk(root, "", &mut |rel, m| {
        if skip_path(rel) {
            return Ok(false);
        }
        if !m.is_dir() {
            v.push(Change { path: rel.to_string(), old_path: None, old: None, new: Some(m.clone()), ranges: None });
        }
        Ok(true)
    })?;
    Ok(v)
}

pub struct DiffOpts {
    pub stat: bool,
    pub numstat: bool,
    pub shortstat: bool,
    pub name_only: bool,
    pub name_status: bool,
    pub check: bool,
    pub patch: bool,
    pub context: usize,
    pub stat_width: usize,
    pub filter: Option<String>,
}

impl DiffOpts {
    fn from(a: &crate::args::Parsed, default_patch: bool) -> Result<DiffOpts> {
        let stat = a.has("--stat");
        let numstat = a.has("--numstat");
        let shortstat = a.has("--shortstat");
        let name_only = a.has("--name-only");
        let name_status = a.has("--name-status");
        let check = a.has("--check");
        let any_summary = stat || numstat || shortstat || name_only || name_status || check;
        Ok(DiffOpts {
            stat,
            numstat,
            shortstat,
            name_only,
            name_status,
            check,
            patch: a.has("-p") || (!any_summary && default_patch),
            context: a.get("-U").map(|v| v.parse()).transpose()?.unwrap_or(3),
            stat_width: a.get("--stat-width").map(|v| v.parse()).transpose()?.unwrap_or(80),
            filter: a.get("--diff-filter").map(|s| s.to_string()),
        })
    }
}

pub fn print_diffs(ctx: &Ctx, fds: &[FileDiff], o: &DiffOpts) -> bool {
    let fds: Vec<&FileDiff> = match &o.filter {
        Some(f) => fds.iter().filter(|d| f.contains(d.status())).collect(),
        None => fds.iter().collect(),
    };
    let owned: Vec<FileDiff> = fds.iter().map(|f| FileDiff { old: f.old.clone(), new: f.new.clone() }).collect();
    let mut problems = false;
    if o.check {
        let s = diff::check(&owned);
        problems = !s.is_empty();
        ctx.print(&s);
        return problems;
    }
    if o.numstat {
        ctx.print(&diff::numstat(&owned));
    }
    if o.stat {
        ctx.print(&diff::stat(&owned, o.stat_width));
    }
    if o.shortstat {
        ctx.print(&diff::shortstat(&owned));
    }
    if o.name_only {
        ctx.print(&diff::name_only(&owned));
    }
    if o.name_status {
        ctx.print(&diff::name_status(&owned));
    }
    if o.patch {
        if o.stat || o.numstat || o.shortstat {
            outln!(ctx);
        }
        for f in &owned {
            ctx.print(&diff::patch(f, o.context));
        }
    }
    problems
}

/// Split diff arguments into revisions and paths, as git does without `--`.
fn split_revs(repo: &Repo, line: Option<&Line>, a: &crate::args::Parsed) -> (Vec<String>, Vec<String>) {
    let mut revs = Vec::new();
    let mut paths = a.paths.clone();
    let mut in_paths = false;
    for p in &a.pos {
        if !in_paths && is_rev(repo, line, p) {
            revs.push(p.clone());
        } else {
            in_paths = true;
            paths.push(p.clone());
        }
    }
    (revs, paths)
}

fn is_rev(repo: &Repo, line: Option<&Line>, s: &str) -> bool {
    if let Some((a, b)) = s.split_once("...") {
        return (a.is_empty() || revs::resolve(repo, line, a).is_ok()) && (b.is_empty() || revs::resolve(repo, line, b).is_ok());
    }
    if let Some((a, b)) = s.split_once("..") {
        return (a.is_empty() || revs::resolve(repo, line, a).is_ok()) && (b.is_empty() || revs::resolve(repo, line, b).is_ok());
    }
    revs::resolve(repo, line, s).is_ok()
}

pub fn diff(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&diff_spec(), args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = h.line.clone();
    let (revs_v, paths) = split_revs(repo, line.as_ref(), &a);
    let specs = h.specs(&paths)?;
    let cached = a.has("--cached");
    let opts = DiffOpts::from(&a, true)?;
    let mut keep: Vec<Temp> = Vec::new();
    let resolve = |r: &str| -> Result<PathBuf> {
        let id = revs::resolve(repo, line.as_ref(), if r.is_empty() { "HEAD" } else { r })?;
        repo.state_root(&id)
    };
    // (old root, new root, new side is the working content, index for tracked check)
    let mut index_root: Option<PathBuf> = None;
    let (old, new, working): (PathBuf, PathBuf, bool) = match revs_v.len() {
        0 => {
            let l = h.line()?;
            let (idx, it) = repo.index_view(l)?;
            if let Some(t) = it {
                keep.push(t);
            }
            if cached {
                (repo.state_root(&l.head)?, idx, false)
            } else {
                let scan = repo.scan(l)?;
                let p = scan.path.clone();
                keep.push(scan);
                index_root = Some(idx.clone());
                (idx, p, true)
            }
        }
        1 => {
            let r = &revs_v[0];
            if let Some((x, y)) = r.split_once("...") {
                let xi = revs::resolve(repo, line.as_ref(), if x.is_empty() { "HEAD" } else { x })?;
                let yi = revs::resolve(repo, line.as_ref(), if y.is_empty() { "HEAD" } else { y })?;
                let b = repo.merge_base(&xi, &yi)?.ok_or_else(|| anyhow::anyhow!("no merge base"))?;
                (repo.state_root(&b)?, repo.state_root(&yi)?, false)
            } else if let Some((x, y)) = r.split_once("..") {
                (resolve(x)?, resolve(y)?, false)
            } else {
                let l = h.line()?;
                let old = resolve(r)?;
                let (idx, it) = repo.index_view(l)?;
                if let Some(t) = it {
                    keep.push(t);
                }
                if cached {
                    (old, idx, false)
                } else {
                    let scan = repo.scan(l)?;
                    let p = scan.path.clone();
                    keep.push(scan);
                    index_root = Some(idx);
                    (old, p, true)
                }
            }
        }
        _ => (resolve(&revs_v[0])?, resolve(&revs_v[1])?, false),
    };
    let (old, new) = if a.has("-R") { (new, old) } else { (old, new) };
    let mut list: Vec<Change> = changes::file_changes(&old, &new)?
        .into_iter()
        .filter(|c| {
            !skip_path(&c.path)
                && (matches(&specs, &c.path) || c.old_path.as_deref().map(|o| matches(&specs, o)).unwrap_or(false))
        })
        .collect();
    if working {
        // Untracked files are not part of a working tree diff.
        let idx = index_root.clone().unwrap();
        list = changes::without_moves(list);
        list.retain(|c| !(c.old.is_none() && fsutil::lmeta(&idx, &c.path).is_none()));
    }
    let list = if a.has("--no-renames") { list } else { changes::with_renames(&old, &new, list) };
    let fds = diff::file_diffs(&old, &new, &list)?;
    let quiet = a.has("--quiet");
    let problems = if quiet { false } else { print_diffs(ctx, &fds, &opts) };
    drop(keep);
    if opts.check && problems {
        return Ok(2);
    }
    if (a.has("--exit-code") || quiet) && !fds.is_empty() {
        return Ok(1);
    }
    Ok(0)
}

// ----- formatting of states -----

pub fn fmt_date(ms: i64, style: &str) -> String {
    let dt = chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default();
    match style {
        "iso" | "iso8601" => dt.format("%Y-%m-%d %H:%M:%S +0000").to_string(),
        "iso-strict" | "iso8601-strict" => dt.format("%Y-%m-%dT%H:%M:%S+00:00").to_string(),
        "short" => dt.format("%Y-%m-%d").to_string(),
        "unix" => (ms / 1000).to_string(),
        "relative" => relative(ms),
        "rfc" | "rfc2822" => dt.format("%a, %-d %b %Y %H:%M:%S +0000").to_string(),
        _ => dt.format("%a %b %-d %H:%M:%S %Y +0000").to_string(),
    }
}

fn relative(ms: i64) -> String {
    let d = (ids::now_ms() - ms) / 1000;
    let (n, u) = if d < 90 {
        (d, "second")
    } else if d < 90 * 60 {
        (d / 60, "minute")
    } else if d < 36 * 3600 {
        (d / 3600, "hour")
    } else if d < 14 * 86400 {
        (d / 86400, "day")
    } else if d < 70 * 86400 {
        (d / (7 * 86400), "week")
    } else if d < 365 * 86400 {
        (d / (30 * 86400), "month")
    } else {
        (d / (365 * 86400), "year")
    };
    format!("{n} {u}{} ago", if n == 1 { "" } else { "s" })
}

pub struct Decor {
    map: HashMap<String, Vec<String>>,
}

impl Decor {
    pub fn load(repo: &Repo, current: Option<&Line>) -> Result<Decor> {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for l in repo.lines()? {
            let name =
                if current.map(|c| c.id == l.id).unwrap_or(false) { format!("HEAD -> {}", l.name) } else { l.name.clone() };
            map.entry(l.head.clone()).or_default().push(name);
        }
        for (t, s, _) in repo.p.db.tags()? {
            map.entry(s).or_default().push(format!("tag: {t}"));
        }
        for (r, s, _) in repo.p.db.remote_refs()? {
            map.entry(s).or_default().push(r);
        }
        for v in map.values_mut() {
            v.sort_by_key(|x| (!x.starts_with("HEAD"), x.starts_with("tag:"), x.clone()));
        }
        Ok(Decor { map })
    }
    pub fn of(&self, id: &str) -> String {
        match self.map.get(id) {
            Some(v) if !v.is_empty() => format!(" ({})", v.join(", ")),
            _ => String::new(),
        }
    }
    pub fn raw(&self, id: &str) -> String {
        self.map.get(id).map(|v| v.join(", ")).unwrap_or_default()
    }
}

fn subject(s: &StateRec) -> String {
    let m = if s.message.is_empty() && s.kind == StateKind::Auto { "(automatic state)" } else { s.message.as_str() };
    m.lines().next().unwrap_or("").to_string()
}

fn body(s: &StateRec) -> String {
    let mut it = s.message.splitn(2, '\n');
    it.next();
    it.next().unwrap_or("").trim_start_matches('\n').to_string()
}

fn unescape_format(f: &str) -> String {
    f.to_string()
}

/// Expand a `--format` string for one state.
pub fn format_state(f: &str, s: &StateRec, decor: &Decor, date_style: &str) -> String {
    let f = unescape_format(f);
    let mut out = String::new();
    let b = f.as_bytes();
    let mut i = 0;
    let date = |st: &str| fmt_date(s.time, st);
    while i < b.len() {
        if b[i] != b'%' || i + 1 >= b.len() {
            out.push(b[i] as char);
            i += 1;
            continue;
        }
        let c = b[i + 1] as char;
        i += 2;
        let two = if i < b.len() { Some(b[i] as char) } else { None };
        let take2 = |o: &mut String, v: String| {
            o.push_str(&v);
        };
        match c {
            'H' => out.push_str(&ids::display(&s.id)),
            'h' => out.push_str(&ids::short(&s.id)),
            'T' => out.push_str(&ids::display(&s.id)),
            't' => out.push_str(&ids::short(&s.id)),
            'P' => out.push_str(&s.parents.iter().map(|p| ids::display(p)).collect::<Vec<_>>().join(" ")),
            'p' => out.push_str(&s.parents.iter().map(|p| ids::short(p)).collect::<Vec<_>>().join(" ")),
            's' => out.push_str(&subject(s)),
            'b' => {
                let bd = body(s);
                if !bd.is_empty() {
                    out.push_str(&bd);
                    out.push('\n');
                }
            }
            'B' => {
                out.push_str(&s.message);
                out.push('\n');
            }
            'n' => out.push('\n'),
            '%' => out.push('%'),
            'd' => out.push_str(&decor.of(&s.id)),
            'D' => out.push_str(&decor.raw(&s.id)),
            'x' => {
                if i + 1 < b.len() + 1 && i + 2 <= b.len() {
                    if let Ok(v) = u8::from_str_radix(&f[i..i + 2], 16) {
                        out.push(v as char);
                        i += 2;
                    }
                }
            }
            'a' | 'c' => {
                let k = two.unwrap_or(' ');
                i += 1;
                match k {
                    'n' => take2(&mut out, s.author.name.clone()),
                    'N' => take2(&mut out, s.author.name.clone()),
                    'e' | 'E' => take2(&mut out, s.author.email.clone()),
                    'l' => take2(&mut out, s.author.email.split('@').next().unwrap_or("").to_string()),
                    'd' => take2(&mut out, date(date_style)),
                    'D' => take2(&mut out, date("rfc")),
                    'r' => take2(&mut out, date("relative")),
                    't' => take2(&mut out, date("unix")),
                    'i' => take2(&mut out, date("iso")),
                    'I' => take2(&mut out, date("iso-strict")),
                    's' => take2(&mut out, date("short")),
                    _ => {
                        out.push('%');
                        out.push(c);
                        i -= 1;
                    }
                }
            }
            _ => {
                out.push('%');
                out.push(c);
            }
        }
    }
    out
}

fn medium(s: &StateRec, decor: &Decor, date_style: &str, kind_note: bool) -> String {
    let mut o = format!("commit {}{}\n", ids::display(&s.id), decor.of(&s.id));
    if s.parents.len() > 1 {
        o.push_str(&format!("Merge: {}\n", s.parents.iter().map(|p| ids::short(p)).collect::<Vec<_>>().join(" ")));
    }
    o.push_str(&format!("Author: {} <{}>\n", s.author.name, s.author.email));
    o.push_str(&format!("Date:   {}\n", fmt_date(s.time, date_style)));
    if kind_note && s.kind != StateKind::Commit {
        let k = match s.kind {
            StateKind::Auto => {
                if s.running {
                    "automatic state (taken while processes ran)"
                } else {
                    "automatic state"
                }
            }
            StateKind::Stash => "stash",
            StateKind::Import => "imported from git",
            StateKind::Commit => "",
        };
        o.push_str(&format!("Kind:   {k}\n"));
    }
    o.push('\n');
    let msg = if s.message.is_empty() { "(no message)".to_string() } else { s.message.clone() };
    for l in msg.lines() {
        if l.is_empty() {
            o.push('\n');
        } else {
            o.push_str(&format!("    {l}\n"));
        }
    }
    if !s.conflicts.is_empty() {
        o.push_str("\n    Conflicts:\n");
        for c in &s.conflicts {
            o.push_str(&format!("    \t{} ({})\n", c.path, c.kind));
        }
    }
    o
}

/// Changes of a state against its first parent (all files for a root state).
fn state_changes(repo: &Repo, s: &StateRec) -> Result<(PathBuf, Option<Temp>, PathBuf, Vec<Change>)> {
    let root = repo.state_root(&s.id)?;
    match s.parents.first() {
        Some(p) => {
            let pr = repo.state_root(p)?;
            let v = changes::file_changes(&pr, &root)?.into_iter().filter(|c| !skip_path(&c.path)).collect();
            let v = changes::with_renames(&pr, &root, v);
            Ok((pr, None, root, v))
        }
        None => {
            let e = empty_root()?;
            let v = all_files(&root)?;
            Ok((e.path.clone(), Some(e), root, v))
        }
    }
}

fn pretty_format(a: &crate::args::Parsed) -> Option<String> {
    if a.has("--oneline") {
        return Some("oneline".into());
    }
    a.get("--format").or(a.get("--pretty")).map(|s| s.to_string())
}

/// Print one state in a log or show format.
fn print_state(ctx: &Ctx, s: &StateRec, fmt: Option<&str>, decor: &Decor, date_style: &str, first: bool) -> Result<bool> {
    match fmt {
        Some("oneline") => {
            outln!(ctx, "{}{} {}", ids::short(&s.id), decor.of(&s.id), subject(s));
            Ok(false)
        }
        Some("short") => {
            if !first {
                outln!(ctx);
            }
            out!(
                ctx,
                "commit {}{}\nAuthor: {} <{}>\n\n    {}\n",
                ids::display(&s.id),
                decor.of(&s.id),
                s.author.name,
                s.author.email,
                subject(s)
            );
            Ok(true)
        }
        Some("full") | Some("fuller") | Some("medium") | None => {
            if !first {
                outln!(ctx);
            }
            ctx.print(&medium(s, decor, date_style, true));
            Ok(true)
        }
        Some("raw") => {
            if !first {
                outln!(ctx);
            }
            ctx.print(&medium(s, decor, "unix", true));
            Ok(true)
        }
        Some(f) => {
            let (body, term) = if let Some(x) = f.strip_prefix("format:") {
                (x, false)
            } else if let Some(x) = f.strip_prefix("tformat:") {
                (x, true)
            } else {
                (f, true)
            };
            if !term && !first {
                outln!(ctx);
            }
            let text = format_state(body, s, decor, date_style);
            out!(ctx, "{}", text);
            if term && !text.is_empty() {
                outln!(ctx);
            }
            Ok(!text.is_empty())
        }
    }
}

fn show_spec() -> Spec {
    diff_spec()
        .optional("--format|--pretty")
        .flag("--oneline")
        .flag("-s|--no-patch")
        .value("--date")
        .flag("--decorate")
        .flag("--no-decorate")
        .flag("--abbrev-commit")
        .flag("--first-parent")
}

pub fn show(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&show_spec(), args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = h.line.clone();
    let mut revs_v: Vec<String> = a.pos.clone();
    if revs_v.is_empty() {
        revs_v.push("HEAD".into());
    }
    let specs = h.specs(&a.paths)?;
    let decor = Decor::load(repo, line.as_ref())?;
    let date_style = a.get("--date").unwrap_or("default").to_string();
    let fmt = pretty_format(&a);
    let opts = DiffOpts::from(&a, true)?;
    let mut first = true;
    for r in revs_v {
        // <rev>:<path> and :<path> (index).
        if let Some((rv, path)) = r.split_once(':') {
            let root_path: PathBuf;
            let mut _keep = None;
            if rv.is_empty() {
                let l = h.line()?;
                let (idx, t) = repo.index_view(l)?;
                _keep = t;
                root_path = idx;
            } else {
                let id = revs::resolve(repo, line.as_ref(), rv)?;
                root_path = repo.state_root(&id)?;
            }
            let p = if path.starts_with("./") || path.starts_with("../") {
                super::normalize(&h.loc.sub, path)?
            } else {
                path.trim_start_matches('/').to_string()
            };
            match fsutil::lmeta(&root_path, &p) {
                None => return Err(exit(128, format!("fatal: path '{p}' does not exist in '{rv}'"))),
                Some(m) if m.is_dir() => {
                    outln!(ctx, "tree {r}\n");
                    let d = fsutil::safe_join(&root_path, &p)?;
                    let mut names: Vec<(String, bool)> = std::fs::read_dir(d)?
                        .flatten()
                        .map(|e| {
                            (e.file_name().to_string_lossy().into_owned(), e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                        })
                        .filter(|(n, _)| !(p.is_empty() && n == ".git"))
                        .collect();
                    names.sort();
                    for (n, d) in names {
                        outln!(ctx, "{n}{}", if d { "/" } else { "" });
                    }
                }
                Some(_) => {
                    let data = fsutil::entry_bytes(&root_path, &p)?;
                    use std::io::Write;
                    let _ = ctx.out.borrow_mut().write_all(&data);
                }
            }
            first = false;
            continue;
        }
        let id = revs::resolve(repo, line.as_ref(), &r)?;
        let s = repo.state(&id)?;
        let header_ends_blank = print_state(ctx, &s, fmt.as_deref(), &decor, &date_style, first)?;
        first = false;
        if a.has("-s") {
            continue;
        }
        let (old, _t, new, list) = state_changes(repo, &s)?;
        let list: Vec<Change> = list.into_iter().filter(|c| matches(&specs, &c.path)).collect();
        if list.is_empty() {
            continue;
        }
        let fds = diff::file_diffs(&old, &new, &list)?;
        if header_ends_blank && (opts.patch || opts.stat || opts.name_only || opts.name_status || opts.numstat || opts.shortstat)
        {
            outln!(ctx);
        }
        print_diffs(ctx, &fds, &opts);
    }
    Ok(0)
}

fn log_spec() -> Spec {
    show_spec()
        .value("-n|--max-count")
        .flag("--all")
        .flag("--reverse")
        .flag("--graph")
        .value("--skip")
        .value("--since|--after")
        .value("--until|--before")
        .value("--author")
        .value("--grep")
        .flag("--no-merges")
        .flag("--merges")
        .flag("--auto")
        .flag("--follow")
        .flag("-i|--regexp-ignore-case")
}

fn parse_since(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis());
    }
    let parts: Vec<&str> = s.split([' ', '.']).filter(|x| !x.is_empty()).collect();
    if parts.len() >= 2 {
        let n: i64 = parts[0].parse().ok()?;
        let unit = parts[1].trim_end_matches('s');
        let secs = match unit {
            "second" => 1,
            "minute" => 60,
            "hour" => 3600,
            "day" => 86400,
            "week" => 7 * 86400,
            "month" => 30 * 86400,
            "year" => 365 * 86400,
            _ => return None,
        };
        return Some(ids::now_ms() - n * secs * 1000);
    }
    None
}

pub fn log(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&log_spec(), args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = h.line.clone();
    let (revs_v, paths) = split_revs(repo, line.as_ref(), &a);
    let specs = h.specs(&paths)?;
    let mut include: Vec<String> = Vec::new();
    let mut exclude: BTreeSet<String> = BTreeSet::new();
    let mut add_excl = |id: &str| -> Result<()> {
        for x in repo.ancestors(id)? {
            exclude.insert(x);
        }
        Ok(())
    };
    for r in &revs_v {
        if let Some((x, y)) = r.split_once("...") {
            let xi = revs::resolve(repo, line.as_ref(), if x.is_empty() { "HEAD" } else { x })?;
            let yi = revs::resolve(repo, line.as_ref(), if y.is_empty() { "HEAD" } else { y })?;
            include.push(xi.clone());
            include.push(yi.clone());
            if let Some(b) = repo.merge_base(&xi, &yi)? {
                add_excl(&b)?;
            }
        } else if let Some((x, y)) = r.split_once("..") {
            let xi = revs::resolve(repo, line.as_ref(), if x.is_empty() { "HEAD" } else { x })?;
            let yi = revs::resolve(repo, line.as_ref(), if y.is_empty() { "HEAD" } else { y })?;
            add_excl(&xi)?;
            include.push(yi);
        } else if let Some(x) = r.strip_prefix('^') {
            add_excl(&revs::resolve(repo, line.as_ref(), x)?)?;
        } else {
            include.push(revs::resolve(repo, line.as_ref(), r)?);
        }
    }
    if a.has("--all") {
        for l in repo.lines()? {
            include.push(l.head);
        }
        for (_, s, _) in repo.p.db.tags()? {
            include.push(s);
        }
        for (_, s, _) in repo.p.db.remote_refs()? {
            include.push(s);
        }
    }
    if include.is_empty() {
        include.push(h.line()?.head.clone());
    }
    if a.has("--auto") {
        if let Some(l) = &line {
            for s in repo.p.db.line_states(&l.id, StateKind::Auto)? {
                include.push(s.id);
            }
        }
    }
    // Walk in date order.
    let first_parent = a.has("--first-parent");
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut heap: std::collections::BinaryHeap<(i64, String)> = std::collections::BinaryHeap::new();
    for i in include {
        if !exclude.contains(&i) {
            if let Some(s) = repo.p.db.state(&i)? {
                heap.push((s.time, i));
            }
        }
    }
    let max: usize = a.get("-n").map(|v| v.parse()).transpose()?.unwrap_or(usize::MAX);
    let skip: usize = a.get("--skip").map(|v| v.parse()).transpose()?.unwrap_or(0);
    let since = a.get("--since").and_then(parse_since);
    let until = a.get("--until").and_then(parse_since);
    let author = a.get("--author").map(|s| regex::RegexBuilder::new(s).case_insensitive(a.has("-i")).build()).transpose()?;
    let grep = a.get("--grep").map(|s| regex::RegexBuilder::new(s).case_insensitive(a.has("-i")).build()).transpose()?;
    let mut picked: Vec<StateRec> = Vec::new();
    let mut skipped = 0;
    while let Some((_, id)) = heap.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let s = match repo.p.db.state(&id)? {
            Some(s) => s,
            None => continue,
        };
        let parents: Vec<String> = if first_parent { s.parents.iter().take(1).cloned().collect() } else { s.parents.clone() };
        for p in &parents {
            if !exclude.contains(p) && !seen.contains(p) {
                if let Some(ps) = repo.p.db.state(p)? {
                    heap.push((ps.time, p.clone()));
                }
            }
        }
        if picked.len() >= max && !a.has("--reverse") {
            break;
        }
        if a.has("--no-merges") && s.parents.len() > 1 || a.has("--merges") && s.parents.len() < 2 {
            continue;
        }
        if since.map(|t| s.time < t).unwrap_or(false) || until.map(|t| s.time > t).unwrap_or(false) {
            continue;
        }
        if let Some(r) = &author {
            if !r.is_match(&format!("{} <{}>", s.author.name, s.author.email)) {
                continue;
            }
        }
        if let Some(r) = &grep {
            if !r.is_match(&s.message) {
                continue;
            }
        }
        if !specs.is_empty() {
            if !repo.p.state_path(&s.id).exists() {
                continue;
            }
            let (_o, _t, _n, list) = state_changes(repo, &s)?;
            if !list
                .iter()
                .any(|c| matches(&specs, &c.path) || c.old_path.as_deref().map(|o| matches(&specs, o)).unwrap_or(false))
            {
                continue;
            }
        }
        if skipped < skip {
            skipped += 1;
            continue;
        }
        picked.push(s);
        if picked.len() >= max && !a.has("--reverse") {
            break;
        }
    }
    if a.has("--reverse") {
        picked.truncate(max);
        picked.reverse();
    }
    let decor = Decor::load(repo, line.as_ref())?;
    let decor = if a.has("--decorate") || a.get("--format").map(|f| f.contains("%d") || f.contains("%D")).unwrap_or(false) {
        decor
    } else {
        Decor { map: HashMap::new() }
    };
    let date_style = a.get("--date").unwrap_or("default").to_string();
    let fmt = pretty_format(&a);
    let opts = DiffOpts::from(&a, false)?;
    let want_diff = opts.patch || opts.stat || opts.numstat || opts.name_only || opts.name_status || opts.shortstat;
    let mut first = true;
    for s in &picked {
        let blank = print_state(ctx, s, fmt.as_deref(), &decor, &date_style, first)?;
        first = false;
        if want_diff && repo.p.state_path(&s.id).exists() {
            let (old, _t, new, list) = state_changes(repo, s)?;
            let list: Vec<Change> = list.into_iter().filter(|c| matches(&specs, &c.path)).collect();
            if !list.is_empty() {
                if blank {
                    outln!(ctx);
                }
                let fds = diff::file_diffs(&old, &new, &list)?;
                print_diffs(ctx, &fds, &opts);
            }
        }
    }
    Ok(0)
}

pub fn rev_parse(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .optional("--short")
        .flag("--show-toplevel")
        .optional("--abbrev-ref")
        .flag("--git-dir")
        .flag("--absolute-git-dir")
        .flag("--is-inside-work-tree")
        .flag("--is-inside-git-dir")
        .flag("--is-bare-repository")
        .flag("--verify")
        .flag("-q|--quiet")
        .flag("--show-prefix")
        .flag("--show-cdup")
        .flag("--symbolic-full-name")
        .flag("--git-common-dir");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = h.line.clone();
    if a.has("--show-toplevel") {
        outln!(ctx, "{}", repo.working(h.line()?).display());
    }
    if a.has("--is-inside-work-tree") {
        outln!(ctx, "{}", if line.is_some() { "true" } else { "false" });
    }
    if a.has("--is-inside-git-dir") || a.has("--is-bare-repository") {
        outln!(ctx, "false");
    }
    if a.has("--git-dir") || a.has("--absolute-git-dir") || a.has("--git-common-dir") {
        let g = repo.working(h.line()?).join(".git");
        if !g.exists() {
            return Err(exit(128, "fatal: this line has no .git layer"));
        }
        outln!(ctx, "{}", g.display());
    }
    if a.has("--show-prefix") {
        outln!(ctx, "{}", if h.loc.sub.is_empty() { String::new() } else { format!("{}/", h.loc.sub) });
    }
    if a.has("--show-cdup") {
        let n = if h.loc.sub.is_empty() { 0 } else { h.loc.sub.split('/').count() };
        outln!(ctx, "{}", "../".repeat(n));
    }
    let short: Option<usize> = match a.get("--short") {
        Some("") => Some(7),
        Some(n) => Some(n.parse()?),
        None => None,
    };
    for r in &a.pos {
        if a.has("--abbrev-ref") || a.has("--symbolic-full-name") {
            if r == "HEAD" || r == "@" {
                let n = h.line()?.name.clone();
                outln!(ctx, "{}", if a.has("--symbolic-full-name") { format!("refs/heads/{n}") } else { n });
                continue;
            }
            if r.ends_with("@{upstream}") || r.ends_with("@{u}") {
                let n = h.line()?.name.clone();
                if repo.p.db.remote_ref(&format!("origin/{n}"))?.is_some() {
                    outln!(ctx, "origin/{n}");
                    continue;
                }
                return Err(exit(128, format!("fatal: no upstream configured for line '{n}'")));
            }
            outln!(ctx, "{r}");
            continue;
        }
        let rr = r.trim_end_matches("^{commit}").trim_end_matches("^{}");
        match revs::resolve(repo, line.as_ref(), rr) {
            Ok(id) => match short {
                Some(n) => outln!(ctx, "{}", &ids::display(&id)[..n.clamp(4, 32)]),
                None => outln!(ctx, "{}", ids::display(&id)),
            },
            Err(e) => {
                if a.has("-q") {
                    return Ok(1);
                }
                if a.has("--verify") {
                    return Err(exit(128, "fatal: Needed a single revision"));
                }
                return Err(exit(128, format!("fatal: {e}")));
            }
        }
    }
    Ok(0)
}

pub fn merge_base(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("--is-ancestor").flag("-a|--all").flag("--fork-point");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let line = h.line.clone();
    if a.pos.len() < 2 {
        return Err(exit(128, "usage: layr merge-base [--is-ancestor] <a> <b>"));
    }
    let x = revs::resolve(&h.repo, line.as_ref(), &a.pos[0])?;
    let y = revs::resolve(&h.repo, line.as_ref(), &a.pos[1])?;
    if a.has("--is-ancestor") {
        return Ok(if h.repo.is_ancestor(&x, &y)? { 0 } else { 1 });
    }
    match h.repo.merge_base(&x, &y)? {
        Some(b) => {
            outln!(ctx, "{}", ids::display(&b));
            Ok(0)
        }
        None => Ok(1),
    }
}

pub fn blame(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .value("-L")
        .flag("-s")
        .flag("-e|--show-email")
        .flag("-l")
        .flag("--porcelain")
        .flag("-w")
        .flag("-M")
        .flag("-C");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = h.line.clone();
    let (rev, path) = match (a.pos.len(), a.paths.len()) {
        (_, 1) => (a.pos.first().cloned(), a.paths[0].clone()),
        (1, 0) => (None, a.pos[0].clone()),
        (2, 0) => (Some(a.pos[0].clone()), a.pos[1].clone()),
        _ => return Err(exit(128, "usage: layr blame [-L <a>,<b>] [<rev>] [--] <file>")),
    };
    let path = h.specs(&[path])?.remove(0);
    // Versions of the file along first parents; the working content when no revision is given.
    let start = revs::resolve(repo, line.as_ref(), rev.as_deref().unwrap_or("HEAD"))?;
    let mut versions: Vec<(Option<StateRec>, Vec<u8>)> = Vec::new();
    let mut _scan = None;
    if rev.is_none() {
        if let Some(l) = &line {
            let scan = repo.scan(l)?;
            if let Ok(d) = fsutil::read_file(&scan.path, &path) {
                versions.push((None, d));
            }
            _scan = Some(scan);
        }
    }
    let mut cur = Some(start.clone());
    while let Some(id) = cur {
        let s = repo.state(&id)?;
        let root = match repo.state_root(&id) {
            Ok(r) => r,
            Err(_) => break,
        };
        match fsutil::read_file(&root, &path) {
            Ok(d) => {
                if versions.last().map(|v| v.1 != d).unwrap_or(true) {
                    versions.push((Some(s.clone()), d));
                } else if let Some(last) = versions.last_mut() {
                    if last.0.is_some() {
                        last.0 = Some(s.clone());
                    }
                }
            }
            Err(_) => break,
        }
        cur = s.parents.first().cloned();
    }
    if versions.is_empty() {
        return Err(exit(128, format!("fatal: no such path '{path}'")));
    }
    // Attribute lines from the newest version backwards.
    let newest = String::from_utf8_lossy(&versions[0].1).into_owned();
    let lines: Vec<&str> = newest.split_inclusive('\n').collect();
    let mut owner: Vec<usize> = vec![0; lines.len()];
    // map[i] = index of the line in the current older version, or None once attributed.
    let mut map: Vec<Option<usize>> = (0..lines.len()).map(Some).collect();
    for vi in 1..versions.len() {
        let newer = String::from_utf8_lossy(&versions[vi - 1].1).into_owned();
        let older = String::from_utf8_lossy(&versions[vi].1).into_owned();
        let d = similar::TextDiff::from_lines(&older, &newer);
        let mut new_to_old: HashMap<usize, usize> = HashMap::new();
        for op in d.ops() {
            if let similar::DiffOp::Equal { old_index, new_index, len } = op {
                for k in 0..*len {
                    new_to_old.insert(new_index + k, old_index + k);
                }
            }
        }
        for i in 0..lines.len() {
            if let Some(m) = map[i] {
                match new_to_old.get(&m) {
                    Some(o) => {
                        map[i] = Some(*o);
                        owner[i] = vi;
                    }
                    None => map[i] = None,
                }
            }
        }
    }
    let (from, to) = match a.get("-L") {
        Some(r) => {
            let (x, y) = r.split_once(',').unwrap_or((r, r));
            let x: usize = x.parse().unwrap_or(1);
            let y: usize = if let Some(n) = y.strip_prefix('+') {
                x + n.parse::<usize>().unwrap_or(1) - 1
            } else {
                y.parse().unwrap_or(lines.len())
            };
            (x.max(1), y.min(lines.len()))
        }
        None => (1, lines.len()),
    };
    let width = lines.len().to_string().len();
    let name_w = versions.iter().filter_map(|v| v.0.as_ref().map(|s| s.author.name.chars().count())).max().unwrap_or(10).max(10);
    for i in from.saturating_sub(1)..to {
        let v = &versions[owner[i]];
        let text = lines[i].trim_end_matches('\n');
        match &v.0 {
            None => {
                if a.has("-s") {
                    outln!(ctx, "00000000 {:>w$}) {}", i + 1, text, w = width);
                } else {
                    outln!(
                        ctx,
                        "00000000 ({:<nw$} {} {:>w$}) {}",
                        "Not Committed Yet",
                        fmt_date(ids::now_ms(), "iso"),
                        i + 1,
                        text,
                        nw = name_w,
                        w = width
                    );
                }
            }
            Some(s) => {
                let boundary = owner[i] == versions.len() - 1 && s.parents.is_empty();
                let idn = &ids::display(&s.id)[..if boundary { 7 } else { 8 }];
                let pre = if boundary { "^" } else { "" };
                if a.has("-s") {
                    outln!(ctx, "{pre}{idn} {:>w$}) {}", i + 1, text, w = width);
                } else {
                    let who = if a.has("-e") { format!("<{}>", s.author.email) } else { s.author.name.clone() };
                    outln!(
                        ctx,
                        "{pre}{idn} ({:<nw$} {} {:>w$}) {}",
                        who,
                        fmt_date(s.time, "iso"),
                        i + 1,
                        text,
                        nw = name_w,
                        w = width
                    );
                }
            }
        }
    }
    Ok(0)
}

fn search_root(repo: &Repo, line: Option<&Line>, rev: Option<&str>) -> Result<(PathBuf, Vec<Temp>, Option<PathBuf>)> {
    match rev {
        Some(r) => {
            let id = revs::resolve(repo, line, r)?;
            Ok((repo.state_root(&id)?, Vec::new(), None))
        }
        None => {
            // A read-only snapshot of the working content: the live folder belongs to its owner
            // and can change while the service (root) reads it.
            let l = match line {
                Some(l) => l,
                None => bail!("not in a line"),
            };
            let (idx, it) = repo.index_view(l)?;
            let scan = repo.scan(l)?;
            let root = scan.path.clone();
            let mut keep = vec![scan];
            keep.extend(it);
            Ok((root, keep, Some(idx)))
        }
    }
}

pub fn grep(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .flag("-n|--line-number")
        .flag("-i|--ignore-case")
        .flag("-w|--word-regexp")
        .flag("-l|--files-with-matches|--name-only")
        .flag("-L|--files-without-match")
        .flag("-c|--count")
        .flag("-F|--fixed-strings")
        .flag("-E|--extended-regexp")
        .flag("-G|--basic-regexp")
        .flag("-P|--perl-regexp")
        .flag("-v|--invert-match")
        .flag("-h")
        .flag("-H")
        .flag("-q|--quiet")
        .flag("--untracked")
        .flag("-I")
        .flag("--cached")
        .flag("-z|--null")
        .value("-e")
        .value("-A|--after-context")
        .value("-B|--before-context")
        .value("-C|--context")
        .value("-m|--max-count");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = h.line.clone();
    let mut pos = a.pos.clone();
    let mut pats = a.all("-e");
    if pats.is_empty() {
        if pos.is_empty() {
            return Err(exit(128, "usage: layr grep [<options>] <pattern> [<rev>] [-- <path>...]"));
        }
        pats.push(pos.remove(0));
    }
    let mut rev: Option<String> = None;
    let mut paths = a.paths.clone();
    for p in pos {
        if rev.is_none() && paths.is_empty() && revs::resolve(repo, line.as_ref(), &p).is_ok() {
            rev = Some(p);
        } else {
            paths.push(p);
        }
    }
    let specs = h.specs(&paths)?;
    let joined = pats
        .iter()
        .map(|p| {
            let p = if a.has("-F") { regex::escape(p) } else { p.clone() };
            if a.has("-w") {
                format!(r"\b(?:{p})\b")
            } else {
                format!("(?:{p})")
            }
        })
        .collect::<Vec<_>>()
        .join("|");
    let re = regex::bytes::RegexBuilder::new(&joined).case_insensitive(a.has("-i")).build()?;
    let (root, _t, index) = if a.has("--cached") {
        let l = h.line()?;
        let (idx, t) = repo.index_view(l)?;
        (idx, t.into_iter().collect(), None)
    } else {
        search_root(repo, line.as_ref(), rev.as_deref())?
    };
    let rules = crate::ignore_rules::Rules::new(&root);
    let prefix = rev.as_ref().map(|r| format!("{r}:")).unwrap_or_default();
    let maxc: usize = a.get("-m").map(|v| v.parse()).transpose()?.unwrap_or(usize::MAX);
    let mut found = false;
    let mut files = Vec::new();
    fsutil::walk(&root, "", &mut |rel, m| {
        if skip_path(rel) {
            return Ok(false);
        }
        if rules.ignored(rel, m.is_dir()) {
            if let Some(idx) = &index {
                if fsutil::lmeta(idx, rel).is_none() {
                    return Ok(false);
                }
            } else {
                return Ok(false);
            }
        }
        if m.is_file() {
            if let Some(idx) = &index {
                if !a.has("--untracked") && fsutil::lmeta(idx, rel).is_none() {
                    return Ok(true);
                }
            }
            if matches(&specs, rel) {
                files.push(rel.to_string());
            }
        }
        Ok(true)
    })?;
    let sep = if a.has("-z") { "\0" } else { ":" };
    for f in files {
        let data = match fsutil::read_file(&root, &f) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let shown = format!("{prefix}{}", if rev.is_some() { f.clone() } else { h.rel(&f) });
        if fsutil::is_binary(&data) {
            if a.has("-I") {
                continue;
            }
            let hit = re.is_match(&data) != a.has("-v");
            if hit {
                found = true;
                if a.has("-q") {
                    return Ok(0);
                }
                if a.has("-l") {
                    outln!(ctx, "{shown}");
                } else if !a.has("-L") {
                    outln!(ctx, "Binary file {shown} matches");
                }
            }
            continue;
        }
        let mut count = 0;
        let mut out_lines = Vec::new();
        for (i, l) in data.split(|b| *b == b'\n').enumerate() {
            if i > 0 && i == data.split(|b| *b == b'\n').count() - 1 && l.is_empty() {
                break;
            }
            if re.is_match(l) != a.has("-v") {
                count += 1;
                if count <= maxc {
                    out_lines.push((i + 1, String::from_utf8_lossy(l).into_owned()));
                }
            }
        }
        if count > 0 {
            found = true;
        }
        if a.has("-q") {
            if found {
                return Ok(0);
            }
            continue;
        }
        if a.has("-L") {
            if count == 0 {
                outln!(ctx, "{shown}");
            }
            continue;
        }
        if count == 0 {
            continue;
        }
        if a.has("-l") {
            outln!(ctx, "{shown}");
        } else if a.has("-c") {
            outln!(ctx, "{shown}{sep}{count}");
        } else {
            for (n, l) in out_lines {
                let name = if a.has("-h") { String::new() } else { format!("{shown}{sep}") };
                if a.has("-n") {
                    outln!(ctx, "{name}{n}{sep}{l}");
                } else {
                    outln!(ctx, "{name}{l}");
                }
            }
        }
    }
    Ok(if found { 0 } else { 1 })
}

pub fn ls_files(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .flag("-c|--cached")
        .flag("-o|--others")
        .flag("-m|--modified")
        .flag("-d|--deleted")
        .flag("-i|--ignored")
        .flag("--exclude-standard")
        .flag("-z")
        .flag("-s|--stage")
        .flag("--full-name")
        .flag("--directory");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let specs = h.specs(&a.rest())?;
    let end = if a.has("-z") { "\0" } else { "\n" };
    let show = |p: &str| if a.has("--full-name") { p.to_string() } else { h.rel(p) };
    let want_cached = a.has("-c") || !(a.has("-o") || a.has("-m") || a.has("-d") || a.has("-i"));
    if want_cached || a.has("-s") {
        let (idx, _t) = h.repo.index_view(&line)?;
        let mut v = Vec::new();
        fsutil::walk(&idx, "", &mut |rel, m| {
            if skip_path(rel) {
                return Ok(false);
            }
            if !m.is_dir() && matches(&specs, rel) {
                v.push((rel.to_string(), m.clone()));
            }
            Ok(true)
        })?;
        for (p, m) in v {
            if a.has("-s") {
                let data = fsutil::entry_bytes(&idx, &p)?;
                out!(ctx, "{:06o} {} 0\t{}{}", m.git_mode(), fsutil::git_blob_id(&data), show(&p), end);
            } else {
                out!(ctx, "{}{}", show(&p), end);
            }
        }
    }
    if a.has("-o") || a.has("-m") || a.has("-d") || a.has("-i") {
        let (st, _scan, _it, _idx) = h.repo.status(&line)?;
        if a.has("-m") || a.has("-d") {
            for c in &st.unstaged {
                if (a.has("-m") || c.new.is_none()) && matches(&specs, &c.path) {
                    out!(ctx, "{}{}", show(&c.path), end);
                }
            }
        }
        if a.has("-o") && !a.has("-i") {
            let list = if a.has("--directory") {
                let mut v: BTreeSet<String> = BTreeSet::new();
                for f in &st.untracked {
                    match st.untracked_dirs.iter().find(|d| f.starts_with(&format!("{d}/"))) {
                        Some(d) => v.insert(format!("{d}/")),
                        None => v.insert(f.clone()),
                    };
                }
                v.into_iter().collect()
            } else {
                st.untracked.clone()
            };
            for u in list {
                if matches(&specs, u.trim_end_matches('/')) {
                    out!(ctx, "{}{}", show(&u), end);
                }
            }
        }
        if a.has("-i") {
            for i in &st.ignored {
                if matches(&specs, i) {
                    out!(ctx, "{}{}", show(i), end);
                }
            }
        }
    }
    Ok(0)
}

#[allow(dead_code)]
fn unused(_: &Here) {}
