//! merge, cherry-pick, revert, rebase and exploration groups.

use super::{exit, here, work};
use crate::args::{parse, Spec};
use crate::changes;
use crate::ctx::Ctx;
use crate::diff;
use crate::ids;
use crate::merge;
use crate::model::{Line, Record, StateKind, StateRec};
use crate::repo::{skip_path, Repo};
use crate::revs;
use anyhow::{bail, Result};

/// Merge `theirs` into `ours` with `base`. Returns the new state (snapshot written, no record)
/// and the merge outcome.
#[allow(clippy::too_many_arguments)]
pub fn merge_states(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    base: &str,
    ours: &str,
    theirs: &str,
    theirs_label: &str,
    parents: Vec<String>,
    message: &str,
) -> Result<(StateRec, merge::Outcome)> {
    let base_root = repo.state_root(base)?;
    let ours_root = repo.state_root(ours)?;
    let theirs_root = repo.state_root(theirs)?;
    let work = repo.temp_work(&ours_root)?;
    let cfg = |k: &str| repo.p.config_value(k).ok().flatten().and_then(|v| v.as_str().map(|s| s.to_string()));
    let out = merge::merge(&merge::Input {
        base: &base_root,
        ours: &ours_root,
        theirs: &theirs_root,
        result: &work.path,
        base_id: base,
        ours_id: ours,
        theirs_id: theirs,
        ours_label: "HEAD",
        theirs_label,
        config: &cfg,
        run_as: Some(repo.lead_ids()),
        allow_root: ctx.caller.is_root(),
    })?;
    let own = repo.own(line);
    if let Some((u, g)) = own {
        for p in &out.changed {
            if let Ok(full) = crate::fsutil::safe_join(&work.path, p) {
                if full.symlink_metadata().is_ok() {
                    crate::fsutil::chown_nofollow(&full, u, g)?;
                }
            }
        }
    }
    let mut st = repo.new_state(ctx, &work.path, StateKind::Commit, parents, message, Some(line))?;
    st.conflicts = out.conflicts.clone();
    Ok((st, out))
}

/// Move the line to `new_head` and update the working folder in place.
fn finish(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    op: &str,
    new_head: &str,
    states: Vec<StateRec>,
    working_before: Option<String>,
    local: &std::collections::BTreeSet<String>,
    data: serde_json::Value,
) -> Result<Vec<changes::Change>> {
    let old_root = repo.state_root(&line.head)?;
    let new_root = repo.state_root(new_head)?;
    let wdir = repo.working(line);
    let applied = match repo.apply_delta(&old_root, &new_root, &wdir, repo.own(line), local, false) {
        Ok(v) => v,
        Err(e) => {
            repo.discard_states(&states);
            return Err(e);
        }
    };
    repo.move_head(ctx, op, line, new_head, states, working_before, data)?;
    let mut l2 = line.clone();
    l2.head = new_head.to_string();
    if let Err(e) = crate::gitbridge::refresh_git_layer(repo, &l2) {
        eprintln!("warning: .git layer not updated: {e:#}");
    }
    Ok(applied)
}

fn print_stat(ctx: &Ctx, repo: &Repo, from: &str, to: &str) -> Result<()> {
    let a = repo.state_root(from)?;
    let b = repo.state_root(to)?;
    let list: Vec<changes::Change> = changes::file_changes(&a, &b)?.into_iter().filter(|c| !skip_path(&c.path)).collect();
    let fds = diff::file_diffs(&a, &b, &list)?;
    ctx.print(&diff::stat(&fds, 80));
    for f in &fds {
        match (&f.old, &f.new) {
            (None, Some(n)) => outln!(ctx, " create mode {:06o} {}", n.mode, n.path),
            (Some(o), None) => outln!(ctx, " delete mode {:06o} {}", o.mode, o.path),
            _ => {}
        }
    }
    Ok(())
}

