//! Addressable shared-state and overlay stacks.

use super::here;
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use anyhow::{bail, Result};

pub fn view(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--json"), args)?;
    let h = here(ctx)?;
    let save = a.pos.first().map(String::as_str) == Some("save");
    let show = a.pos.first().map(String::as_str) == Some("show");
    let rest = if save || show { &a.pos[1..] } else { &a.pos[..] };
    if rest.len() > 1 || a.dashdash {
        bail!("usage: layr view [save [<line>]|show <id>|<rev or line>] [--json]");
    }
    let _lock = h.repo.p.lock()?;
    let current = h.line.as_ref().map(|line| h.repo.line(&line.id)).transpose()?;
    let mut view = if show {
        let id = rest.first().ok_or_else(|| anyhow::anyhow!("usage: layr view show <id> [--json]"))?;
        h.repo.saved_view(id.strip_prefix("views/").unwrap_or(id))?
    } else if save {
        let line = match rest.first() {
            Some(name) => h.repo.line(name)?,
            None => current.clone().ok_or_else(|| anyhow::anyhow!("view save requires a line"))?,
        };
        h.repo.view_of(&line)?
    } else {
        crate::revs::resolve_view(&h.repo, current.as_ref(), rest.first().map(String::as_str).unwrap_or("head"))?
    };
    if show
        && view.layers.iter().any(|layer| {
            h.repo.need_overlay(&layer.overlay).is_ok_and(|i| i.hidden)
                && !h.repo.reads_overlay(&layer.overlay, ctx.caller.uid, ctx.caller.gid).unwrap_or(false)
        })
    {
        bail!("unknown view");
    }
    view.layers.retain(|layer| {
        !h.repo.need_overlay(&layer.overlay).is_ok_and(|i| i.hidden)
            || h.repo.reads_overlay(&layer.overlay, ctx.caller.uid, ctx.caller.gid).unwrap_or(false)
    });
    let id = if save { h.repo.save_view(ctx, &view)? } else { view.id()? };
    if a.has("--json") {
        outln!(ctx, "{}", serde_json::json!({"view_id":id,"view":view}));
    } else {
        outln!(ctx, "{}\t{}", id, serde_json::to_string(&view)?);
    }
    Ok(0)
}
