//! Remotes per layer (docs/cache-overlays.md, section 7).
//!
//! A layr remote (store or peer) takes the whole repository: the shared set and every overlay.
//! A git remote takes one layer: the shared set goes to the project's git remotes, an overlay to
//! the git remotes in its own records (`overlay.<name>.git.<remote>`, which only its readers
//! see). A folder overlay (a cache) never goes to git. `layr push` without a remote name sends
//! the current line to all of them; `--dry-run` lists layer, remote, kind and size.

use super::{exit, here};
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::gitbridge;
use crate::ids;
use crate::model::{Line, Record};
use crate::repo::Repo;
use anyhow::Result;
use std::collections::BTreeMap;

/// An overlay's git remote: its URL and, for a later slice, a branch (docs/cache-overlays.md,
/// section 7: the planned orphan branch of the same repository).
pub struct OverlayRemote {
    pub name: String,
    pub url: String,
    pub branch: Option<String>,
}

/// The git remotes of an overlay, from its own records.
pub fn overlay_remotes(repo: &Repo, overlay: &str) -> Result<Vec<OverlayRemote>> {
    let orepo = repo.overlay_repo(overlay)?;
    let prefix = format!("overlay.{overlay}.git.");
    let mut out: BTreeMap<String, OverlayRemote> = BTreeMap::new();
    for (key, _) in orepo.p.db.config_all()? {
        let Some(rest) = key.strip_prefix(&prefix) else { continue };
        let name = rest.split('@').next().unwrap_or("").to_string();
        if name.is_empty() || out.contains_key(&name) {
            continue;
        }
        let Some(v) = orepo.p.db.overlay_config(&format!("{prefix}{name}"))? else { continue };
        let Some(url) = v["url"].as_str() else { continue };
        out.insert(name.clone(), OverlayRemote { name, url: url.to_string(), branch: v["branch"].as_str().map(str::to_owned) });
    }
    Ok(out.into_values().collect())
}

/// `layr overlay remote add <overlay> <remote> <url> | list <overlay> | remove <overlay> <remote>`.
pub fn overlay_remote(ctx: &Ctx, args: &[String]) -> Result<i32> {
    const USAGE: &str =
        "usage: layr overlay remote add <overlay> <remote> <git url> | list <overlay> | remove <overlay> <remote>";
    let a = parse(&Spec::new(), args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let (sub, overlay) = match (a.pos.first(), a.pos.get(1)) {
        (Some(s), Some(o)) => (s.as_str(), o.clone()),
        _ => return Err(exit(128, USAGE)),
    };
    let info = repo.need_overlay(&overlay)?;
    if !repo.reads_overlay(&overlay, ctx.caller.uid, ctx.caller.gid)? {
        if info.hidden {
            return Err(exit(1, format!("error: no overlay '{overlay}' in project {}", repo.p.name)));
        }
        return Err(exit(1, format!("permission denied: you do not read overlay '{overlay}'")));
    }
    let orepo = repo.overlay_repo(&overlay)?;
    match sub {
        "list" => {
            for r in overlay_remotes(repo, &overlay)? {
                outln!(ctx, "{}\t{}", r.name, r.url);
            }
            Ok(0)
        }
        "add" | "remove" => {
            if !orepo.is_admin(ctx) {
                return Err(exit(1, format!("permission denied: only the owner of overlay '{overlay}' sets its remotes")));
            }
            let name = a.pos.get(2).ok_or_else(|| exit(128, USAGE))?;
            crate::store::check_name("remote", name)?;
            let value = if sub == "add" {
                if repo.overlay_setting(&overlay, "folder")?.is_some() {
                    return Err(exit(
                        1,
                        format!("error: overlay '{overlay}' is a folder overlay (a cache); caches never go to git"),
                    ));
                }
                let url = a.pos.get(3).ok_or_else(|| exit(128, USAGE))?;
                if url.starts_with('-') || url.chars().any(char::is_control) {
                    return Err(exit(128, "fatal: invalid git URL"));
                }
                serde_json::json!({"url": url, "branch": null})
            } else {
                serde_json::Value::Null
            };
            let _lock = orepo.p.lock()?;
            let mut rec = Record::new("config");
            rec.data = serde_json::json!({"config": {format!("overlay.{overlay}.git.{name}"): value}});
            orepo.commit_record(ctx, rec)?;
            Ok(0)
        }
        _ => Err(exit(128, USAGE)),
    }
}

/// One row of `layr push --dry-run`.
struct Row {
    layer: String,
    remote: String,
    kind: &'static str,
    size: String,
}

/// The git remotes of the project (the shared set).
fn git_remotes(repo: &Repo) -> Result<Vec<String>> {
    Ok(repo
        .p
        .db
        .config_all()?
        .into_iter()
        .filter_map(|(k, _)| k.strip_prefix("remote.").and_then(|s| s.strip_suffix(".url")).map(str::to_owned))
        .collect())
}

/// The layr remotes of the project (stores and peers; registries take one revision only).
fn layr_remotes(repo: &Repo) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for (k, v) in repo.p.db.config_all()? {
        let Some(name) = k.strip_prefix("cloud.remote.").and_then(|s| s.strip_suffix(".url")) else { continue };
        let url: String = serde_json::from_str(&v).unwrap_or_default();
        if !matches!(super::remote::kind(&url), Ok(super::remote::Kind::Registry)) {
            out.push(name.to_string());
        }
    }
    Ok(out)
}