/// Group rule: a line of an exploration group whose epoch is older than the group epoch is stale.
fn check_group(repo: &Repo, theirs_line: Option<&Line>) -> Result<Option<String>> {
    let l = match theirs_line {
        Some(l) => l,
        None => return Ok(None),
    };
    let g = match &l.group {
        Some(g) => g.clone(),
        None => return Ok(None),
    };
    let (epoch, _, accepted) = match repo.p.db.group(&g)? {
        Some(x) => x,
        None => return Ok(None),
    };
    let le = repo.p.db.group_line_epoch(&g, &l.id)?.unwrap_or(0);
    if le < epoch {
        bail!(
            "line '{}' is stale: group '{g}' already accepted {} (epoch {epoch}).\nArchive it with 'layr branch -D {}' or start a new attempt from the new main state.",
            l.name,
            accepted.and_then(|a| repo.p.db.line_by_id(&a).ok().flatten()).map(|x| x.name).unwrap_or_else(|| "another line".into()),
            l.name
        );
    }
    Ok(Some(g))
}

pub fn merge(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .value("-m|--message")
        .flag("--no-ff")
        .flag("--ff")
        .flag("--ff-only")
        .flag("--squash")
        .value("--expect")
        .flag("--abort")
        .flag("--continue")
        .flag("--no-edit")
        .flag("--no-commit")
        .flag("-q|--quiet")
        .flag("--stat")
        .flag("--no-stat")
        .value("-s|--strategy")
        .value("-X|--strategy-option");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let line = h.line()?.clone();
    if a.has("--abort") {
        return super::admin::undo_last_of(ctx, &h.repo, &line, &["merge", "pull"]);
    }
    if a.has("--continue") {
        return Err(exit(1, "layr merges never stop on a conflict: fix the files and run 'layr commit'."));
    }
    let rev = a.pos.first().ok_or_else(|| exit(128, "usage: layr merge <rev> [--expect <state>]"))?.clone();
    merge_rev(ctx, &h.repo, &line, &rev, &a)
}

