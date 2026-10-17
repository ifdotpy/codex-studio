//! APFS local-user backend. Clones data extents, scans directories, and rejects source changes.
//! Snapshots use verified directory clones; transport uses the compatible btrfs stream format.
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
    let info = SubvolInfo { uuid: crate::ids::new_id(), received_uuid: None, parent_uuid: parent, readonly };
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
        bail!("layr requires case-sensitive APFS on macOS");
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
type Fingerprint = (u64, u64, u32, u64, i64, i64, i64, i64);
struct ScanEntry {
    fingerprint: Fingerprint,
    meta: crate::fsutil::Meta,
}
impl PartialEq for ScanEntry {
    fn eq(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint
    }
}
fn scan(root: &Path) -> Result<BTreeMap<String, ScanEntry>> {
    fn visit(root: &Path, rel: &str, out: &mut BTreeMap<String, ScanEntry>) -> Result<()> {
        let path = root.join(rel);
        let m = fs::symlink_metadata(&path)?;
        out.insert(
            rel.into(),
            ScanEntry {
                fingerprint: (m.dev(), m.ino(), m.mode(), m.len(), m.mtime(), m.mtime_nsec(), m.ctime(), m.ctime_nsec()),
                meta: crate::fsutil::meta_of(&m),
            },
        );
        if m.is_dir() && (rel.is_empty() || !is_subvolume(&path)) {
            for e in fs::read_dir(path)? {
                let e = e?;
                let name = e.file_name().into_string().map_err(|_| anyhow::anyhow!("non-UTF-8 file name"))?;
                let next = if rel.is_empty() { name } else { format!("{rel}/{name}") };
                visit(root, &next, out)?;
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    visit(root, "", &mut out)?;
    Ok(out)
}
pub fn file_identity(path: &Path) -> Result<Vec<u8>> {
    match crate::sys::apfs_attribute(path, c"com.layr.origin", None) {
        Ok(v) => Ok(v),
        Err(e) if e.raw_os_error() == Some(libc::ENOATTR) => {
            use std::os::macos::fs::MetadataExt as MacMetadataExt;
            let m = fs::symlink_metadata(path)?;
            Ok(format!("{}:{}:{}:{}", m.dev(), m.ino(), m.st_birthtime(), m.st_birthtime_nsec()).into_bytes())
        }
        Err(e) => Err(e.into()),
    }
}
fn copy_tree(src: &Path, dst: &Path, root: bool, links: &mut BTreeMap<(u64, u64), std::path::PathBuf>) -> Result<()> {
    let m = fs::symlink_metadata(src)?;
    if m.is_dir() {
        fs::create_dir(dst)?;
        if root || !is_subvolume(src) {
            for e in fs::read_dir(src)? {
                let e = e?;
                copy_tree(&e.path(), &dst.join(e.file_name()), false, links)?;
            }
        }
        if root || !is_subvolume(src) {
            crate::sys::apfs_copy_metadata(src, dst).with_context(|| format!("copy metadata {}", dst.display()))?;
            crate::sys::apfs_immutable(dst, false).with_context(|| format!("unseal {}", dst.display()))?;
        }
        fs::set_permissions(dst, fs::Permissions::from_mode(m.mode() & 0o7777))?;
        crate::fsutil::set_mtime(&File::open(dst)?, m.mtime(), m.mtime_nsec())
            .with_context(|| format!("set directory time {}", dst.display()))?;
    } else if m.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
        crate::sys::apfs_copy_metadata(src, dst).with_context(|| format!("copy metadata {}", dst.display()))?;
        crate::sys::apfs_immutable(dst, false).with_context(|| format!("unseal {}", dst.display()))?;
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
    // macOS requires write access for setxattr, including on an owned read-only file.
    let needs_write = !m.file_type().is_symlink() && m.mode() & 0o200 == 0;
    if needs_write {
        fs::set_permissions(dst, fs::Permissions::from_mode((m.mode() & 0o7777) | 0o200))?;
    }
    crate::sys::apfs_attribute(dst, c"com.layr.origin", Some(&file_identity(src)?))
        .with_context(|| format!("set file identity on {}", dst.display()))?;
    if needs_write {
        fs::set_permissions(dst, fs::Permissions::from_mode(m.mode() & 0o7777))?;
    }
    Ok(())
}
fn seal(path: &Path, readonly: bool, for_delete: bool) -> Result<()> {
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
        {
            let _t = crate::trace::span("APFS copy tree");
            let mut inodes = std::collections::HashSet::new();
            let native = !before.iter().any(|(p, e)| {
                e.meta.ftype == crate::fsutil::FType::Special
                    || (!p.is_empty() && e.meta.is_dir() && is_subvolume(&src.join(p)))
                    // Recursive clones copy hard links as separate files.
                    || (e.meta.is_file() && !inodes.insert((e.fingerprint.0, e.fingerprint.1)))
            });
            if native {
                // Clone the tree at once when no entry needs file-by-file handling.
                crate::sys::apfs_clone(src, &temp)?;
                seal(&temp, false, false)?;
                for (p, entry) in &before {
                    let dst = temp.join(p);
                    let writable = !entry.meta.ftype.eq(&crate::fsutil::FType::Symlink) && entry.meta.mode & 0o200 == 0;
                    if writable {
                        fs::set_permissions(&dst, fs::Permissions::from_mode(entry.meta.mode | 0o200))?;
                    }
                    crate::sys::apfs_attribute(&dst, c"com.layr.origin", Some(&file_identity(&src.join(p))?))?;
                    if writable {
                        fs::set_permissions(&dst, fs::Permissions::from_mode(entry.meta.mode))?;
                    }
                }
            } else {
                copy_tree(src, &temp, true, &mut BTreeMap::new())?;
            }
        }
        let after = {
            let _t = crate::trace::span("APFS scan after");
            scan(src)?
        };
        if before != after {
            bail!("source changed during APFS snapshot; retry when writers are idle");
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
pub fn candidates(parent: &Path, child: &Path) -> Result<Vec<crate::changes::Change>> {
    let _t = crate::trace::span("APFS candidates");
    let (a, b) = (scan(parent)?, scan(child)?);
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
    let mut removed: BTreeMap<Vec<u8>, Vec<String>> = BTreeMap::new();
    let mut added: BTreeMap<Vec<u8>, Vec<String>> = BTreeMap::new();
    for (p, c) in &changes {
        if c.new.is_none() {
            removed.entry(file_identity(&parent.join(p))?).or_default().push(p.clone());
        }
        if c.old.is_none() {
            added.entry(file_identity(&child.join(p))?).or_default().push(p.clone());
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
            c.ranges = Some(changed_ranges(&parent.join(p), &child.join(p))?);
        }
    }
    Ok(changes.into_values().collect())
}

// Compare fixed blocks when APFS cannot report changed extents. Adjacent blocks coalesce.
fn changed_ranges(a: &Path, b: &Path) -> Result<Vec<(u64, u64)>> {
    use std::io::Read;
    let (mut a, mut b) = (File::open(a)?, File::open(b)?);
    let (alen, blen) = (a.metadata()?.len(), b.metadata()?.len());
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    let (mut ab, mut bb) = ([0u8; 4096], [0u8; 4096]);
    let mut pos = 0;
    while pos < alen.max(blen) {
        let an = (alen.saturating_sub(pos).min(4096)) as usize;
        let bn = (blen.saturating_sub(pos).min(4096)) as usize;
        a.read_exact(&mut ab[..an])?;
        b.read_exact(&mut bb[..bn])?;
        let len = an.max(bn) as u64;
        if ab[..an] != bb[..bn] {
            match ranges.last_mut() {
                Some((start, n)) if *start + *n == pos => *n += len,
                _ => ranges.push((pos, len)),
            }
        }
        pos += len;
    }
    Ok(ranges)
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
        create_subvolume(&src)?;
        fs::write(src.join("a"), b"lower")?;
        fs::write(src.join("A"), b"upper")?;
        fs::write(src.join("readonly"), b"read only")?;
        fs::set_permissions(src.join("readonly"), fs::Permissions::from_mode(0o444))?;
        fs::create_dir(src.join("dir"))?;
        crate::sys::apfs_attribute(&src.join("dir"), c"com.layr.test", Some(b"directory metadata"))?;
        if with_hard_link {
            fs::hard_link(src.join("a"), src.join("hard"))?;
        }
        std::os::unix::fs::symlink("a", src.join("link"))?;
        fs::set_permissions(src.join("A"), fs::Permissions::from_mode(0o755))?;
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
        assert_eq!(fs::symlink_metadata(&dst)?.mode() & 0o777, 0o555);
        assert_eq!(fs::read(dst.join("a"))?, b"lower");
        assert_eq!(fs::read(dst.join("A"))?, b"upper");
        assert_eq!(crate::sys::apfs_attribute(&dst.join("dir"), c"com.layr.test", None)?, b"directory metadata");
        assert_eq!(fs::symlink_metadata(dst.join("A"))?.mode() & 0o777, 0o755);
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
