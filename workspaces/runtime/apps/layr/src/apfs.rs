//! APFS local-user backend. Clones data extents, scans directories, and rejects source changes.
//! Snapshots use verified directory clones; transport uses the compatible btrfs stream format.
use crate::fsutil::Src;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubvolInfo {
    pub uuid: String,
    pub received_uuid: Option<String>,
    pub parent_uuid: Option<String>,
    pub readonly: bool,
    /// The base state of a thin layer (`deltas/<id>`). A complete tree has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}
pub fn is_supported(path: &Path) -> bool {
    crate::sys::apfs_supported(path)
}
pub fn is_subvolume(path: &Path) -> bool {
    path.is_dir() && subvol_info(path).is_ok()
}
pub fn subvol_info(path: &Path) -> Result<SubvolInfo> {
    Ok(serde_json::from_slice(&crate::sys::apfs_marker(path, None)?)?)
}
fn mark(path: &Path, parent: Option<String>, readonly: bool) -> Result<()> {
    write_info(path, &SubvolInfo { uuid: crate::ids::new_id(), received_uuid: None, parent_uuid: parent, readonly, base: None })
}
/// Write the marker of a tree root. The root may be read-only for its owner.
pub fn write_info(path: &Path, info: &SubvolInfo) -> Result<()> {
    let mode = fs::symlink_metadata(path)?.mode() & 0o7777;
    if mode & 0o200 == 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o200))?;
    }
    let marked = crate::sys::apfs_marker(path, Some(&serde_json::to_vec(&info)?));
    if mode & 0o200 == 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    marked?;
    Ok(())
}
fn update_readonly(path: &Path, readonly: bool) -> Result<()> {
    let mut info = subvol_info(path)?;
    // Keep the snapshot identity: signed records refer to this UUID.
    crate::sys::apfs_immutable(path, false)?;
    let mode = fs::symlink_metadata(path)?.mode() & 0o7777;
    fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o200))?;
    info.readonly = readonly;
    let result = crate::sys::apfs_marker(path, Some(&serde_json::to_vec(&info)?));
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    result?;
    Ok(())
}
pub fn set_readonly(path: &Path) -> Result<()> {
    update_readonly(path, true)?;
    if let Err(error) = seal(path, true, false) {
        // Never advertise a partially sealed tree as immutable.
        let _ = update_readonly(path, false);
        return Err(error);
    }
    Ok(())
}
pub fn set_writable(path: &Path) -> Result<()> {
    seal(path, false, false)?;
    update_readonly(path, false)
}
pub fn create_subvolume(path: &Path) -> Result<()> {
    if !is_supported(path.parent().context("missing parent")?) {
        bail!("layr requires APFS on macOS");
    }
    fs::create_dir(path)?;
    if let Err(e) = mark(path, None, false) {
        let _ = fs::remove_dir(path);
        return Err(e);
    }
    Ok(())
}
// Compare inode, device, permissions, size, mtime and ctime before and after cloning.
// Do not use atime: reading the source can change it.
pub(crate) type Fingerprint = (u64, u64, u32, u64, i64, i64, i64, i64);
#[derive(Clone)]
pub(crate) struct ScanEntry {
    pub(crate) fingerprint: Fingerprint,
    pub(crate) meta: crate::fsutil::Meta,
    pub(crate) data: Option<crate::sys::ApfsDataIdentity>,
    /// The number of names of the inode. Not part of the comparison.
    pub(crate) nlink: u64,
    /// The creation time: the file identity for moves (`file_identity`).
    pub(crate) birth: (i64, i64),
}
impl PartialEq for ScanEntry {
    fn eq(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint && self.data == other.data
    }
}
fn scan(root: &Path) -> Result<BTreeMap<String, ScanEntry>> {
    scan_with_data(root, false)
}

pub(crate) fn scan_with_data(root: &Path, data: bool) -> Result<BTreeMap<String, ScanEntry>> {
    let _t = crate::trace::span("APFS bulk scan");
    let _ = data;
    let mut out = BTreeMap::new();
    let m = fs::symlink_metadata(root)?;
    out.insert(String::new(), entry_of_metadata(&m, root, true)?);
    if !m.is_dir() {
        return Ok(out);
    }
    let dir = crate::fsutil::open_dir_nofollow(root)?;
    scan_bulk(&dir, root, "", &mut out, &mut Vec::new())?;
    Ok(out)
}

fn entry_of_metadata(m: &fs::Metadata, path: &Path, data: bool) -> Result<ScanEntry> {
    use std::os::macos::fs::MetadataExt as MacMetadataExt;
    Ok(ScanEntry {
        fingerprint: (m.dev(), m.ino(), m.mode(), m.len(), m.mtime(), m.mtime_nsec(), m.ctime(), m.ctime_nsec()),
        meta: crate::fsutil::meta_of(m),
        data: if data && m.is_file() { Some(crate::sys::apfs_data_identity(path)?) } else { None },
        nlink: m.nlink(),
        birth: (m.st_birthtime(), m.st_birthtime_nsec()),
    })
}

/// Read a folder and its content with `getattrlistbulk`: one call per folder instead of two per
/// entry. Nested layers (subvolumes) appear as folders without content.
fn scan_bulk(dir: &File, root: &Path, rel: &str, out: &mut BTreeMap<String, ScanEntry>, buf: &mut Vec<u8>) -> Result<()> {
    use std::os::fd::AsFd;
    let entries = match crate::sys::apfs_bulk_read(dir.as_fd(), buf) {
        Ok(v) => v,
        // A folder that a writer removed while the scan ran: its parent shows the change.
        Err(e) if !rel.is_empty() && matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => return Ok(()),
        Err(e) => return Err(anyhow::Error::from(e).context(format!("read folder {}", root.join(rel).display()))),
    };
    for e in entries {
        let name = e.name.into_string().map_err(|_| anyhow::anyhow!("non-UTF-8 file name"))?;
        let next = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        let kind = e.mode & libc::S_IFMT as u32;
        let ftype = match kind {
            k if k == libc::S_IFREG as u32 => crate::fsutil::FType::File,
            k if k == libc::S_IFDIR as u32 => crate::fsutil::FType::Dir,
            k if k == libc::S_IFLNK as u32 => crate::fsutil::FType::Symlink,
            _ => crate::fsutil::FType::Special,
        };
        let is_file = ftype == crate::fsutil::FType::File;
        out.insert(
            next.clone(),
            ScanEntry {
                fingerprint: (e.dev, e.ino, e.mode, e.size, e.mtime.0, e.mtime.1, e.ctime.0, e.ctime.1),
                meta: crate::fsutil::Meta {
                    ftype,
                    mode: e.mode & 0o7777,
                    size: e.size,
                    ino: e.ino,
                    uid: e.uid,
                    gid: e.gid,
                    mtime: e.mtime.0,
                },
                data: is_file.then_some(e.data),
                nlink: e.nlink,
                birth: e.birth,
            },
        );
        if ftype == crate::fsutil::FType::Dir {
            let path = root.join(&next);
            if is_subvolume(&path) {
                continue;
            }
            let fd = match rustix::fs::openat(
                dir,
                name.as_str(),
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            ) {
                Ok(fd) => fd,
                // Removed or replaced after the listing: a scan after the copy sees it.
                Err(e) if matches!(e, rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) => {
                    out.remove(&next);
                    continue;
                }
                Err(e) => return Err(anyhow::Error::from(e).context(format!("open folder {}", path.display()))),
            };
            scan_bulk(&File::from(fd), root, &next, out, buf)?;
        }
    }
    Ok(())
}

