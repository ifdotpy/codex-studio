//! `layr overlay`: file trees with an owner, readers and their own history, shown in lines above
//! the shared set (docs/overlays.md; the model is in src/overlay.rs).

use super::{exit, here};
use crate::access::{parse_principal, show_principal, Policy, Role};
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::ids;
use crate::model::{Record, StateKind, StateRec};
use crate::overlay::{Layer, MergeKind, Mode, Props, PROPS};
use crate::repo::Repo;
use crate::storage as btrfs;
use crate::store::{check_name, Project};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

const USAGE: &str =
    "usage: layr overlay create <name> [--readers <principal>,...] [--mode shared|follow] [--hidden] [--folder <path>] [<properties>]
       layr overlay set <name> <properties>
       layr overlay remote add <name> <remote> <git url> | list <name> | remove <name> <remote>
       layr overlay show-to-project <name>
       layr overlay list
       layr overlay readers <name> [<principal>...]
       layr overlay managers <name> [machine:<id>...]
       layr overlay use [--add|--remove] <name>... | --none
       layr overlay move [--purge] <name> <path>... | --cancel [<path>...]
       layr overlay repair <overlay>
       layr overlay show <name> --json
       layr overlay log <name> --ops [--json]
       layr overlay log|show|diff|grep|ls-files|blame <name> [<args>]
properties: --history all|last|<N>  --quiet|--no-quiet  --merge three-way|union-theirs  --gc none|lru:<size>  --key-files <files>|none";

pub fn overlay(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("list");
    let rest = args.get(1..).unwrap_or(&[]).to_vec();
    match sub {
        "create" => create(ctx, &rest),
        "set" => set(ctx, &rest),
        "remote" => super::layerpush::overlay_remote(ctx, &rest),
        "list" => list(ctx, &rest),
        "show-to-project" => show_to_project(ctx, &rest),
        "readers" => readers(ctx, &rest),
        "managers" => managers(ctx, &rest),
        "mode" => access_mode(ctx, &rest),
        "use" => use_cmd(ctx, &rest),
        "move" => move_cmd(ctx, &rest),
        "repair" => repair(ctx, &rest),
        "show" if rest.iter().take_while(|s| s.as_str() != "--").any(|s| s == "--json") => show_json(ctx, &rest),
        "log" if rest.iter().take_while(|s| s.as_str() != "--").any(|s| s == "--ops") => operations(ctx, &rest),
        "log" | "show" | "diff" | "grep" | "ls-files" | "blame" | "rev-parse" => read(ctx, sub, &rest),
        _ => Err(exit(128, USAGE)),
    }
}

/// Principals as typed (`alice`, `@devs`, `uid:1001`, comma separated lists allowed).
fn principals(list: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for item in list.iter().flat_map(|s| s.split(',')).map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if let Some(machine) = item.strip_prefix("machine:") {
            if !crate::model::is_uuid(machine) {
                return Err(exit(128, "a machine reader needs its UUID"));
            }
            out.push(item.to_string());
            continue;
        }
        if let Some(person) = item.strip_prefix("person:") {
            let (login, recipient) = person.split_once('=').ok_or_else(|| exit(128, "use person:<login>=<age recipient>"))?;
            if !layr_store::person::login_name(login) || !layr_store::keys::valid_recipient(recipient) {
                return Err(exit(128, "invalid person reader or recipient"));
            }
            out.push(item.to_string());
            continue;
        }
        match parse_principal(item) {
            Some(p) => out.push(p),
            None => return Err(exit(128, format!("fatal: unknown user or group '{item}'"))),
        }
    }
    Ok(out)
}

/// The access policy of an overlay: its owner is its admin, the readers read it.
pub(crate) fn access_map(owner: u32, readers: &[String]) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for r in readers {
        m.insert(r.clone(), serde_json::json!("reader"));
    }
    m.insert(format!("user:{owner}"), serde_json::json!("admin"));
    serde_json::Value::Object(m)
}

