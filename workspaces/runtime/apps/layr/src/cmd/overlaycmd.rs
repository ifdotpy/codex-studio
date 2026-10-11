//! `layr overlay`: file trees with an owner, readers and their own history, shown in lines above
//! the shared set (docs/overlays.md; the model is in src/overlay.rs).

use super::{exit, here};
use crate::access::{parse_principal, show_principal, Policy, Role};
use crate::args::{parse, Spec};
use crate::btrfs;
use crate::ctx::Ctx;
use crate::ids;
use crate::model::{Record, StateKind, StateRec};
use crate::overlay::{Layer, Mode};
use crate::repo::Repo;
use crate::store::{check_name, Project};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

const USAGE: &str = "usage: layr overlay create <name> [--readers <principal>,...] [--mode shared|follow]
       layr overlay list
       layr overlay readers <name> [<principal>...]
       layr overlay use [--add|--remove] <name>... | --none
       layr overlay move [--purge] <name> <path>... | --cancel [<path>...]
       layr overlay log|show|diff|grep|ls-files|blame <name> [<args>]";

pub fn overlay(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("list");
    let rest = args.get(1..).unwrap_or(&[]).to_vec();
    match sub {
        "create" => create(ctx, &rest),
        "list" => list(ctx, &rest),
        "readers" => readers(ctx, &rest),
        "use" => use_cmd(ctx, &rest),
        "move" => move_cmd(ctx, &rest),
        "log" | "show" | "diff" | "grep" | "ls-files" | "blame" | "rev-parse" => read(ctx, sub, &rest),
        _ => Err(exit(128, USAGE)),
    }
}

/// Principals as typed (`alice`, `@devs`, `uid:1001`, comma separated lists allowed).
fn principals(list: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for item in list.iter().flat_map(|s| s.split(',')).map(|s| s.trim()).filter(|s| !s.is_empty()) {
        match parse_principal(item) {
            Some(p) => out.push(p),
            None => return Err(exit(128, format!("fatal: unknown user or group '{item}'"))),
        }
    }
    Ok(out)
}

/// The access policy of an overlay: its owner is its admin, the readers read it.
fn access_map(owner: u32, readers: &[String]) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for r in readers {
        m.insert(r.clone(), serde_json::json!("reader"));
    }
    m.insert(format!("user:{owner}"), serde_json::json!("admin"));
    serde_json::Value::Object(m)
}

fn create(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().value("--readers|-r").value("--mode").flag("-q|--quiet");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    if repo.p.is_overlay() {
        return Err(exit(1, "error: an overlay has no overlays"));
    }
    let name = a.pos.first().ok_or_else(|| exit(128, USAGE))?.clone();
    check_name("overlay", &name)?;
    repo.require(ctx, "line.create")?;
    let mode = match a.get("--mode") {
        Some(m) => Mode::parse(m).ok_or_else(|| exit(128, format!("fatal: unknown mode '{m}' (shared or follow)")))?,
        None => Mode::Shared,
    };
    let readers = principals(&a.all("--readers"))?;
    let _lock = repo.p.lock()?;
    if repo.overlay_info(&name)?.is_some() {
        return Err(exit(128, format!("fatal: overlay '{name}' already exists")));
    }
    let owner = ctx.caller.uid;
    let pname = format!("{}%{name}", repo.p.name);
    let dir = ctx.store.project_dir(&pname)?;
    let made = (|| -> Result<String> {
        let config = serde_json::json!({
            "access": access_map(owner, &readers),
            "overlay": {"project": repo.p.id, "name": name, "mode": mode.name()},
        });
        let op = Project::create_overlay(&ctx.store, &pname, &ctx.actor(), config)?;
        let orepo = Repo { p: op };
        // The first state of every overlay line: an empty tree.
        let id = ids::new_id();
        let dst = orepo.p.state_path(&id);
        btrfs::create_subvolume(&dst)?;
        btrfs::set_readonly(&dst)?;
        let info = btrfs::subvol_info(&dst)?;
        let st = StateRec {
            id: id.clone(),
            kind: StateKind::Commit,
            parents: Vec::new(),
            line: None,
            subvol: info.uuid,
            author: ctx.author(),
            message: format!("overlay {name}"),
            time: ids::now_ms(),
            running: false,
            conflicts: Vec::new(),
            caches: BTreeMap::new(),
            cache_by: None,
            cache_shared: false,
        };
        let mut rec = Record::new("commit");
        rec.states.push(st);
        orepo.commit_record(ctx, rec)?;
        let mut rec = Record::new("overlay.create");
        let mut cfg = serde_json::Map::new();
        cfg.insert(format!("overlay.{name}"), serde_json::json!({"owner": owner, "mode": mode.name(), "root": id}));
        rec.data = serde_json::json!({"name": name, "config": cfg});
        repo.commit_record(ctx, rec)?;
        Ok(id)
    })();
    if let Err(e) = made {
        // No record of the project names the overlay: its folder goes.
        if repo.overlay_info(&name)?.is_none() {
            let _ = btrfs::delete_tree(&dir);
        }
        return Err(e);
    }
    if !a.has("-q") {
        let mut who: Vec<String> = vec![ctx.caller.user.clone()];
        who.extend(readers.iter().map(|r| show_principal(r)));
        who.dedup();
        outln!(ctx, "Created overlay {name} (mode {}; readers: {})", mode.name(), who.join(", "));
    }
    Ok(0)
}

