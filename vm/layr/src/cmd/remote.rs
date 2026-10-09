//! The remote bridge commands: remote, fetch, pull, push, clone.

use super::admin::set_config;
use super::{exit, here, mergecmd};
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::gitbridge;
use crate::ids;
use crate::model::Record;
use crate::repo::Repo;
use crate::revs;
use anyhow::Result;

pub fn remote(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let h = here(ctx)?;
    let repo = &h.repo;
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    let remotes = || -> Result<Vec<(String, String)>> {
        let mut v = Vec::new();
        for (k, val) in repo.p.db.config_all()? {
            if let Some(n) = k.strip_prefix("remote.").and_then(|x| x.strip_suffix(".url")) {
                let url: String = serde_json::from_str(&val).unwrap_or(val.clone());
                v.push((n.to_string(), url));
            }
        }
        Ok(v)
    };
    match sub {
        "" | "-v" | "--verbose" => {
            for (n, u) in remotes()? {
                if sub.is_empty() {
                    outln!(ctx, "{n}");
                } else {
                    outln!(ctx, "{n}\t{u} (fetch)\n{n}\t{u} (push)");
                }
            }
            Ok(0)
        }
        "add" | "set-url" => {
            let (n, u) = match (args.get(1), args.get(2)) {
                (Some(n), Some(u)) => (n, u),
                _ => return Err(exit(128, "usage: layr remote add <name> <url>")),
            };
            repo.require_lead(ctx, "change remotes")?;
            crate::store::check_name("remote", n)?;
            if sub == "add" && repo.p.config_value(&format!("remote.{n}.url"))?.is_some() {
                return Err(exit(3, format!("error: remote {n} already exists.")));
            }
            let _lock = repo.p.lock()?;
            set_config(ctx, repo, &format!("remote.{n}.url"), serde_json::json!(u))?;
            gitbridge::init_store(repo)?;
            Ok(0)
        }
        "remove" | "rm" => {
            let n = args.get(1).ok_or_else(|| exit(128, "usage: layr remote remove <name>"))?;
            repo.require_lead(ctx, "change remotes")?;
            let _lock = repo.p.lock()?;
            set_config(ctx, repo, &format!("remote.{n}.url"), serde_json::Value::Null)?;
            Ok(0)
        }
        "get-url" => {
            let n = args.get(1).ok_or_else(|| exit(128, "usage: layr remote get-url <name>"))?;
            outln!(ctx, "{}", gitbridge::remote_url(repo, n)?);
            Ok(0)
        }
        _ => Err(exit(128, "usage: layr remote [-v | add <name> <url> | remove <name> | get-url <name> | set-url <name> <url>]")),
    }
}

/// Branches to import after a fetch: the requested ones, the ones imported before, and the
/// ones that match a line name or the default branch.
fn import_after_fetch(
    ctx: &Ctx,
    repo: &Repo,
    remote: &str,
    refs: &[(String, String)],
    only: Option<&str>,
) -> Result<Vec<(String, String, bool)>> {
    let mut out = Vec::new();
    let lines: Vec<String> = repo.lines()?.into_iter().map(|l| l.name).collect();
    for (name, commit) in refs {
        let branch = name.strip_prefix(&format!("{remote}/")).unwrap_or(name);
        if branch == "HEAD" {
            continue;
        }
        let wanted = match only {
            Some(o) => o == branch,
            None => {
                repo.p.db.remote_ref(name)?.is_some() || lines.iter().any(|l| l == branch) || ["main", "master"].contains(&branch)
            }
        };
        if !wanted {
            continue;
        }
        let prev = repo.p.db.remote_ref(name)?;
        let (st, fresh) = gitbridge::import_commit(ctx, repo, commit, name, ctx.caller.uid)?;
        if prev.as_ref().map(|p| p.0 != st).unwrap_or(true) && !fresh {
            let mut rec = Record::new("remote.ref");
            rec.data = serde_json::json!({"ref": name, "state": st, "commit": commit});
            repo.commit_record(ctx, rec)?;
        }
        out.push((name.clone(), st, prev.map(|p| p.0).is_none()));
    }
    Ok(out)
}

