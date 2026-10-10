//! save, undo, op log, try, checks, gc, fsck, reindex, init, projects, config, layer.

use super::{exit, here};
use crate::args::{parse, Spec};
use crate::btrfs;
use crate::changes;
use crate::ctx::Ctx;
use crate::fsutil;
use crate::ids;
use crate::model::{Line, LineChange, Record, StateKind};
use crate::repo::{skip_path, Repo};
use crate::revs;
use crate::store::Project;
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub fn save(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec =
        Spec::new().value("-m|--message").flag("--running").flag("--force").flag("--layers").flag("--all").flag("-q|--quiet");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let lines: Vec<Line> = if a.has("--all") {
        repo.require(ctx, "line.manage")?;
        repo.lines()?.into_iter().filter(|l| repo.working(l).exists()).collect()
    } else {
        vec![h.line()?.clone()]
    };
    let msg = a.get("-m").unwrap_or("").to_string();
    for l in lines {
        repo.authorize_line(ctx, &l, crate::repo::LineOp::Save)?;
        let _lock = repo.p.lock()?;
        let l = repo.line(&l.id)?;
        let st = repo.save(ctx, &l, &msg, a.has("--running"), a.has("--force") || a.has("--layers"))?;
        match st {
            Some(s) => {
                if a.has("--layers") {
                    // Layers saved on a protected line may warm other users' lines.
                    let shared = repo.policy().rule_for(&l.name).is_some();
                    record_layers(ctx, repo, &s.id, &repo.working(&l), repo.owner_ids(&l), shared)?;
                }
                if !a.has("-q") {
                    outln!(ctx, "{} {}", l.name, ids::display(&s.id));
                }
            }
            None => {
                if !a.has("-q") {
                    outln!(ctx, "{} no changes", l.name);
                }
            }
        }
    }
    Ok(0)
}

/// Snapshot the layers of a working folder (`w`) as the layers of `state` and record them
/// (`save.layers`, with their subvolume UUIDs). `shared`: they may go to other users' lines.
/// A layer folder that a tool made again as a plain folder becomes a layer first.
pub fn record_layers(
    ctx: &Ctx,
    repo: &Repo,
    state: &str,
    w: &Path,
    owner: (u32, u32),
    shared: bool,
) -> Result<BTreeMap<String, String>> {
    let mut layers = BTreeMap::new();
    for p in repo.p.config_strings("regenerable") {
        let _ = crate::repo::convert_layer(w, &p, owner.0, owner.1);
        let lp = fsutil::safe_join(w, &p)?;
        let name = format!("{state}@{}", p.replace('/', "%2F"));
        if btrfs::is_subvolume(&lp) && !repo.p.layer_path(state, &name).exists() {
            btrfs::snapshot(&lp, &repo.p.layer_path(state, &name), true)?;
            layers.insert(p, name);
        }
    }
    if layers.is_empty() {
        return Ok(layers);
    }
    let mut uuids = serde_json::Map::new();
    for snap in layers.values() {
        let info = btrfs::subvol_info(&repo.p.layer_path(state, snap))?;
        uuids.insert(snap.clone(), serde_json::Value::String(info.uuid));
    }
    let mut rec = Record::new("save.layers");
    rec.data = serde_json::json!({"state": state, "layers": layers, "uuids": uuids, "shared": shared});
    if let Err(e) = repo.commit_record(ctx, rec) {
        for snap in layers.values() {
            let _ = btrfs::delete_tree(&repo.p.layer_path(state, snap));
        }
        return Err(e);
    }
    Ok(layers)
}

/// Undo the newest operation of the given kinds on a line.
pub fn undo_last_of(ctx: &Ctx, repo: &Repo, line: &Line, ops: &[&str]) -> Result<i32> {
    for r in repo.p.db.records(500)? {
        if r.lines.contains_key(&line.id) && ops.contains(&r.op.as_str()) && !repo.p.db.undone(&r.id)? {
            return undo_record(ctx, repo, &r.id, false);
        }
    }
    Err(exit(1, format!("error: no {} to undo", ops.join(" or "))))
}

const NOT_UNDOABLE: &[&str] = &[
    "line.adopt",
    "machine.trust",
    "backup",
    "save",
    "save.layers",
    "check",
    "gc",
    "machine",
    "project",
    "export",
    "import",
    "mirror",
    "slot",
    "group.create",
    "group.add",
    "group.accept",
    "merge.candidate",
    "approve",
    "approve.withdraw",
    "rebase.step",
    "remote.ref",
    "config",
];

pub fn undo(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("-f|--force").flag("-n|--dry-run");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let id = match a.pos.first() {
        Some(x) => find_record(repo, x)?,
        None => {
            // The newest operation of this caller (user and session label) in the project.
            let session = ctx.session();
            let mut found = None;
            for r in repo.p.db.records(5000)? {
                let local = r.lines.keys().all(|lid| {
                    repo.p.db.line_by_id(lid).ok().flatten().map(|l| repo.p.line_path(&l.name).exists()).unwrap_or(false)
                        || r.op == "line.delete"
                });
                if r.actor.uid == ctx.caller.uid && r.actor.session == session && local && !NOT_UNDOABLE.contains(&r.op.as_str())
                {
                    found = Some(r.id.clone());
                    break;
                }
            }
            found.ok_or_else(|| exit(1, "error: nothing to undo"))?
        }
    };
    if a.has("-n") {
        let r = repo.p.db.record(&id)?.unwrap();
        outln!(ctx, "would undo {} {} ({})", short_op(&r.id), r.op, fmt_time(r.time));
        return Ok(0);
    }
    undo_record(ctx, repo, &id, a.has("-f"))
}

fn find_record(repo: &Repo, prefix: &str) -> Result<String> {
    for r in repo.p.db.records(100000)? {
        if short_op(&r.id).starts_with(prefix) || r.id.starts_with(prefix) || ids::display(&r.id).starts_with(prefix) {
            return Ok(r.id);
        }
    }
    Err(exit(128, format!("fatal: no operation {prefix}")))
}

fn short_op(id: &str) -> String {
    ids::display(id)[..12].to_string()
}

fn fmt_time(ms: i64) -> String {
    super::history::fmt_date(ms, "iso")
}

