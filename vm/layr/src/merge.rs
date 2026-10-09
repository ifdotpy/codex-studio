//! The merge engine. Inputs are three read-only states (base, ours, theirs) and a writable
//! snapshot of ours that receives the result. Change lists come from `btrfs send --no-data`.
//!
//! A path changed on one side takes that side (a reflink copy). A path changed on both sides
//! uses the merge driver from `.gitattributes`/`.layrattributes`: `text` (three-way line merge,
//! `git merge-file`), `union`, `ours`, `theirs`, `binary` (conflict), `blocks` (byte ranges from
//! the change lists, for formats marked safe) or a configured command. A merge never stops on a
//! conflict: the result keeps markers or the ours version and lists each conflict.

use crate::changes::{self, Change};
use crate::fsutil::{self, FType, Meta};
use crate::model::Conflict;
use crate::repo::skip_path;
use anyhow::{bail, Context, Result};
use ignore::gitignore::GitignoreBuilder;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Input<'a> {
    pub base: &'a Path,
    pub ours: &'a Path,
    pub theirs: &'a Path,
    pub result: &'a Path,
    pub base_id: &'a str,
    pub ours_id: &'a str,
    pub theirs_id: &'a str,
    pub ours_label: &'a str,
    pub theirs_label: &'a str,
    /// Project configuration: `merge.driver.<name>` and `merge.verify.<name>` commands.
    pub config: &'a dyn Fn(&str) -> Option<String>,
    /// Configured drivers run as this user: the lead of this machine.
    pub run_as: Option<(u32, u32)>,
    /// The caller is root: a driver may then run as root.
    pub allow_root: bool,
}

#[derive(Default, Debug)]
pub struct Outcome {
    pub conflicts: Vec<Conflict>,
    pub messages: Vec<String>,
    pub changed: BTreeSet<String>,
}

struct AttrRule {
    matcher: ignore::gitignore::Gitignore,
    attrs: Vec<(String, Option<String>)>,
}

pub struct Attrs {
    rules: Vec<AttrRule>,
}

impl Attrs {
    pub fn load(root: &Path) -> Attrs {
        let mut rules = Vec::new();
        for name in [".gitattributes", ".layrattributes"] {
            let data = match fsutil::lmeta(root, name) {
                Some(m) if m.is_file() => fsutil::read_file(root, name).unwrap_or_default(),
                _ => continue,
            };
            for line in String::from_utf8_lossy(&data).lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let pat = match parts.next() {
                    Some(p) => p,
                    None => continue,
                };
                let mut b = GitignoreBuilder::new(root);
                if b.add_line(None, pat).is_err() {
                    continue;
                }
                let matcher = match b.build() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let attrs = parts
                    .map(|a| {
                        if let Some(x) = a.strip_prefix('-') {
                            (x.to_string(), Some("false".to_string()))
                        } else if let Some((k, v)) = a.split_once('=') {
                            (k.to_string(), Some(v.to_string()))
                        } else {
                            (a.to_string(), None)
                        }
                    })
                    .collect();
                rules.push(AttrRule { matcher, attrs });
            }
        }
        Attrs { rules }
    }

    /// The value of an attribute for a path: the last matching rule wins.
    pub fn get(&self, root: &Path, path: &str, key: &str) -> Option<String> {
        let mut out = None;
        for r in &self.rules {
            if r.matcher.matched(root.join(path), false).is_ignore() {
                for (k, v) in &r.attrs {
                    if k == key {
                        out = Some(v.clone().unwrap_or_else(|| "true".into()));
                    }
                    if k == "binary" && key == "merge" {
                        out = Some("binary".into());
                    }
                }
            }
        }
        out
    }
}

enum OursAt {
    Unchanged,
    Changed,
    Deleted,
    Moved(String),
}

struct Side {
    by_path: HashMap<String, Change>,
    moved_from: HashMap<String, String>,
}

fn side(base: &Path, child: &Path) -> Result<(Side, Vec<Change>)> {
    let list: Vec<Change> = changes::candidates(base, child)?.into_iter().filter(|c| !skip_path(&c.path)).collect();
    let mut by_path = HashMap::new();
    let mut moved_from = HashMap::new();
    for c in &list {
        if let Some(o) = &c.old_path {
            moved_from.insert(o.clone(), c.path.clone());
        }
        by_path.insert(c.path.clone(), c.clone());
    }
    Ok((Side { by_path, moved_from }, list))
}

