//! Replication between machines over any byte channel (ssh or any other pipe).
//!
//! `layr sync -- <command>` starts `<command>`, which must run `layr serve-peer <project>` on the
//! other machine, and talks to it over the command's stdin and stdout:
//!
//! 1. hello: project id, per-machine sequence numbers, states present.
//! 2. records: each side sends the records the other lacks. Records are checked against the
//!    machine key from the first record of that machine's log, and appended verbatim.
//! 3. states: `btrfs send -p <latest common state>` for every state the peer lacks
//!    (commits, imports and stashes; automatic states with `--auto`), and with `--caches` the
//!    cache snapshots of the states.

use super::{exit, here};
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::ids;
use crate::model::StateKind;
use crate::repo::Repo;
use crate::storage as btrfs;
use crate::store::{Project, Trust};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::process::{Command, Stdio};

const MAX_FRAME: usize = 16 << 20;
/// Records travel in messages of at most this many lines.
const RECORDS_PER_MESSAGE: usize = 1000;

const MAX_CLONE_SOURCES: usize = 8;

#[derive(Clone, Default)]
struct SendFeatures {
    compressed: bool,
    clones: bool,
    cache_names: bool,
    identities: BTreeMap<String, (String, Option<u64>)>,
}

impl SendFeatures {
    fn from_hello(hello: &Value) -> Self {
        Self {
            compressed: hello["compressed_send_v2"].as_bool().unwrap_or(false),
            identities: serde_json::from_value(hello["clone_identities_v1"].clone()).unwrap_or_default(),
            cache_names: hello.get("caches").is_some(),
            clones: hello["clone_sources_v1"].as_bool().unwrap_or(false),
        }
    }
}

struct Wire<R: Read, W: Write> {
    r: R,
    w: W,
}

impl<R: Read, W: Write> Wire<R, W> {
    fn send(&mut self, kind: u8, data: &[u8]) -> Result<()> {
        self.w.write_all(&[kind])?;
        self.w.write_all(&(data.len() as u32).to_be_bytes())?;
        self.w.write_all(data)?;
        Ok(())
    }
    fn json(&mut self, v: &Value) -> Result<()> {
        self.send(b'J', serde_json::to_string(v)?.as_bytes())?;
        self.w.flush()?;
        Ok(())
    }
    fn recv(&mut self) -> Result<(u8, Vec<u8>)> {
        let mut h = [0u8; 5];
        self.r.read_exact(&mut h).context("peer closed the channel")?;
        let n = u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
        // A frame is a message or a stream chunk: never more than MAX_FRAME bytes.
        if n > MAX_FRAME {
            bail!("protocol error: frame of {n} bytes");
        }
        let mut d = vec![0u8; n];
        self.r.read_exact(&mut d)?;
        Ok((h[0], d))
    }
    fn recv_json(&mut self) -> Result<Value> {
        let (k, d) = self.recv()?;
        if k != b'J' {
            bail!("protocol error: expected a message");
        }
        let v: Value = serde_json::from_slice(&d)?;
        if v["op"] == "error" {
            bail!("peer: {}", v["message"].as_str().unwrap_or("error"));
        }
        Ok(v)
    }
    /// Send a btrfs stream from a child process.
    fn send_stream(&mut self, mut child: std::process::Child) -> Result<u64> {
        let mut out = child.stdout.take().unwrap();
        let mut buf = vec![0u8; 1 << 20];
        let mut total = 0;
        loop {
            let n = out.read(&mut buf)?;
            if n == 0 {
                break;
            }
            self.send(b'D', &buf[..n])?;
            total += n as u64;
        }
        let st = child.wait_with_output()?;
        if !st.status.success() {
            self.send(b'X', &[])?;
            bail!("btrfs send failed: {}", String::from_utf8_lossy(&st.stderr).trim());
        }
        self.send(b'E', &[])?;
        self.w.flush()?;
        Ok(total)
    }
}