pub fn merge_rev(ctx: &Ctx, repo: &Repo, line: &Line, rev: &str, a: &crate::args::Parsed) -> Result<i32> {
    let theirs = revs::resolve(repo, Some(line), rev)?;
    let theirs_line = repo.p.db.line_by_name(rev)?;
    if let Some(e) = a.get("--expect") {
        let want = revs::resolve(repo, Some(line), e)?;
        if want != theirs {
            return Err(exit(
                1,
                format!(
                    "error: {rev} is at {} but --expect names {}: the line changed after the review; review the new state first",
                    ids::short(&theirs),
                    ids::short(&want)
                ),
            ));
        }
    }
    let group = check_group(repo, theirs_line.as_ref())?;
    let (_lock, w) = work::begin_change(ctx, repo, line)?;
    let line = repo.line(&line.id)?;
    let head = line.head.clone();
    if theirs == head || repo.is_ancestor(&theirs, &head)? {
        outln!(ctx, "Already up to date.");
        return Ok(0);
    }
    let (st, _scan, _it, _idx) = repo.status(&line)?;
    if !st.staged.is_empty() {
        return Err(exit(1, "error: you have staged changes; commit or stash them before merging."));
    }
    let local = st.local_paths();
    let ff = repo.is_ancestor(&head, &theirs)?;
    let quiet = a.has("-q");
    let group_rec = |ctx: &Ctx| -> Result<()> {
        if let (Some(g), Some(tl)) = (&group, &theirs_line) {
            let mut rec = Record::new("group.accept");
            rec.data = serde_json::json!({"name": g, "line": tl.id, "state": theirs});
            repo.commit_record(ctx, rec)?;
        }
        Ok(())
    };
    if ff && !a.has("--no-ff") && !a.has("--squash") {
        finish(
            ctx,
            repo,
            &line,
            "merge",
            &theirs,
            vec![],
            w,
            &local,
            serde_json::json!({"theirs": theirs, "fast_forward": true}),
        )?;
        group_rec(ctx)?;
        if !quiet {
            outln!(ctx, "Updating {}..{}\nFast-forward", ids::short(&head), ids::short(&theirs));
            print_stat(ctx, repo, &head, &theirs)?;
        }
        return Ok(0);
    }
    if a.has("--ff-only") {
        return Err(exit(128, "fatal: Not possible to fast-forward, aborting."));
    }
    let base = match repo.merge_base(&head, &theirs)? {
        Some(b) => b,
        None => return Err(exit(128, format!("fatal: refusing to merge unrelated histories ({rev})"))),
    };
    let label = theirs_line.as_ref().map(|l| l.name.clone()).unwrap_or_else(|| rev.to_string());
    let message = a.get("-m").map(|s| s.to_string()).unwrap_or_else(|| {
        if theirs_line.is_some() {
            format!("Merge line '{label}'")
        } else {
            format!("Merge {label}")
        }
    });
    let parents = if a.has("--squash") { vec![head.clone()] } else { vec![head.clone(), theirs.clone()] };
    let (state, out) = merge_states(ctx, repo, &line, &base, &head, &theirs, &label, parents, &message)?;
    let sid = state.id.clone();
    let nconf = out.conflicts.len();
    finish(
        ctx,
        repo,
        &line,
        "merge",
        &sid,
        vec![state],
        w,
        &local,
        serde_json::json!({"theirs": theirs, "base": base, "squash": a.has("--squash"), "conflicts": nconf}),
    )?;
    group_rec(ctx)?;
    for m in &out.messages {
        outln!(ctx, "{m}");
    }
    if nconf > 0 {
        outln!(
            ctx,
            "Automatic merge left {nconf} conflict(s) in state {}: fix them, then 'layr add' and 'layr commit'.",
            ids::short(&sid)
        );
        return Ok(1);
    }
    if !quiet {
        outln!(ctx, "Merge made by the 'layr' strategy: {}", ids::short(&sid));
        print_stat(ctx, repo, &head, &sid)?;
    }
    Ok(0)
}

fn pick(ctx: &Ctx, args: &[String], revert: bool) -> Result<i32> {
    let spec = Spec::new()
        .flag("-n|--no-commit")
        .flag("-x")
        .flag("--no-edit")
        .flag("--abort")
        .flag("--continue")
        .value("-m|--mainline")
        .flag("-e|--edit")
        .flag("-q|--quiet");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let repo = &h.repo;
    if a.has("--abort") {
        return super::admin::undo_last_of(ctx, repo, &line, &[if revert { "revert" } else { "cherry-pick" }]);
    }
    if a.pos.is_empty() {
        return Err(exit(128, "usage: layr cherry-pick <rev>..."));
    }
    let mut picks = Vec::new();
    for r in &a.pos {
        if let Some((x, y)) = r.split_once("..") {
            let xi = revs::resolve(repo, Some(&line), x)?;
            let yi = revs::resolve(repo, Some(&line), y)?;
            let mut chain = repo.first_parent_chain(&yi, Some(&xi))?;
            chain.reverse();
            picks.extend(chain);
        } else {
            picks.push(revs::resolve(repo, Some(&line), r)?);
        }
    }
    let (_lock, w) = work::begin_change(ctx, repo, &line)?;
    let mut line = repo.line(&line.id)?;
    let (st, _scan, _it, _idx) = repo.status(&line)?;
    if !st.staged.is_empty() {
        return Err(exit(1, "error: you have staged changes; commit or stash them first."));
    }
    let local = st.local_paths();
    let mut code = 0;
    let mut first_w = w;
    for p in picks {
        let c = repo.state(&p)?;
        let parent = match c.parents.first() {
            Some(x) => x.clone(),
            None => return Err(exit(128, format!("fatal: {} has no parent", ids::short(&p)))),
        };
        let (base, theirs) = if revert { (p.clone(), parent.clone()) } else { (parent.clone(), p.clone()) };
        let subject = c.message.lines().next().unwrap_or("").to_string();
        let message = if revert {
            format!("Revert \"{subject}\"\n\nThis reverts state {}.", ids::display(&p))
        } else if a.has("-x") {
            format!("{}\n\n(cherry picked from state {})", c.message, ids::display(&p))
        } else {
            c.message.clone()
        };
        let label = ids::short(&p);
        let (mut state, out) =
            merge_states(ctx, repo, &line, &base, &line.head, &theirs, &label, vec![line.head.clone()], &message)?;
        if !revert {
            state.author = c.author.clone();
        }
        let sid = state.id.clone();
        let n = out.conflicts.len();
        finish(
            ctx,
            repo,
            &line,
            if revert { "revert" } else { "cherry-pick" },
            &sid,
            vec![state],
            first_w.take(),
            &local,
            serde_json::json!({"source": p}),
        )?;
        line = repo.line(&line.id)?;
        for m in &out.messages {
            outln!(ctx, "{m}");
        }
        if n > 0 {
            outln!(
                ctx,
                "{} {label} left {n} conflict(s) in state {}: fix them and commit.",
                if revert { "Revert of" } else { "Cherry-pick of" },
                ids::short(&sid)
            );
            code = 1;
        } else if !a.has("-q") {
            outln!(ctx, "[{} {}] {}", line.name, ids::short(&sid), message.lines().next().unwrap_or(""));
        }
    }
    Ok(code)
}