impl Side {
    fn at(&self, p: &str) -> OursAt {
        if let Some(q) = self.moved_from.get(p) {
            return OursAt::Moved(q.clone());
        }
        match self.by_path.get(p) {
            Some(c) if c.new.is_none() => OursAt::Deleted,
            Some(c) if c.old_path.is_none() => OursAt::Changed,
            _ => OursAt::Unchanged,
        }
    }
    fn ranges(&self, p: &str) -> Option<Vec<(u64, u64)>> {
        match self.by_path.get(p) {
            Some(c) if c.old_path.is_none() => c.ranges.clone(),
            Some(_) => None,
            None => Some(Vec::new()),
        }
    }
}

struct Ctx<'a, 'b> {
    i: &'b Input<'a>,
    attrs: Attrs,
    out: Outcome,
    blocked: BTreeSet<String>,
}

impl<'a, 'b> Ctx<'a, 'b> {
    fn conflict(&mut self, path: &str, kind: &str) {
        self.out.conflicts.push(Conflict {
            path: path.to_string(),
            kind: kind.to_string(),
            base: Some(self.i.base_id.to_string()),
            ours: Some(self.i.ours_id.to_string()),
            theirs: Some(self.i.theirs_id.to_string()),
        });
        let label = match kind {
            "content" => format!("CONFLICT (content): Merge conflict in {path}"),
            "add/add" => format!("CONFLICT (add/add): Merge conflict in {path}"),
            "modify/delete" => {
                format!("CONFLICT (modify/delete): {path} deleted in {} and modified in HEAD.", self.i.theirs_label)
            }
            "delete/modify" => {
                format!("CONFLICT (modify/delete): {path} deleted in HEAD and modified in {}.", self.i.theirs_label)
            }
            "binary" => format!("CONFLICT (binary): {path} changed on both sides; kept HEAD"),
            k => format!("CONFLICT ({k}): {path}"),
        };
        self.out.messages.push(label);
    }

    fn take_theirs(&mut self, tp: &str, rp: &str) -> Result<()> {
        fsutil::copy_entry(self.i.theirs, tp, self.i.result, rp, None)?;
        self.out.changed.insert(rp.to_string());
        Ok(())
    }

    fn same(&self, ra: &Path, pa: &str, rb: &Path, pb: &str) -> Result<bool> {
        let (ma, mb) = (fsutil::lmeta(ra, pa), fsutil::lmeta(rb, pb));
        match (&ma, &mb) {
            (Some(a), Some(b)) if a.ftype == FType::File && a.executable() != b.executable() => Ok(false),
            _ => fsutil::same_entry(ra, pa, rb, pb, None),
        }
    }

    fn content_same(&self, ra: &Path, pa: &str, rb: &Path, pb: &str) -> Result<bool> {
        fsutil::same_entry(ra, pa, rb, pb, None)
    }
}