pub fn fetch(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("--all").flag("-q|--quiet").flag("-p|--prune").flag("--tags").flag("-v").value("--depth");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let remote = a.pos.first().cloned().unwrap_or_else(|| "origin".into());
    let only = a.pos.get(1).cloned();
    let refs = gitbridge::fetch(ctx, repo, &remote)?;
    let _lock = repo.p.lock()?;
    let done = import_after_fetch(ctx, repo, &remote, &refs, only.as_deref())?;
    if !a.has("-q") {
        let url = gitbridge::remote_url(repo, &remote)?;
        if !done.is_empty() {
            outln!(ctx, "From {url}");
        }
        for (name, st, new) in done {
            outln!(
                ctx,
                " {} {:<20} -> {} (state {})",
                if new { "* [new]" } else { "  " },
                name.split('/').nth(1).unwrap_or(&name),
                name,
                ids::short(&st)
            );
        }
    }
    Ok(0)
}

pub fn pull(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .flag("--ff-only")
        .flag("--no-ff")
        .flag("--rebase|-r")
        .flag("-q|--quiet")
        .flag("--no-rebase")
        .value("--expect");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let remote = a.pos.first().cloned().unwrap_or_else(|| "origin".into());
    let refs = gitbridge::fetch(ctx, &h.repo, &remote)?;
    let branch = match a.pos.get(1) {
        Some(b) => b.clone(),
        None => {
            if refs.iter().any(|(n, _)| n == &format!("{remote}/{}", line.name)) {
                line.name.clone()
            } else if refs.iter().any(|(n, _)| n == &format!("{remote}/main")) {
                "main".into()
            } else {
                refs.iter()
                    .map(|(n, _)| n.split('/').nth(1).unwrap_or("").to_string())
                    .find(|b| b == "master")
                    .unwrap_or_else(|| "main".into())
            }
        }
    };
    {
        let _lock = h.repo.p.lock()?;
        import_after_fetch(ctx, &h.repo, &remote, &refs, Some(&branch))?;
    }
    let rev = format!("{remote}/{branch}");
    if h.repo.p.db.remote_ref(&rev)?.is_none() {
        return Err(exit(1, format!("fatal: couldn't find remote ref {branch}")));
    }
    if a.has("-r") {
        return mergecmd::rebase(ctx, &[rev]);
    }
    let mut margs = vec![rev];
    for f in ["--ff-only", "--no-ff", "-q"] {
        if a.has(f) {
            margs.push(f.into());
        }
    }
    let pa = parse(&Spec::new().flag("--ff-only").flag("--no-ff").flag("-q|--quiet").value("--expect"), &margs[1..])?;
    mergecmd::merge_rev(ctx, &h.repo, &line, &margs[0], &pa)
}

