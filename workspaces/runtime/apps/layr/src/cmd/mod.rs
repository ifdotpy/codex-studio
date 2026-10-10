//! Command dispatch and helpers shared by commands.

pub mod admin;
pub mod branch;
pub mod history;
pub mod mergecmd;
pub mod remote;
pub mod replicate;
pub mod transfer;
pub mod work;

use crate::ctx::{Ctx, Loc};
use crate::model::Line;
use crate::repo::Repo;
use anyhow::{bail, Result};

/// An error with a specific exit code (git uses 1 for "not done", 128 for fatal errors).
#[derive(Debug)]
pub struct Exit(pub i32, pub String);

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.1)
    }
}
impl std::error::Error for Exit {}

pub fn exit(code: i32, msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Exit(code, msg.into()))
}

pub struct Here<'a> {
    pub repo: Repo<'a>,
    pub line: Option<Line>,
    pub loc: Loc,
}

impl<'a> Here<'a> {
    pub fn line(&self) -> Result<&Line> {
        match &self.line {
            Some(l) => Ok(l),
            None => bail!("not in a line: cd into lines/<name> of project {}", self.repo.p.name),
        }
    }

    /// Convert command line paths (relative to the working folder) to paths from the line root.
    pub fn specs(&self, args: &[String]) -> Result<Vec<String>> {
        args.iter().map(|a| normalize(&self.loc.sub, a)).collect()
    }

    /// Show a root-relative path relative to the working folder, as git does.
    pub fn rel(&self, p: &str) -> String {
        relative_to(&self.loc.sub, p)
    }
}

pub fn here(ctx: &Ctx) -> Result<Here<'_>> {
    let loc = ctx.locate()?;
    let repo = Repo::open(ctx, &loc.project)?;
    let line = match &loc.line {
        Some(n) => Some(repo.line(n)?),
        None => None,
    };
    Ok(Here { repo, line, loc })
}

pub fn normalize(sub: &str, arg: &str) -> Result<String> {
    if arg.starts_with('/') {
        bail!("absolute paths are not supported: {arg}");
    }
    let mut parts: Vec<String> = if sub.is_empty() { vec![] } else { sub.split('/').map(|s| s.to_string()).collect() };
    for c in arg.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    bail!("path outside the line: {arg}");
                }
            }
            x => parts.push(x.to_string()),
        }
    }
    Ok(parts.join("/"))
}

pub fn relative_to(sub: &str, p: &str) -> String {
    if sub.is_empty() {
        return p.to_string();
    }
    let s: Vec<&str> = sub.split('/').collect();
    let t: Vec<&str> = p.split('/').collect();
    let mut i = 0;
    while i < s.len() && i < t.len() && s[i] == t[i] {
        i += 1;
    }
    let mut out: Vec<&str> = std::iter::repeat_n("..", s.len() - i).collect();
    out.extend_from_slice(&t[i..]);
    out.join("/")
}

/// A simple glob: `*` and `?` (both match `/` too, as in git pathspecs) and `[...]`.
/// Iterative, with one backtrack point (the last `*`): time O(pattern * text), no recursion.
pub fn glob(pat: &str, s: &str) -> bool {
    let (p, s) = (pat.as_bytes(), s.as_bytes());
    // One pattern item at p[i] against byte c: Some(length of the item) if it matches.
    let item = |i: usize, c: u8| -> Option<usize> {
        match p[i] {
            b'?' => Some(1),
            b'[' => match p[i + 1..].iter().position(|x| *x == b']') {
                Some(off) => {
                    let end = i + 1 + off;
                    let set = &p[i + 1..end];
                    let (neg, set) =
                        if set.first() == Some(&b'!') || set.first() == Some(&b'^') { (true, &set[1..]) } else { (false, set) };
                    let mut hit = false;
                    let mut k = 0;
                    while k < set.len() {
                        if k + 2 < set.len() && set[k + 1] == b'-' {
                            hit |= c >= set[k] && c <= set[k + 2];
                            k += 3;
                        } else {
                            hit |= c == set[k];
                            k += 1;
                        }
                    }
                    (hit != neg).then_some(end + 1 - i)
                }
                None => (c == b'[').then_some(1),
            },
            x => (x == c).then_some(1),
        }
    };
    let (mut i, mut j) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while j < s.len() {
        if i < p.len() && p[i] == b'*' {
            star = Some((i, j));
            i += 1;
            continue;
        }
        if i < p.len() {
            if let Some(n) = item(i, s[j]) {
                i += n;
                j += 1;
                continue;
            }
        }
        match star {
            // Let the last `*` take one more byte and try again from there.
            Some((si, sj)) => {
                i = si + 1;
                j = sj + 1;
                star = Some((si, sj + 1));
            }
            None => return false,
        }
    }
    while i < p.len() && p[i] == b'*' {
        i += 1;
    }
    i == p.len()
}

/// True if `path` matches one of the pathspecs (empty list matches everything).
pub fn matches(specs: &[String], path: &str) -> bool {
    if specs.is_empty() {
        return true;
    }
    specs.iter().any(|s| {
        s.is_empty() || path == s || path.starts_with(&format!("{s}/")) || (s.contains(['*', '?', '[']) && glob(s, path))
    })
}

const HELP: &str = "usage: layr <command> [<args>]

