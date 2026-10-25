//! File system helpers: metadata, content compare, reflink copies, walks and safe paths.
//!
//! Lines are writable by their owners. Every write into a line checks that the parent folders of the
//! target are real folders inside the root, so a symbolic link in the line cannot redirect a write
//! of the service (which runs as root) outside the line.

use anyhow::{anyhow, bail, Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FType {
    File,
    Dir,
    Symlink,
    Special,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[allow(dead_code)]
pub struct Meta {
    pub ftype: FType,
    pub mode: u32,
    pub size: u64,
    pub ino: u64,
    pub uid: u32,
    pub gid: u32,
    pub mtime: i64,
}

impl Meta {
    pub fn is_file(&self) -> bool {
        self.ftype == FType::File
    }
    pub fn is_dir(&self) -> bool {
        self.ftype == FType::Dir
    }
    pub fn executable(&self) -> bool {
        self.mode & 0o111 != 0
    }
    /// The git mode of this entry: 100644, 100755, 120000 or 040000.
    pub fn git_mode(&self) -> u32 {
        match self.ftype {
            FType::Symlink => 0o120000,
            FType::Dir => 0o040000,
            _ if self.executable() => 0o100755,
            _ => 0o100644,
        }
    }
}

pub fn meta_of(m: &fs::Metadata) -> Meta {
    let ft = m.file_type();
    let ftype = if ft.is_symlink() {
        FType::Symlink
    } else if ft.is_dir() {
        FType::Dir
    } else if ft.is_file() {
        FType::File
    } else {
        FType::Special
    };
    Meta { ftype, mode: m.mode() & 0o7777, size: m.len(), ino: m.ino(), uid: m.uid(), gid: m.gid(), mtime: m.mtime() }
}

pub use layr_store::paths::check_rel;

/// Join `rel` to `root` after checking that every parent folder is a real folder (not a link).
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    check_rel(rel)?;
    let mut cur = root.to_path_buf();
    let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
    for (i, c) in comps.iter().enumerate() {
        cur.push(c);
        if i + 1 < comps.len() {
            match fs::symlink_metadata(&cur) {
                Ok(m) if m.file_type().is_symlink() => bail!("path goes through a symbolic link: {rel}"),
                _ => {}
            }
        }
    }
    Ok(cur)
}

/// A tree to read: a folder, or on APFS a thin state over its base (`thin::Layered`).
pub trait Src {
    /// The path to read `rel` from; it may not exist. Fails for a path through a link.
    fn at(&self, rel: &str) -> Result<PathBuf>;
    /// The folder, when the tree is one complete folder.
    fn folder(&self) -> Option<&Path>;
    /// The layers, when the tree is not one folder.
    #[cfg(target_os = "macos")]
    fn layered(&self) -> Option<&crate::thin::Layered> {
        None
    }
}
impl Src for Path {
    fn at(&self, rel: &str) -> Result<PathBuf> {
        safe_join(self, rel)
    }
    fn folder(&self) -> Option<&Path> {
        Some(self)
    }
}
impl Src for PathBuf {
    fn at(&self, rel: &str) -> Result<PathBuf> {
        safe_join(self, rel)
    }
    fn folder(&self) -> Option<&Path> {
        Some(self)
    }
}
#[cfg(target_os = "macos")]
impl Src for crate::thin::Layered {
    fn at(&self, rel: &str) -> Result<PathBuf> {
        self.path(rel)
    }
    fn folder(&self) -> Option<&Path> {
        self.is_complete().then(|| self.bottom())
    }
    fn layered(&self) -> Option<&crate::thin::Layered> {
        Some(self)
    }
}

/// On a volume that ignores case: the stored name of another entry that `path` finds, such as
/// `README` for `readme`. `None` when `path` is free or names exactly that entry.
#[cfg(any(target_os = "macos", feature = "overlayfs"))]
pub fn case_clash(path: &Path) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let name = path.file_name()?;
        let m = fs::symlink_metadata(path).ok()?;
        if crate::sys::case_sensitive_dev(m.dev(), path) {
            return None;
        }
        // A file with several links: the folder listing names the entries reliably.
        if !m.is_dir() && m.nlink() > 1 {
            let mut other = None;
            for e in fs::read_dir(path.parent()?).ok()?.flatten() {
                let n = e.file_name();
                if n == name {
                    return None;
                }
                if case_key(&n.to_string_lossy()) == case_key(&name.to_string_lossy()) {
                    other = Some(n.to_string_lossy().into_owned());
                }
            }
            return other;
        }
        let stored = crate::sys::stored_name(path)?;
        (stored != name).then(|| stored.to_string_lossy().into_owned())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        None
    }
}

