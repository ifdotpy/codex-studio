//! Repository operations on one project: snapshots, lines, staging, status and in-place updates.

use crate::access::{Policy, Role, Rule, Who};
use crate::changes::{self, Change};
use crate::ctx::Ctx;
use crate::db::need;
use crate::fsutil;
#[cfg(target_os = "macos")]
use crate::fsutil::Src;
use crate::ids;
use crate::ignore_rules::Rules;
use crate::model::{Conflict, Line, LineChange, LineState, Record, StateKind, StateRec};
use crate::storage as btrfs;
use crate::store::{check_name, Project};
use crate::tree::Tree;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// A tree that hot paths read: on APFS the layers of a state, elsewhere its folder.
#[cfg(target_os = "macos")]
pub type Snap = crate::thin::Layered;
#[cfg(not(target_os = "macos"))]
pub type Snap = PathBuf;

pub struct Repo<'a> {
    pub p: Project<'a>,
}

/// A complete folder as a tree to read.
#[cfg(target_os = "macos")]
pub fn snap(p: &Path) -> Snap {
    crate::thin::Layered::complete(p)
}
#[cfg(not(target_os = "macos"))]
pub fn snap(p: &Path) -> Snap {
    p.to_path_buf()
}

/// True when two trees are the same tree (not only equal content).
#[cfg(target_os = "macos")]
pub fn same_tree(a: &Snap, b: &Snap) -> bool {
    a.top() == b.top()
}
#[cfg(not(target_os = "macos"))]
pub fn same_tree(a: &Snap, b: &Snap) -> bool {
    a == b
}

/// A temporary read-only or writable snapshot, deleted on drop.
pub struct Temp {
    pub path: PathBuf,
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = btrfs::delete_tree(&self.path);
    }
}

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct Status {
    pub staged: Vec<Change>,
    pub unstaged: Vec<Change>,
    pub untracked: Vec<String>,
    pub untracked_dirs: Vec<String>,
    pub ignored: Vec<String>,
    pub conflicts: Vec<Conflict>,
}

impl Status {
    /// Paths with local changes that an in-place update must not overwrite.
    pub fn local_paths(&self) -> BTreeSet<String> {
        let mut s = BTreeSet::new();
        for c in self.unstaged.iter().chain(self.staged.iter()) {
            s.insert(c.path.clone());
            if let Some(o) = &c.old_path {
                s.insert(o.clone());
            }
        }
        for u in &self.untracked {
            s.insert(u.clone());
        }
        s
    }
    pub fn clean(&self) -> bool {
        self.staged.is_empty() && self.unstaged.is_empty()
    }
}

/// Whether a cache saved on `saved` (None: by an older layr) may warm a line on the platform
/// `local`. `here`: the cache was saved on this machine.
pub fn cache_fits(saved: Option<&str>, local: &str, here: bool) -> bool {
    match saved {
        Some(p) => p == local,
        None => here,
    }
}

pub fn skip_path(p: &str) -> bool {
    p == ".git" || p.starts_with(".git/")
}

/// The kinds of change to a line, for `Repo::authorize_line`.
#[derive(Clone, Copy, Debug)]
pub enum LineOp {
    /// Snapshots of the working content (save, cache repair): the owner.
    Save,
    /// Commit, reset, add, apply, stash and the other work commands: the owner, and only if the
    /// line is not protected or its rule allows direct changes.
    Direct,
    /// A merge into the line: the owner; for a protected line the rule's update role.
    Merge { expect: bool },
    /// Delete, rename or adopt: the owner, or a maintainer.
    Manage,
    /// Moving the head back: the owner; for a protected line the rule's rewrite role.
    Rewrite,
}