pub fn merge(i: &Input) -> Result<Outcome> {
    let (ours, _) = side(i.base, i.ours)?;
    let (theirs, tlist) = side(i.base, i.theirs)?;
    let mut c = Ctx { i, attrs: Attrs::load(i.ours), out: Outcome::default(), blocked: BTreeSet::new() };
    let mut done: BTreeSet<String> = BTreeSet::new();

    // 1. Moves made by theirs.
    for t in tlist.iter().filter(|t| t.is_rename()) {
        let src = t.old_path.clone().unwrap();
        let dst = t.path.clone();
        done.insert(src.clone());
        done.insert(dst.clone());
        match ours.at(&src) {
            OursAt::Moved(q) if q != dst => {
                c.conflict(&src, &format!("rename/rename ({q} vs {dst})"));
            }
            OursAt::Moved(_) => {
                file_merge(&mut c, &src, &dst, &dst, &ours, &theirs)?;
            }
            OursAt::Deleted => {
                c.take_theirs(&dst, &dst)?;
                c.conflict(&dst, "rename/delete");
            }
            OursAt::Unchanged | OursAt::Changed => {
                if fsutil::lmeta(i.result, &dst).is_some() {
                    c.conflict(&dst, "add/add");
                    continue;
                }
                fsutil::make_parents(i.result, &dst, None)?;
                std::fs::rename(fsutil::safe_join(i.result, &src)?, fsutil::safe_join(i.result, &dst)?)
                    .with_context(|| format!("move {src} to {dst}"))?;
                c.out.changed.insert(dst.clone());
                c.out.changed.insert(src.clone());
                prune(i.result, i.theirs, &src)?;
                file_merge(&mut c, &src, &dst, &dst, &ours, &theirs)?;
            }
        }
    }

    // 2. Adds, changes and deletes made by theirs.
    let mut dir_deletes = Vec::new();
    for t in &tlist {
        let p = t.path.clone();
        if done.contains(&p) || t.is_rename() {
            continue;
        }
        if c.blocked.iter().any(|b| p.starts_with(&format!("{b}/"))) {
            continue;
        }
        let a_b = fsutil::lmeta(i.base, &p);
        let a_t = fsutil::lmeta(i.theirs, &p);
        let q = match ours.at(&p) {
            OursAt::Moved(q) => q,
            _ => p.clone(),
        };
        let a_r = fsutil::lmeta(i.result, &q);
        // Folders.
        if a_t.as_ref().map(|m| m.is_dir()).unwrap_or(false) {
            match (&a_b, &a_r) {
                (Some(b), Some(r)) if !b.is_dir() && !r.is_dir() => {
                    if c.same(i.base, &p, i.result, &q)? {
                        fsutil::remove_entry(i.result, &q)?;
                        std::fs::create_dir_all(fsutil::safe_join(i.result, &q)?)?;
                        c.out.changed.insert(q.clone());
                    } else {
                        c.conflict(&p, "type change (file vs directory)");
                        c.blocked.insert(p.clone());
                    }
                }
                (_, Some(r)) if !r.is_dir() => {
                    c.conflict(&p, "type change (file vs directory)");
                    c.blocked.insert(p.clone());
                }
                (Some(b), None) if b.is_dir() => {
                    if dir_has_entries(i.theirs, &p) {
                        c.conflict(&p, "directory deleted by ours, theirs added files");
                    }
                    std::fs::create_dir_all(fsutil::safe_join(i.result, &q)?)?;
                }
                (_, None) => {
                    fsutil::make_parents(i.result, &format!("{q}/x"), None)?;
                }
                _ => {}
            }
            continue;
        }
        // Deletes.
        if a_t.is_none() {
            if a_b.as_ref().map(|m| m.is_dir()).unwrap_or(false) {
                dir_deletes.push(p.clone());
                continue;
            }
            match ours.at(&p) {
                OursAt::Deleted => {}
                OursAt::Unchanged => {
                    if a_r.is_some() {
                        fsutil::remove_entry(i.result, &q)?;
                        c.out.changed.insert(q.clone());
                        prune(i.result, i.theirs, &q)?;
                    }
                }
                OursAt::Changed | OursAt::Moved(_) => {
                    if a_b.is_some() && a_r.is_some() && c.same(i.base, &p, i.result, &q)? {
                        fsutil::remove_entry(i.result, &q)?;
                        c.out.changed.insert(q.clone());
                    } else if a_b.is_some() {
                        c.conflict(&q, "modify/delete");
                    }
                }
            }
            continue;
        }
        // Theirs added or changed a file or a link.
        let parent = parent_of(&p);
        if !parent.is_empty()
            && fsutil::lmeta(i.base, &parent).map(|m| m.is_dir()).unwrap_or(false)
            && fsutil::lmeta(i.ours, &parent).is_none()
            && !matches!(ours.at(&parent), OursAt::Moved(_))
        {
            c.conflict(&p, "directory deleted by ours, theirs changed or added a file");
            c.take_theirs(&p, &p)?;
            continue;
        }
        if a_b.is_none() {
            match &a_r {
                None => c.take_theirs(&p, &q)?,
                Some(_) => {
                    if c.same(i.theirs, &p, i.result, &q)? {
                        continue;
                    }
                    add_add(&mut c, &p, &q)?;
                }
            }
            continue;
        }
        if a_r.is_none() {
            c.take_theirs(&p, &q)?;
            c.conflict(&q, "delete/modify");
            continue;
        }
        file_merge(&mut c, &p, &p, &q, &ours, &theirs)?;
    }

    // 3. Folders deleted by theirs, deepest first.
    dir_deletes.sort_by_key(|p| std::cmp::Reverse(p.len()));
    for p in dir_deletes {
        let rp = fsutil::safe_join(i.result, &p)?;
        if rp.exists() {
            if std::fs::remove_dir(&rp).is_err() {
                c.conflict(&p, "theirs deleted a directory that ours changed");
            } else {
                c.out.changed.insert(p.clone());
            }
        }
    }
    Ok(c.out)
}