/// The data frames of one stream, read as a byte stream that ends at the end frame.
struct Frames<'w, R: Read, W: Write> {
    w: &'w mut Wire<R, W>,
    buf: Vec<u8>,
    pos: usize,
    done: bool,
}

impl<R: Read, W: Write> Read for Frames<'_, R, W> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.buf.len() {
            if self.done {
                return Ok(0);
            }
            let (k, d) = self.w.recv().map_err(std::io::Error::other)?;
            match k {
                b'D' => {
                    self.buf = d;
                    self.pos = 0;
                }
                b'E' => self.done = true,
                b'X' => {
                    self.done = true;
                    return Err(std::io::Error::other("btrfs send failed on the peer"));
                }
                _ => return Err(std::io::Error::other("protocol error in a stream")),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl<R: Read, W: Write> Frames<'_, R, W> {
    /// Read the rest of a stream that was refused, so the channel stays in step.
    fn drain(&mut self) -> Result<()> {
        while !self.done {
            match self.w.recv()?.0 {
                b'D' => {}
                b'E' | b'X' => self.done = true,
                _ => bail!("protocol error in a stream"),
            }
        }
        Ok(())
    }
}

fn send_records<R: Read, W: Write>(w: &mut Wire<R, W>, lines: &[String]) -> Result<()> {
    for chunk in lines.chunks(RECORDS_PER_MESSAGE) {
        w.json(&json!({"op": "records", "lines": chunk, "more": true}))?;
    }
    w.json(&json!({"op": "records", "lines": [], "more": false}))
}

/// Records of one sync, in total at most this many bytes.
const MAX_RECORDS_BYTES: usize = 512 << 20;

fn recv_records<R: Read, W: Write>(w: &mut Wire<R, W>) -> Result<Vec<String>> {
    let mut all = Vec::new();
    let mut bytes = 0usize;
    loop {
        let v = w.recv_json()?;
        if v["op"] != "records" {
            bail!("protocol error: expected records");
        }
        let lines: Vec<String> = serde_json::from_value(v["lines"].clone())?;
        bytes += lines.iter().map(|l| l.len()).sum::<usize>();
        if bytes > MAX_RECORDS_BYTES {
            bail!("the peer sends more than {} MiB of records in one sync", MAX_RECORDS_BYTES >> 20);
        }
        all.extend(lines);
        if !v["more"].as_bool().unwrap_or(false) {
            return Ok(all);
        }
    }
}

// Only recent candidates need identities; older states remain available as parents.
fn clone_identities(p: &Project) -> Result<BTreeMap<String, (String, Option<u64>)>> {
    let mut out = BTreeMap::new();
    for id in present(p)?.iter().rev().take(MAX_CLONE_SOURCES * 4) {
        if let Ok(identity) = btrfs::clone_identity(&p.state_path(id)) {
            if identity.1.is_none() && !btrfs::supports_local_clone_receive() {
                continue;
            }
            if p.db.state(id)?.is_some_and(|s| s.subvol == identity.0) {
                out.insert(id.clone(), identity);
            }
        }
    }
    Ok(out)
}

fn seqs(p: &Project) -> Result<BTreeMap<String, u64>> {
    Ok(p.db.max_seqs()?.into_iter().collect())
}

/// Cache snapshots on this machine (`<state-id>@<path>`).
fn present_caches(p: &Project) -> Result<BTreeSet<String>> {
    Ok(std::fs::read_dir(p.states_dir())?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains('@'))
        .collect())
}

fn present(p: &Project) -> Result<BTreeSet<String>> {
    Ok(p.db.present_states()?.into_iter().filter(|(_, d)| !d).map(|(id, _)| id).filter(|id| p.state_path(id).exists()).collect())
}

/// Records this side has beyond the peer's sequence numbers, as stored lines.
fn missing_lines(p: &Project, peer: &BTreeMap<String, u64>) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for m in p.db.record_machines()? {
        out.extend(p.db.lines_after(&m, peer.get(&m).copied().unwrap_or(0))?);
    }
    Ok(out)
}

