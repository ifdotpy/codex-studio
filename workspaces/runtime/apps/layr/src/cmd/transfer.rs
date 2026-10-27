//! Export states to folders outside the store.

use super::{exit, here};
use crate::args::{parse, Spec};
use crate::changes;
use crate::ctx::Ctx;
use crate::fsutil;
use crate::ids;
use crate::privs::as_caller_fs;
use crate::repo::skip_path;
use crate::revs;
use crate::storage as btrfs;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

/// Copy one entry from a state (read as root) to a folder of the caller (written as the caller).
fn put_entry(ctx: &Ctx, src_root: &Path, rel: &str, dst_root: &Path) -> Result<()> {
    let src = fsutil::safe_join(src_root, rel)?;
    let m = std::fs::symlink_metadata(&src)?;
    if m.file_type().is_symlink() {
        let t = std::fs::read_link(&src)?;
        return as_caller_fs(ctx, || {
            fsutil::make_parents(dst_root, rel, None)?;
            let d = fsutil::safe_join(dst_root, rel)?;
            if d.symlink_metadata().is_ok() {
                fsutil::remove_entry(dst_root, rel)?;
            }
            std::os::unix::fs::symlink(&t, &d)?;
            Ok(())
        });
    }
    if m.is_dir() {
        return as_caller_fs(ctx, || fsutil::make_parents(dst_root, &format!("{rel}/x"), None));
    }
    let mut s = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(&src)?;
    use std::os::unix::fs::MetadataExt;
    let mode = m.mode() & 0o777;
    let mut d = as_caller_fs(ctx, || {
        fsutil::make_parents(dst_root, rel, None)?;
        let d = fsutil::safe_join(dst_root, rel)?;
        if d.symlink_metadata().is_ok() {
            fsutil::remove_entry(dst_root, rel)?;
        }
        Ok(OpenOptions::new().write(true).create_new(true).custom_flags(libc::O_NOFOLLOW).mode(mode).open(&d)?)
    })?;
    if btrfs::clone_file(&s, &d).is_err() {
        std::io::copy(&mut s, &mut d)?;
    }
    d.set_permissions(std::fs::Permissions::from_mode(mode))?;
    fsutil::set_mtime(&d, m.mtime(), m.mtime_nsec())?;
    Ok(())
}

fn del_entry(ctx: &Ctx, root: &Path, rel: &str) -> Result<()> {
    as_caller_fs(ctx, || fsutil::remove_entry(root, rel))
}

/// Names that cannot coexist on APFS: equal after case folding and Unicode NFC.
fn collisions(paths: &[String]) -> BTreeSet<String> {
    let mut by: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in paths {
        let k: String = p.nfc().collect::<String>().to_lowercase();
        by.entry(k).or_default().push(p.clone());
    }
    by.into_values().filter(|v| v.len() > 1).flatten().collect()
}

fn state_files(root: &Path, tracked_only: bool) -> Result<Vec<String>> {
    let rules = crate::ignore_rules::Rules::new(root);
    let mut v = Vec::new();
    fsutil::walk(root, "", &mut |rel, m| {
        if skip_path(rel) || (tracked_only && rules.ignored(rel, m.is_dir())) {
            return Ok(false);
        }
        if !m.is_dir() {
            v.push(rel.to_string());
        }
        Ok(true)
    })?;
    Ok(v)
}

/// Make `dst` (a caller folder) hold `to`. With `from` (the state it holds now), copy only the
/// differences and the `stale` paths. Returns the paths skipped by the name rules.
fn sync_folder(
    ctx: &Ctx,
    from: Option<&Path>,
    to: &Path,
    dst: &Path,
    stale: &[String],
    tracked: bool,
) -> Result<(usize, Vec<String>)> {
    let all = state_files(to, tracked)?;
    let bad = collisions(&all);
    let rules = crate::ignore_rules::Rules::new(to);
    let mut n = 0;
    match from {
        Some(f) => {
            let mut paths: BTreeSet<String> = stale.iter().cloned().collect();
            for c in changes::file_changes(f, to)? {
                if skip_path(&c.path) {
                    continue;
                }
                if let Some(o) = &c.old_path {
                    paths.insert(o.clone());
                }
                paths.insert(c.path.clone());
            }
            for p in paths {
                if bad.contains(&p) {
                    continue;
                }
                // The same filter as a full copy: an ignored file is never written out.
                if tracked && rules.ignored(&p, false) {
                    if fsutil::lmeta(dst, &p).is_some() {
                        del_entry(ctx, dst, &p)?;
                    }
                    continue;
                }
                match fsutil::lmeta(to, &p) {
                    Some(m) if !m.is_dir() => put_entry(ctx, to, &p, dst)?,
                    _ => del_entry(ctx, dst, &p)?,
                }
                n += 1;
            }
        }
        None => {
            for p in &all {
                if bad.contains(p) {
                    continue;
                }
                put_entry(ctx, to, p, dst)?;
                n += 1;
            }
        }
    }
    Ok((n, bad.into_iter().collect()))
}