fn create(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = props_spec(Spec::new().value("--readers|-r").value("--mode").flag("--hidden").flag("-q").value("--folder"));
    let a = parse(&spec, args)?;
    let props = props_of(&a, Props::default())?;
    let h = here(ctx)?;
    let repo = &h.repo;
    if repo.p.is_overlay() {
        return Err(exit(1, "error: an overlay has no overlays"));
    }
    let name = a.pos.first().ok_or_else(|| exit(128, USAGE))?.clone();
    check_name("overlay", &name)?;
    repo.require(ctx, "line.create")?;
    let hidden = a.has("--hidden");
    let folder = a.get("--folder").map(crate::cache_overlay::folder_path).transpose()?;
    let mode = match a.get("--mode") {
        Some(m) => Mode::parse(m).ok_or_else(|| exit(128, format!("fatal: unknown mode '{m}' (shared or follow)")))?,
        None if hidden || folder.is_some() => Mode::Follow,
        None => Mode::Shared,
    };
    if hidden && mode != Mode::Follow {
        return Err(exit(128, "hidden overlays require follow mode"));
    }
    if folder.is_some() && (hidden || mode != Mode::Follow) {
        return Err(exit(128, "a folder overlay uses follow mode and is not hidden"));
    }
    let readers = principals(&a.all("--readers"))?;
    let _lock = repo.p.lock()?;
    if repo.overlay_info(&name)?.is_some() {
        return Err(exit(128, format!("fatal: overlay '{name}' already exists")));
    }
    if let Some(f) = &folder {
        if let Some((other, _)) = repo.folder_overlays()?.into_iter().find(|(_, x)| x == f) {
            return Err(exit(128, format!("fatal: overlay '{other}' already holds folder {f}")));
        }
    }
    let key_files = a.get("--key-files").map(key_files_value).transpose()?;
    make_overlay(ctx, repo, &name, &readers, mode, hidden, &props, folder.as_deref(), false, key_files.as_ref())?;
    if !a.has("-q") {
        let mut who: Vec<String> = vec![ctx.caller.user.clone()];
        who.extend(readers.iter().map(|r| show_principal(r)));
        who.dedup();
        let mut shown = props.describe();
        if let Some(f) = &folder {
            shown.insert(0, format!("folder={f}"));
        }
        let extra = if shown.is_empty() { String::new() } else { format!("; {}", shown.join(" ")) };
        outln!(ctx, "Created overlay {name} (mode {}; readers: {}{extra})", mode.name(), who.join(", "));
    }
    Ok(0)
}

