//! A signed queue of exact states for the existing protected merge.

use super::{exit, here};
use crate::args::{parse, Spec};
use crate::ctx::{Caller, Ctx};
use crate::ids;
use crate::model::{QueueChange, QueueEntry, Record, RefChange, RefState};
use crate::repo::Repo;
use anyhow::{bail, Result};
use std::fs::File;

fn try_lock(repo: &Repo) -> Result<Option<File>> {
    let file = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(repo.p.meta_dir().join("queue.lock"))?;
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(file)),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn require_lock(repo: &Repo) -> Result<File> {
    try_lock(repo)?.ok_or_else(|| exit(1, "a queue runner is already active for this project"))
}

fn hold(entry: &QueueEntry) -> String {
    format!("holds/queue/{}", entry.id)
}

fn find(repo: &Repo, prefix: &str) -> Result<QueueEntry> {
    let entries: Vec<_> = repo
        .p
        .db
        .queue_entries(None)?
        .into_iter()
        .filter(|entry| entry.id == prefix || (prefix.len() >= 4 && ids::display(&entry.id).starts_with(prefix)))
        .collect();
    match entries.as_slice() {
        [entry] => Ok(entry.clone()),
        [] => bail!("unknown queue entry '{prefix}'"),
        _ => bail!("ambiguous queue entry '{prefix}'"),
    }
}

fn target(repo: &Repo, entry: &QueueEntry) -> Result<crate::model::Line> {
    let line = repo.line(&entry.target_line)?;
    if repo.p.db.line_by_name(&line.name)?.map(|l| l.id != entry.target_line).unwrap_or(true) {
        bail!("queue target line '{}' was deleted", entry.into);
    }
    Ok(line)
}

fn print_entry(ctx: &Ctx, entry: &QueueEntry) {
    outln!(
        ctx,
        "{}\t{}\t{}\t{}{}{}",
        entry.id,
        entry.status,
        entry.into,
        entry.state,
        entry.reason.as_ref().map(|r| format!("\t{r}")).unwrap_or_default(),
        entry
            .probes
            .as_ref()
            .map(|outcome| {
                let rows = outcome["probes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|probe| {
                        let errors = probe["errors"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|error| {
                                format!("; {}: {}", error["side"].as_str().unwrap_or(""), error["error"].as_str().unwrap_or(""))
                            })
                            .collect::<String>();
                        format!("{}: {}{errors}", probe["probe"].as_str().unwrap_or(""), probe["status"].as_str().unwrap_or(""))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("\tprobes: {rows}")
            })
            .unwrap_or_default()
    );
}

pub fn queue(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().value("--into").flag("--json"), args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let command = a.pos.first().map(String::as_str).unwrap_or("");
    let usage = "usage: layr queue (add <rev>|list|remove <entry>|run) [--into <line>] [--json]";
    match command {
        "add" if a.pos.len() == 2 => {
            repo.require(ctx, "merge")?;
            let state = crate::revs::resolve(repo, h.line.as_ref(), &a.pos[1])?;
            let source = repo.source_state(&state)?;
            repo.state_root(&state)?;
            let upstream = match h
                .line
                .as_ref()
                .map(|line| repo.p.config_value(&format!("line.{}.upstream", line.name)))
                .transpose()?
                .flatten()
            {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(name)) => Some(name),
                Some(_) => bail!("the line upstream must be a line name"),
            };
            let into = a.get("--into").or(upstream.as_deref()).unwrap_or("main");
            let line = repo.line(into)?;
            if !repo.working(&line).exists() {
                bail!("queue target line '{into}' has no local work folder");
            }
            let entry = QueueEntry {
                id: ids::new_id(),
                into: line.name,
                target_line: line.id,
                state: state.clone(),
                change_id: source.change_id,
                added_by: ctx.caller.uid,
                time: ids::now_ms(),
                status: "pending".into(),
                attempts: 0,
                result: None,
                reason: None,
                probes: None,
            };
            {
                let _lock = repo.p.lock()?;
                let mut rec = Record::new("queue.add");
                rec.data = serde_json::json!({"queue_change":QueueChange{id:entry.id.clone(),before:None,after:Some(entry.clone())},"ref_change":RefChange{name:hold(&entry),before:None,after:Some(RefState{name:hold(&entry),target:state,expires_at:None})}});
                repo.commit_record(ctx, rec)?;
            }
            if let Err(error) = run_configured(ctx, repo, Some(&entry.target_line)) {
                eprintln!("layr queue: {error:#}");
            }
            let entry = repo.p.db.queue_entry(&entry.id)?.unwrap_or(entry);
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::to_string(&entry)?);
            } else {
                print_entry(ctx, &entry);
            }
            Ok(0)
        }
        "list" if a.pos.len() == 1 => {
            let line = a.get("--into").map(|name| repo.line(name)).transpose()?;
            let entries = repo.p.db.queue_entries(line.as_ref().map(|line| line.id.as_str()))?;
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::to_string(&entries)?);
            } else {
                for entry in entries {
                    print_entry(ctx, &entry);
                }
            }
            Ok(0)
        }
        "remove" if a.pos.len() == 2 => {
            repo.require(ctx, "merge")?;
            let _queue_lock = require_lock(repo)?;
            let _lock = repo.p.lock()?;
            let entry = find(repo, &a.pos[1])?;
            if entry.added_by != ctx.caller.uid {
                repo.require(ctx, "line.manage")?;
            }
            let mut rec = Record::new("queue.remove");
            rec.data = serde_json::json!({"queue_change":QueueChange{id:entry.id.clone(),before:Some(entry.clone()),after:None}});
            release_hold(repo, &entry, &mut rec.data)?;
            repo.commit_record(ctx, rec)?;
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::json!({"removed":entry.id}));
            } else {
                outln!(ctx, "removed {}", entry.id);
            }
            Ok(0)
        }
        "run" if a.pos.len() == 1 => {
            let line = a.get("--into").map(|name| repo.line(name)).transpose()?;
            let entries = run_entries(ctx, repo, line.as_ref().map(|line| line.id.as_str()), false)?;
            let failed = entries.iter().any(|entry| entry.status == "failed");
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::to_string(&entries)?);
            } else {
                for entry in entries {
                    print_entry(ctx, &entry);
                }
            }
            Ok(if failed { 1 } else { 0 })
        }
        _ => Err(exit(128, usage)),
    }
}