/// The state to copy: a revision, or the working content saved as an automatic state.
fn source_state(ctx: &Ctx, h: &super::Here, rev: Option<&str>) -> Result<String> {
    match rev {
        Some(r) => revs::resolve(&h.repo, h.line.as_ref(), r),
        None => {
            let l = h.line()?;
            h.repo.authorize_line(ctx, l, crate::repo::LineOp::Save)?;
            let _lock = h.repo.p.lock()?;
            let l = h.repo.line(&l.id)?;
            let s = h.repo.save(ctx, &l, "copy out", false, false)?;
            Ok(match s {
                Some(s) => s.id,
                None => h.repo.last_auto(&l)?.map(|s| s.id).unwrap_or(l.head.clone()),
            })
        }
    }
}

fn caller_path(ctx: &Ctx, p: &str) -> PathBuf {
    if p.starts_with('/') {
        PathBuf::from(p)
    } else {
        ctx.cwd.join(p)
    }
}

const MARKER: &str = ".layr-export.json";

pub fn export(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("--tracked").flag("--all").flag("-f|--force").flag("-q|--quiet");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    h.repo.require(ctx, "export")?;
    let (rev, folder) = match a.pos.len() {
        1 => (None, a.pos[0].clone()),
        2 => (Some(a.pos[0].clone()), a.pos[1].clone()),
        _ => return Err(exit(128, "usage: layr export [<rev>] <folder> [--all]")),
    };
    let dst = caller_path(ctx, &folder);
    let state = source_state(ctx, &h, rev.as_deref())?;
    crate::secrets::check_contents(ctx, &h.repo, &state)?;
    let to = h.repo.state_root(&state)?;
    as_caller_fs(ctx, || {
        std::fs::create_dir_all(&dst)?;
        Ok(())
    })?;
    let marker: Option<serde_json::Value> = as_caller_fs(ctx, || {
        let mut v = Vec::new();
        Ok(File::open(dst.join(MARKER)).and_then(|f| f.take(1 << 20).read_to_end(&mut v)).ok().map(|_| v))
    })
    .ok()
    .flatten()
    .and_then(|d| serde_json::from_slice(&d).ok());
    let prev = marker
        .as_ref()
        .filter(|m| m["project"].as_str() == Some(h.repo.p.id.as_str()))
        .and_then(|m| m["state"].as_str().map(|s| s.to_string()))
        .filter(|s| h.repo.p.has_state(s));
    let empty = as_caller_fs(ctx, || Ok(std::fs::read_dir(&dst)?.next().is_none()))?;
    if prev.is_none() && !empty && !a.has("-f") {
        return Err(exit(
            1,
            format!("error: {} is not empty and was not exported by layr; use --force to write into it", dst.display()),
        ));
    }
    let from = prev.as_ref().map(|p| h.repo.state_root(p)).transpose()?;
    // Ignored files (build output, local secrets) stay out unless --all.
    let (n, bad) = sync_folder(ctx, from.as_deref(), &to, &dst, &[], !a.has("--all"))?;
    let m = serde_json::json!({"project": h.repo.p.id, "project_name": h.repo.p.name, "state": state, "time": ids::now_ms()});
    as_caller_fs(ctx, || {
        let mut f = File::create(dst.join(MARKER))?;
        f.write_all(serde_json::to_string_pretty(&m)?.as_bytes())?;
        Ok(())
    })?;
    if !a.has("-q") {
        outln!(ctx, "exported state {} to {}: {n} path(s) written", ids::short(&state), dst.display());
    }
    if !bad.is_empty() {
        outln!(ctx, "not written (names differ only in case or Unicode form; they cannot coexist on APFS):");
        for b in bad {
            outln!(ctx, "\t{b}");
        }
        return Ok(1);
    }
    Ok(0)
}