fn parent_of(p: &str) -> String {
    match p.rfind('/') {
        Some(i) => p[..i].to_string(),
        None => String::new(),
    }
}

fn dir_has_entries(root: &Path, p: &str) -> bool {
    fsutil::safe_join(root, p).ok().and_then(|d| std::fs::read_dir(d).ok()).map(|mut r| r.next().is_some()).unwrap_or(false)
}

fn prune(result: &Path, theirs: &Path, rel: &str) -> Result<()> {
    let mut comps: Vec<&str> = rel.split('/').collect();
    comps.pop();
    while !comps.is_empty() {
        let d = comps.join("/");
        if fsutil::lmeta(theirs, &d).is_some() {
            break;
        }
        let p = fsutil::safe_join(result, &d)?;
        if std::fs::remove_dir(&p).is_err() {
            break;
        }
        comps.pop();
    }
    Ok(())
}

fn add_add(c: &mut Ctx, p: &str, q: &str) -> Result<()> {
    let i = c.i;
    let (mr, mt) = (fsutil::lmeta(i.result, q).unwrap(), fsutil::lmeta(i.theirs, p).unwrap());
    if mr.ftype == FType::File && mt.ftype == FType::File {
        let driver = driver_for(c, q, i.result, q, i.theirs, p, None);
        if driver == "text" || driver == "union" {
            let empty = tempfile_path("base")?;
            std::fs::write(&empty, b"")?;
            let r =
                run_merge_file(i, &fsutil::safe_join(i.result, q)?, &empty, &fsutil::safe_join(i.theirs, p)?, driver == "union");
            let _ = std::fs::remove_file(&empty);
            c.out.changed.insert(q.to_string());
            c.out.messages.push(format!("Auto-merging {q}"));
            if r? > 0 {
                c.conflict(q, "add/add");
            }
            return Ok(());
        }
    }
    c.conflict(q, "add/add");
    Ok(())
}

