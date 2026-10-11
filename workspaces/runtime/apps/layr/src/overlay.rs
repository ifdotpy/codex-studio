//! Overlays (docs/overlays.md): file trees with an owner, readers and their own history, shown in
//! a line above the shared set.
//!
//! An overlay is the internal project `<project>%<overlay>` below the project's private `meta`
//! folder, with its own states, signed records and access policy (its owner is its admin, its
//! readers have the reader role). History, permissions and fsck work as for any project, and
//! backups, pushes and replication of the project never see it. Its lines are pointers without
//! working folders: `user-<uid>` (shared mode: one for all lines of a user) and `line-<line id>`
//! (follow mode: one per line of the project, branched and merged with it).
//!
//! A line shows a stack: the shared set (the line's head) at the bottom and overlays above it, as
//! the setting `stack.<line id>` lists them (an overlay name and the overlay state the line shows).
//! The working folder holds the files of all layers, the upper file winning. The base
//! (`meta/base/<line id>@<key>`, read-only) holds the same full tree for the head and the stack,
//! so status and diff still compare two snapshots of one tree in O(changes).
//!
//! A commit splits its change list by layer: a changed path belongs to the highest layer that has
//! it (a moved file stays in the layer of its old path), a new path to the shared set unless
//! `layr overlay move` named it, and a delete goes to every layer that has the path. Each changed
//! layer gets its next state from its previous one: a writable snapshot, the layer's changes as
//! reflink copies, read-only. So the shared set never holds an overlay file. Automatic states of a
//! line with overlays leave the overlay files out.

use crate::access::{Policy, Who};
use crate::btrfs;
use crate::changes::{self, Change};
use crate::ctx::Ctx;
use crate::fsutil;
use crate::ids;
use crate::model::{Line, LineChange, LineState, Record, StateKind, StateRec};
use crate::repo::{skip_path, Repo, Temp};
use crate::store::Project;
use crate::tree::Tree;
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// One overlay of a line's stack: its name and the overlay state the line shows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layer {
    pub overlay: String,
    pub state: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// One overlay line for all lines of a user.
    Shared,
    /// The overlay line branches and merges with the line.
    Follow,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "shared" => Some(Mode::Shared),
            "follow" => Some(Mode::Follow),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Mode::Shared => "shared",
            Mode::Follow => "follow",
        }
    }
}

/// An overlay as the project knows it (setting `overlay.<name>`).
#[derive(Clone, Debug)]
pub struct Info {
    pub owner: u32,
    pub mode: Mode,
    /// The empty first state of the overlay.
    pub root: String,
}

/// The settings key of the stack of a line.
pub fn stack_key(line_id: &str) -> String {
    format!("stack.{line_id}")
}

/// Record data that sets the stack of a line.
pub fn stack_data(line_id: &str, layers: &[Layer]) -> serde_json::Value {
    serde_json::json!({"line": line_id, "layers": layers})
}

/// The files (not folders) of a tree, without `.git`.
pub fn files_of(root: &Path) -> Result<Vec<String>> {
    let mut v = Vec::new();
    fsutil::walk(root, "", &mut |rel, m| {
        if skip_path(rel) {
            return Ok(false);
        }
        if !m.is_dir() {
            v.push(rel.to_string());
        }
        Ok(true)
    })?;
    Ok(v)
}

fn has_file(root: &Path, p: &str) -> bool {
    fsutil::lmeta(root, p).map(|m| !m.is_dir()).unwrap_or(false)
}

/// Remove parent folders of `rel` that became empty (states hold files, as git trees do).
fn prune(t: &Tree, rel: &str) {
    let mut comps: Vec<&str> = rel.split('/').collect();
    comps.pop();
    while !comps.is_empty() && t.rmdir(&comps.join("/")) {
        comps.pop();
    }
}

/// The changes of one layer in a commit, applied to a writable snapshot of its previous state.
#[derive(Default, Debug)]
pub struct Ops {
    pub moves: Vec<(String, String)>,
    pub dels: BTreeSet<String>,
    pub puts: BTreeSet<String>,
}

impl Ops {
    pub fn is_empty(&self) -> bool {
        self.moves.is_empty() && self.dels.is_empty() && self.puts.is_empty()
    }