/// Refuse to write `rel` below `root` when the volume ignores case and another entry (or a
/// folder on the way) has the same name in another case: one would replace the other.
pub fn refuse_case_clash(root: &Path, rel: &str) -> Result<()> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (root, rel);
        Ok(())
    }
    #[cfg(target_os = "macos")]
    refuse_case_clash_apfs(root, rel)
}

#[cfg(target_os = "macos")]
fn refuse_case_clash_apfs(root: &Path, rel: &str) -> Result<()> {
    if fs::symlink_metadata(root).map(|m| crate::sys::case_sensitive_dev(m.dev(), root)).unwrap_or(true) {
        return Ok(());
    }
    let mut path = root.to_path_buf();
    let mut done = String::new();
    for part in rel.split('/').filter(|c| !c.is_empty()) {
        path.push(part);
        if !done.is_empty() {
            done.push('/');
        }
        done.push_str(part);
        if let Some(stored) = case_clash(&path) {
            // A folder with another case that holds no files (removals can leave empty
            // folders) holds nothing: it leaves.
            let other = path.with_file_name(&stored);
            if holds_no_files(&other, "", &|_| false) {
                remove_empty_folders(&other)?;
                continue;
            }
            let parent = done.rsplit_once('/').map(|(p, _)| format!("{p}/")).unwrap_or_default();
            bail!(
                "{done} and {parent}{stored} differ only in case, and the volume of this store does not tell them apart.\nRename one of them, or use a store on a case-sensitive APFS volume."
            );
        }
        if fs::symlink_metadata(&path).is_err() {
            break;
        }
    }
    Ok(())
}

/// The form of a name that a volume that ignores case compares: APFS also ignores the Unicode
/// normal form (NFC or NFD). Each character is folded alone, as Unicode case folding does;
/// `str::to_lowercase` maps a final sigma by its position. A key that is too coarse only
/// refuses more names; a key that is too fine would lose a file.
#[cfg(target_os = "macos")]
pub fn case_key(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let fold = |c: char| match c {
        'ς' => 'σ',
        'ſ' => 's',
        'ϐ' => 'β',
        'ϑ' => 'θ',
        'ϕ' => 'φ',
        'ϖ' => 'π',
        'ϰ' => 'κ',
        'ϱ' => 'ρ',
        'ϵ' => 'ε',
        c => c,
    };
    name.nfd().flat_map(char::to_lowercase).map(fold).nfd().collect()
}

/// True when the entry `rel` below `root` holds no files: it leaves (`leaves(rel)`), or it is a
/// folder whose entries all hold no files.
#[cfg(target_os = "macos")]
fn holds_no_files(root: &Path, rel: &str, leaves: &dyn Fn(&str) -> bool) -> bool {
    if leaves(rel) {
        return true;
    }
    let p = root.join(rel);
    fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir())
        && fs::read_dir(&p).is_ok_and(|rd| {
            rd.into_iter().all(|e| {
                e.is_ok_and(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    holds_no_files(root, &if rel.is_empty() { name } else { format!("{rel}/{name}") }, leaves)
                })
            })
        })
}

#[cfg(target_os = "macos")]
fn remove_empty_folders(path: &Path) -> Result<()> {
    crate::sys::apfs_immutable(path, false)?;
    for e in fs::read_dir(path)? {
        remove_empty_folders(&e?.path())?;
    }
    fs::remove_dir(path)?;
    Ok(())
}

