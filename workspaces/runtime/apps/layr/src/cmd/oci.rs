//! OCI push integration. The generic remote dispatcher owns transport selection.

use super::{exit, here};
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::oci::{self, client::Registry};
use anyhow::{bail, Result};
use serde_json::json;

/// Temporary dispatch hook until the remote workstream integrates OCI transport selection.
pub fn dispatch(ctx: &Ctx, args: &[String]) -> Result<Option<i32>> {
    let mut skip = false;
    let remote = match args.iter().find(|s| {
        if skip {
            skip = false;
            return false;
        }
        if ["--runnable", "-m", "--message"].contains(&s.as_str()) {
            skip = true;
            return false;
        }
        !s.starts_with('-')
    }) {
        Some(n) => n,
        None => return Ok(None),
    };
    let h = here(ctx)?;
    let url = if remote.starts_with("oci://") {
        remote.clone()
    } else {
        match h.repo.p.config_value(&format!("remote.{remote}.url"))?.and_then(|v| v.as_str().map(str::to_owned)) {
            Some(u) if u.starts_with("oci://") => u,
            _ => return Ok(None),
        }
    };
    let a = parse(&Spec::new().value("--runnable").flag("--json"), args)?;
    if a.pos.len() != 2 {
        return Err(exit(128, "usage: layr push <oci-remote> <rev> [--runnable <name>] [--json]"));
    }
    let report = push_report(ctx, &url, &a.pos[1], a.get("--runnable"), Some(remote))?;
    print_report(ctx, &url, &report, a.has("--json"));
    Ok(Some(0))
}

/// Small public transport boundary for the generic remote workstream.
pub fn push(ctx: &Ctx, url: &str, rev: &str, runnable: Option<&str>, json: bool) -> Result<i32> {
    let h = here(ctx)?;
    let mut remote = None;
    for (key, raw) in h.repo.p.db.config_all()? {
        if let Some(name) = key.strip_prefix("cloud.remote.").and_then(|n| n.strip_suffix(".url")) {
            let value: serde_json::Value = serde_json::from_str(&raw)?;
            if value.as_str() == Some(url) {
                remote = Some(name.to_string());
                break;
            }
        }
    }
    let report = push_report(ctx, url, rev, runnable, remote.as_deref())?;
    print_report(ctx, url, &report, json);
    Ok(0)
}

fn print_report(ctx: &Ctx, url: &str, report: &serde_json::Value, json: bool) {
    if json {
        outln!(ctx, "{report}");
        return;
    }
    outln!(ctx, "Pushed {}:{} to {url}", report["runnable"].as_str().unwrap_or(""), report["tag"].as_str().unwrap_or(""));
    outln!(ctx, "Image: {}", report["image"].as_str().unwrap_or(""));
    if let Some(chart) = report["chart"].as_str() {
        outln!(ctx, "Chart: {chart}");
    }
    outln!(ctx, "Blobs: {} uploaded, {} skipped", report["uploaded_blobs"], report["skipped_blobs"]);
}