pub fn undo_record(ctx: &Ctx, repo: &Repo, id: &str, force: bool) -> Result<i32> {
    let r = repo.p.db.record(id)?.ok_or_else(|| exit(128, "fatal: unknown operation"))?;
    if r.op == "export" {
        return Err(exit(1, "error: a push cannot be undone here: the commit is on the remote. Push a revert ('layr revert <state>' then 'layr push')."));
    }
    if NOT_UNDOABLE.contains(&r.op.as_str()) {
        return Err(exit(1, format!("error: operation '{}' has nothing to undo", r.op)));
    }
    // Permissions: undo changes every line the operation changed, so the caller must be allowed
    // to change each of them. An operation without lines (tags, groups) needs the admin or the
    // user who made it.
    for (lid, ch) in &r.lines {
        let owner = repo.p.db.line_by_id(lid)?.map(|l| l.owner).or(ch.before.as_ref().map(|b| b.owner)).unwrap_or(0);
        let probe = Line {
            id: lid.clone(),
            name: ch.before.as_ref().or(ch.after.as_ref()).map(|s| s.name.clone()).unwrap_or_default(),
            head: String::new(),
            owner,
            machine: String::new(),
        };
        repo.authorize_line(ctx, &probe, crate::repo::LineOp::Rewrite)?;
    }
    if r.lines.is_empty() && !repo.is_admin(ctx) && r.actor.uid != ctx.caller.uid {
        return Err(exit(1, "permission denied: only the project admin or the user who made it can undo this operation"));
    }
    let _lock = repo.p.lock()?;
    let mut rec = Record::new("undo");
    rec.data = serde_json::json!({"undo": r.id, "op": r.op});
    // Tags.
    match r.op.as_str() {
        "tag" => {
            rec.op = "tag.delete".into();
            rec.data = serde_json::json!({"name": r.data["name"], "state": r.data["state"], "undo": r.id});
        }
        "tag.delete" => {
            rec.op = "tag".into();
            rec.data = serde_json::json!({"name": r.data["name"], "state": r.data["state"], "undo": r.id});
        }
        "stash.drop" => {
            bail!(
                "a dropped stash can be applied again with 'layr stash apply {}'",
                r.data["state"].as_str().map(ids::short).unwrap_or_default()
            );
        }
        _ => {}
    }
    for (lid, ch) in &r.lines {
        let cur = repo.p.db.line_by_id(lid)?;
        let cur_state = cur
            .as_ref()
            .filter(|_| repo.p.db.line_by_name(&cur.as_ref().unwrap().name).ok().flatten().is_some())
            .map(|l| repo.line_state(l, &l.head));
        let alive = cur
            .as_ref()
            .map(|l| repo.p.db.line_by_name(&l.name).ok().flatten().map(|x| x.id == l.id).unwrap_or(false))
            .unwrap_or(false);
        let cur_state = if alive { cur_state } else { None };
        if !force && cur_state.as_ref().map(|s| (&s.name, &s.head)) != ch.after.as_ref().map(|s| (&s.name, &s.head)) {
            return Err(exit(
                1,
                "error: the line changed after this operation; undo the later operations first (layr op log), or use --force",
            ));
        }
        // Required states must still exist (retention can delete old automatic states).
        for s in [ch.before.as_ref().map(|b| b.head.clone()), ch.working_before.clone()].into_iter().flatten() {
            if !repo.p.state_path(&s).exists() {
                return Err(exit(
                    1,
                    format!(
                        "error: cannot undo: state {} was deleted by retention (undo window: {} days)",
                        ids::short(&s),
                        repo.p.config_u64("undo.days", 7)
                    ),
                ));
            }
        }
        let mut wb = None;
        match (&ch.before, &cur) {
            (Some(b), Some(l)) if alive => {
                // Save the current content, then restore the old pointers and content.
                wb = repo.working_state(ctx, l, "before undo")?;
                if b.name != l.name {
                    std::fs::rename(repo.p.line_path(&l.name), repo.p.line_path(&b.name))?;
                }
                let mut l2 = l.clone();
                l2.name = b.name.clone();
                match (&ch.working_before, &ch.working_after) {
                    // Revert only what the operation changed; later work stays.
                    (Some(before), Some(after)) if repo.p.state_path(after).exists() => {
                        let cur = repo.scan(&l2)?;
                        let later: BTreeSet<String> = changes::file_changes(&repo.state_root(after)?, &cur.path)?
                            .into_iter()
                            .flat_map(|c| [Some(c.path.clone()), c.old_path.clone()])
                            .flatten()
                            .collect();
                        repo.apply_delta(
                            &repo.state_root(after)?,
                            &repo.state_root(before)?,
                            &repo.working(&l2),
                            repo.own(&l2),
                            &later,
                            force,
                        )?;
                    }
                    (Some(before), _) => restore_working(repo, &l2, before)?,
                    (None, _) => {}
                }
                repo.drop_stage(&l2)?;
            }
            (Some(b), _) => {
                // The line was deleted: make it again from its last content.
                let src = ch
                    .working_before
                    .clone()
                    .or(r.data.get("final_state").and_then(|v| v.as_str()).map(|s| s.to_string()))
                    .unwrap_or(b.head.clone());
                let w = repo.p.line_path(&b.name);
                if w.exists() {
                    bail!("{} exists", w.display());
                }
                let l = Line {
                    id: lid.clone(),
                    name: b.name.clone(),
                    head: b.head.clone(),
                    owner: b.owner,
                    machine: repo.p.store.machine.id.clone(),
                };
                repo.materialize(&l, &src, &w)?;
            }
            (None, Some(l)) if alive => {
                // The operation created the line: delete it (content saved first).
                wb = repo.working_state(ctx, l, "before undo")?;
                btrfs::delete_tree(&repo.working(l))?;
                repo.drop_stage(l)?;
            }
            _ => {}
        }
        rec.lines.insert(
            lid.clone(),
            LineChange {
                before: cur_state,
                after: ch.before.clone(),
                working_before: wb,
                working_after: ch.working_before.clone(),
            },
        );
    }
    let done = repo.commit_record(ctx, rec)?;
    crate::gitbridge::sync_store_access(repo)?;
    outln!(ctx, "Undid {} {} ({}); undo it with 'layr undo {}'", short_op(&r.id), r.op, fmt_time(r.time), short_op(&done.id));
    Ok(0)
}

/// Make the working folder equal to a state (all paths, untracked and ignored included).
fn restore_working(repo: &Repo, line: &Line, state: &str) -> Result<()> {
    let target = repo.state_root(state)?;
    let scan = repo.scan(line)?;
    let w = repo.working(line);
    let own = repo.own(line);
    for c in changes::file_changes(&scan.path, &target)? {
        if skip_path(&c.path) {
            continue;
        }
        if let Some(src) = &c.old_path {
            fsutil::remove_entry(&w, src)?;
        }
        if c.new.is_none() {
            fsutil::remove_entry(&w, &c.path)?;
        } else {
            fsutil::copy_entry(&target, &c.path, &w, &c.path, own)?;
        }
    }
    Ok(())
}