/// States of `repo` from `head` back that `pushed` (a state the remote has) does not reach.
fn new_states(repo: &Repo, head: &str, pushed: Option<&str>) -> Result<usize> {
    const LIMIT: usize = 10_000;
    let mut have = std::collections::BTreeSet::new();
    if let Some(p) = pushed {
        let mut stack = vec![p.to_string()];
        while let Some(s) = stack.pop() {
            if have.len() < LIMIT && have.insert(s.clone()) {
                stack.extend(repo.p.db.state(&s)?.map(|r| r.parents).unwrap_or_default());
            }
        }
    }
    let (mut seen, mut stack) = (std::collections::BTreeSet::new(), vec![head.to_string()]);
    while let Some(s) = stack.pop() {
        if have.contains(&s) || seen.len() >= LIMIT || !seen.insert(s.clone()) {
            continue;
        }
        stack.extend(repo.p.db.state(&s)?.map(|r| r.parents).unwrap_or_default());
    }
    Ok(seen.len())
}

/// The newest state that a push of `repo` sent to `remote`/`branch`.
fn pushed_state(repo: &Repo, remote: &str, branch: &str) -> Result<Option<String>> {
    Ok(repo.p.db.exports()?.into_iter().rev().find(|e| e.2 == remote && e.3 == branch && e.5 == "pushed").map(|e| e.1))
}

fn git_size(repo: &Repo, head: &str, remote: &str, branch: &str) -> Result<String> {
    let n = new_states(repo, head, pushed_state(repo, remote, branch)?.as_deref())?;
    let bytes = crate::cache_overlay::tree_size(&repo.state_root(head)?);
    Ok(format!("{n} new state(s), tree {}", human(bytes)))
}

fn human(n: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let (mut v, mut i) = (n as f64, 0);
    while v >= 1024.0 && i + 1 < units.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", units[i])
    }
}

/// The state of overlay `name` that line `line` shows or saved: its overlay line's head.
fn overlay_head(repo: &Repo, line: &Line, name: &str) -> Result<Option<String>> {
    let info = repo.need_overlay(name)?;
    let ol = repo.overlay_project(name)?.db.line_by_name(&Repo::overlay_line_name(&info, &line.id, line.owner))?;
    Ok(ol.map(|l| l.head).filter(|h| *h != info.root))
}

/// Export the history of an overlay state to its git store and push it to `url` `branch`.
fn push_overlay_git(ctx: &Ctx, repo: &Repo, name: &str, remote: &OverlayRemote, state: &str, branch: &str) -> Result<String> {
    let orepo = repo.overlay_repo(name)?;
    let _lock = orepo.p.lock()?;
    gitbridge::init_store(&orepo)?;
    let (commit, made) = gitbridge::export_history(&orepo, state)?;
    let commits: serde_json::Map<String, serde_json::Value> =
        made.into_iter().map(|(s, c)| (s, serde_json::Value::String(c))).collect();
    let eid = ids::new_id();
    let mut rec = Record::new("export");
    rec.data = serde_json::json!({"export": eid, "state": state, "remote": remote.name, "branch": branch, "commit": commit, "commits": commits, "status": "prepared"});
    orepo.commit_record(ctx, rec)?;
    gitbridge::push(ctx, &orepo, &remote.url, &commit, branch, false)?;
    let mut rec = Record::new("export");
    rec.data = serde_json::json!({"export": eid, "state": state, "remote": remote.name, "branch": branch, "commit": commit, "status": "pushed"});
    orepo.commit_record(ctx, rec)?;
    Ok(commit)
}