pub fn cherry_pick(ctx: &Ctx, args: &[String]) -> Result<i32> {
    pick(ctx, args, false)
}

pub fn revert(ctx: &Ctx, args: &[String]) -> Result<i32> {
    pick(ctx, args, true)
}

pub fn rebase(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .value("--onto")
        .flag("--abort")
        .flag("--continue")
        .flag("-i|--interactive")
        .flag("-q|--quiet")
        .flag("--autostash");
    let a = parse(&spec, args)?;
    if a.has("-i") {
        return Err(exit(1, "interactive rebase is not supported"));
    }
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let repo = &h.repo;
    if a.has("--abort") {
        return super::admin::undo_last_of(ctx, repo, &line, &["rebase"]);
    }
    if a.has("--continue") {
        return Err(exit(1, "layr rebase never stops on a conflict: fix the files and run 'layr commit'."));
    }
    let up = a.pos.first().ok_or_else(|| exit(128, "usage: layr rebase [--onto <newbase>] <upstream>"))?;
    let upstream = revs::resolve(repo, Some(&line), up)?;
    let onto = match a.get("--onto") {
        Some(o) => revs::resolve(repo, Some(&line), o)?,
        None => upstream.clone(),
    };
    let (_lock, w) = work::begin_change(ctx, repo, &line)?;
    let line = repo.line(&line.id)?;
    let (st, _scan, _it, _idx) = repo.status(&line)?;
    if !st.clean() {
        return Err(exit(1, "error: cannot rebase: you have unstaged or staged changes.\nPlease commit or stash them."));
    }
    let base = repo.merge_base(&line.head, &upstream)?.ok_or_else(|| exit(128, "fatal: no common ancestor"))?;
    if repo.is_ancestor(&upstream, &line.head)? && onto == upstream {
        outln!(ctx, "Current line {} is up to date.", line.name);
        return Ok(0);
    }
    let mut chain = repo.first_parent_chain(&line.head, Some(&base))?;
    chain.reverse();
    let mut cur = onto.clone();
    let mut states = Vec::new();
    let mut conflicts = 0;
    for p in &chain {
        let c = repo.state(p)?;
        if c.parents.len() > 1 {
            continue;
        }
        let parent = c.parents[0].clone();
        let (mut s, out) = merge_states(ctx, repo, &line, &parent, &cur, p, &ids::short(p), vec![cur.clone()], &c.message)?;
        s.author = c.author.clone();
        conflicts += out.conflicts.len();
        for m in &out.messages {
            outln!(ctx, "{m}");
        }
        cur = s.id.clone();
        // Later picks read this state: write its record now, as part of the rebase.
        let mut rec = Record::new("rebase.step");
        rec.states.push(s.clone());
        repo.commit_record(ctx, rec)?;
        states.push(s);
    }
    let local = st.local_paths();
    finish(
        ctx,
        repo,
        &line,
        "rebase",
        &cur,
        vec![],
        w,
        &local,
        serde_json::json!({"upstream": upstream, "onto": onto, "count": states.len()}),
    )?;
    if conflicts > 0 {
        outln!(
            ctx,
            "Rebased {} state(s) onto {} with {conflicts} conflict(s): fix them and commit.",
            states.len(),
            ids::short(&onto)
        );
        return Ok(1);
    }
    outln!(ctx, "Successfully rebased {} onto {}.", line.name, ids::short(&onto));
    Ok(0)
}