Work in a line (git verbs):
   status, diff, add, rm, mv, restore, reset, commit, stash, clean, apply
Read history:
   log, show, blame, grep, ls-files, rev-parse, merge-base
Lines and states:
   branch, switch, checkout, tag, merge, cherry-pick, revert, rebase
   save, undo, op, try, checks, adopt
Remote (git bridge):
   remote, fetch, pull, push, clone
Projects and machines:
   init, projects, config, layer, export, backup, sync, serve-peer, gc, fsck, reindex, daemon

Lines are folders: <root>/projects/<project>/lines/<line>. Run commands inside a line folder.
";

pub fn run(ctx: &Ctx, argv: &[String]) -> Result<i32> {
    let (cmd, rest) = match argv.first() {
        Some(c) => (c.as_str(), &argv[1..]),
        None => {
            ctx.print(HELP);
            return Ok(1);
        }
    };
    let rest: Vec<String> = rest.to_vec();
    match cmd {
        "help" | "--help" | "-h" => {
            ctx.print(HELP);
            Ok(0)
        }
        "version" | "--version" => {
            outln!(ctx, "layr version {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        "status" => work::status(ctx, &rest),
        "diff" => history::diff(ctx, &rest),
        "show" => history::show(ctx, &rest),
        "log" => history::log(ctx, &rest),
        "rev-parse" => history::rev_parse(ctx, &rest),
        "merge-base" => history::merge_base(ctx, &rest),
        "blame" => history::blame(ctx, &rest),
        "grep" => history::grep(ctx, &rest),
        "ls-files" => history::ls_files(ctx, &rest),
        "add" => work::add(ctx, &rest),
        "rm" => work::rm(ctx, &rest),
        "mv" => work::mv(ctx, &rest),
        "apply" => work::apply(ctx, &rest),
        "commit" => work::commit(ctx, &rest),
        "restore" => work::restore(ctx, &rest),
        "checkout" => branch::checkout(ctx, &rest),
        "switch" => branch::switch(ctx, &rest),
        "reset" => work::reset(ctx, &rest),
        "stash" => work::stash(ctx, &rest),
        "clean" => work::clean(ctx, &rest),
        "branch" => branch::branch(ctx, &rest),
        "tag" => branch::tag(ctx, &rest),
        "adopt" => branch::adopt(ctx, &rest),
        "merge" => mergecmd::merge(ctx, &rest),
        "cherry-pick" => mergecmd::cherry_pick(ctx, &rest),
        "revert" => mergecmd::revert(ctx, &rest),
        "rebase" => mergecmd::rebase(ctx, &rest),
        "save" => admin::save(ctx, &rest),
        "undo" => admin::undo(ctx, &rest),
        "op" => admin::op(ctx, &rest),
        "try" => admin::try_cmd(ctx, &rest),
        "access" => admin::access(ctx, &rest),
        "protect" => admin::protect(ctx, &rest),
        "approve" => admin::approve(ctx, &rest),
        "approvals" => admin::approvals(ctx, &rest),
        "checks" => admin::checks(ctx, &rest),
        "gc" => admin::gc(ctx, &rest),
        "fsck" => admin::fsck(ctx, &rest),
        "reindex" => admin::reindex(ctx, &rest),
        "init" => admin::init(ctx, &rest),
        "projects" => admin::projects(ctx, &rest),
        "machines" => admin::machines(ctx, &rest),
        "debug" => admin::debug(ctx, &rest),
        "config" => admin::config(ctx, &rest),
        "layer" => admin::layer(ctx, &rest),
        "remote" => remote::remote(ctx, &rest),
        "fetch" => remote::fetch(ctx, &rest),
        "pull" => remote::pull(ctx, &rest),
        "push" => remote::push(ctx, &rest),
        "clone" => remote::clone(ctx, &rest),
        "export" => transfer::export(ctx, &rest),
        "backup" => transfer::backup(ctx, &rest),
        "sync" => replicate::sync(ctx, &rest),
        "serve-peer" => replicate::serve(ctx, &rest),
        "worktree" => {
            Err(exit(1, "layr has no worktrees: each line is a folder. Create one with 'layr branch <name> [<start>]'."))
        }
        other => Err(exit(1, format!("layr: '{other}' is not a layr command. See 'layr help'."))),
    }
}

#[cfg(test)]
mod tests {
    use super::glob;

    #[test]
    fn glob_matches() {
        assert!(glob("*.rs", "src/main.rs"));
        assert!(glob("src/*", "src/a/b"));
        assert!(glob("a?c", "abc"));
        assert!(!glob("a?c", "ac"));
        assert!(glob("[a-c]x", "bx"));
        assert!(!glob("[!a-c]x", "bx"));
        assert!(glob("*a*b*c", "xxaxxbxxc"));
        assert!(!glob("*a*b*c", "xxaxxbxx"));
        assert!(glob("[", "["));
        assert!(glob("", ""));
        assert!(!glob("", "a"));
        assert!(glob("**", ""));
        // A pattern that takes exponential time with naive recursion.
        let text = "a".repeat(60);
        let start = std::time::Instant::now();
        assert!(!glob(&("*a".repeat(30) + "b"), &text));
        assert!(start.elapsed().as_secs() < 1);
    }
}
