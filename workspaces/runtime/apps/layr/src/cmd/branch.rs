//! Lines (branch, switch, checkout, adopt) and tags.

use super::{exit, here, work};
use crate::args::{parse, Spec};
use crate::ctx::{Caller, Ctx};
use crate::ids;
use crate::model::{LineChange, Record, StateKind};
use crate::revs;
use anyhow::Result;

fn owner_arg(ctx: &Ctx, v: Option<&str>) -> Result<u32> {
    match v {
        None => Ok(ctx.caller.uid),
        Some(s) => {
            if let Ok(n) = s.parse::<u32>() {
                return Ok(n);
            }
            match crate::sys::user_by_name(s) {
                Some(u) => Ok(u.uid),
                None => {
                    return Err(exit(128, format!("fatal: unknown user '{s}'")));
                }
            }
        }
    }
}

pub fn branch(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .flag("-l|--list")
        .flag("-v|--verbose")
        .flag("-a|--all")
        .flag("-r|--remotes")
        .flag("-d|--delete")
        .flag("-D")
        .flag("-m|--move")
        .flag("-M")
        .flag("-f|--force")
        .flag("--show-current")
        .flag("--path")
        .value("--owner")
        .optional("--format")
        .value("--contains")
        .flag("-q|--quiet");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let current = h.line.clone();
    if a.has("--show-current") {
        if let Some(l) = &current {
            outln!(ctx, "{}", l.name);
        }
        return Ok(0);
    }
    if a.has("-d") || a.has("-D") {
        if a.pos.is_empty() {
            return Err(exit(128, "fatal: line name required"));
        }
        for name in &a.pos {
            let l = repo.line(name)?;
            repo.authorize_line(ctx, &l, crate::repo::LineOp::Manage)?;
            if current.as_ref().map(|c| c.id == l.id).unwrap_or(false) {
                return Err(exit(1, format!("error: cannot delete line '{name}' used as the current folder")));
            }
            if a.has("-d") && !a.has("-f") {
                let into = current.as_ref().map(|c| c.head.clone());
                let merged = match &into {
                    Some(h2) => repo.is_ancestor(&l.head, h2)?,
                    None => false,
                };
                let base_only = repo.p.db.line_states(&l.id, StateKind::Commit)?.is_empty();
                if !merged && !base_only {
                    return Err(exit(1, format!("error: the line '{name}' is not fully merged.\nIf you are sure you want to delete it, run 'layr branch -D {name}'.")));
                }
            }
            let _lock = repo.p.lock()?;
            let last = repo.working_state(ctx, &l, "before line delete")?;
            // Record first, then delete the folder: the content is in the saved state.
            let mut rec = Record::new("line.delete");
            rec.lines.insert(
                l.id.clone(),
                LineChange {
                    before: Some(repo.line_state(&l, &l.head)),
                    after: None,
                    working_before: last.clone(),
                    working_after: None,
                },
            );
            rec.data = serde_json::json!({"name": l.name, "final_state": last});
            repo.commit_record(ctx, rec)?;
            crate::btrfs::delete_tree(&repo.working(&l))?;
            repo.drop_stage(&l)?;
            // The owner of a deleted line no longer reads the git objects.
            crate::gitbridge::sync_store_access(repo)?;
            outln!(ctx, "Deleted line {} (was {}).", l.name, ids::short(&l.head));
        }
        return Ok(0);
    }
    if a.has("-m") || a.has("-M") {
        let (old, new) = match a.pos.len() {
            1 => (current.as_ref().map(|c| c.name.clone()).unwrap_or_default(), a.pos[0].clone()),
            2 => (a.pos[0].clone(), a.pos[1].clone()),
            _ => return Err(exit(128, "usage: layr branch -m [<old>] <new>")),
        };
        crate::store::check_name("line", &new)?;
        let l = repo.line(&old)?;
        repo.authorize_line(ctx, &l, crate::repo::LineOp::Manage)?;
        let _lock = repo.p.lock()?;
        if repo.p.db.line_by_name(&new)?.is_some() {
            return Err(exit(128, format!("fatal: a line named '{new}' already exists")));
        }
        std::fs::rename(repo.p.line_path(&l.name), repo.p.line_path(&new))?;
        let mut after = repo.line_state(&l, &l.head);
        after.name = new.clone();
        let mut rec = Record::new("line.rename");
        rec.lines.insert(
            l.id.clone(),
            LineChange {
                before: Some(repo.line_state(&l, &l.head)),
                after: Some(after),
                working_before: None,
                working_after: None,
            },
        );
        rec.data = serde_json::json!({"from": old, "to": new});
        repo.commit_record(ctx, rec)?;
        outln!(ctx, "Renamed line {old} to {new}: {}", repo.p.line_path(&new).display());
        return Ok(0);
    }
    if !a.pos.is_empty() && !a.has("-l") {
        let name = &a.pos[0];
        let start = match a.pos.get(1) {
            Some(r) => revs::resolve(repo, current.as_ref(), r)?,
            None => match &current {
                Some(c) => c.head.clone(),
                None => match repo.p.db.line_by_name("main")? {
                    Some(m) => m.head,
                    None => return Err(exit(128, "fatal: give a start revision")),
                },
            },
        };
        let owner = owner_arg(ctx, a.get("--owner"))?;
        repo.require(ctx, "line.create").map_err(|e| exit(128, format!("fatal: {e}")))?;
        if owner != ctx.caller.uid {
            repo.require(ctx, "line.manage").map_err(|e| exit(128, format!("fatal: a line for another user: {e}")))?;
        }
        if owner == 0 && !ctx.caller.is_root() {
            return Err(exit(128, "fatal: only root can create a line owned by root"));
        }
        let _lock = repo.p.lock()?;
        let l = repo.create_line(ctx, name, &start, owner)?;
        if a.has("--path") {
            outln!(ctx, "{}", repo.working(&l).display());
        } else if !a.has("-q") {
            outln!(ctx, "Created line {} at {} from {}", l.name, repo.working(&l).display(), ids::short(&start));
        }
        return Ok(0);
    }
    // List.
    let lines = repo.lines()?;
    let pat = a.pos.first().cloned();
    let show_local = !a.has("-r");
    if show_local {
        for l in &lines {
            if let Some(p) = &pat {
                if !super::glob(p, &l.name) {
                    continue;
                }
            }
            let mark = if current.as_ref().map(|c| c.id == l.id).unwrap_or(false) { "* " } else { "  " };
            if a.has("-v") {
                let s = repo.state(&l.head)?;
                let owner = Caller::from_uid(l.owner).user;
                let here_m = if l.machine == repo.p.store.machine.id {
                    String::new()
                } else {
                    format!(" [machine {}]", &ids::display(&l.machine)[..8])
                };
                outln!(
                    ctx,
                    "{mark}{:<20} {} {} ({owner}){here_m}",
                    l.name,
                    ids::short(&l.head),
                    s.message.lines().next().unwrap_or("")
                );
            } else {
                outln!(ctx, "{mark}{}", l.name);
            }
        }
    }
    if a.has("-a") || a.has("-r") {
        for (r, s, _) in repo.p.db.remote_refs()? {
            let prefix = if a.has("-a") { "remotes/" } else { "" };
            if a.has("-v") {
                outln!(ctx, "  {prefix}{r} {}", ids::short(&s));
            } else {
                outln!(ctx, "  {prefix}{r}");
            }
        }
    }
    Ok(0)
}