/// Remove `gone` from `dst`, then copy `write` from `src` to the same paths.
/// Removals go first: on a volume that ignores case, `readme` must leave before `README` comes.
/// A clash that the removals do not clear (an untracked `dir/x` where `Dir/x` comes) is refused
/// before anything changes.
pub fn replace_entries(src: &Path, dst: &Path, gone: &[&str], write: &[&str], own: Option<(u32, u32)>) -> Result<()> {
    #[cfg(target_os = "macos")]
    refuse_remaining_clashes(dst, gone, write)?;
    for g in gone {
        remove_entry(dst, g)?;
    }
    for w in write {
        copy_entry(src, w, dst, w, own)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn refuse_remaining_clashes(dst: &Path, gone: &[&str], write: &[&str]) -> Result<()> {
    if fs::symlink_metadata(dst).map(|m| crate::sys::case_sensitive_dev(m.dev(), dst)).unwrap_or(true) {
        return Ok(());
    }
    let gone: std::collections::BTreeSet<&str> = gone.iter().copied().collect();
    let leaves = |rel: &str| rel.match_indices('/').map(|(i, _)| &rel[..i]).chain([rel]).any(|a| gone.contains(a));
    // Writes below one folder share its ancestors: check each once.
    let mut checked: std::collections::HashSet<String> = Default::default();
    for w in write {
        let mut done = String::new();
        for part in w.split('/').filter(|c| !c.is_empty()) {
            let parent = done.clone();
            if !done.is_empty() {
                done.push('/');
            }
            done.push_str(part);
            if !checked.insert(done.clone()) {
                continue;
            }
            let path = dst.join(&done);
            if let Some(stored) = case_clash(&path) {
                let other = if parent.is_empty() { stored } else { format!("{parent}/{stored}") };
                // After the removals, nothing but empty folders may stay there.
                if !holds_no_files(dst, &other, &leaves) {
                    bail!(
                        "{done} and {other} differ only in case, and the volume of this store does not tell them apart.\nRename one of them, or use a store on a case-sensitive APFS volume."
                    );
                }
                break;
            }
            if fs::symlink_metadata(&path).is_err() {
                break;
            }
        }
    }
    Ok(())
}

/// Refuse a set of paths that a volume that ignores case would store as fewer entries, such
/// as `README` and `Readme`, when `dst` is on such a volume. Ancestors count too.
pub fn refuse_case_collisions<'a>(dst: &Path, paths: impl Iterator<Item = &'a str>) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        if crate::sys::case_sensitive(dst) {
            return Ok(());
        }
        let mut seen: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> = Default::default();
        for p in paths {
            let mut prefix = String::new();
            for part in p.split('/').filter(|c| !c.is_empty()) {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(part);
                seen.entry(case_key(&prefix)).or_default().insert(prefix.clone());
            }
        }
        let groups: Vec<String> =
            seen.into_values().filter(|v| v.len() > 1).map(|v| v.into_iter().collect::<Vec<_>>().join(", ")).collect();
        if !groups.is_empty() {
            let shown: Vec<String> = groups.iter().take(10).map(|g| format!("  {g}")).collect();
            bail!(
                "these names differ only in case, and the volume of this store does not tell them apart:\n{}{}\nRename all but one of each group, or use a store on a case-sensitive APFS volume.",
                shown.join("\n"),
                if groups.len() > 10 { format!("\n  and {} more", groups.len() - 10) } else { String::new() }
            );
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (dst, paths.count());
        Ok(())
    }
}

/// Metadata of `rel` below `root` without following a final link. `None` if absent.
pub fn lmeta(root: &(impl Src + ?Sized), rel: &str) -> Option<Meta> {
    let p = root.at(rel).ok()?;
    exact_metadata(&p, rel).map(|m| meta_of(&m))
}

