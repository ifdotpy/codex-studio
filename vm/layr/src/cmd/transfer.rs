//! Copies outside the store: export, host_exec slots, backups and their restore.

use super::{exit, here};
use crate::args::{parse, Spec};
use crate::btrfs;
use crate::changes;
use crate::ctx::Ctx;
use crate::fsutil;
use crate::ids;
use crate::model::{Line, Record, StateKind};
use crate::privs::as_caller_fs;
use crate::repo::{skip_path, Repo};
use crate::revs;
use crate::store::Project;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
            h.repo.can_write_line(ctx, l)?;
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
    let (rev, folder) = match a.pos.len() {
        1 => (None, a.pos[0].clone()),
        2 => (Some(a.pos[0].clone()), a.pos[1].clone()),
        _ => return Err(exit(128, "usage: layr export [<rev>] <folder> [--all]")),
    };
    let dst = caller_path(ctx, &folder);
    let state = source_state(ctx, &h, rev.as_deref())?;
    let to = h.repo.state_root(&state)?;
    as_caller_fs(ctx, || {
        std::fs::create_dir_all(&dst)?;
        Ok(())
    })?;
    let marker: Option<serde_json::Value> = as_caller_fs(ctx, || Ok(std::fs::read(dst.join(MARKER)).ok()))
        .ok()
        .flatten()
        .and_then(|d| serde_json::from_slice(&d).ok());
    let prev = marker
        .as_ref()
        .filter(|m| m["project"].as_str() == Some(h.repo.p.id.as_str()))
        .and_then(|m| m["state"].as_str().map(|s| s.to_string()))
        .filter(|s| h.repo.p.state_path(s).exists());
    let empty = as_caller_fs(ctx, || Ok(std::fs::read_dir(&dst)?.next().is_none()))?;
    if prev.is_none() && !empty && !a.has("-f") {
        return Err(exit(
            1,
            format!("error: {} is not empty and was not exported by layr; use --force to write into it", dst.display()),
        ));
    }
    let from = prev.as_ref().map(|p| h.repo.p.state_path(p));
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

