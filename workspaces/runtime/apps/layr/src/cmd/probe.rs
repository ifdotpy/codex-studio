//! Probe commands run only on explicit caller request.
use super::here;
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use anyhow::{bail, Result};

pub fn probe(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().value("--probe").flag("--json"), args)?;
    let h = here(ctx)?;
    match a.pos.first().map(String::as_str) {
        Some("run") if a.pos.len() == 2 => {
            let input = crate::probes::resolve(&h.repo, h.line.as_ref(), &a.pos[1])?;
            let results = crate::probes::run(ctx, &h.repo, &input, a.get("--probe"))?;
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::to_string(&results)?);
            } else {
                for r in &results {
                    outln!(
                        ctx,
                        "{}: {} exit {} (outputs {})",
                        r.probe,
                        if r.reused { "reused" } else { "ran" },
                        r.exit,
                        r.output_state
                    );
                }
                if results.is_empty() {
                    outln!(ctx, "No probes in this revision.");
                }
            }
            Ok(if results.iter().any(|r| r.exit != 0) { 1 } else { 0 })
        }
        Some("diff") if a.pos.len() == 3 && a.get("--probe").is_none() => {
            let base = crate::probes::resolve(&h.repo, h.line.as_ref(), &a.pos[1])?;
            let head = crate::probes::resolve(&h.repo, h.line.as_ref(), &a.pos[2])?;
            let diff = crate::probes::diff(&h.repo, &base, &head)?;
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::to_string(&diff)?);
            } else {
                crate::probes::print(ctx, &diff);
            }
            Ok(if diff.probes.iter().any(|p| p["limit_reasons"].as_array().is_some_and(|reasons| !reasons.is_empty())) {
                1
            } else {
                0
            })
        }
        _ => bail!("usage: layr probe run <rev> [--probe <name>] [--json] | layr probe diff <base> <head> [--json]"),
    }
}