/// Merge one entry changed by theirs: base path `bp`, theirs path `tp`, result path `rp`.
fn file_merge(c: &mut Ctx, bp: &str, tp: &str, rp: &str, ours: &Side, theirs: &Side) -> Result<()> {
    let i = c.i;
    let (mb, mt, mr) = match (fsutil::lmeta(i.base, bp), fsutil::lmeta(i.theirs, tp), fsutil::lmeta(i.result, rp)) {
        (Some(b), Some(t), Some(r)) => (b, t, r),
        (_, Some(_), None) => {
            c.take_theirs(tp, rp)?;
            return Ok(());
        }
        _ => return Ok(()),
    };
    if mr.ftype != mt.ftype {
        if mr.ftype == mb.ftype && c.content_same(i.base, bp, i.result, rp)? {
            c.take_theirs(tp, rp)?;
        } else {
            c.conflict(rp, &format!("type change ({} vs {})", kind(&mr), kind(&mt)));
        }
        return Ok(());
    }
    if mt.ftype == FType::Symlink {
        let (b, r, t) = (fsutil::read_link(i.base, bp)?, fsutil::read_link(i.result, rp)?, fsutil::read_link(i.theirs, tp)?);
        if t == b || t == r {
            return Ok(());
        }
        if r == b {
            c.take_theirs(tp, rp)?;
        } else {
            c.conflict(rp, "symlink target");
        }
        return Ok(());
    }
    if mt.ftype != FType::File {
        return Ok(());
    }
    if mb.ftype != FType::File {
        // The base was a link or a folder: both sides made a file here.
        if !c.content_same(i.result, rp, i.theirs, tp)? {
            add_add(c, tp, rp)?;
        }
        return Ok(());
    }
    // Content.
    let t_changed = !c.content_same(i.base, bp, i.theirs, tp)?;
    if t_changed {
        let o_changed = !c.content_same(i.base, bp, i.result, rp)?;
        if !o_changed {
            let mode = fsutil::lmeta(i.result, rp).unwrap().mode;
            fsutil::copy_entry(i.theirs, tp, i.result, rp, None)?;
            fsutil::set_mode(i.result, rp, mode)?;
            c.out.changed.insert(rp.to_string());
        } else if !c.content_same(i.result, rp, i.theirs, tp)? {
            let tr = theirs.ranges(tp);
            let or = ours.ranges(rp);
            let driver = driver_for(c, rp, i.result, rp, i.theirs, tp, Some(i.base));
            run_driver(c, &driver, bp, tp, rp, or, tr)?;
        }
    }
    // Executable bit, merged on its own.
    let (mb, mr, mt) = (mb, fsutil::lmeta(i.result, rp).unwrap_or(mr), mt);
    if mb.executable() != mt.executable() && mr.executable() == mb.executable() {
        let m = if mt.executable() { mr.mode | 0o111 } else { mr.mode & !0o111 };
        fsutil::set_mode(i.result, rp, m)?;
        c.out.changed.insert(rp.to_string());
    } else if mb.executable() != mt.executable() && mr.executable() != mt.executable() {
        c.conflict(rp, "mode");
    }
    Ok(())
}

fn kind(m: &Meta) -> &'static str {
    match m.ftype {
        FType::File => "file",
        FType::Dir => "directory",
        FType::Symlink => "symlink",
        FType::Special => "special",
    }
}

fn driver_for(c: &Ctx, attr_path: &str, ra: &Path, pa: &str, rb: &Path, pb: &str, base: Option<&Path>) -> String {
    if let Some(d) = c.attrs.get(c.i.ours, attr_path, "merge") {
        return match d.as_str() {
            "false" => "binary".into(),
            "true" => "text".into(),
            x => x.to_string(),
        };
    }
    let bin = |r: &Path, p: &str| fsutil::read_file(r, p).map(|d| fsutil::is_binary(&d)).unwrap_or(false);
    let _ = base;
    if bin(ra, pa) || bin(rb, pb) {
        "binary".into()
    } else {
        "text".into()
    }
}

fn tempfile_path(tag: &str) -> Result<PathBuf> {
    Ok(std::env::temp_dir().join(format!("layr-{tag}-{}", crate::ids::new_id())))
}

/// The bytes of a regular file of a version, read without following links. Anything else (a
/// link, a folder, nothing) reads as empty: a link in a merged tree must never make the
/// service read the file it points to.
fn version_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path);
    match f {
        Ok(mut f) if f.metadata().map(|m| m.is_file()).unwrap_or(false) => {
            let mut v = Vec::new();
            use std::io::Read;
            let _ = f.read_to_end(&mut v);
            v
        }
        _ => Vec::new(),
    }
}

/// `git merge-file` on private copies: writes the result into `ours`, returns the number of
/// conflicts.
fn run_merge_file(i: &Input, ours: &Path, base: &Path, theirs: &Path, union: bool) -> Result<i32> {
    let dir = private_dir(None)?;
    let (o, b, t) = (dir.join("ours"), dir.join("base"), dir.join("theirs"));
    std::fs::write(&o, version_bytes(ours))?;
    std::fs::write(&b, version_bytes(base))?;
    std::fs::write(&t, version_bytes(theirs))?;
    let mut cmd = Command::new("git");
    cmd.arg("merge-file");
    if union {
        cmd.arg("--union");
    }
    cmd.args(["-L", i.ours_label, "-L", "base", "-L", i.theirs_label]).arg(&o).arg(&b).arg(&t);
    let st = cmd.status().context("run git merge-file");
    let res = match st {
        Ok(st) => match st.code() {
            Some(n) if n >= 0 => {
                // Write in place (same inode), never through a link.
                let data = std::fs::read(&o)?;
                use std::io::Write;
                use std::os::unix::fs::OpenOptionsExt;
                let mut f = std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NOFOLLOW).open(ours)?;
                f.set_len(0)?;
                f.write_all(&data)?;
                Ok(n)
            }
            _ => Err(anyhow::anyhow!("git merge-file failed")),
        },
        Err(e) => Err(e),
    };
    let _ = std::fs::remove_dir_all(&dir);
    res
}