pub fn slot(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    let rest = args.get(1..).unwrap_or(&[]).to_vec();
    let spec = Spec::new().flag("-f|--force").value("--paths-from").flag("--all").flag("-q|--quiet").value("--into");
    let a = parse(&spec, &rest)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let dir = a.pos.first().ok_or_else(|| exit(128, "usage: layr slot (sync|collect|status|forget) <slot-folder> ..."))?;
    let dst = caller_path(ctx, dir);
    let key = dst.to_string_lossy().into_owned();
    let held = repo.p.db.slot(&key)?;
    let held_stale: Vec<String> = {
        let raw: Option<String> = repo
            .p
            .db
            .conn
            .query_row(
                "SELECT raw FROM records WHERE op='slot' AND json_extract(raw,'$.r.data.path')=?1 ORDER BY time DESC LIMIT 1",
                [&key],
                |r| r.get(0),
            )
            .ok();
        raw.and_then(|l| crate::oplog::parse_line(&l, None).ok())
            .and_then(|s| s.record.data.get("stale").cloned())
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    };
    match sub {
        "sync" => {
            let state = source_state(ctx, &h, a.pos.get(1).map(|s| s.as_str()))?;
            let to = repo.state_root(&state)?;
            as_caller_fs(ctx, || Ok(std::fs::create_dir_all(&dst)?))?;
            let from = held.as_ref().map(|(s, _)| repo.p.state_path(s)).filter(|p| p.exists());
            if from.is_none() {
                let empty = as_caller_fs(ctx, || Ok(std::fs::read_dir(&dst)?.next().is_none()))?;
                if !empty {
                    if !a.has("-f") {
                        return Err(exit(
                            1,
                            format!(
                                "error: slot {} is not empty and its state is unknown; use --force for a full copy",
                                dst.display()
                            ),
                        ));
                    }
                    let names: Vec<String> = as_caller_fs(ctx, || {
                        Ok(std::fs::read_dir(&dst)?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
                    })?;
                    for n in names {
                        del_entry(ctx, &dst, &n)?;
                    }
                }
            }
            let started = std::time::Instant::now();
            let (n, bad) = sync_folder(ctx, from.as_deref(), &to, &dst, &held_stale, false)?;
            let _lock = repo.p.lock()?;
            let mut rec = Record::new("slot");
            rec.data = serde_json::json!({"path": key, "state": state, "line": h.line.as_ref().map(|l| l.id.clone()), "stale": bad, "copied": n});
            repo.commit_record(ctx, rec)?;
            if !a.has("-q") {
                outln!(
                    ctx,
                    "slot {} holds state {}: {n} path(s) copied in {:.2} s",
                    dst.display(),
                    ids::short(&state),
                    started.elapsed().as_secs_f64()
                );
            }
            if !bad.is_empty() {
                outln!(ctx, "not copied (names differ only in case or Unicode form):");
                for b in bad {
                    outln!(ctx, "\t{b}");
                }
            }
            Ok(0)
        }
        "status" => {
            match held {
                Some((s, _)) => outln!(ctx, "slot {} holds state {}", dst.display(), ids::short(&s)),
                None => outln!(ctx, "slot {} holds no known state", dst.display()),
            }
            Ok(0)
        }
        "forget" => {
            let _lock = repo.p.lock()?;
            let mut rec = Record::new("slot");
            rec.data = serde_json::json!({"path": key, "state": null});
            repo.commit_record(ctx, rec)?;
            Ok(0)
        }
        "collect" => {
            let (held_state, _) =
                held.ok_or_else(|| exit(1, "error: the slot holds no known state; run 'layr slot sync' first"))?;
            let base = repo.state_root(&held_state)?;
            let line = match a.get("--into") {
                Some(n) => repo.line(n)?,
                None => h.line()?.clone(),
            };
            // The changed paths, from the Mac side (FSEvents) or by a full compare.
            let mut paths: Vec<String> = a.pos[1..].iter().map(|p| p.trim_start_matches("./").to_string()).collect();
            if let Some(f) = a.get("--paths-from") {
                let data = super::work::read_caller_file(ctx, f)?;
                for l in String::from_utf8_lossy(&data).lines() {
                    let l = l.trim();
                    if !l.is_empty() {
                        paths.push(l.trim_start_matches("./").to_string());
                    }
                }
            }
            if a.has("--all") {
                let mut all = BTreeSet::new();
                as_caller_fs(ctx, || {
                    fsutil::walk(&dst, "", &mut |rel, m| {
                        if !m.is_dir() && rel != MARKER {
                            all.insert(rel.to_string());
                        }
                        Ok(true)
                    })
                })?;
                for p in state_files(&base, false)? {
                    all.insert(p);
                }
                paths.extend(all);
            }
            paths.sort();
            paths.dedup();
            for p in &paths {
                fsutil::check_rel(p)?;
            }
            collect(ctx, repo, &line, &dst, &key, &held_state, &base, &paths, a.has("-q"))
        }
        _ => Err(exit(128, "usage: layr slot (sync|collect|status|forget) <slot-folder> ...")),
    }
}

/// Copy changes made in a slot back into a line. Three versions per path: the state the slot
/// held (base), the slot (theirs) and the line's current content (ours). If ours changed too,
/// the result keeps both (text markers, or `<path>.slot-conflict` for binary files).
#[allow(clippy::too_many_arguments)]
fn collect(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    slot: &Path,
    key: &str,
    held: &str,
    base: &Path,
    paths: &[String],
    quiet: bool,
) -> Result<i32> {
    let (_lock, w) = super::work::begin_change(ctx, repo, line)?;
    let line = repo.line(&line.id)?;
    let scan = repo.scan(&line)?;
    // Read the slot side as the caller into a private temporary copy.
    let tmp = repo.p.work_dir().join(ids::new_id());
    btrfs::create_subvolume(&tmp)?;
    let _t = crate::repo::Temp { path: tmp.clone() };
    for p in paths {
        let src = fsutil::safe_join(slot, p)?;
        let meta = as_caller_fs(ctx, || Ok(std::fs::symlink_metadata(&src).ok()))?;
        match meta {
            Some(m) if m.file_type().is_symlink() => {
                let t = as_caller_fs(ctx, || Ok(std::fs::read_link(&src)?))?;
                fsutil::make_parents(&tmp, p, None)?;
                std::os::unix::fs::symlink(t, fsutil::safe_join(&tmp, p)?)?;
            }
            Some(m) if m.is_file() => {
                let data = as_caller_fs(ctx, || Ok(std::fs::read(&src)?))?;
                use std::os::unix::fs::MetadataExt;
                fsutil::write_file(&tmp, p, &data, m.mode() & 0o777, None)?;
            }
            _ => {}
        }
    }
    let wdir = repo.working(&line);
    let own = repo.own(&line);
    let (mut copied, mut conflicts) = (Vec::new(), Vec::new());
    for p in paths {
        if skip_path(p) || p == MARKER {
            continue;
        }
        if fsutil::same_entry(base, p, &tmp, p, None)? {
            continue;
        }
        if fsutil::same_entry(&scan.path, p, &tmp, p, None)? {
            continue;
        }
        if fsutil::same_entry(base, p, &scan.path, p, None)? {
            match fsutil::lmeta(&tmp, p) {
                Some(_) => fsutil::copy_entry(&tmp, p, &wdir, p, own)?,
                None => fsutil::remove_entry(&wdir, p)?,
            }
            copied.push(p.clone());
            continue;
        }
        // Both changed.
        let text = |r: &Path| {
            fsutil::lmeta(r, p).map(|m| m.is_file()).unwrap_or(false)
                && !fsutil::is_binary(&fsutil::read_file(r, p).unwrap_or_default())
        };
        if text(base) && text(&scan.path) && text(&tmp) {
            let ours = std::env::temp_dir().join(format!("layr-ours-{}", ids::new_id()));
            std::fs::copy(fsutil::safe_join(&scan.path, p)?, &ours)?;
            let st = Command::new("git")
                .args(["merge-file", "-L", "line", "-L", "slot base", "-L", "slot"])
                .arg(&ours)
                .arg(fsutil::safe_join(base, p)?)
                .arg(fsutil::safe_join(&tmp, p)?)
                .status()?;
            let data = std::fs::read(&ours)?;
            let _ = std::fs::remove_file(&ours);
            let mode = fsutil::lmeta(&scan.path, p).map(|m| m.mode).unwrap_or(0o644);
            fsutil::write_file(&wdir, p, &data, mode, own)?;
            if st.code().unwrap_or(1) == 0 {
                copied.push(p.clone());
            } else {
                conflicts.push(format!("{p} (content: markers written)"));
            }
        } else {
            let side = format!("{p}.slot-conflict");
            if fsutil::lmeta(&tmp, p).is_some() {
                fsutil::copy_entry(&tmp, p, &wdir, &side, own)?;
            }
            conflicts.push(format!("{p} (kept the line version; the slot version is {side})"));
        }
    }
    let mut rec = Record::new("slot.collect");
    rec.lines.insert(
        line.id.clone(),
        crate::model::LineChange {
            before: Some(repo.line_state(&line, &line.head)),
            after: Some(repo.line_state(&line, &line.head)),
            working_before: w,
            working_after: None,
        },
    );
    rec.data = serde_json::json!({"path": key, "state": held, "copied": copied, "conflicts": conflicts});
    repo.commit_record(ctx, rec)?;
    // The slot now differs from the held state at these paths: copy them again on the next sync.
    let mut stale: Vec<String> = paths.to_vec();
    stale.sort();
    let mut rec = Record::new("slot");
    rec.data = serde_json::json!({"path": key, "state": held, "line": line.id, "stale": stale});
    repo.commit_record(ctx, rec)?;
    if !quiet {
        outln!(ctx, "collected {} path(s) from the slot into line {}", copied.len(), line.name);
    }
    if !conflicts.is_empty() {
        outln!(ctx, "conflicts (the line also changed these paths):");
        for c in &conflicts {
            outln!(ctx, "\t{c}");
        }
        return Ok(1);
    }
    Ok(0)
}

// ----- backups -----

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct Item {
    seq: u32,
    kind: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    file: String,
    sha256: String,
    bytes: u64,
    #[serde(default)]
    tips: Vec<String>,
    time: i64,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct Manifest {
    format: u32,
    /// The machine that wrote the backup (its settings are taken over on restore).
    #[serde(default)]
    machine: String,
    project: String,
    project_id: String,
    chain: u32,
    created: i64,
    items: Vec<Item>,
}

fn chain_dirs(root: &Path) -> Result<Vec<(u32, PathBuf)>> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if let Some(x) = n.strip_prefix("chain-") {
                if let Ok(k) = x.parse() {
                    v.push((k, e.path()));
                }
            }
        }
    }
    v.sort();
    Ok(v)
}

