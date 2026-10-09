//! The remote bridge to git, and the `.git` layer of each line for tools that call git.
//!
//! The project git object store is `<project>/git` (a bare repository, readable by agents).
//! Import makes a state from a git commit with only the changed paths. Export builds a git tree
//! from the tree of the previous mapped commit plus the changed paths, makes one commit with
//! fixed author and committer data, records the exact commit id, then pushes it. After a lost
//! response, a repeated push first reads the remote ref.

use crate::changes;
use crate::ctx::Ctx;
use crate::fsutil;
use crate::ids;
use crate::model::{Line, Record, StateKind, StateRec};
use crate::repo::{skip_path, Repo};
use anyhow::{anyhow, bail, Context, Result};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn store(repo: &Repo) -> PathBuf {
    repo.p.git_dir()
}

pub fn has_store(repo: &Repo) -> bool {
    store(repo).join("objects").is_dir()
}

fn base_cmd(git_dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-c").arg(format!("safe.directory={}", git_dir.display()));
    c.arg("-c").arg("safe.directory=*");
    c.env("GIT_DIR", git_dir);
    c.env_remove("GIT_WORK_TREE");
    c.env_remove("GIT_INDEX_FILE");
    c.env("GIT_TERMINAL_PROMPT", "0");
    c
}

pub fn git_out(git_dir: &Path, args: &[&str]) -> Result<String> {
    let out = base_cmd(git_dir).args(args).stdin(Stdio::null()).output().context("run git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Who can read the git object store. With `members = "*"` every local user may read the
/// project history, so the store is 0755. With a list of members it is 0750 for root, plus an
/// access list (POSIX ACL) with read access for the lead, the members and the owners of lines
/// on this machine: the objects are the project history too.
pub fn sync_store_access(repo: &Repo) -> Result<()> {
    let s = store(repo);
    if !s.join("objects").is_dir() {
        return Ok(());
    }
    if repo.members_all() {
        std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o755))?;
        let _ = remove_acl(&s);
        return Ok(());
    }
    let mut uids: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    uids.insert(repo.p.lead());
    if let Some(v) = repo.p.config_value("members")? {
        if let Some(a) = v.as_array() {
            uids.extend(a.iter().filter_map(|x| x.as_u64()).map(|x| x as u32));
        }
    }
    for l in repo.lines()? {
        if l.machine == repo.p.store.machine.id {
            uids.insert(l.owner);
        }
    }
    uids.remove(&0);
    std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o750))?;
    set_acl(&s, &uids.into_iter().collect::<Vec<_>>())
}

fn acl_entry(buf: &mut Vec<u8>, tag: u16, perm: u16, id: u32) {
    buf.extend_from_slice(&tag.to_le_bytes());
    buf.extend_from_slice(&perm.to_le_bytes());
    buf.extend_from_slice(&id.to_le_bytes());
}

fn set_acl(path: &Path, users: &[u32]) -> Result<()> {
    const UNDEFINED: u32 = u32::MAX;
    let mut buf = 2u32.to_le_bytes().to_vec();
    acl_entry(&mut buf, 0x01, 7, UNDEFINED); // owner rwx
    for u in users {
        acl_entry(&mut buf, 0x02, 5, *u); // listed users r-x
    }
    acl_entry(&mut buf, 0x04, 5, UNDEFINED); // group r-x
    acl_entry(&mut buf, 0x10, 5, UNDEFINED); // mask r-x
    acl_entry(&mut buf, 0x20, 0, UNDEFINED); // others none
    rustix::fs::setxattr(path, "system.posix_acl_access", &buf, rustix::fs::XattrFlags::empty())
        .map_err(|e| anyhow!("set access list on {}: {e}", path.display()))?;
    Ok(())
}

fn remove_acl(path: &Path) -> Result<()> {
    let _ = rustix::fs::removexattr(path, "system.posix_acl_access");
    Ok(())
}