/// Check and store records from the peer.
fn take_lines(p: &Project, lines: &[String], trust: Trust) -> Result<usize> {
    p.import_lines(lines, trust)
}

/// Record that this project trusts a machine's records (its id and key).
pub fn trust(ctx: &Ctx, p: &Project, machine: &str, key: &str) -> Result<bool> {
    if machine == p.store.machine.id {
        return Ok(false);
    }
    if let Some(k) = p.db.trusted(machine)? {
        if k == key {
            return Ok(false);
        }
        bail!("machine {} is trusted with another key", &ids::display(machine)[..8]);
    }
    let mut rec = crate::model::Record::new("machine.trust");
    rec.data = json!({"machine": machine, "public_key": key});
    p.append(rec, &ctx.actor())?;
    Ok(true)
}

fn own_key(p: &Project) -> String {
    hex::encode(p.store.machine.key.verifying_key().to_bytes())
}

fn wanted(p: &Project, id: &str, auto: bool) -> bool {
    match p.db.state(id).ok().flatten() {
        Some(s) => auto || s.kind != StateKind::Auto,
        None => false,
    }
}

/// States to send, parents first, each with the best parent the peer has.
fn plan(repo: &Repo, mine: &BTreeSet<String>, theirs: &BTreeSet<String>, auto: bool) -> Result<Vec<(String, Option<String>)>> {
    let mut todo: Vec<(i64, String)> = mine
        .iter()
        .filter(|id| !theirs.contains(*id) && wanted(&repo.p, id, auto))
        .filter_map(|id| repo.p.db.state(id).ok().flatten().map(|s| (s.time, id.clone())))
        .collect();
    todo.sort();
    let mut have: BTreeSet<String> = theirs.intersection(mine).cloned().collect();
    let mut out = Vec::new();
    for (_, id) in todo {
        let mut parent = None;
        for a in repo.ancestors(&id)?.into_iter().skip(1) {
            if have.contains(&a) {
                parent = Some(a);
                break;
            }
        }
        if parent.is_none() {
            parent =
                have.iter().filter_map(|h| repo.p.db.state(h).ok().flatten().map(|s| (s.time, h.clone()))).max().map(|x| x.1);
        }
        have.insert(id.clone());
        out.push((id, parent));
    }
    Ok(out)
}

fn send_cache<R: Read, W: Write>(w: &mut Wire<R, W>, repo: &Repo, snap: &str, features: &SendFeatures) -> Result<u64> {
    w.json(&json!({"op": if features.cache_names { "cache" } else { "layer" }, "name": snap}))?;
    let child =
        btrfs::send_optimized(None, &repo.p.cache_path(snap.split('@').next().unwrap_or(""), snap), &[], features.compressed)?
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
    let n = w.send_stream(child)?;
    if w.recv_json()?["op"] != "ok" {
        bail!("peer did not store cache {snap}");
    }
    Ok(n)
}