fn run_driver(
    c: &mut Ctx,
    driver: &str,
    bp: &str,
    tp: &str,
    rp: &str,
    ours_ranges: Option<Vec<(u64, u64)>>,
    theirs_ranges: Option<Vec<(u64, u64)>>,
) -> Result<()> {
    let i = c.i;
    let r_path = fsutil::safe_join(i.result, rp)?;
    let b_path = fsutil::safe_join(i.base, bp)?;
    let t_path = fsutil::safe_join(i.theirs, tp)?;
    c.out.changed.insert(rp.to_string());
    match driver {
        "text" | "union" => {
            c.out.messages.push(format!("Auto-merging {rp}"));
            let n = run_merge_file(i, &r_path, &b_path, &t_path, driver == "union")?;
            if n > 0 {
                c.conflict(rp, "content");
            }
        }
        "ours" => {}
        "theirs" => c.take_theirs(tp, rp)?,
        "binary" => c.conflict(rp, "binary"),
        "blocks" => {
            c.out.messages.push(format!("Auto-merging {rp} (blocks)"));
            match block_merge(i, bp, tp, rp, ours_ranges, theirs_ranges)? {
                true => verify(c, rp)?,
                false => c.conflict(rp, "binary (overlapping blocks)"),
            }
        }
        name => {
            let cmd = match (i.config)(&format!("merge.driver.{name}")) {
                Some(cmd) => cmd,
                None => {
                    c.conflict(rp, &format!("unknown merge driver {name}"));
                    return Ok(());
                }
            };
            c.out.messages.push(format!("Auto-merging {rp} ({name})"));
            if !run_command_driver(i, &cmd, &b_path, &r_path, &t_path, rp)? {
                c.conflict(rp, &format!("driver {name}"));
            } else {
                verify(c, rp)?;
            }
        }
    }
    Ok(())
}

fn verify(c: &mut Ctx, rp: &str) -> Result<()> {
    let i = c.i;
    let name = match c.attrs.get(i.ours, rp, "merge-verify") {
        Some(n) => n,
        None => return Ok(()),
    };
    let cmd = match (i.config)(&format!("merge.verify.{name}")) {
        Some(c) => c,
        None => {
            c.conflict(rp, &format!("unknown verify command {name}"));
            return Ok(());
        }
    };
    let dir = private_dir(i.run_as)?;
    let a = dir.join("result");
    std::fs::write(&a, version_bytes(&fsutil::safe_join(i.result, rp)?))?;
    chown_tree(&dir, i.run_as)?;
    let ok = shell(&cmd.replace("%A", &sh_quote(&a)).replace("%P", &sh_quote_str(rp)), i.run_as, i.allow_root)?;
    let _ = std::fs::remove_dir_all(&dir);
    if !ok {
        // Keep ours: copy back the ours version.
        fsutil::copy_entry(i.ours, rp, i.result, rp, None)?;
        c.conflict(rp, &format!("verify {name} failed"));
    }
    Ok(())
}

fn private_dir(run_as: Option<(u32, u32)>) -> Result<PathBuf> {
    let d = tempfile_path("driver")?;
    std::fs::create_dir(&d)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700))?;
    chown_tree(&d, run_as)?;
    Ok(d)
}

fn chown_tree(d: &Path, run_as: Option<(u32, u32)>) -> Result<()> {
    if let Some((u, g)) = run_as {
        if rustix::process::geteuid().is_root() {
            fsutil::chown_nofollow(d, u, g)?;
            for e in std::fs::read_dir(d)?.flatten() {
                fsutil::chown_nofollow(&e.path(), u, g)?;
            }
        }
    }
    Ok(())
}

fn sh_quote(p: &Path) -> String {
    sh_quote_str(&p.to_string_lossy())
}