/// Make overlay `name` of the caller with its empty first state and its records. The caller holds
/// the project lock and has checked that no overlay has the name. `folder`: a folder overlay;
/// `cache`: the cache overlay of a cache path (`save --caches`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn make_overlay(
    ctx: &Ctx,
    repo: &Repo,
    name: &str,
    readers: &[String],
    mode: Mode,
    hidden: bool,
    props: &Props,
    folder: Option<&str>,
    cache: bool,
    key_files: Option<&serde_json::Value>,
) -> Result<()> {
    let owner = ctx.caller.uid;
    let pname = format!("{}%{name}", repo.p.name);
    let dir = ctx.store.project_dir(&pname)?;
    let made = (|| -> Result<String> {
        let config = serde_json::json!({
            "access": access_map(owner, readers),
            "overlay": {"project": repo.p.id, "name": name, "mode": mode.name(), "hidden": hidden},
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
            change_id: Some(ids::new_id()),
            derived_from: None,
            context: None,
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
            cache_platform: None,
        };
        let mut rec = Record::new("commit");
        rec.states.push(st);
        orepo.commit_record(ctx, rec)?;
        let mut rec = Record::new("overlay.create");
        let mut cfg = serde_json::Map::new();
        cfg.insert(
            format!("overlay.{name}"),
            serde_json::json!({"id":orepo.p.id,"owner": owner, "mode": mode.name(), "root": id, "hidden": hidden}),
        );
        // Properties are separate settings, so an older layr reads the overlay as before.
        for prop in PROPS {
            let v = props.value(prop);
            if !v.is_null() {
                cfg.insert(format!("overlay.{name}.{prop}"), v);
            }
        }
        if let Some(f) = folder {
            cfg.insert(format!("overlay.{name}.folder"), serde_json::json!(f));
        }
        if let Some(k) = key_files {
            cfg.insert(format!("overlay.{name}.key-files"), k.clone());
        }
        // A cache overlay follows the cache paths (`regenerable`, `cache add`) of each line.
        if cache {
            cfg.insert(format!("overlay.{name}.cache"), serde_json::json!(true));
        }
        rec.data = serde_json::json!({"name": name, "parent":repo.p.id, "config": cfg});
        if hidden {
            orepo.commit_record(ctx, rec)?;
        } else {
            repo.commit_record(ctx, rec)?;
        }
        Ok(id)
    })();
    if let Err(e) = made {
        // No record of the project names the overlay: its folder goes.
        if repo.overlay_info(name)?.is_none() {
            let _ = btrfs::delete_tree(&dir);
        }
        return Err(e);
    }
    Ok(())
}

/// A folder overlay is not part of the file stack: `save --caches`, `line new` and `merge` move it (docs/cache-overlays.md, section 3).
fn refuse_folder(repo: &Repo, name: &str) -> Result<()> {
    if repo.overlay_info(name)?.is_some() {
        if let Some(f) = repo.overlay_setting(name, "folder")? {
            return Err(exit(
                1,
                format!(
                    "error: overlay '{name}' is the folder overlay of {}; use layr save --caches or layr merge",
                    f.as_str().unwrap_or("?")
                ),
            ));
        }
    }
    Ok(())
}

/// The options of overlay properties (docs/cache-overlays.md, section 4).
fn props_spec(spec: Spec) -> Spec {
    spec.value("--history").flag("--quiet").flag("--no-quiet").value("--merge").value("--gc").value("--key-files")
}

/// `--key-files a,b` as the setting value; `none` clears it.
fn key_files_value(v: &str) -> Result<serde_json::Value> {
    if v == "none" {
        return Ok(serde_json::Value::Null);
    }
    let mut files = Vec::new();
    for f in v.split(',').map(str::trim).filter(|f| !f.is_empty()) {
        files.push(crate::cache_overlay::folder_path(f)?);
    }
    Ok(serde_json::json!(files))
}

/// `base` with the property options of `a`.
fn props_of(a: &crate::args::Parsed, base: Props) -> Result<Props> {
    let mut p = base;
    if let Some(v) = a.get("--history") {
        p.history = crate::overlay::parse_history(v)
            .ok_or_else(|| exit(128, format!("fatal: invalid --history '{v}' (all, last or a number)")))?;
    }
    if a.has("--quiet") && a.has("--no-quiet") {
        return Err(exit(128, "fatal: --quiet and --no-quiet exclude each other"));
    }
    if a.has("--quiet") {
        p.quiet = true;
    }
    if a.has("--no-quiet") {
        p.quiet = false;
    }
    if let Some(v) = a.get("--merge") {
        p.merge =
            MergeKind::parse(v).ok_or_else(|| exit(128, format!("fatal: invalid --merge '{v}' (three-way or union-theirs)")))?;
    }
    if let Some(v) = a.get("--gc") {
        p.gc_lru = crate::overlay::parse_gc(v)
            .ok_or_else(|| exit(128, format!("fatal: invalid --gc '{v}' (none or lru:<size>, for example lru:20G)")))?;
    }
    Ok(p)
}

/// `layr overlay set <name> <properties>`: the owner changes properties. The settings go into the
/// records of the project, or of the overlay when it is hidden.
fn set(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&props_spec(Spec::new()), args)?;
    let h = here(ctx)?;
    let name = a.pos.first().ok_or_else(|| exit(128, USAGE))?.clone();
    if a.pos.len() != 1 {
        return Err(exit(128, USAGE));
    }
    let info = visible_info(ctx, &h.repo, &name)?;
    let orepo = h.repo.overlay_repo(&name)?;
    if !orepo.is_admin(ctx) {
        return Err(exit(1, format!("permission denied: only the owner of overlay '{name}' sets its properties")));
    }
    let _lock = h.repo.p.lock()?;
    let old = h.repo.overlay_props(&name)?;
    let new = props_of(&a, old.clone())?;
    let mut cfg = serde_json::Map::new();
    for prop in PROPS {
        if old.value(prop) != new.value(prop) {
            cfg.insert(format!("overlay.{name}.{prop}"), new.value(prop));
        }
    }
    // The files of the platform key of a cache (docs/cache-overlays.md, section 6).
    if let Some(v) = a.get("--key-files") {
        cfg.insert(format!("overlay.{name}.key-files"), key_files_value(v)?);
    }
    if !cfg.is_empty() {
        let mut rec = Record::new("config");
        rec.data = serde_json::json!({"config": cfg});
        if info.hidden {
            let _olock = orepo.p.lock()?;
            orepo.commit_record(ctx, rec)?;
        } else {
            h.repo.commit_record(ctx, rec)?;
        }
    }
    let shown = new.describe();
    outln!(ctx, "overlay {name}: {}", if shown.is_empty() { "no properties".to_string() } else { shown.join(" ") });
    if new.history.is_some() && new.history != old.history {
        h.repo.after_overlay_move(ctx, &name);
    }
    Ok(0)
}

/// The principals with the reader role (or more) of an overlay.
fn reader_list(p: &Project) -> Vec<String> {
    let pol = Policy::load(p);
    pol.access.iter().filter(|(_, r)| **r >= Role::Reader).map(|(k, _)| show_principal(k)).collect()
}

/// A folder overlay's folder in a line, as `cache list -v` showed it: the state, the size of a
/// cache, and the head of the overlay line that the line saved.
struct InLine {
    state: String,
    size: Option<u64>,
    saved: Option<String>,
}

