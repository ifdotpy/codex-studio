//! `layr overlay`: file trees with an owner, readers and their own history, shown in lines above
//! the shared set (docs/overlays.md; the model is in src/overlay.rs).

use super::{exit, here};
use crate::access::{parse_principal, show_principal, Policy, Role};
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::ids;
use crate::model::{Record, StateKind, StateRec};
use crate::overlay::{Layer, Mode};
use crate::repo::Repo;
use crate::storage as btrfs;
use crate::store::{check_name, Project};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

const USAGE: &str = "usage: layr overlay create <name> [--readers <principal>,...] [--mode shared|follow] [--hidden]
       layr overlay show-to-project <name>
       layr overlay list
       layr overlay readers <name> [<principal>...]
       layr overlay managers <name> [machine:<id>...]
       layr overlay use [--add|--remove] <name>... | --none
       layr overlay move [--purge] <name> <path>... | --cancel [<path>...]
       layr overlay show <name> --json
       layr overlay log <name> --ops [--json]
       layr overlay log|show|diff|grep|ls-files|blame <name> [<args>]";

pub fn overlay(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("list");
    let rest = args.get(1..).unwrap_or(&[]).to_vec();
    match sub {
        "create" => create(ctx, &rest),
        "list" => list(ctx, &rest),
        "show-to-project" => show_to_project(ctx, &rest),
        "readers" => readers(ctx, &rest),
        "managers" => managers(ctx, &rest),
        "use" => use_cmd(ctx, &rest),
        "move" => move_cmd(ctx, &rest),
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
fn access_map(owner: u32, readers: &[String]) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for r in readers {
        m.insert(r.clone(), serde_json::json!("reader"));
    }
    m.insert(format!("user:{owner}"), serde_json::json!("admin"));
    serde_json::Value::Object(m)
}

fn create(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().value("--readers|-r").value("--mode").flag("--hidden").flag("-q|--quiet");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    if repo.p.is_overlay() {
        return Err(exit(1, "error: an overlay has no overlays"));
    }
    let name = a.pos.first().ok_or_else(|| exit(128, USAGE))?.clone();
    check_name("overlay", &name)?;
    repo.require(ctx, "line.create")?;
    let hidden = a.has("--hidden");
    let mode = match a.get("--mode") {
        Some(m) => Mode::parse(m).ok_or_else(|| exit(128, format!("fatal: unknown mode '{m}' (shared or follow)")))?,
        None if hidden => Mode::Follow,
        None => Mode::Shared,
    };
    if hidden && mode != Mode::Follow {
        return Err(exit(128, "hidden overlays require follow mode"));
    }
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

fn list(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--json"), args)?;
    let mut rows = Vec::new();
    let h = here(ctx)?;
    let repo = &h.repo;
    let shown: BTreeSet<String> = match &h.line {
        Some(l) => repo.layers(l)?.into_iter().map(|x| x.overlay).collect(),
        None => BTreeSet::new(),
    };
    for name in repo.visible_overlay_names(ctx)? {
        let info = match repo.overlay_info(&name)? {
            Some(i) => i,
            None => continue,
        };
        let readers = repo.overlay_project(&name).map(|p| reader_list(&p)).unwrap_or_default();
        let mark = if shown.contains(&name) { "* " } else { "  " };
        let owner = crate::ctx::Caller::from_uid(info.owner).user;
        if a.has("--json") {
            rows.push(serde_json::json!({"name":name,"mode":info.mode.name(),"hidden":info.hidden,"owner":info.owner,"readers":readers,"shown":shown.contains(&name)}));
        } else {
            outln!(
                ctx,
                "{mark}{name}\t{}{}\towner {owner}\treaders {}",
                info.mode.name(),
                if info.hidden { " hidden" } else { "" },
                readers.join(",")
            );
        }
    }
    if a.has("--json") {
        outln!(ctx, "{}", serde_json::to_string(&rows)?);
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
        "name":name,"hidden":info.hidden,"readers":readers,
        "managers":crate::overlay_managers::of(&h.repo, name)?.machines,
        "epochs":layr_model::overlays::epochs(&records),
        "patterns":setting("patterns")?.unwrap_or_else(|| serde_json::json!([])),
        "peers":setting("peers")?,"state":state,
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
