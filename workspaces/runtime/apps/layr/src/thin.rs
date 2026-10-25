//! Thin states on APFS. A directory clone copies the metadata of every entry, so a complete tree
//! for each state costs space in proportion to the number of files. A thin layer (`deltas/<id>`)
//! keeps only the entries that differ from a base state and the paths that it removes. A chain
//! of layers ends at a complete tree (`states/<id>`).
//!
//! Layer rules:
//! - An entry in a layer replaces the same path in the layers below it.
//! - A folder in a layer merges with the folder below it, unless the layer also removes the path.
//! - A removed path hides the path and all of its content in the layers below.
//! - Every folder in a layer has the metadata of that folder in the complete tree.
//! - The ancestors of every written or removed path are folders in the layer.
//!
//! `states/<id>` of a thin state is a view: a complete tree made when a command needs one.
//! `layr gc` removes views that were not used recently.
use crate::fsutil::FType;
use crate::storage::{self as apfs, ScanEntry, SubvolInfo};
use crate::store::Project;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

const REMOVED: &std::ffi::CStr = c"com.layr.removed";
/// `layr gc` gives a line head with a longer chain a complete tree (setting `thin.chain`).
pub const MAX_CHAIN: u64 = 32;
/// `layr gc` keeps a view that a command used within this time (setting `thin.view_seconds`).
pub const VIEW_SECONDS: u64 = 600;

#[derive(Debug, Clone)]
pub struct Layer {
    pub root: PathBuf,
    pub removed: BTreeSet<String>,
}

/// A tree made of layers: upper layers first, a complete tree last.
#[derive(Debug, Clone)]
pub struct Layered {
    pub layers: Vec<Layer>,
}

pub fn removed_of(root: &Path) -> Result<BTreeSet<String>> {
    let set: BTreeSet<String> = match crate::sys::apfs_attribute(root, REMOVED, None) {
        Ok(v) => serde_json::from_slice(&v).context("read removed paths of a thin layer")?,
        Err(e) if e.raw_os_error() == Some(libc::ENOATTR) => BTreeSet::new(),
        Err(e) => return Err(e.into()),
    };
    for p in &set {
        if p.is_empty() {
            bail!("a thin layer cannot remove its root");
        }
        crate::fsutil::check_rel(p)?;
    }
    Ok(set)
}

/// Replace the removed paths of a writable layer.
pub fn set_removed(root: &Path, removed: &BTreeSet<String>) -> Result<()> {
    if removed.is_empty() {
        return match crate::sys::apfs_remove_attribute(root, REMOVED) {
            Err(e) if e.raw_os_error() != Some(libc::ENOATTR) => Err(e.into()),
            _ => Ok(()),
        };
    }
    crate::sys::apfs_attribute(root, REMOVED, Some(&serde_json::to_vec(removed)?))?;
    Ok(())
}

fn not_found(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory)
}

fn parent_of(rel: &str) -> &str {
    rel.rfind('/').map(|i| &rel[..i]).unwrap_or("")
}

/// The ancestors of `rel`, nearest to the root first, without the root.
fn ancestors(rel: &str) -> impl Iterator<Item = &str> {
    rel.match_indices('/').map(move |(i, _)| &rel[..i])
}

fn hides(layer: &Layer, rel: &str) -> bool {
    !layer.removed.is_empty()
        && !rel.is_empty()
        && (layer.removed.contains(rel) || ancestors(rel).any(|a| layer.removed.contains(a)))
}

fn remove_subtree<V>(map: &mut BTreeMap<String, V>, rel: &str) {
    map.remove(rel);
    let prefix = format!("{rel}/");
    let keys: Vec<String> = map.range(prefix.clone()..).map(|(k, _)| k).take_while(|k| k.starts_with(&prefix)).cloned().collect();
    for k in keys {
        map.remove(&k);
    }
}

impl Layered {
    pub fn complete(root: &Path) -> Self {
        Layered { layers: vec![Layer { root: root.to_path_buf(), removed: BTreeSet::new() }] }
    }

    /// The layers of a state: its own layer and those of its bases, down to a complete tree.
    pub fn of_state(p: &Project, id: &str) -> Result<Self> {
        let mut layers = Vec::new();
        let mut id = id.to_string();
        let mut seen = BTreeSet::new();
        loop {
            if !seen.insert(id.clone()) {
                bail!("the thin state {} has a cyclic base", crate::ids::short(&id));
            }
            let full = p.state_path(&id);
            if full.is_dir() {
                layers.push(Layer { root: full, removed: BTreeSet::new() });
                return Ok(Layered { layers });
            }
            let d = p.delta_path(&id);
            if !d.is_dir() {
                bail!("state {} is not on this machine (run layr sync)", crate::ids::short(&id));
            }
            let base = apfs::subvol_info(&d)?.base.with_context(|| format!("thin state {id} has no base"))?;
            layers.push(Layer { removed: removed_of(&d)?, root: d });
            id = base;
        }
    }

    /// A layer, such as an index, over another tree.
    pub fn over(root: &Path, base: Layered) -> Result<Self> {
        let mut layers = vec![Layer { root: root.to_path_buf(), removed: removed_of(root)? }];
        layers.extend(base.layers);
        Ok(Layered { layers })
    }

    pub fn is_complete(&self) -> bool {
        self.layers.len() == 1
    }

    pub fn top(&self) -> &Path {
        &self.layers[0].root
    }

    pub fn bottom(&self) -> &Path {
        &self.layers[self.layers.len() - 1].root
    }