fn folder_in_line(repo: &Repo, l: &crate::model::Line, name: &str, folder: &str, versioned: bool) -> InLine {
    let cp = repo.working(l).join(folder);
    let state = match (versioned, btrfs::is_subvolume(&cp)) {
        (true, true) => format!("not a cache: has versioned files (run layr overlay repair {name})"),
        (true, false) => crate::cmd::admin::VERSIONED.to_string(),
        (false, true) => "cache".to_string(),
        (false, false) if cp.exists() => format!("plain folder (run layr overlay repair {name})"),
        (false, false) => "absent".to_string(),
    };
    let size = (state == "cache").then(|| crate::cache_overlay::tree_size(&cp));
    let saved = repo
        .overlay_project(name)
        .ok()
        .and_then(|p| p.db.line_by_name(&format!("line-{}", l.id)).ok().flatten())
        .map(|ol| ids::short(&ol.head));
    InLine { state, size, saved }
}

fn list(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--json"), args)?;
    let mut rows = Vec::new();
    let h = here(ctx)?;
    let repo = &h.repo;
    let shown: BTreeSet<String> = match &h.line {
        Some(l) => repo.layers(l)?.into_iter().map(|x| x.overlay).collect(),
        None => BTreeSet::new(),
    };
    // The versioned folders of the line: a folder overlay there is no cache.
    let versioned = match &h.line {
        Some(l) => repo.line_cache_paths(l)?.1,
        None => Vec::new(),
    };
    for name in repo.visible_overlay_names(ctx)? {
        let info = match repo.overlay_info(&name)? {
            Some(i) => i,
            None => continue,
        };
        let readers = repo.overlay_project(&name).map(|p| reader_list(&p)).unwrap_or_default();
        let mark = if shown.contains(&name) { "* " } else { "  " };
        let owner = crate::ctx::Caller::from_uid(info.owner).user;
        let props = repo.overlay_props(&name)?;
        let folder = repo.overlay_setting(&name, "folder")?.and_then(|v| v.as_str().map(str::to_owned));
        let line_state = match (&h.line, &folder) {
            (Some(l), Some(f)) => Some(folder_in_line(repo, l, &name, f, versioned.contains(f))),
            _ => None,
        };
        if a.has("--json") {
            rows.push(serde_json::json!({
                "name":name,"mode":info.mode.name(),"hidden":info.hidden,"owner":info.owner,"readers":readers,
                "shown":shown.contains(&name),"properties":props.to_json(),"folder":folder,
                "state":line_state.as_ref().map(|s| &s.state),"size":line_state.as_ref().and_then(|s| s.size),
                "saved":line_state.as_ref().and_then(|s| s.saved.clone()),
            }));
        } else {
            let mut described = props.describe();
            if let Some(f) = &folder {
                described.insert(0, format!("folder={f}"));
            }
            let keys = repo.key_files(&name)?;
            if !keys.is_empty() {
                described.push(format!("key-files={}", keys.join(",")));
            }
            // A folder overlay in the line: its state, its size and its saved state (was `cache list -v`).
            let tail = line_state
                .map(|s| {
                    let size = s.size.map(crate::cmd::admin::human_size).unwrap_or_else(|| "-".into());
                    let saved = s.saved.map(|x| format!("saved {x}")).unwrap_or_else(|| "not saved".into());
                    format!("\t{}\t{size}\t{saved}", s.state)
                })
                .unwrap_or_default();
            outln!(
                ctx,
                "{mark}{name}\t{}{}\towner {owner}\treaders {}{}{tail}",
                info.mode.name(),
                if info.hidden { " hidden" } else { "" },
                readers.join(","),
                if described.is_empty() { String::new() } else { format!("\t{}", described.join(" ")) }
            );
        }
    }
    if a.has("--json") {
        outln!(ctx, "{}", serde_json::to_string(&rows)?);
    }
    Ok(0)
}

/// `layr overlay repair <overlay>`: in the current line, the folder of a folder overlay that a tool
/// made again as a plain folder becomes a cache again. A cache folder with versioned files becomes
/// a plain folder again (`layr cache repair` before; docs/cache-overlays.md, section 4).
fn repair(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new(), args)?;
    let name = match a.pos.as_slice() {
        [name] => name.clone(),
        _ => return Err(exit(128, "usage: layr overlay repair <overlay>")),
    };
    let h = here(ctx)?;
    let repo = &h.repo;
    repo.need_overlay(&name)?;
    let folder = repo
        .overlay_setting(&name, "folder")?
        .and_then(|v| v.as_str().map(str::to_owned))
        .ok_or_else(|| exit(128, format!("fatal: overlay '{name}' is no folder overlay")))?;
    let l = h.line()?.clone();
    repo.authorize_line(ctx, &l, crate::repo::LineOp::Save)?;
    let _lock = repo.p.lock()?;
    let l = repo.line(&l.id)?;
    let (u, g) = repo.owner_ids(&l);
    crate::repo::recover_cache_replacements(&repo.working(&l))?;
    let (caches, versioned) = repo.line_cache_paths(&l)?;
    if versioned.contains(&folder) {
        if crate::repo::uncache(&repo.working(&l), &folder, u, g)? {
            outln!(ctx, "{folder}: {}", crate::cmd::admin::VERSIONED_AGAIN);
        }
    } else if caches.contains(&folder) {
        if crate::repo::convert_cache(&repo.working(&l), &folder, u, g)? {
            outln!(ctx, "{folder}: a cache again");
        }
    } else {
        return Err(exit(1, format!("error: {folder} is no cache path of this line (regenerable)")));
    }
    Ok(0)
}