/// Metadata of the entry at `path`, which is `rel` below some root, only when the volume
/// stores exactly that name. On a volume that ignores case, `readme` also finds `README`;
/// for Layr these are two paths.
pub fn exact_metadata(path: &Path, rel: &str) -> Option<fs::Metadata> {
    let m = fs::symlink_metadata(path).ok()?;
    #[cfg(target_os = "macos")]
    if !rel.is_empty() && !crate::sys::case_sensitive_dev(m.dev(), path) && !stored_exactly(path, rel, &m) {
        return None;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = rel;
    Some(m)
}

/// True when the listing of folder `dir` has exactly `name`. The last listing is kept per
/// thread while the folder stays the same: status checks each hard-linked file of a folder.
#[cfg(target_os = "macos")]
fn listed(dir: &Path, name: &std::ffi::OsStr) -> bool {
    use std::cell::RefCell;
    use std::collections::HashSet;
    type Listing = (PathBuf, (u64, u64, i64, i64), HashSet<std::ffi::OsString>);
    thread_local!(static LAST: RefCell<Option<Listing>> = const { RefCell::new(None) });
    let Ok(m) = fs::symlink_metadata(dir) else {
        return false;
    };
    // A new, removed or renamed entry changes the ctime of the folder; tools can set its mtime
    // back, but not its ctime.
    let key = (m.dev(), m.ino(), m.ctime(), m.ctime_nsec());
    LAST.with_borrow_mut(|last| {
        if !last.as_ref().is_some_and(|(d, k, _)| d == dir && *k == key) {
            *last = None;
            // A listing with an error is not kept.
            let names = fs::read_dir(dir).and_then(|rd| rd.map(|e| e.map(|e| e.file_name())).collect::<std::io::Result<_>>());
            let Ok(names) = names else {
                return false;
            };
            *last = Some((dir.to_path_buf(), key, names));
        }
        last.as_ref().is_some_and(|(_, _, names)| names.contains(name))
    })
}

/// True when the volume stores the last `rel` components of `path` with exactly these names.
#[cfg(target_os = "macos")]
fn stored_exactly(path: &Path, rel: &str, m: &fs::Metadata) -> bool {
    // The kernel names a file with several links by one of them, maybe another one: check the
    // name in the listing of its folder, and the folder by its own stored path.
    if !m.is_dir() && m.nlink() > 1 {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return false;
        };
        return listed(parent, name)
            && rel.rsplit_once('/').is_none_or(|(dir, _)| {
                stored_exactly(parent, dir, &fs::symlink_metadata(parent).ok().unwrap_or_else(|| m.clone()))
            });
    }
    match crate::sys::stored_path(path) {
        Some(stored) => {
            let stored = stored.to_string_lossy();
            stored.ends_with(&format!("/{rel}")) || *stored == *rel
        }
        // Without the full path: compare each name.
        None => {
            let mut at = path.to_path_buf();
            for part in rel.rsplit('/') {
                if crate::sys::stored_name(&at).is_none_or(|n| n != part) {
                    return false;
                }
                at.pop();
            }
            true
        }
    }
}

pub fn read_link(root: &(impl Src + ?Sized), rel: &str) -> Result<String> {
    let p = root.at(rel)?;
    Ok(fs::read_link(&p).with_context(|| format!("readlink {}", p.display()))?.to_string_lossy().into_owned())
}

/// Files larger than this are binary for diff, grep, blame, rename detection and merge: their
/// content is never read into memory as a whole.
pub const MAX_TEXT: u64 = 64 << 20;

fn open_regular(p: &Path) -> Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(p)
        .with_context(|| format!("open {}", p.display()))?;
    if !f.metadata()?.is_file() {
        bail!("{} is not a regular file", p.display());
    }
    Ok(f)
}

/// Read a regular file (not following links) of at most `MAX_TEXT` bytes.
pub fn read_file(root: &(impl Src + ?Sized), rel: &str) -> Result<Vec<u8>> {
    let p = root.at(rel)?;
    let f = open_regular(&p)?;
    let len = f.metadata()?.len();
    let mut v = Vec::with_capacity(len.min(MAX_TEXT) as usize);
    f.take(MAX_TEXT + 1).read_to_end(&mut v)?;
    if len > MAX_TEXT || v.len() as u64 > MAX_TEXT {
        bail!("{} is larger than {} MiB", p.display(), MAX_TEXT >> 20);
    }
    Ok(v)
}

/// Binary by the git rule (a NUL byte in the first 8000 bytes), or larger than `MAX_TEXT`.
/// Reads at most 8000 bytes. Not a regular file: false.
pub fn is_binary_file(root: &(impl Src + ?Sized), rel: &str) -> bool {
    let f = match root.at(rel).and_then(|p| open_regular(&p)) {
        Ok(f) => f,
        Err(_) => return false,
    };
    if f.metadata().map(|m| m.len() > MAX_TEXT).unwrap_or(false) {
        return true;
    }
    let mut v = Vec::new();
    f.take(8000).read_to_end(&mut v).is_ok() && v.contains(&0)
}

/// Copy a regular file (not following links) into a new private file `dst`, in pieces.
pub fn copy_regular(src: &Path, dst: &Path) -> Result<()> {
    let mut f = open_regular(src)?;
    let mut out = OpenOptions::new().write(true).create_new(true).mode(0o600).open(dst)?;
    std::io::copy(&mut f, &mut out)?;
    Ok(())
}

