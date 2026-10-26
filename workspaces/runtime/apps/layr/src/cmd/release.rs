//! Attach, list and download built files of named releases.
use super::here;
use crate::{
    args::{parse, Spec},
    ctx::Ctx,
};
use anyhow::{bail, Context, Result};

pub fn release(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(
        &Spec::new().flag("--json").flag("--public").value("--remote").value("--name").value("--expect").value("-o|--output"),
        args,
    )?;
    let usage = "usage: layr release (attach release/<version> <path>... [--name <name>]|files release/<version>|get release/<version> [<name>] [-o <dir>]) [--remote <name>]";
    let command = a.pos.first().map(String::as_str).context(usage)?;
    let name = a.pos.get(1).context(usage)?;
    if !name.starts_with("release/") || !crate::model::is_ref(name) {
        bail!("release ref must start with release/");
    }
    let mut h = here(ctx)?;
    let mut store = crate::store_remote::open_named(a.get("--remote").unwrap_or("origin"), &h.repo)?;
    store.use_public_access(a.has("--public"));
    let files = match command {
        "attach" if a.pos.len() >= 3 => {
            if a.has("-o") || a.has("--public") {
                bail!("attach needs private access and does not take -o");
            }
            let expected = a.get("--expect").map(|rev| crate::revs::resolve(&h.repo, h.line.as_ref(), rev)).transpose()?;
            store.attach_release(ctx, &mut h.repo, name, &a.pos[2..], a.get("--name"), expected.as_deref())?.files
        }
        "files" | "get" if a.pos.len() <= if command == "files" { 2 } else { 3 } => {
            if a.has("--name") || a.has("--expect") {
                bail!("--name and --expect are only valid for attach");
            }
            let mut files = store.release_files(ctx, name)?;
            if let Some(selected) = a.pos.get(2) {
                files.retain(|f| &f.name == selected);
                if files.is_empty() {
                    bail!("release file '{selected}' does not exist");
                }
            }
            if command == "get" {
                let output = ctx.cwd.join(a.get("-o").unwrap_or("."));
                for file in &files {
                    store.get_release_file(ctx, file, &output)?;
                }
            }
            files
        }
        _ => bail!("{usage}"),
    };
    if a.has("--json") {
        outln!(ctx, "{}", serde_json::to_string(&files)?);
    } else {
        for file in files {
            outln!(ctx, "{}\t{}\t{}", file.name, file.size, file.sha256);
        }
    }
    Ok(0)
}