pub fn op(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("log");
    if sub == "export" {
        let h = here(ctx)?;
        h.repo.require(ctx, "records")?;
        ctx.print(&h.repo.p.export_jsonl()?);
        return Ok(0);
    }
    if sub == "import" {
        let h = here(ctx)?;
        h.repo.require(ctx, "records")?;
        let data = super::work::read_caller_file(ctx, args.get(1).map(|s| s.as_str()).unwrap_or("-"), 512 << 20)?;
        let lines: Vec<String> =
            String::from_utf8_lossy(&data).lines().filter(|l| !l.trim().is_empty()).map(|l| l.to_string()).collect();
        let _lock = h.repo.p.lock()?;
        let n = h.repo.p.import_lines(&lines, crate::store::Trust::Known)?;
        outln!(ctx, "imported {n} new record(s) of {}", lines.len());
        return Ok(0);
    }
    if sub != "log" && sub != "show" {
        return Err(exit(
            128,
            "usage: layr op log [-n N] [--all] | layr op show <id> | layr op export | layr op import [<file>]",
        ));
    }
    let spec = Spec::new().value("-n").flag("--all").flag("--json");
    let a = parse(&spec, args.get(1..).unwrap_or(&[]))?;
    let h = here(ctx)?;
    let repo = &h.repo;
    if sub == "show" {
        let id = find_record(repo, a.pos.first().ok_or_else(|| exit(128, "usage: layr op show <id>"))?)?;
        let r = repo.p.db.record(&id)?.unwrap();
        outln!(ctx, "{}", serde_json::to_string_pretty(&r)?);
        return Ok(0);
    }
    let n: usize = a.get("-n").map(|v| v.parse()).transpose()?.unwrap_or(30);
    let line = h.line.clone();
    let mut shown = 0;
    for r in repo.p.db.records(100000)? {
        if !a.has("--all") {
            if let Some(l) = &line {
                if !r.lines.contains_key(&l.id) && r.data.get("line").and_then(|v| v.as_str()) != Some(&l.id) {
                    continue;
                }
            }
        }
        if a.has("--json") {
            outln!(ctx, "{}", serde_json::to_string(&r)?);
        } else {
            let undone = if repo.p.db.undone(&r.id)? { " (undone)" } else { "" };
            let what = r
                .states
                .first()
                .map(|s| format!(" {} {}", ids::short(&s.id), s.message.lines().next().unwrap_or("")))
                .unwrap_or_default();
            let session = r.actor.session.as_ref().map(|a| format!(" [{a}]")).unwrap_or_default();
            outln!(ctx, "{} {} {}{} {}{}{}", short_op(&r.id), fmt_time(r.time), r.actor.name, session, r.op, what, undone);
        }
        shown += 1;
        if shown >= n {
            break;
        }
    }
    Ok(0)
}

/// Run a command in a temporary writable copy of an exact state and record the result.
pub fn try_cmd(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("--keep").flag("-q|--quiet");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    repo.require(ctx, "try")?;
    let rev = a.pos.first().ok_or_else(|| exit(128, "usage: layr try <rev> -- <command>..."))?;
    if a.paths.is_empty() {
        return Err(exit(128, "usage: layr try <rev> -- <command>..."));
    }
    let state = revs::resolve(repo, h.line.as_ref(), rev)?;
    // `try` is the caller's own command: it gets the caller's environment.
    let env: Vec<(String, String)> = ctx.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    run_check_in(ctx, repo, &state, &a.paths, &a.paths.join(" "), &env, a.has("-q"), a.has("--keep"), false)
}

/// Run a configured check as the caller in a temporary copy of `state` and record its exit code
/// as a check of that state (`command` is the text recorded). The check runs the code of the
/// state, which may come from another user: it gets a clean environment (no tokens or keys of
/// the caller), only HOME, USER, LOGNAME and PATH.
///
/// After a passing check, the layers of the copy (its build output) become the layers of the
/// state, shared: the next lines from this state, and the next checks, start warm.
pub fn run_check(ctx: &Ctx, repo: &Repo, state: &str, argv: &[String], command: &str, quiet: bool) -> Result<i32> {
    run_check_in(ctx, repo, state, argv, command, &[], quiet, false, true)
}

#[allow(clippy::too_many_arguments)]
fn run_check_in(
    ctx: &Ctx,
    repo: &Repo,
    state: &str,
    argv: &[String],
    command: &str,
    env: &[(String, String)],
    quiet: bool,
    keep: bool,
    capture_layers: bool,
) -> Result<i32> {
    let name = format!(".try-{:016x}", rand::random::<u64>());
    let dir = repo.p.line_path(&name);
    let tmp_line = Line {
        id: format!("try-{name}"),
        name: name.clone(),
        head: state.to_string(),
        owner: ctx.caller.uid,
        machine: repo.p.store.machine.id.clone(),
    };
    repo.materialize(&tmp_line, state, &dir)?;
    let res = (|| -> Result<i32> {
        if !quiet {
            eprintln!("layr: running '{command}' in {} (state {})", dir.display(), ids::short(state));
        }
        let started = std::time::Instant::now();
        let mut c = std::process::Command::new(&argv[0]);
        c.args(&argv[1..]).current_dir(&dir);
        // The caller's identity (uid, groups), never the service's.
        let mut env = env.to_vec();
        env.push(("LAYR_TRY_STATE".into(), ids::display(state)));
        crate::privs::command_as(&mut c, ctx.caller.uid, &env)?;
        let st = c.status()?;
        let code = st.code().unwrap_or(128);
        let _lock = repo.p.lock()?;
        if code == 0 && capture_layers && repo.p.db.state(state)?.map(|s| s.layers.is_empty()).unwrap_or(false) {
            let owner = (ctx.caller.uid, ctx.caller.gid);
            if let Err(e) = record_layers(ctx, repo, state, &dir, owner, true) {
                eprintln!("warning: the layers of the check were not kept: {e:#}");
            }
        }
        let mut rec = Record::new("check");
        rec.data =
            serde_json::json!({"state": state, "command": command, "exit": code, "seconds": started.elapsed().as_secs_f64()});
        repo.commit_record(ctx, rec)?;
        Ok(code)
    })();
    if keep {
        eprintln!("layr try: kept {}", dir.display());
    } else {
        let _ = btrfs::delete_tree(&dir);
    }
    let code = res?;
    if !quiet {
        eprintln!("layr: state {} exit {code}", ids::short(state));
    }
    Ok(code)
}

pub fn checks(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let h = here(ctx)?;
    let state = revs::resolve(&h.repo, h.line.as_ref(), args.first().map(|s| s.as_str()).unwrap_or("HEAD"))?;
    let v = h.repo.p.db.checks(&state)?;
    if v.is_empty() {
        outln!(ctx, "no checks for {}", ids::short(&state));
        return Ok(1);
    }
    for (cmd, exitc, time, actor) in v {
        outln!(ctx, "{} {} {} exit {exitc}: {cmd}", ids::short(&state), fmt_time(time), actor);
    }
    Ok(0)
}