/// The previous scan with one `lstat` and one `getattrlist` per entry, for comparison tests.
#[cfg(test)]
pub(crate) fn scan_with_lstat(root: &Path, data: bool) -> Result<BTreeMap<String, ScanEntry>> {
    fn visit(root: &Path, rel: &str, data: bool, out: &mut BTreeMap<String, ScanEntry>) -> Result<()> {
        let path = root.join(rel);
        let m = fs::symlink_metadata(&path)?;
        out.insert(
            rel.into(),
            ScanEntry {
                fingerprint: (m.dev(), m.ino(), m.mode(), m.len(), m.mtime(), m.mtime_nsec(), m.ctime(), m.ctime_nsec()),
                meta: crate::fsutil::meta_of(&m),
                data: if data && m.is_file() { Some(crate::sys::apfs_data_identity(&path)?) } else { None },
                nlink: m.nlink(),
                birth: {
                    use std::os::macos::fs::MetadataExt as MacMetadataExt;
                    (m.st_birthtime(), m.st_birthtime_nsec())
                },
            },
        );
        if m.is_dir() && (rel.is_empty() || !is_subvolume(&path)) {
            for e in fs::read_dir(path)? {
                let e = e?;
                let name = e.file_name().into_string().map_err(|_| anyhow::anyhow!("non-UTF-8 file name"))?;
                let next = if rel.is_empty() { name } else { format!("{rel}/{name}") };
                visit(root, &next, data, out)?;
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    visit(root, "", data, &mut out)?;
    Ok(out)
}

/// Validate a read of live trees with kernel generation counts on both sides.
/// Unknown counts (including mmap) or a concurrent change require a snapshot.
pub fn stable_read<T>(roots: &[&Path], read: impl FnOnce() -> Result<T>) -> Result<Option<T>> {
    let _t = crate::trace::span("APFS validated read");
    let before = roots.iter().map(|root| scan_with_data(root, true)).collect::<Result<Vec<_>>>()?;
    if before
        .iter()
        .any(|tree| tree.values().any(|entry| entry.meta.is_file() && entry.data.and_then(|d| d.generation).is_none()))
    {
        return Ok(None);
    }
    let result = read();
    for (root, expected) in roots.iter().zip(before) {
        // A deleted or replaced path can make either the read or the second scan fail.
        // Discard the result and use the existing snapshot path in that case.
        match scan_with_data(root, true) {
            Ok(after) if after == expected => {}
            _ => return Ok(None),
        }
    }
    result.map(Some)
}

/// Copy selected entries to a private tree. The caller must validate the live source
/// before it publishes any of these bytes to an index.
pub fn copy_selected(src: &Path, dst: &Path, paths: &std::collections::BTreeSet<String>) -> Result<()> {
    let _t = crate::trace::span("APFS copy selected");
    let source = crate::tree::Tree::open(src)?;
    let target = crate::tree::Tree::open(dst)?;
    for rel in paths {
        // Descendants of a directory replaced by a symlink are deletions.
        let path = match crate::fsutil::safe_join(src, rel) {
            Ok(path) => path,
            Err(_) => continue,
        };
        let m = match fs::symlink_metadata(&path) {
            // On a volume that ignores case, `readme` also finds `README`: another path.
            Ok(_) if crate::fsutil::exact_metadata(&path, rel).is_none() => continue,
            Ok(metadata) => crate::fsutil::meta_of(&metadata),
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => continue,
            Err(e) => return Err(e.into()),
        };
        target.mkdirs_for(rel, None)?;
        match m.ftype {
            crate::fsutil::FType::File => {
                let mut input = source.open_read(rel)?;
                let metadata = input.metadata()?;
                let output = match target.clone_new_file(&input, rel)? {
                    Some(file) => file,
                    None => {
                        let mut file = target.create(rel, 0o600)?;
                        io::copy(&mut input, &mut file)?;
                        keep_birthtime(&metadata, &dst.join(rel))?;
                        file
                    }
                };
                output.set_permissions(fs::Permissions::from_mode(m.mode & 0o7777))?;
                crate::fsutil::set_mtime(&output, metadata.mtime(), metadata.mtime_nsec())?;
            }
            crate::fsutil::FType::Symlink => {
                target.symlink(rel, &source.read_link(rel)?)?;
                keep_birthtime(&fs::symlink_metadata(src.join(rel))?, &dst.join(rel))?;
            }
            crate::fsutil::FType::Dir => target.mkdirs(rel, None)?,
            crate::fsutil::FType::Special => bail!("cannot snapshot special file {}", src.join(rel).display()),
        }
    }
    Ok(())
}
/// Identity used to find moves. APFS clones, writes, and renames keep the creation time.
/// A new inode, such as a safe save, has a new identity, as with btrfs inode numbers.
pub fn file_identity(path: &Path) -> Result<Vec<u8>> {
    use std::os::macos::fs::MetadataExt as MacMetadataExt;
    let m = fs::symlink_metadata(path)?;
    Ok(identity((m.st_birthtime(), m.st_birthtime_nsec()), m.dev(), m.ino()))
}

/// A creation time with whole seconds usually comes from a tool that set an old modification
/// time (tar, zip, npm): APFS then lowers the creation time to it, and many unrelated files share
/// it. Such a file gets an identity of this tree only, so it never matches as a move.
fn identity(birth: (i64, i64), dev: u64, ino: u64) -> Vec<u8> {
    if birth.1 == 0 {
        format!("inode:{dev}:{ino}").into_bytes()
    } else {
        format!("{}:{}", birth.0, birth.1).into_bytes()
    }
}
pub(crate) fn keep_birthtime(src: &fs::Metadata, dst: &Path) -> Result<()> {
    use std::os::macos::fs::MetadataExt as MacMetadataExt;
    crate::sys::apfs_set_birthtime(dst, src.st_birthtime(), src.st_birthtime_nsec())
        .with_context(|| format!("set creation time {}", dst.display()))
}
/// Copy a folder that is not a tree root, file by file. Nested layers stay empty.
pub(crate) fn copy_subtree(src: &Path, dst: &Path) -> Result<()> {
    copy_tree(src, dst, false, &mut BTreeMap::new())
}
fn copy_tree(src: &Path, dst: &Path, root: bool, links: &mut BTreeMap<(u64, u64), std::path::PathBuf>) -> Result<()> {
    let m = fs::symlink_metadata(src)?;
    if m.is_dir() {
        fs::create_dir(dst)?;
        if root || !is_subvolume(src) {
            for e in fs::read_dir(src)? {
                let e = e?;
                let (from, to) = (e.path(), dst.join(e.file_name()));
                if let Err(err) = copy_tree(&from, &to, false, links) {
                    // An entry that a writer removed during the copy: the scan after the copy
                    // sees the change. Any other failure stops the copy.
                    if fs::symlink_metadata(&from).is_ok() {
                        return Err(err);
                    }
                    let _ = delete_tree(&to);
                }
            }
        }
        if root || !is_subvolume(src) {
            crate::sys::apfs_copy_metadata(src, dst).with_context(|| format!("copy metadata {}", dst.display()))?;
            crate::sys::apfs_immutable(dst, false).with_context(|| format!("unseal {}", dst.display()))?;
        }
        keep_birthtime(&m, dst)?;
        fs::set_permissions(dst, fs::Permissions::from_mode(m.mode() & 0o7777))?;
        crate::fsutil::set_mtime(&File::open(dst)?, m.mtime(), m.mtime_nsec())
            .with_context(|| format!("set directory time {}", dst.display()))?;
    } else if m.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
        crate::sys::apfs_copy_metadata(src, dst).with_context(|| format!("copy metadata {}", dst.display()))?;
        crate::sys::apfs_immutable(dst, false).with_context(|| format!("unseal {}", dst.display()))?;
        keep_birthtime(&m, dst)?;
    } else if m.is_file() {
        let key = (m.dev(), m.ino());
        if let Some(first) = links.get(&key) {
            fs::hard_link(first, dst)?;
        } else {
            crate::sys::apfs_clone(src, dst).with_context(|| format!("clone {}", src.display()))?;
            crate::sys::apfs_immutable(dst, false).with_context(|| format!("unseal {}", dst.display()))?;
            links.insert(key, dst.to_path_buf());
        }
    } else {
        bail!("cannot snapshot special file {}", src.display());
    }
    Ok(())
}
pub(crate) fn seal(path: &Path, readonly: bool, for_delete: bool) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    if !readonly {
        crate::sys::apfs_immutable(path, false)?;
        if for_delete && m.is_dir() && m.mode() & 0o700 != 0o700 {
            fs::set_permissions(path, fs::Permissions::from_mode((m.mode() & 0o7777) | 0o700))?;
        }
    }
    if m.is_dir() {
        for e in fs::read_dir(path)? {
            seal(&e?.path(), readonly, for_delete)?;
        }
    }
    if readonly {
        crate::sys::apfs_immutable(path, true)?;
    }
    Ok(())
}
/// True when a scanned tree needs file-by-file copies: special files, nested layers, or hard
/// links (a recursive clone copies hard links as separate files).
pub(crate) fn needs_file_copies(src: &Path, entries: &BTreeMap<String, ScanEntry>) -> bool {
    let mut inodes = std::collections::HashSet::new();
    entries.iter().any(|(p, e)| {
        e.meta.ftype == crate::fsutil::FType::Special
            || (!p.is_empty() && e.meta.is_dir() && is_subvolume(&src.join(p)))
            || (e.meta.is_file() && !inodes.insert((e.fingerprint.0, e.fingerprint.1)))
    })
}

/// Copy the complete tree `src` (scanned as `entries`) to the new path `dst`. A read-only copy
/// keeps the immutable flags of its entries except the root; a writable copy has none.
pub(crate) fn clone_complete(src: &Path, dst: &Path, entries: &BTreeMap<String, ScanEntry>, readonly: bool) -> Result<()> {
    if needs_file_copies(src, entries) {
        return copy_tree(src, dst, true, &mut BTreeMap::new());
    }
    // Clone the tree at once when no entry needs file-by-file handling. The clone keeps flags,
    // attributes, and creation times.
    crate::sys::apfs_clone(src, dst)?;
    if readonly {
        crate::sys::apfs_immutable(dst, false)?;
    } else {
        seal(dst, false, false)?;
    }
    Ok(())
}

pub fn snapshot(src: &Path, dst: &Path, readonly: bool) -> Result<()> {
    let _t = crate::trace::span(format!("APFS clone {}", dst.display()));
    if fs::symlink_metadata(dst).is_ok() {
        bail!("snapshot destination already exists");
    }
    let parent = subvol_info(src)?.uuid;
    let before = {
        let _t = crate::trace::span("APFS scan before");
        scan(src)?
    };
    let temp = dst.with_file_name(format!(".apfs-{}", crate::ids::new_id()));
    let mut published = false;
    let result = (|| -> Result<()> {
        let mut before = before;
        for round in 0.. {
            let _t = crate::trace::span("APFS copy tree");
            match clone_complete(src, &temp, &before, readonly) {
                Ok(()) => break,
                Err(e) => {
                    let now = scan(src)?;
                    if now == before || round + 1 >= crate::thin::COPY_ROUNDS {
                        return Err(e);
                    }
                    let _ = delete_tree(&temp);
                    crate::thin::settle(round);
                    before = now;
                }
            }
        }
        // Writers can change the source during the copy: copy again what changed, until a
        // round sees no change. After COPY_ROUNDS rounds the copy stays: each entry is then a
        // consistent copy from some moment of the copy, as with `git add`.
        for round in 0..crate::thin::COPY_ROUNDS {
            let after = {
                let _t = crate::trace::span("APFS scan after");
                scan(src)?
            };
            if before == after {
                break;
            }
            if round + 1 == crate::thin::COPY_ROUNDS {
                eprintln!(
                    "warning: {} changed during the whole copy; each file is kept as one of its versions from that time",
                    src.display()
                );
                break;
            }
            let paths = crate::thin::changed_paths(&before, &after);
            let _t = crate::trace::span(format!("APFS copy {} changed entries again", paths.len()));
            if needs_file_copies(src, &after) {
                // Hard links survive only a copy of the whole tree with its map of inodes.
                let mut copied = false;
                for _ in 0..crate::thin::COPY_ROUNDS {
                    crate::thin::settle(round);
                    let now = scan(src)?;
                    delete_tree(&temp)?;
                    if clone_complete(src, &temp, &now, readonly).is_ok() {
                        before = now;
                        copied = true;
                        break;
                    }
                }
                if !copied {
                    bail!("cannot copy {} while it changes", src.display());
                }
                continue;
            }
            if let Err(e) = crate::thin::resync(src, &temp, &paths) {
                // An entry that went away is a change: the next round copies it again.
                // Without a change it is an error.
                if scan(src)? == after {
                    return Err(e);
                }
                crate::thin::settle(round);
                continue;
            }
            crate::thin::settle(round);
            before = after;
        }
        mark(&temp, Some(parent), readonly).context("mark snapshot")?;
        // macOS 14 requires write access to a directory when rename updates its parent.
        let mode = fs::symlink_metadata(&temp)?.mode() & 0o7777;
        fs::set_permissions(&temp, fs::Permissions::from_mode(mode | 0o200))?;
        crate::sys::apfs_publish(&temp, dst).context("publish snapshot")?;
        published = true;
        fs::set_permissions(dst, fs::Permissions::from_mode(mode))?;
        if readonly {
            let _t = crate::trace::span("APFS seal tree");
            seal(dst, true, false).context("seal snapshot")?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = delete_tree(&temp);
        if published {
            let _ = delete_tree(dst);
        }
    }
    result
}
pub fn delete_tree(path: &Path) -> Result<()> {
    let m = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    seal(path, false, true)?;
    if m.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}
pub fn delete_subvolume(path: &Path) -> Result<()> {
    delete_tree(path)
}
pub fn is_not_empty(e: &anyhow::Error) -> bool {
    e.chain().any(|e| e.downcast_ref::<io::Error>().is_some_and(|e| e.raw_os_error() == Some(libc::ENOTEMPTY)))
}
// APFS cannot reflink into an existing destination inode. Callers use their byte-copy fallback.
pub fn clone_file(_src: &File, _dst: &File) -> io::Result<()> {
    Err(io::Error::from_raw_os_error(libc::ENOTSUP))
}
pub fn clone_range(_src: &File, _off: u64, _len: u64, _dst: &File, _dst_off: u64) -> io::Result<()> {
    Err(io::Error::from_raw_os_error(libc::ENOTSUP))
}
pub fn send_command(_parent: Option<&Path>, child: &Path) -> Result<Command> {
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("__apfs-send").arg(child);
    Ok(cmd)
}

pub fn send_optimized(parent: Option<&Path>, child: &Path, _clones: &[PathBuf], _compressed: bool) -> Result<Command> {
    send_command(parent, child)
}

pub fn supports_local_clone_receive() -> bool {
    true
}

pub fn supports_compressed_receive() -> bool {
    false
}

pub fn send_clone_identity(path: &Path) -> Result<(String, u64)> {
    Ok((clone_identity(path)?.0, 1))
}
pub fn clone_identity(path: &Path) -> Result<(String, Option<u64>)> {
    let info = subvol_info(path)?;
    if !info.readonly {
        bail!("clone source is not immutable");
    }
    // The APFS receiver resolves a verified source path itself, without the kernel's
    // UUID/transaction lookup. Its data therefore do not depend on a btrfs transaction.
    Ok((info.received_uuid.unwrap_or(info.uuid), None))
}
/// All entries of a folder or a layered tree, with data identities.
pub(crate) fn scan_src(s: &(impl Src + ?Sized)) -> Result<BTreeMap<String, ScanEntry>> {
    match s.layered() {
        Some(l) if !l.is_complete() => l.scan(true),
        _ => scan_with_data(s.folder().context("missing tree folder")?, true),
    }
}
pub fn candidates(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized)) -> Result<Vec<crate::changes::Change>> {
    let _t = crate::trace::span("APFS candidates");
    candidates_scanned(parent, child, scan_src(parent)?, scan_src(child)?)
}

/// Entries at and below `paths` of a folder or a layered tree, as `scan_src` gives them.
pub(crate) fn scan_paths(
    s: &(impl Src + ?Sized),
    paths: &std::collections::BTreeSet<String>,
) -> Result<BTreeMap<String, ScanEntry>> {
    let mut out = BTreeMap::new();
    for p in paths {
        if p.is_empty() || out.contains_key(p) {
            continue;
        }
        match s.layered() {
            Some(l) => out.extend(l.scan_at(p)?),
            None => out.extend(scan_at(s.folder().context("missing tree folder")?, p)?),
        }
    }
    Ok(out)
}

/// The entry at `rel` below `root` and its content, with paths relative to `root`.
pub(crate) fn scan_at(root: &Path, rel: &str) -> Result<BTreeMap<String, ScanEntry>> {
    let mut out = BTreeMap::new();
    let Ok(path) = crate::fsutil::safe_join(root, rel) else {
        return Ok(out);
    };
    let m = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    out.insert(rel.to_string(), entry_of_metadata(&m, &path, true)?);
    if m.is_dir() && !is_subvolume(&path) {
        let dir = crate::fsutil::open_dir_nofollow(&path)?;
        scan_bulk(&dir, root, rel, &mut out, &mut Vec::new())?;
    }
    Ok(out)
}

/// `candidates` from given scans of both trees.
pub(crate) fn candidates_scanned(
    parent: &(impl Src + ?Sized),
    child: &(impl Src + ?Sized),
    a: BTreeMap<String, ScanEntry>,
    b: BTreeMap<String, ScanEntry>,
) -> Result<Vec<crate::changes::Change>> {
    let names: std::collections::BTreeSet<_> = a.keys().chain(b.keys()).filter(|s| !s.is_empty()).collect();
    let mut changes: BTreeMap<_, _> = names
        .into_iter()
        .map(|p| {
            (
                p.clone(),
                crate::changes::Change {
                    path: p.clone(),
                    old_path: None,
                    old: a.get(p).map(|entry| entry.meta.clone()),
                    new: b.get(p).map(|entry| entry.meta.clone()),
                    ranges: None,
                },
            )
        })
        .collect();
    let id_of = |e: &ScanEntry| identity(e.birth, e.fingerprint.0, e.fingerprint.1);
    // An identity that more than one entry of either tree has is no evidence of a move: hard
    // links, copies that keep the creation time, or files with equal times.
    let mut count: BTreeMap<(bool, Vec<u8>), usize> = BTreeMap::new();
    for (side, tree) in [(false, &a), (true, &b)] {
        for (p, e) in tree.iter() {
            if !p.is_empty() {
                *count.entry((side, id_of(e))).or_default() += 1;
            }
        }
    }
    let mut removed: BTreeMap<Vec<u8>, Vec<String>> = BTreeMap::new();
    let mut added: BTreeMap<Vec<u8>, Vec<String>> = BTreeMap::new();
    for (p, c) in &changes {
        if c.new.is_none() {
            removed.entry(id_of(&a[p])).or_default().push(p.clone());
        }
        if c.old.is_none() {
            added.entry(id_of(&b[p])).or_default().push(p.clone());
        }
    }
    for (id, srcs) in removed {
        let Some(dsts) = added.get(&id) else {
            continue;
        };
        // Multiple hard links have the same identity. Do not invent a unique move.
        if srcs.len() != 1 || dsts.len() != 1 {
            continue;
        }
        if count.get(&(false, id.clone())) != Some(&1) || count.get(&(true, id.clone())) != Some(&1) {
            continue;
        }
        let (src, dst) = (&srcs[0], &dsts[0]);
        if changes[src].old.as_ref().map(|m| m.ftype) != changes[dst].new.as_ref().map(|m| m.ftype) {
            continue;
        }
        let old = changes.remove(src).unwrap().old;
        let c = changes.get_mut(dst).unwrap();
        c.old = old;
        c.old_path = Some(src.clone());
    }
    for (p, c) in &mut changes {
        if c.old_path.is_none() && c.old.as_ref().is_some_and(|m| m.is_file()) && c.new.as_ref().is_some_and(|m| m.is_file()) {
            let old = &a[p];
            let new = &b[p];
            let shared = old.fingerprint.0 == new.fingerprint.0
                && old.meta.size == new.meta.size
                && old.data.zip(new.data).is_some_and(|(a, b)| {
                    a.generation.is_some() && b.generation.is_some() && a.clone_id.is_some() && a.clone_id == b.clone_id
                });
            c.ranges = Some(if shared { Vec::new() } else { changed_ranges(parent, child, p)? });
        }
    }
    Ok(changes.into_values().collect())
}

// Compare fixed blocks when APFS cannot report changed extents. Adjacent blocks coalesce.
fn changed_ranges(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized), rel: &str) -> Result<Vec<(u64, u64)>> {
    use std::os::unix::fs::FileExt;
    let open = |p: PathBuf| -> Result<File> {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(&p).with_context(|| format!("open {}", p.display()))
    };
    let (a, b) = (open(parent.at(rel)?)?, open(child.at(rel)?)?);
    let (alen, blen) = (a.metadata()?.len(), b.metadata()?.len());
    let end = alen.max(blen);
    // Blocks at the same physical address hold the same bytes: only the others are read.
    let read = if alen == blen && alen >= SHARED_MIN { unshared(&a, &b, alen) } else { vec![(0, end)] };
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    let (mut ab, mut bb) = ([0u8; 4096], [0u8; 4096]);
    let mut done = 0u64;
    for (from, to) in read {
        let mut pos = (from - from % 4096).max(done);
        while pos < to {
            let an = (alen.saturating_sub(pos).min(4096)) as usize;
            let bn = (blen.saturating_sub(pos).min(4096)) as usize;
            a.read_exact_at(&mut ab[..an], pos)?;
            b.read_exact_at(&mut bb[..bn], pos)?;
            let len = an.max(bn) as u64;
            if ab[..an] != bb[..bn] {
                match ranges.last_mut() {
                    Some((start, n)) if *start + *n == pos => *n += len,
                    _ => ranges.push((pos, len)),
                }
            }
            pos += len;
        }
        done = pos;
    }
    Ok(ranges)
}

/// Files smaller than this are compared in full: reading them costs less than the mapping.
const SHARED_MIN: u64 = 1 << 20;

/// Byte ranges of two files of length `len` that are not at the same physical address, as
/// (start, end). The whole file when APFS does not report a mapping.
fn unshared(a: &File, b: &File, len: u64) -> Vec<(u64, u64)> {
    let _t = crate::trace::span("APFS shared extents");
    // Data in the page cache can still map to the old shared block: write it first.
    if a.sync_data().is_err() || b.sync_data().is_err() {
        return vec![(0, len)];
    }
    let (Some(ea), Some(eb)) = (crate::sys::apfs_extents(a, len), crate::sys::apfs_extents(b, len)) else {
        return vec![(0, len)];
    };
    let mut out: Vec<(u64, u64)> = Vec::new();
    let (mut i, mut j, mut pos) = (0, 0, 0u64);
    while pos < len && i < ea.len() && j < eb.len() {
        let (oa, la, da) = ea[i];
        let (ob, lb, db) = eb[j];
        let stop = (oa + la).min(ob + lb);
        if da + (pos - oa) != db + (pos - ob) {
            match out.last_mut() {
                Some((_, e)) if *e == pos => *e = stop,
                _ => out.push((pos, stop)),
            }
        }
        pos = stop;
        if pos >= oa + la {
            i += 1;
        }
        if pos >= ob + lb {
            j += 1;
        }
    }
    if pos < len {
        out.push((pos, len));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Sandbox(PathBuf);
    impl Sandbox {
        fn new() -> Self {
            let base = std::env::var_os("LAYR_APFS_TEST_ROOT").expect("set LAYR_APFS_TEST_ROOT to a case-sensitive APFS volume");
            let path = PathBuf::from(base).join(format!("backend-test-{}", crate::ids::new_id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = delete_tree(&self.0);
        }
    }

    #[test]
    #[ignore = "requires an APFS test volume"]
    fn populated_cache_conversion_keeps_clone_identity() -> Result<()> {
        let s = Sandbox::new();
        let work = s.0.join("work");
        fs::create_dir_all(work.join("target"))?;
        let source = work.join("target/data");
        fs::write(&source, vec![42; 1024 * 1024])?;
        let reference = s.0.join("reference");
        crate::sys::apfs_clone(&source, &reference)?;
        let identity = crate::sys::apfs_data_identity(&reference)?.clone_id;
        assert!(identity.is_some());
        assert!(crate::repo::convert_cache(
            &work,
            "target",
            rustix::process::getuid().as_raw(),
            rustix::process::getgid().as_raw()
        )?);
        assert_eq!(crate::sys::apfs_data_identity(&source)?.clone_id, identity);
        fs::write(&source, b"new cache output")?;
        assert_eq!(fs::read(&reference)?, vec![42; 1024 * 1024]);
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn snapshot_preserves_files_and_excludes_layers() -> Result<()> {
        check_snapshot_files(true, true)
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn snapshot_preserves_files_without_layers() -> Result<()> {
        check_snapshot_files(false, true)
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn snapshot_preserves_files_without_hard_links_or_layers() -> Result<()> {
        check_snapshot_files(false, false)
    }

    fn check_snapshot_files(with_layer: bool, with_hard_link: bool) -> Result<()> {
        let s = Sandbox::new();
        let src = s.0.join("work");
        // On a volume that ignores case, "a" and "A" are one entry.
        let upper = if crate::sys::case_sensitive(&s.0) { "A" } else { "B" };
        create_subvolume(&src)?;
        fs::write(src.join("a"), b"lower")?;
        fs::write(src.join(upper), b"upper")?;
        fs::write(src.join("readonly"), b"read only")?;
        fs::set_permissions(src.join("readonly"), fs::Permissions::from_mode(0o444))?;
        fs::create_dir(src.join("dir"))?;
        crate::sys::apfs_attribute(&src.join("dir"), c"com.layr.test", Some(b"directory metadata"))?;
        if with_hard_link {
            fs::hard_link(src.join("a"), src.join("hard"))?;
        }
        std::os::unix::fs::symlink("a", src.join("link"))?;
        fs::set_permissions(src.join(upper), fs::Permissions::from_mode(0o755))?;
        if with_layer {
            create_subvolume(&src.join("layer"))?;
            fs::write(src.join("layer/cache"), b"not in history")?;
        }
        fs::create_dir(src.join("readonly-dir"))?;
        fs::write(src.join("readonly-dir/file"), b"data")?;
        fs::set_permissions(src.join("readonly-dir"), fs::Permissions::from_mode(0o555))?;
        fs::set_permissions(&src, fs::Permissions::from_mode(0o555))?;
        let dst = s.0.join("state");
        snapshot(&src, &dst, true).context("snapshot original tree")?;
        for p in ["a", "dir", "link", "readonly-dir", "readonly-dir/file"] {
            assert_eq!(file_identity(&src.join(p))?, file_identity(&dst.join(p))?, "identity of {p}");
        }
        assert_ne!(file_identity(&src.join("a"))?, file_identity(&src.join(upper))?);
        assert_eq!(fs::symlink_metadata(&dst)?.mode() & 0o777, 0o555);
        assert_eq!(fs::read(dst.join("a"))?, b"lower");
        assert_eq!(fs::read(dst.join(upper))?, b"upper");
        assert_eq!(crate::sys::apfs_attribute(&dst.join("dir"), c"com.layr.test", None)?, b"directory metadata");
        assert_eq!(fs::symlink_metadata(dst.join(upper))?.mode() & 0o777, 0o755);
        assert_eq!(fs::read_link(dst.join("link"))?, Path::new("a"));
        if with_hard_link {
            assert_eq!(fs::symlink_metadata(dst.join("a"))?.ino(), fs::symlink_metadata(dst.join("hard"))?.ino());
        }
        assert_eq!(dst.join("layer").is_dir(), with_layer);
        assert!(!dst.join("layer/cache").exists());
        assert!(fs::write(dst.join("a"), b"bad").is_err());
        assert!(fs::write(dst.join("new"), b"bad").is_err());
        assert!(subvol_info(&dst)?.readonly);
        fs::write(src.join("a"), b"new")?;
        assert_eq!(fs::read(dst.join("a"))?, b"lower");
        let branch = s.0.join("branch");
        snapshot(&dst, &branch, false).context("branch from readonly snapshot")?;
        assert_eq!(fs::symlink_metadata(&branch)?.mode() & 0o777, 0o555);
        fs::write(branch.join("a"), b"branch")?;
        assert_eq!(fs::read(dst.join("a"))?, b"lower");
        // An existing destination must remain intact on failure.
        assert!(snapshot(&src, &branch, false).is_err());
        assert_eq!(fs::read(branch.join("a"))?, b"branch");
        delete_tree(&dst).context("delete readonly snapshot")?;
        assert!(!dst.exists());
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn clones_refuse_symlink_components() -> Result<()> {
        let s = Sandbox::new();
        let real = s.0.join("real");
        fs::create_dir(&real)?;
        fs::write(real.join("file"), b"data")?;
        std::os::unix::fs::symlink(&real, s.0.join("link"))?;
        assert!(crate::sys::apfs_clone(&s.0.join("link/file"), &s.0.join("copy")).is_err());
        assert!(crate::sys::apfs_clone(&real.join("file"), &s.0.join("link/copy")).is_err());
        std::os::unix::fs::symlink(real.join("file"), s.0.join("file-link"))?;
        assert!(crate::sys::apfs_clone(&s.0.join("file-link"), &s.0.join("copy")).is_err());
        assert!(!real.join("copy").exists());
        crate::sys::apfs_clone(&real.join("file"), &s.0.join("copy"))?;
        assert_eq!(fs::read(s.0.join("copy"))?, b"data");
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn readonly_transitions_preserve_identity_and_modes() -> Result<()> {
        let s = Sandbox::new();
        let tree = s.0.join("tree");
        create_subvolume(&tree)?;
        fs::create_dir(tree.join("dir"))?;
        fs::write(tree.join("dir/file"), b"data")?;
        fs::set_permissions(tree.join("dir"), fs::Permissions::from_mode(0o555))?;
        let id = subvol_info(&tree)?.uuid;
        set_readonly(&tree)?;
        assert!(fs::write(tree.join("dir/file"), b"bad").is_err());
        set_writable(&tree)?;
        assert_eq!(subvol_info(&tree)?.uuid, id);
        assert!(!subvol_info(&tree)?.readonly);
        assert_eq!(fs::metadata(tree.join("dir"))?.mode() & 0o777, 0o555);
        fs::write(tree.join("dir/file"), b"new")?;
        set_readonly(&tree)?;
        assert_eq!(subvol_info(&tree)?.uuid, id);
        assert!(subvol_info(&tree)?.readonly);
        assert!(fs::write(tree.join("dir/file"), b"bad").is_err());
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn moves_and_block_ranges_survive_clones() -> Result<()> {
        let s = Sandbox::new();
        let src = s.0.join("work");
        create_subvolume(&src)?;
        fs::write(src.join("file"), vec![0u8; 16384])?;
        let base = s.0.join("base");
        snapshot(&src, &base, true)?;
        let branch = s.0.join("branch");
        snapshot(&base, &branch, false)?;
        fs::rename(branch.join("file"), branch.join("moved"))?;
        fs::write(branch.join("moved"), vec![1u8; 16384])?;
        let next = s.0.join("next");
        snapshot(&branch, &next, true)?;
        let changes = candidates(&base, &next)?;
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].old_path.as_deref(), Some("file"));
        assert_eq!(changes[0].path, "moved");
        fs::rename(branch.join("moved"), branch.join("file"))?;
        let mut data = vec![0u8; 16384];
        data[8192] = 1;
        fs::write(branch.join("file"), data)?;
        let changes = candidates(&base, &branch)?;
        assert_eq!(changes[0].ranges, Some(vec![(8192, 4096)]));
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn bulk_scan_matches_lstat_scan() -> Result<()> {
        let s = Sandbox::new();
        let src = s.0.join("work");
        create_subvolume(&src)?;
        fs::create_dir_all(src.join("a/b/c"))?;
        fs::write(src.join("a/file"), b"data")?;
        fs::write(src.join("empty"), b"")?;
        fs::write(src.join("a/b/c/deep"), vec![7u8; 100_000])?;
        fs::write(src.join("новый файл"), b"unicode")?;
        fs::hard_link(src.join("a/file"), src.join("hard"))?;
        std::os::unix::fs::symlink("a/file", src.join("link"))?;
        std::os::unix::fs::symlink("missing/target", src.join("dangling"))?;
        fs::write(src.join("ro"), b"read only")?;
        fs::set_permissions(src.join("ro"), fs::Permissions::from_mode(0o4444))?;
        fs::set_permissions(src.join("a/b"), fs::Permissions::from_mode(0o750))?;
        create_subvolume(&src.join("layer"))?;
        fs::write(src.join("layer/hidden"), b"not scanned")?;
        crate::sys::apfs_clone(&src.join("a/file"), &src.join("clone"))?;
        let (bulk, old) = (scan_with_data(&src, true)?, scan_with_lstat(&src, true)?);
        assert_eq!(bulk.keys().collect::<Vec<_>>(), old.keys().collect::<Vec<_>>());
        for (k, b) in &bulk {
            let o = &old[k];
            let (mut fb, mut fo) = (b.fingerprint, o.fingerprint);
            if b.meta.is_dir() {
                // Folder sizes are not compared anywhere.
                (fb.3, fo.3) = (0, 0);
            }
            assert_eq!(fb, fo, "fingerprint of {k:?}");
            assert_eq!(
                (b.meta.ftype, b.meta.mode, b.meta.uid, b.meta.gid, b.meta.mtime, b.meta.ino),
                (o.meta.ftype, o.meta.mode, o.meta.uid, o.meta.gid, o.meta.mtime, o.meta.ino),
                "meta of {k:?}"
            );
            if !b.meta.is_dir() {
                assert_eq!(b.meta.size, o.meta.size, "size of {k:?}");
            }
            assert_eq!(b.data, o.data, "data of {k:?}");
            assert_eq!(b.birth, o.birth, "birth of {k:?}");
            if b.meta.is_file() {
                assert_eq!(b.nlink, o.nlink, "links of {k:?}");
            }
        }
        assert!(bulk.contains_key("layer") && !bulk.contains_key("layer/hidden"));
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn changed_ranges_read_only_unshared_blocks() -> Result<()> {
        use std::os::unix::fs::FileExt;
        let s = Sandbox::new();
        let data: Vec<u8> = (0..8u32 << 20).map(|i| (i / 4096 % 251) as u8).collect();
        fs::write(s.0.join("big"), &data)?;
        File::open(s.0.join("big"))?.sync_all()?;
        crate::sys::apfs_clone(&s.0.join("big"), &s.0.join("copy"))?;
        let f = fs::OpenOptions::new().write(true).open(s.0.join("copy"))?;
        f.write_all_at(b"changed", 100 * 4096 + 10)?;
        f.write_all_at(b"changed", 1500 * 4096)?;
        f.sync_all()?;
        let (a, b) = (File::open(s.0.join("big"))?, File::open(s.0.join("copy"))?);
        let read: u64 = unshared(&a, &b, data.len() as u64).iter().map(|(x, y)| y - x).sum();
        assert!(read < 1 << 20, "read {read} bytes");
        assert_eq!(changed_ranges(&s.0, &s.0, "big").ok(), Some(vec![]));
        fs::create_dir(s.0.join("x"))?;
        fs::create_dir(s.0.join("y"))?;
        fs::rename(s.0.join("big"), s.0.join("x/f"))?;
        fs::rename(s.0.join("copy"), s.0.join("y/f"))?;
        assert_eq!(changed_ranges(&s.0.join("x"), &s.0.join("y"), "f")?, vec![(100 * 4096, 4096), (1500 * 4096, 4096)]);
        // A write that is still in the page cache counts too.
        let f = fs::OpenOptions::new().write(true).open(s.0.join("y/f"))?;
        f.write_all_at(b"not flushed", 700 * 4096)?;
        assert_eq!(
            changed_ranges(&s.0.join("x"), &s.0.join("y"), "f")?,
            vec![(100 * 4096, 4096), (700 * 4096, 4096), (1500 * 4096, 4096)]
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn old_fixed_times_do_not_make_moves() -> Result<()> {
        use std::os::macos::fs::MetadataExt as MacMetadataExt;
        // npm and tar set old whole-second times; APFS lowers the creation time to them.
        let fixed = |p: &Path| crate::sys::apfs_mtime(p, 499162500, 0);
        let s = Sandbox::new();
        let src = s.0.join("work");
        create_subvolume(&src)?;
        fs::write(src.join("old.js"), b"one package")?;
        fixed(&src.join("old.js"))?;
        assert_eq!(fs::symlink_metadata(src.join("old.js"))?.st_birthtime(), 499162500);
        let base = s.0.join("base");
        snapshot(&src, &base, true)?;
        let branch = s.0.join("branch");
        snapshot(&base, &branch, false)?;
        fs::remove_file(branch.join("old.js"))?;
        fs::write(branch.join("new.js"), b"another package")?;
        fixed(&branch.join("new.js"))?;
        let changes = candidates(&base, &branch)?;
        assert!(changes.iter().all(|c| c.old_path.is_none()), "{changes:?}");
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn kernel_identity_detects_writes_with_restored_mtime() -> Result<()> {
        use std::os::unix::fs::FileExt;
        let s = Sandbox::new();
        let src = s.0.join("source");
        create_subvolume(&src)?;
        fs::write(src.join("file"), vec![0; 16384])?;
        let base = s.0.join("base");
        snapshot(&src, &base, true)?;
        let left = crate::sys::apfs_data_identity(&src.join("file"))?;
        let right = crate::sys::apfs_data_identity(&base.join("file"))?;
        assert!(left.clone_id.is_some());
        assert_eq!(left.clone_id, right.clone_id);
        assert!(crate::changes::file_changes(&base, &src)?.is_empty());
        let m = fs::metadata(src.join("file"))?;
        let file = fs::OpenOptions::new().write(true).open(src.join("file"))?;
        file.write_all_at(b"x", 8192)?;
        crate::fsutil::set_mtime(&file, m.mtime(), m.mtime_nsec())?;
        assert_ne!(crate::sys::apfs_data_identity(&src.join("file"))?.clone_id, right.clone_id);
        let changes = crate::changes::file_changes(&base, &src)?;
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].ranges, Some(vec![(8192, 4096)]));
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn live_read_rejects_writes_and_deletes() -> Result<()> {
        use std::os::unix::fs::FileExt;
        let s = Sandbox::new();
        let path = s.0.join("file");
        fs::write(&path, vec![0; 16384])?;
        assert_eq!(stable_read(&[&s.0], || Ok(7))?, Some(7));
        let m = fs::metadata(&path)?;
        let file = fs::OpenOptions::new().read(true).write(true).open(&path)?;
        assert_eq!(
            stable_read(&[&s.0], || {
                file.write_all_at(b"x", 0)?;
                crate::fsutil::set_mtime(&file, m.mtime(), m.mtime_nsec())?;
                Ok(7)
            })?,
            None
        );
        assert_eq!(
            stable_read(&[&s.0], || {
                fs::remove_file(&path)?;
                Ok(7)
            })?,
            None
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn index_replacement_shares_data_and_keeps_destination_metadata() -> Result<()> {
        let s = Sandbox::new();
        let src = s.0.join("source");
        let dst = s.0.join("index");
        create_subvolume(&src)?;
        create_subvolume(&dst)?;
        let source_file = File::create(src.join("file"))?;
        source_file.set_len(64 * 1024 * 1024)?;
        crate::fsutil::set_mtime(&source_file, 1600000000, 123456789)?;
        crate::sys::apfs_attribute(&src.join("file"), c"com.layr.source-only", Some(b"remove"))?;
        fs::set_permissions(src.join("file"), fs::Permissions::from_mode(0o444))?;
        set_readonly(&src)?;
        fs::write(dst.join("file"), b"old")?;
        crate::sys::apfs_attribute(&dst.join("file"), c"com.layr.origin", Some(b"index origin"))?;
        crate::sys::apfs_attribute(&dst.join("file"), c"com.layr.keep", Some(b"keep"))?;
        let old_inode = fs::metadata(dst.join("file"))?.ino();
        crate::fsutil::copy_index_entry(&src, "file", &dst, "file", None)?;
        let a = crate::sys::apfs_data_identity(&src.join("file"))?;
        let b = crate::sys::apfs_data_identity(&dst.join("file"))?;
        assert!(a.clone_id.is_some());
        assert_eq!(a.clone_id, b.clone_id);
        let m = fs::metadata(dst.join("file"))?;
        assert_ne!(m.ino(), old_inode);
        assert_eq!(m.len(), 64 * 1024 * 1024);
        assert_eq!(m.mode() & 0o7777, 0o444);
        assert_eq!((m.mtime(), m.mtime_nsec()), (1600000000, 123456789));
        assert_eq!(crate::sys::apfs_attribute(&dst.join("file"), c"com.layr.origin", None)?, b"index origin");
        assert_eq!(crate::sys::apfs_attribute(&dst.join("file"), c"com.layr.keep", None)?, b"keep");
        assert_eq!(
            crate::sys::apfs_attribute(&dst.join("file"), c"com.layr.source-only", None).unwrap_err().raw_os_error(),
            Some(libc::ENOATTR)
        );
        assert_eq!(fs::read_dir(&dst)?.count(), 1);
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn index_copy_preserves_hard_links_and_failed_copy_keeps_original() -> Result<()> {
        let s = Sandbox::new();
        let src = s.0.join("source");
        let dst = s.0.join("index");
        fs::create_dir(&src)?;
        fs::create_dir(&dst)?;
        fs::write(src.join("file"), b"new")?;
        fs::write(dst.join("file"), b"old")?;
        fs::hard_link(dst.join("file"), dst.join("alias"))?;
        let original = fs::metadata(dst.join("file"))?.ino();
        crate::fsutil::copy_index_entry(&src, "file", &dst, "file", None)?;
        assert_eq!(fs::metadata(dst.join("file"))?.ino(), original);
        assert_eq!(fs::metadata(dst.join("alias"))?.ino(), original);
        assert_eq!(fs::read(dst.join("alias"))?, b"new");
        fs::remove_file(dst.join("alias"))?;
        fs::write(src.join("file"), b"later")?;
        let tree = crate::tree::Tree::open(&dst)?;
        let result = tree.replace_index_file(&File::open(src.join("file"))?, "file", |_| bail!("test preparation error"));
        assert!(result.is_err());
        assert_eq!(fs::metadata(dst.join("file"))?.ino(), original);
        assert_eq!(fs::read(dst.join("file"))?, b"new");
        assert_eq!(fs::read_dir(&dst)?.count(), 1);
        // The general copy path still updates a working inode in place.
        crate::fsutil::copy_entry(&src, "file", &dst, "file", None)?;
        assert_eq!(fs::metadata(dst.join("file"))?.ino(), original);
        assert_eq!(fs::read(dst.join("file"))?, b"later");
        crate::sys::apfs_immutable(&dst.join("file"), true)?;
        assert!(!tree.replace_index_file(&File::open(src.join("file"))?, "file", |_| bail!("must not prepare"))?);
        crate::sys::apfs_immutable(&dst.join("file"), false)?;
        assert_eq!(fs::read_dir(&dst)?.count(), 1);
        Ok(())
    }
}

pub fn mark_received(path: &Path, uuid: &str) -> Result<()> {
    let mut info = subvol_info(path)?;
    info.received_uuid = Some(uuid.to_owned());
    let mode = fs::symlink_metadata(path)?.mode() & 0o7777;
    fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o200))?;
    let result = crate::sys::apfs_marker(path, Some(&serde_json::to_vec(&info)?));
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    result?;
    Ok(())
}