fn send_states<R: Read, W: Write>(
    w: &mut Wire<R, W>,
    repo: &Repo,
    list: &[(String, Option<String>)],
    caches: bool,
    peer_caches: &BTreeSet<String>,
    peer_states: &BTreeSet<String>,
    features: SendFeatures,
) -> Result<(usize, u64)> {
    let mut bytes = 0;
    let mut have = peer_states.clone();
    let mut identities = features.identities.clone();
    // Caches of states the peer has already (sent earlier without --caches).
    if caches {
        for id in peer_states {
            if let Some(s) = repo.p.db.state(id)? {
                for snap in s.caches.values() {
                    if !peer_caches.contains(snap) && repo.p.cache_path(id, snap).exists() {
                        bytes += send_cache(w, repo, snap, &features)?;
                    }
                }
            }
        }
    }
    for (id, parent) in list {
        let clones: Vec<String> = if features.clones {
            have.iter()
                .rev()
                .filter(|source| Some(*source) != parent.as_ref() && *source != id)
                // Bound the search when a peer lists many states absent on this machine.
                .take(MAX_CLONE_SOURCES * 4)
                .filter(|source| {
                    repo.p.db.state(source).ok().flatten().is_some_and(|state| {
                        btrfs::send_clone_identity(&repo.p.state_path(source)).ok().is_some_and(|(uuid, transid)| {
                            uuid == state.subvol
                                && identities.get(*source).is_some_and(|(u, t)| *u == uuid && t.is_none_or(|t| t == transid))
                        })
                    })
                })
                .take(MAX_CLONE_SOURCES)
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        w.json(&json!({"op": "state", "id": id, "parent": parent, "clones": clones}))?;
        let pp = parent.as_ref().map(|p| repo.p.state_path(p));
        let sources: Vec<_> = clones.iter().map(|id| repo.p.state_path(id)).collect();
        let child = btrfs::send_optimized(pp.as_deref(), &repo.p.state_path(id), &sources, features.compressed)?
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        bytes += w.send_stream(child)?;
        let v = w.recv_json()?;
        if v["op"] != "ok" {
            bail!("peer did not store state {}", ids::short(id));
        }
        have.insert(id.clone());
        if let Ok((uuid, transid)) = btrfs::send_clone_identity(&repo.p.state_path(id)) {
            identities.insert(id.clone(), (uuid, Some(transid)));
        }
        // Regenerable caches travel only for a live move (warm builds on the peer).
        if caches {
            if let Some(s) = repo.p.db.state(id)? {
                for snap in s.caches.values() {
                    let path = repo.p.cache_path(id, snap);
                    if !path.exists() || peer_caches.contains(snap) {
                        continue;
                    }
                    bytes += send_cache(w, repo, snap, &features)?;
                }
            }
        }
    }
    w.json(&json!({"op": "done"}))?;
    Ok((list.len(), bytes))
}

/// Receive one subvolume named `name` into `states/`, checked on the way (`recv`): the
/// subvolume its signed record names, with a parent only from this project.
fn receive_one<R: Read, W: Write>(
    w: &mut Wire<R, W>,
    p: &Project,
    name: &str,
    uuid: &str,
    incremental: bool,
    clones: &[(String, Option<u64>, std::path::PathBuf)],
) -> Result<()> {
    let parent = |u: &str| -> Option<std::path::PathBuf> {
        if !incremental {
            return None;
        }
        let id = p.db.state_by_subvol(u).ok().flatten()?;
        let path = p.state_path(&id);
        path.exists().then_some(path)
    };
    let mut frames = Frames { w, buf: Vec::new(), pos: 0, done: false };
    let res = crate::recv::receive(&mut frames, &p.states_dir(), &crate::recv::Expect { name, uuid, parent: &parent, clones });
    let drained = frames.drain();
    res.and(drained)
}

fn recv_states<R: Read, W: Write>(w: &mut Wire<R, W>, p: &Project) -> Result<usize> {
    let _lock = p.lock()?;
    let mut n = 0;
    loop {
        let v = w.recv_json()?;
        let res = match v["op"].as_str() {
            Some("done") => break,
            Some("cache" | "layer") => {
                // Only a cache that a known state names.
                let name = v["name"].as_str().unwrap_or("").to_string();
                let id = name.split('@').next().unwrap_or("").to_string();
                let known = p.db.state(&id).ok().flatten().map(|s| s.caches.values().any(|x| x == &name)).unwrap_or(false);
                if !known || !crate::model::is_cache_name(&name, &id) {
                    bail!("unknown cache {name:?}");
                }
                if p.cache_path(&id, &name).exists() {
                    bail!("cache {name} exists here already");
                }
                // The cache must be the subvolume that the signed save.caches record names.
                let uuid = p.db.cache_uuid(&id, &name)?.ok_or_else(|| anyhow!("no signed subvolume id for cache {name}"))?;
                receive_one(w, p, &name, &uuid, false, &[])
            }
            Some("state") => {
                // Only a state that a signed record names, as the subvolume that record names.
                let id = v["id"].as_str().unwrap_or("").to_string();
                let rec = match p.db.state(&id).ok().flatten() {
                    Some(r) if crate::model::is_uuid(&id) => r,
                    _ => bail!("no record for state {id:?}"),
                };
                if p.state_path(&id).exists() {
                    bail!("state {} exists here already", ids::short(&id));
                }
                let ids: Vec<String> = match v.get("clones") {
                    None => Vec::new(),
                    Some(value) => serde_json::from_value(value.clone())?,
                };
                if ids.len() > MAX_CLONE_SOURCES {
                    bail!("too many clone sources");
                }
                let mut clones = Vec::new();
                for source in ids {
                    let state = p.db.state(&source)?.ok_or_else(|| anyhow!("unknown clone source"))?;
                    let identity = btrfs::clone_identity(&p.state_path(&source))?;
                    if identity.0 != state.subvol {
                        bail!("clone source does not match its signed state");
                    }
                    clones.push((identity.0, identity.1, p.state_path(&source)));
                }
                receive_one(w, p, &id, &rec.subvol, true, &clones).and_then(|_| {
                    if let Err(e) = p.db.set_present(&id, true) {
                        let _ = btrfs::delete_tree(&p.state_path(&id));
                        return Err(e);
                    }
                    n += 1;
                    Ok(())
                })
            }
            _ => bail!("protocol error: {v}"),
        };
        match res {
            Ok(()) => w.json(&json!({"op": "ok"}))?,
            Err(e) => {
                w.json(&json!({"op": "error", "message": e.to_string()}))?;
                return Err(e);
            }
        }
    }
    Ok(n)
}

pub fn sync(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .flag("--auto")
        .flag("--caches|--layers")
        .flag("--push-only")
        .flag("--pull-only")
        .flag("-q|--quiet")
        .value("--project");
    let a = parse(&spec, args)?;
    if a.paths.is_empty() {
        return Err(exit(
            128,
            "usage: layr sync [--auto] [--caches] [--push-only|--pull-only] -- <command that runs 'layr serve-peer <project>' on the peer>",
        ));
    }
    let h = here(ctx)?;
    h.repo.require(ctx, "sync")?;
    let repo = &h.repo;
    let mut c = Command::new(&a.paths[0]);
    c.args(&a.paths[1..]).stdin(Stdio::piped()).stdout(Stdio::piped());
    crate::privs::command_as(
        &mut c,
        ctx.caller.uid,
        &crate::privs::pick_env(ctx, &["SSH_AUTH_SOCK", "PATH", "LAYR_ROOT", "LAYR_SOCKET"]),
    )?;
    let mut child = c.spawn().context("start the peer command")?;
    let mut w = Wire { r: child.stdout.take().unwrap(), w: std::io::BufWriter::new(child.stdin.take().unwrap()) };
    let started = std::time::Instant::now();
    let auto = a.has("--auto");
    let result = (|| -> Result<(usize, usize, usize, usize, u64)> {
        let mine = present(&repo.p)?;
        w.json(&json!({"op": "hello", "compressed_send_v2": btrfs::supports_compressed_receive(), "clone_sources_v1": true, "clone_identities_v1": clone_identities(&repo.p)?, "project": repo.p.name, "project_id": repo.p.id, "machine": repo.p.store.machine.id, "public_key": own_key(&repo.p), "seqs": seqs(&repo.p)?, "present": mine, "caches": present_caches(&repo.p)?, "layers": present_caches(&repo.p)?}))?;
        let hello = w.recv_json()?;
        // The peer holds this project, or a new empty copy of it.
        let peer_project = hello["project_id"].as_str().unwrap_or("");
        if !peer_project.is_empty() && peer_project != repo.p.id {
            bail!("the peer holds another project ({peer_project})");
        }
        let peer = (hello["machine"].as_str().unwrap_or("").to_string(), hello["public_key"].as_str().unwrap_or("").to_string());
        let peer_caches: BTreeSet<String> =
            serde_json::from_value(hello.get("caches").or_else(|| hello.get("layers")).cloned().unwrap_or(Value::Null))
                .unwrap_or_default();
        let peer_seqs: BTreeMap<String, u64> = serde_json::from_value(hello["seqs"].clone())?;
        let peer_present: BTreeSet<String> = serde_json::from_value(hello["present"].clone())?;
        // Records both ways.
        let out_lines = missing_lines(&repo.p, &peer_seqs)?;
        let sent_records = out_lines.len();
        send_records(&mut w, &out_lines)?;
        let lines = recv_records(&mut w)?;
        // The admin chose this peer: its records are taken, and after they passed the checks its
        // machine is trusted from now on.
        let got_records = {
            let _lock = repo.p.lock()?;
            let n = take_lines(&repo.p, &lines, Trust::Peer(&peer.0, &peer.1))?;
            if !peer.0.is_empty() && !peer.1.is_empty() {
                trust(ctx, &repo.p, &peer.0, &peer.1)?;
            }
            n
        };
        let _ = w.recv_json()?;
        // States to the peer.
        let (mut sent, mut bytes) = (0, 0);
        if !a.has("--pull-only") {
            let list = plan(repo, &mine, &peer_present, auto)?;
            let (n, b) = send_states(
                &mut w,
                repo,
                &list,
                a.has("--caches"),
                &peer_caches,
                &peer_present,
                SendFeatures::from_hello(&hello),
            )?;
            sent = n;
            bytes = b;
        } else {
            w.json(&json!({"op": "done"}))?;
        }
        // States from the peer.
        let mine_now = present(&repo.p)?;
        let want: Vec<String> = if a.has("--push-only") {
            vec![]
        } else {
            peer_present.iter().filter(|id| !mine_now.contains(*id) && wanted(&repo.p, id, auto)).cloned().collect()
        };
        w.json(&json!({"op": "want", "clone_identities_v1": clone_identities(&repo.p)?, "ids": want, "have": mine_now, "caches": a.has("--caches"), "have_caches": present_caches(&repo.p)?, "have_layers": present_caches(&repo.p)?, "layers": a.has("--caches")}))?;
        let got = recv_states(&mut w, &repo.p)?;
        w.json(&json!({"op": "bye"}))?;
        Ok((sent_records, got_records, sent, got, bytes))
    })();
    drop(w);
    let _ = child.wait();
    let (sr, gr, ss, gs, bytes) = result?;
    if !a.has("-q") {
        outln!(
            ctx,
            "sync {}: records sent {sr}, received {gr}; states sent {ss} ({:.1} MiB), received {gs}; {:.2} s",
            repo.p.name,
            bytes as f64 / 1048576.0,
            started.elapsed().as_secs_f64()
        );
    }
    Ok(0)
}

/// The peer side of `layr sync`, on stdin and stdout.
pub fn serve(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("--create");
    let a = parse(&spec, args)?;
    let name = a.pos.first().ok_or_else(|| exit(128, "usage: layr serve-peer <project>"))?.clone();
    if !ctx.caller.is_root() && !(cfg!(target_os = "macos") && rustix::process::geteuid().as_raw() == ctx.caller.uid) {
        return Err(exit(1, "error: serve-peer needs root through the service"));
    }
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut w = Wire { r: stdin.lock(), w: std::io::BufWriter::new(stdout.lock()) };
    let hello = w.recv_json()?;
    let pid = hello["project_id"].as_str().unwrap_or("").to_string();
    let exists = crate::store::db_file(&ctx.store.projects_dir().join(&name)).is_file();
    if !exists {
        Project::create_empty(&ctx.store, &name)?;
    }
    let p = match Project::open(&ctx.store, &name) {
        Ok(p) => p,
        Err(_) => Project::open_partial(&ctx.store, &name)?,
    };
    if !p.id.is_empty() && p.id != pid {
        w.json(&json!({"op": "error", "message": format!("project '{name}' here has another id")}))?;
        bail!("project id mismatch");
    }
    let mut repo = Repo { p };
    let peer_seqs: BTreeMap<String, u64> = serde_json::from_value(hello["seqs"].clone())?;
    w.json(&json!({"op": "hello", "compressed_send_v2": btrfs::supports_compressed_receive(), "clone_sources_v1": true, "clone_identities_v1": clone_identities(&repo.p)?, "project_id": repo.p.id, "machine": repo.p.store.machine.id, "public_key": own_key(&repo.p), "seqs": seqs(&repo.p)?, "present": present(&repo.p)?, "caches": present_caches(&repo.p)?, "layers": present_caches(&repo.p)?}))?;
    let lines = recv_records(&mut w)?;
    let mine = missing_lines(&repo.p, &peer_seqs)?;
    send_records(&mut w, &mine)?;
    {
        // A new replica trusts the machines of the project it copies; an existing one only takes
        // records of machines it trusts.
        let _lock = repo.p.lock()?;
        let bootstrap = !exists;
        let taken = take_lines(&repo.p, &lines, if bootstrap { Trust::All } else { Trust::Known }).and_then(|n| {
            if repo.p.id.is_empty() {
                repo.p.reload_id()?;
            }
            // The records must be the project the client named.
            if repo.p.id != pid {
                bail!("the records belong to project {}, not {pid}", repo.p.id);
            }
            Ok(n)
        });
        if let Err(e) = taken {
            if bootstrap {
                drop(_lock);
                let _ = std::fs::remove_dir_all(&repo.p.dir);
            }
            let _ = w.json(&json!({"op": "error", "message": e.to_string()}));
            return Err(e);
        }
        if bootstrap {
            for (m, k) in repo.p.machine_keys_of(&lines) {
                trust(ctx, &repo.p, &m, &k)?;
            }
        }
    }
    w.json(&json!({"op": "count", "count": mine.len()}))?;
    let _ = a.has("--create");
    recv_states(&mut w, &repo.p)?;
    let want = w.recv_json()?;
    let ids_v: Vec<String> = serde_json::from_value(want["ids"].clone())?;
    let have: BTreeSet<String> = serde_json::from_value(want["have"].clone())?;
    let mine_set = present(&repo.p)?;
    let mut list = Vec::new();
    let mut have2 = have.clone();
    let mut todo: Vec<(i64, String)> = ids_v
        .iter()
        .filter(|i| mine_set.contains(*i))
        .filter_map(|i| repo.p.db.state(i).ok().flatten().map(|s| (s.time, i.clone())))
        .collect();
    todo.sort();
    for (_, id) in todo {
        let mut parent = None;
        for a in repo.ancestors(&id)?.into_iter().skip(1) {
            if have2.contains(&a) && mine_set.contains(&a) {
                parent = Some(a);
                break;
            }
        }
        have2.insert(id.clone());
        list.push((id, parent));
    }
    let peer_caches: BTreeSet<String> =
        serde_json::from_value(want.get("have_caches").or_else(|| want.get("have_layers")).cloned().unwrap_or(Value::Null))
            .unwrap_or_default();
    let mut features = SendFeatures::from_hello(&hello);
    features.identities = serde_json::from_value(want["clone_identities_v1"].clone()).unwrap_or_default();
    send_states(
        &mut w,
        &repo,
        &list,
        want.get("caches").or_else(|| want.get("layers")).and_then(Value::as_bool).unwrap_or(false),
        &peer_caches,
        &have,
        features,
    )?;
    let _ = w.recv_json();
    let _ = ctx;
    Ok(0)
}