/// Retention. Kept: line heads and their history, tags, remote refs, stashes, the newest
/// export per branch, conflict sources, automatic states (the newest N per line and
/// one per hour for a day), and every state that a record inside the undo window names.
pub fn retention_plan(repo: &Repo) -> Result<(BTreeSet<String>, Vec<String>)> {
    let db = &repo.p.db;
    let mut roots: Vec<String> = Vec::new();
    let now = ids::now_ms();
    let keep_auto = repo.p.config_u64("retention.auto", 50) as usize;
    let hourly = repo.p.config_u64("retention.hours", 24) as i64;
    let undo_days = repo.p.config_u64("undo.days", 7) as i64;
    let mut keep: BTreeSet<String> = BTreeSet::new();
    for l in db.lines()? {
        roots.push(l.head.clone());
        for (_, h) in db.line_heads(&l.id)? {
            roots.push(h);
        }
        let autos = db.line_states(&l.id, StateKind::Auto)?;
        let mut buckets: BTreeSet<i64> = BTreeSet::new();
        for (i, s) in autos.iter().enumerate() {
            if i < keep_auto {
                keep.insert(s.id.clone());
            } else if now - s.time < hourly * 3600 * 1000 && buckets.insert(s.time / 3_600_000) {
                keep.insert(s.id.clone());
            }
        }
        for s in db.line_states(&l.id, StateKind::Stash)? {
            roots.push(s.id);
        }
    }
    for (_, s, _) in db.tags()? {
        roots.push(s);
    }
    for (_, s, _) in db.remote_refs()? {
        roots.push(s);
    }
    let mut newest_export: BTreeMap<(String, String), String> = BTreeMap::new();
    for (_, st, remote, branch, _, _) in db.exports()? {
        newest_export.insert((remote, branch), st);
    }
    roots.extend(newest_export.into_values());
    for r in db.records_since(now - undo_days * 86400 * 1000)? {
        for s in &r.states {
            keep.insert(s.id.clone());
        }
        for ch in r.lines.values() {
            for x in [
                ch.before.as_ref().map(|b| b.head.clone()),
                ch.after.as_ref().map(|b| b.head.clone()),
                ch.working_before.clone(),
                ch.working_after.clone(),
            ]
            .into_iter()
            .flatten()
            {
                keep.insert(x);
            }
        }
    }
    // History of every root.
    let mut stack = roots;
    while let Some(x) = stack.pop() {
        if !keep.insert(x.clone()) && repo.p.state_path(&x).exists() {
            // Already kept: its parents were or will be visited from the first insert.
        }
        if let Some(s) = db.state(&x)? {
            for p in &s.parents {
                if !keep.contains(p) {
                    stack.push(p.clone());
                }
            }
            for c in &s.conflicts {
                for y in [&c.base, &c.ours, &c.theirs].into_iter().flatten() {
                    keep.insert(y.clone());
                }
            }
        }
    }
    // Commits reachable from kept states (an automatic state's parent is a commit).
    let kept: Vec<String> = keep.iter().cloned().collect();
    for k in kept {
        if let Some(s) = db.state(&k)? {
            let mut stack: Vec<String> = s.parents.clone();
            while let Some(p) = stack.pop() {
                if keep.insert(p.clone()) {
                    if let Some(ps) = db.state(&p)? {
                        stack.extend(ps.parents);
                    }
                }
            }
        }
    }
    let mut delete = Vec::new();
    for (id, _) in db.present_states()? {
        if !keep.contains(&id) {
            delete.push(id);
        }
    }
    Ok((keep, delete))
}

pub fn run_gc(ctx: &Ctx, repo: &Repo, dry: bool) -> Result<Vec<String>> {
    let _lock = repo.p.lock()?;
    let (_keep, delete) = retention_plan(repo)?;
    if dry {
        return Ok(delete);
    }
    let mut deleted = Vec::new();
    for id in &delete {
        let s = repo.p.db.state(id)?;
        if let Some(s) = &s {
            for snap in s.layers.values() {
                let _ = btrfs::delete_tree(&repo.p.layer_path(id, snap));
            }
        }
        if btrfs::delete_tree(&repo.p.state_path(id)).is_ok() {
            deleted.push(id.clone());
        }
    }
    let layers = prune_layers(repo)?;
    if !deleted.is_empty() || !layers.is_empty() {
        let mut rec = Record::new("gc");
        rec.data = serde_json::json!({"deleted": deleted, "layers": layers});
        repo.commit_record(ctx, rec)?;
    }
    // Old temporary snapshots.
    for d in [repo.p.scan_dir(), repo.p.work_dir()] {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let old = e
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .map(|t| t.elapsed().map(|x| x.as_secs() > 3600).unwrap_or(false))
                    .unwrap_or(false);
                if old {
                    let _ = btrfs::delete_tree(&e.path());
                }
            }
        }
    }
    Ok(deleted)
}

/// Layer snapshots (build output) are large and only the newest ones warm new lines: they stay
/// for the heads of lines and for the newest `retention.layers` (5) states with layers. The
/// states themselves stay; only their layer snapshots go.
fn prune_layers(repo: &Repo) -> Result<Vec<String>> {
    let keep_n = repo.p.config_u64("retention.layers", 5) as usize;
    let heads: BTreeSet<String> = repo.p.db.lines()?.into_iter().map(|l| l.head).collect();
    let mut layered: Vec<(i64, String, Vec<String>)> = Vec::new();
    for (id, _) in repo.p.db.present_states()? {
        if let Some(s) = repo.p.db.state(&id)? {
            let snaps: Vec<String> = s.layers.values().filter(|n| repo.p.layer_path(&id, n).exists()).cloned().collect();
            if !snaps.is_empty() {
                layered.push((s.time, id, snaps));
            }
        }
    }
    layered.sort_by_key(|x| std::cmp::Reverse(x.0));
    let mut deleted = Vec::new();
    for (_, id, snaps) in layered.into_iter().skip(keep_n) {
        if heads.contains(&id) {
            continue;
        }
        for n in snaps {
            if btrfs::delete_tree(&repo.p.layer_path(&id, &n)).is_ok() {
                deleted.push(n);
            }
        }
    }
    Ok(deleted)
}

pub fn gc(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("-n|--dry-run").flag("-q|--quiet");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    h.repo.require(ctx, "gc")?;
    let v = run_gc(ctx, &h.repo, a.has("-n"))?;
    if !a.has("-q") {
        let verb = if a.has("-n") { "would delete" } else { "deleted" };
        outln!(ctx, "{verb} {} state(s)", v.len());
        for id in v.iter().take(50) {
            outln!(ctx, "\t{}", ids::short(id));
        }
    }
    Ok(0)
}

pub fn fsck(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("--repair");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    repo.require(ctx, "fsck")?;
    let _lock = repo.p.lock()?;
    let mut problems = 0;
    let ic: String = repo.p.db.conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if ic != "ok" {
        problems += 1;
        outln!(ctx, "database: {ic}");
    }
    match repo.p.verify_records() {
        Ok(v) => {
            for (m, n) in v {
                outln!(ctx, "records of machine {}: {n}, signatures ok", &ids::display(&m)[..8]);
            }
        }
        Err(err) => {
            problems += 1;
            outln!(ctx, "records: {err:#}");
        }
    }
    // Snapshots without a record (a crash between the snapshot and the record).
    for e in std::fs::read_dir(repo.p.states_dir())?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let id = name.split('@').next().unwrap_or(&name).to_string();
        if repo.p.db.state(&id)?.is_none() {
            problems += 1;
            if a.has("--repair") {
                btrfs::delete_tree(&e.path())?;
                outln!(ctx, "deleted snapshot without record: {name}");
            } else {
                outln!(ctx, "snapshot without record: {name} (use --repair)");
            }
        }
    }
    let mut st = repo.p.db.conn.prepare("SELECT id FROM states WHERE present=1")?;
    let ids_v: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
    drop(st);
    for id in ids_v {
        if !repo.p.state_path(&id).exists() {
            problems += 1;
            outln!(ctx, "state {} is marked present but missing", ids::short(&id));
            if a.has("--repair") {
                repo.p.db.set_present(&id, false)?;
            }
        }
    }
    for l in repo.lines()? {
        if l.machine == repo.p.store.machine.id && !repo.working(&l).exists() {
            problems += 1;
            outln!(ctx, "line {} has no working folder (layr adopt {})", l.name, l.name);
        }
    }
    outln!(ctx, "{problems} problem(s)");
    Ok(if problems > 0 { 1 } else { 0 })
}

