//! Logical changes and the immutable states of each version.

use super::{here, history};
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::diff;
use crate::model::StateRec;
use crate::repo::{skip_path, Repo};
use anyhow::{bail, Result};

pub fn find(repo: &Repo, prefix: &str) -> Result<String> {
    let ids: Vec<String> = repo
        .p
        .db
        .changes()?
        .into_iter()
        .filter(|id| id == prefix || (prefix.len() >= 4 && crate::ids::display(id).starts_with(prefix)))
        .collect();
    match ids.as_slice() {
        [id] => Ok(id.clone()),
        [] => bail!("unknown change '{prefix}'"),
        _ => bail!("ambiguous change '{prefix}'"),
    }
}

pub fn version(value: &str) -> Result<usize> {
    let n = value
        .strip_prefix('v')
        .ok_or_else(|| anyhow::anyhow!("bad change version '{value}': use v1, v2, ..."))?
        .parse::<usize>()?;
    if n == 0 {
        bail!("change versions start at v1");
    }
    Ok(n - 1)
}

pub fn change(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--json").value("--from").value("--to"), args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let usage = "usage: layr change (list|show <id>|log <id>|diff <id> [--from vN] [--to vN]) [--json]";
    let command = a.pos.first().map(String::as_str).unwrap_or("");
    if command == "list" && a.pos.len() == 1 {
        let mut rows = Vec::new();
        for id in repo.p.db.changes()? {
            let versions = repo.p.db.change_versions(&id)?;
            if let Some(last) = versions.last() {
                rows.push(serde_json::json!({"change_id":id,"versions":versions.len(),"state":last.id,"message":last.message}));
            }
        }
        if a.has("--json") {
            outln!(ctx, "{}", serde_json::to_string(&rows)?);
        } else {
            for r in rows {
                outln!(
                    ctx,
                    "{}\tv{}\t{}",
                    r["change_id"].as_str().unwrap_or(""),
                    r["versions"],
                    r["message"].as_str().unwrap_or("").lines().next().unwrap_or("")
                );
            }
        }
        return Ok(0);
    }
    if a.pos.len() != 2 {
        bail!("{usage}");
    }
    let id = find(repo, &a.pos[1])?;
    let versions = repo.p.db.change_versions(&id)?;
    match command {
        "show" => {
            let mut rows = Vec::new();
            let model = repo.read_model()?;
            for (n, s) in versions.iter().enumerate() {
                let mut row = serde_json::json!({"version":format!("v{}",n+1),"state":s,"attestations":repo.p.db.attestations(&s.id,None)?});
                if let Some(version) = model.version_info(&s.id) {
                    row["operation"] = version;
                }
                rows.push(row);
            }
            let behavior = versions.last().map(|state| crate::probes::change_behavior(repo, state)).transpose();
            let behavior_json = match &behavior {
                Ok(Some(value)) => serde_json::to_value(value)?,
                Ok(None) => serde_json::json!({"probes": []}),
                Err(e) => serde_json::json!({"status":"unavailable","reason":format!("{e:#}")}),
            };
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::json!({"change_id":id,"versions":rows,"behavior":behavior_json}));
            } else {
                outln!(ctx, "change {id}");
                for r in rows {
                    outln!(
                        ctx,
                        "{}\t{}\t{}",
                        r["version"].as_str().unwrap_or(""),
                        r["state"]["id"].as_str().unwrap_or(""),
                        r["state"]["message"].as_str().unwrap_or("").lines().next().unwrap_or("")
                    );
                    for a in r["attestations"].as_array().into_iter().flatten() {
                        outln!(ctx, "  {}\t{}", a["kind"].as_str().unwrap_or(""), a["attestation_id"].as_str().unwrap_or(""));
                    }
                }
                outln!(ctx, "Behavior");
                match behavior {
                    Ok(Some(value)) => crate::probes::print(ctx, &value),
                    Ok(None) => outln!(ctx, "  No probes."),
                    Err(e) => outln!(ctx, "  unavailable: {e:#}"),
                }
            }
        }
        "log" => {
            let states: std::collections::BTreeSet<&str> = versions.iter().map(|s| s.id.as_str()).collect();
            let records: Vec<_> = repo
                .p
                .db
                .records(usize::MAX >> 1)?
                .into_iter()
                .filter(|r| {
                    r.states.iter().any(|s| states.contains(s.id.as_str()))
                        || r.data["attestation"]["change_id"].as_str() == Some(&id)
                })
                .collect();
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::to_string(&records)?);
            } else {
                for r in records {
                    outln!(ctx, "{}\t{}\t{}", r.id, history::fmt_date(r.time, "iso"), super::admin::operation_name(&r.op));
                }
            }
        }
        "diff" => {
            if versions.len() < 2 && (a.get("--from").is_none() || a.get("--to").is_none()) {
                bail!("change {id} has only one version");
            }
            let from = a.get("--from").map(version).transpose()?.unwrap_or_else(|| versions.len().saturating_sub(2));
            let to = a.get("--to").map(version).transpose()?.unwrap_or(versions.len() - 1);
            let old = versions.get(from).ok_or_else(|| anyhow::anyhow!("change {id} has no version v{}", from + 1))?;
            let new = versions.get(to).ok_or_else(|| anyhow::anyhow!("change {id} has no version v{}", to + 1))?;
            let (old, new) = (patch(repo, old)?, patch(repo, new)?);
            let delta = layr_model::interdiff::from_changes(&id, &format!("v{}", from + 1), &format!("v{}", to + 1), &old, &new);
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::to_string(&delta)?);
            } else {
                ctx.print(&delta.diff);
            }
        }
        _ => bail!("{usage}"),
    }
    Ok(0)
}

fn patch(repo: &Repo, state: &StateRec) -> Result<Vec<diff::FileDiff>> {
    repo.source_state(&state.id)?;
    let new = repo.state_root(&state.id)?;
    let empty = if state.parents.is_empty() {
        let path = std::env::temp_dir().join(format!("layr-change-{}", crate::ids::new_id()));
        std::fs::create_dir(&path)?;
        Some(crate::repo::Temp { path })
    } else {
        None
    };
    let old = match state.parents.first() {
        Some(p) => repo.state_root(p)?,
        None => empty.as_ref().unwrap().path.clone(),
    };
    let changes: Vec<_> = crate::changes::file_changes(&old, &new)?.into_iter().filter(|c| !skip_path(&c.path)).collect();
    diff::file_diffs(&old, &new, &changes)
}