fn readers(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new(), args)?;
    let h = here(ctx)?;
    let name = a.pos.first().ok_or_else(|| exit(128, USAGE))?.clone();
    let info = visible_info(ctx, &h.repo, &name)?;
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
    let _parent_lock = h.repo.p.lock()?;
    let _lock = orepo.p.lock()?;
    crate::overlay_managers::of(&h.repo, &name)?.require(&ctx.store.machine.id)?;
    let access = access_map(info.owner, &list);
    let removed = orepo.policy().access.iter().any(|(principal, role)| *role >= Role::Reader && access.get(principal).is_none());
    let mut record = Record::new("overlay.readers");
    record.data = serde_json::json!({"config":{"access":access}});
    if removed {
        record.data["config"]["overlay.revocation"] = serde_json::json!(ids::new_id());
    }
    orepo.commit_record(ctx, record)?;
    Ok(0)
}

fn access_mode(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--json"), args)?;
    if a.pos.len() != 2 {
        return Err(exit(128, "usage: layr overlay mode <name> private|service|public"));
    }
    let h = here(ctx)?;
    let name = &a.pos[0];
    let info = visible_info(ctx, &h.repo, name)?;
    let mode = layr_store::epochs::Mode::parse(&a.pos[1])?;
    let parent = crate::store_remote::epochs::project_mode(&h.repo)?;
    if mode > parent {
        return Err(exit(1, format!("overlay mode {} is more open than project mode {}", mode.name(), parent.name())));
    }
    if info.hidden && mode != layr_store::epochs::Mode::Private {
        return Err(exit(1, "hidden overlays are always private"));
    }
    let orepo = h.repo.overlay_repo(name)?;
    if !orepo.is_admin(ctx) {
        return Err(exit(1, "only the overlay owner can set its access mode"));
    }
    crate::overlay_managers::of(&h.repo, name)?.require(&ctx.store.machine.id)?;
    if mode == layr_store::epochs::Mode::Service {
        let service = h.repo.p.config_value("service.recipient")?.and_then(|v| v.as_str().map(str::to_owned));
        if service.as_ref().is_none_or(|s| !layr_store::keys::valid_recipient(s)) {
            return Err(exit(1, "service overlay requires layr config service.recipient <age recipient>"));
        }
    }
    let _parent = h.repo.p.lock()?;
    let _lock = orepo.p.lock()?;
    let old = crate::store_remote::epochs::overlay_mode(&orepo)?;
    if old != mode {
        let mut record = Record::new("overlay.mode");
        record.data = serde_json::json!({"mode":mode,"config":{"overlay.access-mode":mode}});
        if mode < old {
            record.data["config"]["overlay.revocation"] = serde_json::json!(ids::new_id());
        }
        orepo.commit_record(ctx, record)?;
    }
    if a.has("--json") {
        outln!(ctx, "{}", serde_json::json!({"name":name,"access_mode":mode}));
    } else {
        outln!(ctx, "overlay {name}: {} (takes effect on the next push)", mode.name());
    }
    Ok(0)
}