pub fn switch(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().value("-c|--create").value("-C|--force-create").flag("-d|--detach").flag("-q");
    let a = parse(&spec, args)?;
    if let Some(n) = a.get("-c").or(a.get("-C")) {
        let mut v = vec![n.to_string()];
        v.extend(a.pos.iter().cloned());
        let code = branch(ctx, &v)?;
        let h = here(ctx)?;
        let l = h.repo.line(n)?;
        outln!(ctx, "Lines are folders: run 'cd {}' to work in it.", h.repo.working(&l).display());
        return Ok(code);
    }
    let h = here(ctx)?;
    match a.pos.first() {
        Some(n) => match h.repo.line(n) {
            Ok(l) => Err(exit(
                1,
                format!("Lines are folders: run 'cd {}' to work in line '{}'.", h.repo.working(&l).display(), l.name),
            )),
            Err(_) => Err(exit(128, format!("fatal: invalid reference: {n}"))),
        },
        None => Err(exit(128, "fatal: missing line name")),
    }
}

pub fn checkout(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .value("-b")
        .value("-B")
        .flag("--ours")
        .flag("--theirs")
        .flag("-f|--force")
        .flag("-q|--quiet")
        .flag("--detach");
    let a = parse(&spec, args)?;
    if let Some(n) = a.get("-b").or(a.get("-B")) {
        let mut v = vec![n.to_string()];
        v.extend(a.pos.iter().cloned());
        branch(ctx, &v)?;
        let h = here(ctx)?;
        let l = h.repo.line(n)?;
        outln!(ctx, "Lines are folders: run 'cd {}' to work in it.", h.repo.working(&l).display());
        return Ok(0);
    }
    let h = here(ctx)?;
    let line = h.line()?.clone();
    // checkout [<rev>] -- <paths>: restore paths into the index and the working folder.
    if !a.paths.is_empty() || a.has("--ours") || a.has("--theirs") {
        let mut paths = a.paths.clone();
        let mut rev: Option<String> = None;
        if a.dashdash {
            rev = a.pos.first().cloned();
        } else {
            paths.extend(a.pos.iter().cloned());
        }
        let specs = h.specs(&paths)?;
        let staged = rev.is_some();
        return work::restore_impl(ctx, &h, &line, &specs, staged, true, rev.as_deref(), a.has("--ours"), a.has("--theirs"));
    }
    let target = match a.pos.first() {
        Some(t) => t.clone(),
        None => return Ok(0),
    };
    if let Ok(l) = h.repo.line(&target) {
        if l.id == line.id {
            outln!(ctx, "Already on '{}'", l.name);
            return Ok(0);
        }
        return Err(exit(
            1,
            format!("Lines are folders: run 'cd {}' to work in line '{}'.", h.repo.working(&l).display(), l.name),
        ));
    }
    if revs::resolve(&h.repo, Some(&line), &target).is_ok() {
        // Could also be a path.
        let p = super::normalize(&h.loc.sub, &target)?;
        if crate::fsutil::lmeta(&h.repo.working(&line), &p).is_some() {
            return work::restore_impl(ctx, &h, &line, &[p], false, true, None, false, false);
        }
        return Err(exit(
            1,
            format!("layr does not detach a line. Use 'layr reset --hard {target}' to move this line, or 'layr branch <name> {target}' for a new line."),
        ));
    }
    let p = super::normalize(&h.loc.sub, &target)?;
    work::restore_impl(ctx, &h, &line, &[p], false, true, None, false, false)
}

