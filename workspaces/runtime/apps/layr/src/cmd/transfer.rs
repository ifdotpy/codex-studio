//! Copies outside the store: export, backups and their restore.

use super::{exit, here};
use crate::args::{parse, Spec};
use crate::btrfs;
use crate::changes;
use crate::ctx::Ctx;
use crate::fsutil;
use crate::ids;
use crate::model::{Record, StateKind};
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

// ----- backups -----

/// Manifest format 2: signed by the machine that wrote the backup.
const FORMAT: u32 = 2;
/// The records of a backup are at most this large after decompression.
const MAX_RECORDS: u64 = 512 << 20;

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
    /// The machine that wrote the backup and signed this manifest.
    machine: String,
    project: String,
    project_id: String,
    chain: u32,
    created: i64,
    items: Vec<Item>,
}

/// `manifest.json`: the manifest text and the Ed25519 signature of exactly that text by the
/// machine that wrote the backup. The SHA-256 of each file is taken from the bytes the service
/// produced, so a changed file or manifest fails the check.
#[derive(serde::Deserialize)]
struct SignedManifest<'a> {
    #[serde(borrow)]
    manifest: &'a serde_json::value::RawValue,
    signature: String,
}

struct Loaded {
    m: Manifest,
    text: String,
    signature: String,
}

/// Chain folders of a project folder, oldest first. Links are skipped.
fn chain_dirs(root: &Path) -> Result<Vec<(u32, PathBuf)>> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
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

/// Read `manifest.json` of an open chain folder: a regular file of at most 64 MiB, not checked
/// against its signature yet.
fn read_manifest(dir: &File, shown: &Path) -> Result<Loaded> {
    let f = fsutil::open_file_at(dir, "manifest.json").with_context(|| format!("read {}/manifest.json", shown.display()))?;
    let mut d = Vec::new();
    f.take(64 << 20).read_to_end(&mut d)?;
    let s: SignedManifest = match serde_json::from_slice(&d) {
        Ok(s) => s,
        Err(_) => bail!("{}/manifest.json is not a signed manifest (an older layr wrote it; make a new backup)", shown.display()),
    };
    let m: Manifest = serde_json::from_str(s.manifest.get())?;
    if m.format != FORMAT {
        bail!("{}/manifest.json has format {}, this layr reads {FORMAT}", shown.display(), m.format);
    }
    for i in &m.items {
        if !safe_item_name(&i.file) {
            bail!("{}/manifest.json: item file name '{}' is not a plain file name", shown.display(), i.file);
        }
    }
    Ok(Loaded { text: s.manifest.get().to_string(), signature: s.signature.clone(), m })
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

fn write_manifest(dir: &Path, m: &Manifest, key: &ed25519_dalek::SigningKey) -> Result<()> {
    let text = serde_json::to_string_pretty(m)?;
    let sig = crate::oplog::sign_bytes(text.as_bytes(), key);
    fsutil::atomic_write(
        &dir.join("manifest.json"),
        format!("{{\"manifest\":{text},\"signature\":\"{sig}\"}}\n").as_bytes(),
        0o644,
    )
}

/// SHA-256 and size of everything read through it.
struct Hashing<R> {
    r: R,
    h: sha2::Sha256,
    n: u64,
}

impl<R: Read> Hashing<R> {
    fn new(r: R) -> Self {
        use sha2::Digest;
        Hashing { r, h: sha2::Sha256::new(), n: 0 }
    }
    fn finish(self) -> (String, u64) {
        use sha2::Digest;
        (hex::encode(self.h.finalize()), self.n)
    }
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use sha2::Digest;
        let n = self.r.read(buf)?;
        self.h.update(&buf[..n]);
        self.n += n as u64;
        Ok(n)
    }
}

/// A new file for backup output, created as the writer (never through an existing name).
fn create_out(path: &Path, writer: (u32, u32)) -> Result<File> {
    crate::privs::as_user_fs(writer.0, writer.1, || {
        let _ = std::fs::remove_file(path);
        Ok(OpenOptions::new().write(true).create_new(true).custom_flags(libc::O_NOFOLLOW).mode(0o644).open(path)?)
    })
}

