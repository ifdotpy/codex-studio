//! Search a first-parent history, optionally including automatic states or ref positions.

use super::here;
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::model::{StateKind, StateRec};
use crate::repo::Repo;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Serialize)]
struct Step {
    state: String,
    exit: i32,
    reused: bool,
}

fn cached(repo: &Repo, state: &str, argv: &[String]) -> Result<Option<i32>> {
    Ok(repo.p.db.check_exit(state, argv, None)?.and_then(|code| i32::try_from(code).ok()))
}

fn check(ctx: &Ctx, repo: &Repo, state: &str, argv: &[String], json: bool, steps: &mut Vec<Step>) -> Result<i32> {
    let previous = cached(repo, state, argv)?;
    let code = match previous {
        Some(code) => code,
        None => super::admin::try_state(ctx, repo, state, argv, json, json)?,
    };
    steps.push(Step { state: state.to_string(), exit: code, reused: previous.is_some() });
    if !json {
        outln!(ctx, "{} {}{}", state, if code == 0 { "good" } else { "bad" }, if previous.is_some() { " (reused)" } else { "" });
    }
    Ok(code)
}

fn first_parent(repo: &Repo, good: &str, bad: &str) -> Result<Vec<StateRec>> {
    let mut states = Vec::new();
    let mut current = bad.to_string();
    let mut seen = BTreeSet::new();
    loop {
        if !seen.insert(current.clone()) {
            bail!("cycle in first-parent history");
        }
        let state = repo.source_state(&current)?;
        let parent = state.parents.first().cloned();
        states.push(state);
        if current == good {
            break;
        }
        current = parent.context("--good is not on the first-parent history of --bad")?;
    }
    states.reverse();
    Ok(states)
}

pub fn run(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().value("--good").value("--bad").flag("--auto").value("--ref-log").flag("--json"), args)?;
    if a.pos == ["list"] && a.paths.is_empty() {
        let h = here(ctx)?;
        let rows = h.repo.read_model()?.bisect_results();
        if a.has("--json") {
            outln!(ctx, "{}", serde_json::to_string(&rows)?);
        } else {
            for row in rows {
                outln!(ctx, "{} {}", row["id"].as_str().unwrap_or(""), row["first_bad"].as_str().unwrap_or(""));
            }
        }
        return Ok(0);
    }
    if a.pos != ["run"] || a.paths.is_empty() {
        bail!("usage: layr bisect run [--good <rev>] [--bad <rev>] [--auto] [--ref-log <ref>] [--json] -- <cmd>");
    }
    let h = here(ctx)?;
    let repo = &h.repo;
    repo.require(ctx, "try")?;
    if a.has("--auto") && a.has("--ref-log") {
        bail!("--auto and --ref-log select different histories; use one");
    }
    let mut positions = Vec::new();
    if let Some(name) = a.get("--ref-log") {
        for change in repo.p.db.ref_log(name)?.into_iter().rev() {
            if let Some(position) = change.after {
                positions.push(position.target);
            }
        }
        if positions.is_empty() {
            bail!("ref '{name}' has no positions");
        }
    }
    let bad = match a.get("--bad") {
        Some(rev) => crate::revs::resolve(repo, h.line.as_ref(), rev)?,
        None if !positions.is_empty() => positions.last().unwrap().clone(),
        None => {
            let _lock = repo.p.lock()?;
            let line = repo.line(&h.line()?.id)?;
            repo.authorize_line(ctx, &line, crate::repo::LineOp::Save)?;
            repo.save(ctx, &line, "bisect bad (working content)", false, true)?
                .context("the line has no working folder; use --bad")?
                .id
        }
    };
    let good = match a.get("--good") {
        Some(rev) => crate::revs::resolve(repo, h.line.as_ref(), rev)?,
        None => {
            let history = if positions.is_empty() {
                repo.first_parent_chain(&h.line()?.head, None)?
            } else {
                let end = positions.iter().rposition(|s| s == &bad).context("--bad is not a ref position")?;
                positions[..end].iter().rev().cloned().collect()
            };
            let mut found = None;
            for id in history {
                if cached(repo, &id, &a.paths)? == Some(0) {
                    found = Some(id);
                    break;
                }
            }
            found.context("no ancestor has a passing check for this command; use --good <rev>")?
        }
    };
    if good == bad {
        bail!("--good and --bad must name different states");
    }
    let mut states = if positions.is_empty() {
        first_parent(repo, &good, &bad)?
    } else {
        let end = positions.iter().rposition(|s| s == &bad).context("--bad is not a ref position")?;
        let start = positions[..end].iter().rposition(|s| s == &good).context("--good must be an earlier ref position")?;
        positions[start..=end].iter().map(|id| repo.source_state(id)).collect::<Result<Vec<_>>>()?
    };
    if a.has("--auto") {
        let low = states.first().unwrap().time;
        let high = states.last().unwrap().time;
        let mut autos = repo.p.db.line_states(&h.line()?.id, StateKind::Auto)?;
        autos.retain(|s| s.time >= low && s.time <= high && s.id != good && s.id != bad);
        states.extend(autos);
        states.sort_by(|a, b| (a.time, &a.id).cmp(&(b.time, &b.id)));
        states.dedup_by(|a, b| a.id == b.id);
        // The explicit bounds remain endpoints, including states made in the same millisecond.
        states.retain(|s| s.id != good && s.id != bad);
        states.insert(0, repo.state(&good)?);
        states.push(repo.state(&bad)?);
    }
    let json = a.has("--json");
    let mut steps = Vec::new();
    if check(ctx, repo, &good, &a.paths, json, &mut steps)? != 0 {
        bail!("--good fails this command");
    }
    if check(ctx, repo, &bad, &a.paths, json, &mut steps)? == 0 {
        bail!("--bad passes this command");
    }
    let mut lo = 0;
    let mut hi = states.len() - 1;
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if check(ctx, repo, &states[mid].id, &a.paths, json, &mut steps)? == 0 {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let first = &states[hi];
    let origins: Vec<String> = repo.p.db.attestations(&first.id, Some("origin"))?.into_iter().map(|a| a.payload_digest).collect();
    let result = serde_json::json!({"good":good,"bad":bad,"first_bad":first.id,"argv":a.paths,"state":first.id,"message":first.message,"kind":crate::db::kind_str(first.kind),"origin":origins,"steps":steps});
    {
        let _lock = repo.p.lock()?;
        let mut rec = crate::model::Record::new("bisect.run");
        rec.data = result.clone();
        if let Some(line) = &h.line {
            rec.data["line"] = serde_json::json!(line.id);
        }
        repo.commit_record(ctx, rec)?;
    }
    if json {
        outln!(ctx, "{}", result);
    } else {
        outln!(
            ctx,
            "First bad state: {} ({})",
            first.id,
            match first.kind {
                StateKind::Commit => "state",
                StateKind::Stash => "park",
                kind => crate::db::kind_str(kind),
            }
        );
        outln!(ctx, "{}", first.message);
        for origin in origins {
            outln!(ctx, "origin {origin}");
        }
    }
    Ok(0)
}