/// `layr push [--dry-run]` without a remote name: every layer to each of its remotes.
pub fn push_all(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("-n|--dry-run").flag("-q|--quiet"), args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = h.line()?.clone();
    let dry = a.has("-n");
    let mut rows = Vec::new();
    let layr = layr_remotes(repo)?;
    let git = git_remotes(repo)?;
    // The repository to each layr remote: the shared set and every overlay, caches included.
    for r in &layr {
        rows.push(Row { layer: "repository".into(), remote: r.clone(), kind: "layr", size: "all layers".into() });
        for (name, folder) in repo.folder_overlays()? {
            if let Some(head) = overlay_head(repo, &line, &name)? {
                let orepo = repo.overlay_repo(&name)?;
                let platform = orepo.p.db.state(&head)?.and_then(|s| s.cache_platform).unwrap_or_default();
                let bytes = crate::cache_overlay::tree_size(&orepo.p.state_path(&head));
                rows.push(Row {
                    layer: format!("{name} ({folder})"),
                    remote: r.clone(),
                    kind: "layr",
                    size: format!("1 state, {} ({platform})", human(bytes)),
                });
            }
        }
    }
    // The shared set to each git remote of the project.
    for r in &git {
        rows.push(Row {
            layer: "shared".into(),
            remote: r.clone(),
            kind: "git",
            size: git_size(repo, &line.head, r, &line.name)?,
        });
    }
    // Each overlay to its own git remotes; folder overlays never.
    let mut overlay_pushes = Vec::new();
    for name in repo.visible_overlay_names(ctx)? {
        if repo.overlay_setting(&name, "folder")?.is_some() || !repo.reads_overlay(&name, ctx.caller.uid, ctx.caller.gid)? {
            continue;
        }
        let Some(head) = overlay_head(repo, &line, &name)? else { continue };
        let orepo = repo.overlay_repo(&name)?;
        for r in overlay_remotes(repo, &name)? {
            let branch = r.branch.clone().unwrap_or_else(|| line.name.clone());
            rows.push(Row {
                layer: name.clone(),
                remote: r.name.clone(),
                kind: "git",
                size: git_size(&orepo, &head, &r.name, &branch)?,
            });
            overlay_pushes.push((name.clone(), r, head.clone(), branch));
        }
    }
    if dry {
        outln!(ctx, "layer\tremote\tkind\tsize");
        for r in &rows {
            outln!(ctx, "{}\t{}\t{}\t{}", r.layer, r.remote, r.kind, r.size);
        }
        return Ok(0);
    }
    if rows.is_empty() {
        return Err(exit(1, "error: no remotes; add one with layr remote add or layr git remote add"));
    }
    let quiet: Vec<String> = if a.has("-q") { vec!["-q".into()] } else { Vec::new() };
    let mut failed = 0;
    for r in &layr {
        let mut v = vec![r.clone()];
        v.extend(quiet.clone());
        if let Err(e) = super::remote::push(ctx, &v) {
            eprintln!("error: push to {r}: {e:#}");
            failed += 1;
        }
    }
    for r in &git {
        let mut v = vec![r.clone(), line.name.clone()];
        v.extend(quiet.clone());
        if let Err(e) = super::remote::git::push(ctx, &v) {
            eprintln!("error: git push to {r}: {e:#}");
            failed += 1;
        }
    }
    for (name, r, head, branch) in overlay_pushes {
        match push_overlay_git(ctx, repo, &name, &r, &head, &branch) {
            Ok(c) if !a.has("-q") => outln!(ctx, "overlay {name} -> {} {branch}: {}", r.name, &c[..10]),
            Ok(_) => {}
            Err(e) => {
                eprintln!("error: overlay {name} to {}: {e:#}", r.name);
                failed += 1;
            }
        }
    }
    Ok(if failed > 0 { 1 } else { 0 })
}