fn managers(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new(), args)?;
    let h = here(ctx)?;
    let name = a.pos.first().ok_or_else(|| exit(128, USAGE))?;
    visible_info(ctx, &h.repo, name)?;
    let orepo = h.repo.overlay_repo(name)?;
    let _parent_lock = h.repo.p.lock()?;
    let _lock = orepo.p.lock()?;
    let authority = crate::overlay_managers::of(&h.repo, name)?;
    if a.pos.len() == 1 {
        for machine in authority.machines {
            outln!(ctx, "machine:{machine}");
        }
        return Ok(0);
    }
    if !orepo.is_admin(ctx) {
        return Err(exit(1, "only the overlay owner can change key managers"));
    }
    authority.require(&ctx.store.machine.id)?;
    let mut machines = BTreeSet::new();
    for principal in &a.pos[1..] {
        let machine = principal
            .strip_prefix("machine:")
            .filter(|id| crate::model::is_uuid(id))
            .ok_or_else(|| exit(128, "use machine:<UUID> for each key manager"))?;
        if !crate::store_remote::overlays::trusted_keys(&h.repo)?.contains_key(machine) {
            return Err(exit(1, format!("key manager machine {machine} is not trusted")));
        }
        machines.insert(machine.to_string());
    }
    if !machines.contains(&ctx.store.machine.id) {
        return Err(exit(1, "a remaining key manager must sign manager removal"));
    }
    let change = crate::overlay_managers::Change {
        parent: h.repo.p.id.clone(),
        overlay: orepo.p.id.clone(),
        name: name.clone(),
        machines,
        previous: authority.previous.clone(),
        retained: crate::overlay_managers::retained(&authority, &orepo)?,
    };
    let data = serde_json::to_value(change)?;
    let mut pending = None;
    for line in orepo.p.export_jsonl()?.lines() {
        let record = crate::oplog::parse_line(line, None)?.record;
        if record.op == "overlay.managers"
            && record.machine == ctx.store.machine.id
            && record.data["previous"] == data["previous"]
        {
            if record.data["machines"] != data["machines"] {
                return Err(exit(1, "retry the pending manager change before another change"));
            }
            pending = Some(line.to_string());
        }
    }
    let signed = if let Some(signed) = pending {
        signed
    } else {
        let mut record = Record::new("overlay.managers");
        record.data = data;
        let record = orepo.p.append(record, &ctx.actor())?;
        orepo.p.db.record_line(&record.machine, record.seq)?.ok_or_else(|| exit(1, "manager record is missing"))?
    };
    if h.repo.need_overlay(name)?.hidden {
        return Ok(0);
    }
    let mut parent = Record::new("overlay.authority");
    parent.data = serde_json::json!({"name":name,"authority":signed});
    h.repo.commit_record(ctx, parent)?;
    Ok(0)
}

fn visible_info(ctx: &Ctx, repo: &Repo, name: &str) -> Result<crate::overlay::Info> {
    let info = repo.need_overlay(name)?;
    if info.hidden && !repo.reads_overlay(name, ctx.caller.uid, ctx.caller.gid)? {
        return Err(exit(1, format!("error: no overlay '{name}' in project {}", repo.p.name)));
    }
    Ok(info)
}

/// Public metadata only: never serialize a complete key or operation payload.
fn reader_detail(principal: &str) -> serde_json::Value {
    let kind = match principal.split_once(':').map(|(kind, _)| kind) {
        Some("user") => "user",
        Some("group") => "group",
        Some("machine") => "machine",
        Some("person") => "person",
        _ => "everyone",
    };
    // Person policy entries include an age recipient; the login alone identifies the reader.
    let principal = if kind == "person" { principal.split('=').next().unwrap_or(principal) } else { principal };
    serde_json::json!({"principal":principal,"kind":kind})
}

fn operation_detail(r: &Record) -> serde_json::Value {
    serde_json::json!({"id":r.id,"op":super::admin::operation_name(&r.op),"actor":r.actor,"time":r.time})
}

/// Signed records in their application order, independent of UUID spelling and machine export order.
fn own_records(repo: &Repo) -> Result<Vec<Record>> {
    repo.p.db.all_lines()?.iter().map(|line| Ok(crate::oplog::parse_line(line, None)?.record)).collect()
}

fn show_json(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--json"), args)?;
    if a.pos.len() != 1 || !a.paths.is_empty() {
        return Err(exit(128, USAGE));
    }
    let name = &a.pos[0];
    let h = here(ctx)?;
    let info = visible_info(ctx, &h.repo, name)?;
    let orepo = Repo::open(ctx, &format!("{}%{name}", h.repo.p.name))?;
    let records = own_records(&orepo)?;
    let readers: Vec<_> = orepo
        .policy()
        .access
        .iter()
        .filter(|(_, role)| **role >= Role::Reader)
        .map(|(principal, _)| reader_detail(principal))
        .collect();
    let setting = |suffix: &str| -> Result<Option<serde_json::Value>> {
        let key = format!("overlay.{name}.{suffix}");
        if info.hidden {
            orepo.p.db.overlay_config(&key)
        } else {
            h.repo.p.config_value(&key)
        }
    };
    let state = overlay_line(ctx, &h.repo, h.line.as_ref(), &info, name, &orepo)?.map(|l| l.head).unwrap_or(info.root);
    let value = serde_json::json!({
        "name":name,"hidden":info.hidden,"readers":readers,"access_mode":crate::store_remote::epochs::overlay_mode(&orepo)?,
        "managers":crate::overlay_managers::of(&h.repo, name)?.machines,
        "epochs":layr_model::overlays::epochs(&records),
        "patterns":setting("patterns")?.unwrap_or_else(|| serde_json::json!([])),
        "peers":setting("peers")?,"state":state,"properties":h.repo.overlay_props(name)?.to_json(),
    });
    outln!(ctx, "{}", serde_json::to_string(&value)?);
    Ok(0)
}