pub fn reindex(ctx: &Ctx, _args: &[String]) -> Result<i32> {
    let h = here(ctx)?;
    h.repo.require(ctx, "fsck")?;
    let _lock = h.repo.p.lock()?;
    h.repo.p.rebuild_index()?;
    let n: i64 = h.repo.p.db.conn.query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0))?;
    outln!(ctx, "index rebuilt from {n} records");
    Ok(0)
}

fn copy_folder(ctx: &Ctx, src: &std::path::Path, dst: &std::path::Path, include_ignored: bool) -> Result<usize> {
    let rules = crate::ignore_rules::Rules::new(src);
    let mut n = 0;
    let mut entries = Vec::new();
    crate::privs::as_caller_fs(ctx, || {
        fsutil::walk(src, "", &mut |rel, m| {
            if rel == ".git" || rel.starts_with(".git/") {
                return Ok(false);
            }
            if !include_ignored && rules.ignored(rel, m.is_dir()) {
                return Ok(false);
            }
            entries.push((rel.to_string(), m.clone()));
            Ok(true)
        })
    })?;
    for (rel, m) in entries {
        if m.is_dir() {
            fsutil::make_parents(dst, &format!("{rel}/x"), None)?;
            continue;
        }
        crate::privs::as_caller_fs(ctx, || fsutil::copy_entry(src, &rel, dst, &rel, None))?;
        n += 1;
    }
    Ok(n)
}

fn caller_git(ctx: &Ctx, dir: &std::path::Path, args: &[&str]) -> Result<std::process::Output> {
    let mut command = std::process::Command::new("git");
    command.arg("-c").arg("safe.directory=*").arg("-C").arg(dir).args(args);
    crate::privs::command_as(&mut command, ctx.caller.uid, &[])?;
    Ok(command.output()?)
}

pub fn init(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .value("--from")
        .value("--git")
        .value("-b|--branch")
        .value("--line")
        .value("--owner")
        .flag("--include-ignored")
        .value("-m|--message")
        .flag("-q|--quiet");
    let a = parse(&spec, args)?;
    let name =
        a.pos.first().ok_or_else(|| exit(128, "usage: layr init <project> [--from <folder> | --git <url>] [--line main]"))?;
    let line_name = a.get("--line").unwrap_or("main").to_string();
    let owner = match a.get("--owner") {
        Some(o) if ctx.caller.is_root() => o.parse::<u32>().or_else(|_| -> Result<u32> {
            match crate::sys::user_by_name(o) {
                Some(u) => Ok(u.uid),
                None => {
                    bail!("unknown user {o}");
                }
            }
        })?,
        Some(_) => return Err(exit(128, "fatal: only root can create a project for another user")),
        None => ctx.caller.uid,
    };
    let actor = ctx.actor();
    let p = Project::create(
        &ctx.store,
        name,
        &actor,
        serde_json::json!({"access": {format!("user:{owner}"): "admin"}, "protect": {"main": {}}}),
    )?;
    let repo = Repo { p };
    let res = (|| -> Result<(String, String)> {
        let _lock = repo.p.lock()?;
        let work = repo.p.work_dir().join(ids::new_id());
        btrfs::create_subvolume(&work)?;
        let cleanup = crate::repo::Temp { path: work.clone() };
        let mut parents = Vec::new();
        let mut msg = a.get("-m").unwrap_or("initial state").to_string();
        if let Some(url) = a.get("--git") {
            set_config(ctx, &repo, "remote.origin.url", serde_json::json!(url))?;
            let refs = crate::gitbridge::fetch(ctx, &repo, "origin")?;
            let branch = a.get("-b").map(|s| s.to_string()).or_else(|| default_branch(&repo)).unwrap_or_else(|| "main".into());
            let commit = refs
                .iter()
                .find(|(b, _)| b == &format!("origin/{branch}") || b.ends_with(&format!("/{branch}")))
                .map(|x| x.1.clone());
            let commit = commit.ok_or_else(|| exit(128, format!("fatal: remote has no branch '{branch}'")))?;
            let (st, _) = crate::gitbridge::import_commit(ctx, &repo, &commit, &format!("origin/{branch}"), owner)?;
            drop(cleanup);
            return Ok((st, branch));
        }
        if let Some(from) = a.get("--from") {
            let src = if from.starts_with('/') { std::path::PathBuf::from(from) } else { ctx.cwd.join(from) };
            if src.join(".git").exists() {
                // Keep the git history: the store gets the folder's objects; the state of the
                // folder's HEAD is the parent of the folder content.
                let s = crate::gitbridge::init_store(&repo)?;
                let url = caller_git(ctx, &src, &["config", "--get", "remote.origin.url"])
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .filter(|s| !s.is_empty());
                set_config(ctx, &repo, "remote.local.url", serde_json::json!(src.to_string_lossy()))?;
                if let Some(u) = &url {
                    set_config(ctx, &repo, "remote.origin.url", serde_json::json!(u))?;
                }
                let refs = crate::gitbridge::fetch(ctx, &repo, "local")?;
                let head = caller_git(ctx, &src, &["rev-parse", "HEAD"])?;
                let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
                let branch = caller_git(ctx, &src, &["rev-parse", "--abbrev-ref", "HEAD"])?;
                let branch = String::from_utf8_lossy(&branch.stdout).trim().to_string();
                let _ = (s, refs);
                if !head.is_empty() && head != "HEAD" {
                    let (st, _) = crate::gitbridge::import_commit(ctx, &repo, &head, &format!("local/{branch}"), owner)?;
                    parents.push(st.clone());
                    // The folder content on top (uncommitted changes, if any).
                    btrfs::delete_tree(&work)?;
                    btrfs::snapshot(&repo.state_root(&st)?, &work, false)?;
                    let tmp = repo.p.work_dir().join(ids::new_id());
                    btrfs::create_subvolume(&tmp)?;
                    let _t2 = crate::repo::Temp { path: tmp.clone() };
                    copy_folder(ctx, &src, &tmp, a.has("--include-ignored"))?;
                    let snap = repo.temp_snapshot(&tmp, true)?;
                    let imported = repo.state_root(&st)?;
                    let diffs = changes::file_changes(&imported, &snap.path);
                    // Different file systems give no common lineage: compare by walking instead.
                    let _ = diffs;
                    let mut changed = false;
                    let mut seen = BTreeSet::new();
                    fsutil::walk(&snap.path, "", &mut |rel, m| {
                        if m.is_dir() {
                            return Ok(true);
                        }
                        seen.insert(rel.to_string());
                        if !fsutil::same_entry(&imported, rel, &snap.path, rel, None)?
                            || fsutil::lmeta(&imported, rel).map(|x| x.mode != m.mode).unwrap_or(true)
                        {
                            fsutil::copy_entry(&snap.path, rel, &work, rel, None)?;
                            changed = true;
                        }
                        Ok(true)
                    })?;
                    let mut gone = Vec::new();
                    fsutil::walk(&imported, "", &mut |rel, m| {
                        if !m.is_dir() && !skip_path(rel) && !seen.contains(rel) {
                            gone.push(rel.to_string());
                        }
                        Ok(true)
                    })?;
                    for g in gone {
                        fsutil::remove_entry(&work, &g)?;
                        changed = true;
                    }
                    if !changed {
                        drop(cleanup);
                        return Ok((st, branch));
                    }
                    msg = "uncommitted changes of the imported folder".into();
                } else {
                    copy_folder(ctx, &src, &work, a.has("--include-ignored"))?;
                }
            } else {
                copy_folder(ctx, &src, &work, a.has("--include-ignored"))?;
            }
        }
        let st = repo.new_state(ctx, &work, StateKind::Commit, parents, &msg, None)?;
        let mut rec = Record::new("commit");
        rec.states.push(st.clone());
        repo.commit_record(ctx, rec)?;
        drop(cleanup);
        Ok((st.id, line_name.clone()))
    })();
    let (state, _branch) = match res {
        Ok(x) => x,
        Err(e) => {
            let _ = btrfs::delete_tree(&repo.p.dir.join("states"));
            let _ = std::fs::remove_dir_all(&repo.p.dir);
            return Err(e);
        }
    };
    let _lock = repo.p.lock()?;
    let l = repo.create_line(ctx, &line_name, &state, owner)?;
    if !a.has("-q") {
        outln!(
            ctx,
            "Initialized project {name}: line {} at {} (state {})",
            l.name,
            repo.working(&l).display(),
            ids::short(&state)
        );
    } else {
        outln!(ctx, "{}", repo.working(&l).display());
    }
    Ok(0)
}

