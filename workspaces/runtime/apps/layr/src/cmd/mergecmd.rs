//! merge, cherry-pick, revert and rebase.

use super::{exit, here, work};
use crate::args::{parse, Spec};
use crate::changes;
use crate::ctx::Ctx;
use crate::diff;
use crate::ids;
use crate::merge;
use crate::model::{Line, Record, StateKind, StateRec};
use crate::overlay::{Layer, Mode};
use crate::repo::{skip_path, Repo};
use crate::revs;
use anyhow::Result;

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
        run_as: ctx.caller.driver_ids(),
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

/// The merge of the follow overlays of two lines (docs/overlays.md, section 1): it goes with the
/// merge of the shared set, and the user who merges must read each of them.
#[derive(Default)]
pub struct OverlayMerge {
    /// The stack of the line after the merge.
    layers: Vec<Layer>,
    /// Overlay, its line of the merged line, the new head, the new state (none for a fast-forward).
    moves: Vec<(String, Line, String, Option<StateRec>)>,
    messages: Vec<String>,
    conflicts: usize,
}

impl OverlayMerge {
    fn changed(&self) -> bool {
        !self.moves.is_empty()
    }

    fn names(&self) -> Vec<String> {
        self.moves.iter().map(|m| m.0.clone()).collect()
    }

    fn discard(&self, repo: &Repo) {
        for (name, _, _, st) in &self.moves {
            if let (Some(st), Ok(o)) = (st, repo.overlay_repo(name)) {
                o.discard_states(std::slice::from_ref(st));
            }
        }
    }

    /// The records in the overlays, before the record of the line.
    fn record(&self, ctx: &Ctx, repo: &Repo) -> Result<()> {
        for (name, ol, head, st) in &self.moves {
            let o = repo.overlay_repo(name)?;
            let _lock = o.p.lock()?;
            crate::overlay::move_overlay_line(&o, ctx, "merge", ol, head, st.iter().cloned().collect())?;
        }
        Ok(())
    }
}

/// Merge the follow overlays of `theirs` into those of `line`: a fast-forward, or a three-way merge
/// in the overlay (states written, no records yet).
fn plan_overlays(ctx: &Ctx, repo: &Repo, line: &Line, theirs: Option<&Line>, label: &str, message: &str) -> Result<OverlayMerge> {
    let ours = repo.layers(line)?;
    let mut m = OverlayMerge { layers: ours.clone(), ..Default::default() };
    let theirs = match theirs {
        Some(t) => t,
        None => return Ok(m),
    };
    let r = (|| -> Result<()> {
        for tl in repo.layers(theirs)? {
            let info = match repo.overlay_info(&tl.overlay)? {
                Some(i) if i.mode == Mode::Follow => i,
                _ => continue,
            };
            if !repo.reads_overlay(&tl.overlay, ctx.caller.uid, ctx.caller.gid)? {
                return Err(exit(
                    1,
                    format!("error: merging line '{}' needs the read right of its overlay '{}'", theirs.name, tl.overlay),
                ));
            }
            let i = match ours.iter().position(|l| l.overlay == tl.overlay) {
                Some(i) => i,
                None => {
                    return Err(exit(
                        1,
                        format!(
                            "error: line '{}' shows overlay '{}' (follow mode), which line '{}' does not use; run 'layr overlay use --add {}' in '{}' first",
                            theirs.name, tl.overlay, line.name, tl.overlay, line.name
                        ),
                    ))
                }
            };
            let ours_s = ours[i].state.clone();
            let o = repo.overlay_repo(&tl.overlay)?;
            if tl.state == ours_s || o.is_ancestor(&tl.state, &ours_s)? {
                continue;
            }
            let ol =
                o.p.db
                    .line_by_name(&Repo::overlay_line_name(&info, &line.id, line.owner))?
                    .ok_or_else(|| exit(1, format!("error: line '{}' has no line in overlay '{}'", line.name, tl.overlay)))?;
            if o.is_ancestor(&ours_s, &tl.state)? {
                m.layers[i].state = tl.state.clone();
                m.moves.push((tl.overlay.clone(), ol, tl.state.clone(), None));
                continue;
            }
            let base = o
                .merge_base(&ours_s, &tl.state)?
                .ok_or_else(|| exit(128, format!("fatal: overlay '{}': refusing to merge unrelated histories", tl.overlay)))?;
            let (st, out) =
                merge_states(ctx, &o, &ol, &base, &ours_s, &tl.state, label, vec![ours_s.clone(), tl.state.clone()], message)?;
            m.conflicts += out.conflicts.len();
            m.messages.extend(out.messages.iter().map(|x| format!("overlay {}: {x}", tl.overlay)));
            m.layers[i].state = st.id.clone();
            m.moves.push((tl.overlay.clone(), ol, st.id.clone(), Some(st)));
        }
        Ok(())
    })();
    if let Err(e) = r {
        m.discard(repo);
        return Err(e);
    }
    Ok(m)
}