    /// Apply to `dst` (writable) with the content of `src`: moves first (the inode stays, so btrfs
    /// reports the move), then deletes, then reflink copies.
    pub fn apply(&self, dst: &Path, src: &Path, own: Option<(u32, u32)>) -> Result<()> {
        let t = Tree::open(dst)?;
        for (o, p) in &self.moves {
            if t.stat(o).is_some() {
                if t.stat(p).is_none() {
                    t.mkdirs_for(p, own)?;
                    t.rename(o, p)?;
                } else {
                    t.remove(o)?;
                }
                prune(&t, o);
            }
        }
        for p in &self.dels {
            if t.stat(p).is_some() {
                t.remove(p)?;
            }
            prune(&t, p);
        }
        for p in &self.puts {
            fsutil::copy_entry(src, p, dst, p, own)?;
        }
        Ok(())
    }
}

/// Which layer gets a change: the shared set or an overlay of the stack (index, bottom first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Owner {
    Shared,
    Overlay(usize),
}

/// The split of a change list by layer.
pub struct Split {
    pub shared: Ops,
    pub overlays: Vec<Ops>,
    /// The change list of the whole tree (with renames), for the summary.
    pub list: Vec<Change>,
}

/// Split the changes from `base` (the line's full tree) to `new` by layer. `shared` and `roots`
/// are the previous states of the layers; `pending` names paths that `layr overlay move` sends to
/// an overlay.
pub fn split(base: &Path, new: &Path, shared: &Path, roots: &[PathBuf], pending: &BTreeMap<String, usize>) -> Result<Split> {
    let list: Vec<Change> = changes::file_changes(base, new)?.into_iter().filter(|c| !skip_path(&c.path)).collect();
    let list = changes::with_renames(base, new, list);
    let all = |p: &str| -> Vec<Owner> {
        let mut v: Vec<Owner> = Vec::new();
        if has_file(shared, p) {
            v.push(Owner::Shared);
        }
        for (i, r) in roots.iter().enumerate() {
            if has_file(r, p) {
                v.push(Owner::Overlay(i));
            }
        }
        v
    };
    let top = |p: &str| -> Option<Owner> { all(p).into_iter().max() };
    let mut s = Split { shared: Ops::default(), overlays: roots.iter().map(|_| Ops::default()).collect(), list: Vec::new() };
    fn ops(s: &mut Split, o: Owner) -> &mut Ops {
        match o {
            Owner::Shared => &mut s.shared,
            Owner::Overlay(i) => &mut s.overlays[i],
        }
    }
    let mut handled: BTreeSet<String> = BTreeSet::new();
    for c in &list {
        handled.insert(c.path.clone());
        let forced = pending.get(&c.path).map(|i| Owner::Overlay(*i));
        match (&c.old, &c.new, &c.old_path) {
            // A delete goes to every layer that has the path, so no lower file shows again.
            (_, None, _) => {
                for o in all(&c.path) {
                    ops(&mut s, o).dels.insert(c.path.clone());
                }
            }
            // A moved file stays in the layer of its old path: a move never publishes it.
            (Some(_), Some(_), Some(old)) => {
                handled.insert(old.clone());
                let owner = forced.or(top(old)).unwrap_or(Owner::Shared);
                for o in all(old) {
                    if o == owner {
                        ops(&mut s, o).moves.push((old.clone(), c.path.clone()));
                    } else {
                        ops(&mut s, o).dels.insert(old.clone());
                    }
                }
                ops(&mut s, owner).puts.insert(c.path.clone());
            }
            // A new path goes to the shared set, unless `overlay move` named it.
            (None, Some(_), _) => {
                ops(&mut s, forced.unwrap_or(Owner::Shared)).puts.insert(c.path.clone());
            }
            // A change goes to the highest layer that has the path.
            (Some(_), Some(_), None) => {
                let owner = forced.or(top(&c.path)).unwrap_or(Owner::Shared);
                ops(&mut s, owner).puts.insert(c.path.clone());
                if forced.is_some() {
                    for o in all(&c.path) {
                        if o != owner {
                            ops(&mut s, o).dels.insert(c.path.clone());
                        }
                    }
                }
            }
        }
    }
    // Moves of unchanged files: out of their layer, into the overlay.
    for (p, i) in pending {
        if handled.contains(p) || !has_file(new, p) {
            continue;
        }
        let owner = Owner::Overlay(*i);
        ops(&mut s, owner).puts.insert(p.clone());
        for o in all(p) {
            if o != owner {
                ops(&mut s, o).dels.insert(p.clone());
            }
        }
    }
    s.list = list;
    Ok(s)
}

