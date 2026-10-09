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

pub fn save(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .value("-m|--message")
        .flag("--turn-end")
        .flag("--running")
        .flag("--force")
        .flag("--layers")
        .flag("--all")
        .flag("-q|--quiet");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let lines: Vec<Line> = if a.has("--all") {
        repo.require_lead(ctx, "save all lines")?;
        repo.lines()?.into_iter().filter(|l| repo.working(l).exists()).collect()
    } else {
        vec![h.line()?.clone()]
    };
    let msg = a.get("-m").unwrap_or(if a.has("--turn-end") { "end of turn" } else { "" }).to_string();
    for l in lines {
        repo.can_write_line(ctx, &l)?;
        let _lock = repo.p.lock()?;
        let l = repo.line(&l.id)?;
        if a.has("--turn-end") {
            // A quiet point: turn regenerable folders that a tool recreated back into layers.
            let (u, g) = repo.owner_ids(&l);
            for p in repo.p.config_strings("regenerable") {
                let _ = crate::repo::convert_layer(&repo.working(&l), &p, u, g);
            }
        }
        let st = repo.save(ctx, &l, &msg, a.has("--running"), a.has("--force") || a.has("--layers"))?;
        match st {
            Some(mut s) => {
                if a.has("--layers") {
                    let layers = snapshot_layers(repo, &l, &s.id)?;
                    if !layers.is_empty() {
                        s.layers = layers.clone();
                        let mut rec = Record::new("save.layers");
                        rec.data = serde_json::json!({"state": s.id, "layers": layers});
                        repo.commit_record(ctx, rec)?;
                    }
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

fn snapshot_layers(repo: &Repo, line: &Line, state: &str) -> Result<BTreeMap<String, String>> {
    let mut m = BTreeMap::new();
    let w = repo.working(line);
    for p in repo.p.config_strings("regenerable") {
        let lp = fsutil::safe_join(&w, &p)?;
        if btrfs::is_subvolume(&lp) {
            let name = format!("{state}@{}", p.replace('/', "%2F"));
            btrfs::snapshot(&lp, &repo.p.layer_path(state, &name), true)?;
            m.insert(p, name);
        }
    }
    Ok(m)
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
    "slot",
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
            // The newest operation of this caller (user and agent label) in the project.
            let agent = ctx.env("LAYR_AGENT").map(|s| s.to_string());
            let mut found = None;
            for r in repo.p.db.records(5000)? {
                let local = r.lines.keys().all(|lid| {
                    repo.p.db.line_by_id(lid).ok().flatten().map(|l| repo.p.line_path(&l.name).exists()).unwrap_or(false)
                        || r.op == "line.delete"
                });
                if r.actor.uid == ctx.caller.uid && r.actor.agent == agent && local && !NOT_UNDOABLE.contains(&r.op.as_str()) {
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
    // to change each of them. An operation without lines (tags, groups) needs the lead or the
    // user who made it.
    for (lid, ch) in &r.lines {
        let owner = repo.p.db.line_by_id(lid)?.map(|l| l.owner).or(ch.before.as_ref().map(|b| b.owner)).unwrap_or(0);
        let probe = Line {
            id: lid.clone(),
            name: ch.before.as_ref().or(ch.after.as_ref()).map(|s| s.name.clone()).unwrap_or_default(),
            head: String::new(),
            owner,
            machine: String::new(),
            group: None,
        };
        repo.can_write_line(ctx, &probe)?;
    }
    if r.lines.is_empty() && !repo.is_lead(ctx) && r.actor.uid != ctx.caller.uid {
        return Err(exit(1, "permission denied: only the project lead or the user who made it can undo this operation"));
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
                    group: b.group.clone(),
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
        h.repo.require_lead(ctx, "export the records")?;
        ctx.print(&h.repo.p.export_jsonl()?);
        return Ok(0);
    }
    if sub == "import" {
        let h = here(ctx)?;
        h.repo.require_lead(ctx, "import records")?;
        let data = match args.get(1).map(|s| s.as_str()) {
            None | Some("-") => {
                let mut v = Vec::new();
                std::io::Read::read_to_end(&mut std::io::stdin(), &mut v)?;
                v
            }
            Some(f) => super::work::read_caller_file(ctx, f)?,
        };
        let lines: Vec<String> =
            String::from_utf8_lossy(&data).lines().filter(|l| !l.trim().is_empty()).map(|l| l.to_string()).collect();
        let _lock = h.repo.p.lock()?;
        let n = h.repo.p.import_lines(&lines, false)?;
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
            let agent = r.actor.agent.as_ref().map(|a| format!(" [{a}]")).unwrap_or_default();
            outln!(ctx, "{} {} {}{} {}{}{}", short_op(&r.id), fmt_time(r.time), r.actor.name, agent, r.op, what, undone);
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
    let rev = a.pos.first().ok_or_else(|| exit(128, "usage: layr try <rev> -- <command>..."))?;
    if a.paths.is_empty() {
        return Err(exit(128, "usage: layr try <rev> -- <command>..."));
    }
    let state = revs::resolve(repo, h.line.as_ref(), rev)?;
    let name = format!(".try-{:016x}", rand::random::<u64>());
    let dir = repo.p.line_path(&name);
    let tmp_line = Line {
        id: format!("try-{name}"),
        name: name.clone(),
        head: state.clone(),
        owner: ctx.caller.uid,
        machine: repo.p.store.machine.id.clone(),
        group: None,
    };
    repo.materialize(&tmp_line, &state, &dir)?;
    let res = (|| -> Result<i32> {
        if !a.has("-q") {
            eprintln!("layr try: running in {} (state {})", dir.display(), ids::short(&state));
        }
        let started = std::time::Instant::now();
        let mut c = std::process::Command::new(&a.paths[0]);
        c.args(&a.paths[1..]).current_dir(&dir);
        // The caller's own environment and identity (uid, groups), never the service's.
        let mut env: Vec<(String, String)> = ctx.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        env.push(("LAYR_TRY_STATE".into(), ids::display(&state)));
        crate::privs::command_as(&mut c, ctx.caller.uid, &env)?;
        let st = c.status()?;
        let code = st.code().unwrap_or(128);
        let _lock = repo.p.lock()?;
        let mut rec = Record::new("check");
        rec.data = serde_json::json!({"state": state, "command": a.paths.join(" "), "exit": code, "seconds": started.elapsed().as_secs_f64()});
        repo.commit_record(ctx, rec)?;
        Ok(code)
    })();
    if a.has("--keep") {
        eprintln!("layr try: kept {}", dir.display());
    } else {
        let _ = btrfs::delete_tree(&dir);
    }
    let code = res?;
    if !a.has("-q") {
        eprintln!("layr try: state {} exit {code}", ids::short(&state));
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

/// Retention. Kept: line heads and their history, tags, remote refs, groups, stashes, the newest
/// export per branch, conflict sources, slot states, automatic states (the newest N per line and
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
    for (_, _, base, _) in db.groups()? {
        roots.push(base);
    }
    let mut newest_export: BTreeMap<(String, String), String> = BTreeMap::new();
    for (_, st, remote, branch, _, _) in db.exports()? {
        newest_export.insert((remote, branch), st);
    }
    roots.extend(newest_export.into_values());
    let mut st = db.conn.prepare("SELECT state FROM slots")?;
    let slots: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
    drop(st);
    roots.extend(slots);
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
    if dry || delete.is_empty() {
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
    let mut rec = Record::new("gc");
    rec.data = serde_json::json!({"deleted": deleted});
    repo.commit_record(ctx, rec)?;
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

pub fn gc(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("-n|--dry-run").flag("-q|--quiet");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    h.repo.require_lead(ctx, "run gc")?;
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
    repo.require_lead(ctx, "run fsck")?;
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
    h.repo.require_lead(ctx, "rebuild the index")?;
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
    let p = Project::create(&ctx.store, name, &actor, serde_json::json!({"lead": owner, "members": "*"}))?;
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
    let l = repo.create_line(ctx, &line_name, &state, owner, None)?;
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
    repo.require_lead(ctx, "change the project configuration")?;
    let key = a.get("--unset").unwrap_or_else(|| a.pos.first().map(|s| s.as_str()).unwrap_or(""));
    if key == "lead" && !ctx.caller.is_root() {
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
    if k == "members" || k == "lead" {
        crate::gitbridge::sync_store_access(repo)?;
    }
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
                            "plain folder (run layr save --turn-end)"
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
            repo.require_lead(ctx, "change layers")?;
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
        _ => Err(exit(128, "usage: layr layer (list|add|remove) [<path>]")),
    }
}

pub fn machines(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let h = here(ctx)?;
    if args.first().map(|s| s.as_str()) == Some("trust") {
        let spec = Spec::new().value("--key");
        let a = parse(&spec, &args[1..])?;
        h.repo.require_lead(ctx, "trust machines")?;
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
        let trusted = if &id == me || h.repo.p.db.trusted(&id)?.as_deref() == Some(key.as_str()) { "trusted" } else { "untrusted" };
        outln!(ctx, "{}{} {:<16} {:<9} id {} key {}", if &id == me { "* " } else { "  " }, &ids::display(&id)[..8], name, trusted, id, key);
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
        _ => Err(exit(128, "usage: layr debug changes <a> <b>")),
    }
}
