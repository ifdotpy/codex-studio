//! Repository operations on one project: snapshots, lines, staging, status and in-place updates.

use crate::access::{Policy, Role, Rule, Who};
use crate::btrfs;
use crate::changes::{self, Change};
use crate::ctx::Ctx;
use crate::db::need;
use crate::fsutil;
use crate::ids;
use crate::ignore_rules::Rules;
use crate::model::{Conflict, Line, LineChange, LineState, Record, StateKind, StateRec};
use crate::store::{check_name, Project};
use crate::tree::Tree;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub struct Repo<'a> {
    pub p: Project<'a>,
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

#[derive(Default)]
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

pub fn skip_path(p: &str) -> bool {
    p == ".git" || p.starts_with(".git/")
}

/// The kinds of change to a line, for `Repo::authorize_line`.
#[derive(Clone, Copy, Debug)]
pub enum LineOp {
    /// Snapshots of the working content (save, layer repair): the owner.
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
                if reader_action
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
        if pol.denied_by(&who, "read").is_none() && self.p.db.lines()?.iter().any(|l| l.owner == ctx.caller.uid) {
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

    /// The user that the service's own backups are written as: the one who set `backup.dir`.
    pub fn backup_writer(&self) -> (u32, u32) {
        let uid = self.p.config_u64("backup.writer", self.p.legacy_admin() as u64) as u32;
        (uid, crate::ctx::Caller::from_uid(uid).gid)
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

    /// Path of a state snapshot on this machine.
    pub fn state_root(&self, id: &str) -> Result<PathBuf> {
        let p = self.p.state_path(id);
        if !p.exists() {
            match self.p.db.state_flags(id)? {
                Some((_, true)) => bail!("state {} was deleted by retention", ids::short(id)),
                Some(_) => bail!("state {} is not on this machine (run layr sync)", ids::short(id)),
                None => bail!("unknown state {}", ids::short(id)),
            }
        }
        Ok(p)
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
        let info = btrfs::subvol_info(&dst)?;
        Ok(StateRec {
            id,
            kind,
            parents,
            line: line.map(|l| l.id.clone()),
            subvol: info.uuid,
            author: ctx.author(),
            message: message.to_string(),
            time: ids::now_ms(),
            running: false,
            conflicts: Vec::new(),
            layers: BTreeMap::new(),
            layer_by: None,
            layer_shared: false,
        })
    }

    /// Remove state snapshots that were made for a record that was not written.
    pub fn discard_states(&self, states: &[StateRec]) {
        for s in states {
            let _ = btrfs::delete_tree(&self.p.state_path(&s.id));
        }
    }

    /// Write a record. On failure, the states it names are deleted (no snapshot without record).
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
        Ok(self.p.db.line_states(&line.id, StateKind::Auto)?.into_iter().find(|s| self.p.state_path(&s.id).exists()))
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
        let mut st = self.new_state(ctx, &w, StateKind::Auto, vec![line.head.clone()], message, Some(line))?;
        st.running = running;
        if !force {
            let prev = self.last_auto(line)?.map(|s| s.id).unwrap_or(line.head.clone());
            let base = self.p.state_path(&prev);
            if base.exists() {
                let changed = changes::candidates(&base, &self.p.state_path(&st.id))?.into_iter().any(|c| !skip_path(&c.path));
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

    /// The staging line, created from the head state if missing or based on an older head.
    pub fn ensure_stage(&self, line: &Line) -> Result<PathBuf> {
        let p = self.p.stage_path(&line.id);
        let basef = self.stage_base_file(line);
        if p.exists() {
            let base = std::fs::read_to_string(&basef).unwrap_or_default();
            if base.trim() == line.head {
                return Ok(p);
            }
            btrfs::delete_tree(&p)?;
        }
        btrfs::snapshot(&self.state_root(&line.head)?, &p, false)?;
        crate::fsutil::atomic_write(&basef, line.head.as_bytes(), 0o600)?;
        Ok(p)
    }

    /// The staging line if it was made from the current head (a stale one is dropped).
    pub fn current_stage(&self, line: &Line) -> Result<Option<PathBuf>> {
        match self.stage(line) {
            Some(s) => {
                let base = std::fs::read_to_string(self.stage_base_file(line)).unwrap_or_default();
                if base.trim() == line.head {
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

    /// The staging content as a read-only snapshot, or the head state when nothing is staged.
    pub fn index_view(&self, line: &Line) -> Result<(PathBuf, Option<Temp>)> {
        match self.stage(line) {
            Some(s) => {
                let base = std::fs::read_to_string(self.stage_base_file(line)).unwrap_or_default();
                if base.trim() != line.head {
                    self.drop_stage(line)?;
                    return Ok((self.state_root(&line.head)?, None));
                }
                let t = self.temp_snapshot(&s, true)?;
                Ok((t.path.clone(), Some(t)))
            }
            None => Ok((self.state_root(&line.head)?, None)),
        }
    }

    // ----- status -----

    /// Changes of the working content: staged (head to index), unstaged (index to working),
    /// untracked and ignored paths, and conflicts left by a merge.
    pub fn status_of(&self, line: &Line, scan: &Path, index: &Path) -> Result<Status> {
        let head = self.state_root(&line.head)?;
        let mut st = Status::default();
        if index != head.as_path() {
            let staged: Vec<Change> = changes::file_changes(&head, index)?.into_iter().filter(|c| !skip_path(&c.path)).collect();
            st.staged = changes::with_renames(&head, index, staged);
        }
        let rules = Rules::new(scan);
        let mut untracked_files = Vec::new();
        for c in changes::without_moves(changes::file_changes(index, scan)?) {
            if skip_path(&c.path) {
                continue;
            }
            if c.old.is_none() {
                if rules.ignored(&c.path, false) {
                    st.ignored.push(c.path.clone());
                } else {
                    untracked_files.push(c.path.clone());
                }
                continue;
            }
            st.unstaged.push(c);
        }
        // Untracked folders: the topmost folder that does not exist in the index.
        let mut dirs: BTreeSet<String> = BTreeSet::new();
        for f in &untracked_files {
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
        st.untracked = untracked_files;
        let head_rec = self.state(&line.head)?;
        for c in &head_rec.conflicts {
            if conflict_open(scan, c)? {
                st.conflicts.push(c.clone());
            }
        }
        Ok(st)
    }

    pub fn status(&self, line: &Line) -> Result<(Status, Temp, Option<Temp>, PathBuf)> {
        let scan = self.scan(line)?;
        let (index, itemp) = self.index_view(line)?;
        let st = self.status_of(line, &scan.path, &index)?;
        Ok((st, scan, itemp, index))
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
                        "error: Your local changes to the following files would be overwritten:\n{}\nPlease commit your changes or stash them.",
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
        check_name("line", name)?;
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
        self.materialize(&line, start, &dst)?;
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
            self.p.append(rec, &ctx.actor())
        })();
        if let Err(e) = res {
            let _ = btrfs::delete_tree(&dst);
            return Err(e);
        }
        // The owner of the new line may read the git objects (its .git layer uses them).
        crate::gitbridge::sync_store_access(self)?;
        Ok(line)
    }

    /// Make the working folder of a line at `dst` from a state. The folder is prepared in the
    /// service's private work folder (owner, mode, git layer, layers) and appears at `dst` only
    /// when it is ready, so no other user can change it while root works on it.
    pub fn materialize(&self, line: &Line, state: &str, dst: &Path) -> Result<()> {
        if dst.exists() {
            bail!("{} already exists", dst.display());
        }
        let tmp = self.p.work_dir().join(ids::new_id());
        btrfs::snapshot(&self.state_root(state)?, &tmp, false)?;
        let r = self.prepare_at(line, state, &tmp).and_then(|_| Ok(std::fs::rename(&tmp, dst)?));
        if r.is_err() {
            let _ = btrfs::delete_tree(&tmp);
        }
        r
    }

    fn prepare_at(&self, line: &Line, state: &str, w: &Path) -> Result<()> {
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
        fsutil::chown_nofollow(w, uid, gid)?;
        std::fs::set_permissions(w, std::fs::Permissions::from_mode(0o700))?;
        crate::gitbridge::make_git_layer(self, line, state, w)?;
        let st = self.state(state)?;
        // Layers of this machine's setting, and the layers the state carries (a live move).
        let mut paths: Vec<String> = self.p.config_strings("regenerable");
        for k in st.layers.keys() {
            if !paths.contains(k) {
                paths.push(k.clone());
            }
        }
        for path in paths {
            let lp = fsutil::safe_join(w, &path)?;
            if let Some(src) = self.warm_layer(state, &path, uid)? {
                if lp.symlink_metadata().is_ok() {
                    fsutil::remove_entry(w, &path)?;
                }
                fsutil::make_parents(w, &path, Some((uid, gid)))?;
                btrfs::snapshot(&src, &lp, false)?;
                // The copy shares its blocks; only its files change owner.
                give_tree(&lp, uid, gid)?;
                continue;
            }
            make_layer(w, &path, uid, gid)?;
        }
        Ok(())
    }

    /// The layer snapshot that a line of `uid` starting at `state` takes for `path`: the state's
    /// own, or else the one of its nearest ancestor that has one on this machine (the build
    /// output closest to the line's content). A layer made by another user is taken only if it
    /// was made on a protected line (`layer_shared`): a layer holds code that review does not see.
    pub fn warm_layer(&self, state: &str, path: &str, uid: u32) -> Result<Option<PathBuf>> {
        const SEARCH: usize = 200;
        let local = self.p.store.machine.id.clone();
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
            if let Some(snap) = rec.layers.get(path) {
                let src = self.p.layer_path(&id, snap);
                let owner_of_line = rec.line.as_ref().and_then(|l| self.p.db.line_by_id(l).ok().flatten()).map(|l| l.owner);
                let allowed = rec.layer_shared || rec.layer_by == Some(uid) || owner_of_line == Some(uid);
                // Another state's layer only from this machine; the state's own also after a move.
                if src.exists() && allowed && (id == state || self.p.db.state_machine(&id)?.as_deref() == Some(local.as_str())) {
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
        if let Err(e) = crate::gitbridge::refresh_git_layer(self, &moved) {
            eprintln!("warning: .git layer of line {} not updated: {e:#}", line.name);
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
        let st = self.new_state(ctx, &w, StateKind::Auto, vec![line.head.clone()], &format!("after {op}"), Some(line))?;
        let id = st.id.clone();
        rec.states.push(st);
        Ok(Some(id))
    }

    // ----- ancestry -----

    pub fn parents(&self, id: &str) -> Result<Vec<String>> {
        Ok(self.state(id)?.parents)
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

/// True if a conflict is still open in `root`. A content conflict is open while the file holds
/// conflict markers (a merge writes markers only into files of at most `MAX_TEXT` bytes, so a
/// larger file has none). Other kinds (moves, deletes, types, binary files) stay open until the
/// next commit of the line, which is the admin's decision.
pub fn conflict_open(root: &Path, c: &Conflict) -> Result<bool> {
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

/// Give every entry of a tree (a new layer copy, still private) to `uid:gid`, without following
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

/// Create an empty nested subvolume for a regenerable layer at `path` in the working folder.
pub fn make_layer(w: &Path, path: &str, uid: u32, gid: u32) -> Result<()> {
    let t = Tree::open(w)?;
    if let Some(m) = t.stat(path) {
        if m.is_dir() && m.ino == 256 {
            return Ok(());
        }
        if m.is_dir() && t.list(path)?.is_empty() {
            t.rmdir(path);
        } else {
            return Ok(());
        }
    }
    t.mkdirs_for(path, Some((uid, gid)))?;
    t.create_subvolume(path).with_context(|| format!("create layer {path}"))?;
    t.chown(path, uid, gid)?;
    Ok(())
}

/// Copy a folder within one tree (both sides may change under us: all through descriptors).
fn copy_tree(t: &Tree, from: &str, to: &str, own: Option<(u32, u32)>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for name in t.list(from)? {
        let (s, d) = (format!("{from}/{name}"), format!("{to}/{name}"));
        let m = match t.stat(&s) {
            Some(m) => m,
            None => continue,
        };
        match m.ftype {
            fsutil::FType::Dir => {
                t.mkdirs(&d, own)?;
                t.chmod(&d, m.mode)?;
                copy_tree(t, &s, &d, own)?;
            }
            fsutil::FType::Symlink => {
                let target = t.read_link(&s)?;
                t.symlink(&d, &target)?;
                if let Some((u, g)) = own {
                    t.chown(&d, u, g)?;
                }
            }
            fsutil::FType::File => {
                let src = t.open_read(&s)?;
                let dst = t.create(&d, m.mode)?;
                if btrfs::clone_file(&src, &dst).is_err() {
                    std::io::copy(&mut &src, &mut &dst)?;
                }
                dst.set_permissions(std::fs::Permissions::from_mode(m.mode))?;
                fsutil::set_mtime(&dst, m.mtime, 0)?;
                if let Some((u, g)) = own {
                    t.chown(&d, u, g)?;
                }
            }
            fsutil::FType::Special => {}
        }
    }
    Ok(())
}

/// Turn a plain folder at a layer path back into a nested subvolume (after a tool deleted the
/// layer and made the folder again). Copies with reflinks: O(files) once.
pub fn convert_layer(w: &Path, path: &str, uid: u32, gid: u32) -> Result<bool> {
    let t = Tree::open(w)?;
    let m = match t.stat(path) {
        Some(m) => m,
        None => {
            make_layer(w, path, uid, gid)?;
            return Ok(true);
        }
    };
    if !m.is_dir() || m.ino == 256 {
        return Ok(false);
    }
    let (tmp, old) = (format!("{path}.layr-new"), format!("{path}.layr-old"));
    for x in [&tmp, &old] {
        if t.stat(x).is_some() {
            t.remove(x)?;
        }
    }
    t.create_subvolume(&tmp)?;
    copy_tree(&t, path, &tmp, Some((uid, gid)))?;
    t.rename(path, &old)?;
    t.rename(&tmp, path)?;
    t.chown(path, uid, gid)?;
    t.remove(&old)?;
    Ok(true)
}
