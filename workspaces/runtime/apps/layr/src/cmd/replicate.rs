//! Replication between machines over any byte channel (ssh or any other pipe).
//!
//! Named peer remotes start `ssh host layr serve-peer <project>` and exchange data over
//! stdin and stdout. Tests can select a command transport with `--command -- <argv>`:
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
    transfer: crate::model::Transfer,
    overlay_transfers: Vec<(String, crate::model::Transfer)>,
}

impl<R: Read, W: Write> Wire<R, W> {
    fn send(&mut self, kind: u8, data: &[u8]) -> Result<()> {
        self.w.write_all(&[kind])?;
        self.w.write_all(&(data.len() as u32).to_be_bytes())?;
        self.w.write_all(data)?;
        if kind == b'D' {
            self.transfer.bytes_sent += data.len() as u64;
        }
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
        if h[0] == b'D' {
            self.transfer.bytes_received += d.len() as u64;
        }
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
    w.transfer.records_sent += lines.len();
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
            w.transfer.records_received += all.len();
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
    Ok(p.db.present_states()?.into_iter().filter(|(_, d)| !d).map(|(id, _)| id).filter(|id| p.has_state(id)).collect())
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

// The final working snapshot of a deleted line is an undo dependency, even without --auto.
fn deletion_states(rec: &crate::model::Record) -> BTreeSet<String> {
    if rec.op != "line.delete" {
        return BTreeSet::new();
    }
    rec.lines
        .values()
        .filter_map(|ch| ch.working_before.clone())
        .chain(rec.data.get("final_state").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

fn undo_dependencies(p: &Project) -> Result<BTreeSet<String>> {
    let mut stmt = p.db.conn.prepare("SELECT raw FROM records WHERE op='line.delete'")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut required = BTreeSet::new();
    for raw in rows {
        required.extend(deletion_states(&crate::oplog::parse_line(&raw?, None)?.record));
    }
    Ok(required)
}

// Transport also accepts auxiliary states, which source-history traversal rejects.
fn transfer_ancestors(p: &Project, id: &str) -> Result<Vec<String>> {
    let mut seen = BTreeSet::new();
    let mut order = Vec::new();
    let mut pending = std::collections::VecDeque::from([id.to_owned()]);
    while let Some(id) = pending.pop_front() {
        if seen.insert(id.clone()) {
            if let Some(state) = p.db.state(&id)? {
                pending.extend(state.parents);
            }
            order.push(id);
        }
    }
    Ok(order)
}

fn required_states(p: &Project) -> Result<BTreeSet<String>> {
    let mut required = undo_dependencies(p)?;
    for reference in p.db.refs("", ids::now_ms())? {
        required.extend(transfer_ancestors(p, &reference.target)?);
    }
    Ok(required)
}

fn wanted(p: &Project, id: &str, auto: bool, caches: bool, required: &BTreeSet<String>) -> bool {
    match p.db.state(id).ok().flatten() {
        Some(s) if s.kind == StateKind::Cache => caches,
        Some(s) => auto || s.kind != StateKind::Auto || required.contains(id),
        None => false,
    }
}

/// States to send, parents first, each with the best parent the peer has.
fn plan(
    repo: &Repo,
    mine: &BTreeSet<String>,
    theirs: &BTreeSet<String>,
    auto: bool,
    caches: bool,
) -> Result<Vec<(String, Option<String>)>> {
    let required = required_states(&repo.p)?;
    let mut todo: Vec<(i64, String)> = mine
        .iter()
        .filter(|id| !theirs.contains(*id) && wanted(&repo.p, id, auto, caches, &required))
        .filter_map(|id| repo.p.db.state(id).ok().flatten().map(|s| (s.time, id.clone())))
        .collect();
    todo.sort();
    let mut have: BTreeSet<String> = theirs.intersection(mine).cloned().collect();
    let mut out = Vec::new();
    for (_, id) in todo {
        let mut parent = None;
        for a in transfer_ancestors(&repo.p, &id)?.into_iter().skip(1) {
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
    ctx: &Ctx,
    w: &mut Wire<R, W>,
    repo: &Repo,
    list: &[(String, Option<String>)],
    caches: bool,
    peer_caches: &BTreeSet<String>,
    peer_states: &BTreeSet<String>,
    features: SendFeatures,
) -> Result<(usize, u64)> {
    for (id, _) in list {
        crate::secrets::check_state(ctx, repo, id)?;
    }
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
        let child = btrfs::send_optimized(pp.as_deref(), &repo.state_root(id)?, &sources, features.compressed)?
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        bytes += w.send_stream(child)?;
        let v = w.recv_json()?;
        if v["op"] != "ok" {
            bail!("peer did not store state {}", ids::short(id));
        }
        w.transfer.states_sent += 1;
        w.transfer.state_ids_sent.push(id.clone());
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

fn recv_states<R: Read, W: Write>(w: &mut Wire<R, W>, p: &Project, allow: bool) -> Result<usize> {
    let _lock = p.lock()?;
    let mut n = 0;
    loop {
        let v = w.recv_json()?;
        if !allow && v["op"] != "done" {
            bail!("state transfer violates the exchange direction");
        }
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
                if p.has_state(&id) {
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
                        let _ = p.delete_state(&id);
                        return Err(e);
                    }
                    n += 1;
                    w.transfer.states_received += 1;
                    w.transfer.state_ids_received.push(id.clone());
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

pub struct Exchange {
    pub transfer: crate::model::Transfer,
    pub overlays: Vec<(String, crate::model::Transfer)>,
}

pub fn exchange(
    ctx: &Ctx,
    command: &[String],
    direction: &str,
    auto: bool,
    caches: bool,
    quiet: bool,
    json_output: bool,
) -> Result<Exchange> {
    let h = here(ctx)?;
    let repo = &h.repo;
    let mut c = Command::new(&command[0]);
    c.args(&command[1..]).stdin(Stdio::piped()).stdout(Stdio::piped());
    crate::privs::command_as(
        &mut c,
        ctx.caller.uid,
        &crate::privs::pick_env(ctx, &["SSH_AUTH_SOCK", "PATH", "LAYR_ROOT", "LAYR_SOCKET"]),
    )?;
    let mut child = c.spawn().context("start the peer command")?;
    let mut w = Wire {
        r: child.stdout.take().unwrap(),
        w: std::io::BufWriter::new(child.stdin.take().unwrap()),
        transfer: Default::default(),
        overlay_transfers: Vec::new(),
    };
    let started = std::time::Instant::now();
    let result = (|| -> Result<(usize, usize, usize, usize, u64, Vec<String>)> {
        let mine = present(&repo.p)?;
        w.json(&json!({"op": "hello", "direction_v1": true, "overlay_sync_v1": true, "include_auto": auto, "include_caches": caches, "direction": direction, "compressed_send_v2": btrfs::supports_compressed_receive(), "clone_sources_v1": true, "clone_identities_v1": clone_identities(&repo.p)?, "project": repo.p.name, "project_id": repo.p.id, "machine": repo.p.store.machine.id, "public_key": own_key(&repo.p), "seqs": seqs(&repo.p)?, "present": mine, "caches": present_caches(&repo.p)?, "layers": present_caches(&repo.p)?}))?;
        let hello = w.recv_json()?;
        if direction != "sync" && hello["direction_v1"] != true {
            bail!("the peer does not support one-way exchange; update it or use layr sync <remote>");
        }
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
        let out_lines = if direction == "fetch" { vec![] } else { missing_lines(&repo.p, &peer_seqs)? };
        let sent_records = out_lines.len();
        send_records(&mut w, &out_lines)?;
        let lines = recv_records(&mut w)?;
        // The admin chose this peer: its records are taken, and after they passed the checks its
        // machine is trusted from now on.
        let got_records = {
            let _lock = repo.p.lock()?;
            if direction == "push" && !lines.is_empty() {
                bail!("the peer sent records during push");
            }
            let n = take_lines(&repo.p, &lines, Trust::Peer(&peer.0, &peer.1))?;
            if direction != "push" && !peer.0.is_empty() && repo.p.db.machine_key(&peer.0)?.as_deref() == Some(peer.1.as_str()) {
                trust(ctx, &repo.p, &peer.0, &peer.1)?;
            }
            n
        };
        let _ = w.recv_json()?;
        // States to the peer.
        let (mut sent, mut bytes) = (0, 0);
        if direction != "fetch" {
            let list = plan(repo, &mine, &peer_present, auto, caches)?;
            let (n, b) =
                send_states(ctx, &mut w, repo, &list, caches, &peer_caches, &peer_present, SendFeatures::from_hello(&hello))?;
            sent = n;
            bytes = b;
        } else {
            w.json(&json!({"op": "done"}))?;
        }
        // States from the peer.
        let mine_now = present(&repo.p)?;
        let required = required_states(&repo.p)?;
        let want: Vec<String> = if direction == "push" {
            vec![]
        } else {
            peer_present
                .iter()
                .filter(|id| !mine_now.contains(*id) && wanted(&repo.p, id, auto, caches, &required))
                .cloned()
                .collect()
        };
        w.json(&json!({"op": "want", "clone_identities_v1": clone_identities(&repo.p)?, "ids": want, "have": mine_now, "caches": caches, "have_caches": present_caches(&repo.p)?, "have_layers": present_caches(&repo.p)?, "layers": caches}))?;
        let got = recv_states(&mut w, &repo.p, direction != "push")?;
        let overlay_omitted = if hello["overlay_sync_v1"] == true {
            exchange_overlays(ctx, &mut w, repo, direction, auto, caches, &peer.0, &peer.1, true)?
        } else {
            repo.visible_overlay_names(ctx)?
                .into_iter()
                .map(|name| format!("overlay {name}: peer does not support overlays"))
                .collect()
        };
        w.json(&json!({"op": "bye"}))?;
        let mut omitted: BTreeSet<String> = overlay_omitted.into_iter().collect();
        if !caches {
            let outgoing = mine.difference(&peer_present).filter(|_| direction != "fetch");
            let incoming = peer_present.difference(&mine).filter(|_| direction != "push");
            for id in outgoing.chain(incoming) {
                if repo.p.db.state(id)?.is_some_and(|s| s.kind == StateKind::Cache) {
                    omitted.insert(format!("cache state {id}: use --caches"));
                }
            }
        }
        Ok((sent_records, got_records, sent, got, bytes, omitted.into_iter().collect()))
    })();
    let mut transfer = w.transfer.clone();
    transfer.direction = direction.into();
    let mut overlays = BTreeMap::<String, crate::model::Transfer>::new();
    for (name, part) in std::mem::take(&mut w.overlay_transfers) {
        let total = overlays.entry(name).or_default();
        total.direction = direction.into();
        total.records_sent += part.records_sent;
        total.records_received += part.records_received;
        total.states_sent += part.states_sent;
        total.states_received += part.states_received;
        total.bytes_sent += part.bytes_sent;
        total.bytes_received += part.bytes_received;
        total.state_ids_sent.extend(part.state_ids_sent);
        total.state_ids_received.extend(part.state_ids_received);
    }
    transfer.overlays.sort();
    transfer.overlays.dedup();
    drop(w);
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let (sr, gr, ss, gs, bytes, omitted) = result?;
    if !status.success() {
        bail!("the peer command failed: {status}");
    }
    if json_output {
        outln!(
            ctx,
            "{}",
            json!({"direction": direction, "records_sent": sr, "records_received": gr, "states_sent": ss, "states_received": gs, "bytes_sent": bytes, "omitted": omitted})
        );
    } else if !quiet {
        outln!(
            ctx,
            "{direction} {}: records sent {sr}, received {gr}; states sent {ss} ({:.1} MiB), received {gs}; {:.2} s",
            repo.p.name,
            bytes as f64 / 1048576.0,
            started.elapsed().as_secs_f64()
        );
        for reason in omitted {
            outln!(ctx, "omitted {reason}");
        }
    }
    Ok(Exchange { transfer, overlays: overlays.into_iter().collect() })
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
    let mut w = Wire {
        r: stdin.lock(),
        w: std::io::BufWriter::new(stdout.lock()),
        transfer: Default::default(),
        overlay_transfers: Vec::new(),
    };
    let hello = w.recv_json()?;
    let direction = hello["direction"].as_str().unwrap_or("sync");
    if !["sync", "push", "fetch"].contains(&direction) {
        bail!("invalid peer direction");
    }
    let pid = hello["project_id"].as_str().unwrap_or("").to_string();
    let exists = crate::store::db_file(&ctx.store.projects_dir().join(&name)).is_file();
    if !exists && direction == "fetch" {
        w.json(&json!({"op": "error", "message": "the peer project does not exist"}))?;
        bail!("the peer project does not exist");
    }
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
    w.json(&json!({"op": "hello", "direction_v1": true, "overlay_sync_v1": true, "compressed_send_v2": btrfs::supports_compressed_receive(), "clone_sources_v1": true, "clone_identities_v1": clone_identities(&repo.p)?, "project_id": repo.p.id, "machine": repo.p.store.machine.id, "public_key": own_key(&repo.p), "seqs": seqs(&repo.p)?, "present": present(&repo.p)?, "caches": present_caches(&repo.p)?, "layers": present_caches(&repo.p)?}))?;
    let lines = recv_records(&mut w)?;
    if direction == "fetch" && !lines.is_empty() {
        bail!("the client sent records during fetch");
    }
    let mine = if direction == "push" { vec![] } else { missing_lines(&repo.p, &peer_seqs)? };
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
    recv_states(&mut w, &repo.p, direction != "fetch")?;
    let want = w.recv_json()?;
    let ids_v: Vec<String> = serde_json::from_value(want["ids"].clone())?;
    if direction == "push" && !ids_v.is_empty() {
        bail!("the client requested states during push");
    }
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
        for a in transfer_ancestors(&repo.p, &id)?.into_iter().skip(1) {
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
        ctx,
        &mut w,
        &repo,
        &list,
        want.get("caches").or_else(|| want.get("layers")).and_then(Value::as_bool).unwrap_or(false),
        &peer_caches,
        &have,
        features,
    )?;
    if hello["overlay_sync_v1"] == true {
        exchange_overlays(
            ctx,
            &mut w,
            &repo,
            direction,
            hello["include_auto"].as_bool().unwrap_or(false),
            hello["include_caches"].as_bool().unwrap_or(false),
            hello["machine"].as_str().unwrap_or(""),
            hello["public_key"].as_str().unwrap_or(""),
            false,
        )?;
    }
    let _ = w.recv_json();
    let _ = ctx;
    Ok(0)
}

/// Plaintext overlays only go to an explicitly permitted machine.
fn overlay_peer_allowed(repo: &Repo, name: &str, machine: &str, public: &str) -> Result<bool> {
    let info = repo.need_overlay(name)?;
    let peers = if info.hidden {
        repo.overlay_project(name)?.db.overlay_config(&format!("overlay.{name}.peers"))?
    } else {
        repo.p.config_value(&format!("overlay.{name}.peers"))?
    };
    if let Some(peers) = peers {
        let peers = peers.as_array().context("overlay peers must be an array of machine IDs")?;
        return Ok(peers.iter().any(|peer| peer.as_str() == Some(machine)));
    }
    let orepo = repo.overlay_repo(name)?;
    Ok(orepo.policy().access.get(&format!("machine:{machine}")).is_some_and(|role| *role >= crate::access::Role::Reader)
        && repo.p.db.trusted(machine)?.as_deref() == Some(public))
}

#[allow(clippy::too_many_arguments)]
fn exchange_overlays<R: Read, W: Write>(
    ctx: &Ctx,
    w: &mut Wire<R, W>,
    repo: &Repo,
    direction: &str,
    auto: bool,
    caches: bool,
    peer: &str,
    public: &str,
    initiator: bool,
) -> Result<Vec<String>> {
    let mut omitted = Vec::new();
    for send_first in [true, false] {
        let sending = initiator == send_first;
        let enabled = if initiator { direction != "fetch" } else { direction != "push" };
        if sending {
            let mut names = Vec::new();
            let mut skipped = Vec::new();
            if enabled {
                for name in repo.visible_overlay_names(ctx)? {
                    if repo.overlay_repo(&name).is_err() {
                        continue;
                    }
                    if overlay_peer_allowed(repo, &name, peer, public)? {
                        names.push(name);
                    } else {
                        omitted.push(format!("overlay {name}: peer is not allowed to hold it"));
                        if !repo.need_overlay(&name)?.hidden {
                            skipped.push(name);
                        }
                    }
                }
            }
            w.json(&json!({"op":"overlays", "names":names, "omitted":skipped}))?;
            for name in names {
                let parent_transfer = std::mem::take(&mut w.transfer);
                let orepo = repo.overlay_repo(&name)?;
                w.json(&json!({"op":"overlay", "name":name, "id":orepo.p.id}))?;
                let hello = w.recv_json()?;
                if hello["op"] != "overlay.hello" {
                    bail!("protocol error: expected overlay hello");
                }
                let seqs: BTreeMap<String, u64> = serde_json::from_value(hello["seqs"].clone())?;
                let have: BTreeSet<String> = serde_json::from_value(hello["present"].clone())?;
                let have_caches: BTreeSet<String> = serde_json::from_value(hello["caches"].clone())?;
                send_records(w, &missing_lines(&orepo.p, &seqs)?)?;
                let _ = w.recv_json()?;
                let list = plan(&orepo, &present(&orepo.p)?, &have, auto, caches)?;
                send_states(ctx, w, &orepo, &list, caches, &have_caches, &have, SendFeatures::from_hello(&hello))?;
                let mut overlay_transfer = std::mem::replace(&mut w.transfer, parent_transfer);
                overlay_transfer.direction = direction.into();
                if overlay_transfer.records_sent > 0 || overlay_transfer.states_sent > 0 {
                    if !repo.need_overlay(&name)?.hidden {
                        w.transfer.overlays.push(name.clone());
                    }
                    w.overlay_transfers.push((name, overlay_transfer));
                }
            }
        } else {
            let offer = w.recv_json()?;
            if offer["op"] != "overlays" {
                bail!("protocol error: expected overlays");
            }
            let names: Vec<String> = serde_json::from_value(offer["names"].clone())?;
            let skipped: Vec<String> = serde_json::from_value(offer["omitted"].clone()).unwrap_or_default();
            for name in skipped {
                crate::store::check_name("overlay", &name)?;
                omitted.push(format!("overlay {name}: peer is not allowed to hold it"));
            }
            if (initiator && direction == "push" || !initiator && direction == "fetch") && !names.is_empty() {
                bail!("peer sends overlays against the exchange direction");
            }
            let mut seen = BTreeSet::new();
            for name in names {
                crate::store::check_name("overlay", &name)?;
                if !seen.insert(name.clone()) {
                    bail!("duplicate overlay offer");
                }
                let offer = w.recv_json()?;
                if offer["op"] != "overlay" || offer["name"].as_str() != Some(&name) {
                    bail!("invalid overlay offer");
                }
                let id = offer["id"].as_str().filter(|id| crate::model::is_uuid(id)).context("invalid overlay project id")?;
                let pname = format!("{}%{name}", repo.p.name);
                let mut project = if crate::store::db_file(&ctx.store.project_dir(&pname)?).exists() {
                    Project::open_partial(&ctx.store, &pname)?
                } else {
                    Project::create_empty_overlay(&ctx.store, &pname)?
                };
                if !project.id.is_empty() && project.id != id {
                    bail!("overlay project id differs");
                }
                w.json(&json!({"op":"overlay.hello","seqs":seqs(&project)?,"present":present(&project)?,
                    "caches":present_caches(&project)?,"clone_identities_v1":clone_identities(&project)?,
                    "compressed_send_v2":btrfs::supports_compressed_receive(),"clone_sources_v1":true}))?;
                let parent_transfer = std::mem::take(&mut w.transfer);
                let lines = recv_records(w)?;
                let trusted = crate::store_remote::overlays::trusted_keys(repo)?;
                for line in &lines {
                    let signed = crate::oplog::parse_line(line, None)?;
                    let key =
                        trusted.get(&signed.record.machine).context("overlay record signer is not trusted by the parent")?;
                    crate::oplog::parse_line(line, Some(key))?;
                    if signed.record.project != id {
                        bail!("overlay record belongs to another project");
                    }
                }
                let _lock = project.lock()?;
                let bootstrap = project.id.is_empty();
                // Parent trust is checked above, before the overlay's first project record exists.
                project.import_lines(&lines, Trust::All)?;
                if bootstrap {
                    project.reload_id()?;
                }
                if project.id != id {
                    bail!("overlay project id differs");
                }
                for (machine, key) in &trusted {
                    trust(ctx, &project, machine, &hex::encode(key.to_bytes()))?;
                }
                drop(_lock);
                w.json(&json!({"op":"overlay.ready"}))?;
                recv_states(w, &project, true)?;
                let mut overlay_transfer = std::mem::replace(&mut w.transfer, parent_transfer);
                overlay_transfer.direction = direction.into();
                if overlay_transfer.records_received > 0 || overlay_transfer.states_received > 0 {
                    if !repo.need_overlay(&name)?.hidden {
                        w.transfer.overlays.push(name.clone());
                    }
                    w.overlay_transfers.push((name, overlay_transfer));
                }
            }
        }
    }
    Ok(omitted)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires a root Btrfs fixture and a built peer binary"]
    fn auxiliary_states_cross_the_peer_protocol() {
        use crate::ctx::Caller;
        use crate::model::Record;
        // Only a root Btrfs fixture sets this folder.
        let Some(base) = std::env::var_os("LAYR_TEST_AUX_ROOT").map(std::path::PathBuf::from) else { return };
        let binary = std::env::var("LAYR_TEST_BINARY").expect("LAYR_TEST_BINARY");
        let source = base.join("source");
        btrfs::create_subvolume(&source).unwrap();
        std::fs::write(source.join("payload"), "auxiliary payload").unwrap();
        let ctx = Ctx {
            store: crate::store::Store::open(&base.join("a")).unwrap(),
            caller: Caller::from_uid(0),
            cwd: base.clone(),
            env: std::collections::HashMap::from([("LAYR_PROJECT".into(), "p".into())]),
            out: std::cell::RefCell::new(Box::new(std::fs::File::create(base.join("report.json")).unwrap())),
        };
        let repo = Repo { p: Project::create(&ctx.store, "p", &ctx.actor(), json!({})).unwrap() };
        let mut ids = Vec::new();
        for (kind, name) in [(StateKind::Cache, "caches/peer"), (StateKind::Derived, "derived/peer")] {
            let _lock = repo.p.lock().unwrap();
            let mut state = repo.new_state(&ctx, &source, kind, vec![], "peer fixture", None).unwrap();
            state.context = Some("peer-fixture-v1".into());
            let id = state.id.clone();
            let mut rec = Record::new("fixture");
            rec.states.push(state);
            repo.commit_record(&ctx, rec).unwrap();
            let mut rec = Record::new("ref.set");
            rec.data = json!({"ref_change":{"name":name,"before":null,"after":{"name":name,"target":id,"expires_at":null}}});
            repo.commit_record(&ctx, rec).unwrap();
            ids.push(id);
        }
        drop(repo);
        let peer_root = std::path::PathBuf::from(std::env::var_os("LAYR_TEST_AUX_PEER_ROOT").expect("LAYR_TEST_AUX_PEER_ROOT"));
        let command = vec![
            "env".into(),
            format!("LAYR_ROOT={}", peer_root.display()),
            "LAYR_DIRECT=1".into(),
            binary,
            "serve-peer".into(),
            "p".into(),
        ];
        exchange(&ctx, &command, "push", false, false, false, true).unwrap();
        let report: Value = serde_json::from_slice(&std::fs::read(base.join("report.json")).unwrap()).unwrap();
        assert!(report["omitted"].as_array().unwrap().iter().any(|v| v.as_str().unwrap().contains(&ids[0])));
        let peer_state = |id: &str| peer_root.join("projects/p/states").join(id);
        assert!(!peer_state(&ids[0]).exists());
        assert_eq!(std::fs::read_to_string(peer_state(&ids[1]).join("payload")).unwrap(), "auxiliary payload");
        exchange(&ctx, &command, "push", false, true, true, false).unwrap();
        assert_eq!(std::fs::read_to_string(peer_state(&ids[0]).join("payload")).unwrap(), "auxiliary payload");
        // Fetch must use the same policy, and the server must traverse auxiliary states.
        for id in &ids {
            btrfs::delete_tree(&ctx.store.projects_dir().join("p/states").join(id)).unwrap();
        }
        exchange(&ctx, &command, "fetch", false, false, true, false).unwrap();
        assert!(!ctx.store.projects_dir().join("p/states").join(&ids[0]).exists());
        assert!(ctx.store.projects_dir().join("p/states").join(&ids[1]).join("payload").exists());
        exchange(&ctx, &command, "fetch", false, true, true, false).unwrap();
        assert!(ctx.store.projects_dir().join("p/states").join(&ids[0]).join("payload").exists());
    }

    #[test]
    fn live_refs_include_auto_ancestry_and_gate_only_cache_states() {
        use crate::model::{Author, Record};
        let dir = std::env::temp_dir().join(format!("layr-peer-refs-{}", ids::new_id()));
        let store = crate::store::Store::open(&dir).unwrap();
        let project_dir = store.projects_dir().join("p");
        std::fs::create_dir_all(project_dir.join("meta/db")).unwrap();
        std::fs::create_dir(project_dir.join("states")).unwrap();
        let p = Project {
            store: &store,
            name: "p".into(),
            dir: project_dir.clone(),
            id: ids::new_id(),
            db: crate::db::Db::open(&project_dir.join("meta/db/layr.sqlite")).unwrap(),
        };
        let actor = crate::model::Actor { uid: 0, name: "root".into(), session: None };
        let mut rec = Record::new("project");
        rec.data = json!({"id":p.id,"name":"p","config":{}});
        p.append(rec, &actor).unwrap();
        let mut all: Vec<String> = Vec::new();
        for (index, kind) in
            [StateKind::Auto, StateKind::Auto, StateKind::Auto, StateKind::Cache, StateKind::Derived].into_iter().enumerate()
        {
            let id = ids::new_id();
            let parents = if index == 1 { vec![all[0].clone()] } else { vec![] };
            let mut rec = Record::new("fixture");
            rec.states.push(
                serde_json::from_value(json!({"id":id,"kind":kind,"parents":parents,
                "subvol":"","author":Author::default(),"time":index as i64,"context":"peer-fixture-v1"}))
                .unwrap(),
            );
            p.append(rec, &actor).unwrap();
            all.push(id);
        }
        for (name, index, expires_at) in
            [("pins/auto", 1, None), ("pins/expired", 2, Some(1)), ("caches/test", 3, None), ("derived/test", 4, None)]
        {
            let mut rec = Record::new("ref.set");
            rec.data = json!({"ref_change":{"name":name,"before":null,"after":{"name":name,"target":all[index],"expires_at":expires_at}}});
            p.append(rec, &actor).unwrap();
        }
        let required = required_states(&p).unwrap();
        assert!(required.contains(&all[0]) && required.contains(&all[1]));
        assert!(!required.contains(&all[2]));
        let repo = Repo { p };
        let mine = all.iter().cloned().collect();
        let selected: Vec<_> =
            plan(&repo, &mine, &BTreeSet::new(), false, false).unwrap().into_iter().map(|(id, _)| id).collect();
        assert_eq!(selected, vec![all[0].clone(), all[1].clone(), all[4].clone()]);
        let selected: Vec<_> = plan(&repo, &mine, &BTreeSet::new(), false, true).unwrap().into_iter().map(|(id, _)| id).collect();
        assert_eq!(selected, vec![all[0].clone(), all[1].clone(), all[3].clone(), all[4].clone()]);
        drop(repo);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn deleted_line_snapshots_are_undo_dependencies() {
        let mut rec = crate::model::Record::new("line.delete");
        rec.lines.insert(
            "line".into(),
            crate::model::LineChange { before: None, after: None, working_before: Some("working".into()), working_after: None },
        );
        rec.data = json!({"final_state": "working"});
        assert_eq!(deletion_states(&rec), BTreeSet::from(["working".into()]));
        rec.op = "save".into();
        assert!(deletion_states(&rec).is_empty());
    }
}