/// The key of a line's base: its head and stack.
fn base_key(head: &str, layers: &[Layer]) -> String {
    use sha2::Digest;
    let text = serde_json::json!({"head": head, "layers": layers}).to_string();
    hex::encode(sha2::Sha256::digest(text.as_bytes()))[..16].to_string()
}

impl<'a> Repo<'a> {
    // ----- overlays of the project -----

    pub fn overlay_info(&self, name: &str) -> Result<Option<Info>> {
        if crate::store::check_name("overlay", name).is_err() {
            return Ok(None);
        }
        let v = match self.p.config_value(&format!("overlay.{name}"))? {
            Some(v) if v.is_object() => v,
            _ => return Ok(None),
        };
        Ok(Some(Info {
            owner: v["owner"].as_u64().unwrap_or(0) as u32,
            mode: v["mode"].as_str().and_then(Mode::parse).unwrap_or(Mode::Shared),
            root: v["root"].as_str().unwrap_or("").to_string(),
        }))
    }

    pub fn need_overlay(&self, name: &str) -> Result<Info> {
        self.overlay_info(name)?
            .ok_or_else(|| crate::cmd::exit(1, format!("error: no overlay '{name}' in project {}", self.p.name)))
    }

    /// Names of the overlays of this project on this machine.
    pub fn overlay_names(&self) -> Result<Vec<String>> {
        Ok(self
            .p
            .db
            .config_all()?
            .into_iter()
            .filter_map(|(k, _)| k.strip_prefix("overlay.").map(|s| s.to_string()))
            .filter(|n| !n.contains('@') && crate::store::check_name("overlay", n).is_ok())
            .collect())
    }

