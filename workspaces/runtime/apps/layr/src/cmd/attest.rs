//! Attestations are separate signed operation records, outside state identity.

use super::{exit, here};
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::ids;
use crate::model::{Attestation, Record, Subject, ATTESTATION_KINDS};
use crate::repo::Repo;
use anyhow::{bail, Result};
use sha2::{Digest, Sha256};

/// The project appender stamps the signer, record id and time before it signs the record.
pub fn record(repo: &Repo, state: &str, kind: &str, payload: &[u8]) -> Result<Record> {
    record_subject(repo, Subject { kind: "state".into(), id: state.into() }, kind, payload)
}

pub fn record_view(repo: &Repo, view: &crate::overlay::View, kind: &str, payload: &[u8]) -> Result<Record> {
    record_subject(repo, Subject { kind: "view".into(), id: view.id()? }, kind, payload)
}

fn record_subject(repo: &Repo, subject: Subject, kind: &str, payload: &[u8]) -> Result<Record> {
    if !ATTESTATION_KINDS.contains(&kind) {
        bail!("unknown attestation kind '{kind}'");
    }
    let change_id = if subject.kind == "state" {
        repo.state(&subject.id)?.change_id
    } else {
        repo.saved_view(&subject.id)?;
        None
    };
    let mut rec = Record::new("attest");
    rec.data = serde_json::json!({"attestation": Attestation {
        format: 1, attestation_id: ids::new_id(), subject, change_id,
        kind: kind.into(), payload_digest: hex::encode(Sha256::digest(payload)),
        payload_ref: None, signer: String::new(), time: 0,
    }});
    Ok(rec)
}

fn subject(repo: &Repo, line: Option<&crate::model::Line>, rev: &str) -> Result<Subject> {
    if let Some(id) = rev.strip_prefix("views/") {
        repo.saved_view(id)?;
        Ok(Subject { kind: "view".into(), id: id.into() })
    } else {
        Ok(Subject { kind: "state".into(), id: crate::revs::resolve(repo, line, rev)? })
    }
}

pub fn attest(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().value("--payload").flag("--json"), args)?;
    if a.pos.len() != 2 {
        bail!("usage: layr attest <kind> <rev> [--payload <file>] [--json]");
    }
    let h = here(ctx)?;
    let kind = &a.pos[0];
    // Readers may report their own evidence. Existing line rules decide whose evidence counts.
    h.repo.require(ctx, "try")?;
    if kind == "quarantine" {
        h.repo.require(ctx, "gc")?;
    }
    let subject = subject(&h.repo, h.line.as_ref(), &a.pos[1])?;
    let payload = match a.get("--payload") {
        Some(file) => super::work::read_caller_file(ctx, file, 8 << 20)?,
        None => Vec::new(),
    };
    let _lock = h.repo.p.lock()?;
    let mut rec = record_subject(&h.repo, subject.clone(), kind, &payload)?;
    if kind == "revoke" {
        let ids = h.repo.p.db.own_attestations(&subject.id, &h.repo.p.store.machine.id, ctx.caller.uid, None)?;
        rec.data["revokes"] = serde_json::json!(ids);
    }
    let rec = h.repo.commit_record(ctx, rec)?;
    if a.has("--json") {
        outln!(ctx, "{}", rec.data["attestation"]);
    } else {
        outln!(ctx, "{}\t{}\t{}", rec.data["attestation"]["attestation_id"].as_str().unwrap_or(""), kind, subject.id);
    }
    Ok(0)
}

pub fn attestations(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().value("--kind").flag("--json"), args)?;
    if a.pos.len() != 1 {
        return Err(exit(128, "usage: layr attestations <rev> [--kind <kind>] [--json]"));
    }
    if let Some(kind) = a.get("--kind") {
        if !ATTESTATION_KINDS.contains(&kind) {
            bail!("unknown attestation kind '{kind}'");
        }
    }
    let h = here(ctx)?;
    let subject = subject(&h.repo, h.line.as_ref(), &a.pos[0])?;
    let v = h.repo.p.db.attestations(&subject.id, a.get("--kind"))?;
    if a.has("--json") {
        outln!(ctx, "{}", serde_json::to_string(&v)?);
    } else {
        for a in v {
            outln!(ctx, "{}\t{}\t{}\t{}", a.attestation_id, a.kind, a.signer, super::history::fmt_date(a.time, "iso"));
            if a.kind == "check" {
                if let Some(r) = h.repo.p.db.record(&a.attestation_id)? {
                    let result = r.data.get("check").unwrap_or(&r.data);
                    if let Some(command) = result["command"].as_str() {
                        outln!(ctx, "  {} exit {}: {}", r.actor.name, result["exit"], command);
                    }
                }
            }
        }
    }
    Ok(0)
}