impl<'a> Repo<'a> {
    pub fn open(ctx: &'a Ctx, project: &str) -> Result<Repo<'a>> {
        let p = Project::open(&ctx.store, project)?;
        let r = Repo { p };
        r.can_read(ctx)?;
        Ok(r)
    }

    // ----- permissions (docs/design.md, section 3; src/access.rs) -----

    pub fn policy(&self) -> Policy {
        Policy::load(&self.p)
    }

    pub fn who(ctx: &Ctx) -> Who {
        Who::of(ctx.caller.uid, ctx.caller.gid)
    }

    /// The caller may take a project action (`access::ACTIONS`), or an error that says why not.
    /// Owning a line includes reading the project: the reader actions (`read`, `try`, `export`)
    /// are open to line owners too, unless a deny rule takes them away.
    pub fn require(&self, ctx: &Ctx, action: &str) -> Result<()> {
        if ctx.caller.is_root() {
            return Ok(());
        }
        let pol = self.policy();
        let who = Self::who(ctx);
        match pol.check(&who, action) {
            Ok(()) => Ok(()),
            Err(reason) => {
                let reader_action = crate::access::action_role(action) == Some(Role::Reader);
                // An overlay is read by its readers only: a former reader keeps its overlay lines.
                if reader_action
                    && !self.p.is_overlay()
                    && pol.denied_by(&who, action).is_none()
                    && self.p.db.lines()?.iter().any(|l| l.owner == ctx.caller.uid)
                {
                    return Ok(());
                }
                Err(anyhow::anyhow!("permission denied: {reason}"))
            }
        }
    }

    /// The caller has the admin role (root has).
    pub fn is_admin(&self, ctx: &Ctx) -> bool {
        ctx.caller.is_root() || self.policy().role(&Self::who(ctx)) == Role::Admin
    }

    /// Reading needs the reader role, or owning a line of the project (unless a deny rule takes
    /// `read` away).
    pub fn can_read(&self, ctx: &Ctx) -> Result<()> {
        if ctx.caller.is_root() {
            return Ok(());
        }
        let pol = self.policy();
        let who = Self::who(ctx);
        if pol.check(&who, "read").is_ok() {
            return Ok(());
        }
        if !self.p.is_overlay()
            && pol.denied_by(&who, "read").is_none()
            && self.p.db.lines()?.iter().any(|l| l.owner == ctx.caller.uid)
        {
            return Ok(());
        }
        bail!("permission denied: user {} may not read project {}", ctx.caller.user, self.p.name)
    }

    /// Whether the caller may make this kind of change to a line. Returns the line's protection
    /// rule. Only the owner changes a line; a protected line changes through merges by the role
    /// its rule names; moving a protected line back needs the rule's rewrite role.
    pub fn authorize_line(&self, ctx: &Ctx, line: &Line, op: LineOp) -> Result<Option<Rule>> {
        let pol = self.policy();
        let rule = pol.rule_for(&line.name);
        if ctx.caller.is_root() {
            return Ok(rule);
        }
        let name = &line.name;
        if line.owner == 0 {
            bail!("permission denied: only root changes the root-owned line '{name}'");
        }
        let who = Self::who(ctx);
        let role = pol.role(&who);
        let owner = ctx.caller.uid == line.owner;
        let not_owner = || anyhow::anyhow!("permission denied: line '{name}' belongs to another user");
        match op {
            LineOp::Save => {
                if !owner {
                    return Err(not_owner());
                }
            }
            LineOp::Direct => {
                if rule.as_ref().map(|r| !r.direct).unwrap_or(false) {
                    bail!("line '{name}' is protected: it changes only through 'layr merge <line> --expect <state>'");
                }
                if !owner {
                    return Err(not_owner());
                }
            }
            LineOp::Merge { expect } => match &rule {
                None if !owner => return Err(not_owner()),
                None => {}
                Some(r) => {
                    pol.check(&who, "merge").map_err(|e| anyhow::anyhow!("permission denied: {e}"))?;
                    if role < r.update {
                        bail!(
                            "permission denied: merging into the protected line '{name}' needs the role {}; you are {}",
                            r.update.name(),
                            role.name()
                        );
                    }
                    if r.expect && !expect {
                        bail!("the protected line '{name}' takes only a reviewed state: add --expect <state>");
                    }
                }
            },
            LineOp::Manage => {
                if !(owner && rule.is_none()) {
                    self.require(ctx, "line.manage")?;
                }
                if let Some(r) = &rule {
                    if role < r.rewrite {
                        bail!(
                            "permission denied: this change of the protected line '{name}' needs the role {}",
                            r.rewrite.name()
                        );
                    }
                }
            }
            LineOp::Rewrite => match &rule {
                None if !owner => return Err(not_owner()),
                None => {}
                Some(r) if role < r.rewrite => {
                    bail!(
                        "permission denied: moving the protected line '{name}' back needs the role {}; you are {}",
                        r.rewrite.name(),
                        role.name()
                    );
                }
                Some(_) => {}
            },
        }
        Ok(rule)
    }

    /// A move of a line's head that is not forward (reset, amend, undo of a merge) rewrites the
    /// line's history.
    pub fn check_move(&self, ctx: &Ctx, line: &Line, new_head: &str) -> Result<()> {
        if new_head == line.head || self.is_ancestor(&line.head, new_head)? {
            return Ok(());
        }
        self.authorize_line(ctx, line, LineOp::Rewrite).map(|_| ())
    }

    // ----- lookups -----

    pub fn line(&self, name: &str) -> Result<Line> {
        if let Some(l) = self.p.db.line_by_name(name)? {
            return self.localize(l);
        }
        if let Some(l) = self.p.db.line_by_id(name)? {
            return self.localize(l);
        }
        bail!("line '{name}' not found")
    }

    /// A line continued on several machines has one head per machine. On a machine with a
    /// working folder of the line, its own head is the head; the others are `<line>@<machine>`.
    pub fn localize(&self, mut l: Line) -> Result<Line> {
        let me = &self.p.store.machine.id;
        if l.machine == *me {
            return Ok(l);
        }
        if let Some((_, h)) = self.p.db.line_heads(&l.id)?.into_iter().find(|(m, _)| m == me) {
            if self.p.line_path(&l.name).exists() {
                l.head = h;
                l.machine = me.clone();
            }
        }
        Ok(l)
    }

    pub fn lines(&self) -> Result<Vec<Line>> {
        self.p.db.lines()?.into_iter().map(|l| self.localize(l)).collect()
    }

    pub fn state(&self, id: &str) -> Result<StateRec> {
        need(self.p.db.state(id)?, &format!("state {}", ids::short(id)))
    }

    /// Source history cannot contain cache or derived states.
    pub fn source_state(&self, id: &str) -> Result<StateRec> {
        let state = self.state(id)?;
        if state.kind.auxiliary() {
            bail!(
                "state {} is {}; cache and derived states are outside source history, diff, merge and Git export",
                ids::short(id),
                crate::db::kind_str(state.kind)
            );
        }
        Ok(state)
    }

    /// The complete tree of a state. On APFS a thin state gets a view (`thin::complete_tree`);
    /// commands that read few paths use `state_src` instead.
    pub fn state_root(&self, id: &str) -> Result<PathBuf> {
        self.check_state(id)?;
        #[cfg(target_os = "macos")]
        return crate::thin::complete_tree(&self.p, id);
        #[cfg(not(target_os = "macos"))]
        Ok(self.p.state_path(id))
    }

    fn check_state(&self, id: &str) -> Result<()> {
        if !self.p.has_state(id) {
            match self.p.db.state_flags(id)? {
                Some((_, true)) => bail!("state {} was deleted by retention", ids::short(id)),
                Some(_) => bail!("state {} is not on this machine (run layr fetch <remote>)", ids::short(id)),
                None => bail!("unknown state {}", ids::short(id)),
            }
        }
        Ok(())
    }

    /// A state to read without a complete copy: on APFS its layers, elsewhere its folder.
    pub fn state_src(&self, id: &str) -> Result<Snap> {
        self.check_state(id)?;
        #[cfg(target_os = "macos")]
        return crate::thin::Layered::of_state(&self.p, id);
        #[cfg(not(target_os = "macos"))]
        Ok(self.p.state_path(id))
    }

    /// The head of a line to read (`state_src`). A line with overlays reads its base.
    pub fn head_src(&self, line: &Line) -> Result<Snap> {
        let layers = if self.p.is_overlay() { Vec::new() } else { self.layers(line)? };
        if layers.is_empty() {
            return self.state_src(&line.head);
        }
        let base = self.head_tree(line)?;
        #[cfg(target_os = "macos")]
        return Ok(crate::thin::Layered::complete(&base));
        #[cfg(not(target_os = "macos"))]
        Ok(base)
    }

    /// A complete folder of a tree. A layered tree gets a temporary read-only copy in `keep`.
    pub fn complete(&self, tree: Snap, keep: &mut Vec<Temp>) -> Result<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            if tree.is_complete() {
                return Ok(tree.bottom().to_path_buf());
            }
            let t = Temp { path: self.p.scan_dir().join(ids::new_id()) };
            tree.materialize(&t.path, true)?;
            btrfs::write_info(
                &t.path,
                &btrfs::SubvolInfo { uuid: ids::new_id(), received_uuid: None, parent_uuid: None, readonly: true, base: None },
            )?;
            btrfs::seal(&t.path, true, false)?;
            let p = t.path.clone();
            keep.push(t);
            Ok(p)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = keep;
            Ok(tree)
        }
    }

    /// Copy a state to `dst`: a writable folder, or a read-only snapshot.
    pub fn snapshot_state(&self, id: &str, dst: &Path, readonly: bool) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let src = self.state_src(id)?;
            if src.is_complete() {
                return btrfs::snapshot(src.bottom(), dst, readonly);
            }
            let parent = btrfs::subvol_info(src.top())?.uuid;
            src.materialize(dst, readonly)?;
            let marked = (|| -> Result<()> {
                let info = btrfs::SubvolInfo {
                    uuid: ids::new_id(),
                    received_uuid: None,
                    parent_uuid: Some(parent),
                    readonly,
                    base: None,
                };
                btrfs::write_info(dst, &info)?;
                if readonly {
                    btrfs::seal(dst, true, false)?;
                }
                Ok(())
            })();
            if marked.is_err() {
                let _ = btrfs::delete_tree(dst);
            }
            marked
        }
        #[cfg(not(target_os = "macos"))]
        btrfs::snapshot(&self.state_root(id)?, dst, readonly)
    }

    pub fn working(&self, line: &Line) -> PathBuf {
        self.p.line_path(&line.name)
    }

    pub fn owner_ids(&self, line: &Line) -> (u32, u32) {
        let c = crate::ctx::Caller::from_uid(line.owner);
        (line.owner, c.gid)
    }

    pub fn own(&self, line: &Line) -> Option<(u32, u32)> {
        Some(self.owner_ids(line))
    }

    pub fn line_state(&self, line: &Line, head: &str) -> LineState {
        LineState { name: line.name.clone(), head: head.to_string(), owner: line.owner, machine: line.machine.clone() }
    }

    // ----- snapshots -----

    /// Snapshot `src` into a new state folder (read-only). The record is not written yet.
    pub fn new_state(
        &self,
        ctx: &Ctx,
        src: &Path,
        kind: StateKind,
        parents: Vec<String>,
        message: &str,
        line: Option<&Line>,
    ) -> Result<StateRec> {
        let id = ids::new_id();
        let dst = self.p.state_path(&id);
        btrfs::snapshot(src, &dst, true)?;
        self.describe_state(ctx, id, kind, parents, message, line)
    }

    /// A new state with the content of `base`: on APFS an empty thin layer over it, elsewhere a
    /// snapshot. The record is not written yet.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn same_state(
        &self,
        ctx: &Ctx,
        base: &str,
        kind: StateKind,
        parents: Vec<String>,
        message: &str,
        line: Option<&Line>,
    ) -> Result<StateRec> {
        // Overlay projects read their states as folders: they keep complete trees.
        #[cfg(target_os = "macos")]
        if !self.p.is_overlay() {
            let id = ids::new_id();
            let src = self.state_src(base)?;
            let dst = self.p.delta_path(&id);
            crate::thin::write_layer(&src.at("")?, &dst, &crate::thin::Diff::default(), base, false, None)?;
            return self.describe_state(ctx, id, kind, parents, message, line);
        }
        self.new_state(ctx, &self.state_root(base)?, kind, parents, message, line)
    }

    /// Describe an already prepared immutable tree. No record is written here.
    pub fn describe_state(
        &self,
        ctx: &Ctx,
        id: String,
        kind: StateKind,
        parents: Vec<String>,
        message: &str,
        line: Option<&Line>,
    ) -> Result<StateRec> {
        let mut dst = self.p.state_path(&id);
        if !dst.exists() && self.p.delta_path(&id).exists() {
            dst = self.p.delta_path(&id);
        }
        let info = btrfs::subvol_info(&dst)?;
        Ok(StateRec {
            id,
            kind,
            change_id: (kind == StateKind::Commit).then(ids::new_id),
            derived_from: if kind.auxiliary() { parents.first().cloned() } else { None },
            context: None,
            parents: if kind.auxiliary() { Vec::new() } else { parents },
            line: if kind.auxiliary() { None } else { line.map(|l| l.id.clone()) },
            subvol: info.uuid,
            author: ctx.author(),
            message: message.to_string(),
            time: ids::now_ms(),
            running: false,
            conflicts: Vec::new(),
            caches: BTreeMap::new(),
            cache_by: None,
            cache_shared: false,
            cache_platform: None,
        })
    }

    /// An automatic state of a line's working content. For a line with overlays the snapshot
    /// leaves the overlay files out (a writable snapshot, the files removed, read-only): no state
    /// of the project holds a file of an overlay.
    pub fn working_snapshot(&self, ctx: &Ctx, line: &Line, parents: Vec<String>, message: &str) -> Result<StateRec> {
        let w = self.working(line);
        let private = self.private_paths(line)?;
        #[cfg(target_os = "macos")]
        if private.is_empty() && self.thin_line(line)? {
            // A thin layer over the head: only the entries that changed are copied.
            let id = ids::new_id();
            let head = self.state_src(&line.head)?;
            let dst = self.p.delta_path(&id);
            match crate::thin::snapshot_over(&head, &line.head, &w, &dst, false) {
                Ok(Some(_)) => return self.describe_state(ctx, id, StateKind::Auto, parents, message, Some(line)),
                Ok(None) => {
                    crate::thin::write_layer(&head.at("")?, &dst, &crate::thin::Diff::default(), &line.head, false, None)?;
                    return self.describe_state(ctx, id, StateKind::Auto, parents, message, Some(line));
                }
                // Hard links need a complete tree.
                Err(e) if crate::thin::needs_complete(&e) => {}
                Err(e) => return Err(e),
            }
        }
        if private.is_empty() {
            return self.new_state(ctx, &w, StateKind::Auto, parents, message, Some(line));
        }
        let id = ids::new_id();
        let dst = self.p.state_path(&id);
        btrfs::snapshot(&w, &dst, false)?;
        let made = (|| -> Result<btrfs::SubvolInfo> {
            let t = Tree::open(&dst)?;
            for p in &private {
                if t.stat(p).is_some() {
                    t.remove(p)?;
                }
            }
            drop(t);
            btrfs::set_readonly(&dst)?;
            // An overlay file under another name leaves too: moved from an overlay path, or a
            // new file with the content of an overlay file (a copy, an editor's extra copy).
            let copies = self.private_copies(line, &dst, &private)?;
            if !copies.is_empty() {
                btrfs::set_writable(&dst)?;
                let t = Tree::open(&dst)?;
                for p in &copies {
                    t.remove(p)?;
                }
                drop(t);
                btrfs::set_readonly(&dst)?;
            }
            btrfs::subvol_info(&dst)
        })();
        let info = match made {
            Ok(i) => i,
            Err(e) => {
                let _ = btrfs::delete_tree(&dst);
                return Err(e);
            }
        };
        Ok(StateRec {
            id,
            kind: StateKind::Auto,
            change_id: None,
            derived_from: None,
            context: None,
            parents,
            line: Some(line.id.clone()),
            subvol: info.uuid,
            author: ctx.author(),
            message: message.to_string(),
            time: ids::now_ms(),
            running: false,
            conflicts: Vec::new(),
            caches: BTreeMap::new(),
            cache_by: None,
            cache_shared: false,
            cache_platform: None,
        })
    }

    /// True when the states of a line can be thin layers: APFS, no overlays.
    #[cfg(target_os = "macos")]
    fn thin_line(&self, line: &Line) -> Result<bool> {
        Ok(!self.p.is_overlay() && self.layers(line)?.is_empty())
    }

    /// Files of a snapshot of the working folder that hold an overlay file under another name:
    /// moved from a path in `private`, or new with the same content as one of them.
    fn private_copies(&self, line: &Line, snap: &Path, private: &BTreeSet<String>) -> Result<Vec<String>> {
        let base = self.head_tree(line)?;
        let mut by_size: std::collections::HashMap<u64, Vec<&String>> = std::collections::HashMap::new();
        for p in private {
            if let Some(m) = fsutil::lmeta(&base, p).filter(|m| !m.is_dir()) {
                by_size.entry(m.size).or_default().push(p);
            }
        }
        let mut out = Vec::new();
        for c in changes::file_changes(&base, snap)? {
            let n = match &c.new {
                Some(n) => n,
                None => continue,
            };
            if c.old_path.as_ref().map(|o| private.contains(o)).unwrap_or(false) {
                out.push(c.path.clone());
                continue;
            }
            if c.old.is_none() {
                for p in by_size.get(&n.size).into_iter().flatten() {
                    if fsutil::same_entry(&base, p, snap, &c.path, None)? {
                        out.push(c.path.clone());
                        break;
                    }
                }
            }
        }
        Ok(out)
    }

    /// Remove state snapshots that were made for a record that was not written.
    pub fn discard_states(&self, states: &[StateRec]) {
        for s in states {
            #[cfg(target_os = "macos")]
            if crate::apfs_commit::preserves(&self.p, &s.id).unwrap_or(true) {
                // The journal owns rollback of the only staged copy.
                continue;
            }
            let _ = self.p.delete_state(&s.id);
        }
    }

    /// Read authenticated records already accepted into the local database.
    pub fn read_model(&self) -> Result<layr_model::Project> {
        let records = self
            .p
            .db
            .all_lines()?
            .iter()
            .map(|line| crate::oplog::parse_line(line, None).map(|s| s.record))
            .collect::<Result<Vec<_>>>()?;
        layr_model::Project::from_records(&self.p.id, records, &self.p.store.machine.id)
    }

    /// Append a completed named transfer through the normal signed record path.
    pub fn record_transfer(&self, ctx: &Ctx, op: &str, remote: &str, url: &str, transfer: &crate::model::Transfer) -> Result<()> {
        let _lock = self.p.lock()?;
        let mut rec = Record::new(op);
        let mut transfer = transfer.clone();
        if !self.p.is_overlay() {
            transfer.overlays = transfer
                .overlays
                .into_iter()
                .filter(|name| self.overlay_info(name).ok().flatten().is_some_and(|info| !info.hidden))
                .collect();
        }
        rec.data = serde_json::to_value(transfer)?;
        rec.data["remote"] = serde_json::json!(remote);
        rec.data["url"] = serde_json::json!(layr_model::operations::public_url(url));
        rec.data["result"] = serde_json::json!("success");
        if let Some((_, overlay)) = self.p.name.split_once('%') {
            rec.data["overlay"] = serde_json::json!(overlay);
        }
        self.commit_record(ctx, rec)?;
        Ok(())
    }

    /// Write a record. On failure, delete the states it names.
    pub fn commit_record(&self, ctx: &Ctx, rec: Record) -> Result<Record> {
        let states = rec.states.clone();
        match self.p.append(rec, &ctx.actor()) {
            Ok(r) => Ok(r),
            Err(e) => {
                // The record and its rows are one transaction: nothing was recorded.
                self.discard_states(&states);
                Err(e)
            }
        }
    }

    pub fn temp_snapshot(&self, src: &Path, readonly: bool) -> Result<Temp> {
        let dst = self.p.scan_dir().join(ids::new_id());
        btrfs::snapshot(src, &dst, readonly)?;
        Ok(Temp { path: dst })
    }

    pub fn temp_work(&self, src: &Path) -> Result<Temp> {
        let dst = self.p.work_dir().join(ids::new_id());
        btrfs::snapshot(src, &dst, false)?;
        Ok(Temp { path: dst })
    }

    /// Read-only snapshot of a line's working content.
    pub fn scan(&self, line: &Line) -> Result<Temp> {
        let w = self.working(line);
        if !w.exists() {
            bail!("line '{}' has no working folder on this machine", line.name);
        }
        self.temp_snapshot(&w, true)
    }

    /// The newest automatic state of a line, if any.
    pub fn last_auto(&self, line: &Line) -> Result<Option<StateRec>> {
        Ok(self.p.db.line_states(&line.id, StateKind::Auto)?.into_iter().find(|s| self.p.has_state(&s.id)))
    }

    /// The id of a state equal to the working content: a new automatic state, or the previous
    /// automatic state (or head) when nothing changed. The caller holds the project lock.
    pub fn working_state(&self, ctx: &Ctx, line: &Line, message: &str) -> Result<Option<String>> {
        if !self.working(line).exists() {
            return Ok(None);
        }
        Ok(match self.save(ctx, line, message, false, false)? {
            Some(s) => Some(s.id),
            None => Some(self.last_auto(line)?.map(|s| s.id).unwrap_or(line.head.clone())),
        })
    }

    /// Save the working content as an automatic state if it changed since the previous one.
    /// The caller holds the project lock.
    pub fn save(&self, ctx: &Ctx, line: &Line, message: &str, running: bool, force: bool) -> Result<Option<StateRec>> {
        let w = self.working(line);
        if !w.exists() {
            return Ok(None);
        }
        #[cfg(target_os = "macos")]
        if !force && self.private_paths(line)?.is_empty() && self.thin_line(line)? {
            // Compare before any copy: an unchanged folder makes no state. One scan of the
            // working folder serves the comparison and the layer over the head.
            let prev = self.last_auto(line)?.map(|s| s.id).unwrap_or(line.head.clone());
            let new = btrfs::scan_with_data(&w, true)?;
            let head = self.state_src(&line.head)?;
            let head_scan = head.scan(true)?;
            let head_diff = crate::thin::diff(&head_scan, &|k| head.path(k), &new, &w)?;
            let relevant = |d: &crate::thin::Diff| d.write.iter().chain(d.removed.iter()).any(|p| !skip_path(p));
            let changed = if prev == line.head {
                relevant(&head_diff)
            } else if self.p.has_state(&prev) {
                let base = self.state_src(&prev)?;
                relevant(&crate::thin::diff(&base.scan(true)?, &|k| base.path(k), &new, &w)?)
            } else {
                true
            };
            if !changed {
                return Ok(None);
            }
            let id = ids::new_id();
            let dst = self.p.delta_path(&id);
            let mut st = match crate::thin::write_checked(&w, &head, &head_scan, &new, &head_diff, &dst, &line.head, false) {
                Ok(()) => self.describe_state(ctx, id, StateKind::Auto, vec![line.head.clone()], message, Some(line))?,
                // Hard links need a complete tree.
                Err(e) if crate::thin::needs_complete(&e) => {
                    self.new_state(ctx, &w, StateKind::Auto, vec![line.head.clone()], message, Some(line))?
                }
                Err(e) => return Err(e),
            };
            st.running = running;
            let mut rec = Record::new("save");
            rec.states.push(st.clone());
            rec.data = serde_json::json!({"line": line.id});
            self.commit_record(ctx, rec)?;
            return Ok(Some(st));
        }
        let mut st = self.working_snapshot(ctx, line, vec![line.head.clone()], message)?;
        st.running = running;
        if !force {
            let prev = self.last_auto(line)?.map(|s| s.id).unwrap_or(line.head.clone());
            if self.p.has_state(&prev) {
                let base = self.state_src(&prev)?;
                let changed = changes::candidates(&base, &self.state_src(&st.id)?)?.into_iter().any(|c| !skip_path(&c.path));
                if !changed {
                    self.discard_states(std::slice::from_ref(&st));
                    return Ok(None);
                }
            }
        }
        let mut rec = Record::new("save");
        rec.states.push(st.clone());
        rec.data = serde_json::json!({"line": line.id});
        self.commit_record(ctx, rec)?;
        Ok(Some(st))
    }

    /// The state that `save --caches` gives a line's caches to: on a protected line with no
    /// changes since its head, the head, so the next lines made from that head start warm; else a
    /// new automatic state. Only a protected line gives caches to a state that other lines share,
    /// so such a state never holds a cache that only its maker may use.
    pub fn caches_state(&self, ctx: &Ctx, line: &Line, message: &str, running: bool, caches: &[String]) -> Result<StateRec> {
        let w = self.working(line);
        if !w.exists() {
            return Err(crate::cmd::exit(1, format!("error: the line '{}' has no working folder", line.name)));
        }
        let mut st = self.working_snapshot(ctx, line, vec![line.head.clone()], message)?;
        if let Some(head) = self.p.db.state(&line.head)? {
            if self.policy().rule_for(&line.name).is_some() && self.p.has_state(&head.id) {
                let base = self.state_src(&head.id)?;
                // Changes as git would see them: a cache is an empty folder in a snapshot, a
                // merge leaves new owners and times on unchanged files, and ignored files (such
                // as other build output) are not changes.
                let in_cache = |path: &str| caches.iter().any(|c| path == c || path.starts_with(&format!("{c}/")));
                let made = self.state_src(&st.id)?;
                let rules = Rules::of(&made);
                let changed = changes::file_changes(&base, &made)?
                    .into_iter()
                    .any(|c| !skip_path(&c.path) && !in_cache(&c.path) && !rules.ignored(&c.path, false));
                if !changed {
                    self.discard_states(std::slice::from_ref(&st));
                    return Ok(head);
                }
            }
        }
        st.running = running;
        let mut rec = Record::new("save");
        rec.states.push(st.clone());
        rec.data = serde_json::json!({"line": line.id});
        self.commit_record(ctx, rec)?;
        Ok(st)
    }

    // ----- staging -----

    pub fn stage(&self, line: &Line) -> Option<PathBuf> {
        let p = self.p.stage_path(&line.id);
        if p.exists() {
            Some(p)
        } else {
            None
        }
    }

    fn stage_base_file(&self, line: &Line) -> PathBuf {
        self.p.meta_dir().join("stage").join(format!("{}.base", line.id))
    }

    /// The staging line, created from the head (the full tree of a line with overlays) if
    /// missing or based on an older head or stack. On APFS it is a thin layer over the head
    /// when the line has no overlays; `ensure_full_stage` gives commands a complete tree.
    pub fn ensure_stage(&self, line: &Line) -> Result<PathBuf> {
        let p = self.p.stage_path(&line.id);
        let basef = self.stage_base_file(line);
        let key = self.stage_key(line)?;
        if p.exists() {
            let base = std::fs::read_to_string(&basef).unwrap_or_default();
            if base.trim() == key {
                return Ok(p);
            }
            self.drop_stage(line)?;
        }
        #[cfg(target_os = "macos")]
        let thin = !self.p.is_overlay() && self.layers(line)?.is_empty();
        #[cfg(not(target_os = "macos"))]
        let thin = false;
        if thin {
            #[cfg(target_os = "macos")]
            {
                let head = self.head_src(line)?;
                crate::thin::empty_layer(&p, &line.head, &head.at("")?)?;
            }
        } else {
            btrfs::snapshot(&self.head_tree(line)?, &p, false)?;
        }
        crate::fsutil::atomic_write(&basef, key.as_bytes(), 0o600)?;
        Ok(p)
    }

    /// The staging line as a complete writable tree, for commands that change it in ways a
    /// thin layer does not support (a patch, a move, a removal, a restore).
    pub fn ensure_full_stage(&self, line: &Line) -> Result<PathBuf> {
        let p = self.ensure_stage(line)?;
        #[cfg(target_os = "macos")]
        if crate::thin::is_thin(&p) {
            let tree = self.stage_tree(line, &p)?;
            let tmp = self.p.work_dir().join(ids::new_id());
            tree.materialize(&tmp, false)?;
            let done = (|| -> Result<()> {
                let info = btrfs::SubvolInfo {
                    uuid: ids::new_id(),
                    received_uuid: None,
                    parent_uuid: None,
                    readonly: false,
                    base: None,
                };
                btrfs::write_info(&tmp, &info)?;
                // One atomic swap: a stop at any point leaves one complete index in place.
                crate::sys::apfs_swap(&tmp, &p)?;
                btrfs::delete_tree(&tmp)
            })();
            if done.is_err() {
                let _ = btrfs::delete_tree(&tmp);
            }
            done?;
        }
        Ok(p)
    }

    /// The tree of a staging line: a thin layer over the head, or a complete tree.
    #[cfg(target_os = "macos")]
    fn stage_tree(&self, line: &Line, stage: &Path) -> Result<Snap> {
        if crate::thin::is_thin(stage) {
            crate::thin::Layered::over(stage, self.head_src(line)?)
        } else {
            Ok(crate::thin::Layered::complete(stage))
        }
    }
    #[cfg(not(target_os = "macos"))]
    #[allow(dead_code)]
    fn stage_tree(&self, _line: &Line, stage: &Path) -> Result<Snap> {
        Ok(stage.to_path_buf())
    }

    /// Write the key of a staging line made for `line` (a soft reset keeps the index for a new head).
    pub fn mark_stage(&self, line: &Line) -> Result<()> {
        crate::fsutil::atomic_write(&self.stage_base_file(line), self.stage_key(line)?.as_bytes(), 0o600)
    }

    /// The staging line if it was made from the current head (a stale one is dropped).
    pub fn current_stage(&self, line: &Line) -> Result<Option<PathBuf>> {
        match self.stage(line) {
            Some(s) => {
                let base = std::fs::read_to_string(self.stage_base_file(line)).unwrap_or_default();
                if base.trim() == self.stage_key(line)? {
                    Ok(Some(s))
                } else {
                    self.drop_stage(line)?;
                    Ok(None)
                }
            }
            None => Ok(None),
        }
    }

    pub fn drop_stage(&self, line: &Line) -> Result<()> {
        let p = self.p.stage_path(&line.id);
        if p.exists() {
            btrfs::delete_tree(&p)?;
        }
        let _ = std::fs::remove_file(self.stage_base_file(line));
        Ok(())
    }

    /// The index to read and the staging folder when there is one. The caller must hold the
    /// project lock while it reads a mutable index.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn index_source(&self, line: &Line) -> Result<(Snap, Option<PathBuf>)> {
        match self.current_stage(line)? {
            Some(s) => Ok((self.stage_tree(line, &s)?, Some(s))),
            None => Ok((self.head_src(line)?, None)),
        }
    }

    /// The staging content as a read-only snapshot, or the head state when nothing is staged.
    pub fn index_view(&self, line: &Line) -> Result<(Snap, Option<Temp>)> {
        match self.current_stage(line)? {
            Some(s) => {
                let t = self.temp_snapshot(&s, true)?;
                #[cfg(target_os = "macos")]
                let tree = if crate::thin::is_thin(&s) {
                    crate::thin::Layered::over(&t.path, self.head_src(line)?)?
                } else {
                    crate::thin::Layered::complete(&t.path)
                };
                #[cfg(not(target_os = "macos"))]
                let tree = t.path.clone();
                Ok((tree, Some(t)))
            }
            None => Ok((self.head_src(line)?, None)),
        }
    }

    // ----- status -----

    /// Changes of the working content: staged (head to index), unstaged (index to working),
    /// untracked and ignored paths, and conflicts left by a merge.
    pub fn status_of(&self, line: &Line, scan: &Path, index: &Snap) -> Result<Status> {
        self.status_of_inner(line, scan, index, true)
    }

    /// Add only needs paths that differ between the index and the working tree.
    #[cfg(target_os = "macos")]
    pub fn status_for_add(&self, line: &Line, scan: &Path, index: &Snap) -> Result<Status> {
        self.status_of_inner(line, scan, index, false)
    }

    fn status_of_inner(&self, line: &Line, scan: &Path, index: &Snap, full: bool) -> Result<Status> {
        let mut st = Status::default();
        if full {
            let head = self.head_src(line)?;
            if !same_tree(index, &head) {
                let staged: Vec<Change> =
                    changes::file_changes(&head, index)?.into_iter().filter(|c| !skip_path(&c.path)).collect();
                st.staged = changes::with_renames(&head, index, staged);
            }
        }
        classify(scan, changes::file_changes(index, scan)?, &mut st);
        self.finish_status(line, scan, index, full, &mut st)?;
        Ok(st)
    }

    /// Untracked folders and open conflicts of a status whose lists are complete.
    fn finish_status(&self, line: &Line, scan: &Path, index: &Snap, full: bool, st: &mut Status) -> Result<()> {
        // Untracked folders: the topmost folder that does not exist in the index.
        let mut dirs: BTreeSet<String> = BTreeSet::new();
        for f in &st.untracked {
            let comps: Vec<&str> = f.split('/').collect();
            let mut top = None;
            for i in 1..comps.len() {
                let d = comps[..i].join("/");
                if fsutil::lmeta(index, &d).is_none() {
                    top = Some(d);
                    break;
                }
            }
            if let Some(d) = top {
                dirs.insert(d);
            }
        }
        st.untracked_dirs = dirs.into_iter().collect();
        st.conflicts.clear();
        if !full {
            return Ok(());
        }
        let head_rec = self.state(&line.head)?;
        for c in &head_rec.conflicts {
            if conflict_open(scan, c)? {
                st.conflicts.push(c.clone());
            }
        }
        // Conflicts of a merge of the line's follow overlays.
        if !self.p.is_overlay() {
            for l in self.layers(line)? {
                if let Some(rec) = self.overlay_repo(&l.overlay).ok().and_then(|o| o.p.db.state(&l.state).ok().flatten()) {
                    for c in &rec.conflicts {
                        if conflict_open(scan, c)? {
                            st.conflicts.push(c.clone());
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn status(&self, line: &Line) -> Result<(Status, Temp, Option<Temp>, Snap)> {
        let scan = self.scan(line)?;
        let (index, itemp) = self.index_view(line)?;
        let st = self.status_of(line, &scan.path, &index)?;
        Ok((st, scan, itemp, index))
    }

    /// A status report needs no retained snapshot. Call with the project lock held.
    /// Commands that use the returned file bytes must still call `status`.
    #[cfg(target_os = "macos")]
    pub fn status_report(&self, line: &Line) -> Result<Status> {
        let work = self.working(line);
        let (index, stage) = self.index_source(line)?;
        // LAYR_NO_STATUS_CACHE=1 compares the whole folder every time (for comparisons).
        let key = match std::env::var_os("LAYR_NO_STATUS_CACHE") {
            Some(_) => None,
            None => self.status_key(line, &work, &stage)?,
        };
        // One scan of the working folder: it finds the changed paths for the saved status, and
        // it is the baseline of the next saved status.
        let now = match &key {
            Some(_) => Some(btrfs::scan_with_data(&work, true)?),
            None => None,
        };
        if let (Some(key), Some(now)) = (&key, &now) {
            match self.cached_status(line, &work, &index, key, now) {
                Ok(Some(st)) => return Ok(st),
                Ok(None) => {}
                Err(e) => eprintln!("warning: saved status failed, comparing the whole folder: {e:#}"),
            }
        }
        // States are immutable; the lock keeps the index still. Only the working folder moves.
        let mut roots = vec![work.as_path()];
        if let Some(s) = &stage {
            roots.push(s.as_path());
        }
        let st = match crate::storage::stable_read(&roots, || self.status_of(line, &work, &index))? {
            Some(st) => st,
            None => self.status(line)?.0,
        };
        if let (Some(key), Some(now)) = (key, now) {
            // `now` comes before the read: a change during the read differs from it next time.
            let _ = self.save_status_cache(line, &StatusCache::new(key, &now, st.clone()));
        }
        Ok(st)
    }

    #[cfg(target_os = "macos")]
    fn status_cache_path(&self, line: &Line) -> PathBuf {
        self.p.meta_dir().join("status").join(format!("{}.cache", line.id))
    }

    #[cfg(target_os = "macos")]
    fn save_status_cache(&self, line: &Line, cache: &StatusCache) -> Result<()> {
        // A lost cache costs only time: no fsync, but never a partial file.
        let path = self.status_cache_path(line);
        let dir = path.parent().context("status cache folder")?;
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".{}.tmp", ids::new_id()));
        std::fs::write(&tmp, cache.encode()?)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&tmp, &path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        Ok(())
    }

    /// What a saved status depends on besides the working folder: the head, the index layer,
    /// and the working folder itself. `None` when a saved status cannot be used: a line with
    /// overlays, or a complete index.
    #[cfg(target_os = "macos")]
    fn status_key(&self, line: &Line, work: &Path, stage: &Option<PathBuf>) -> Result<Option<String>> {
        use sha1::{Digest, Sha1};
        if self.p.is_overlay() || !self.layers(line)?.is_empty() {
            return Ok(None);
        }
        let mut h = Sha1::new();
        let w = std::fs::symlink_metadata(work)?;
        h.update(format!("{}|{}|{}|{}|", line.head, w.dev(), w.ino(), work.display()));
        if let Some(s) = stage {
            if !crate::thin::is_thin(s) {
                return Ok(None);
            }
            h.update(std::fs::read(self.stage_base_file(line)).unwrap_or_default());
            h.update(btrfs::subvol_info(s)?.uuid.as_bytes());
            h.update(serde_json::to_vec(&crate::thin::removed_of(s)?)?);
            for (k, e) in btrfs::scan_with_data(s, true)? {
                h.update(format!("{k}|{:?}|{:?}\n", e.fingerprint, e.data).as_bytes());
            }
        }
        Ok(Some(hex::encode(h.finalize())))
    }

    /// The saved status, brought up to date for the entries of the working folder whose
    /// metadata, change counter, or clone ID differ from the saved scan (as git's index does
    /// with stat data). `now` is a scan of the working folder made before this read. `None`
    /// when the saved status cannot answer: then the caller compares the whole folder.
    #[cfg(target_os = "macos")]
    fn cached_status(
        &self,
        line: &Line,
        work: &Path,
        index: &Snap,
        key: &str,
        now: &BTreeMap<String, btrfs::ScanEntry>,
    ) -> Result<Option<Status>> {
        let Ok(raw) = std::fs::read(self.status_cache_path(line)) else {
            return Ok(None);
        };
        let Some(cache) = StatusCache::decode(&raw) else {
            return Ok(None);
        };
        if cache.version != STATUS_CACHE_VERSION || cache.key != key {
            return Ok(None);
        }
        let _t = crate::trace::span("saved status");
        let paths = cache.changed(now);
        if paths.len() > STATUS_CACHE_MAX_PATHS {
            return Ok(None);
        }
        for p in &paths {
            let name = p.rsplit('/').next().unwrap_or("");
            if name == ".gitignore" || name == ".layrignore" {
                // Ignore rules decide the state of unchanged paths too.
                return Ok(None);
            }
        }
        if paths.is_empty() {
            let _used = crate::trace::span("saved status used for 0 paths");
            let mut st = cache.status;
            self.finish_status(line, work, index, true, &mut st)?;
            return Ok(Some(st));
        }
        let below = |p: &str| paths.contains(p) || p.match_indices('/').any(|(i, _)| paths.contains(&p[..i]));
        let w: BTreeMap<String, btrfs::ScanEntry> =
            now.iter().filter(|(k, _)| below(k)).map(|(k, v)| (k.clone(), v.clone())).collect();
        let list =
            changes::file_changes_of(index, work, btrfs::candidates_scanned(index, work, btrfs::scan_paths(index, &paths)?, w)?)?;
        // The content read must see the entries of the scan.
        let again = btrfs::scan_paths(work, &paths)?;
        if paths.iter().any(|p| again.get(p) != now.get(p)) {
            return Ok(None);
        }
        let mut st = cache.status;
        st.unstaged.retain(|c| !below(&c.path) && !c.old_path.as_deref().is_some_and(below));
        st.untracked.retain(|p| !below(p));
        st.ignored.retain(|p| !below(p));
        classify(work, list, &mut st);
        st.unstaged.sort_by(|a, b| a.path.cmp(&b.path));
        st.untracked.sort();
        st.ignored.sort();
        self.finish_status(line, work, index, true, &mut st)?;
        // A new baseline saves work when many paths changed. With few, the old baseline and
        // its status stay valid: the same paths are compared again next time.
        if paths.len() >= STATUS_CACHE_RESAVE {
            let _ = self.save_status_cache(line, &StatusCache::new(key.to_string(), now, st.clone()));
        }
        let _used = crate::trace::span(format!("saved status used for {} paths", paths.len()));
        Ok(Some(st))
    }

    // ----- in-place updates -----

    /// Change `target` (a working folder) from the content of `from` to the content of `to`,
    /// touching only the paths that differ. Paths in `local` hold local changes: if a change
    /// touches one, nothing is written and the paths are reported, unless `force`.
    pub fn apply_delta(
        &self,
        from: &Path,
        to: &Path,
        target: &Path,
        own: Option<(u32, u32)>,
        local: &BTreeSet<String>,
        force: bool,
    ) -> Result<Vec<Change>> {
        let list: Vec<Change> = changes::file_changes(from, to)?.into_iter().filter(|c| !skip_path(&c.path)).collect();
        if !force {
            let mut hit = BTreeSet::new();
            for c in &list {
                for p in [Some(c.path.as_str()), c.old_path.as_deref()].into_iter().flatten() {
                    if local.contains(p) {
                        hit.insert(p.to_string());
                    }
                    let pre = format!("{p}/");
                    for l in local.range(pre.clone()..) {
                        if !l.starts_with(&pre) {
                            break;
                        }
                        hit.insert(l.clone());
                    }
                }
            }
            if !hit.is_empty() {
                let names: Vec<String> = hit.into_iter().map(|p| format!("\t{p}")).collect();
                return Err(crate::cmd::exit(
                    1,
                    format!(
                        "error: Your local changes to the following files would be overwritten:\n{}\nPlease save your changes or park them.",
                        names.join("\n")
                    ),
                ));
            }
        }
        // Moves keep the inode; then deletes (a folder can replace a file); then writes.
        for c in &list {
            if let Some(src) = &c.old_path {
                let t = Tree::open(target)?;
                if t.stat(src).is_some() && t.stat(&c.path).is_none() {
                    t.mkdirs_for(&c.path, own)?;
                    t.rename(src, &c.path)?;
                } else {
                    fsutil::remove_entry(target, src)?;
                }
                prune_empty_parents(target, to, src)?;
            }
        }
        for c in &list {
            if c.old_path.is_some() {
                continue;
            } else if c.new.is_none() {
                fsutil::remove_entry(target, &c.path)?;
                prune_empty_parents(target, to, &c.path)?;
            }
        }
        for c in &list {
            if c.new.is_some() {
                fsutil::copy_entry(to, &c.path, target, &c.path, own)?;
            }
        }
        Ok(list)
    }

    // ----- lines -----

    /// Create a line from a state: a writable snapshot at `lines/<name>`.
    pub fn create_line(&self, ctx: &Ctx, name: &str, start: &str, owner: u32) -> Result<Line> {
        self.create_line_from(ctx, name, start, owner, None)
    }

    /// Create a line; a line made from another line (`source`) shows the overlays of the source
    /// that its owner reads (docs/overlays.md).
    pub fn create_line_from(&self, ctx: &Ctx, name: &str, start: &str, owner: u32, source: Option<&Line>) -> Result<Line> {
        check_name("line", name)?;
        self.source_state(start)?;
        if self.p.db.line_by_name(name)?.is_some() {
            bail!("a line named '{name}' already exists");
        }
        let dst = self.p.line_path(name);
        if dst.exists() {
            bail!("{} already exists", dst.display());
        }
        let line = Line {
            id: ids::new_id(),
            name: name.to_string(),
            head: start.to_string(),
            owner,
            machine: self.p.store.machine.id.clone(),
        };
        let layers = match source {
            Some(s) if !self.p.is_overlay() => self.inherit_layers(ctx, s, &line.id, owner)?,
            _ => Vec::new(),
        };
        self.materialize_with(&line, start, &dst, &layers, true)?;
        let res = (|| -> Result<Record> {
            let mut rec = Record::new("line.create");
            rec.lines.insert(
                line.id.clone(),
                LineChange {
                    before: None,
                    after: Some(self.line_state(&line, start)),
                    working_before: None,
                    working_after: None,
                },
            );
            rec.data = serde_json::json!({"name": name, "start": start});
            if !layers.is_empty() {
                if !self.public_layers(&layers)?.is_empty() {
                    rec.data["stack"] = self.public_stack_data(&line.id, &layers)?;
                }
            }
            self.p.append(rec, &ctx.actor())
        })();
        if let Err(e) = res {
            let _ = btrfs::delete_tree(&dst);
            let _ = self.prune_bases(&line, None, 0);
            return Err(e);
        }
        // The owner of the new line may read the git objects (its .git folder uses them).
        crate::gitbridge::sync_store_access(self)?;
        Ok(line)
    }

    /// Make the working folder of a line at `dst` from a state. The folder is prepared in the
    /// service's private work folder (owner, mode, .git folder, caches) and appears at `dst` only
    /// when it is ready, so no other user can change it while root works on it.
    pub fn materialize(&self, line: &Line, state: &str, dst: &Path) -> Result<()> {
        // Only available layers that the line owner reads enter its working folder.
        let layers = self.readable_layers(line)?;
        self.materialize_with(line, state, dst, &layers, false)
    }

    /// `materialize` with the overlay files of `layers`. With `base`, the new folder (made from
    /// the line's head) is also kept as the line's base.
    pub fn materialize_with(
        &self,
        line: &Line,
        state: &str,
        dst: &Path,
        layers: &[crate::overlay::Layer],
        base: bool,
    ) -> Result<()> {
        if dst.exists() {
            bail!("{} already exists", dst.display());
        }
        let tmp = self.p.work_dir().join(ids::new_id());
        self.snapshot_state(state, &tmp, false)?;
        let r = self
            .put_layers(&tmp, layers, self.own(line))
            .and_then(|_| self.prepare_at(line, state, &tmp))
            .and_then(|_| {
                if base && !layers.is_empty() {
                    let view = self.temp_snapshot(&tmp, true)?;
                    self.install_base(line, view, state, layers)?;
                }
                Ok(())
            })
            .and_then(|_| Ok(crate::fsutil::rename_tree(&tmp, dst)?));
        if r.is_err() {
            let _ = btrfs::delete_tree(&tmp);
        }
        r
    }

    pub(crate) fn prepare_at(&self, line: &Line, state: &str, w: &Path) -> Result<()> {
        let (uid, gid) = self.owner_ids(line);
        let root_meta = std::fs::symlink_metadata(w)?;
        if root_meta.uid() != uid {
            // Files of another owner: give the whole tree to the line owner.
            let mut paths = Vec::new();
            fsutil::walk(w, "", &mut |rel, m| {
                if m.uid != uid {
                    paths.push(rel.to_string());
                }
                Ok(true)
            })?;
            for p in paths {
                fsutil::chown_nofollow(&w.join(p), uid, gid)?;
            }
        }
        #[cfg(target_os = "macos")]
        crate::sys::apfs_clear_acl(w)?;
        fsutil::chown_nofollow(w, uid, gid)?;
        std::fs::set_permissions(w, std::fs::Permissions::from_mode(0o700))?;
        crate::gitbridge::make_git_folder(self, line, state, w)?;
        let st = self.state(state)?;
        // Caches of this machine's setting, and the caches the state carries (a live move).
        let mut paths: Vec<String> = self.p.config_strings("regenerable");
        for k in st.caches.keys() {
            if !paths.contains(k) {
                paths.push(k.clone());
            }
        }
        // These always get a cache, warm or empty. A folder that the line's `.layrattributes`
        // makes regenerable gets one only warm, from an ancestor that saved it.
        let fixed = paths.len();
        let (regenerable, ignored) = (Rules::attribute(w, "regenerable"), Rules::new(w));
        for k in self.saved_cache_paths(state)? {
            let parent = Path::new(&k).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            let parent_ok = parent.is_empty() || fsutil::lmeta(w, &parent).map(|m| m.is_dir()).unwrap_or(false);
            if !paths.contains(&k) && parent_ok && regenerable.ignored(&k, true) && ignored.ignored(&k, true) {
                paths.push(k);
            }
        }
        for (i, path) in paths.iter().enumerate() {
            // Versioned files win: a folder that holds files of the state comes from the state,
            // also when a record of an older layr lists it as a cache.
            if holds_files(w, path) {
                continue;
            }
            let cp = fsutil::safe_join(w, path)?;
            if let Some(src) = self.warm_cache(state, path, uid)? {
                if cp.symlink_metadata().is_ok() {
                    fsutil::remove_entry(w, path)?;
                }
                fsutil::make_parents(w, path, Some((uid, gid)))?;
                btrfs::snapshot(&src, &cp, false)?;
                // The copy shares its blocks; only its files change owner.
                give_tree(&cp, uid, gid)?;
                continue;
            }
            if i < fixed {
                make_cache(w, path, uid, gid)?;
            }
        }
        Ok(())
    }

    /// The cache paths of the machine setting and of the folders that `.layrattributes` files in
    /// the working folder `w` make regenerable.
    pub fn cache_paths(&self, w: &Path) -> Vec<String> {
        let mut paths = self.p.config_strings("regenerable");
        for p in regenerable_folders(w) {
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
        paths
    }

    /// The cache paths of a line (`cache_paths`) in two lists: the caches, and the paths whose
    /// folder holds files in the head of the line. Such a folder is versioned: never a cache.
    pub fn line_cache_paths(&self, line: &Line) -> Result<(Vec<String>, Vec<String>)> {
        let head = self.state_root(&line.head)?;
        Ok(self.cache_paths(&self.working(line)).into_iter().partition(|p| !holds_files(&head, p)))
    }

    /// The cache paths that `state` or its ancestors saved (the search of `warm_cache`).
    fn saved_cache_paths(&self, state: &str) -> Result<BTreeSet<String>> {
        const SEARCH: usize = 200;
        let (mut seen, mut found) = (BTreeSet::new(), BTreeSet::new());
        let mut q = std::collections::VecDeque::from([state.to_string()]);
        while let Some(id) = q.pop_front() {
            if seen.len() >= SEARCH {
                break;
            }
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(rec) = self.p.db.state(&id)? {
                found.extend(rec.caches.keys().cloned());
                q.extend(rec.parents);
            }
        }
        Ok(found)
    }

    /// The cache snapshot that a line of `uid` starting at `state` takes for `path`: the state's
    /// own, or else the one of its nearest ancestor that has one on this machine (the build
    /// output closest to the line's content). A cache made by another user is taken only if it
    /// was made on a protected line (`cache_shared`): a cache holds code that review does not see.
    /// A cache is taken only on the platform it was saved on (`model::cache_platform`); a cache
    /// of an older layr that does not name its platform only on the machine that saved it.
    pub fn warm_cache(&self, state: &str, path: &str, uid: u32) -> Result<Option<PathBuf>> {
        const SEARCH: usize = 200;
        let local = self.p.store.machine.id.clone();
        let platform = crate::model::cache_platform();
        let mut seen = BTreeSet::new();
        let mut q = std::collections::VecDeque::from([state.to_string()]);
        while let Some(id) = q.pop_front() {
            if seen.len() >= SEARCH {
                break;
            }
            if !seen.insert(id.clone()) {
                continue;
            }
            let rec = match self.p.db.state(&id)? {
                Some(r) => r,
                None => continue,
            };
            if let Some(snap) = rec.caches.get(path) {
                let src = self.p.cache_path(&id, snap);
                let owner_of_line = rec.line.as_ref().and_then(|l| self.p.db.line_by_id(l).ok().flatten()).map(|l| l.owner);
                let allowed = rec.cache_shared || rec.cache_by == Some(uid) || owner_of_line == Some(uid);
                // Another state's cache only from this machine; the state's own also after a move.
                let here = self.p.db.state_machine(&id)?.as_deref() == Some(local.as_str());
                if src.exists() && allowed && cache_fits(rec.cache_platform.as_deref(), &platform, here) && (id == state || here)
                {
                    return Ok(Some(src));
                }
            }
            q.extend(rec.parents);
        }
        Ok(None)
    }

    /// Record a new head for a line (and optionally a working state change).
    #[allow(clippy::too_many_arguments)]
    pub fn move_head(
        &self,
        ctx: &Ctx,
        op: &str,
        line: &Line,
        new_head: &str,
        states: Vec<StateRec>,
        working_before: Option<String>,
        data: serde_json::Value,
    ) -> Result<Record> {
        let mut rec = Record::new(op);
        rec.states = states;
        let working_after = match &working_before {
            Some(_) => self.after_state(ctx, line, op, &mut rec)?,
            None => None,
        };
        rec.lines.insert(
            line.id.clone(),
            LineChange {
                before: Some(self.line_state(line, &line.head)),
                after: Some(self.line_state(line, new_head)),
                working_before,
                working_after,
            },
        );
        rec.data = data;
        let r = self.commit_record(ctx, rec)?;
        let mut moved = line.clone();
        moved.head = new_head.to_string();
        if let Err(e) = crate::gitbridge::refresh_git_folder(self, &moved) {
            eprintln!("warning: .git folder of line {} not updated: {e:#}", line.name);
        }
        Ok(r)
    }

    /// A snapshot of the working content right after an operation, so undo can revert only the
    /// changes of that operation and keep later work.
    pub fn after_state(&self, ctx: &Ctx, line: &Line, op: &str, rec: &mut Record) -> Result<Option<String>> {
        let w = self.working(line);
        if !w.exists() {
            return Ok(None);
        }
        let st = self.working_snapshot(ctx, line, vec![line.head.clone()], &format!("after {op}"))?;
        let id = st.id.clone();
        rec.states.push(st);
        Ok(Some(id))
    }

    // ----- ancestry -----

    pub fn parents(&self, id: &str) -> Result<Vec<String>> {
        Ok(self.source_state(id)?.parents)
    }

    /// All ancestors of `id` (including itself), breadth first.
    pub fn ancestors(&self, id: &str) -> Result<Vec<String>> {
        let mut seen = BTreeSet::new();
        let mut order = Vec::new();
        let mut q = std::collections::VecDeque::from([id.to_string()]);
        while let Some(x) = q.pop_front() {
            if !seen.insert(x.clone()) {
                continue;
            }
            order.push(x.clone());
            if let Some(s) = self.p.db.state(&x)? {
                if s.kind.auxiliary() {
                    self.source_state(&x)?;
                }
                for p in s.parents {
                    q.push_back(p);
                }
            }
        }
        Ok(order)
    }

    pub fn is_ancestor(&self, a: &str, b: &str) -> Result<bool> {
        Ok(self.ancestors(b)?.iter().any(|x| x == a))
    }

    /// The best common ancestor: a common ancestor that is not an ancestor of another one.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>> {
        let aa = self.ancestors(a)?;
        let bs: BTreeSet<String> = self.ancestors(b)?.into_iter().collect();
        let common: Vec<String> = aa.into_iter().filter(|x| bs.contains(x)).collect();
        for c in &common {
            let mut best = true;
            for d in &common {
                if d != c && self.is_ancestor(c, d)? {
                    best = false;
                    break;
                }
            }
            if best {
                return Ok(Some(c.clone()));
            }
        }
        Ok(None)
    }

    /// Commits reachable from `id` by first parents.
    pub fn first_parent_chain(&self, id: &str, stop: Option<&str>) -> Result<Vec<String>> {
        let mut v = Vec::new();
        let mut cur = Some(id.to_string());
        while let Some(c) = cur {
            if Some(c.as_str()) == stop {
                break;
            }
            v.push(c.clone());
            cur = self.p.db.state(&c)?.and_then(|s| s.parents.first().cloned());
        }
        Ok(v)
    }
}

/// A saved status of a line and the scan of its working folder that it matches
/// (`Repo::status_report`). Each entry keeps its type, inode, mode, size, times, change counter,
/// and clone ID: a write changes the change counter, as with git's stat data.
#[cfg(target_os = "macos")]
struct StatusCache {
    version: u32,
    key: String,
    /// Path, then fingerprint and data identity of each entry, in path order.
    entries: Vec<(String, [i64; 10])>,
    status: Status,
}

#[cfg(target_os = "macos")]
#[derive(serde::Serialize, serde::Deserialize)]
struct StatusHead {
    version: u32,
    key: String,
    status: Status,
}

#[cfg(target_os = "macos")]
const STATUS_CACHE_VERSION: u32 = 2;
#[cfg(target_os = "macos")]
const STATUS_CACHE_MAGIC: &[u8] = b"layr status cache\n";

/// More changed paths than this compare the whole folder.
#[cfg(target_os = "macos")]
const STATUS_CACHE_MAX_PATHS: usize = 20_000;

/// From this many changed paths on, a saved status gets a new baseline.
#[cfg(target_os = "macos")]
const STATUS_CACHE_RESAVE: usize = 256;

#[cfg(target_os = "macos")]
fn entry_key(e: &btrfs::ScanEntry) -> [i64; 10] {
    let f = e.fingerprint;
    let d = e.data.unwrap_or_default();
    [
        f.0 as i64,
        f.1 as i64,
        f.2 as i64,
        f.3 as i64,
        f.4,
        f.5,
        f.6,
        f.7,
        d.generation.map(|g| g as i64).unwrap_or(-1),
        d.clone_id.map(|c| c as i64).unwrap_or(-1),
    ]
}

#[cfg(target_os = "macos")]
impl StatusCache {
    fn new(key: String, scan: &BTreeMap<String, btrfs::ScanEntry>, status: Status) -> Self {
        StatusCache {
            version: STATUS_CACHE_VERSION,
            key,
            entries: scan.iter().map(|(k, e)| (k.clone(), entry_key(e))).collect(),
            status,
        }
    }

    /// A header in JSON, then each entry as a path length, the path, and ten numbers.
    fn encode(&self) -> Result<Vec<u8>> {
        let head = serde_json::to_vec(&StatusHead { version: self.version, key: self.key.clone(), status: self.status.clone() })?;
        let mut out = Vec::with_capacity(head.len() + self.entries.len() * 120);
        out.extend_from_slice(STATUS_CACHE_MAGIC);
        out.extend_from_slice(&(head.len() as u64).to_le_bytes());
        out.extend_from_slice(&head);
        out.extend_from_slice(&(self.entries.len() as u64).to_le_bytes());
        for (k, v) in &self.entries {
            out.extend_from_slice(&(k.len() as u32).to_le_bytes());
            out.extend_from_slice(k.as_bytes());
            for n in v {
                out.extend_from_slice(&n.to_le_bytes());
            }
        }
        Ok(out)
    }

    fn decode(raw: &[u8]) -> Option<Self> {
        let mut r = raw.strip_prefix(STATUS_CACHE_MAGIC)?;
        let mut take = |n: usize| -> Option<&[u8]> {
            let (a, b) = (r.get(..n)?, r.get(n..)?);
            r = b;
            Some(a)
        };
        let head_len = u64::from_le_bytes(take(8)?.try_into().ok()?) as usize;
        let head: StatusHead = serde_json::from_slice(take(head_len)?).ok()?;
        let count = u64::from_le_bytes(take(8)?.try_into().ok()?) as usize;
        let mut entries = Vec::with_capacity(count.min(10_000_000));
        for _ in 0..count {
            let len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
            let path = String::from_utf8(take(len)?.to_vec()).ok()?;
            let mut v = [0i64; 10];
            for n in v.iter_mut() {
                *n = i64::from_le_bytes(take(8)?.try_into().ok()?);
            }
            entries.push((path, v));
        }
        Some(StatusCache { version: head.version, key: head.key, entries, status: head.status })
    }

    /// Paths whose entries differ from the saved scan, and files without a change counter (a
    /// file that a process maps can change without a visible sign). A folder that only changed
    /// its times is left out: each changed entry inside it is in the list itself.
    fn changed(&self, now: &BTreeMap<String, btrfs::ScanEntry>) -> BTreeSet<String> {
        let kind = |v: &[i64; 10]| v[2] & libc::S_IFMT as i64;
        const DIR: i64 = libc::S_IFDIR as i64;
        const FILE: i64 = libc::S_IFREG as i64;
        let mut out = BTreeSet::new();
        // Both lists are in path order: one walk over both.
        let mut old = self.entries.iter().peekable();
        for (k, e) in now {
            while let Some((ok, _)) = old.peek() {
                if ok.as_str() < k.as_str() {
                    out.insert(ok.clone());
                    old.next();
                } else {
                    break;
                }
            }
            let new = entry_key(e);
            let same = match old.peek() {
                Some((ok, ov)) if ok == k => {
                    let same = *ov == new || (kind(ov) == DIR && kind(&new) == DIR);
                    old.next();
                    same
                }
                _ => false,
            };
            let no_counter = kind(&new) == FILE && new[8] < 0;
            if !k.is_empty() && (!same || no_counter) {
                out.insert(k.clone());
            }
        }
        out.extend(old.map(|(k, _)| k.clone()));
        out.remove("");
        out
    }
}

/// Sort index-to-working changes into unstaged, untracked, and ignored paths.
fn classify(scan: &Path, list: Vec<Change>, st: &mut Status) {
    let rules = Rules::new(scan);
    for c in changes::without_moves(list) {
        if skip_path(&c.path) {
            continue;
        }
        if c.old.is_none() {
            if rules.ignored(&c.path, false) {
                st.ignored.push(c.path.clone());
            } else {
                st.untracked.push(c.path.clone());
            }
            continue;
        }
        st.unstaged.push(c);
    }
}

/// True if a conflict is still open in `root`. A content conflict is open while the file holds
/// conflict markers (a merge writes markers only into files of at most `MAX_TEXT` bytes, so a
/// larger file has none). Other kinds (moves, deletes, types, binary files) stay open until the
/// next commit of the line, which is the admin's decision.
pub fn conflict_open(root: &(impl fsutil::Src + ?Sized), c: &Conflict) -> Result<bool> {
    if c.kind == "content" || c.kind == "add/add" {
        return Ok(match fsutil::lmeta(root, &c.path) {
            Some(m) if m.is_file() && m.size <= fsutil::MAX_TEXT => has_markers(&fsutil::read_file(root, &c.path)?),
            _ => false,
        });
    }
    Ok(true)
}

pub fn has_markers(data: &[u8]) -> bool {
    let s = String::from_utf8_lossy(data);
    let mut a = false;
    let mut b = false;
    for l in s.lines() {
        if l.starts_with("<<<<<<< ") {
            a = true;
        } else if l.starts_with(">>>>>>> ") && a {
            b = true;
        }
    }
    a && b
}

/// Remove parent folders of `rel` in `target` that became empty and do not exist in `to`.
fn prune_empty_parents(target: &Path, to: &Path, rel: &str) -> Result<()> {
    let t = Tree::open(target)?;
    let mut comps: Vec<&str> = rel.split('/').collect();
    comps.pop();
    while !comps.is_empty() {
        let d = comps.join("/");
        if fsutil::lmeta(to, &d).is_some() || !t.rmdir(&d) {
            break;
        }
        comps.pop();
    }
    Ok(())
}

/// Give every entry of a tree (a new cache copy, still private) to `uid:gid`, without following
/// links and without entering nested subvolumes.
fn give_tree(root: &Path, uid: u32, gid: u32) -> Result<()> {
    let mut paths = Vec::new();
    fsutil::walk(root, "", &mut |rel, m| {
        if m.uid != uid || m.gid != gid {
            paths.push(rel.to_string());
        }
        Ok(true)
    })?;
    for p in paths {
        fsutil::chown_nofollow(&root.join(p), uid, gid)?;
    }
    fsutil::chown_nofollow(root, uid, gid)
}

/// The folders of the working folder `w` that `.layrattributes` files make `regenerable` and
/// that the ignore rules ignore. A regenerable folder that is not ignored is no cache: its files
/// are in history, and a cache must not hide them. The walk does not enter a regenerable folder or
/// another subvolume.
pub fn regenerable_folders(w: &Path) -> Vec<String> {
    let (regenerable, ignored) = (Rules::attribute(w, "regenerable"), Rules::new(w));
    // Through descriptors: the owner may change the folder while root reads it.
    let Ok(t) = Tree::open(w) else { return Vec::new() };
    let mut found = Vec::new();
    let mut stack = vec![String::new()];
    while let Some(dir) = stack.pop() {
        for name in t.list(&dir).unwrap_or_default() {
            let rel = if dir.is_empty() { name } else { format!("{dir}/{name}") };
            if rel == ".git" {
                continue;
            }
            match t.stat(&rel) {
                Some(m) if m.is_dir() => {
                    if regenerable.ignored(&rel, true) {
                        if ignored.ignored(&rel, true) {
                            found.push(rel);
                        }
                    } else if !fsutil::is_layer(w, &rel, &m) {
                        // Another subvolume (a cache of the machine setting) is not entered.
                        stack.push(rel);
                    }
                }
                _ => {}
            }
        }
    }
    found.sort();
    found
}

/// Create an empty nested subvolume for a regenerable cache at `path` in the working folder.
pub fn make_cache(w: &Path, path: &str, uid: u32, gid: u32) -> Result<()> {
    let t = Tree::open(w)?;
    if let Some(m) = t.stat(path) {
        if fsutil::is_layer(w, path, &m) {
            return Ok(());
        }
        if m.is_dir() && t.list(path)?.is_empty() {
            t.rmdir(path);
        } else {
            return Ok(());
        }
    }
    t.mkdirs_for(path, Some((uid, gid)))?;
    t.create_subvolume(path).with_context(|| format!("create cache {path}"))?;
    t.chown(path, uid, gid)?;
    Ok(())
}

/// Copy a folder within one tree (both sides may change under us: all through descriptors).
fn copy_tree(
    t: &Tree,
    from: &str,
    to: &str,
    own: Option<(u32, u32)>,
    linked: &mut std::collections::HashMap<(u64, u64), String>,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    for name in t.list(from)? {
        let (s, d) = (format!("{from}/{name}"), format!("{to}/{name}"));
        let m = t.stat_checked(&s)?;
        match m.ftype {
            fsutil::FType::Dir => {
                t.mkdir_private(&d, own)?;
                copy_tree(t, &s, &d, own, linked)?;
                t.copy_directory_metadata(&s, &d)?;
            }
            fsutil::FType::Symlink => {
                let target = t.read_link(&s)?;
                t.symlink(&d, &target)?;
                if let Some((u, g)) = own {
                    t.chown(&d, u, g)?;
                }
                t.copy_symlink_xattrs(&s, &d)?;
            }
            fsutil::FType::File => {
                let src = t.open_read(&s)?;
                let meta = src.metadata()?;
                let inode = (meta.dev(), meta.ino());
                if let Some(first) = linked.get(&inode) {
                    t.hard_link(first, &d)?;
                    continue;
                }
                #[cfg(target_os = "macos")]
                let cloned = t.clone_new_file(&src, &d)?;
                #[cfg(not(target_os = "macos"))]
                let cloned: Option<std::fs::File> = None;
                let (dst, cloned) = match cloned {
                    Some(dst) => (dst, true),
                    None => (t.create(&d, m.mode)?, false),
                };
                if !cloned && btrfs::clone_file(&src, &dst).is_err() {
                    std::io::copy(&mut &src, &mut &dst)?;
                }
                fsutil::copy_metadata(&src, &dst)?;
                if let Some((u, g)) = own {
                    t.chown(&d, u, g)?;
                }
                if meta.nlink() > 1 {
                    linked.insert(inode, d);
                }
            }
            fsutil::FType::Special => anyhow::bail!("cannot copy special cache entry {s}"),
        }
    }
    Ok(())
}

/// Turn a plain folder at a cache path back into a nested subvolume (after a tool deleted the
/// cache and made the folder again). Copies with reflinks: O(files) once.
pub fn convert_cache(w: &Path, path: &str, uid: u32, gid: u32) -> Result<bool> {
    let t = Tree::open(w)?;
    let m = match t.stat(path) {
        Some(m) => m,
        None => {
            make_cache(w, path, uid, gid)?;
            return Ok(true);
        }
    };
    if !m.is_dir() || fsutil::is_layer(w, path, &m) {
        return Ok(false);
    }
    replace_folder(&t, path, true, uid, gid)?;
    Ok(true)
}

/// Turn a cache (a nested subvolume) at `path` back into a plain folder with the same files, so
/// that states hold them again: the folder holds versioned files. Copies with reflinks.
pub fn uncache(w: &Path, path: &str, uid: u32, gid: u32) -> Result<bool> {
    let t = Tree::open(w)?;
    match t.stat(path) {
        Some(m) if fsutil::is_layer(w, path, &m) => {}
        _ => return Ok(false),
    }
    replace_folder(&t, path, false, uid, gid)?;
    Ok(true)
}

/// Replace the folder at `path` with a copy of its files in a new subvolume or plain folder.
fn replace_folder(t: &Tree, path: &str, subvolume: bool, uid: u32, gid: u32) -> Result<()> {
    let (tmp, old) = (format!("{path}.layr-new"), format!("{path}.layr-old"));
    for x in [&tmp, &old] {
        if t.stat(x).is_some() {
            t.remove(x)?;
        }
    }
    let copy = (|| {
        if subvolume {
            t.create_subvolume(&tmp)?;
            t.chmod(&tmp, 0o700)?;
        } else {
            t.mkdir_private(&tmp, None)?;
        }
        copy_tree(t, path, &tmp, Some((uid, gid)), &mut std::collections::HashMap::new())?;
        t.copy_directory_metadata(path, &tmp)
    })();
    if let Err(error) = copy {
        t.remove(&tmp).with_context(|| format!("remove incomplete cache {tmp} after {error:#}"))?;
        return Err(error);
    }
    t.rename(path, &old)?;
    t.rename(&tmp, path)?;
    t.chown(path, uid, gid)?;
    t.remove(&old)?;
    Ok(())
}

/// Whether the tree at `root` has a file, link or other entry that is not a folder at `path` or
/// below it (a folder of a state that holds versioned files). A path below such an entry counts;
/// an error counts too, so that a folder whose files cannot be read never becomes a cache.
pub fn holds_files(root: &Path, path: &str) -> bool {
    let start = match fsutil::safe_join(root, path) {
        Ok(p) => p,
        Err(_) => return true,
    };
    match std::fs::symlink_metadata(&start) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    }
    let mut stack = vec![start];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return true,
        };
        for e in entries {
            match e.and_then(|e| Ok((e.path(), e.file_type()?))) {
                Ok((p, t)) if t.is_dir() => stack.push(p),
                _ => return true,
            }
        }
    }
    false
}