/// Copy a regular file in pieces to `out`.
pub fn copy_file_to(root: &(impl Src + ?Sized), rel: &str, out: &mut dyn std::io::Write) -> Result<()> {
    let mut f = open_regular(&root.at(rel)?)?;
    std::io::copy(&mut f, out)?;
    Ok(())
}

/// The bytes of an entry for diffs: file content, or the link target for a symbolic link.
pub fn entry_bytes(root: &(impl Src + ?Sized), rel: &str) -> Result<Vec<u8>> {
    match lmeta(root, rel) {
        Some(m) if m.ftype == FType::Symlink => Ok(read_link(root, rel)?.into_bytes()),
        Some(m) if m.ftype == FType::File => read_file(root, rel),
        Some(_) => Ok(Vec::new()),
        None => Ok(Vec::new()),
    }
}

fn open_ro(p: &Path) -> Result<File> {
    OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(p).with_context(|| format!("open {}", p.display()))
}

fn same_range(a: &mut File, b: &mut File, off: u64, len: u64) -> Result<bool> {
    a.seek(SeekFrom::Start(off))?;
    b.seek(SeekFrom::Start(off))?;
    let mut left = len;
    let mut ba = vec![0u8; 1 << 16];
    let mut bb = vec![0u8; 1 << 16];
    while left > 0 {
        let want = left.min(ba.len() as u64) as usize;
        let na = read_full(a, &mut ba[..want])?;
        let nb = read_full(b, &mut bb[..want])?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na < want {
            break;
        }
        left -= na as u64;
    }
    Ok(true)
}

fn read_full(f: &mut File, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let r = f.read(&mut buf[n..])?;
        if r == 0 {
            break;
        }
        n += r;
    }
    Ok(n)
}

/// Compare two entries. With `ranges`, only those byte ranges of two regular files can differ
/// (the rest share extents), so only they are read.
pub fn same_entry(
    ra: &(impl Src + ?Sized),
    pa: &str,
    rb: &(impl Src + ?Sized),
    pb: &str,
    ranges: Option<&[(u64, u64)]>,
) -> Result<bool> {
    let (ma, mb) = match (lmeta(ra, pa), lmeta(rb, pb)) {
        (Some(a), Some(b)) => (a, b),
        (None, None) => return Ok(true),
        _ => return Ok(false),
    };
    if ma.ftype != mb.ftype {
        return Ok(false);
    }
    match ma.ftype {
        FType::Symlink => Ok(read_link(ra, pa)? == read_link(rb, pb)?),
        FType::Dir | FType::Special => Ok(true),
        FType::File => {
            if ma.size != mb.size {
                return Ok(false);
            }
            let mut a = open_ro(&ra.at(pa)?)?;
            let mut b = open_ro(&rb.at(pb)?)?;
            match ranges {
                Some(rs) => {
                    for (off, len) in rs {
                        if *off >= ma.size {
                            continue;
                        }
                        let l = (*len).min(ma.size - off);
                        if !same_range(&mut a, &mut b, *off, l)? {
                            return Ok(false);
                        }
                    }
                    Ok(true)
                }
                None => same_range(&mut a, &mut b, 0, ma.size),
            }
        }
    }
}

/// Create the parent folders of `rel` below `root`, with owner `own`.
///
/// All writes below a root go through `Tree` (descriptor based, no link is followed), because a
/// line belongs to its owner while the service writes as root.
pub fn make_parents(root: &Path, rel: &str, own: Option<(u32, u32)>) -> Result<()> {
    crate::tree::Tree::open(root)?.mkdirs_for(rel, own)
}

pub fn chown_nofollow(p: &Path, uid: u32, gid: u32) -> Result<()> {
    let r = rustix::fs::chownat(
        rustix::fs::CWD,
        p,
        Some(rustix::fs::Uid::from_raw(uid)),
        Some(rustix::fs::Gid::from_raw(gid)),
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    );
    match r {
        Ok(()) => Ok(()),
        // Without root, a chown to oneself fails harmlessly.
        Err(rustix::io::Errno::PERM) if !rustix::process::geteuid().is_root() => Ok(()),
        Err(e) => Err(anyhow!("chown {}: {e}", p.display())),
    }
}