fn default_branch(repo: &Repo) -> Option<String> {
    let s = crate::gitbridge::store(repo);
    let out = crate::gitbridge::git_out(&s, &["for-each-ref", "--format=%(refname:strip=3)", "refs/remotes/origin/"]).ok()?;
    let names: Vec<&str> = out.lines().collect();
    for want in ["main", "master", "trunk"] {
        if names.contains(&want) {
            return Some(want.to_string());
        }
    }
    names.first().map(|s| s.to_string())
}

pub fn set_config(ctx: &Ctx, repo: &Repo, key: &str, value: serde_json::Value) -> Result<()> {
    let mut rec = Record::new("config");
    let mut m = serde_json::Map::new();
    m.insert(key.to_string(), value);
    rec.data = serde_json::json!({"config": m});
    repo.commit_record(ctx, rec)?;
    Ok(())
}

pub fn projects(ctx: &Ctx, _args: &[String]) -> Result<i32> {
    for n in ctx.store.project_names()? {
        match Repo::open(ctx, &n) {
            Ok(r) => {
                let lines = r.lines()?.len();
                outln!(ctx, "{n}\t{lines} line(s)\t{}", r.p.dir.display());
            }
            Err(_) => continue,
        }
    }
    Ok(0)
}

pub fn config(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("-l|--list").value("--get").value("--unset").flag("--json");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    if a.has("-l") || (a.pos.is_empty() && !a.has("--get") && !a.has("--unset")) {
        for (k, v) in repo.p.db.config_all()? {
            outln!(ctx, "{k}={v}");
        }
        return Ok(0);
    }
    if let Some(k) = a.get("--get").or(if a.pos.len() == 1 { Some(a.pos[0].as_str()) } else { None }) {
        return match repo.p.config_value(k)? {
            Some(v) => {
                match v.as_str() {
                    Some(s) => outln!(ctx, "{s}"),
                    None => outln!(ctx, "{v}"),
                }
                Ok(0)
            }
            None => Ok(1),
        };
    }
    let key = a.get("--unset").unwrap_or_else(|| a.pos.first().map(|s| s.as_str()).unwrap_or(""));
    repo.require(ctx, if policy_key(key) { "access" } else { "config" })?;
    if (key == "admin" || key == "lead") && !ctx.caller.is_root() {
        return Err(exit(1, format!("permission denied: only root changes '{key}'")));
    }
    let _lock = repo.p.lock()?;
    if let Some(k) = a.get("--unset") {
        set_config(ctx, repo, k, serde_json::Value::Null)?;
        return Ok(0);
    }
    let (k, v) = (&a.pos[0], a.pos[1..].join(" "));
    let val: serde_json::Value = serde_json::from_str(&v).unwrap_or(serde_json::Value::String(v.clone()));
    set_config(ctx, repo, k, val)?;
    if k == "backup.dir" {
        // The service writes its own backups into this folder as the user who named it.
        set_config(ctx, repo, "backup.writer", serde_json::json!(ctx.caller.uid))?;
    }
    if POLICY_KEYS.contains(&k.as_str()) {
        crate::gitbridge::sync_store_access(repo)?;
    }
    Ok(0)
}

/// Settings that decide who may do what: changing them is the `access` action.
const POLICY_KEYS: &[&str] = &["access", "grants", "deny", "protect", "members", "admin", "lead"];

/// A named check's command belongs to the protection rules that name it (`check.<name>`).
fn policy_key(key: &str) -> bool {
    POLICY_KEYS.contains(&key) || key.starts_with("check.")
}