fn operations(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--ops").flag("--json"), args)?;
    if a.pos.len() != 1 || !a.paths.is_empty() {
        return Err(exit(128, USAGE));
    }
    let name = &a.pos[0];
    let h = here(ctx)?;
    visible_info(ctx, &h.repo, name)?;
    let orepo = Repo::open(ctx, &format!("{}%{name}", h.repo.p.name))?;
    let records = own_records(&orepo)?;
    if a.has("--json") {
        let rows: Vec<_> = records.iter().rev().map(operation_detail).collect();
        outln!(ctx, "{}", serde_json::to_string(&rows)?);
    } else {
        for r in records.iter().rev() {
            outln!(
                ctx,
                "{} {} {} {}",
                r.id,
                super::history::fmt_date(r.time, "iso"),
                r.actor.name,
                super::admin::operation_name(&r.op)
            );
        }
    }
    Ok(0)
}

fn show_to_project(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new(), args)?;
    let h = here(ctx)?;
    let name = a.pos.first().ok_or_else(|| exit(128, USAGE))?;
    let info = visible_info(ctx, &h.repo, name)?;
    let orepo = h.repo.overlay_repo(name)?;
    if !orepo.is_admin(ctx) {
        return Err(exit(1, "only the overlay owner can show it to the project"));
    }
    let _parent = h.repo.p.lock()?;
    let _lock = orepo.p.lock()?;
    let own_lines: Vec<_> = orepo.p.export_jsonl()?.lines().map(str::to_owned).collect();
    let creator = own_lines.iter().find(|line| {
        crate::oplog::parse_line(line, None).is_ok_and(|s| {
            s.record.op == "overlay.create" && s.record.data["config"][format!("overlay.{name}")]["hidden"] == true
        })
    });
    let Some(creator) = creator else {
        return Ok(0);
    };
    let authority = layr_model::authority::from_overlay_lines(
        &h.repo.p.id,
        &orepo.p.id,
        &own_lines,
        &crate::store_remote::overlays::trusted_keys(&h.repo)?,
    )?
    .1;
    authority.require(&ctx.store.machine.id)?;
    let mut metadata = orepo.p.db.overlay_config("overlay")?.ok_or_else(|| exit(1, "overlay metadata is missing"))?;
    let mut public =
        orepo.p.db.overlay_config(&format!("overlay.{name}"))?.ok_or_else(|| exit(1, "overlay creation is missing"))?;
    public["hidden"] = serde_json::json!(false);
    if metadata["hidden"] != false {
        metadata["hidden"] = serde_json::json!(false);
        let mut show = Record::new("overlay.show-to-project");
        show.data = serde_json::json!({"name":name,"config":{"overlay":metadata,format!("overlay.{name}"):public}});
        orepo.commit_record(ctx, show)?;
    }
    let parent_lines: Vec<_> = h.repo.p.export_jsonl()?.lines().map(str::to_owned).collect();
    let parent_records: Vec<_> =
        parent_lines.iter().map(|line| crate::oplog::parse_line(line, None).map(|s| s.record)).collect::<Result<_>>()?;
    if !parent_records.iter().any(|r| r.op == "overlay.create" && r.data["name"] == *name) {
        let mut create = Record::new("overlay.create");
        create.data = serde_json::json!({"name":name,"config":{format!("overlay.{name}"):public},"hidden_origin":creator});
        h.repo.commit_record(ctx, create)?;
    }
    for line in &own_lines {
        if crate::oplog::parse_line(line, None)?.record.op == "overlay.managers"
            && !parent_records.iter().any(|r| r.op == "overlay.authority" && r.data["authority"] == *line)
        {
            let mut proof = Record::new("overlay.authority");
            proof.data = serde_json::json!({"name":name,"authority":line});
            h.repo.commit_record(ctx, proof)?;
        }
    }
    for (key, value) in orepo.p.db.config_all()? {
        if key.starts_with(&format!("overlay.{name}.")) {
            let value: serde_json::Value = serde_json::from_str(&value)?;
            if h.repo.p.db.overlay_config(&key)?.as_ref() != Some(&value) {
                super::admin::set_config(ctx, &h.repo, &key, value)?;
            }
        }
    }
    // Public stacks start naming the overlay only after the show operation.
    for line in h.repo.lines()? {
        if orepo.p.db.line_by_name(&Repo::overlay_line_name(&info, &line.id, line.owner))?.is_some()
            && !orepo.p.db.overlay_config(&format!("hidden.stack.{}", line.id))?.is_some_and(|v| v["enabled"] == false)
        {
            let mut layers = h.repo.layers(&line)?;
            if !layers.iter().any(|l| l.overlay == *name) {
                let ol = orepo.p.db.line_by_name(&Repo::overlay_line_name(&info, &line.id, line.owner))?.unwrap();
                let position = orepo
                    .p
                    .db
                    .overlay_config(&format!("hidden.stack.{}", line.id))?
                    .and_then(|v| v["position"].as_u64())
                    .unwrap_or(u64::MAX);
                layers.insert((position as usize).min(layers.len()), Layer { overlay: name.clone(), state: ol.head });
                let mut record = Record::new("overlay.use");
                record.data = serde_json::json!({"stack":h.repo.public_stack_data(&line.id, &layers)?});
                h.repo.commit_record(ctx, record)?;
            }
        }
    }
    outln!(ctx, "Overlay {name} is visible to the project");
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
        let layers: Vec<_> = repo
            .layers(&line)?
            .into_iter()
            .filter(|l| {
                !repo.need_overlay(&l.overlay).map(|i| i.hidden).unwrap_or(true)
                    || repo.reads_overlay(&l.overlay, ctx.caller.uid, ctx.caller.gid).unwrap_or(false)
            })
            .collect();
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
        refuse_folder(&h.repo, n)?;
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
        return Err(exit(1, "error: the line has staged changes; save them or unstage them ('layr restore --staged') first"));
    }
    for (p, o) in repo.pending_moves(line) {
        if !layers.iter().any(|l| l.overlay == o) {
            return Err(exit(
                1,
                format!(
                    "error: '{p}' is to move to overlay '{o}' at the next state; save first, or 'layr overlay move --cancel {p}'"
                ),
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
    refuse_folder(repo, &name)?;
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
        let Ok(root) = repo.state_src(&st.id) else {
            continue;
        };
        if paths.iter().any(|p| crate::fsutil::lmeta(&root, p).is_some()) {
            holding.push(st.id);
        }
    }
    if a.has("--purge") && !holding.is_empty() {
        let mut deleted = Vec::new();
        for id in &holding {
            #[cfg(target_os = "macos")]
            let gone = crate::thin::delete_now(&repo.p, id).is_ok();
            #[cfg(not(target_os = "macos"))]
            let gone = repo.p.delete_state(id).is_ok();
            if gone {
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
            outln!(ctx, "move to overlay {name} at the next state: {}", h.rel(p));
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

fn overlay_line(
    ctx: &Ctx,
    repo: &Repo,
    line: Option<&crate::model::Line>,
    info: &crate::overlay::Info,
    name: &str,
    orepo: &Repo,
) -> Result<Option<crate::model::Line>> {
    // The overlay line of the current line; else the caller's own line, else the owner's.
    let mut names = Vec::new();
    if let Some(l) = line {
        if repo.layers(l)?.iter().any(|x| x.overlay == name) {
            names.push(Repo::overlay_line_name(info, &l.id, l.owner));
        }
    }
    if info.hidden {
        for line in orepo.lines()? {
            names.push(line.name);
        }
    }
    names.push(format!("user-{}", ctx.caller.uid));
    names.push(format!("user-{}", info.owner));
    Ok(names.into_iter().find_map(|n| orepo.p.db.line_by_name(&n).ok().flatten()))
}

/// A reading command on an overlay's history, as a reader of the overlay: `layr <cmd>` in the
/// overlay's own project, at the overlay line of the current line (or of the caller).
fn read(ctx: &Ctx, cmd: &str, args: &[String]) -> Result<i32> {
    let name = args.first().ok_or_else(|| exit(128, USAGE))?.clone();
    let h = here(ctx)?;
    let info = visible_info(ctx, &h.repo, &name)?;
    let pname = format!("{}%{name}", h.repo.p.name);
    // The overlay's own policy decides: a user who does not read it is refused here.
    let orepo = Repo::open(ctx, &pname)?;
    let olname = overlay_line(ctx, &h.repo, h.line.as_ref(), &info, &name, &orepo)?.map(|l| l.name);
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn operation_output_only_contains_public_fields_and_cli_names() {
        for (signed, plain) in [("commit", "state"), ("rebase.step", "replay.step"), ("overlay.key", "overlay.key")] {
            let mut r = Record::new(signed);
            r.data = json!({"identity":"secret","recipient":"key material"});
            assert_eq!(operation_detail(&r), json!({"id":r.id,"op":plain,"actor":r.actor,"time":r.time}));
        }
    }

    #[test]
    fn reader_details_identify_principals_without_key_material() {
        for (principal, kind) in [("user:1000", "user"), ("group:1000", "group"), ("*", "everyone"), ("machine:id", "machine")] {
            assert_eq!(reader_detail(principal), json!({"principal":principal,"kind":kind}));
        }
        assert_eq!(
            reader_detail("person:reader@example.com=age1recipient"),
            json!({"principal":"person:reader@example.com","kind":"person"})
        );
    }
}