pub fn init_store(repo: &Repo) -> Result<PathBuf> {
    let s = store(repo);
    if !has_store(repo) {
        let out = Command::new("git").args(["init", "--bare", "-q", "-b", "main"]).arg(&s).output()?;
        if !out.status.success() {
            bail!("git init failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        git_out(&s, &["config", "core.logAllRefUpdates", "false"])?;
        git_out(&s, &["config", "gc.auto", "0"])?;
        std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o750))?;
        sync_store_access(repo)?;
    }
    std::fs::create_dir_all(s.join("layr-index"))?;
    Ok(s)
}

// ----- network operations as the caller -----

/// Environment passed to git network commands from the caller.
const NET_ENV: &[&str] = &[
    "SSH_AUTH_SOCK",
    "GIT_SSH_COMMAND",
    "GIT_SSH",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
    "https_proxy",
    "http_proxy",
    "no_proxy",
    "GIT_CONFIG_GLOBAL",
];

/// Network git commands run as the caller: its uid, groups and selected environment (ssh agent,
/// tokens, proxies), never the service's environment.
fn as_caller(ctx: &Ctx, c: &mut Command, git_dir: &Path) -> Result<()> {
    let mut env = crate::privs::pick_env(ctx, NET_ENV);
    if let Some(p) = ctx.env("PATH") {
        env.push(("PATH".into(), p.to_string()));
    }
    env.push(("GIT_DIR".into(), git_dir.to_string_lossy().into_owned()));
    env.push(("GIT_TERMINAL_PROMPT".into(), "0".into()));
    crate::privs::command_as(c, ctx.caller.uid, &env)
}

pub fn remote_url(repo: &Repo, remote: &str) -> Result<String> {
    if let Some(v) = repo.p.config_value(&format!("remote.{remote}.url"))? {
        if let Some(s) = v.as_str() {
            return Ok(s.to_string());
        }
    }
    bail!("remote '{remote}' is not configured (layr remote add {remote} <url>)")
}

fn fetched_refs(ctx: &Ctx, tmp: &Path, remote: &str) -> Result<Vec<(String, String)>> {
    let mut command = base_cmd(tmp);
    command.args(["for-each-ref", "--format=%(objectname) %(refname)", &format!("refs/remotes/{remote}/"), "refs/tags/"]);
    as_caller(ctx, &mut command, tmp)?;
    let output = command.stdin(Stdio::null()).output()?;
    if !output.status.success() {
        bail!("cannot list fetched refs: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let mut refs = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let (oid, name) = line.split_once(' ').ok_or_else(|| anyhow!("bad fetched ref"))?;
        if oid.len() != 40 || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("bad fetched object id");
        }
        if !name.starts_with(&format!("refs/remotes/{remote}/")) && !name.starts_with("refs/tags/") {
            bail!("bad fetched ref name");
        }
        refs.push((name.to_string(), oid.to_string()));
    }
    Ok(refs)
}