fn release_hold(repo: &Repo, entry: &QueueEntry, data: &mut serde_json::Value) -> Result<()> {
    if let Some(before) = repo.p.db.ref_value(&hold(entry), ids::now_ms())? {
        data["ref_change"] = serde_json::to_value(RefChange { name: hold(entry), before: Some(before), after: None })?;
    }
    Ok(())
}

/// Called under the merge's project lock. Queue status and line movement share one record.
pub fn complete_data(
    repo: &Repo,
    entry: &QueueEntry,
    head: &str,
    attempts: u32,
    probes: Option<serde_json::Value>,
    mut data: serde_json::Value,
) -> Result<serde_json::Value> {
    if repo.p.db.queue_entry(&entry.id)?.as_ref() != Some(entry) {
        bail!("queue entry '{}' changed while the merge ran", entry.id);
    }
    let mut after = entry.clone();
    after.status = "done".into();
    after.result = Some(head.into());
    after.reason = None;
    after.attempts = attempts;
    after.probes = probes;
    data["queue_change"] =
        serde_json::to_value(QueueChange { id: entry.id.clone(), before: Some(entry.clone()), after: Some(after) })?;
    release_hold(repo, entry, &mut data)?;
    Ok(data)
}

fn run_entries(ctx: &Ctx, repo: &Repo, into: Option<&str>, skip_busy: bool) -> Result<Vec<QueueEntry>> {
    repo.require(ctx, "merge")?;
    let _queue_lock = match try_lock(repo)? {
        Some(lock) => lock,
        None if skip_busy => return Ok(Vec::new()),
        None => return Err(exit(1, "a queue runner is already active for this project")),
    };
    let merge_ctx = Ctx {
        store: crate::store::Store::open(&ctx.store.root)?,
        caller: ctx.caller.clone(),
        cwd: ctx.cwd.clone(),
        env: ctx.env.clone(),
        out: std::cell::RefCell::new(Box::new(std::io::sink())),
    };
    let merge_repo = Repo::open(&merge_ctx, &repo.p.name)?;
    let mut results = Vec::new();
    while let Some(entry) = repo.p.db.queue_entries(into)?.into_iter().find(|entry| entry.status == "pending") {
        let line = target(repo, &entry)?;
        // A caller who cannot update this line cannot fail other users' entries.
        repo.authorize_line(ctx, &line, crate::repo::LineOp::Merge { expect: true })?;
        let mut attempts = 0;
        let outcome = loop {
            attempts += 1;
            let line = target(&merge_repo, &entry)?;
            let (result, probes) = super::mergecmd::merge_queued(&merge_ctx, &merge_repo, &line, &entry, &line.head, attempts);
            if attempts == 1
                && result.as_ref().err().is_some_and(|error| error.downcast_ref::<super::mergecmd::TargetMoved>().is_some())
            {
                continue;
            }
            break (result, probes);
        };
        let (outcome, probes) = outcome;
        match outcome {
            Ok(0) => {}
            result => {
                let reason = match result {
                    Err(error) => format!("{error:#}"),
                    Ok(code) => format!("merge failed (exit {code})"),
                };
                let _lock = repo.p.lock()?;
                if repo.p.db.queue_entry(&entry.id)?.as_ref() != Some(&entry) {
                    bail!("queue entry '{}' changed", entry.id);
                }
                let mut after = entry.clone();
                after.status = "failed".into();
                after.reason = Some(reason);
                after.attempts = attempts;
                after.probes = probes;
                let mut rec = Record::new("queue.result");
                rec.data = serde_json::json!({"queue_change":QueueChange{id:entry.id.clone(),before:Some(entry.clone()),after:Some(after)}});
                repo.commit_record(ctx, rec)?;
            }
        }
        results.push(repo.p.db.queue_entry(&entry.id)?.ok_or_else(|| anyhow::anyhow!("queue entry disappeared"))?);
    }
    Ok(results)
}

/// The daemon and queue add use the target line owner, with the normal merge policy.
pub fn run_configured(ctx: &Ctx, repo: &Repo, only: Option<&str>) -> Result<()> {
    let configured = match repo.p.config_value("queue.run")? {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(value)) => value,
        Some(_) => bail!("queue.run must be a comma-separated list of line names"),
    };
    for name in configured.split(',').map(str::trim).filter(|name| !name.is_empty()) {
        let line = repo.line(name)?;
        if only.map(|id| id != line.id).unwrap_or(false) {
            continue;
        }
        let run_ctx = Ctx {
            store: crate::store::Store::open(&ctx.store.root)?,
            caller: Caller::from_uid(line.owner),
            cwd: repo.working(&line),
            env: std::collections::HashMap::new(),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
        };
        let run_repo = Repo::open(&run_ctx, &repo.p.name)?;
        run_entries(&run_ctx, &run_repo, Some(&line.id), true)?;
    }
    Ok(())
}