/// Move the line to `new_head` and update the working folder in place.
#[allow(clippy::too_many_arguments)]
fn finish(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    op: &str,
    new_head: &str,
    states: Vec<StateRec>,
    working_before: Option<String>,
    local: &std::collections::BTreeSet<String>,
    mut data: serde_json::Value,
    ovl: Option<&OverlayMerge>,
) -> Result<Vec<changes::Change>> {
    let fail = |e: anyhow::Error| {
        repo.discard_states(&states);
        if let Some(o) = ovl {
            o.discard(repo);
        }
        e
    };
    if let Err(e) = repo.check_move(ctx, line, new_head) {
        return Err(fail(e));
    }
    // A line with overlays: the full trees of the old and the new head, with the overlays in
    // place, so the update never touches an overlay file it does not change.
    let layers = match ovl {
        Some(o) if o.changed() => o.layers.clone(),
        _ => repo.layers(line)?,
    };
    let view = if layers.is_empty() {
        None
    } else {
        match repo.assemble(line, new_head, &layers) {
            Ok(v) => Some(v),
            Err(e) => return Err(fail(e)),
        }
    };
    let old_root = repo.head_tree(line)?;
    let new_root = match &view {
        Some(v) => v.path.clone(),
        None => repo.state_root(new_head)?,
    };
    let wdir = repo.working(line);
    let applied = match repo.apply_delta(&old_root, &new_root, &wdir, repo.own(line), local, false) {
        Ok(v) => v,
        Err(e) => return Err(fail(e)),
    };
    if let Some(o) = ovl.filter(|o| o.changed()) {
        if let Err(e) = o.record(ctx, repo) {
            return Err(fail(e));
        }
        data["stack"] = crate::overlay::stack_data(&line.id, &layers);
        data["line"] = serde_json::json!(line.id);
        data["overlays"] = serde_json::json!(o.names());
    }
    repo.move_head(ctx, op, line, new_head, states, working_before, data)?;
    if let Some(v) = view {
        repo.install_base(line, v, new_head, &layers)?;
    }
    let mut l2 = line.clone();
    l2.head = new_head.to_string();
    if let Err(e) = crate::gitbridge::refresh_git_folder(repo, &l2) {
        eprintln!("warning: .git folder not updated: {e:#}");
    }
    Ok(applied)
}