/// Remove an entry (recursively for a folder; nested subvolumes are destroyed).
pub fn remove_entry(root: &Path, rel: &str) -> Result<()> {
    if rel.is_empty() {
        bail!("refusing to remove the root of {}", root.display());
    }
    // On a volume that ignores case, `readme` also finds `README`: remove only this name.
    if let Ok(p) = safe_join(root, rel) {
        if fs::symlink_metadata(&p).is_ok() && exact_metadata(&p, rel).is_none() {
            return Ok(());
        }
    }
    crate::tree::Tree::open(root)?.remove(rel)
}

fn fchown(f: &File, own: Option<(u32, u32)>) -> Result<()> {
    if let Some((u, g)) = own {
        rustix::fs::fchown(f, Some(rustix::fs::Uid::from_raw(u)), Some(rustix::fs::Gid::from_raw(g)))?;
    }
    Ok(())
}

/// Copy one entry from `src_root/src_rel` (a snapshot or a private folder) to
/// `dst_root/dst_rel`: a reflink copy for a file (in place when a file is there, so the inode
/// stays), the same target for a link, a folder for a folder.
pub fn copy_entry(
    src_root: &(impl Src + ?Sized),
    src_rel: &str,
    dst_root: &Path,
    dst_rel: &str,
    own: Option<(u32, u32)>,
) -> Result<()> {
    copy_entry_impl(src_root, src_rel, dst_root, dst_rel, own, false)
}

/// Copy into a private index. APFS can replace a single-link inode with a new clone.
pub fn copy_index_entry(
    src_root: &(impl Src + ?Sized),
    src_rel: &str,
    dst_root: &Path,
    dst_rel: &str,
    own: Option<(u32, u32)>,
) -> Result<()> {
    copy_entry_impl(src_root, src_rel, dst_root, dst_rel, own, true)
}

fn copy_entry_impl(
    src_root: &(impl Src + ?Sized),
    src_rel: &str,
    dst_root: &Path,
    dst_rel: &str,
    own: Option<(u32, u32)>,
    _replace_index: bool,
) -> Result<()> {
    let src = src_root.at(src_rel)?;
    let m = fs::symlink_metadata(&src).with_context(|| format!("stat {}", src.display()))?;
    refuse_case_clash(dst_root, dst_rel)?;
    let t = crate::tree::Tree::open(dst_root)?;
    t.mkdirs_for(dst_rel, own)?;
    let ft = m.file_type();
    if ft.is_symlink() {
        let target = fs::read_link(&src)?.to_string_lossy().into_owned();
        t.symlink(dst_rel, &target)?;
        if let Some((u, g)) = own {
            t.chown(dst_rel, u, g)?;
        }
        return Ok(());
    }
    if ft.is_dir() {
        t.mkdirs(dst_rel, own)?;
        t.chmod(dst_rel, m.mode() & 0o7777)?;
        return Ok(());
    }
    if !ft.is_file() {
        bail!("cannot copy special file {}", src.display());
    }
    let s = open_ro(&src)?;
    let finish = |d: &File| -> Result<()> {
        d.set_permissions(fs::Permissions::from_mode(m.mode() & 0o7777))?;
        set_mtime(d, m.mtime(), m.mtime_nsec())?;
        fchown(d, own)
    };
    #[cfg(target_os = "macos")]
    if _replace_index && t.replace_index_file(&s, dst_rel, finish)? {
        return Ok(());
    }
    // Keep the inode of a file that is there: btrfs then reports an in-place change with byte
    // ranges, and running tools keep their file handles.
    let (d, cloned) = match t.open_write(dst_rel)? {
        Some(d) => {
            d.set_len(0)?;
            (d, false)
        }
        None => {
            #[cfg(target_os = "macos")]
            let cloned = t.clone_new_file(&s, dst_rel)?;
            #[cfg(target_os = "linux")]
            let cloned: Option<File> = None;
            match cloned {
                Some(d) => (d, true),
                None => (t.create(dst_rel, m.mode() & 0o7777)?, false),
            }
        }
    };
    if !cloned && crate::storage::clone_file(&s, &d).is_err() {
        let mut s2 = open_ro(&src)?;
        let mut d2 = d.try_clone()?;
        std::io::copy(&mut s2, &mut d2)?;
    }
    finish(&d)
}

