//! Replication between machines over any byte channel (ssh, the paired Studio server channel).
//!
//! `layr sync -- <command>` starts `<command>`, which must run `layr serve-peer <project>` on the
//! other machine, and talks to it over the command's stdin and stdout:
//!
//! 1. hello: project id, per-machine sequence numbers, states present.
//! 2. records: each side sends the records the other lacks. Records are checked against the
//!    machine key from the first record of that machine's log, and appended verbatim.
//! 3. states: `btrfs send -p <latest common state>` for every state the peer lacks
//!    (commits, imports and stashes; automatic states with `--auto`).

use super::{exit, here};
use crate::args::{parse, Spec};
use crate::btrfs;
use crate::ctx::Ctx;
use crate::ids;
use crate::model::StateKind;
use crate::repo::Repo;
use crate::store::Project;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::process::{Command, Stdio};

const MAX_FRAME: usize = 16 << 20;
/// Records travel in messages of at most this many lines.
const RECORDS_PER_MESSAGE: usize = 1000;

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
    /// Receive a btrfs stream into a folder.
    fn recv_stream(&mut self, dir: &std::path::Path) -> Result<()> {
        // `-e`: stop at the end of the first stream; a sender cannot add more subvolumes.
        let mut child =
            Command::new("btrfs").args(["receive", "-q", "-e"]).arg(dir).stdin(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        let mut ok = true;
        loop {
            let (k, d) = self.recv()?;
            match k {
                b'D' => {
                    if ok && stdin.write_all(&d).is_err() {
                        ok = false;
                    }
                }
                b'E' => break,
                b'X' => {
                    ok = false;
                    break;
                }
                _ => bail!("protocol error in a stream"),
            }
        }
        drop(stdin);
        let out = child.wait_with_output()?;
        if !ok || !out.status.success() {
            bail!("btrfs receive failed: {}", String::from_utf8_lossy(&out.stderr).trim());
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

fn recv_records<R: Read, W: Write>(w: &mut Wire<R, W>) -> Result<Vec<String>> {
    let mut all = Vec::new();
    loop {
        let v = w.recv_json()?;
        if v["op"] != "records" {
            bail!("protocol error: expected records");
        }
        let lines: Vec<String> = serde_json::from_value(v["lines"].clone())?;
        all.extend(lines);
        if !v["more"].as_bool().unwrap_or(false) {
            return Ok(all);
        }
    }
}

fn seqs(p: &Project) -> Result<BTreeMap<String, u64>> {
    Ok(p.db.max_seqs()?.into_iter().collect())
}

/// Layer snapshots on this machine (`<state-id>@<path>`).
fn present_layers(p: &Project) -> Result<BTreeSet<String>> {
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
fn take_lines(p: &Project, lines: &[String], bootstrap: bool) -> Result<usize> {
    p.import_lines(lines, bootstrap)
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

fn send_layer<R: Read, W: Write>(w: &mut Wire<R, W>, repo: &Repo, snap: &str) -> Result<u64> {
    w.json(&json!({"op": "layer", "name": snap}))?;
    let child =
        btrfs::send_command(None, &repo.p.layer_path(snap.split('@').next().unwrap_or(""), snap)).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let n = w.send_stream(child)?;
    if w.recv_json()?["op"] != "ok" {
        bail!("peer did not store layer {snap}");
    }
    Ok(n)
}

fn send_states<R: Read, W: Write>(
    w: &mut Wire<R, W>,
    repo: &Repo,
    list: &[(String, Option<String>)],
    layers: bool,
    peer_layers: &BTreeSet<String>,
    peer_states: &BTreeSet<String>,
) -> Result<(usize, u64)> {
    let mut bytes = 0;
    // Layers of states the peer has already (sent earlier without --layers).
    if layers {
        for id in peer_states {
            if let Some(s) = repo.p.db.state(id)? {
                for snap in s.layers.values() {
                    if !peer_layers.contains(snap) && repo.p.layer_path(id, snap).exists() {
                        bytes += send_layer(w, repo, snap)?;
                    }
                }
            }
        }
    }
    for (id, parent) in list {
        w.json(&json!({"op": "state", "id": id, "parent": parent}))?;
        let pp = parent.as_ref().map(|p| repo.p.state_path(p));
        let child =
            btrfs::send_command(pp.as_deref(), &repo.p.state_path(id)).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        bytes += w.send_stream(child)?;
        let v = w.recv_json()?;
        if v["op"] != "ok" {
            bail!("peer did not store state {}", ids::short(id));
        }
        // Regenerable layers travel only for a live move (warm builds on the peer).
        if layers {
            if let Some(s) = repo.p.db.state(id)? {
                for snap in s.layers.values() {
                    let path = repo.p.layer_path(id, snap);
                    if !path.exists() || peer_layers.contains(snap) {
                        continue;
                    }
                    w.json(&json!({"op": "layer", "name": snap}))?;
                    let child = btrfs::send_command(None, &path).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
                    bytes += w.send_stream(child)?;
                    let v = w.recv_json()?;
                    if v["op"] != "ok" {
                        bail!("peer did not store layer {snap}");
                    }
                }
            }
        }
    }
    w.json(&json!({"op": "done"}))?;
    Ok((list.len(), bytes))
}

/// Receive one subvolume named `name` into `states/`: exactly one new entry with that name,
/// read-only, with the received UUID that the signed record names (for a state).
fn receive_one<R: Read, W: Write>(w: &mut Wire<R, W>, p: &Project, name: &str, expected_uuid: Option<&str>) -> Result<()> {
    let dir = p.states_dir();
    let before: BTreeSet<String> = std::fs::read_dir(&dir)?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    let received = w.recv_stream(&dir);
    let after: BTreeSet<String> = std::fs::read_dir(&dir)?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    let new: Vec<String> = after.difference(&before).cloned().collect();
    let check = (|| -> Result<()> {
        received?;
        if new.len() != 1 || new[0] != name {
            bail!("the stream did not contain exactly {name} (got {:?})", new);
        }
        let info = btrfs::subvol_info(&dir.join(name))?;
        if !info.readonly || info.received_uuid.is_none() {
            bail!("incomplete receive of {name}");
        }
        if let Some(u) = expected_uuid {
            if info.received_uuid.as_deref() != Some(u) {
                bail!("the stream of {name} is not the subvolume its record names");
            }
        }
        Ok(())
    })();
    if check.is_err() {
        for n in &new {
            let _ = btrfs::delete_tree(&dir.join(n));
        }
    }
    check
}

fn recv_states<R: Read, W: Write>(w: &mut Wire<R, W>, p: &Project) -> Result<usize> {
    let _lock = p.lock()?;
    let mut n = 0;
    loop {
        let v = w.recv_json()?;
        let res = match v["op"].as_str() {
            Some("done") => break,
            Some("layer") => {
                // Only a layer that a known state names.
                let name = v["name"].as_str().unwrap_or("").to_string();
                let id = name.split('@').next().unwrap_or("").to_string();
                let known = p.db.state(&id).ok().flatten().map(|s| s.layers.values().any(|x| x == &name)).unwrap_or(false);
                if !known || !crate::model::is_layer_name(&name, &id) {
                    bail!("unknown layer {name:?}");
                }
                if p.layer_path(&id, &name).exists() {
                    bail!("layer {name} exists here already");
                }
                receive_one(w, p, &name, None)
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
                receive_one(w, p, &id, Some(&rec.subvol)).map(|_| {
                    let _ = p.db.set_present(&id, true);
                    n += 1;
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
    let spec =
        Spec::new().flag("--auto").flag("--layers").flag("--push-only").flag("--pull-only").flag("-q|--quiet").value("--project");
    let a = parse(&spec, args)?;
    if a.paths.is_empty() {
        return Err(exit(
            128,
            "usage: layr sync [--auto] [--push-only|--pull-only] -- <command that runs 'layr serve-peer <project>' on the peer>",
        ));
    }
    let h = here(ctx)?;
    h.repo.require_lead(ctx, "replicate the project")?;
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
        w.json(&json!({"op": "hello", "project": repo.p.name, "project_id": repo.p.id, "machine": repo.p.store.machine.id, "public_key": own_key(&repo.p), "seqs": seqs(&repo.p)?, "present": mine, "layers": present_layers(&repo.p)?}))?;
        let hello = w.recv_json()?;
        // The lead chose this peer: its machine is trusted from now on.
        {
            let _lock = repo.p.lock()?;
            if let (Some(m), Some(k)) = (hello["machine"].as_str(), hello["public_key"].as_str()) {
                trust(ctx, &repo.p, m, k)?;
            }
        }
        let peer_layers: BTreeSet<String> = serde_json::from_value(hello["layers"].clone()).unwrap_or_default();
        let peer_seqs: BTreeMap<String, u64> = serde_json::from_value(hello["seqs"].clone())?;
        let peer_present: BTreeSet<String> = serde_json::from_value(hello["present"].clone())?;
        // Records both ways.
        let out_lines = missing_lines(&repo.p, &peer_seqs)?;
        let sent_records = out_lines.len();
        send_records(&mut w, &out_lines)?;
        let lines = recv_records(&mut w)?;
        let got_records = {
            let _lock = repo.p.lock()?;
            take_lines(&repo.p, &lines, false)?
        };
        let _ = w.recv_json()?;
        // States to the peer.
        let (mut sent, mut bytes) = (0, 0);
        if !a.has("--pull-only") {
            let list = plan(repo, &mine, &peer_present, auto)?;
            let (n, b) = send_states(&mut w, repo, &list, a.has("--layers"), &peer_layers, &peer_present)?;
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
        w.json(&json!({"op": "want", "ids": want, "have": mine_now, "layers": a.has("--layers"), "have_layers": present_layers(&repo.p)?}))?;
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
    if !ctx.caller.is_root() {
        return Err(exit(1, "error: serve-peer needs root (it receives btrfs streams)"));
    }
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut w = Wire { r: stdin.lock(), w: std::io::BufWriter::new(stdout.lock()) };
    let hello = w.recv_json()?;
    let pid = hello["project_id"].as_str().unwrap_or("").to_string();
    let exists = ctx.store.projects_dir().join(&name).join("meta").join("layr.sqlite").is_file();
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
    w.json(&json!({"op": "hello", "machine": repo.p.store.machine.id, "public_key": own_key(&repo.p), "seqs": seqs(&repo.p)?, "present": present(&repo.p)?, "layers": present_layers(&repo.p)?}))?;
    let lines = recv_records(&mut w)?;
    let mine = missing_lines(&repo.p, &peer_seqs)?;
    send_records(&mut w, &mine)?;
    {
        // A new replica trusts the machines of the project it copies; an existing one only takes
        // records of machines it trusts.
        let _lock = repo.p.lock()?;
        let bootstrap = !exists;
        let taken = take_lines(&repo.p, &lines, bootstrap).and_then(|n| {
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
    let peer_layers: BTreeSet<String> = serde_json::from_value(want["have_layers"].clone()).unwrap_or_default();
    send_states(&mut w, &repo, &list, want["layers"].as_bool().unwrap_or(false), &peer_layers, &have)?;
    let _ = w.recv_json();
    let _ = ctx;
    Ok(0)
}