#[cfg(test)]
mod cache_copy_tests {
    use super::*;
    use std::os::unix::{fs::PermissionsExt, net::UnixListener};

    #[test]
    fn incomplete_cache_directories_are_private() -> Result<()> {
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = Scratch(std::env::temp_dir().join(format!("layr-copy-{}", crate::ids::short(&crate::ids::new_id()))));
        std::fs::create_dir_all(root.0.join("source/private"))?;
        std::fs::set_permissions(root.0.join("source/private"), std::fs::Permissions::from_mode(0o700))?;
        std::fs::create_dir(root.0.join("destination"))?;
        let _socket = UnixListener::bind(root.0.join("source/private/socket"))?;
        let tree = Tree::open(&root.0)?;
        assert!(copy_tree(&tree, "source", "destination", None, &mut std::collections::HashMap::new()).is_err());
        assert_eq!(std::fs::metadata(root.0.join("destination/private"))?.permissions().mode() & 0o777, 0o700);
        Ok(())
    }
}

#[cfg(test)]
mod cache_platform_tests {
    use super::cache_fits;

    #[test]
    fn a_cache_warms_only_lines_of_its_platform() {
        // A Mac build in a Linux line: never, also after a move to this machine.
        assert!(!cache_fits(Some("macos-aarch64"), "linux-aarch64", false));
        assert!(!cache_fits(Some("macos-aarch64"), "linux-aarch64", true));
        assert!(!cache_fits(Some("linux-x86_64"), "linux-aarch64", false));
        assert!(cache_fits(Some("linux-aarch64"), "linux-aarch64", false));
        // An older record names no platform: only a cache saved on this machine.
        assert!(cache_fits(None, "linux-aarch64", true));
        assert!(!cache_fits(None, "linux-aarch64", false));
    }
    #[test]
    fn auxiliary_states_resolve_but_source_commands_refuse_them() {
        use super::*;
        use crate::ctx::Caller;
        use crate::model::{Actor, Author};
        use crate::store::{Project, Store};
        let dir = std::env::temp_dir().join(format!("layr-auxiliary-{}", ids::new_id()));
        let ctx = Ctx {
            store: Store::open(&dir).unwrap(),
            caller: Caller::from_uid(0),
            cwd: dir.clone(),
            env: std::collections::HashMap::from([("LAYR_PROJECT".into(), "p".into())]),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
        };
        let project_dir = ctx.store.projects_dir().join("p");
        std::fs::create_dir_all(project_dir.join("meta/db")).unwrap();
        std::fs::create_dir(project_dir.join("states")).unwrap();
        let id = ids::new_id();
        let p = Project {
            store: &ctx.store,
            name: "p".into(),
            dir: project_dir.clone(),
            id: id.clone(),
            db: crate::db::Db::open(&project_dir.join("meta/db/layr.sqlite")).unwrap(),
        };
        let mut rec = Record::new("project");
        rec.data = serde_json::json!({"id":id,"name":"p","config":{}});
        p.append(rec, &Actor { uid: 0, name: "root".into(), session: None }).unwrap();
        for kind in [StateKind::Cache, StateKind::Derived] {
            let id = ids::new_id();
            std::fs::create_dir(p.state_path(&id)).unwrap();
            let mut rec = Record::new("state");
            rec.states.push(StateRec {
                id: id.clone(),
                kind,
                change_id: None,
                derived_from: None,
                context: Some("test-v1".into()),
                parents: vec![],
                line: None,
                subvol: String::new(),
                author: Author::default(),
                message: String::new(),
                time: ids::now_ms(),
                running: false,
                conflicts: vec![],
                caches: Default::default(),
                cache_by: None,
                cache_shared: false,
                cache_platform: None,
            });
            p.append(rec, &ctx.actor()).unwrap();
            let repo = Repo::open(&ctx, "p").unwrap();
            assert!(repo.state_root(&id).is_ok());
            assert_eq!(crate::revs::resolve(&repo, None, &id).unwrap(), id);
            for args in [vec!["line", "new", "rejected", &id], vec!["diff", &id, &id], vec!["log", &id], vec!["show", &id]] {
                let args: Vec<String> = args.into_iter().map(String::from).collect();
                let error = crate::cmd::run(&ctx, &args).unwrap_err().to_string();
                assert!(error.contains("outside source history"), "{args:?}: {error}");
            }
            assert!(crate::gitbridge::export_history(&repo, &id).unwrap_err().to_string().contains("outside source history"));
            assert!(repo.merge_base(&id, &id).unwrap_err().to_string().contains("outside source history"));
            let args = vec!["ref".into(), "set".into(), format!("caches/{}", crate::db::kind_str(kind)), id.clone()];
            assert_eq!(crate::cmd::run(&ctx, &args).unwrap(), 0);
        }
        drop(p);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