/// A manifest item names a plain file inside the chain folder: no folders, no `..`, no
/// absolute paths. Every read and delete of a backup goes through this check.
fn safe_item_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 200
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '@')
}

fn read_manifest(dir: &Path) -> Result<Manifest> {
    // A regular file, not through a link, at most 64 MiB.
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join("manifest.json"))
        .with_context(|| format!("read {}/manifest.json", dir.display()))?;
    if !f.metadata()?.is_file() {
        bail!("{}/manifest.json is not a regular file", dir.display());
    }
    let mut d = Vec::new();
    f.take(64 << 20).read_to_end(&mut d)?;
    let m: Manifest = serde_json::from_slice(&d)?;
    for i in &m.items {
        if !safe_item_name(&i.file) {
            bail!("{}/manifest.json: item file name '{}' is not a plain file name", dir.display(), i.file);
        }
    }
    Ok(m)
}

fn next_seq(m: &Manifest) -> u32 {
    m.items.iter().map(|i| i.seq).max().unwrap_or(0) + 1
}

/// A file name that no earlier run of the chain used.
fn unique_name(dir: &Path, m: &Manifest, seq: u32, tail: &str) -> String {
    let mut n = format!("{seq:05}-{tail}");
    let mut k = 1;
    while dir.join(&n).exists() || m.items.iter().any(|i| i.file == n) {
        n = format!("{seq:05}-{k}-{tail}");
        k += 1;
    }
    n
}