    /// The path of `rel` in the layer that holds it. `None` when the tree has no such entry.
    pub fn find(&self, rel: &str) -> Result<Option<PathBuf>> {
        for (i, l) in self.layers.iter().enumerate() {
            if i > 0 && hides(&self.layers[i - 1], rel) {
                return Ok(None);
            }
            // A link above `rel` in this layer is the visible entry: the path does not exist.
            let Ok(p) = crate::fsutil::safe_join(&l.root, rel) else {
                return Ok(None);
            };
            match fs::symlink_metadata(&p) {
                // On a volume that ignores case, `readme` also finds `README`: another path.
                Ok(_) if crate::fsutil::exact_metadata(&p, rel).is_some() => return Ok(Some(p)),
                Ok(_) => continue,
                Err(e) if not_found(&e) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(None)
    }

    /// The path to read `rel` from. An absent entry gives a path that does not exist.
    pub fn path(&self, rel: &str) -> Result<PathBuf> {
        match self.find(rel)? {
            Some(p) => Ok(p),
            None => crate::fsutil::safe_join(self.top(), rel),
        }
    }

    /// All entries of the merged tree, as `apfs::scan_with_data` gives them for a folder.
    pub fn scan(&self, data: bool) -> Result<BTreeMap<String, ScanEntry>> {
        let mut map = apfs::scan_with_data(self.bottom(), data)?;
        for l in self.layers.iter().rev().skip(1) {
            for r in &l.removed {
                remove_subtree(&mut map, r);
            }
            map.extend(apfs::scan_with_data(&l.root, data)?);
        }
        Ok(map)
    }

    /// The entry at `rel` and its content in the merged tree, as `scan` gives them.
    pub fn scan_at(&self, rel: &str) -> Result<BTreeMap<String, ScanEntry>> {
        let mut map = apfs::scan_at(self.bottom(), rel)?;
        for l in self.layers.iter().rev().skip(1) {
            if hides(l, rel) {
                map.clear();
            } else {
                let prefix = format!("{rel}/");
                for r in l.removed.iter().filter(|r| r.starts_with(&prefix)) {
                    remove_subtree(&mut map, r);
                }
            }
            map.extend(apfs::scan_at(&l.root, rel)?);
        }
        Ok(map)
    }

    /// Entries in walk order (a folder before its content), as `fsutil::walk` gives them.
    pub fn entries(&self, rel: &str) -> Result<Vec<(String, crate::fsutil::Meta)>> {
        let map = self.scan(false)?;
        let prefix = if rel.is_empty() { String::new() } else { format!("{rel}/") };
        Ok(map.into_iter().filter(|(k, _)| !k.is_empty() && k.starts_with(&prefix)).map(|(k, v)| (k, v.meta)).collect())
    }

    /// Make a complete tree at `dst`, which must not exist. A read-only tree keeps the
    /// immutable flags of the cloned entries; the caller seals the rest and writes the marker.
    pub fn materialize(&self, dst: &Path, readonly: bool) -> Result<()> {
        let _t = crate::trace::span(format!("thin materialize {} layers", self.layers.len()));
        let entries = apfs::scan_with_data(self.bottom(), false)?;
        apfs::clone_complete(self.bottom(), dst, &entries, readonly)
            .with_context(|| format!("clone {}", self.bottom().display()))?;
        let result = (|| -> Result<()> {
            for l in self.layers.iter().rev().skip(1) {
                apply(l, dst)?;
            }
            if !readonly {
                apfs::seal(dst, false, false)?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = apfs::delete_tree(dst);
        }
        result
    }
}

/// Make the parent folder of `rel` below `root` writable for a change of its entries. The
/// caller restores its metadata from the layer.
fn open_parent(root: &Path, rel: &str) -> Result<()> {
    let parent = root.join(parent_of(rel));
    crate::sys::apfs_immutable(&parent, false)?;
    let mode = fs::symlink_metadata(&parent)?.mode() & 0o7777;
    if mode & 0o300 != 0o300 {
        fs::set_permissions(&parent, fs::Permissions::from_mode(mode | 0o300))?;
    }
    Ok(())
}

/// Give folder `dst` the metadata of folder `src`, except the Layr markers.
fn copy_dir_meta(src: &Path, dst: &Path, root: bool) -> Result<()> {
    let m = fs::symlink_metadata(src)?;
    crate::sys::apfs_immutable(dst, false)?;
    if !root {
        crate::sys::apfs_copy_metadata(src, dst).with_context(|| format!("copy metadata {}", dst.display()))?;
        crate::sys::apfs_immutable(dst, false)?;
        for name in [c"com.layr.subvolume", REMOVED] {
            if let Err(e) = crate::sys::apfs_remove_attribute(dst, name) {
                if e.raw_os_error() != Some(libc::ENOATTR) {
                    return Err(e.into());
                }
            }
        }
        apfs::keep_birthtime(&m, dst)?;
    }
    fs::set_permissions(dst, fs::Permissions::from_mode(m.mode() & 0o7777))?;
    crate::fsutil::set_mtime(&fs::File::open(dst)?, m.mtime(), m.mtime_nsec())?;
    Ok(())
}

/// Clone an entry with its metadata. A folder brings its content. A clone cannot start at a link.
fn clone_entry(from: &Path, to: &Path, m: &fs::Metadata) -> Result<()> {
    if m.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(from)?, to)?;
        crate::sys::apfs_copy_metadata(from, to).with_context(|| format!("copy metadata {}", to.display()))?;
        crate::sys::apfs_immutable(to, false)?;
        apfs::keep_birthtime(m, to)?;
        crate::sys::apfs_mtime(to, m.mtime(), m.mtime_nsec())?;
        return Ok(());
    }
    crate::sys::apfs_clone(from, to).with_context(|| format!("clone {}", from.display()))?;
    Ok(())
}

/// Apply one layer to a writable complete tree.
fn apply(layer: &Layer, dst: &Path) -> Result<()> {
    for r in &layer.removed {
        let p = dst.join(r);
        if crate::fsutil::exact_metadata(&p, r).is_some() {
            open_parent(dst, r)?;
            apfs::delete_tree(&p)?;
        }
    }
    fn visit(layer: &Path, dst: &Path, rel: &str, dirs: &mut Vec<String>) -> Result<()> {
        let mut names: Vec<String> = fs::read_dir(layer.join(rel))?
            .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect::<std::io::Result<_>>()?;
        names.sort();
        for n in names {
            let child = if rel.is_empty() { n } else { format!("{rel}/{n}") };
            let lm = fs::symlink_metadata(layer.join(&child))?;
            let target = dst.join(&child);
            match crate::fsutil::exact_metadata(&target, &child).ok_or(()) {
                Ok(dm) if lm.is_dir() && dm.is_dir() => {
                    dirs.push(child.clone());
                    visit(layer, dst, &child, dirs)?;
                }
                existing => {
                    open_parent(dst, &child)?;
                    if existing.is_ok() {
                        apfs::delete_tree(&target)?;
                    }
                    clone_entry(&layer.join(&child), &target, &lm)?;
                }
            }
        }
        Ok(())
    }
    let mut dirs = Vec::new();
    visit(&layer.root, dst, "", &mut dirs)?;
    for d in dirs.iter().rev() {
        copy_dir_meta(&layer.root.join(d), &dst.join(d), false)?;
    }
    copy_dir_meta(&layer.root, dst, true)
}

/// The changes from one scan to another, as a thin layer stores them.
#[derive(Debug, Default)]
pub struct Diff {
    /// Paths to copy from the new tree, a folder before its content.
    pub write: BTreeSet<String>,
    /// Topmost paths that the new tree removes or replaces with another type.
    pub removed: BTreeSet<String>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.write.is_empty() && self.removed.is_empty()
    }
}

/// True when two entries have the same type, metadata, and data. Unknown data differs.
fn same(a: &ScanEntry, b: &ScanEntry, link: impl Fn() -> Result<bool>) -> Result<bool> {
    let (fa, fb) = (&a.fingerprint, &b.fingerprint);
    if a.meta.ftype != b.meta.ftype
        || a.meta.mode != b.meta.mode
        || a.meta.uid != b.meta.uid
        || a.meta.gid != b.meta.gid
        // A folder size counts the entries of one layer only.
        || (fa.3 != fb.3 && a.meta.ftype != FType::Dir)
        || (fa.4, fa.5) != (fb.4, fb.5)
    {
        return Ok(false);
    }
    match a.meta.ftype {
        FType::Dir => Ok(true),
        FType::File => Ok(a.data.zip(b.data).is_some_and(|(x, y)| {
            x.generation.is_some() && y.generation.is_some() && x.clone_id.is_some() && x.clone_id == y.clone_id
        })),
        FType::Symlink => link(),
        FType::Special => Ok(false),
    }
}

/// Compare a scan of `new` with a scan of `old`. Both scans need data identities.
pub fn diff(
    old: &BTreeMap<String, ScanEntry>,
    old_path: &dyn Fn(&str) -> Result<PathBuf>,
    new: &BTreeMap<String, ScanEntry>,
    new_root: &Path,
) -> Result<Diff> {
    let mut d = Diff::default();
    for (k, e) in new {
        if k.is_empty() {
            continue;
        }
        match old.get(k) {
            None => {
                d.write.insert(k.clone());
            }
            Some(o) if o.meta.ftype != e.meta.ftype => {
                d.removed.insert(k.clone());
                d.write.insert(k.clone());
            }
            Some(o) => {
                let link = || Ok(fs::read_link(old_path(k)?)? == fs::read_link(new_root.join(k))?);
                if !same(o, e, link)? {
                    d.write.insert(k.clone());
                }
            }
        }
    }
    for k in old.keys() {
        if !k.is_empty() && !new.contains_key(k) && !ancestors(k).any(|a| d.removed.contains(a)) {
            d.removed.insert(k.clone());
        }
    }
    // A replaced folder hides its old content; drop paths below it.
    let replaced: Vec<String> = d.removed.iter().filter(|k| ancestors(k).any(|a| d.removed.contains(a))).cloned().collect();
    for k in replaced {
        d.removed.remove(&k);
    }
    Ok(d)
}

/// Write a thin layer at `dst` (which must not exist) with the entries of `src` that `d` names.
/// The caller validates `src` against concurrent changes. `writable` keeps the layer open for
/// later changes (an index); otherwise the layer is sealed.
/// With `gone`, an entry that left `src` after `d` was made is skipped and named in `gone`.
pub fn write_layer(
    src: &Path,
    dst: &Path,
    d: &Diff,
    base: &str,
    writable: bool,
    gone: Option<&mut BTreeSet<String>>,
) -> Result<()> {
    let _t = crate::trace::span(format!("thin write {} entries", d.write.len()));
    ensure_parent(dst)?;
    fs::create_dir(dst)?;
    let result = (|| -> Result<()> {
        apfs::write_info(
            dst,
            &SubvolInfo {
                uuid: crate::ids::new_id(),
                received_uuid: None,
                parent_uuid: None,
                readonly: false,
                base: Some(base.to_string()),
            },
        )?;
        set_removed(dst, &d.removed)?;
        add_entries(src, dst, d.write.iter().map(|s| s.as_str()), d.removed.iter().map(|s| s.as_str()), gone)?;
        copy_dir_meta(src, dst, true)?;
        if !writable {
            apfs::seal(dst, true, false)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = apfs::delete_tree(dst);
    }
    result
}

/// Copy entries of `src` into a writable layer. Each folder of the layer gets the metadata of
/// the same folder of `src`.
pub fn add_entries<'a>(
    src: &Path,
    dst: &Path,
    write: impl Iterator<Item = &'a str>,
    removed: impl Iterator<Item = &'a str>,
    mut gone: Option<&mut BTreeSet<String>>,
) -> Result<()> {
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    // The ancestors of written and removed paths are folders of the layer (layer rule 5).
    // `diff` compares exact names, so a removed path and a written path never need the same
    // folder in two cases: a folder renamed to another case is removed as a whole.
    let container = |dirs: &mut BTreeSet<String>, rel: &str| -> Result<()> {
        for a in ancestors(rel) {
            if dirs.insert(a.to_string()) || fs::symlink_metadata(dst.join(a)).is_err() {
                match fs::create_dir(dst.join(a)) {
                    Err(e) if e.kind() != ErrorKind::AlreadyExists => return Err(e.into()),
                    _ => {}
                }
            }
        }
        Ok(())
    };
    for r in removed {
        container(&mut dirs, r)?;
    }
    for k in write {
        // Below a new folder that went away: the entry went away too.
        if let Some(g) = gone.as_deref_mut() {
            if ancestors(k).any(|a| g.contains(a)) {
                g.insert(k.to_string());
                continue;
            }
        }
        container(&mut dirs, k)?;
        let to = dst.join(k);
        let copied = (|| -> Result<()> {
            let from = crate::fsutil::safe_join(src, k)?;
            let m = fs::symlink_metadata(&from)?;
            if crate::fsutil::exact_metadata(&to, k).is_some() {
                open_parent(dst, k)?;
                apfs::delete_tree(&to)?;
            }
            if m.is_dir() {
                fs::create_dir(&to)?;
                dirs.insert(k.to_string());
            } else if m.is_file() {
                crate::sys::apfs_clone(&from, &to).with_context(|| format!("clone {}", from.display()))?;
                crate::sys::apfs_immutable(&to, false)?;
            } else if m.file_type().is_symlink() {
                clone_entry(&from, &to, &m)?;
            } else {
                bail!("cannot snapshot special file {}", from.display());
            }
            Ok(())
        })();
        if let Err(e) = copied {
            let left = crate::fsutil::safe_join(src, k).ok().and_then(|f| fs::symlink_metadata(f).ok()).is_none();
            match gone.as_deref_mut() {
                Some(g) if left => {
                    let _ = apfs::delete_tree(&to);
                    g.insert(k.to_string());
                }
                _ => return Err(e),
            }
        }
    }
    for d in dirs.iter().rev() {
        let from = crate::fsutil::safe_join(src, d).ok();
        let is_dir = from.as_ref().is_some_and(|f| fs::symlink_metadata(f).map(|m| m.is_dir()).unwrap_or(false));
        let copied = match &from {
            Some(f) if is_dir => copy_dir_meta(f, &dst.join(d), false),
            _ => Ok(()),
        };
        let left = from.as_ref().and_then(|f| fs::symlink_metadata(f).ok()).is_none();
        match gone.as_deref_mut() {
            // The folder went away during the last copy: so did all of its content.
            Some(g) if left => {
                let _ = apfs::delete_tree(&dst.join(d));
                g.insert(d.clone());
            }
            _ => copied?,
        }
    }
    Ok(())
}

/// Create the folder of thin layers when a project has none yet: a project made by a restore,
/// a sync, or an older version.
pub fn ensure_parent(dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent().filter(|p| !p.exists()) {
        match fs::create_dir(parent) {
            Err(e) if e.kind() != ErrorKind::AlreadyExists => return Err(e.into()),
            _ => fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?,
        }
    }
    Ok(())
}

/// A thin layer cannot keep hard links between its entries: the caller makes a complete tree.
#[derive(Debug)]
pub struct HardLinks;
impl std::fmt::Display for HardLinks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a changed file has hard links")
    }
}
impl std::error::Error for HardLinks {}