/// Copy `src` into the new file `out` (created as the writer, then `fsync`), and return the
/// SHA-256 and size of the bytes the service wrote.
fn write_out(src: &mut dyn Read, out: &Path, writer: (u32, u32)) -> Result<(String, u64)> {
    let tmp = PathBuf::from(format!("{}.partial", out.display()));
    let mut f = create_out(&tmp, writer)?;
    let mut h = Hashing::new(src);
    let copied = std::io::copy(&mut h, &mut f).and_then(|_| f.sync_all());
    drop(f);
    let done = copied
        .map_err(anyhow::Error::from)
        .and_then(|_| crate::privs::as_user_fs(writer.0, writer.1, || Ok(std::fs::rename(&tmp, out)?)));
    if let Err(e) = done {
        let _ = crate::privs::as_user_fs(writer.0, writer.1, || Ok(std::fs::remove_file(&tmp)?));
        return Err(e);
    }
    Ok(h.finish())
}

/// Run a producer command (as root: it reads the store) and compress its output with zstd
/// into `out`.
fn compress_to(mut producer: Command, out: &Path, writer: (u32, u32)) -> Result<(String, u64)> {
    let mut p = producer.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("start producer")?;
    let pout = p.stdout.take().unwrap();
    let mut z = Command::new("zstd").args(["-q", "-3", "-c"]).stdin(pout).stdout(Stdio::piped()).spawn().context("run zstd")?;
    let mut zout = z.stdout.take().unwrap();
    let written = write_out(&mut zout, out, writer);
    let zs = z.wait()?;
    let ps = p.wait_with_output()?;
    if written.is_err() || !ps.status.success() || !zs.success() {
        let _ = crate::privs::as_user_fs(writer.0, writer.1, || Ok(std::fs::remove_file(out)?));
        written?;
        bail!("backup stream failed: {}", String::from_utf8_lossy(&ps.stderr).trim());
    }
    written
}