/// `layr access`: roles and deny rules (docs/design.md, section 3).
pub fn access(ctx: &Ctx, args: &[String]) -> Result<i32> {
    use crate::access::{action_role, parse_principal, show_principal, Role, ACTIONS};
    let h = here(ctx)?;
    let repo = &h.repo;
    let sub = args.first().map(|s| s.as_str()).unwrap_or("list");
    let rest = args.get(1..).unwrap_or(&[]);
    let principal = |s: Option<&String>| -> Result<String> {
        let s =
            s.ok_or_else(|| exit(128, "usage: layr access (grant|revoke|deny|allow|check) <user|@group|uid:N|gid:N|*> ..."))?;
        parse_principal(s).ok_or_else(|| exit(128, format!("fatal: unknown user or group '{s}'")))
    };
    let pol = repo.policy();
    match sub {
        "list" => {
            for (k, r) in &pol.access {
                outln!(ctx, "{}\t{}", show_principal(k), r.name());
            }
            for (k, (r, end)) in &pol.grants {
                outln!(ctx, "{}\t{}\tuntil {}", show_principal(k), r.name(), fmt_time(*end));
            }
            for (k, v) in &pol.deny {
                outln!(ctx, "{}	deny {}", show_principal(k), v.join(" "));
            }
            Ok(0)
        }
        "check" => {
            // Explain a decision: `layr access check <principal> <action>`.
            let k = principal(rest.first())?;
            let action = rest.get(1).ok_or_else(|| exit(128, "usage: layr access check <principal> <action>"))?;
            if action_role(action).is_none() {
                return Err(exit(128, format!("fatal: unknown action '{action}'")));
            }
            let who = match k.strip_prefix("user:").and_then(|u| u.parse::<u32>().ok()) {
                Some(uid) => crate::access::Who::of(uid, crate::ctx::Caller::from_uid(uid).gid),
                None => return Err(exit(128, "fatal: check takes a user")),
            };
            match pol.check(&who, action) {
                Ok(()) => outln!(ctx, "allowed: {} is {}", show_principal(&k), pol.role(&who).name()),
                Err(e) => outln!(ctx, "refused: {e}"),
            }
            Ok(0)
        }
        "grant" | "revoke" | "deny" | "allow" => {
            repo.require(ctx, "access")?;
            let k = principal(rest.first())?;
            let _lock = repo.p.lock()?;
            let pol = repo.policy();
            match sub {
                "grant" => {
                    let role = rest.get(1).and_then(|r| Role::parse(r)).ok_or_else(|| {
                        exit(128, "usage: layr access grant <principal> (reader|writer|maintainer|admin) [--until <30m|2h|1d>]")
                    })?;
                    let mut pol = pol.clone();
                    match rest.iter().position(|x| x == "--until") {
                        Some(i) => {
                            // A role for a time adds to the principal's role and ends by itself.
                            let d = rest.get(i + 1).ok_or_else(|| exit(128, "usage: --until <30m|2h|1d>"))?;
                            pol.grants.insert(k, (role, ids::now_ms() + duration_ms(d)?));
                            set_config(ctx, repo, "grants", pol.grants_json())?;
                        }
                        None => {
                            pol.access.insert(k, role);
                            set_config(ctx, repo, "access", pol.access_json())?;
                        }
                    }
                }
                "revoke" => {
                    let mut pol = pol.clone();
                    if pol.grants.remove(&k).is_some() {
                        set_config(ctx, repo, "grants", pol.grants_json())?;
                    }
                    pol.access.remove(&k);
                    set_config(ctx, repo, "access", pol.access_json())?;
                }
                _ => {
                    let actions: Vec<String> = rest[1..].to_vec();
                    if actions.is_empty() {
                        return Err(exit(128, format!("usage: layr access {sub} <principal> <action>...")));
                    }
                    for x in &actions {
                        if x != "*" && action_role(x).is_none() {
                            let known: Vec<&str> = ACTIONS.iter().map(|(a, _)| *a).collect();
                            return Err(exit(128, format!("fatal: unknown action '{x}' (known: {})", known.join(", "))));
                        }
                    }
                    let mut d = pol.deny.clone();
                    let e = d.entry(k.clone()).or_default();
                    if sub == "deny" {
                        for x in actions {
                            if !e.contains(&x) {
                                e.push(x);
                            }
                        }
                    } else {
                        e.retain(|x| !actions.contains(x));
                    }
                    if e.is_empty() {
                        d.remove(&k);
                    }
                    set_config(ctx, repo, "deny", serde_json::to_value(d)?)?;
                }
            }
            crate::gitbridge::sync_store_access(repo)?;
            Ok(0)
        }
        _ => Err(exit(128, "usage: layr access [list|grant|revoke|deny|allow|check] ...")),
    }
}

/// `layr approve [<rev>] [--withdraw]`: approve an exact state (or withdraw the approval). A
/// protection rule can require approvals of the state a merge takes (`--approvals`).
pub fn approve(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("--withdraw").flag("-q|--quiet");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let state = revs::resolve(repo, h.line.as_ref(), a.pos.first().map(|s| s.as_str()).unwrap_or("HEAD"))?;
    let s = repo.state(&state)?;
    let _lock = repo.p.lock()?;
    let withdraw = a.has("--withdraw");
    let mut rec = Record::new(if withdraw { "approve.withdraw" } else { "approve" });
    rec.data = serde_json::json!({"state": state});
    repo.commit_record(ctx, rec)?;
    if !a.has("-q") {
        let what = if withdraw { "Withdrew the approval of" } else { "Approved" };
        outln!(ctx, "{what} state {} {}", ids::short(&state), s.message.lines().next().unwrap_or(""));
    }
    Ok(0)
}

/// `layr approvals [<rev>]`: who approved exactly this state.
pub fn approvals(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let h = here(ctx)?;
    let state = revs::resolve(&h.repo, h.line.as_ref(), args.first().map(|s| s.as_str()).unwrap_or("HEAD"))?;
    let v = h.repo.p.db.approvals(&state)?;
    for (uid, time) in &v {
        outln!(ctx, "{}\t{}", crate::ctx::Caller::from_uid(*uid).user, fmt_time(*time));
    }
    Ok(if v.is_empty() { 1 } else { 0 })
}