pub fn set_mtime(f: &File, sec: i64, nsec: i64) -> Result<()> {
    let ts = rustix::fs::Timestamps {
        last_access: rustix::fs::Timespec { tv_sec: 0, tv_nsec: rustix::fs::UTIME_OMIT },
        last_modification: rustix::fs::Timespec { tv_sec: sec, tv_nsec: nsec as _ },
    };
    rustix::fs::futimens(f, &ts)?;
    Ok(())
}

/// Write bytes as a new file at `root/rel` (replacing what is there), with a mode and owner.
pub fn write_file(root: &Path, rel: &str, data: &[u8], mode: u32, own: Option<(u32, u32)>) -> Result<()> {
    write_file_from(root, rel, &mut &data[..], mode, own)
}

/// `write_file` with the content read from `src` in pieces.
pub fn write_file_from(root: &Path, rel: &str, src: &mut dyn Read, mode: u32, own: Option<(u32, u32)>) -> Result<()> {
    let t = crate::tree::Tree::open(root)?;
    t.mkdirs_for(rel, own)?;
    let mut f = t.create(rel, mode)?;
    std::io::copy(src, &mut f)?;
    f.set_permissions(fs::Permissions::from_mode(mode))?;
    fchown(&f, own)?;
    Ok(())
}

pub fn set_mode(root: &Path, rel: &str, mode: u32) -> Result<()> {
    crate::tree::Tree::open(root)?.chmod(rel, mode)
}

/// Walk all entries below `root/rel` (not following links, not entering nested subvolumes).
/// Paths are relative to `root`. Folders are reported before their content.
pub fn walk(root: &(impl Src + ?Sized), rel: &str, f: &mut dyn FnMut(&str, &Meta) -> Result<bool>) -> Result<()> {
    match root.folder() {
        Some(dir) => walk_folder(dir, rel, f),
        #[cfg(target_os = "macos")]
        None => {
            let layered = root.layered().expect("a tree that is not a folder has layers");
            // Entries come sorted by path, so the content of a folder follows it.
            let mut skip: Option<String> = None;
            for (path, m) in layered.entries(rel)? {
                if skip.as_ref().is_some_and(|s| path.starts_with(s.as_str())) {
                    continue;
                }
                skip = None;
                if !f(&path, &m)? && m.is_dir() {
                    skip = Some(format!("{path}/"));
                }
            }
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        None => unreachable!("only APFS has layered trees"),
    }
}

fn walk_folder(root: &Path, rel: &str, f: &mut dyn FnMut(&str, &Meta) -> Result<bool>) -> Result<()> {
    let start = safe_join(root, rel)?;
    let dev = match fs::symlink_metadata(&start) {
        Ok(m) => m.dev(),
        Err(_) => return Ok(()),
    };
    let mut stack = vec![rel.to_string()];
    while let Some(dir) = stack.pop() {
        let p = if dir.is_empty() { root.to_path_buf() } else { root.join(&dir) };
        let rd = match fs::read_dir(&p) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        let mut subdirs = Vec::new();
        for n in names {
            let child = if dir.is_empty() { n.clone() } else { format!("{dir}/{n}") };
            let m = match fs::symlink_metadata(root.join(&child)) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let meta = meta_of(&m);
            if meta.is_dir()
                && (m.dev() != dev
                    || ((cfg!(target_os = "macos") || cfg!(feature = "overlayfs"))
                        && crate::storage::is_subvolume(&root.join(&child))))
            {
                continue;
            }
            let descend = f(&child, &meta)?;
            if meta.is_dir() && descend {
                subdirs.push(child);
            }
        }
        for s in subdirs.into_iter().rev() {
            stack.push(s);
        }
    }
    Ok(())
}

/// Write `data` to `path` atomically: a temporary file, `fsync`, rename, `fsync` of the folder.
pub fn atomic_write(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().ok_or_else(|| anyhow!("no parent"))?;
    // A new file with an unpredictable name, never through an existing name or a link.
    let tmp = dir.join(format!(
        ".{}.tmp-{}-{:016x}",
        path.file_name().unwrap().to_string_lossy(),
        std::process::id(),
        rand::random::<u64>()
    ));
    {
        let mut f = OpenOptions::new().write(true).create_new(true).custom_flags(libc::O_NOFOLLOW).mode(mode).open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// The git blob id of some bytes.
pub fn git_blob_id(data: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(format!("blob {}\0", data.len()).as_bytes());
    h.update(data);
    hex::encode(h.finalize())
}

/// The git blob id of a regular file, read in pieces (any size).
pub fn git_blob_id_file(root: &(impl Src + ?Sized), rel: &str) -> Result<String> {
    use sha1::{Digest, Sha1};
    let mut f = open_regular(&root.at(rel)?)?;
    let len = f.metadata()?.len();
    let mut h = Sha1::new();
    h.update(format!("blob {len}\0").as_bytes());
    let mut buf = vec![0u8; 1 << 20];
    let mut n_total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        n_total += n as u64;
        h.update(&buf[..n]);
    }
    if n_total != len {
        bail!("{rel} changed while it was read");
    }
    Ok(hex::encode(h.finalize()))
}

/// Binary content by the git rule: a NUL byte in the first 8000 bytes.
pub fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(8000)].contains(&0)
}