    /// The internal project of an overlay.
    pub fn overlay_project(&self, name: &str) -> Result<Project<'a>> {
        Project::open(self.p.store, &format!("{}%{name}", self.p.name))
    }

    pub fn overlay_repo(&self, name: &str) -> Result<Repo<'a>> {
        Ok(Repo { p: self.overlay_project(name)? })
    }

    /// The user reads the overlay: the reader role in its policy (its owner is its admin).
    pub fn reads_overlay(&self, name: &str, uid: u32, gid: u32) -> Result<bool> {
        if uid == 0 {
            return Ok(true);
        }
        let op = self.overlay_project(name)?;
        Ok(Policy::load(&op).check(&Who::of(uid, gid), "read").is_ok())
    }

    fn owner_reads(&self, name: &str, owner: u32) -> Result<bool> {
        let gid = crate::ctx::Caller::from_uid(owner).gid;
        self.reads_overlay(name, owner, gid)
    }

    /// The name of the overlay line that a line of `owner` uses.
    pub fn overlay_line_name(info: &Info, line_id: &str, owner: u32) -> String {
        match info.mode {
            Mode::Shared => format!("user-{owner}"),
            Mode::Follow => format!("line-{line_id}"),
        }
    }

    // ----- the stack of a line -----

    /// The overlays a line shows, bottom first.
    pub fn layers(&self, line: &Line) -> Result<Vec<Layer>> {
        Ok(match self.p.config_value(&stack_key(&line.id))? {
            Some(v) => serde_json::from_value(v).unwrap_or_default(),
            None => Vec::new(),
        })
    }

    pub fn is_stacked(&self, line: &Line) -> Result<bool> {
        Ok(!self.p.is_overlay() && !self.layers(line)?.is_empty())
    }

    /// Every layer of the stack must still be readable by the line owner before the line changes
    /// it: a file of an overlay that is not in the stack would go to the shared set.
    pub fn check_stack(&self, line: &Line, layers: &[Layer]) -> Result<()> {
        for l in layers {
            if self.overlay_info(&l.overlay)?.is_none() || self.layer_root(l).is_none() {
                return Err(crate::cmd::exit(
                    1,
                    format!(
                        "error: line '{}' shows overlay '{}', which is not on this machine; run 'layr overlay use --remove {}'",
                        line.name, l.overlay, l.overlay
                    ),
                ));
            }
            if !self.owner_reads(&l.overlay, line.owner)? {
                return Err(crate::cmd::exit(
                    1,
                    format!(
                        "error: line '{}' shows overlay '{}', which its owner no longer reads; run 'layr overlay use' without it",
                        line.name, l.overlay
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Reading the working content of a line with overlays (status, diff, grep, ls-files, the
    /// index) needs the read right of each of its overlays, unless the caller owns the line.
    pub fn check_view(&self, ctx: &Ctx, line: &Line) -> Result<()> {
        if ctx.caller.is_root() || ctx.caller.uid == line.owner || self.p.is_overlay() {
            return Ok(());
        }
        for l in self.layers(line)? {
            if !self.reads_overlay(&l.overlay, ctx.caller.uid, ctx.caller.gid)? {
                return Err(crate::cmd::exit(
                    1,
                    format!("permission denied: line '{}' shows overlays that you do not read", line.name),
                ));
            }
        }
        Ok(())
    }

    /// The paths that `layr overlay move` sends to an overlay at the next commit. They stay until
    /// that commit or `overlay move --cancel`, whatever happens to the index: until then the
    /// automatic states leave them out.
    pub fn pending_moves(&self, line: &Line) -> BTreeMap<String, String> {
        let f = self.p.meta_dir().join("stage").join(format!("{}.moves", line.id));
        std::fs::read(&f).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default()
    }

    pub fn set_pending_moves(&self, line: &Line, m: &BTreeMap<String, String>) -> Result<()> {
        let f = self.p.meta_dir().join("stage").join(format!("{}.moves", line.id));
        if m.is_empty() {
            let _ = std::fs::remove_file(&f);
            return Ok(());
        }
        fsutil::atomic_write(&f, serde_json::to_string(m)?.as_bytes(), 0o600)
    }

    /// The key of the staging line: it is current while the head and the stack are.
    pub fn stage_key(&self, line: &Line) -> Result<String> {
        let layers = self.layers(line)?;
        Ok(if layers.is_empty() { line.head.clone() } else { format!("{}@{}", line.head, base_key(&line.head, &layers)) })
    }

    /// Files that automatic states of the line leave out: the files of its overlays (the states it
    /// shows and the newest states of its overlay lines) and the paths of pending moves.
    pub fn private_paths(&self, line: &Line) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        if self.p.is_overlay() {
            return Ok(out);
        }
        for l in self.layers(line)? {
            let op = match self.overlay_project(&l.overlay) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let mut states = vec![l.state.clone()];
            if let Some(info) = self.overlay_info(&l.overlay)? {
                if let Some(ol) = op.db.line_by_name(&Self::overlay_line_name(&info, &line.id, line.owner))? {
                    states.push(ol.head);
                }
            }
            for s in states {
                let root = op.state_path(&s);
                if root.exists() {
                    out.extend(files_of(&root)?);
                }
            }
        }
        out.extend(self.pending_moves(line).into_keys());
        Ok(out)
    }

    // ----- the base: the full tree of a line with overlays -----

    fn base_path(&self, line: &Line, head: &str, layers: &[Layer]) -> PathBuf {
        self.p.base_dir().join(format!("{}@{}", line.id, base_key(head, layers)))
    }

    /// The full tree of the line's head: the head state, or for a line with overlays its base
    /// (made when missing).
    pub fn head_tree(&self, line: &Line) -> Result<PathBuf> {
        let layers = if self.p.is_overlay() { Vec::new() } else { self.layers(line)? };
        if layers.is_empty() {
            return self.state_root(&line.head);
        }
        let path = self.base_path(line, &line.head, &layers);
        if path.exists() {
            // A base that another request just placed may still be writable for a moment.
            if !btrfs::subvol_info(&path).map(|i| i.readonly).unwrap_or(true) {
                btrfs::set_readonly(&path)?;
            }
            return Ok(path);
        }
        let view = self.assemble(line, &line.head, &layers)?;
        self.place_base(&view, &path)?;
        Ok(path)
    }

    fn place_base(&self, view: &Temp, path: &Path) -> Result<()> {
        let dir = self.p.base_dir();
        if !dir.exists() {
            std::fs::create_dir_all(&dir)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        // btrfs renames only a writable subvolume: the view is complete, so it is writable only
        // for the rename.
        btrfs::set_writable(&view.path)?;
        match std::fs::rename(&view.path, path) {
            Ok(()) => btrfs::set_readonly(path),
            // Another request made the same base at the same time.
            Err(_) if path.exists() => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// The full tree for `head` and `layers`, as a temporary read-only snapshot: a writable
    /// snapshot of the head state, the files of each overlay state from the bottom (an upper file
    /// wins), read-only.
    pub fn assemble(&self, line: &Line, head: &str, layers: &[Layer]) -> Result<Temp> {
        let _t = crate::trace::span("assemble");
        let t = Temp { path: self.p.work_dir().join(ids::new_id()) };
        btrfs::snapshot(&self.state_root(head)?, &t.path, false)?;
        self.put_layers(&t.path, layers, self.own(line))?;
        btrfs::set_readonly(&t.path)?;
        Ok(t)
    }

    /// The folder of the state a layer shows, if the overlay and the state are on this machine
    /// (a restored or replicated project has the stacks of its lines but not their overlays).
    pub fn layer_root(&self, l: &Layer) -> Option<PathBuf> {
        let op = self.overlay_project(&l.overlay).ok()?;
        let root = op.state_path(&l.state);
        root.exists().then_some(root)
    }

    /// Reflink copies of the files of each overlay state into `dst`, from the bottom. A layer
    /// whose overlay is not on this machine adds nothing (`check_stack` refuses a commit then).
    pub fn put_layers(&self, dst: &Path, layers: &[Layer], own: Option<(u32, u32)>) -> Result<()> {
        for l in layers {
            if let Some(root) = self.layer_root(l) {
                for rel in files_of(&root)? {
                    fsutil::copy_entry(&root, &rel, dst, &rel, own)?;
                }
            }
        }
        Ok(())
    }

    /// Keep `view` as the base of the line for `head` and `layers`. Older bases of the line stay a
    /// minute for requests that still read them.
    pub fn install_base(&self, line: &Line, view: Temp, head: &str, layers: &[Layer]) -> Result<()> {
        let path = self.base_path(line, head, layers);
        if layers.is_empty() {
            drop(view);
        } else if !path.exists() {
            self.place_base(&view, &path)?;
        }
        self.prune_bases(line, Some(&path), 60)
    }

    /// Delete the bases of every line that are not its current one (older than a minute), and
    /// those of deleted lines (`gc`).
    pub fn prune_all_bases(&self) -> Result<()> {
        if self.p.is_overlay() || !self.p.base_dir().exists() {
            return Ok(());
        }
        let lines = self.lines()?;
        let live: BTreeSet<String> = lines.iter().map(|l| l.id.clone()).collect();
        for l in &lines {
            let layers = self.layers(l)?;
            let keep = (!layers.is_empty()).then(|| self.base_path(l, &l.head, &layers));
            self.prune_bases(l, keep.as_deref(), 60)?;
        }
        for e in std::fs::read_dir(self.p.base_dir())?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let id = name.split('@').next().unwrap_or("").to_string();
            if !live.contains(&id) {
                let _ = btrfs::delete_tree(&e.path());
            }
        }
        Ok(())
    }

    /// Delete the bases of a line except `keep`, those older than `age` seconds.
    pub fn prune_bases(&self, line: &Line, keep: Option<&Path>, age: u64) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let dir = self.p.base_dir();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let pre = format!("{}@", line.id);
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&pre) || Some(e.path().as_path()) == keep {
                continue;
            }
            let ctime = e.metadata().map(|m| m.ctime()).unwrap_or(0);
            if now - ctime >= age as i64 {
                let _ = btrfs::delete_tree(&e.path());
            }
        }
        Ok(())
    }

    /// Records in an overlay: a new line `name` of `owner` at `start`.
    fn overlay_line_create(&self, ctx: &Ctx, name: &str, start: &str, owner: u32) -> Result<Line> {
        let line = Line {
            id: ids::new_id(),
            name: name.to_string(),
            head: start.to_string(),
            owner,
            machine: self.p.store.machine.id.clone(),
        };
        let mut rec = Record::new("line.create");
        rec.lines.insert(
            line.id.clone(),
            LineChange { before: None, after: Some(self.line_state(&line, start)), working_before: None, working_after: None },
        );
        rec.data = serde_json::json!({"name": name, "start": start});
        self.p.append(rec, &ctx.actor())?;
        Ok(line)
    }

    /// The overlay line that a line uses for overlay `info`, made at `start` when missing (only
    /// in an overlay's own repository).
    pub fn ensure_overlay_line(&self, ctx: &Ctx, name: &str, start: &str, owner: u32) -> Result<Line> {
        match self.p.db.line_by_name(name)? {
            Some(l) => Ok(l),
            None => self.overlay_line_create(ctx, name, start, owner),
        }
    }

    /// The layers a new line of `owner` takes from the line it starts from: the overlays of the
    /// source that the owner reads. A follow overlay gets a new overlay line at the source's
    /// state; a shared one uses the owner's line (made at the source's state when missing).
    pub fn inherit_layers(&self, ctx: &Ctx, source: &Line, line_id: &str, owner: u32) -> Result<Vec<Layer>> {
        let mut out = Vec::new();
        for l in self.layers(source)? {
            // An overlay that is not on this machine (a restored project) is left out.
            let info = match self.overlay_info(&l.overlay)? {
                Some(i) if self.layer_root(&l).is_some() => i,
                _ => continue,
            };
            if !self.owner_reads(&l.overlay, owner)? {
                continue;
            }
            let orepo = self.overlay_repo(&l.overlay)?;
            let _lock = orepo.p.lock()?;
            let ol = orepo.ensure_overlay_line(ctx, &Self::overlay_line_name(&info, line_id, owner), &l.state, owner)?;
            out.push(Layer { overlay: l.overlay.clone(), state: ol.head });
        }
        Ok(out)
    }
}

/// A new state of a layer from its previous one: a writable snapshot, the layer's changes from
/// `src`, read-only. No record is written.
#[allow(clippy::too_many_arguments)]
pub fn next_state(
    repo: &Repo,
    ctx: &Ctx,
    prev: &str,
    src: &Path,
    ops: &Ops,
    own: Option<(u32, u32)>,
    parents: Vec<String>,
    message: &str,
    line: Option<&str>,
) -> Result<StateRec> {
    let _t = crate::trace::span("layer state");
    let id = ids::new_id();
    let dst = repo.p.state_path(&id);
    btrfs::snapshot(&repo.state_root(prev)?, &dst, false)?;
    let made = ops.apply(&dst, src, own).and_then(|_| btrfs::set_readonly(&dst)).and_then(|_| btrfs::subvol_info(&dst));
    let info = match made {
        Ok(i) => i,
        Err(e) => {
            let _ = btrfs::delete_tree(&dst);
            return Err(e);
        }
    };
    Ok(StateRec {
        id,
        kind: StateKind::Commit,
        parents,
        line: line.map(|s| s.to_string()),
        subvol: info.uuid,
        author: ctx.author(),
        message: message.to_string(),
        time: ids::now_ms(),
        running: false,
        conflicts: Vec::new(),
        caches: BTreeMap::new(),
        cache_by: None,
        cache_shared: false,
    })
}

/// A record in an overlay that moves an overlay line to `head` (with the new states it names).
pub fn move_overlay_line(orepo: &Repo, ctx: &Ctx, op: &str, ol: &Line, head: &str, states: Vec<StateRec>) -> Result<()> {
    let mut rec = Record::new(op);
    rec.states = states;
    let st = |h: &str| LineState { name: ol.name.clone(), head: h.to_string(), owner: ol.owner, machine: ol.machine.clone() };
    rec.lines.insert(
        ol.id.clone(),
        LineChange { before: Some(st(&ol.head)), after: Some(st(head)), working_before: None, working_after: None },
    );
    rec.data = serde_json::json!({"line": ol.id});
    orepo.commit_record(ctx, rec)?;
    Ok(())
}

/// What a commit in a line with overlays made.
pub struct Committed {
    pub shared: Option<StateRec>,
    pub overlays: Vec<(String, StateRec)>,
    /// The changes of the whole tree, for the summary (when asked for).
    pub fds: Vec<crate::diff::FileDiff>,
}

/// Commit the index of a line with overlays (docs/overlays.md, section 3.2). The caller holds the
/// project lock and has checked that the caller owns the line. `new_tree` is a read-only snapshot
/// of the index; it becomes the line's base. Returns None when there is nothing to commit.
#[allow(clippy::too_many_arguments)]
pub fn commit_layers(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    layers: &[Layer],
    new_tree: Temp,
    message: &str,
    allow_empty: bool,
    author: Option<(String, String)>,
    summary: bool,
) -> Result<Option<Committed>> {
    let base = repo.head_tree(line)?;
    let shared_root = repo.state_root(&line.head)?;
    let mut orepos = Vec::new();
    let mut roots = Vec::new();
    for l in layers {
        let r = repo.overlay_repo(&l.overlay)?;
        roots.push(r.state_root(&l.state)?);
        orepos.push(r);
    }
    let by_name: BTreeMap<&str, usize> = layers.iter().enumerate().map(|(i, l)| (l.overlay.as_str(), i)).collect();
    let mut pending = BTreeMap::new();
    for (p, o) in repo.pending_moves(line) {
        match by_name.get(o.as_str()) {
            Some(i) => {
                pending.insert(p, *i);
            }
            None => bail!("'{p}' is to move to overlay '{o}', which line '{}' does not show", line.name),
        }
    }
    let s = split(&base, &new_tree.path, &shared_root, &roots, &pending)?;
    if s.shared.is_empty() && s.overlays.iter().all(|o| o.is_empty()) && !allow_empty {
        return Ok(None);
    }
    // A protected line takes changes of the shared set only through merges; changes of its
    // overlays belong to its owner.
    if !s.shared.is_empty() {
        repo.authorize_line(ctx, line, crate::repo::LineOp::Direct)?;
    }
    let own = repo.own(line);
    let mut made: Vec<(usize, StateRec, Line)> = Vec::new();
    let discard = |made: &[(usize, StateRec, Line)], orepos: &[Repo]| {
        for (i, st, _) in made {
            orepos[*i].discard_states(std::slice::from_ref(st));
        }
    };
    for (i, ops) in s.overlays.iter().enumerate() {
        if ops.is_empty() {
            continue;
        }
        let l = &layers[i];
        let info = repo.need_overlay(&l.overlay)?;
        let orepo = &orepos[i];
        let ol = orepo
            .p
            .db
            .line_by_name(&Repo::overlay_line_name(&info, &line.id, line.owner))?
            .ok_or_else(|| anyhow!("line '{}' has no line in overlay '{}'", line.name, l.overlay))?;
        if ol.head != l.state {
            discard(&made, &orepos);
            return Err(crate::cmd::exit(
                1,
                format!(
                    "error: overlay '{}' changed in another line since this line took it; run 'layr overlay use {}' to take its newest state, then commit",
                    l.overlay,
                    layers.iter().map(|x| x.overlay.as_str()).collect::<Vec<_>>().join(" ")
                ),
            ));
        }
        let mut st =
            match next_state(orepo, ctx, &l.state, &new_tree.path, ops, own, vec![l.state.clone()], message, Some(&ol.id)) {
                Ok(st) => st,
                Err(e) => {
                    discard(&made, &orepos);
                    return Err(e);
                }
            };
        if let Some((n, e)) = &author {
            st.author.name = n.clone();
            st.author.email = e.clone();
        }
        // Conflicts of a merge of the overlay stay listed while the file has markers.
        if let Some(prev) = orepo.p.db.state(&l.state)? {
            let root = orepo.p.state_path(&st.id);
            for c in &prev.conflicts {
                if (c.kind == "content" || c.kind == "add/add") && crate::repo::conflict_open(&root, c)? {
                    st.conflicts.push(c.clone());
                }
            }
        }
        made.push((i, st, ol));
    }
    let head = repo.state(&line.head)?;
    let shared = if s.shared.is_empty() && !(allow_empty && made.is_empty()) {
        None
    } else {
        match next_state(repo, ctx, &line.head, &new_tree.path, &s.shared, own, vec![line.head.clone()], message, Some(&line.id))
        {
            Ok(mut st) => {
                if let Some((n, e)) = &author {
                    st.author.name = n.clone();
                    st.author.email = e.clone();
                }
                // Content conflicts stay listed while the committed file has markers.
                let root = repo.p.state_path(&st.id);
                for c in &head.conflicts {
                    if (c.kind == "content" || c.kind == "add/add") && crate::repo::conflict_open(&root, c)? {
                        st.conflicts.push(c.clone());
                    }
                }
                Some(st)
            }
            Err(e) => {
                discard(&made, &orepos);
                return Err(e);
            }
        }
    };
    // The overlays first: a lost main record leaves an overlay line ahead of the stack, which the
    // next commit reports, never a stack that names a missing state.
    let mut new_layers = layers.to_vec();
    let mut overlays = Vec::new();
    for (i, st, ol) in made {
        let orepo = &orepos[i];
        let _lock = orepo.p.lock()?;
        move_overlay_line(orepo, ctx, "commit", &ol, &st.id, vec![st.clone()])?;
        new_layers[i].state = st.id.clone();
        overlays.push((layers[i].overlay.clone(), st));
    }
    let new_head = shared.as_ref().map(|s| s.id.clone()).unwrap_or(line.head.clone());
    let data = serde_json::json!({
        "line": line.id,
        "stack": stack_data(&line.id, &new_layers),
        "overlays": overlays.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
    });
    let states: Vec<StateRec> = shared.iter().cloned().collect();
    repo.move_head(ctx, "commit", line, &new_head, states, None, data)?;
    let fds = if summary { crate::diff::file_diffs(&base, &new_tree.path, &s.list)? } else { Vec::new() };
    repo.install_base(line, new_tree, &new_head, &new_layers)?;
    Ok(Some(Committed { shared, overlays, fds }))
}

/// Change the stack of a line to `layers` and its working folder with it: overlay files come and
/// go, local changes stay (a change in the way refuses, unless `force`). The caller holds the
/// project lock and owns the line.
pub fn set_layers(ctx: &Ctx, repo: &Repo, line: &Line, layers: &[Layer], force: bool, op: &str) -> Result<Vec<Change>> {
    let old = repo.head_tree(line)?;
    let (st, _scan, _it, _idx) = repo.status(line)?;
    let view = repo.assemble(line, &line.head, layers)?;
    let wdir = repo.working(line);
    // An ignored file of the user (a key of its own) is a local file too: never overwritten.
    let mut local = st.local_paths();
    local.extend(st.ignored.iter().cloned());
    let applied = repo.apply_delta(&old, &view.path, &wdir, repo.own(line), &local, force)?;
    let mut rec = Record::new(op);
    rec.data = serde_json::json!({"line": line.id, "stack": stack_data(&line.id, layers)});
    repo.commit_record(ctx, rec)?;
    repo.drop_stage(line)?;
    repo.install_base(line, view, &line.head, layers)?;
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_base_key_names_the_head_and_the_stack() {
        let l = |s: &str| vec![Layer { overlay: "keys".into(), state: s.into() }];
        assert_eq!(base_key("h", &l("a")), base_key("h", &l("a")));
        assert_ne!(base_key("h", &l("a")), base_key("h", &l("b")));
        assert_ne!(base_key("h", &l("a")), base_key("g", &l("a")));
        assert_eq!(base_key("h", &l("a")).len(), 16);
        assert_eq!(Mode::parse("follow"), Some(Mode::Follow));
        assert_eq!(Mode::parse("shared").map(|m| m.name()), Some("shared"));
        assert_eq!(Mode::parse("ephemeral"), None);
    }

    #[test]
    fn ops_move_delete_and_copy() {
        let root = std::env::temp_dir().join(format!("layr-ops-{}", std::process::id()));
        let (src, dst) = (root.join("src"), root.join("dst"));
        let _ = std::fs::remove_dir_all(&root);
        for d in [&src, &dst] {
            std::fs::create_dir_all(d.join("a/b")).unwrap();
        }
        std::fs::write(dst.join("a/b/old.txt"), "moved").unwrap();
        std::fs::write(dst.join("a/b/gone.txt"), "x").unwrap();
        std::fs::write(dst.join("keep.txt"), "keep").unwrap();
        std::fs::write(src.join("new.txt"), "new").unwrap();
        std::fs::create_dir_all(src.join("c")).unwrap();
        std::fs::write(src.join("c/moved.txt"), "moved and changed").unwrap();
        let ops = Ops {
            moves: vec![("a/b/old.txt".into(), "c/moved.txt".into())],
            dels: ["a/b/gone.txt".to_string()].into(),
            puts: ["new.txt".to_string(), "c/moved.txt".to_string()].into(),
        };
        ops.apply(&dst, &src, None).unwrap();
        assert_eq!(std::fs::read_to_string(dst.join("c/moved.txt")).unwrap(), "moved and changed");
        assert_eq!(std::fs::read_to_string(dst.join("new.txt")).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(dst.join("keep.txt")).unwrap(), "keep");
        assert!(!dst.join("a").exists(), "empty folders go, as in git trees");
        let _ = std::fs::remove_dir_all(&root);
    }
}