/// Write a backup into `root` as `writer` (uid, gid): every file and folder in the backup is
/// created with the writer's permissions; the store is read as root.
pub fn run_backup(ctx: &Ctx, repo: &Repo, root: &Path, full: bool, writer: (u32, u32)) -> Result<(u32, usize)> {
    let w = |f: &mut dyn FnMut() -> Result<()>| crate::privs::as_user_fs(writer.0, writer.1, f);
    let key = &repo.p.store.machine.key;
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
    // A chain grows only while its manifest is one this machine signed, at most a week old.
    let current = |d: &Path| -> Option<Manifest> {
        let l = crate::privs::as_user_fs(writer.0, writer.1, || read_manifest(&fsutil::open_dir_nofollow(d)?, d)).ok()?;
        let mine = l.m.machine == repo.p.store.machine.id && l.m.project_id == repo.p.id;
        let signed = crate::oplog::verify_bytes(l.text.as_bytes(), &l.signature, &key.verifying_key()).is_ok();
        (mine && signed && ids::now_ms() - l.m.created <= week).then_some(l.m)
    };
    let (num, dir, man) = match chains.last() {
        Some((n, d)) if !full => match current(d) {
            Some(m) => (*n, d.clone(), Some(m)),
            None => {
                outln!(ctx, "{} is older than a week or not a chain this machine signed; starting chain {}", d.display(), n + 1);
                (n + 1, proot.join(format!("chain-{:04}", n + 1)), None)
            }
        },
        Some((n, _)) => (n + 1, proot.join(format!("chain-{:04}", n + 1)), None),
        None => (1, proot.join("chain-0001"), None),
    };
    w(&mut || Ok(std::fs::create_dir_all(&dir)?))?;
    let mut m = man.unwrap_or(Manifest {
        format: FORMAT,
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
        let (sha, bytes) = compress_to(btrfs::send_command(pp.as_deref(), &repo.p.state_path(&id)), &dir.join(&file), writer)?;
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
        w(&mut || write_manifest(&dir, &m, key))?;
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
            let mut g =
                Command::new("git").env("GIT_DIR", &gs).args(&args).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
            let mut gout = g.stdout.take().unwrap();
            let written = write_out(&mut gout, &dir.join(&file), writer);
            if g.wait()?.success() {
                let (sha, bytes) = written?;
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
                w(&mut || write_manifest(&dir, &m, key))?;
            } else if written.is_ok() {
                let _ = w(&mut || Ok(std::fs::remove_file(dir.join(&file))?));
            }
        }
    }
    // The operation records as JSONL: the whole history of the project.
    let seq = next_seq(&m);
    let file = unique_name(&dir, &m, seq, "records.jsonl.zst");
    let jsonl = repo.p.export_jsonl()?;
    let (sha, bytes) = {
        let mut z = Command::new("zstd")
            .args(["-q", "-3", "-c"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("run zstd")?;
        let mut zin = z.stdin.take().unwrap();
        let feeder = std::thread::spawn(move || zin.write_all(jsonl.as_bytes()));
        let mut zout = z.stdout.take().unwrap();
        let written = write_out(&mut zout, &dir.join(&file), writer);
        let fed = feeder.join().map_err(|_| anyhow::anyhow!("zstd feeder failed"))?;
        if !z.wait()?.success() || fed.is_err() {
            let _ = w(&mut || Ok(std::fs::remove_file(dir.join(&file))?));
            bail!("zstd failed");
        }
        written?
    };
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
    w(&mut || write_manifest(&dir, &m, key))?;
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

/// Pipe `src` through `zstd -d` into `consume`, hashing the compressed bytes. Returns what
/// `consume` returned, and the SHA-256 and size of `src`. zstd is stopped when `consume` fails.
fn decompress<T>(src: File, consume: impl FnOnce(&mut dyn Read) -> Result<T>) -> Result<(T, (String, u64))> {
    let mut z = Command::new("zstd")
        .args(["-q", "-d", "-c"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("run zstd")?;
    let mut zin = z.stdin.take().unwrap();
    let feeder = std::thread::spawn(move || {
        let mut h = Hashing::new(src);
        let r = std::io::copy(&mut h, &mut zin);
        drop(zin);
        r.map(|_| h.finish())
    });
    let mut zout = z.stdout.take().unwrap();
    let res = consume(&mut zout);
    drop(zout);
    if res.is_err() {
        let _ = z.kill();
    }
    let st = z.wait()?;
    let fed = feeder.join().map_err(|_| anyhow::anyhow!("zstd feeder failed"))?;
    let v = res?;
    if !st.success() {
        bail!("zstd failed");
    }
    Ok((v, fed?))
}

/// A checked backup chain: the manifest, signed by the machine that wrote it, and the records.
struct Chain {
    m: Manifest,
    records: Vec<String>,
}

/// Check a chain folder: the records file against its SHA-256, the manifest signature with the
/// key those records give for the machine that wrote it (this machine's own key when the backup
/// names this machine), and with `all_files` every other file against its SHA-256. A restore
/// checks state streams and bundles while it reads them.
fn check_chain(dir: &Path, local: &crate::store::Machine, all_files: bool) -> Result<Chain> {
    let d = fsutil::open_dir_nofollow(dir)?;
    let l = read_manifest(&d, dir)?;
    let rec_items: Vec<&Item> = l.m.items.iter().filter(|i| i.kind == "records").collect();
    let item = match rec_items.as_slice() {
        [i] => *i,
        _ => bail!("{}: the manifest must name one records file", dir.display()),
    };
    let (text, (sha, _)) = decompress(fsutil::open_file_at(&d, &item.file)?, |r| {
        let mut v = Vec::new();
        r.take(MAX_RECORDS + 1).read_to_end(&mut v)?;
        if v.len() as u64 > MAX_RECORDS {
            bail!("the records are larger than {} MiB", MAX_RECORDS >> 20);
        }
        Ok(v)
    })?;
    if sha != item.sha256 {
        bail!("{}: checksum mismatch", item.file);
    }
    let records: Vec<String> = String::from_utf8_lossy(&text).lines().filter(|x| !x.is_empty()).map(|x| x.to_string()).collect();
    let first = records
        .iter()
        .filter_map(|x| crate::oplog::parse_line(x, None).ok())
        .find(|s| s.record.machine == l.m.machine && s.record.op == "machine" && s.record.seq == 1)
        .ok_or_else(|| anyhow::anyhow!("the backup has no key record of the machine that wrote it"))?;
    let key = crate::oplog::machine_key(&first.record)?;
    if l.m.machine == local.id && key != local.key.verifying_key() {
        bail!("the backup claims this machine with another key");
    }
    crate::oplog::verify_bytes(l.text.as_bytes(), &l.signature, &key)
        .map_err(|_| anyhow::anyhow!("{}: the manifest signature does not verify", dir.display()))?;
    if all_files {
        for i in l.m.items.iter().filter(|i| i.kind != "records") {
            let mut h = Hashing::new(fsutil::open_file_at(&d, &i.file)?);
            std::io::copy(&mut h, &mut std::io::sink())?;
            if h.finish().0 != i.sha256 {
                bail!("{}: checksum mismatch", i.file);
            }
        }
    }
    Ok(Chain { m: l.m, records })
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
            h.repo.require(ctx, "backup")?;
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
            let m = if verify {
                check_chain(&d, &ctx.store.machine, true).map(|c| c.m)
            } else {
                fsutil::open_dir_nofollow(&d).and_then(|f| read_manifest(&f, &d)).map(|l| l.m)
            };
            match m {
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

/// Restore one item: a state stream (checked on its way to `btrfs receive`, see `recv`) or a git
/// bundle. Each file is read once, and its SHA-256 must match the signed manifest.
fn restore_item(p: &Project, d: &File, i: &Item) -> Result<bool> {
    let f = fsutil::open_file_at(d, &i.file)?;
    match i.kind.as_str() {
        "state" => {
            let id = i.state.clone().unwrap_or_default();
            let rec = p.db.state(&id)?.ok_or_else(|| anyhow::anyhow!("no record for state {id:?}"))?;
            let parent = |u: &str| -> Option<PathBuf> {
                let pid = p.db.state_by_subvol(u).ok().flatten()?;
                let path = p.state_path(&pid);
                path.exists().then_some(path)
            };
            let e = crate::recv::Expect { name: &id, uuid: &rec.subvol, parent: &parent };
            let ((), (sha, _)) = decompress(f, |r| crate::recv::receive(r, &p.states_dir(), &e))?;
            if sha != i.sha256 {
                let _ = btrfs::delete_tree(&p.state_path(&id));
                bail!("{}: checksum mismatch", i.file);
            }
            Ok(true)
        }
        "git" => {
            // A private copy, checked, then fetched.
            let tmp = std::env::temp_dir().join(format!("layr-bundle-{}", ids::new_id()));
            let mut out = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
            let mut h = Hashing::new(f);
            let copied = std::io::copy(&mut h, &mut out);
            let res = (|| -> Result<()> {
                copied?;
                if h.finish().0 != i.sha256 {
                    bail!("{}: checksum mismatch", i.file);
                }
                let s = p.git_dir();
                if !s.join("objects").exists() {
                    let o = Command::new("git").args(["init", "--bare", "-q"]).arg(&s).output()?;
                    if !o.status.success() {
                        bail!("git init failed");
                    }
                    std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o755))?;
                }
                crate::gitbridge::git_out(&s, &["fetch", "-q", &tmp.to_string_lossy(), "+refs/*:refs/*"])?;
                Ok(())
            })();
            let _ = std::fs::remove_file(&tmp);
            res.map(|_| false)
        }
        "records" => Ok(false),
        k => bail!("unknown backup item kind {k:?}"),
    }
}

fn restore(ctx: &Ctx, root: &Path, as_name: Option<&str>, chain: Option<u32>, quiet: bool) -> Result<i32> {
    // The folder of one project (or the backup root with one project in it).
    let proot = if chain_dirs(root)?.is_empty() {
        let mut v: Vec<PathBuf> = std::fs::read_dir(root)?
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
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
    let c = check_chain(&dir, &ctx.store.machine, false)?;
    let m = c.m;
    let d = fsutil::open_dir_nofollow(&dir)?;
    let name = as_name.unwrap_or(&m.project).to_string();
    let p = Project::create_empty(&ctx.store, &name)?;
    let started = std::time::Instant::now();
    let res = (|| -> Result<usize> {
        // The records first: each state stream must be the subvolume its record names.
        p.import_lines(&c.records, crate::store::Trust::All)?;
        let pid =
            p.db.record_line(&m.machine, 1)?.and_then(|l| crate::oplog::parse_line(&l, None).ok()).map(|s| s.record.project);
        if pid.as_deref() != Some(m.project_id.as_str()) {
            bail!("the records are not project {}", m.project_id);
        }
        let mut n = 0;
        for i in &m.items {
            if restore_item(&p, &d, i)? {
                n += 1;
            }
        }
        Ok(n)
    })();
    let n = match res {
        Ok(n) => n,
        Err(e) => {
            let pd = p.dir.clone();
            drop(p);
            let _ = btrfs::delete_tree(&pd);
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
        // the machine that wrote it (admin, members, remotes, merge commands).
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