fn write_manifest(dir: &Path, m: &Manifest) -> Result<()> {
    fsutil::atomic_write(&dir.join("manifest.json"), serde_json::to_string_pretty(m)?.as_bytes(), 0o644)
}

/// A new file for backup output, created as the writer (never through an existing name).
fn create_out(path: &Path, writer: (u32, u32)) -> Result<File> {
    crate::privs::as_user_fs(writer.0, writer.1, || {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::remove_file(path);
        Ok(std::fs::OpenOptions::new().write(true).create_new(true).custom_flags(libc::O_NOFOLLOW).mode(0o644).open(path)?)
    })
}

fn finish_out(tmp: &Path, out: &Path, writer: (u32, u32)) -> Result<()> {
    crate::privs::as_user_fs(writer.0, writer.1, || {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(tmp)?.sync_all()?;
        std::fs::rename(tmp, out)?;
        Ok(())
    })
}

/// Run a producer command (as root: it reads the store) and compress its output with zstd
/// into `out`, a file created as the writer, then `fsync`.
fn compress_to(mut producer: Command, out: &Path, writer: (u32, u32)) -> Result<()> {
    let tmp = out.with_extension("partial");
    let f = create_out(&tmp, writer)?;
    let mut p = producer.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("start producer")?;
    let pout = p.stdout.take().unwrap();
    let z = Command::new("zstd").args(["-q", "-3", "-c"]).stdin(pout).stdout(f).status().context("run zstd")?;
    let ps = p.wait_with_output()?;
    if !ps.status.success() || !z.success() {
        let _ = crate::privs::as_user_fs(writer.0, writer.1, || Ok(std::fs::remove_file(&tmp)?));
        bail!("backup stream failed: {}", String::from_utf8_lossy(&ps.stderr).trim());
    }
    finish_out(&tmp, out, writer)
}