/// Path rules of a protected line: every path that the merge changes needs the role of its
/// rule (`layr protect <line> --path <path>=<role>`).
fn check_paths(ctx: &Ctx, repo: &Repo, line: &Line, rule: Option<&crate::access::Rule>, head: &str, result: &str) -> Result<()> {
    let rule = match rule {
        Some(r) if !r.paths.is_empty() && !ctx.caller.is_root() => r,
        _ => return Ok(()),
    };
    let role = repo.policy().role(&Repo::who(ctx));
    for c in changes::file_changes(&repo.state_root(head)?, &repo.state_root(result)?)? {
        for p in [Some(c.path.clone()), c.old_path.clone()].into_iter().flatten() {
            let need = rule.role_for_path(&p);
            if role < need {
                return Err(exit(
                    1,
                    format!(
                        "permission denied: the merge changes '{p}', which in line '{}' needs the role {}; you are {}",
                        line.name,
                        need.name(),
                        role.name()
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Approvals of a protected line: the reviewed state (`theirs`) needs the rule's number of
/// approvals of exactly that state, from users who may approve and who are not authors of the
/// states the merge brings in.
fn check_approvals(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    rule: Option<&crate::access::Rule>,
    head: &str,
    theirs: &str,
) -> Result<()> {
    let rule = match rule {
        Some(r) if r.approvals > 0 && !ctx.caller.is_root() => r,
        _ => return Ok(()),
    };
    let pol = repo.policy();
    let have: std::collections::BTreeSet<String> = repo.ancestors(head)?.into_iter().collect();
    let mut authors = std::collections::BTreeSet::new();
    for s in repo.ancestors(theirs)? {
        if !have.contains(&s) {
            if let Some(r) = repo.p.db.state(&s)? {
                authors.insert(r.author.uid);
            }
        }
    }
    let counted: Vec<u32> = repo
        .p
        .db
        .approvals(theirs)?
        .into_iter()
        .map(|(u, _)| u)
        .filter(|u| !authors.contains(u))
        .filter(|u| pol.may_approve(rule, &crate::access::Who::of(*u, crate::ctx::Caller::from_uid(*u).gid)))
        .collect();
    if (counted.len() as u32) < rule.approvals {
        let from: Vec<String> = rule.approvers.iter().map(|k| crate::access::show_principal(k)).collect();
        return Err(exit(
            1,
            format!(
                "error: line '{}' takes state {} after {} approval(s) of exactly that state by {} other than its authors; it has {}",
                line.name,
                ids::short(theirs),
                rule.approvals,
                if from.is_empty() { "maintainers".to_string() } else { from.join(" and ") },
                counted.len()
            ),
        ));
    }
    Ok(())
}

/// The checks of a protected line, on the state the line would take. They run as the caller
/// without the project lock; a passing check of the same state and command is reused. Then the
/// lock is taken again and the line must still be at `head`. Returns the lock (held until the
/// merge finishes) and the paths with local changes in the line's folder now.
fn run_line_checks(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    head: &str,
    candidate: &str,
    checks: &[String],
    quiet: bool,
) -> Result<(crate::store::Lock, std::collections::BTreeSet<String>)> {
    for name in checks {
        let cmd =
            repo.p.config_value(&format!("check.{name}"))?.and_then(|v| v.as_str().map(|s| s.to_string())).ok_or_else(|| {
                exit(
                    1,
                    format!(
                        "error: check '{name}' of line '{}' is not defined (layr config check.{name} '<command>')",
                        line.name
                    ),
                )
            })?;
        if repo.p.db.checks(candidate)?.iter().any(|c| c.0 == cmd && c.1 == 0) {
            continue;
        }
        let argv = vec!["sh".to_string(), "-c".to_string(), cmd.clone()];
        let code = super::admin::run_check(ctx, repo, candidate, &argv, &cmd, quiet)?;
        if code != 0 {
            return Err(exit(
                1,
                format!(
                    "error: check '{name}' failed (exit {code}) on {}, the state '{}' would take; the line stays at {}",
                    ids::short(candidate),
                    line.name,
                    ids::short(head)
                ),
            ));
        }
    }
    let lock = repo.p.lock()?;
    let now = repo.line(&line.id)?;
    if now.head != head {
        return Err(exit(1, format!("error: line '{}' moved while its checks ran; merge again", line.name)));
    }
    let (st, _scan, _it, _idx) = repo.status(&now)?;
    Ok((lock, local_paths(repo, &now, &st)?))
}

/// Paths a merge must not overwrite: local changes, and in a line with overlays also ignored
/// files (a user's own key where an overlay has one).
fn local_paths(repo: &Repo, line: &Line, st: &crate::repo::Status) -> Result<std::collections::BTreeSet<String>> {
    let mut local = st.local_paths();
    if !repo.layers(line)?.is_empty() {
        local.extend(st.ignored.iter().cloned());
    }
    Ok(local)
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
    let expect = a.get("--expect").is_some();
    let (lock, w) = work::begin(ctx, repo, line, crate::repo::LineOp::Merge { expect })?;
    let rule = repo.policy().rule_for(&line.name);
    let line = repo.line(&line.id)?;
    let head = line.head.clone();
    let label = theirs_line.as_ref().map(|l| l.name.clone()).unwrap_or_else(|| rev.to_string());
    let message = a.get("-m").map(|s| s.to_string()).unwrap_or_else(|| {
        if theirs_line.is_some() {
            format!("Merge line '{label}'")
        } else {
            format!("Merge {label}")
        }
    });
    let theirs_line = match theirs_line {
        Some(l) => Some(repo.localize(l)?).filter(|l| l.head == theirs),
        None => None,
    };
    let up_to_date = theirs == head || repo.is_ancestor(&theirs, &head)?;
    if up_to_date && theirs_line.as_ref().map(|t| repo.layers(t).map(|v| v.is_empty())).transpose()?.unwrap_or(true) {
        outln!(ctx, "Already up to date.");
        return Ok(0);
    }
    let (st, _scan, _it, _idx) = repo.status(&line)?;
    if !st.staged.is_empty() {
        return Err(exit(1, "error: you have staged changes; commit or stash them before merging."));
    }
    let mut local = local_paths(repo, &line, &st)?;
    let quiet = a.has("-q");
    let checks = rule.as_ref().map(|r| r.check.clone()).unwrap_or_default();
    // The follow overlays merge with the line: all of them or none.
    let ovl = plan_overlays(ctx, repo, &line, theirs_line.as_ref(), &label, &message)?;
    if !checks.is_empty() && ovl.conflicts > 0 {
        ovl.discard(repo);
        return Err(exit(
            1,
            format!(
                "error: the merge of the overlays into the protected line '{}' has conflicts; resolve them in a line first",
                line.name
            ),
        ));
    }
    let report = |ctx: &Ctx| {
        for m in &ovl.messages {
            outln!(ctx, "{m}");
        }
        if ovl.changed() && !quiet {
            outln!(ctx, "Merged overlays: {}", ovl.names().join(" "));
        }
    };
    if up_to_date {
        if !ovl.changed() {
            outln!(ctx, "Already up to date.");
            return Ok(0);
        }
        // Only the overlays changed: the shared set stays.
        finish(
            ctx,
            repo,
            &line,
            "merge",
            &head,
            vec![],
            w,
            &local,
            serde_json::json!({"theirs": theirs, "overlays_only": true}),
            Some(&ovl),
        )?;
        report(ctx);
        return Ok(if ovl.conflicts > 0 { 1 } else { 0 });
    }
    let ff = repo.is_ancestor(&head, &theirs)?;
    let mut _relock = None;
    if ff && !a.has("--no-ff") && !a.has("--squash") {
        check_paths(ctx, repo, &line, rule.as_ref(), &head, &theirs)?;
        check_approvals(ctx, repo, &line, rule.as_ref(), &head, &theirs)?;
        if !checks.is_empty() {
            drop(lock);
            let (l, paths) = run_line_checks(ctx, repo, &line, &head, &theirs, &checks, quiet)?;
            _relock = Some(l);
            local = paths;
        }
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
            Some(&ovl),
        )?;
        if !quiet {
            outln!(ctx, "Updating {}..{}\nFast-forward", ids::short(&head), ids::short(&theirs));
            print_stat(ctx, repo, &head, &theirs)?;
        }
        report(ctx);
        return Ok(if ovl.conflicts > 0 { 1 } else { 0 });
    }
    if a.has("--ff-only") {
        ovl.discard(repo);
        return Err(exit(128, "fatal: Not possible to fast-forward, aborting."));
    }
    let base = match repo.merge_base(&head, &theirs)? {
        Some(b) => b,
        None => {
            ovl.discard(repo);
            return Err(exit(128, format!("fatal: refusing to merge unrelated histories ({rev})")));
        }
    };
    let parents = if a.has("--squash") { vec![head.clone()] } else { vec![head.clone(), theirs.clone()] };
    let (state, out) = merge_states(ctx, repo, &line, &base, &head, &theirs, &label, parents, &message)?;
    let sid = state.id.clone();
    let nconf = out.conflicts.len();
    let mut states = vec![state];
    if let Err(e) = check_paths(ctx, repo, &line, rule.as_ref(), &head, &sid)
        .and_then(|_| check_approvals(ctx, repo, &line, rule.as_ref(), &head, &theirs))
    {
        repo.discard_states(&states);
        ovl.discard(repo);
        return Err(e);
    }
    if !checks.is_empty() {
        if nconf > 0 {
            repo.discard_states(&states);
            ovl.discard(repo);
            return Err(exit(
                1,
                format!("error: the merge into the protected line '{}' has {nconf} conflict(s); resolve them in a line and merge that line", line.name),
            ));
        }
        // The result becomes a state of its own record, so the check runs on exactly the content
        // that the line will take, and a failed result stays for inspection.
        let mut rec = Record::new("merge.candidate");
        rec.states = std::mem::take(&mut states);
        rec.data = serde_json::json!({"line": line.id, "theirs": theirs});
        repo.commit_record(ctx, rec)?;
        drop(lock);
        let (l, paths) = run_line_checks(ctx, repo, &line, &head, &sid, &checks, quiet)?;
        _relock = Some(l);
        local = paths;
    }
    finish(
        ctx,
        repo,
        &line,
        "merge",
        &sid,
        states,
        w,
        &local,
        serde_json::json!({"theirs": theirs, "base": base, "squash": a.has("--squash"), "conflicts": nconf}),
        Some(&ovl),
    )?;
    for m in &out.messages {
        outln!(ctx, "{m}");
    }
    report(ctx);
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
    Ok(if ovl.conflicts > 0 { 1 } else { 0 })
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
            None,
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
        None,
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