pub fn push(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .flag("-f|--force")
        .flag("--force-with-lease")
        .flag("-n|--dry-run")
        .flag("-u|--set-upstream")
        .flag("-q|--quiet")
        .value("-m|--message")
        .flag("--no-verify")
        .flag("--tags");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = h.line.clone();
    let remote = a.pos.first().cloned().unwrap_or_else(|| "origin".into());
    let (rev, branch) = match a.pos.get(1) {
        Some(spec) => match spec.split_once(':') {
            Some((r, b)) => (r.to_string(), b.trim_start_matches("refs/heads/").to_string()),
            None => {
                if repo.p.db.line_by_name(spec)?.is_some() {
                    (spec.clone(), spec.clone())
                } else {
                    ("HEAD".to_string(), spec.clone())
                }
            }
        },
        None => ("HEAD".to_string(), h.line()?.name.clone()),
    };
    // The lead decides when a task goes to the remote (members with push.members = true too).
    if !repo.is_lead(ctx) && !repo.p.config_bool("push.members", false) {
        return Err(exit(1, "permission denied: only the project lead pushes (set push.members true to allow members)"));
    }
    let state_id = revs::resolve(repo, line.as_ref(), &rev)?;
    let state = repo.state(&state_id)?;
    if !state.conflicts.is_empty() {
        return Err(exit(
            1,
            format!(
                "error: state {} has {} open conflict(s); resolve and commit them before a push",
                ids::short(&state_id),
                state.conflicts.len()
            ),
        ));
    }
    if repo.p.config_bool("push.require_check", false) {
        let ok = repo.p.db.checks(&state_id)?.iter().any(|c| c.1 == 0);
        if !ok {
            return Err(exit(
                1,
                format!(
                    "error: state {} has no passing check; run 'layr try {} -- <tests>' first",
                    ids::short(&state_id),
                    ids::short(&state_id)
                ),
            ));
        }
    }
    let url = gitbridge::remote_url(repo, &remote)?;
    // A push prepared earlier (for example before a lost response): read the remote ref first.
    let mut prepared: Option<(String, String)> = None;
    for (eid, st, r, b, c, status) in repo.p.db.exports()? {
        if st == state_id && r == remote && b == branch && status == "prepared" {
            prepared = Some((eid, c.clone()));
        }
        if st == state_id && r == remote && b == branch && status == "pushed" {
            prepared = None;
            if !a.has("-f") && gitbridge::ls_remote(ctx, repo, &url, &branch)?.as_deref() == Some(c.as_str()) {
                outln!(ctx, "Everything up-to-date ({} is {} on {remote}/{branch})", ids::short(&state_id), &c[..10]);
                return Ok(0);
            }
        }
    }
    let _lock = repo.p.lock()?;
    let (export_id, commit) = match prepared {
        Some((eid, c)) => {
            if gitbridge::ls_remote(ctx, repo, &url, &branch)?.as_deref() == Some(c.as_str()) {
                let mut rec = Record::new("export");
                rec.data = serde_json::json!({"export": eid, "state": state_id, "remote": remote, "branch": branch, "commit": c, "status": "pushed", "verified": "remote ref"});
                repo.commit_record(ctx, rec)?;
                outln!(ctx, "Already pushed before: {remote}/{branch} is {} (state {})", &c[..10], ids::short(&state_id));
                return Ok(0);
            }
            (eid, c)
        }
        None => {
            let (c, parent) = gitbridge::make_commit(repo, &state, a.get("-m"))?;
            let eid = ids::new_id();
            let mut rec = Record::new("export");
            rec.data = serde_json::json!({"export": eid, "state": state_id, "remote": remote, "branch": branch, "commit": c, "parent": parent, "status": "prepared"});
            repo.commit_record(ctx, rec)?;
            (eid, c)
        }
    };
    if a.has("-n") {
        outln!(ctx, "Would push {} (state {}) to {url} {branch}", &commit[..10], ids::short(&state_id));
        return Ok(0);
    }
    let before = gitbridge::ls_remote(ctx, repo, &url, &branch).ok().flatten();
    gitbridge::push(ctx, repo, &url, &commit, &branch, a.has("-f") || a.has("--force-with-lease"))?;
    let mut rec = Record::new("export");
    rec.data = serde_json::json!({"export": export_id, "state": state_id, "remote": remote, "branch": branch, "commit": commit, "status": "pushed"});
    repo.commit_record(ctx, rec)?;
    let _ = gitbridge::git_out(&gitbridge::store(repo), &["update-ref", &format!("refs/remotes/{remote}/{branch}"), &commit]);
    if let Some(l) = &line {
        if let Err(e) = gitbridge::refresh_git_layer(repo, &repo.line(&l.id)?) {
            eprintln!("warning: .git layer not updated: {e:#}");
        }
    }
    if !a.has("-q") {
        outln!(ctx, "To {url}");
        match before {
            None => outln!(ctx, " * [new branch]      {} -> {branch}", &commit[..7]),
            Some(b) => outln!(ctx, "   {}..{}  {} -> {branch}", &b[..7], &commit[..7], ids::short(&state_id)),
        }
        outln!(ctx, "state {} = commit {commit}", ids::short(&state_id));
    }
    Ok(0)
}

pub fn clone(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().value("-b|--branch").value("--owner").flag("-q|--quiet").value("--depth");
    let a = parse(&spec, args)?;
    let url = a.pos.first().ok_or_else(|| exit(128, "usage: layr clone <url> [<project>]"))?.clone();
    let name = match a.pos.get(1) {
        Some(n) => n.clone(),
        None => url.trim_end_matches('/').rsplit('/').next().unwrap_or("project").trim_end_matches(".git").to_string(),
    };
    let mut v = vec![name, "--git".into(), url];
    if let Some(b) = a.get("-b") {
        v.push("--branch".into());
        v.push(b.into());
    }
    if let Some(o) = a.get("--owner") {
        v.push("--owner".into());
        v.push(o.into());
    }
    if a.has("-q") {
        v.push("-q".into());
    }
    super::admin::init(ctx, &v)
}