/// Write a backup into `root` as `writer` (uid, gid): every file and folder in the backup is
/// created with the writer's permissions; the store is read as root.
pub fn run_backup(ctx: &Ctx, repo: &Repo, root: &Path, full: bool, writer: (u32, u32)) -> Result<(u32, usize)> {
    let w = |f: &mut dyn FnMut() -> Result<()>| crate::privs::as_user_fs(writer.0, writer.1, f);
    let _lock = repo.p.lock()?;
    // Working content of every local line, as automatic states.
    for l in repo.lines()? {
        if l.machine == repo.p.store.machine.id && repo.working(&l).exists() {
            repo.save(ctx, &l, "backup", true, false)?;
        }
    }
    let proot = root.join(format!("{}-{}", repo.p.name, &repo.p.id[..8]));
    w(&mut || Ok(std::fs::create_dir_all(&proot)?))?;
    let chains = crate::privs::as_user_fs(writer.0, writer.1, || chain_dirs(&proot))?;
    let week = 7 * 86400 * 1000;
    let (num, dir, mut man) = match chains.last() {
        Some((n, d)) if !full => {
            let m = crate::privs::as_user_fs(writer.0, writer.1, || read_manifest(d))?;
            if ids::now_ms() - m.created > week {
                (n + 1, proot.join(format!("chain-{:04}", n + 1)), None)
            } else {
                (*n, d.clone(), Some(m))
            }
        }
        Some((n, _)) => (n + 1, proot.join(format!("chain-{:04}", n + 1)), None),
        None => (1, proot.join("chain-0001"), None),
    };
    w(&mut || Ok(std::fs::create_dir_all(&dir)?))?;
    let mut m = man.take().unwrap_or(Manifest {
        format: 1,
        machine: repo.p.store.machine.id.clone(),
        project: repo.p.name.clone(),
        project_id: repo.p.id.clone(),
        chain: num,
        created: ids::now_ms(),
        items: vec![],
    });
    let mut sent: BTreeSet<String> = m.items.iter().filter_map(|i| i.state.clone()).collect();
    let mut order: Vec<(i64, String)> = repo
        .p
        .db
        .present_states()?
        .into_iter()
        .filter(|(_, d)| !d)
        .filter_map(|(id, _)| repo.p.db.state(&id).ok().flatten().map(|s| (s.time, id)))
        .collect();
    order.sort();
    let mut count = 0;
    let mut last_sent: Option<String> = m.items.iter().rev().find_map(|i| i.state.clone());
    for (_, id) in order {
        if sent.contains(&id) || !repo.p.state_path(&id).exists() {
            continue;
        }
        let s = repo.state(&id)?;
        let parent = s.parents.iter().find(|p| sent.contains(*p)).cloned().or(last_sent.clone());
        let seq = next_seq(&m);
        let file = unique_name(&dir, &m, seq, &format!("{}.stream.zst", ids::short(&id)));
        let pp = parent.as_ref().map(|p| repo.p.state_path(p));
        compress_to(btrfs::send_command(pp.as_deref(), &repo.p.state_path(&id)), &dir.join(&file), writer)?;
        let (sha, bytes) = file_sum(&dir.join(&file), writer)?;
        m.items.push(Item {
            seq,
            kind: "state".into(),
            state: Some(id.clone()),
            parent,
            file,
            sha256: sha,
            bytes,
            tips: vec![],
            time: ids::now_ms(),
        });
        w(&mut || write_manifest(&dir, &m))?;
        sent.insert(id.clone());
        last_sent = Some(id);
        count += 1;
    }
    // The git object store, as bundles (each one needs the previous ones).
    let gs = crate::gitbridge::store(repo);
    if gs.join("objects").is_dir() {
        let tips: Vec<String> = crate::gitbridge::git_out(&gs, &["for-each-ref", "--format=%(objectname)"])?
            .lines()
            .map(|s| s.to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let prev: Vec<String> = m.items.iter().filter(|i| i.kind == "git").flat_map(|i| i.tips.clone()).collect();
        let new: Vec<&String> = tips.iter().filter(|t| !prev.contains(t)).collect();
        if !tips.is_empty() && !new.is_empty() {
            let seq = next_seq(&m);
            let file = unique_name(&dir, &m, seq, "git.bundle");
            // git writes the bundle to stdout; the file is created as the writer.
            let mut args =
                vec!["-c".to_string(), "safe.directory=*".into(), "bundle".into(), "create".into(), "-".into(), "--all".into()];
            for p in &prev {
                args.push(format!("^{p}"));
            }
            let tmp = dir.join(format!("{file}.partial"));
            let out = create_out(&tmp, writer)?;
            let st = Command::new("git").env("GIT_DIR", &gs).args(&args).stdout(out).stderr(Stdio::null()).status()?;
            if st.success() {
                finish_out(&tmp, &dir.join(&file), writer)?;
                let (sha, bytes) = file_sum(&dir.join(&file), writer)?;
                m.items.push(Item {
                    seq,
                    kind: "git".into(),
                    state: None,
                    parent: None,
                    file,
                    sha256: sha,
                    bytes,
                    tips: tips.clone(),
                    time: ids::now_ms(),
                });
                w(&mut || write_manifest(&dir, &m))?;
            } else {
                let _ = w(&mut || Ok(std::fs::remove_file(&tmp)?));
            }
        }
    }
    // The operation records as JSONL: the whole history of the project.
    let seq = next_seq(&m);
    let file = unique_name(&dir, &m, seq, "records.jsonl.zst");
    let jsonl = repo.p.export_jsonl()?;
    {
        let tmp = dir.join(format!("{file}.partial"));
        let f = create_out(&tmp, writer)?;
        let mut z = Command::new("zstd").args(["-q", "-3", "-c"]).stdin(Stdio::piped()).stdout(f).spawn().context("run zstd")?;
        z.stdin.take().unwrap().write_all(jsonl.as_bytes())?;
        if !z.wait()?.success() {
            bail!("zstd failed");
        }
        finish_out(&tmp, &dir.join(&file), writer)?;
    }
    let (sha, bytes) = file_sum(&dir.join(&file), writer)?;
    // Only the newest copy of the records is needed.
    let old_logs: Vec<String> = m.items.iter().filter(|i| i.kind == "records").map(|i| i.file.clone()).collect();
    m.items.retain(|i| i.kind != "records");
    m.items.push(Item {
        seq,
        kind: "records".into(),
        state: None,
        parent: None,
        file,
        sha256: sha,
        bytes,
        tips: vec![],
        time: ids::now_ms(),
    });
    w(&mut || write_manifest(&dir, &m))?;
    w(&mut || {
        for f in &old_logs {
            if safe_item_name(f) && m.items.iter().all(|i| &i.file != f) {
                let _ = std::fs::remove_file(dir.join(f));
            }
        }
        File::open(&dir)?.sync_all()?;
        // Retention: the newest two chains.
        let chains = chain_dirs(&proot)?;
        if chains.len() > 2 {
            for (_, d) in &chains[..chains.len() - 2] {
                let _ = std::fs::remove_dir_all(d);
            }
        }
        Ok(())
    })?;
    let mut rec = Record::new("backup");
    rec.data = serde_json::json!({"dir": dir.to_string_lossy(), "chain": num, "states": count});
    repo.commit_record(ctx, rec)?;
    Ok((num, count))
}

fn file_sum(p: &Path, writer: (u32, u32)) -> Result<(String, u64)> {
    crate::privs::as_user_fs(writer.0, writer.1, || Ok((fsutil::sha256_file(p)?, std::fs::symlink_metadata(p)?.len())))
}

fn verify_chain(dir: &Path) -> Result<Manifest> {
    let m = read_manifest(dir)?;
    for i in &m.items {
        let p = dir.join(&i.file);
        let sha = fsutil::sha256_file(&p).with_context(|| format!("missing {}", p.display()))?;
        if sha != i.sha256 {
            bail!("{}: checksum mismatch", i.file);
        }
    }
    Ok(m)
}

pub fn backup(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    let (sub, rest) = match sub {
        "verify" | "list" | "restore" | "run" => (sub, args[1..].to_vec()),
        _ => ("run", args.to_vec()),
    };
    let spec = Spec::new().flag("--full").value("--as").value("--chain").flag("-q|--quiet");
    let a = parse(&spec, &rest)?;
    match sub {
        "run" => {
            let h = here(ctx)?;
            h.repo.require_lead(ctx, "make backups")?;
            let dir = match a.pos.first() {
                Some(d) => caller_path(ctx, d),
                None => match h.repo.p.config_value("backup.dir")?.and_then(|v| v.as_str().map(PathBuf::from)) {
                    Some(d) => d,
                    None => return Err(exit(128, "usage: layr backup <folder> (or set backup.dir)")),
                },
            };
            if !ctx.caller.is_root() {
                crate::privs::check_caller_writable(ctx, &dir)?;
            }
            let (n, c) = run_backup(ctx, &h.repo, &dir, a.has("--full"), (ctx.caller.uid, ctx.caller.gid))?;
            if !a.has("-q") {
                outln!(ctx, "backup chain {n}: {c} new state stream(s), logs and git objects written to {}", dir.display());
            }
            Ok(0)
        }
        "list" | "verify" => {
            let root = caller_path(ctx, a.pos.first().ok_or_else(|| exit(128, "usage: layr backup verify <folder>"))?);
            // The folder belongs to the caller: read it with the caller's permissions.
            crate::privs::as_caller_fs(ctx, || list_or_verify(ctx, &root, sub == "verify"))
        }
        "restore" => {
            if !ctx.caller.is_root() {
                return Err(exit(1, "error: restore needs root (it receives btrfs streams)"));
            }
            let root = caller_path(
                ctx,
                a.pos.first().ok_or_else(|| exit(128, "usage: layr backup restore <folder> [--as <project>] [--chain N]"))?,
            );
            restore(ctx, &root, a.get("--as"), a.get("--chain").map(|c| c.parse()).transpose()?, a.has("-q"))
        }
        _ => unreachable!(),
    }
}

fn list_or_verify(ctx: &Ctx, root: &Path, verify: bool) -> Result<i32> {
    let mut bad = 0;
    for e in std::fs::read_dir(root)?.flatten() {
        for (_, d) in chain_dirs(&e.path())? {
            match if verify { verify_chain(&d) } else { read_manifest(&d) } {
                Ok(m) => {
                    let states = m.items.iter().filter(|i| i.kind == "state").count();
                    let bytes: u64 = m.items.iter().map(|i| i.bytes).sum();
                    outln!(
                        ctx,
                        "{}\tchain {}\t{states} states\t{:.1} MiB\t{}",
                        m.project,
                        m.chain,
                        bytes as f64 / 1048576.0,
                        if verify { "ok" } else { "" }
                    );
                }
                Err(err) => {
                    bad += 1;
                    outln!(ctx, "{}: {err}", d.display());
                }
            }
        }
    }
    Ok(if bad > 0 { 1 } else { 0 })
}

fn restore(ctx: &Ctx, root: &Path, as_name: Option<&str>, chain: Option<u32>, quiet: bool) -> Result<i32> {
    // The folder of one project (or the backup root with one project in it).
    let proot = if chain_dirs(root)?.is_empty() {
        let mut v: Vec<PathBuf> = std::fs::read_dir(root)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| !chain_dirs(p).unwrap_or_default().is_empty())
            .collect();
        if v.len() != 1 {
            bail!("{} holds {} projects; name one project folder", root.display(), v.len());
        }
        v.remove(0)
    } else {
        root.to_path_buf()
    };
    let chains = chain_dirs(&proot)?;
    let dir = match chain {
        Some(n) => chains.iter().find(|(k, _)| *k == n).map(|x| x.1.clone()).ok_or_else(|| exit(1, format!("no chain {n}")))?,
        None => chains.last().map(|x| x.1.clone()).ok_or_else(|| exit(1, "no backup chain"))?,
    };
    let m = verify_chain(&dir)?;
    let name = as_name.unwrap_or(&m.project).to_string();
    let p = Project::create_empty(&ctx.store, &name)?;
    let started = std::time::Instant::now();
    let res = (|| -> Result<usize> {
        let mut n = 0;
        for i in &m.items {
            let f = dir.join(&i.file);
            match i.kind.as_str() {
                "state" => {
                    let z = Command::new("zstd").args(["-q", "-d", "-c"]).arg(&f).stdout(Stdio::piped()).spawn()?;
                    let st =
                        Command::new("btrfs").args(["receive", "-q"]).arg(p.states_dir()).stdin(z.stdout.unwrap()).status()?;
                    if !st.success() {
                        bail!("btrfs receive of {} failed", i.file);
                    }
                    n += 1;
                }
                "git" => {
                    let s = p.git_dir();
                    if !s.join("objects").exists() {
                        let o = Command::new("git").args(["init", "--bare", "-q"]).arg(&s).output()?;
                        if !o.status.success() {
                            bail!("git init failed");
                        }
                        std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o755))?;
                    }
                    crate::gitbridge::git_out(&s, &["fetch", "-q", f.to_str().unwrap(), "+refs/*:refs/*"])?;
                }
                "records" => {
                    let out = Command::new("zstd").args(["-q", "-d", "-c"]).arg(&f).output()?;
                    if !out.status.success() {
                        bail!("cannot read {}", i.file);
                    }
                    let lines: Vec<String> =
                        String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.is_empty()).map(|l| l.to_string()).collect();
                    p.import_lines(&lines, true)?;
                }
                _ => {}
            }
        }
        Ok(n)
    })();
    let n = match res {
        Ok(n) => n,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&p.dir);
            return Err(e);
        }
    };
    drop(p);
    // Check the records, mark the received states, give the lines working folders here.
    let p = Project::open(&ctx.store, &name)?;
    p.verify_records()?;
    p.rebuild_index()?;
    {
        // The machines of the backup are trusted, and this machine takes over the settings of
        // the machine that wrote it (lead, members, remotes, merge commands).
        let _lock = p.lock()?;
        for m in p.db.record_machines()? {
            if m != p.store.machine.id {
                if let Some(k) = p.db.machine_key(&m)? {
                    super::replicate::trust(ctx, &p, &m, &k)?;
                }
            }
        }
        let suffix = format!("@{}", m.machine);
        let mut take = serde_json::Map::new();
        for (k, v) in p.db.config_all()? {
            if let Some(key) = k.strip_suffix(&suffix) {
                take.insert(key.to_string(), serde_json::from_str(&v).unwrap_or(serde_json::Value::String(v)));
            }
        }
        if !take.is_empty() {
            let mut rec = Record::new("config");
            rec.data = serde_json::json!({"config": take});
            p.append(rec, &ctx.actor())?;
        }
    }
    {
        let r = Repo { p: Project::open(&ctx.store, &name)? };
        crate::gitbridge::sync_store_access(&r)?;
    }
    std::fs::create_dir_all(p.git_dir().join("layr-index")).ok();
    let repo = Repo { p };
    let mut adopted = Vec::new();
    for l in repo.lines()? {
        let mut args = vec![l.name.clone()];
        let has_auto = repo.p.db.line_states(&l.id, StateKind::Auto)?.iter().any(|s| repo.p.state_path(&s.id).exists());
        if !has_auto {
            args.push("--head".into());
        }
        let mut sub = Ctx {
            store: crate::store::Store::open(&ctx.store.root)?,
            caller: ctx.caller.clone(),
            cwd: repo.p.dir.clone(),
            env: ctx.env.clone(),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
        };
        sub.env.insert("LAYR_PROJECT".into(), name.clone());
        sub.env.remove("LAYR_LINE");
        if super::branch::adopt(&sub, &args).is_ok() {
            adopted.push(l.name.clone());
        }
    }
    if !quiet {
        outln!(
            ctx,
            "restored project {name} from {}: {n} state(s), {} line(s) [{}] in {:.1} s",
            dir.display(),
            adopted.len(),
            adopted.join(", "),
            started.elapsed().as_secs_f64()
        );
    }
    let _ = File::open(&dir)?.read(&mut [0u8; 0]);
    Ok(0)
}