/// A duration: `90s`, `30m`, `2h`, `1d`.
fn duration_ms(s: &str) -> Result<i64> {
    let (n, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = n.parse().map_err(|_| exit(128, format!("fatal: bad duration '{s}' (for example 30m, 2h, 1d)")))?;
    let k = match unit {
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return Err(exit(128, format!("fatal: bad duration '{s}' (for example 30m, 2h, 1d)"))),
    };
    Ok(n.saturating_mul(k))
}

/// `layr protect`: protection rules of line name patterns.
pub fn protect(ctx: &Ctx, args: &[String]) -> Result<i32> {
    use crate::access::{Role, RuleSpec};
    let spec = Spec::new()
        .value("--update")
        .flag("--no-expect")
        .flag("--expect")
        .value("--check")
        .flag("--direct")
        .flag("--no-direct")
        .value("--rewrite")
        .value("--path")
        .value("--approvals")
        .value("--approvers")
        .flag("--remove");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let pol = repo.policy();
    let pattern = match a.pos.first() {
        None => {
            for (p, r) in &pol.protect {
                let rule = pol.rule_for(p);
                let checks = r.check.join(",");
                match rule {
                    Some(x) => {
                        let paths: Vec<String> = x.paths.iter().map(|(p, r)| format!("{p}={}", r.name())).collect();
                        outln!(
                            ctx,
                            "{p}\tupdate {}\texpect {}\tcheck {}\tdirect {}\trewrite {}{}{}",
                            x.update.name(),
                            x.expect,
                            if checks.is_empty() { "-".into() } else { checks },
                            x.direct,
                            x.rewrite.name(),
                            if paths.is_empty() { String::new() } else { format!("\tpaths {}", paths.join(",")) },
                            if x.approvals == 0 {
                                String::new()
                            } else {
                                let from: Vec<String> = x.approvers.iter().map(|k| crate::access::show_principal(k)).collect();
                                format!(
                                    "\tapprovals {} from {}",
                                    x.approvals,
                                    if from.is_empty() { "maintainers".to_string() } else { from.join(",") }
                                )
                            }
                        )
                    }
                    None => outln!(ctx, "{p}"),
                }
            }
            return Ok(0);
        }
        Some(p) => p.clone(),
    };
    repo.require(ctx, "access")?;
    let _lock = repo.p.lock()?;
    let mut all = repo.policy().protect;
    if a.has("--remove") {
        all.remove(&pattern);
    } else {
        let mut r: RuleSpec = all.get(&pattern).cloned().unwrap_or_default();
        let role = |s: &str| -> Result<String> {
            Role::parse(s).map(|r| r.name().to_string()).ok_or_else(|| exit(128, format!("fatal: unknown role '{s}'")))
        };
        if let Some(u) = a.get("--update") {
            r.update = Some(role(u)?);
        }
        if let Some(u) = a.get("--rewrite") {
            r.rewrite = Some(role(u)?);
        }
        if a.has("--no-expect") {
            r.expect = Some(false);
        }
        if a.has("--expect") {
            r.expect = Some(true);
        }
        if a.has("--direct") {
            r.direct = Some(true);
        }
        if a.has("--no-direct") {
            r.direct = Some(false);
        }
        if let Some(c) = a.get("--check") {
            r.check = c.split(',').filter(|x| !x.is_empty()).map(|x| x.to_string()).collect();
        }
        if let Some(n) = a.get("--approvals") {
            r.approvals = Some(n.parse().map_err(|_| exit(128, format!("fatal: --approvals takes a number, not '{n}'")))?);
        }
        if let Some(who) = a.get("--approvers") {
            r.approvers = Vec::new();
            for w in who.split(',').filter(|x| !x.is_empty()) {
                r.approvers.push(
                    crate::access::parse_principal(w).ok_or_else(|| exit(128, format!("fatal: unknown user or group '{w}'")))?,
                );
            }
        }
        // `--path <path>=<role>` (repeatable); an empty role removes the path rule.
        for pr in a.all("--path") {
            let (path, rl) = pr.split_once('=').ok_or_else(|| exit(128, "usage: --path <path>=<role>"))?;
            fsutil::check_rel(path.trim_end_matches('/')).map_err(|_| exit(128, format!("fatal: bad path '{path}'")))?;
            if rl.is_empty() {
                r.paths.remove(path);
            } else {
                r.paths.insert(path.to_string(), role(rl)?);
            }
        }
        all.insert(pattern, r);
    }
    set_config(ctx, repo, "protect", serde_json::to_value(all)?)?;
    Ok(0)
}

/// Regenerable layers: folders kept as nested subvolumes, outside automatic states.
pub fn layer(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let h = here(ctx)?;
    let repo = &h.repo;
    let sub = args.first().map(|s| s.as_str()).unwrap_or("list");
    match sub {
        "list" => {
            for p in repo.p.config_strings("regenerable") {
                let state = match &h.line {
                    Some(l) => {
                        let lp = repo.working(l).join(&p);
                        if btrfs::is_subvolume(&lp) {
                            "layer"
                        } else if lp.exists() {
                            "plain folder (run layr layer repair)"
                        } else {
                            "absent"
                        }
                    }
                    None => "",
                };
                outln!(ctx, "{p}\t{state}");
            }
            Ok(0)
        }
        "add" | "remove" => {
            repo.require(ctx, "config")?;
            let path = args.get(1).ok_or_else(|| exit(128, "usage: layr layer add|remove <path>"))?;
            fsutil::check_rel(path)?;
            let mut v = repo.p.config_strings("regenerable");
            if sub == "add" {
                if !v.contains(path) {
                    v.push(path.clone());
                }
            } else {
                v.retain(|x| x != path);
            }
            let _lock = repo.p.lock()?;
            set_config(ctx, repo, "regenerable", serde_json::json!(v))?;
            if sub == "add" {
                if let Some(l) = &h.line {
                    let (u, g) = repo.owner_ids(l);
                    crate::repo::convert_layer(&repo.working(l), path, u, g)?;
                }
            }
            Ok(0)
        }
        "repair" => {
            // Layer folders that a tool deleted and made again as plain folders become layers
            // again. Best at a quiet point, when no build runs in the line.
            let l = h.line()?.clone();
            repo.authorize_line(ctx, &l, crate::repo::LineOp::Save)?;
            let _lock = repo.p.lock()?;
            let l = repo.line(&l.id)?;
            let (u, g) = repo.owner_ids(&l);
            for p in repo.p.config_strings("regenerable") {
                if crate::repo::convert_layer(&repo.working(&l), &p, u, g)? {
                    outln!(ctx, "{p}: a layer again");
                }
            }
            Ok(0)
        }
        _ => Err(exit(128, "usage: layr layer (list|add|remove|repair) [<path>]")),
    }
}

pub fn machines(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let h = here(ctx)?;
    if args.first().map(|s| s.as_str()) == Some("trust") {
        let spec = Spec::new().value("--key");
        let a = parse(&spec, &args[1..])?;
        h.repo.require(ctx, "sync")?;
        let id = a.pos.first().ok_or_else(|| exit(128, "usage: layr machines trust <id> --key <public key>"))?;
        let key = a.get("--key").ok_or_else(|| exit(128, "usage: layr machines trust <id> --key <public key>"))?;
        let full = if id.len() == 36 {
            id.clone()
        } else {
            h.repo.p.db.machine_by_name(id)?.ok_or_else(|| exit(128, format!("unknown machine {id}: give its full id")))?
        };
        let _lock = h.repo.p.lock()?;
        super::replicate::trust(ctx, &h.repo.p, &full, key)?;
        outln!(ctx, "trusted machine {}", &ids::display(&full)[..8]);
        return Ok(0);
    }
    let me = &h.repo.p.store.machine.id;
    for (id, name) in h.repo.p.db.machines()? {
        let key = h.repo.p.db.machine_key(&id)?.unwrap_or_default();
        let trusted =
            if &id == me || h.repo.p.db.trusted(&id)?.as_deref() == Some(key.as_str()) { "trusted" } else { "untrusted" };
        outln!(
            ctx,
            "{}{} {:<16} {:<9} id {} key {}",
            if &id == me { "* " } else { "  " },
            &ids::display(&id)[..8],
            name,
            trusted,
            id,
            key
        );
    }
    Ok(0)
}

/// Hidden helpers for tests: `layr debug changes <a> <b>` prints the raw change list.
pub fn debug(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let h = here(ctx)?;
    match args.first().map(|s| s.as_str()) {
        Some("changes") if args.len() >= 3 => {
            let a = h.repo.state_root(&revs::resolve(&h.repo, h.line.as_ref(), &args[1])?)?;
            let b = h.repo.state_root(&revs::resolve(&h.repo, h.line.as_ref(), &args[2])?)?;
            for c in changes::candidates(&a, &b)? {
                let k = |m: &Option<fsutil::Meta>| m.as_ref().map(|x| format!("{:?}", x.ftype)).unwrap_or("-".into());
                outln!(
                    ctx,
                    "{}\t{}\t{}\t{}\t{:?}",
                    c.old_path.clone().unwrap_or_default(),
                    c.path,
                    k(&c.old),
                    k(&c.new),
                    c.ranges
                );
            }
            Ok(0)
        }
        // A checked receive of a stream from stdin into the project's states (for tests of
        // the checks in `recv`): root only.
        Some("receive") if args.len() >= 3 => {
            if !ctx.caller.is_root() {
                return Err(exit(1, "error: debug receive needs root"));
            }
            let p = &h.repo.p;
            let parent = |u: &str| -> Option<std::path::PathBuf> {
                let id = p.db.state_by_subvol(u).ok().flatten()?;
                let path = p.state_path(&id);
                path.exists().then_some(path)
            };
            let e = crate::recv::Expect { name: &args[1], uuid: &args[2], parent: &parent };
            crate::recv::receive(&mut std::io::stdin(), &p.states_dir(), &e)?;
            outln!(ctx, "received {}", args[1]);
            Ok(0)
        }
        _ => Err(exit(128, "usage: layr debug changes <a> <b> | layr debug receive <name> <uuid> < stream")),
    }
}