/// True for an error that asks for a complete tree instead of a thin layer.
pub fn needs_complete(e: &anyhow::Error) -> bool {
    e.downcast_ref::<HardLinks>().is_some()
}

fn linked(e: &ScanEntry) -> bool {
    e.meta.is_file() && e.nlink > 1
}

/// True when a file at one of `paths` below `root` has another name: staging it into a thin
/// layer would split the names.
pub fn any_linked(root: &Path, paths: &BTreeSet<String>) -> bool {
    paths.iter().any(|p| {
        crate::fsutil::safe_join(root, p)
            .ok()
            .and_then(|f| fs::symlink_metadata(f).ok())
            .is_some_and(|m| m.is_file() && m.nlink() > 1)
    })
}

/// True when the tree at `path` is a thin layer over a base.
pub fn is_thin(path: &Path) -> bool {
    apfs::subvol_info(path).ok().and_then(|i| i.base).is_some()
}

/// Write an empty writable layer over `base`. Its root gets the metadata of `root`.
pub fn empty_layer(dst: &Path, base: &str, root: &Path) -> Result<()> {
    write_layer(root, dst, &Diff::default(), base, true, None)
}

impl Layered {
    /// True when folder `rel` of the merged tree has no entries.
    pub fn is_empty_dir(&self, rel: &str) -> Result<bool> {
        for (i, l) in self.layers.iter().enumerate() {
            if i > 0 && hides(&self.layers[i - 1], rel) {
                break;
            }
            let dir = crate::fsutil::safe_join(&l.root, rel)?;
            if !rel.is_empty() && fs::symlink_metadata(&dir).is_ok() && crate::fsutil::exact_metadata(&dir, rel).is_none() {
                // Another case of this folder in this layer: another path.
                continue;
            }
            let rd = match fs::read_dir(&dir) {
                Ok(r) => r,
                Err(e) if not_found(&e) => continue,
                Err(e) => return Err(e.into()),
            };
            for e in rd {
                let name = e?.file_name().to_string_lossy().into_owned();
                let child = if rel.is_empty() { name } else { format!("{rel}/{name}") };
                if self.find(&child)?.is_some() {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// The tree without its top layer.
    pub fn below(&self) -> Option<Layered> {
        (self.layers.len() > 1).then(|| Layered { layers: self.layers[1..].to_vec() })
    }
}

/// Make the ancestors of `rel` folders of a writable layer. New folders get owner `own`.
fn containers(stage: &Path, rel: &str, own: Option<(u32, u32)>, dirs: &mut BTreeSet<String>, write: bool) -> Result<()> {
    for a in ancestors(rel) {
        let d = crate::fsutil::safe_join(stage, a)?;
        if crate::fsutil::exact_metadata(&d, a).is_none() {
            if fs::symlink_metadata(&d).is_ok() {
                // On a volume that ignores case the layer holds another case of this folder
                // (`Dir` for `dir`). For a removal, its mark alone hides the old content
                // (`hides` reads only the marks), and a folder made below `Dir` would become
                // part of the new tree: the removal gets no folders, an exception to the layer
                // rule. For a write, the other folder must be empty: then it goes.
                if !write {
                    return Ok(());
                }
                let other = crate::fsutil::case_clash(&d).unwrap_or_default();
                if fs::read_dir(&d)?.next().is_some() {
                    bail!(
                        "{a} and {other} differ only in case, and the volume of this store does not tell them apart.\nStage all paths of both folders together (layr add -A), or use a store on a case-sensitive APFS volume."
                    );
                }
                open_parent(stage, a)?;
                fs::remove_dir(&d)?;
            }
            open_parent(stage, a)?;
            fs::create_dir(&d)?;
            if let Some((u, g)) = own {
                crate::fsutil::chown_nofollow(&d, u, g)?;
            }
        }
        dirs.insert(a.to_string());
    }
    Ok(())
}

/// Stage `paths` of `src` into the writable index layer at the top of `index`: an entry that
/// `src` has replaces the index entry, an entry that it lacks leaves the index, and folders that
/// become empty leave too (a state holds files, as a git tree does). New folders get owner `own`.
pub fn stage_entries(index: &Layered, src: &Path, paths: &BTreeSet<String>, own: Option<(u32, u32)>) -> Result<()> {
    let _t = crate::trace::span(format!("thin stage {} paths", paths.len()));
    let stage = index.top().to_path_buf();
    let below = index.below().context("the index has no base")?;
    let mut removed = index.layers[0].removed.clone();
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    let mut gone: Vec<String> = Vec::new();
    // Removals first: on a volume that ignores case, `dir` must leave before `Dir` comes.
    let present = |p: &String| crate::fsutil::safe_join(src, p).ok().and_then(|f| crate::fsutil::exact_metadata(&f, p)).is_some();
    let (gone_first, written): (Vec<&String>, Vec<&String>) = paths.iter().partition(|p| !present(p));
    for p in gone_first.into_iter().chain(written) {
        if p.is_empty() {
            continue;
        }
        // A path below a folder that became a link is a deletion (`copy_selected`).
        let from = crate::fsutil::safe_join(src, p).ok();
        let new = from.as_ref().and_then(|f| crate::fsutil::exact_metadata(f, p));
        // Never follow a link of the index: a path below one is already gone.
        let Ok(at) = crate::fsutil::safe_join(&stage, p) else {
            continue;
        };
        if crate::fsutil::exact_metadata(&at, p).is_some() {
            open_parent(&stage, p)?;
            apfs::delete_tree(&at)?;
        }
        let old = below.find(p).ok().flatten().map(fs::symlink_metadata).transpose()?;
        let (Some(m), Some(from)) = (new, from) else {
            if old.is_some() {
                removed.insert(p.clone());
                containers(&stage, p, own, &mut dirs, false)?;
            }
            gone.push(p.clone());
            continue;
        };
        if old.as_ref().is_some_and(|o| o.is_dir() != m.is_dir() || o.file_type().is_symlink() != m.file_type().is_symlink()) {
            removed.insert(p.clone());
        }
        containers(&stage, p, own, &mut dirs, true)?;
        if m.is_dir() {
            fs::create_dir(&at)?;
            dirs.insert(p.clone());
        } else if m.is_file() || m.file_type().is_symlink() {
            clone_entry(&from, &at, &m)?;
            crate::sys::apfs_immutable(&at, false)?;
        } else {
            bail!("cannot snapshot special file {}", from.display());
        }
    }
    // Only paths that the base has need a removal mark.
    removed.retain(|r| below.find(r).ok().flatten().is_some());
    set_removed(&stage, &removed)?;
    let now = Layered::over(&stage, below.clone())?;
    // Folders that lost their last entry leave, the deepest first.
    for p in gone.iter().rev() {
        for a in ancestors(p).collect::<Vec<_>>().into_iter().rev() {
            let is_dir = now.find(a).ok().flatten().is_some_and(|f| fs::symlink_metadata(f).map(|m| m.is_dir()).unwrap_or(false));
            if !is_dir || !now.is_empty_dir(a)? {
                break;
            }
            let d = crate::fsutil::safe_join(&stage, a)?;
            if crate::fsutil::exact_metadata(&d, a).is_some() {
                open_parent(&stage, a)?;
                apfs::delete_tree(&d)?;
            }
            if below.find(a)?.is_some() {
                removed.insert(a.to_string());
                set_removed(&stage, &removed)?;
            }
            dirs.remove(a);
        }
    }
    // Folders of the index keep the metadata that the index had.
    for d in dirs.iter().rev() {
        let Ok(at) = crate::fsutil::safe_join(&stage, d) else {
            continue;
        };
        if !crate::fsutil::exact_metadata(&at, d).map(|m| m.is_dir()).unwrap_or(false) {
            continue;
        }
        if let Some(b) = below.find(d).ok().flatten().filter(|b| fs::symlink_metadata(b).map(|m| m.is_dir()).unwrap_or(false)) {
            copy_dir_meta(&b, &at, false)?;
        }
    }
    Ok(())
}

/// Copy the entries of `paths` that `src` holds into a new private folder `dst`, for a tool
/// that reads a few files of a state from a folder (git). Absent paths stay absent.
pub fn extract<'a>(src: &Layered, paths: impl Iterator<Item = &'a str>, dst: &Path) -> Result<()> {
    fs::create_dir(dst)?;
    fs::set_permissions(dst, fs::Permissions::from_mode(0o700))?;
    let result = (|| -> Result<()> {
        for p in paths {
            let Some(from) = src.find(p)? else {
                continue;
            };
            let m = fs::symlink_metadata(&from)?;
            if m.is_dir() {
                continue;
            }
            let to = dst.join(p);
            if crate::fsutil::exact_metadata(&to, p).is_some() {
                continue;
            }
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)?;
            }
            clone_entry(&from, &to, &m)?;
            crate::sys::apfs_immutable(&to, false)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = apfs::delete_tree(dst);
    }
    result
}

/// Write a thin layer of `src` over `base`, or `None` when nothing differs. A concurrent
/// change of `src` returns an error.
pub fn snapshot_over(base: &Layered, base_id: &str, src: &Path, dst: &Path, writable: bool) -> Result<Option<Diff>> {
    let old = base.scan(true)?;
    let before = apfs::scan_with_data(src, true)?;
    let d = diff(&old, &|k| base.path(k), &before, src)?;
    if d.is_empty() {
        return Ok(None);
    }
    write_checked(src, base, &old, &before, &d, dst, base_id, writable)?;
    Ok(Some(d))
}

/// Paths whose entries differ between two scans of one tree.
pub(crate) fn changed_paths(before: &BTreeMap<String, ScanEntry>, after: &BTreeMap<String, ScanEntry>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (k, v) in before {
        if after.get(k) != Some(v) {
            out.insert(k.clone());
        }
    }
    for k in after.keys() {
        if !before.contains_key(k) {
            out.insert(k.clone());
        }
    }
    out
}

/// Bring the entries `paths` of the writable complete copy `dst` to their state in `src` now:
/// remove, clone again, or copy the metadata of a folder. Folders keep their other content.
pub(crate) fn resync(src: &Path, dst: &Path, paths: &BTreeSet<String>) -> Result<()> {
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    let mut whole: Vec<String> = Vec::new();
    let source =
        |p: &str| crate::fsutil::safe_join(src, p).ok().and_then(|f| crate::fsutil::exact_metadata(&f, p).map(|m| (f, m)));
    // Removals first: on a volume that ignores case, `readme` must go before `README` comes.
    let (gone, kept): (Vec<_>, Vec<_>) = paths.iter().map(|p| (p, source(p))).partition(|(p, f)| !p.is_empty() && f.is_none());
    for (p, from) in gone.into_iter().chain(kept) {
        if p.is_empty() {
            dirs.insert(String::new());
            continue;
        }
        if whole.iter().any(|w| p.starts_with(&format!("{w}/"))) {
            continue;
        }
        // An entry below a link: the link is in `paths` too and comes first.
        let Ok(at) = crate::fsutil::safe_join(dst, p) else {
            continue;
        };
        let old = crate::fsutil::exact_metadata(&at, p);
        if let (Some((_, m)), Some(o)) = (&from, &old) {
            if m.is_dir() && o.is_dir() {
                dirs.insert(p.clone());
                continue;
            }
        }
        if !crate::fsutil::exact_metadata(&dst.join(parent_of(p)), parent_of(p)).is_some_and(|m| m.is_dir()) {
            continue;
        }
        open_parent(dst, p)?;
        dirs.insert(parent_of(p).to_string());
        if old.is_some() {
            apfs::delete_tree(&at)?;
        }
        if let Some((f, m)) = from {
            if m.is_dir() {
                apfs::copy_subtree(&f, &at)?;
                whole.push(p.clone());
            } else if m.is_file() || m.file_type().is_symlink() {
                clone_entry(&f, &at, &m)?;
            } else {
                bail!("cannot snapshot special file {}", f.display());
            }
        }
    }
    for d in dirs.iter().rev() {
        let (Ok(f), Ok(t)) = (crate::fsutil::safe_join(src, d), crate::fsutil::safe_join(dst, d)) else {
            continue;
        };
        if crate::fsutil::exact_metadata(&f, d).is_some_and(|m| m.is_dir())
            && crate::fsutil::exact_metadata(&t, d).is_some_and(|m| m.is_dir())
        {
            copy_dir_meta(&f, &t, d.is_empty())?;
        }
    }
    Ok(())
}

/// Rounds of copying again what changed during a copy, before Layr keeps the copy as it is.
pub(crate) const COPY_ROUNDS: usize = 6;

/// Wait a little longer after each round in which the source changed.
pub(crate) fn settle(round: usize) {
    std::thread::sleep(std::time::Duration::from_millis((25 * (round as u64 + 1)).min(200)));
}

/// Write the layer `d` of `src` over `base` (scanned as `old`), then check that `src` still
/// matches the scan `before` that gave `d`. When `src` changed during the copy, the difference
/// is made again and the layer is written again. After `COPY_ROUNDS` changed rounds the last
/// layer stays: each entry is a consistent copy from some moment of the copy, as with `git add`.
/// A changed file with more than one name returns `HardLinks`.
#[allow(clippy::too_many_arguments)]
pub fn write_checked(
    src: &Path,
    base: &Layered,
    old: &BTreeMap<String, ScanEntry>,
    before: &BTreeMap<String, ScanEntry>,
    d: &Diff,
    dst: &Path,
    base_id: &str,
    writable: bool,
) -> Result<()> {
    let mut before = before.clone();
    let mut d = Diff { write: d.write.clone(), removed: d.removed.clone() };
    let mut round = 0;
    loop {
        if d.write.iter().any(|k| before.get(k).is_some_and(linked)) {
            bail!(HardLinks);
        }
        if round + 1 >= COPY_ROUNDS {
            // The last round keeps what it copies. Entries that left during the copy are gone.
            let mut gone = BTreeSet::new();
            write_layer(src, dst, &d, base_id, true, Some(&mut gone))?;
            if !gone.is_empty() {
                let mut removed = d.removed.clone();
                removed.extend(gone.into_iter().filter(|g| old.contains_key(g)));
                set_removed(dst, &removed)?;
            }
            eprintln!(
                "warning: {} changed during the whole copy; each file is kept as one of its versions from that time",
                src.display()
            );
            if !writable {
                apfs::seal(dst, true, false)?;
            }
            return Ok(());
        }
        let wrote = write_layer(src, dst, &d, base_id, true, None);
        let after = apfs::scan_with_data(src, true)?;
        if let Err(e) = wrote {
            // A file that went away during the copy is a change; anything else is an error.
            if after == before {
                return Err(e);
            }
            settle(round);
            round += 1;
            d = diff(old, &|k| base.path(k), &after, src)?;
            before = after;
            continue;
        }
        if after == before {
            if !writable {
                apfs::seal(dst, true, false)?;
            }
            return Ok(());
        }
        // Writers changed the source during the copy: make the difference again.
        apfs::delete_tree(dst)?;
        settle(round);
        round += 1;
        d = diff(old, &|k| base.path(k), &after, src)?;
        before = after;
    }
}

fn view_stamp(p: &Project, id: &str) -> PathBuf {
    p.meta_dir().join("views").join(id)
}

fn touch_view(p: &Project, id: &str) {
    let stamp = view_stamp(p, id);
    if let Some(dir) = stamp.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::File::create(&stamp).and_then(|f| f.set_modified(std::time::SystemTime::now()));
}

/// A complete tree of a state: the state's own tree, or a view made from its layers.
pub fn complete_tree(p: &Project, id: &str) -> Result<PathBuf> {
    let full = p.state_path(id);
    let delta = p.delta_path(id);
    if full.is_dir() {
        if delta.is_dir() {
            touch_view(p, id);
        }
        return Ok(full);
    }
    let layered = Layered::of_state(p, id)?;
    let info = apfs::subvol_info(&delta)?;
    let tmp = p.work_dir().join(crate::ids::new_id());
    layered.materialize(&tmp, true)?;
    let made = (|| -> Result<()> {
        apfs::write_info(&tmp, &SubvolInfo { base: None, readonly: true, ..info })?;
        let mode = fs::symlink_metadata(&tmp)?.mode() & 0o7777;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode | 0o200))?;
        match crate::sys::apfs_publish(&tmp, &full) {
            Ok(()) => {
                fs::set_permissions(&full, fs::Permissions::from_mode(mode))?;
                apfs::seal(&full, true, false)?;
            }
            // Another command made the same view.
            Err(_) if full.is_dir() => apfs::delete_tree(&tmp)?,
            Err(e) => return Err(e.into()),
        }
        Ok(())
    })();
    if made.is_err() {
        let _ = apfs::delete_tree(&tmp);
    }
    made?;
    touch_view(p, id);
    Ok(full)
}

/// The base of each thin state on this machine.
pub fn bases(p: &Project) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let dir = p.deltas_dir();
    let rd = match fs::read_dir(&dir) {
        Ok(r) => r,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for e in rd {
        let name = e?.file_name().to_string_lossy().into_owned();
        if !crate::model::is_uuid(&name) {
            continue;
        }
        if let Some(b) = apfs::subvol_info(&p.delta_path(&name)).ok().and_then(|i| i.base) {
            out.insert(name, b);
        }
    }
    Ok(out)
}

/// Delete the trees of a state. A state that is the base of another thin state keeps its
/// trees until that state goes; this returns false then.
pub fn delete_state(p: &Project, id: &str) -> Result<bool> {
    if bases(p)?.values().any(|b| b == id) {
        // Keep the layer; a view is not needed.
        if p.delta_path(id).is_dir() {
            apfs::delete_tree(&p.state_path(id))?;
            let _ = fs::remove_file(view_stamp(p, id));
        }
        return Ok(false);
    }
    apfs::delete_tree(&p.state_path(id))?;
    apfs::delete_tree(&p.delta_path(id))?;
    let _ = fs::remove_file(view_stamp(p, id));
    Ok(true)
}

/// Delete the trees of a state now, also when other thin states use it as their base: those
/// states first get complete trees. For content that must leave the disk (`overlay move --purge`).
pub fn delete_now(p: &Project, id: &str) -> Result<()> {
    for (upper, base) in bases(p)? {
        if base == id {
            make_keyframe(p, &upper)?;
        }
    }
    if !delete_state(p, id)? {
        bail!("state {} is still the base of another state", crate::ids::short(id));
    }
    Ok(())
}

/// Remove the views that no command used within `thin.view_seconds`. Returns their count.
pub fn evict_views(p: &Project, keep: &BTreeSet<String>) -> Result<usize> {
    let limit = p.config_u64("thin.view_seconds", VIEW_SECONDS);
    let mut n = 0;
    for id in bases(p)?.into_keys() {
        let full = p.state_path(&id);
        if keep.contains(&id) || !full.is_dir() {
            continue;
        }
        let age = fs::metadata(view_stamp(p, &id))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|d| d.as_secs())
            .unwrap_or(u64::MAX);
        if age >= limit {
            apfs::delete_tree(&full)?;
            let _ = fs::remove_file(view_stamp(p, &id));
            n += 1;
        }
    }
    Ok(n)
}