fn sh_quote_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Run a configured command as the driver user (the lead of this machine), with a clean
/// environment. Never as root unless the caller of the merge is root.
fn shell(cmd: &str, run_as: Option<(u32, u32)>, allow_root: bool) -> Result<bool> {
    let uid = run_as.map(|r| r.0).unwrap_or(0);
    if uid == 0 && !allow_root && rustix::process::geteuid().is_root() {
        bail!("refusing to run a merge command as root (the project has no lead on this machine)");
    }
    let mut c = Command::new("sh");
    c.arg("-c").arg(cmd);
    crate::privs::command_as(&mut c, uid, &[])?;
    Ok(c.status().with_context(|| format!("run {cmd}"))?.success())
}

/// A configured driver command, as in git: `%O` base, `%A` ours (the result is written here),
/// `%B` theirs, `%P` path, `%L` marker size.
fn run_command_driver(i: &Input, cmd: &str, b: &Path, r: &Path, t: &Path, rp: &str) -> Result<bool> {
    let dir = private_dir(i.run_as)?;
    let (o, a, bb) = (dir.join("base"), dir.join("ours"), dir.join("theirs"));
    std::fs::write(&o, version_bytes(b))?;
    std::fs::write(&a, version_bytes(r))?;
    std::fs::write(&bb, version_bytes(t))?;
    chown_tree(&dir, i.run_as)?;
    let full = cmd
        .replace("%O", &sh_quote(&o))
        .replace("%A", &sh_quote(&a))
        .replace("%B", &sh_quote(&bb))
        .replace("%P", &sh_quote_str(rp))
        .replace("%L", "7");
    let ok = shell(&full, i.run_as, i.allow_root)?;
    if ok {
        let data = std::fs::read(&a)?;
        let mode = fsutil::lmeta(i.result, rp).map(|m| m.mode).unwrap_or(0o644);
        fsutil::write_file(i.result, rp, &data, mode, None)?;
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(ok)
}

fn overlaps(a: &[(u64, u64)], b: &[(u64, u64)]) -> bool {
    for (ao, al) in a {
        let ae = ao.saturating_add(*al);
        for (bo, bl) in b {
            let be = bo.saturating_add(*bl);
            if *ao < be && *bo < ae {
                return true;
            }
        }
    }
    false
}

/// Merge a binary file by byte ranges: allowed when both sides kept the inode, the size did not
/// change and the changed ranges do not overlap. Theirs ranges are copied onto ours.
fn block_merge(
    i: &Input,
    bp: &str,
    tp: &str,
    rp: &str,
    ours_ranges: Option<Vec<(u64, u64)>>,
    theirs_ranges: Option<Vec<(u64, u64)>>,
) -> Result<bool> {
    let (or, tr) = match (ours_ranges, theirs_ranges) {
        (Some(o), Some(t)) => (o, t),
        _ => return Ok(false),
    };
    let (mb, mo, mt) = match (fsutil::lmeta(i.base, bp), fsutil::lmeta(i.ours, rp), fsutil::lmeta(i.theirs, tp)) {
        (Some(b), Some(o), Some(t)) => (b, o, t),
        _ => return Ok(false),
    };
    if mb.size != mo.size || mb.size != mt.size || overlaps(&or, &tr) {
        return Ok(false);
    }
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::unix::fs::OpenOptionsExt;
    if mb.ftype != FType::File || mo.ftype != FType::File || mt.ftype != FType::File {
        return Ok(false);
    }
    let mut src = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(fsutil::safe_join(i.theirs, tp)?)?;
    let mut dst =
        std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NOFOLLOW).open(fsutil::safe_join(i.result, rp)?)?;
    for (off, len) in tr {
        if off >= mt.size {
            continue;
        }
        let len = len.min(mt.size - off);
        if crate::btrfs::clone_range(&src, off, len, &dst, off).is_ok() {
            continue;
        }
        let mut buf = vec![0u8; len as usize];
        src.seek(SeekFrom::Start(off))?;
        src.read_exact(&mut buf)?;
        dst.seek(SeekFrom::Start(off))?;
        dst.write_all(&buf)?;
    }
    dst.sync_all()?;
    Ok(true)
}