// ----- writes into folders owned by users, through file descriptors -----

/// Open a folder without following a link at its last component.
pub fn open_dir_nofollow(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open folder {}", path.display()))
}

/// Open a regular file `name` of an open folder for reading: never a link, a device or a pipe.
pub fn open_file_at(dir: &File, name: &str) -> Result<File> {
    use rustix::fs::{Mode, OFlags};
    let fd = rustix::fs::openat(dir, name, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK, Mode::empty())
        .with_context(|| format!("open {name}"))?;
    let f = File::from(fd);
    if !f.metadata()?.is_file() {
        bail!("{name} is not a regular file");
    }
    Ok(f)
}

/// Read a regular file `name` of an open folder; `None` if absent, a link or not a file.
pub fn read_at(dir: &File, name: &str) -> Option<Vec<u8>> {
    use rustix::fs::{Mode, OFlags};
    let fd = rustix::fs::openat(dir, name, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK, Mode::empty())
        .ok()?;
    let mut f = File::from(fd);
    if !f.metadata().ok()?.is_file() {
        return None;
    }
    let mut v = Vec::new();
    f.read_to_end(&mut v).ok()?;
    Some(v)
}

/// Replace `name` in an open folder: write a new file (never through an existing name), then
/// rename it over `name`. A link at `name` is replaced, not followed.
pub fn replace_at(dir: &File, name: &str, content: &mut dyn FnMut(&File) -> Result<()>, own: Option<(u32, u32)>) -> Result<()> {
    use rustix::fs::{AtFlags, Mode, OFlags};
    let tmp = format!(".{name}.layr-{}-{:016x}", std::process::id(), rand::random::<u64>());
    let _ = rustix::fs::unlinkat(dir, tmp.as_str(), AtFlags::empty());
    let fd = rustix::fs::openat(
        dir,
        tmp.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o644),
    )
    .map_err(|e| anyhow!("create {tmp}: {e}"))?;
    let f = File::from(fd);
    let r = (|| -> Result<()> {
        content(&f)?;
        fchown(&f, own)?;
        rustix::fs::renameat(dir, tmp.as_str(), dir, name).map_err(|e| anyhow!("rename to {name}: {e}"))?;
        Ok(())
    })();
    if r.is_err() {
        let _ = rustix::fs::unlinkat(dir, tmp.as_str(), AtFlags::empty());
    }
    r
}

/// Remove `name` from an open folder (a link is removed, not followed).
pub fn unlink_at(dir: &File, name: &str) {
    let _ = rustix::fs::unlinkat(dir, name, rustix::fs::AtFlags::empty());
}

/// Whether a known directory is a nested storage root.
pub fn is_layer(root: &Path, rel: &str, m: &Meta) -> bool {
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    {
        let _ = (root, rel);
        m.is_dir() && m.ino == 256
    }
    #[cfg(any(target_os = "macos", feature = "overlayfs"))]
    {
        m.is_dir() && crate::storage::is_subvolume(&root.join(rel))
    }
}

/// Move a managed tree, including its mount when the backend requires it.
pub fn rename_tree(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<()> {
    #[cfg(feature = "overlayfs")]
    return crate::storage::rename_tree(from.as_ref(), to.as_ref());
    #[cfg(not(feature = "overlayfs"))]
    Ok(fs::rename(from, to)?)
}