/// Give a thin state a complete tree and drop its layer, so the chains above it get short.
pub fn make_keyframe(p: &Project, id: &str) -> Result<()> {
    complete_tree(p, id)?;
    apfs::delete_tree(&p.delta_path(id))?;
    let _ = fs::remove_file(view_stamp(p, id));
    Ok(())
}

/// What `layr gc` did to thin states.
#[derive(Debug, Default)]
pub struct Upkeep {
    pub views: usize,
    pub keyframes: usize,
    pub deleted: usize,
}

/// Upkeep for `layr gc`, with the project lock held: delete states that a record deleted while
/// another thin state still used them and layers that never got a record, remove unused views,
/// and give a head with a long chain a complete tree. `heads` are the states that lines read now.
pub fn upkeep(p: &Project, heads: &BTreeSet<String>) -> Result<Upkeep> {
    let mut done = Upkeep::default();
    // Deleted states that stayed as a base: an upper state may have gone since.
    loop {
        let bases = bases(p)?;
        let used: BTreeSet<&String> = bases.values().collect();
        let mut gone = 0;
        let mut ids: BTreeSet<String> = bases.keys().cloned().collect();
        for e in fs::read_dir(p.states_dir())?.flatten() {
            ids.insert(e.file_name().to_string_lossy().into_owned());
        }
        for id in ids {
            if !crate::model::is_uuid(&id) || used.contains(&id) {
                continue;
            }
            let gone_record = match p.db.state_flags(&id)? {
                Some((_, deleted)) => deleted,
                // A layer without a record: a process stopped before its record. A commit
                // journal owns its own layer; a young layer may still get its record.
                None => p.delta_path(&id).is_dir() && old_layer(&p.delta_path(&id)) && !crate::apfs_commit::preserves(p, &id)?,
            };
            if gone_record {
                apfs::delete_tree(&p.state_path(&id))?;
                apfs::delete_tree(&p.delta_path(&id))?;
                let _ = fs::remove_file(view_stamp(p, &id));
                gone += 1;
            }
        }
        done.deleted += gone;
        if gone == 0 {
            break;
        }
    }
    done.views = evict_views(p, &BTreeSet::new())?;
    let max = p.config_u64("thin.chain", MAX_CHAIN) as usize;
    for h in heads {
        if p.delta_path(h).is_dir() && chain_len(p, h) > max {
            make_keyframe(p, h)?;
            done.keyframes += 1;
        }
    }
    Ok(done)
}