/// Import objects through a pipe. Root never opens the temporary repository that belongs to
/// the caller, so links or alternates in it cannot make root read another path.
fn import_fetched(ctx: &Ctx, store: &Path, tmp: &Path, refs: &[(String, String)]) -> Result<()> {
    if refs.is_empty() {
        return Ok(());
    }
    let known = git_out(store, &["for-each-ref", "--format=%(objectname)"])?;
    let mut pack = base_cmd(tmp);
    pack.args(["pack-objects", "--stdout", "--revs", "--thin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    as_caller(ctx, &mut pack, tmp)?;
    let mut pack = pack.spawn().context("start git pack-objects")?;
    let pack_out = pack.stdout.take().ok_or_else(|| anyhow!("git pack-objects has no output"))?;

    let mut index = base_cmd(store);
    let mut index = index
        .args(["index-pack", "--stdin", "--fix-thin"])
        .stdin(Stdio::from(pack_out))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("start git index-pack")?;
    let write_result = (|| -> Result<()> {
        let input = pack.stdin.as_mut().ok_or_else(|| anyhow!("git pack-objects has no input"))?;
        for (_, oid) in refs {
            writeln!(input, "{oid}")?;
        }
        for oid in known.lines().filter(|s| !s.is_empty()) {
            writeln!(input, "^{oid}")?;
        }
        Ok(())
    })();
    drop(pack.stdin.take());
    let pack_status = pack.wait()?;
    let index_status = index.wait()?;
    write_result?;
    if !pack_status.success() || !index_status.success() {
        bail!("cannot import fetched git objects");
    }

    let mut update = base_cmd(store);
    update.args(["update-ref", "--stdin"]).stdin(Stdio::piped()).stderr(Stdio::piped());
    let mut update = update.spawn()?;
    {
        let input = update.stdin.as_mut().ok_or_else(|| anyhow!("git update-ref has no input"))?;
        for (name, oid) in refs {
            writeln!(input, "update {name} {oid}")?;
        }
    }
    drop(update.stdin.take());
    let output = update.wait_with_output()?;
    if !output.status.success() {
        bail!("cannot update fetched refs: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

/// `git fetch` of a remote into the store. Network access runs as the caller, into a temporary
/// repository that borrows the store objects; the service then imports a pack through a pipe.
pub fn fetch(ctx: &Ctx, repo: &Repo, remote: &str) -> Result<Vec<(String, String)>> {
    let s = init_store(repo)?;
    let url = remote_url(repo, remote)?;
    let tmp = std::env::temp_dir().join(format!("layr-fetch-{}", ids::new_id()));
    std::fs::create_dir_all(&tmp)?;
    let out = Command::new("git").args(["init", "--bare", "-q"]).arg(&tmp).output()?;
    if !out.status.success() {
        bail!("git init failed");
    }
    std::fs::write(tmp.join("objects/info/alternates"), format!("{}\n", s.join("objects").display()))?;
    // Known refs of the store as "have" lines for a small transfer.
    let refs = git_out(&s, &["for-each-ref", "--format=%(objectname) %(refname)"])?;
    let mut packed = String::new();
    for l in refs.lines() {
        if let Some((oid, name)) = l.split_once(' ') {
            packed.push_str(&format!("{oid} refs/layr-have/{}\n", name.trim_start_matches("refs/")));
        }
    }
    std::fs::write(tmp.join("packed-refs"), packed)?;
    if rustix::process::geteuid().is_root() && ctx.caller.uid != 0 {
        let (u, g) = (ctx.caller.uid, ctx.caller.gid);
        let mut paths = Vec::new();
        fsutil::walk(&tmp, "", &mut |rel, _| {
            paths.push(rel.to_string());
            Ok(true)
        })?;
        fsutil::chown_nofollow(&tmp, u, g)?;
        for p in paths {
            fsutil::chown_nofollow(&tmp.join(p), u, g)?;
        }
    }
    let mut c = base_cmd(&tmp);
    c.args(["fetch", "-q", "--no-tags", "--no-write-fetch-head"])
        .arg(&url)
        .arg(format!("+refs/heads/*:refs/remotes/{remote}/*"))
        .arg("+refs/tags/*:refs/tags/*");
    as_caller(ctx, &mut c, &tmp)?;
    let out = c.stdin(Stdio::null()).output().context("run git fetch")?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        bail!("git fetch {url} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let refs = fetched_refs(ctx, &tmp, remote);
    let r = refs.and_then(|refs| {
        import_fetched(ctx, &s, &tmp, &refs)?;
        Ok(refs)
    });
    let _ = std::fs::remove_dir_all(&tmp);
    let prefix = format!("refs/remotes/");
    Ok(r?.into_iter().filter_map(|(name, oid)| name.strip_prefix(&prefix).map(|name| (name.to_string(), oid))).collect())
}

/// Read a remote ref without fetching (`git ls-remote`), as the caller.
pub fn ls_remote(ctx: &Ctx, repo: &Repo, url: &str, branch: &str) -> Result<Option<String>> {
    let s = init_store(repo)?;
    let mut c = base_cmd(&s);
    c.args(["ls-remote", url, &format!("refs/heads/{branch}")]);
    as_caller(ctx, &mut c, &s)?;
    let out = c.stdin(Stdio::null()).output()?;
    if !out.status.success() {
        bail!("git ls-remote failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).split_whitespace().next().map(|s| s.to_string()))
}

// ----- import -----

fn commit_info(s: &Path, c: &str) -> Result<(String, String, String, i64)> {
    let out = git_out(s, &["log", "-1", "--format=%an%x00%ae%x00%at%x00%B", c])?;
    let mut it = out.splitn(4, '\0');
    let name = it.next().unwrap_or("").to_string();
    let email = it.next().unwrap_or("").to_string();
    let time: i64 = it.next().unwrap_or("0").trim().parse().unwrap_or(0);
    let msg = it.next().unwrap_or("").trim_end().to_string();
    Ok((name, email, msg, time * 1000))
}

/// Write the files of `commit` for `paths` (all when `None`) into `dst`, deleting paths that
/// are not in the commit.
fn materialize(s: &Path, commit: &str, dst: &Path, paths: Option<&[String]>, own: Option<(u32, u32)>) -> Result<()> {
    let mut args = vec!["ls-tree", "-r", "-z", "--full-tree", commit];
    let owned: Vec<String>;
    if let Some(p) = paths {
        args.push("--");
        owned = p.to_vec();
        args.extend(owned.iter().map(|s| s.as_str()));
    }
    // ls-tree with many paths: call in batches.
    let mut entries: Vec<(String, String, String)> = Vec::new();
    let mut present = std::collections::HashSet::new();
    let batch = |ps: &[&str]| -> Result<Vec<(String, String, String)>> {
        let mut a = vec!["ls-tree", "-r", "-z", "--full-tree", commit];
        if !ps.is_empty() {
            a.push("--");
            a.extend_from_slice(ps);
        }
        let out = base_cmd(s).args(&a).output()?;
        if !out.status.success() {
            bail!("git ls-tree failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        let mut v = Vec::new();
        for rec in out.stdout.split(|b| *b == 0) {
            if rec.is_empty() {
                continue;
            }
            let r = String::from_utf8_lossy(rec);
            let (meta, path) = r.split_once('\t').ok_or_else(|| anyhow!("bad ls-tree line"))?;
            let f: Vec<&str> = meta.split(' ').collect();
            v.push((f[0].to_string(), f[2].to_string(), path.to_string()));
        }
        Ok(v)
    };
    match paths {
        None => entries = batch(&[])?,
        Some(p) => {
            for chunk in p.chunks(500) {
                let refs: Vec<&str> = chunk.iter().map(|s| s.as_str()).collect();
                entries.extend(batch(&refs)?);
            }
        }
    }
    let _ = args;
    for (_, _, p) in &entries {
        present.insert(p.clone());
    }
    if let Some(p) = paths {
        for path in p {
            if !present.contains(path) && fsutil::lmeta(dst, path).is_some() {
                fsutil::remove_entry(dst, path)?;
            }
        }
    }
    // Content through one `git cat-file --batch`.
    let mut child = base_cmd(s).args(["cat-file", "--batch"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let ids: Vec<String> = entries.iter().filter(|e| e.0 != "160000").map(|e| e.1.clone()).collect();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        for id in ids {
            writeln!(stdin, "{id}")?;
        }
        Ok(())
    });
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    use std::io::{BufRead, Read};
    for (mode, _oid, path) in &entries {
        if mode == "160000" {
            fsutil::make_parents(dst, &format!("{path}/x"), own)?;
            continue;
        }
        let mut header = String::new();
        stdout.read_line(&mut header)?;
        let size: usize = header
            .split_whitespace()
            .nth(2)
            .and_then(|x| x.parse().ok())
            .ok_or_else(|| anyhow!("bad cat-file header {header}"))?;
        let mut data = vec![0u8; size];
        stdout.read_exact(&mut data)?;
        let mut nl = [0u8; 1];
        stdout.read_exact(&mut nl)?;
        if fsutil::lmeta(dst, path).is_some() {
            fsutil::remove_entry(dst, path)?;
        }
        if mode == "120000" {
            fsutil::make_parents(dst, path, own)?;
            let target = String::from_utf8_lossy(&data).into_owned();
            let p = fsutil::safe_join(dst, path)?;
            std::os::unix::fs::symlink(target, &p)?;
            if let Some((u, g)) = own {
                fsutil::chown_nofollow(&p, u, g)?;
            }
        } else {
            let m = if mode == "100755" { 0o755 } else { 0o644 };
            fsutil::write_file(dst, path, &data, m, own)?;
        }
    }
    writer.join().map_err(|_| anyhow!("writer thread"))??;
    child.wait()?;
    Ok(())
}

/// Make (or find) the state of a git commit. `prev` is the previous import of the same ref.
pub fn import_commit(ctx: &Ctx, repo: &Repo, commit: &str, refname: &str, author_uid: u32) -> Result<(String, bool)> {
    let s = init_store(repo)?;
    let mapped = repo.p.db.state_of_commit(commit)?;
    if let Some(st) = mapped.iter().find(|x| repo.p.state_path(x).exists()) {
        let _ = author_uid;
        return Ok((st.clone(), false));
    }
    // The nearest ancestor commit that has a state.
    let prev: Option<(String, String)> = {
        let mut found = None;
        let revs = git_out(&s, &["rev-list", "--first-parent", "--max-count=10000", commit])?;
        for c in revs.lines().skip(1) {
            if let Some(st) = repo.p.db.state_of_commit(c)?.into_iter().find(|x| repo.p.state_path(x).exists()) {
                found = Some((st, c.to_string()));
                break;
            }
        }
        found
    };
    let work = repo.p.work_dir().join(ids::new_id());
    let res = (|| -> Result<StateRec> {
        let mut parents = Vec::new();
        match &prev {
            Some((pst, pc)) => {
                crate::btrfs::snapshot(&repo.state_root(pst)?, &work, false)?;
                let out = base_cmd(&s).args(["diff", "--name-only", "-z", "--no-renames", pc, commit]).output()?;
                let paths: Vec<String> = out
                    .stdout
                    .split(|b| *b == 0)
                    .filter(|x| !x.is_empty())
                    .map(|x| String::from_utf8_lossy(x).into_owned())
                    .collect();
                materialize(&s, commit, &work, Some(&paths), None)?;
                parents.push(pst.clone());
            }
            None => {
                crate::btrfs::create_subvolume(&work)?;
                materialize(&s, commit, &work, None, None)?;
            }
        }
        // Exported states that this commit contains become parents too (good merge bases).
        for (_, st, _, _, gc, status) in repo.p.db.exports()? {
            if status != "pushed" || parents.contains(&st) {
                continue;
            }
            let ok = base_cmd(&s).args(["merge-base", "--is-ancestor", &gc, commit]).status()?.success();
            if ok && !parents.iter().any(|p| repo.is_ancestor(&st, p).unwrap_or(false)) {
                parents.push(st);
            }
        }
        let (name, email, msg, time) = commit_info(&s, commit)?;
        let mut st = repo.new_state(ctx, &work, StateKind::Import, parents, &msg, None)?;
        st.author = crate::model::Author { name, email, uid: 0, agent: None };
        st.time = time.max(1);
        Ok(st)
    })();
    let _ = crate::btrfs::delete_tree(&work);
    let st = res?;
    let mut rec = Record::new("import");
    rec.states.push(st.clone());
    rec.data = serde_json::json!({"ref": refname, "commit": commit, "state": st.id});
    repo.commit_record(ctx, rec)?;
    // Git index for lines made from this state.
    let _ = build_index(repo, &st.id, commit, prev.as_ref().map(|p| p.0.as_str()));
    Ok((st.id, true))
}

fn index_path(repo: &Repo, state: &str) -> PathBuf {
    store(repo).join("layr-index").join(state)
}

/// A git index of `commit` whose stat data matches the files of `state`.
pub fn build_index(repo: &Repo, state: &str, commit: &str, prev_state: Option<&str>) -> Result<()> {
    let s = store(repo);
    let idx = index_path(repo, state);
    let root = repo.state_root(state)?;
    let prev_idx = prev_state.map(|p| index_path(repo, p)).filter(|p| p.exists());
    let run = |args: &[&str], input: Option<&[u8]>| -> Result<()> {
        let mut c = base_cmd(&s);
        c.env("GIT_INDEX_FILE", &idx)
            .env("GIT_WORK_TREE", &root)
            .args(["-c", "core.checkStat=minimal", "-c", "core.trustctime=false"])
            .args(args);
        c.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::null()).stderr(Stdio::piped());
        let mut ch = c.spawn()?;
        if let Some(d) = input {
            ch.stdin.take().unwrap().write_all(d)?;
        }
        let out = ch.wait_with_output()?;
        if !out.status.success() && args[0] != "update-index" {
            bail!("git {} failed: {}", args[0], String::from_utf8_lossy(&out.stderr));
        }
        Ok(())
    };
    match (prev_idx, prev_state) {
        (Some(pi), Some(ps)) => {
            std::fs::copy(&pi, &idx)?;
            let prev_root = repo.state_root(ps)?;
            let mut input = Vec::new();
            for c in changes::file_changes(&prev_root, &root)? {
                for p in [Some(c.path.clone()), c.old_path.clone()].into_iter().flatten() {
                    if !skip_path(&p) {
                        input.extend_from_slice(p.as_bytes());
                        input.push(0);
                    }
                }
            }
            run(&["update-index", "--add", "--remove", "-z", "--stdin"], Some(&input))?;
            run(&["update-index", "-q", "--refresh"], None)?;
        }
        _ => {
            run(&["read-tree", commit], None)?;
            run(&["update-index", "-q", "--refresh"], None)?;
        }
    }
    Ok(())
}

// ----- export -----

/// The nearest ancestor state with a git commit.
pub fn mapped_ancestor(repo: &Repo, state: &str) -> Result<Option<(String, String)>> {
    for a in repo.ancestors(state)? {
        if let Some(c) = repo.p.db.git_commit_of(&a)? {
            if git_out(&store(repo), &["cat-file", "-e", &format!("{c}^{{commit}}")]).is_ok() {
                return Ok(Some((a, c)));
            }
        }
    }
    Ok(None)
}

/// Build the commit for a state: the tree of the nearest mapped ancestor plus changed paths,
/// one parent, author and committer from the state. The same state gives the same commit.
pub fn make_commit(repo: &Repo, state: &StateRec, message: Option<&str>) -> Result<(String, Option<String>)> {
    let s = init_store(repo)?;
    let root = repo.state_root(&state.id)?;
    let anc = mapped_ancestor(repo, &state.id)?;
    let tmp_idx = std::env::temp_dir().join(format!("layr-index-{}", ids::new_id()));
    let res = (|| -> Result<(String, Option<String>)> {
        let mut paths: Vec<String> = Vec::new();
        let parent_commit = match &anc {
            Some((ast, ac)) if ast == &state.id => return Ok((ac.clone(), None)),
            Some((ast, ac)) => {
                let ai = index_path(repo, ast);
                if ai.exists() {
                    std::fs::copy(&ai, &tmp_idx)?;
                } else {
                    let out = base_cmd(&s).env("GIT_INDEX_FILE", &tmp_idx).args(["read-tree", ac]).output()?;
                    if !out.status.success() {
                        bail!("git read-tree failed");
                    }
                }
                let aroot = repo.state_root(ast)?;
                let rules = crate::ignore_rules::Rules::new(&root);
                for c in changes::file_changes(&aroot, &root)? {
                    for p in [Some(c.path.clone()), c.old_path.clone()].into_iter().flatten() {
                        if skip_path(&p) {
                            continue;
                        }
                        // Ignored paths are never exported, unless the ancestor tracked them.
                        if c.old.is_none() && rules.ignored(&p, false) {
                            continue;
                        }
                        paths.push(p);
                    }
                }
                Some(ac.clone())
            }
            None => {
                let rules = crate::ignore_rules::Rules::new(&root);
                fsutil::walk(&root, "", &mut |rel, m| {
                    if skip_path(rel) || rules.ignored(rel, m.is_dir()) {
                        return Ok(false);
                    }
                    if !m.is_dir() {
                        paths.push(rel.to_string());
                    }
                    Ok(true)
                })?;
                None
            }
        };
        let mut input = Vec::new();
        for p in &paths {
            input.extend_from_slice(p.as_bytes());
            input.push(0);
        }
        let mut c = base_cmd(&s);
        c.env("GIT_INDEX_FILE", &tmp_idx).env("GIT_WORK_TREE", &root).args([
            "update-index",
            "--add",
            "--remove",
            "-z",
            "--stdin",
        ]);
        c.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
        let mut ch = c.spawn()?;
        ch.stdin.take().unwrap().write_all(&input)?;
        let out = ch.wait_with_output()?;
        if !out.status.success() {
            bail!("git update-index failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        let tree = String::from_utf8_lossy(&base_cmd(&s).env("GIT_INDEX_FILE", &tmp_idx).args(["write-tree"]).output()?.stdout)
            .trim()
            .to_string();
        if tree.is_empty() {
            bail!("git write-tree failed");
        }
        let msg = message.map(|m| m.to_string()).unwrap_or_else(|| {
            if state.message.is_empty() {
                "layr state".to_string()
            } else {
                state.message.clone()
            }
        });
        let secs = state.time / 1000;
        let date = format!("{secs} +0000");
        let mut c = base_cmd(&s);
        c.args(["commit-tree", &tree]);
        if let Some(p) = &parent_commit {
            c.args(["-p", p]);
        }
        c.env("GIT_AUTHOR_NAME", &state.author.name)
            .env("GIT_AUTHOR_EMAIL", &state.author.email)
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_NAME", &state.author.name)
            .env("GIT_COMMITTER_EMAIL", &state.author.email)
            .env("GIT_COMMITTER_DATE", &date);
        c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut ch = c.spawn()?;
        ch.stdin.take().unwrap().write_all(msg.as_bytes())?;
        let out = ch.wait_with_output()?;
        if !out.status.success() {
            bail!("git commit-tree failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        let commit = String::from_utf8_lossy(&out.stdout).trim().to_string();
        git_out(&s, &["update-ref", &format!("refs/layr/states/{}", ids::display(&state.id)), &commit])?;
        // Keep the index for the next export and for lines made from this state.
        let dst = index_path(repo, &state.id);
        if !dst.exists() {
            std::fs::copy(&tmp_idx, &dst).context("keep the git index of the exported state")?;
            let _ = refresh_index(repo, &state.id);
        }
        Ok((commit, parent_commit))
    })();
    let _ = std::fs::remove_file(&tmp_idx);
    res
}

fn refresh_index(repo: &Repo, state: &str) -> Result<()> {
    let s = store(repo);
    let root = repo.state_root(state)?;
    base_cmd(&s)
        .env("GIT_INDEX_FILE", index_path(repo, state))
        .env("GIT_WORK_TREE", &root)
        .args(["-c", "core.checkStat=minimal", "-c", "core.trustctime=false", "update-index", "-q", "--refresh"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(())
}

/// Push a prepared commit to `refs/heads/<branch>` of the remote, as the caller.
pub fn push(ctx: &Ctx, repo: &Repo, url: &str, commit: &str, branch: &str, force: bool) -> Result<()> {
    let s = store(repo);
    let mut c = base_cmd(&s);
    c.args(["push", "-q", url]);
    let spec = format!("{}{commit}:refs/heads/{branch}", if force { "+" } else { "" });
    c.arg(spec);
    as_caller(ctx, &mut c, &s)?;
    let out = c.stdin(Stdio::null()).output().context("run git push")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("non-fast-forward") || err.contains("fetch first") || err.contains("rejected") {
            bail!("push rejected: the remote branch '{branch}' has commits that this state does not contain.\nRun 'layr pull' (fetch and merge), then push again.\n{}", err.trim());
        }
        bail!("git push failed: {}", err.trim());
    }
    Ok(())
}

// ----- the .git layer of a line -----

/// Give a new working folder a `.git` layer (a nested subvolume) so tools that call git see the
/// nearest mapped commit. Objects come from the project store through `alternates`.
pub fn make_git_layer(repo: &Repo, line: &Line, state: &str, w: &Path) -> Result<()> {
    if !has_store(repo) {
        return Ok(());
    }
    let g = w.join(".git");
    if let Ok(m) = std::fs::symlink_metadata(&g) {
        if m.is_dir() && !crate::btrfs::is_subvolume(&g) && std::fs::read_dir(&g)?.next().is_none() {
            std::fs::remove_dir(&g)?;
        } else {
            return Ok(());
        }
    }
    crate::btrfs::create_subvolume(&g)?;
    let s = store(repo);
    for d in ["objects/info", "refs/heads", "refs/tags", "info"] {
        std::fs::create_dir_all(g.join(d))?;
    }
    std::fs::write(g.join("objects/info/alternates"), format!("{}\n", s.join("objects").display()))?;
    std::fs::write(
        g.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tcheckStat = minimal\n\ttrustctime = false\n\tlogallrefupdates = false\n",
    )?;
    std::fs::write(g.join("info/exclude"), "")?;
    let anc = mapped_ancestor(repo, state).ok().flatten();
    match &anc {
        Some((_, c)) => std::fs::write(g.join("HEAD"), format!("{c}\n"))?,
        None => std::fs::write(g.join("HEAD"), "ref: refs/heads/main\n")?,
    }
    let refs =
        git_out(&s, &["for-each-ref", "--format=%(objectname) %(refname)", "refs/tags", "refs/remotes"]).unwrap_or_default();
    let mut packed = String::from("# pack-refs with: peeled fully-peeled sorted \n");
    for l in refs.lines() {
        packed.push_str(l);
        packed.push('\n');
    }
    std::fs::write(g.join("packed-refs"), packed)?;
    if let Some((ast, _)) = &anc {
        let idx = index_path(repo, ast);
        if idx.exists() {
            let src = std::fs::File::open(&idx)?;
            let dst = std::fs::File::create(g.join("index"))?;
            if crate::btrfs::clone_file(&src, &dst).is_err() {
                std::fs::copy(&idx, g.join("index"))?;
            }
        }
    }
    let (u, gid) = repo.owner_ids(line);
    let mut paths = Vec::new();
    fsutil::walk(&g, "", &mut |rel, _| {
        paths.push(rel.to_string());
        Ok(true)
    })?;
    fsutil::chown_nofollow(&g, u, gid)?;
    for p in paths {
        fsutil::chown_nofollow(&g.join(p), u, gid)?;
    }
    Ok(())
}

/// Point the `.git` layer of a line at the commit of a new head (after an export or import).
/// The layer belongs to the line owner, so all access goes through descriptors of the `.git`
/// folder and never follows a link the owner made.
pub fn refresh_git_layer(repo: &Repo, line: &Line) -> Result<()> {
    let g = repo.working(line).join(".git");
    let dir = match fsutil::open_dir_nofollow(&g) {
        Ok(d) => d,
        Err(_) => return Ok(()),
    };
    // Only the layer layr made (a nested subvolume); a .git folder the agent made is its own.
    {
        use std::os::unix::fs::MetadataExt;
        if dir.metadata()?.ino() != 256 {
            return Ok(());
        }
    }
    // A HEAD that is not a regular file (for example a link) is replaced.
    let cur = fsutil::read_at(&dir, "HEAD").map(|c| String::from_utf8_lossy(&c).trim().to_string()).unwrap_or_default();
    if let Some((ast, c)) = mapped_ancestor(repo, &line.head)? {
        if cur == c {
            return Ok(());
        }
        let own = Some(repo.owner_ids(line));
        let head = format!("{c}\n");
        fsutil::replace_at(&dir, "HEAD", &mut |f| Ok((&*f).write_all(head.as_bytes())?), own)?;
        // The index of the mapped state: its stat data matches files copied from that state.
        let idx = index_path(repo, &ast);
        if idx.exists() {
            fsutil::replace_at(
                &dir,
                "index",
                &mut |f| {
                    let src = std::fs::File::open(&idx)?;
                    if crate::btrfs::clone_file(&src, f).is_err() {
                        std::io::copy(&mut std::fs::File::open(&idx)?, &mut &*f)?;
                    }
                    Ok(())
                },
                own,
            )?;
        } else {
            fsutil::unlink_at(&dir, "index");
        }
    }
    Ok(())
}