/// The principals with the reader role (or more) of an overlay.
fn reader_list(p: &Project) -> Vec<String> {
    let pol = Policy::load(p);
    pol.access.iter().filter(|(_, r)| **r >= Role::Reader).map(|(k, _)| show_principal(k)).collect()
}

fn list(ctx: &Ctx, _args: &[String]) -> Result<i32> {
    let h = here(ctx)?;
    let repo = &h.repo;
    let shown: BTreeSet<String> = match &h.line {
        Some(l) => repo.layers(l)?.into_iter().map(|x| x.overlay).collect(),
        None => BTreeSet::new(),
    };
    for name in repo.overlay_names()? {
        let info = match repo.overlay_info(&name)? {
            Some(i) => i,
            None => continue,
        };
        let readers = repo.overlay_project(&name).map(|p| reader_list(&p)).unwrap_or_default();
        let mark = if shown.contains(&name) { "* " } else { "  " };
        let owner = crate::ctx::Caller::from_uid(info.owner).user;
        outln!(ctx, "{mark}{name}\t{}\towner {owner}\treaders {}", info.mode.name(), readers.join(","));
    }
    Ok(0)
}

fn readers(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new(), args)?;
    let h = here(ctx)?;
    let name = a.pos.first().ok_or_else(|| exit(128, USAGE))?.clone();
    let info = h.repo.need_overlay(&name)?;
    let orepo = h.repo.overlay_repo(&name)?;
    if a.pos.len() == 1 {
        for r in reader_list(&orepo.p) {
            outln!(ctx, "{r}");
        }
        return Ok(0);
    }
    if !orepo.is_admin(ctx) {
        return Err(exit(1, format!("permission denied: only the owner of overlay '{name}' sets its readers")));
    }
    let list = principals(&a.pos[1..])?;
    let _lock = orepo.p.lock()?;
    crate::cmd::admin::set_config(ctx, &orepo, "access", access_map(info.owner, &list))?;
    Ok(0)
}

/// The layer of overlay `name` for a line of `owner`: its overlay line, made when missing (a
/// follow line starts empty; a shared line of another user starts at the owner's).
fn layer_for(ctx: &Ctx, repo: &Repo, name: &str, line: &crate::model::Line) -> Result<Layer> {
    let info = repo.need_overlay(name)?;
    let gid = crate::ctx::Caller::from_uid(line.owner).gid;
    if !repo.reads_overlay(name, line.owner, gid)? {
        return Err(exit(1, format!("error: the owner of line '{}' does not read overlay '{name}'", line.name)));
    }
    let orepo = repo.overlay_repo(name)?;
    let _lock = orepo.p.lock()?;
    let start = match info.mode {
        Mode::Follow => info.root.clone(),
        Mode::Shared => orepo.p.db.line_by_name(&format!("user-{}", info.owner))?.map(|l| l.head).unwrap_or(info.root.clone()),
    };
    let ol = orepo.ensure_overlay_line(ctx, &Repo::overlay_line_name(&info, &line.id, line.owner), &start, line.owner)?;
    Ok(Layer { overlay: name.to_string(), state: ol.head })
}