pub fn group(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let h = here(ctx)?;
    let repo = &h.repo;
    let sub = args.first().map(|s| s.as_str()).unwrap_or("list");
    let rest = args.get(1..).unwrap_or(&[]);
    match sub {
        "create" => {
            repo.require_lead(ctx, "create exploration groups")?;
            let name = rest.first().ok_or_else(|| exit(128, "usage: layr group create <name> [<base>]"))?;
            crate::store::check_name("group", name)?;
            let base = revs::resolve(repo, h.line.as_ref(), rest.get(1).map(|s| s.as_str()).unwrap_or("HEAD"))?;
            let _lock = repo.p.lock()?;
            if repo.p.db.group(name)?.is_some() {
                return Err(exit(128, format!("fatal: group '{name}' exists")));
            }
            let mut rec = Record::new("group.create");
            rec.data = serde_json::json!({"name": name, "base": base});
            repo.commit_record(ctx, rec)?;
            outln!(ctx, "Created group {name} at {}. Add lines with 'layr branch <name> --group {name}'.", ids::short(&base));
            Ok(0)
        }
        "add" => {
            let (g, l) = match (rest.first(), rest.get(1)) {
                (Some(g), Some(l)) => (g, l),
                _ => return Err(exit(128, "usage: layr group add <group> <line>")),
            };
            let line = repo.line(l)?;
            repo.can_write_line(ctx, &line)?;
            if repo.p.db.group(g)?.is_none() {
                return Err(exit(128, format!("fatal: no group '{g}'")));
            }
            let _lock = repo.p.lock()?;
            let mut rec = Record::new("group.add");
            rec.data = serde_json::json!({"name": g, "line": line.id});
            let mut after = repo.line_state(&line, &line.head);
            after.group = Some(g.clone());
            rec.lines.insert(
                line.id.clone(),
                crate::model::LineChange {
                    before: Some(repo.line_state(&line, &line.head)),
                    after: Some(after),
                    working_before: None,
                    working_after: None,
                },
            );
            repo.commit_record(ctx, rec)?;
            Ok(0)
        }
        "show" | "list" => {
            for (name, epoch, base, accepted) in repo.p.db.groups()? {
                if sub == "show" && rest.first().map(|r| r != &name).unwrap_or(false) {
                    continue;
                }
                let acc =
                    accepted.and_then(|a| repo.p.db.line_by_id(&a).ok().flatten()).map(|l| l.name).unwrap_or_else(|| "-".into());
                outln!(ctx, "{name}\tepoch {epoch}\tbase {}\taccepted {acc}", ids::short(&base));
                for (lid, e) in repo.p.db.group_lines(&name)? {
                    if let Some(l) = repo.p.db.line_by_id(&lid)? {
                        outln!(ctx, "\t{}\t{}", l.name, if e < epoch { "stale" } else { "active" });
                    }
                }
            }
            Ok(0)
        }
        _ => Err(exit(128, "usage: layr group (create|add|list|show)")),
    }
}