/// True when a layer was made more than an hour ago.
fn old_layer(path: &Path) -> bool {
    use std::os::macos::fs::MetadataExt as MacMetadataExt;
    let Ok(m) = fs::symlink_metadata(path) else {
        return false;
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    now - m.st_birthtime() > 3600
}

/// The number of layers of a state, or 0 when it is not on this machine.
pub fn chain_len(p: &Project, id: &str) -> usize {
    let mut n = 0;
    let mut id = id.to_string();
    loop {
        if p.state_path(&id).is_dir() {
            return n + 1;
        }
        match apfs::subvol_info(&p.delta_path(&id)).ok().and_then(|i| i.base) {
            Some(b) if n < 10_000 => {
                n += 1;
                id = b;
            }
            _ => return 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Sandbox(PathBuf);
    impl Sandbox {
        fn new() -> Self {
            let base = std::env::var_os("LAYR_APFS_TEST_ROOT").expect("set LAYR_APFS_TEST_ROOT to a case-sensitive APFS volume");
            let path = PathBuf::from(base).join(format!("thin-test-{}", crate::ids::new_id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = apfs::delete_tree(&self.0);
        }
    }

    /// Type, mode, size, time, and bytes of every entry except the root.
    fn content(map: &BTreeMap<String, ScanEntry>, path: impl Fn(&str) -> PathBuf) -> Vec<(String, String)> {
        map.iter()
            .filter(|(k, _)| !k.is_empty())
            .map(|(k, e)| {
                let p = path(k);
                let body = match e.meta.ftype {
                    FType::File => format!("{:?}", fs::read(&p).unwrap()),
                    FType::Symlink => fs::read_link(&p).unwrap().display().to_string(),
                    _ => String::new(),
                };
                let (size, mtime) = if e.meta.is_dir() { (0, 0) } else { (e.meta.size, e.fingerprint.4) };
                (k.clone(), format!("{:?} {:o} {} {} {}", e.meta.ftype, e.meta.mode, size, mtime, body))
            })
            .collect()
    }

    fn check_same(layered: &Layered, full: &Path) {
        let a = layered.scan(false).unwrap();
        let b = apfs::scan_with_data(full, false).unwrap();
        assert_eq!(content(&a, |k| layered.path(k).unwrap()), content(&b, |k| full.join(k)));
    }

    fn tree(root: &Path) {
        apfs::create_subvolume(root).unwrap();
        for d in ["a", "a/deep", "b", "gone", "gone/inner", "swap-dir", "linkdir", "quiet"] {
            fs::create_dir(root.join(d)).unwrap();
        }
        fs::write(root.join("a/same"), b"same").unwrap();
        fs::write(root.join("a/edit"), b"old").unwrap();
        fs::write(root.join("a/deep/file"), b"deep").unwrap();
        fs::write(root.join("gone/inner/file"), b"gone").unwrap();
        fs::write(root.join("swap-file"), b"file").unwrap();
        fs::write(root.join("swap-dir/child"), b"child").unwrap();
        fs::write(root.join("b/x"), b"x").unwrap();
        fs::write(root.join("linkdir/inner"), b"inner").unwrap();
        fs::write(root.join("quiet/one"), b"one").unwrap();
        fs::write(root.join("quiet/two"), b"two").unwrap();
        std::os::unix::fs::symlink("a/same", root.join("link")).unwrap();
        fs::hard_link(root.join("a/deep/file"), root.join("b/linked")).unwrap();
    }

    fn linked(root: &Path) -> bool {
        fs::symlink_metadata(root.join("b/linked")).unwrap().ino()
            == fs::symlink_metadata(root.join("a/deep/file")).unwrap().ino()
    }

    fn change(root: &Path) {
        fs::write(root.join("a/edit"), b"new bytes").unwrap();
        fs::write(root.join("a/new"), b"added").unwrap();
        fs::remove_dir_all(root.join("gone")).unwrap();
        fs::remove_file(root.join("swap-file")).unwrap();
        fs::create_dir(root.join("swap-file")).unwrap();
        fs::write(root.join("swap-file/inside"), b"inside").unwrap();
        fs::remove_dir_all(root.join("swap-dir")).unwrap();
        fs::write(root.join("swap-dir"), b"now a file").unwrap();
        fs::set_permissions(root.join("b"), fs::Permissions::from_mode(0o750)).unwrap();
        fs::remove_file(root.join("link")).unwrap();
        std::os::unix::fs::symlink("a/new", root.join("link")).unwrap();
        fs::create_dir(root.join("fresh")).unwrap();
        fs::write(root.join("fresh/f"), b"fresh").unwrap();
        fs::remove_dir_all(root.join("linkdir")).unwrap();
        std::os::unix::fs::symlink("a", root.join("linkdir")).unwrap();
        // A removal whose folder keeps its time: the layer still holds the folder.
        let quiet = fs::metadata(root.join("quiet")).unwrap();
        fs::remove_file(root.join("quiet/one")).unwrap();
        crate::sys::apfs_mtime(&root.join("quiet"), quiet.mtime(), quiet.mtime_nsec()).unwrap();
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn a_base_stays_until_its_upper_state_goes() -> Result<()> {
        let s = Sandbox::new();
        let store = crate::store::Store::open(&s.0.join("store"))?;
        let actor = crate::model::Actor { uid: rustix::process::geteuid().as_raw(), name: "test".into(), session: None };
        let p = Project::create(&store, "p", &actor, serde_json::json!({}))?;
        let (a, b) = (crate::ids::new_id(), crate::ids::new_id());
        tree(&p.state_path(&a));
        apfs::set_readonly(&p.state_path(&a))?;
        let work = s.0.join("work");
        apfs::snapshot(&p.state_path(&a), &work, false)?;
        change(&work);
        snapshot_over(&Layered::complete(&p.state_path(&a)), &a, &work, &p.delta_path(&b), false)?.expect("changes");
        assert_eq!(bases(&p)?.get(&b), Some(&a));
        assert_eq!(chain_len(&p, &b), 2);
        assert!(!delete_state(&p, &a)?, "a base stays");
        assert!(p.state_path(&a).is_dir());
        let view = complete_tree(&p, &b)?;
        check_same(&Layered::complete(&view), &work);
        check_same(&Layered::of_state(&p, &b)?, &work);
        assert!(delete_state(&p, &b)?);
        assert!(!p.delta_path(&b).exists() && !p.state_path(&b).exists());
        assert!(delete_state(&p, &a)?);
        assert!(!p.state_path(&a).exists());
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn gc_removes_old_layers_without_a_record() -> Result<()> {
        let s = Sandbox::new();
        let store = crate::store::Store::open(&s.0.join("store"))?;
        let actor = crate::model::Actor { uid: rustix::process::geteuid().as_raw(), name: "test".into(), session: None };
        let p = Project::create(&store, "p", &actor, serde_json::json!({}))?;
        let base = crate::ids::new_id();
        tree(&p.state_path(&base));
        apfs::set_readonly(&p.state_path(&base))?;
        let (old, young) = (crate::ids::new_id(), crate::ids::new_id());
        for id in [&old, &young] {
            empty_layer(&p.delta_path(id), &base, &p.state_path(&base))?;
        }
        crate::sys::apfs_set_birthtime(&p.delta_path(&old), 1_600_000_000, 1)?;
        let done = upkeep(&p, &BTreeSet::new())?;
        assert_eq!(done.deleted, 1);
        assert!(!p.delta_path(&old).exists() && p.delta_path(&young).is_dir());
        // The base without a layer of its own is not touched.
        assert!(p.state_path(&base).is_dir());
        Ok(())
    }

    /// Change, create, and delete files in `root` until `stop` is set.
    fn writer(root: PathBuf, stop: std::sync::Arc<std::sync::atomic::AtomicBool>) -> std::thread::JoinHandle<usize> {
        std::thread::spawn(move || {
            let mut n = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let i = n % 500;
                let _ = fs::write(root.join(format!("d{}/f{}", i % 10, i)), format!("version {n}"));
                let _ = fs::write(root.join(format!("tmp{}", n % 7)), b"short");
                let _ = fs::remove_file(root.join(format!("tmp{}", (n + 3) % 7)));
                // Folders come and go too.
                let dir = root.join(format!("gone{}", n % 3));
                let _ = fs::create_dir_all(dir.join("inner"));
                let _ = fs::write(dir.join("inner/file"), b"x");
                let _ = fs::remove_dir_all(root.join(format!("gone{}", (n + 1) % 3)));
                std::thread::sleep(std::time::Duration::from_micros(300));
                n += 1;
            }
            n
        })
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn copies_succeed_while_files_change() -> Result<()> {
        let s = Sandbox::new();
        let src = s.0.join("work");
        apfs::create_subvolume(&src)?;
        for i in 0..2000 {
            fs::create_dir_all(src.join(format!("d{}", i % 10)))?;
            fs::write(src.join(format!("d{}/f{i}", i % 10)), b"first")?;
        }
        fs::hard_link(src.join("d5/f1005"), src.join("linked"))?;
        let base = s.0.join("base");
        apfs::snapshot(&src, &base, true)?;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let w = writer(src.clone(), stop.clone());
        let results = (|| -> Result<()> {
            for n in 0..2 {
                apfs::snapshot(&src, &s.0.join(format!("full{n}")), n % 2 == 0)?;
                snapshot_over(&Layered::complete(&base), "base", &src, &s.0.join(format!("thin{n}")), false)?;
            }
            Ok(())
        })();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let writes = w.join().unwrap();
        results?;
        assert!(writes > 100, "the writer ran ({writes} writes)");
        // Each copy holds every file, each with one complete version.
        for n in 0..2 {
            let full = s.0.join(format!("full{n}"));
            assert_eq!(
                fs::metadata(full.join("linked"))?.ino(),
                fs::metadata(full.join("d5/f1005"))?.ino(),
                "hard link in copy {n}"
            );
            let thin = Layered::over(&s.0.join(format!("thin{n}")), Layered::complete(&base))?;
            for i in 0..2000 {
                let rel = format!("d{}/f{i}", i % 10);
                for bytes in [fs::read(full.join(&rel))?, fs::read(thin.path(&rel)?)?] {
                    // The writer truncates before it writes: an empty file is one of its versions.
                    assert!(bytes.is_empty() || bytes == b"first" || bytes.starts_with(b"version "), "{rel}: {bytes:?}");
                }
            }
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn layers_read_and_materialize_like_the_complete_tree() -> Result<()> {
        let s = Sandbox::new();
        let base = s.0.join("base");
        tree(&base);
        apfs::set_readonly(&base)?;
        let work = s.0.join("work");
        apfs::snapshot(&base, &work, false)?;
        change(&work);
        let layer = s.0.join("layer");
        let d = snapshot_over(&Layered::complete(&base), "base-id", &work, &layer, false)?.expect("changes");
        assert!(!d.write.contains("a/same"), "unchanged clones are not copied: {d:?}");
        assert!(d.removed.contains("gone") && !d.removed.contains("gone/inner"));
        assert!(d.removed.contains("swap-file") && d.removed.contains("swap-dir"));
        assert_eq!(apfs::subvol_info(&layer)?.base.as_deref(), Some("base-id"));
        let layered = Layered::over(&layer, Layered::complete(&base))?;
        check_same(&layered, &work);
        assert!(layered.find("gone/inner/file")?.is_none());
        assert!(layered.find("linkdir/inner")?.is_none(), "a path below a new link is absent");
        assert!(layer.join("quiet").is_dir() && d.removed.contains("quiet/one"));
        let some = s.0.join("some");
        extract(&layered, ["linkdir", "linkdir/inner", "a/edit", "gone/inner/file"].into_iter(), &some)?;
        assert!(fs::symlink_metadata(some.join("linkdir"))?.file_type().is_symlink());
        assert_eq!(fs::read(some.join("a/edit"))?, b"new bytes");
        assert!(!some.join("gone").exists());
        assert!(layered.find("swap-dir/child")?.is_none());
        assert_eq!(fs::read(layered.path("a/same")?)?, b"same");
        assert!(layered.path("a/same")?.starts_with(&base));
        assert!(layered.path("a/edit")?.starts_with(&layer));
        let out = s.0.join("out");
        layered.materialize(&out, false)?;
        check_same(&Layered::complete(&out), &work);
        assert!(linked(&base) && linked(&work) && linked(&out), "hard links survive");
        let (qw, qo) = (fs::metadata(work.join("quiet"))?, fs::metadata(out.join("quiet"))?);
        assert_eq!((qw.mtime(), qw.mtime_nsec()), (qo.mtime(), qo.mtime_nsec()), "folder time of a removal");
        assert_eq!(fs::symlink_metadata(out.join("b"))?.mode() & 0o777, 0o750);
        fs::write(out.join("a/same"), b"writable")?;
        assert_eq!(fs::read(base.join("a/same"))?, b"same");
        // A changed file with two names needs a complete tree.
        let work3 = s.0.join("work3");
        layered.materialize(&work3, false)?;
        fs::write(work3.join("b/linked"), b"both names")?;
        let e = snapshot_over(&layered, "layer-id", &work3, &s.0.join("refused"), false).unwrap_err();
        assert!(needs_complete(&e) && !s.0.join("refused").exists());
        // A second layer over the first one.
        let work2 = s.0.join("work2");
        layered.materialize(&work2, false)?;
        fs::remove_file(work2.join("a/new"))?;
        fs::write(work2.join("gone"), b"back as a file")?;
        let layer2 = s.0.join("layer2");
        snapshot_over(&layered, "layer-id", &work2, &layer2, false)?.expect("changes");
        let two = Layered::over(&layer2, layered.clone())?;
        check_same(&two, &work2);
        let out2 = s.0.join("out2");
        two.materialize(&out2, true)?;
        check_same(&Layered::complete(&out2), &work2);
        // No change gives no layer.
        assert!(snapshot_over(&two, "x", &work2, &s.0.join("none"), false)?.is_none());
        assert!(!s.0.join("none").exists());
        Ok(())
    }
}