/// Set the stack of the current line. The owner's choice: the line's overlay files come and go.
fn use_cmd(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("--add").flag("--remove").flag("--none").flag("-f|--force").flag("-q|--quiet");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = h.line()?.clone();
    if a.pos.is_empty() && !a.has("--none") {
        let layers = repo.layers(&line)?;
        if layers.is_empty() {
            outln!(ctx, "line {} shows no overlays", line.name);
        }
        for l in layers {
            outln!(ctx, "{}\t{}", l.overlay, ids::short(&l.state));
        }
        return Ok(0);
    }
    repo.authorize_line(ctx, &line, crate::repo::LineOp::Save)?;
    let _lock = repo.p.lock()?;
    let line = repo.line(&line.id)?;
    let cur = repo.layers(&line)?;
    let mut names: Vec<String> = Vec::new();
    if a.has("--add") || a.has("--remove") {
        names.extend(cur.iter().map(|l| l.overlay.clone()));
    }
    for n in &a.pos {
        if a.has("--remove") {
            names.retain(|x| x != n);
        } else if !names.contains(n) {
            names.push(n.clone());
        }
    }
    if a.has("--none") {
        names.clear();
    }
    // Named overlays take their newest state; with --add and --remove the others stay as shown.
    let keep = a.has("--add") || a.has("--remove");
    let mut layers = Vec::new();
    for n in &names {
        match cur.iter().find(|l| &l.overlay == n) {
            Some(l) if keep && !a.pos.contains(n) => layers.push(l.clone()),
            _ => layers.push(layer_for(ctx, repo, n, &line)?),
        }
    }
    change_stack(ctx, repo, &line, &layers, a.has("-f"))?;
    if !a.has("-q") {
        if layers.is_empty() {
            outln!(ctx, "line {} shows no overlays", line.name);
        } else {
            outln!(ctx, "line {} shows overlays: {}", line.name, names.join(" "));
        }
    }
    Ok(0)
}

fn change_stack(ctx: &Ctx, repo: &Repo, line: &crate::model::Line, layers: &[Layer], force: bool) -> Result<()> {
    if repo.layers(line)? == layers {
        return Ok(());
    }
    let (st, _scan, _it, _idx) = repo.status(line)?;
    if !st.staged.is_empty() {
        return Err(exit(1, "error: the line has staged changes; commit them or unstage them ('layr restore --staged') first"));
    }
    for (p, o) in repo.pending_moves(line) {
        if !layers.iter().any(|l| l.overlay == o) {
            return Err(exit(
                1,
                format!("error: '{p}' is to move to overlay '{o}' at the next commit; commit first, or 'layr overlay move --cancel {p}'"),
            ));
        }
    }
    crate::overlay::set_layers(ctx, repo, line, layers, force, "overlay.use")?;
    Ok(())
}