/// Give a line a working folder on this machine (after a move or a restore).
pub fn adopt(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().value("--state").flag("--head");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let name = a.pos.first().ok_or_else(|| exit(128, "usage: layr adopt <line> [--state <rev>|--head]"))?;
    let l = repo.line(name)?;
    repo.authorize_line(ctx, &l, crate::repo::LineOp::Manage)?;
    let _lock = repo.p.lock()?;
    let w = repo.working(&l);
    if w.exists() {
        return Err(exit(1, format!("line '{name}' already has a working folder here: {}", w.display())));
    }
    let start = match a.get("--state") {
        Some(r) => revs::resolve(repo, Some(&l), r)?,
        None if a.has("--head") => l.head.clone(),
        None => repo
            .p
            .db
            .line_states(&l.id, StateKind::Auto)?
            .into_iter()
            .find(|s| repo.p.state_path(&s.id).exists())
            .map(|s| s.id)
            .unwrap_or(l.head.clone()),
    };
    let mut moved = l.clone();
    moved.machine = repo.p.store.machine.id.clone();
    repo.materialize(&moved, &start, &w)?;
    crate::gitbridge::sync_store_access(repo)?;
    let mut rec = Record::new("line.adopt");
    rec.lines.insert(
        l.id.clone(),
        LineChange {
            before: Some(repo.line_state(&l, &l.head)),
            after: Some(repo.line_state(&moved, &l.head)),
            working_before: None,
            working_after: Some(start.clone()),
        },
    );
    rec.data = serde_json::json!({"name": l.name, "from_state": start});
    repo.commit_record(ctx, rec)?;
    outln!(ctx, "Line {} now has a working folder here: {} (content of {})", l.name, w.display(), ids::short(&start));
    Ok(0)
}

pub fn tag(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .flag("-a|--annotate")
        .value("-m|--message")
        .flag("-d|--delete")
        .flag("-l|--list")
        .flag("-f|--force")
        .optional("-n");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = h.line.clone();
    if a.has("-d") {
        repo.require(ctx, "tag")?;
        let _lock = repo.p.lock()?;
        for t in &a.pos {
            let id = repo.p.db.tag(t)?.ok_or_else(|| exit(1, format!("error: tag '{t}' not found.")))?;
            let mut rec = Record::new("tag.delete");
            rec.data = serde_json::json!({"name": t, "state": id});
            repo.commit_record(ctx, rec)?;
            outln!(ctx, "Deleted tag '{t}' (was {})", ids::short(&id));
        }
        return Ok(0);
    }
    if a.pos.is_empty() || a.has("-l") {
        for (t, _s, m) in repo.p.db.tags()? {
            if let Some(p) = a.pos.first() {
                if !super::glob(p, &t) {
                    continue;
                }
            }
            if a.has("-n") {
                outln!(ctx, "{:<15} {}", t, m.unwrap_or_default());
            } else {
                outln!(ctx, "{t}");
            }
        }
        return Ok(0);
    }
    let name = &a.pos[0];
    crate::store::check_name("tag", name)?;
    // Tags are names of the whole project and keep their states from gc: the admin sets them.
    repo.require(ctx, "tag")?;
    let id = revs::resolve(repo, line.as_ref(), a.pos.get(1).map(|s| s.as_str()).unwrap_or("HEAD"))?;
    let _lock = repo.p.lock()?;
    if repo.p.db.tag(name)?.is_some() && !a.has("-f") {
        return Err(exit(128, format!("fatal: tag '{name}' already exists")));
    }
    let mut rec = Record::new("tag");
    rec.data = serde_json::json!({"name": name, "state": id, "message": a.get("-m")});
    repo.commit_record(ctx, rec)?;
    Ok(0)
}