fn push_report(ctx: &Ctx, url: &str, rev: &str, runnable: Option<&str>, remote: Option<&str>) -> Result<serde_json::Value> {
    let h = here(ctx)?;
    h.repo.require(ctx, "push")?;
    h.repo.require(ctx, "export")?;
    let _lock = h.repo.p.lock()?;
    let e = oci::export(ctx, &h.repo, rev, h.line.as_ref(), runnable)?;
    // An image carries the selected tree only, so check its whole contents.
    crate::secrets::check_contents(ctx, &h.repo, &e.state)?;
    let cfg = |key: &str| -> Result<Option<String>> {
        Ok(match remote {
            Some(n) => h
                .repo
                .p
                .config_value(&format!("cloud.remote.{n}.{key}"))?
                .or(h.repo.p.config_value(&format!("remote.{n}.{key}"))?)
                .and_then(|v| v.as_str().map(str::to_owned)),
            None => None,
        })
    };
    let insecure = remote
        .map(|n| {
            h.repo
                .p
                .config_bool(&format!("cloud.remote.{n}.insecure"), h.repo.p.config_bool(&format!("remote.{n}.insecure"), false))
        })
        .unwrap_or(false);
    // Credentials come only from the caller's environment: project settings are signed records
    // that every machine and every store remote receives.
    for key in ["username", "password"] {
        if cfg(key)?.is_some() {
            bail!("remote setting '{key}' is not used: set LAYR_OCI_USERNAME and LAYR_OCI_PASSWORD in the environment and remove the setting");
        }
    }
    let username = ctx.env("LAYR_OCI_USERNAME").map(str::to_owned);
    let password = ctx.env("LAYR_OCI_PASSWORD").map(str::to_owned);
    let mut registry = Registry::new(url, insecure, username, password)?;
    let repo = registry.repository.clone();
    let tag = oci::revision_tag(&h.repo, rev, &e.state)?;
    let mut total = e.image.layers.len() + 1;
    let mut uploaded = registry.push(&repo, &tag, &e.cache, &e.image)?;
    let chart = if let Some(c) = &e.chart {
        total += c.layers.len() + 1;
        let (base, _) = repo.rsplit_once('/').unwrap_or(("", &repo));
        let target = if base.is_empty() { format!("charts/{}", e.runnable) } else { format!("{base}/charts/{}", e.runnable) };
        let version = e.chart_version.as_ref().unwrap().replace('+', "_");
        uploaded += registry.push(&target, &version, &e.cache, c)?;
        Some(c.manifest.digest.clone())
    } else {
        None
    };
    Ok(
        json!({"state": e.state, "runnable": e.runnable, "tag": tag, "image": e.image.manifest.digest, "chart": chart, "uploaded_blobs": uploaded, "skipped_blobs": total - uploaded}),
    )
}

/// Issue one opaque credential for a local principal and this machine. Only its hash is recorded.
pub fn token(ctx: &Ctx, args: &[String]) -> Result<i32> {
    use crate::access::parse_principal;
    use crate::oci::registry::TokenRecord;
    use rand::RngCore;
    use std::collections::BTreeMap;
    let a = parse(&Spec::new().flag("--revoke").value("--until").flag("--json"), args)?;
    if a.pos.len() != 1 {
        return Err(exit(128, "usage: layr access token <principal> [--until <duration>] [--json] | --revoke <principal>"));
    }
    let principal = parse_principal(&a.pos[0]).ok_or_else(|| exit(128, "unknown principal"))?;
    let uid: u32 = principal
        .strip_prefix("user:")
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| exit(128, "registry tokens require a user principal"))?;
    let h = here(ctx)?;
    h.repo.require(ctx, "access")?;
    let _lock = h.repo.p.lock()?;
    let mut tokens: BTreeMap<String, TokenRecord> =
        h.repo.p.config_value("registry.tokens")?.map(serde_json::from_value).transpose()?.unwrap_or_default();
    // Rotation also revokes credentials that this principal had on this machine.
    tokens.retain(|_, r| r.uid != uid || r.machine != ctx.store.machine.id);
    if a.has("--revoke") {
        super::admin::set_config(ctx, &h.repo, "registry.tokens", serde_json::to_value(tokens)?)?;
        if a.has("--json") {
            outln!(ctx, "{}", json!({"principal": principal, "revoked": true}));
        }
        return Ok(0);
    }
    let value = a.get("--until").unwrap_or("1d");
    if !value.is_ascii() {
        return Err(exit(128, "invalid token duration; use 30m, 2h or 1d"));
    }
    let duration = super::admin::duration_ms(value)?;
    if duration <= 0 {
        return Err(exit(128, "token duration must be positive"));
    }
    let until = crate::ids::now_ms().checked_add(duration).ok_or_else(|| exit(128, "token expiry is too large"))?;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let token = hex::encode(bytes);
    tokens.insert(oci::digest(token.as_bytes()), TokenRecord { uid, machine: ctx.store.machine.id.clone(), until });
    super::admin::set_config(ctx, &h.repo, "registry.tokens", serde_json::to_value(tokens)?)?;
    if a.has("--json") {
        outln!(ctx, "{}", json!({"principal": principal, "until": until, "token": token}));
    } else {
        outln!(ctx, "{token}");
    }
    Ok(0)
}