/// Move files of the current line into an overlay: the next commit takes them out of the layer
/// that has them and into the overlay's history.
fn move_cmd(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("--purge").flag("--cancel").flag("-q|--quiet");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = h.line()?.clone();
    if a.has("--cancel") {
        repo.authorize_line(ctx, &line, crate::repo::LineOp::Save)?;
        let _lock = repo.p.lock()?;
        let specs = h.specs(&a.rest())?;
        let mut pending = repo.pending_moves(&line);
        pending.retain(|p, _| !(specs.is_empty() || super::matches(&specs, p)));
        repo.set_pending_moves(&line, &pending)?;
        return Ok(0);
    }
    if a.rest().len() < 2 {
        return Err(exit(128, USAGE));
    }
    let name = a.rest()[0].clone();
    let specs = h.specs(&a.rest()[1..])?;
    repo.need_overlay(&name)?;
    repo.authorize_line(ctx, &line, crate::repo::LineOp::Save)?;
    let _lock = repo.p.lock()?;
    let mut line = repo.line(&line.id)?;
    // The line shows the overlay first (its files come into the folder).
    let cur = repo.layers(&line)?;
    if !cur.iter().any(|l| l.overlay == name) {
        let mut layers = cur.clone();
        layers.push(layer_for(ctx, repo, &name, &line)?);
        change_stack(ctx, repo, &line, &layers, false)?;
        line = repo.line(&line.id)?;
    }
    let scan = repo.scan(&line)?;
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for s in &specs {
        if s.is_empty() || crate::repo::skip_path(s) {
            return Err(exit(128, format!("fatal: cannot move '{}' into an overlay", h.rel(s))));
        }
        match crate::fsutil::lmeta(&scan.path, s) {
            None => return Err(exit(128, format!("fatal: pathspec '{}' did not match any files", h.rel(s)))),
            Some(m) if m.is_dir() => {
                crate::fsutil::walk(&scan.path, s, &mut |rel, m| {
                    if !m.is_dir() {
                        paths.insert(rel.to_string());
                    }
                    Ok(true)
                })?;
            }
            Some(_) => {
                paths.insert(s.clone());
            }
        }
    }
    let mut pending = repo.pending_moves(&line);
    for p in &paths {
        pending.insert(p.clone(), name.clone());
    }
    repo.set_pending_moves(&line, &pending)?;
    // Automatic states of the line made before the move hold the files (they hold the whole
    // working folder, ignored files included), and every reader of the project reads them.
    let mut holding = Vec::new();
    for st in repo.p.db.line_states(&line.id, StateKind::Auto)? {
        let root = repo.p.state_path(&st.id);
        if root.exists() && paths.iter().any(|p| crate::fsutil::lmeta(&root, p).is_some()) {
            holding.push(st.id);
        }
    }
    if a.has("--purge") && !holding.is_empty() {
        let mut deleted = Vec::new();
        for id in &holding {
            if btrfs::delete_tree(&repo.p.state_path(id)).is_ok() {
                deleted.push(id.clone());
            }
        }
        let mut rec = Record::new("gc");
        rec.data = serde_json::json!({"deleted": deleted, "purge": "overlay move"});
        repo.commit_record(ctx, rec)?;
        if !a.has("-q") {
            outln!(ctx, "deleted {} automatic state(s) of line {} that held the files", deleted.len(), line.name);
        }
    } else if !holding.is_empty() {
        outln!(
            ctx,
            "note: {} automatic state(s) of line {} hold these files, and readers of the project can read them; 'layr overlay move --purge' deletes those states",
            holding.len(),
            line.name
        );
    }
    if !a.has("-q") {
        let shared = repo.state_root(&line.head)?;
        let pushed = !repo.p.db.remote_refs()?.is_empty();
        for p in &paths {
            outln!(ctx, "move to overlay {name} at the next commit: {}", h.rel(p));
            if crate::fsutil::lmeta(&shared, p).is_some() {
                outln!(
                    ctx,
                    "note: '{}' is in the shared set: earlier states keep it{}; layr does not rewrite history",
                    h.rel(p),
                    if pushed { ", and so may the git remote" } else { "" }
                );
            }
        }
    }
    Ok(0)
}

/// A reading command on an overlay's history, as a reader of the overlay: `layr <cmd>` in the
/// overlay's own project, at the overlay line of the current line (or of the caller).
fn read(ctx: &Ctx, cmd: &str, args: &[String]) -> Result<i32> {
    let name = args.first().ok_or_else(|| exit(128, USAGE))?.clone();
    let h = here(ctx)?;
    let info = h.repo.need_overlay(&name)?;
    // The overlay line of the current line; else the caller's own line, else the owner's.
    let mut names = Vec::new();
    if let Some(l) = &h.line {
        if h.repo.layers(l)?.iter().any(|x| x.overlay == name) {
            names.push(Repo::overlay_line_name(&info, &l.id, l.owner));
        }
    }
    names.push(format!("user-{}", ctx.caller.uid));
    names.push(format!("user-{}", info.owner));
    let pname = format!("{}%{name}", h.repo.p.name);
    // The overlay's own policy decides: a user who does not read it is refused here.
    let orepo = Repo::open(ctx, &pname)?;
    let olname = names.into_iter().find(|n| orepo.p.db.line_by_name(n).ok().flatten().is_some());
    let mut env = ctx.env.clone();
    env.insert("LAYR_PROJECT".into(), pname);
    match olname {
        Some(n) => env.insert("LAYR_LINE".into(), n),
        None => env.remove("LAYR_LINE"),
    };
    use std::io::Write;
    let _ = ctx.out.borrow_mut().flush();
    let sub = Ctx {
        store: crate::store::Store::open(&ctx.store.root)?,
        caller: ctx.caller.clone(),
        cwd: ctx.cwd.clone(),
        env,
        out: std::cell::RefCell::new(Box::new(std::io::BufWriter::new(std::io::stdout()))),
    };
    let mut argv = vec![cmd.to_string()];
    argv.extend(args[1..].iter().cloned());
    let r = super::run(&sub, &argv);
    let _ = sub.out.borrow_mut().flush();
    r
}
