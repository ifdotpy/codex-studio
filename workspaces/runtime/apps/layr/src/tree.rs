//! File operations inside a folder tree that another user can change at the same time (a line
//! belongs to its owner, the service runs as root).
//!
//! Every path is resolved from a descriptor of the tree root with `openat2` and
//! `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`: a folder that the owner replaces with a link fails
//! the resolution instead of sending a root write outside the tree. The last component is then
//! used with `*at` calls that never follow a link.

use crate::fsutil::{meta_of, Meta};
use anyhow::{anyhow, bail, Context, Result};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
fn resolve() -> rustix::fs::ResolveFlags {
    rustix::fs::ResolveFlags::BENEATH | rustix::fs::ResolveFlags::NO_SYMLINKS | rustix::fs::ResolveFlags::NO_MAGICLINKS
}

#[cfg(target_os = "linux")]
fn openat2(dir: impl AsFd, path: &str, flags: OFlags) -> io::Result<OwnedFd> {
    let p = if path.is_empty() { "." } else { path };
    Ok(rustix::fs::openat2(dir, p, flags | OFlags::CLOEXEC, Mode::empty(), resolve())?)
}

fn split(rel: &str) -> (&str, &str) {
    match rel.rfind('/') {
        Some(i) => (&rel[..i], &rel[i + 1..]),
        None => ("", rel),
    }
}

fn lstat_at(dir: impl AsFd, name: &str) -> Option<rustix::fs::Stat> {
    rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).ok()
}

fn is_dir(st: &rustix::fs::Stat) -> bool {
    FileType::from_raw_mode(st.st_mode) == FileType::Directory
}

pub struct Tree {
    root: OwnedFd,
    pub path: PathBuf,
}

impl Tree {
    pub fn open(path: &Path) -> Result<Tree> {
        let fd = rustix::fs::open(path, path_flags() | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())
            .map_err(|e| anyhow!("open {}: {e}", path.display()))?;
        Ok(Tree { root: fd, path: path.to_path_buf() })
    }

    /// A readable descriptor of the folder `rel_dir` (no links on the way; btrfs ioctls need a
    /// readable descriptor, not O_PATH).
    fn dir(&self, rel_dir: &str) -> io::Result<OwnedFd> {
        openat2(&self.root, rel_dir, OFlags::RDONLY | OFlags::DIRECTORY)
    }

    fn parent<'a>(&self, rel: &'a str) -> Result<(OwnedFd, &'a str)> {
        crate::fsutil::check_rel(rel)?;
        let (p, n) = split(rel);
        if n.is_empty() {
            bail!("empty name");
        }
        let d = self.dir(p).with_context(|| format!("{}: folder '{p}' (a link or missing)", self.path.display()))?;
        Ok((d, n))
    }

    pub fn stat(&self, rel: &str) -> Option<Meta> {
        self.stat_checked(rel).ok()
    }

    /// Read entry metadata without hiding a failed open or following a symbolic link.
    pub fn stat_checked(&self, rel: &str) -> Result<Meta> {
        if rel.is_empty() {
            let f = File::from(self.root.try_clone()?);
            return Ok(meta_of(&f.metadata()?));
        }
        let (d, n) = self.parent(rel)?;
        // Open the entry itself (no follow) to get std metadata.
        let fd =
            rustix::fs::openat(&d, n, entry_flags() | OFlags::CLOEXEC, Mode::empty()).with_context(|| format!("stat {rel}"))?;
        Ok(meta_of(&File::from(fd).metadata()?))
    }

    /// Copy directory metadata through descriptors, after its children have been copied.
    pub fn copy_directory_metadata(&self, from: &str, to: &str) -> Result<()> {
        let source = File::from(self.dir(from)?);
        let destination = File::from(self.dir(to)?);
        crate::fsutil::copy_metadata(&source, &destination)
    }

    /// Create a new private directory before any cache content is copied into it.
    pub fn mkdir_private(&self, rel: &str, own: Option<(u32, u32)>) -> Result<()> {
        let (dir, name) = self.parent(rel)?;
        rustix::fs::mkdirat(&dir, name, Mode::from_raw_mode(0o700))?;
        if let Some((u, g)) = own {
            self.chown(rel, u, g)?;
        }
        Ok(())
    }

    /// Copy attributes of the links themselves, without following either link or its parents.
    pub fn copy_symlink_xattrs(&self, from: &str, to: &str) -> Result<()> {
        let (source_dir, source_name) = self.parent(from)?;
        let (destination_dir, destination_name) = self.parent(to)?;
        #[cfg(target_os = "macos")]
        {
            // O_SYMLINK pins the link itself; descriptor xattr calls cannot follow its target.
            let flags = OFlags::RDONLY | OFlags::SYMLINK | OFlags::NONBLOCK | OFlags::CLOEXEC;
            let source = File::from(rustix::fs::openat(&source_dir, source_name, flags, Mode::empty())?);
            let destination = File::from(rustix::fs::openat(&destination_dir, destination_name, flags, Mode::empty())?);
            if !source.metadata()?.file_type().is_symlink() || !destination.metadata()?.file_type().is_symlink() {
                bail!("cache symlink changed while copying attributes");
            }
            crate::fsutil::copy_xattrs(&source, &destination)
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            // /proc pins each parent by its live descriptor. The l* calls do not follow the leaf.
            let source = PathBuf::from(format!("/proc/self/fd/{}/{source_name}", source_dir.as_raw_fd()));
            let destination = PathBuf::from(format!("/proc/self/fd/{}/{destination_name}", destination_dir.as_raw_fd()));
            crate::fsutil::copy_symlink_xattrs(&source, &destination)
        }
    }

    /// Create the folders of `rel_dir` (replacing a non-folder in the way).
    pub fn mkdirs(&self, rel_dir: &str, own: Option<(u32, u32)>) -> Result<()> {
        crate::fsutil::check_rel(rel_dir)?;
        let mut cur = self.dir("")?;
        for comp in rel_dir.split('/').filter(|c| !c.is_empty()) {
            let st = lstat_at(&cur, comp);
            let exists_dir = st.as_ref().map(is_dir).unwrap_or(false);
            if st.is_some() && !exists_dir {
                rustix::fs::unlinkat(&cur, comp, AtFlags::empty())?;
            }
            if !exists_dir {
                match rustix::fs::mkdirat(&cur, comp, Mode::from_raw_mode(0o755)) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(e) => return Err(anyhow!("mkdir {comp}: {e}")),
                }
                if let Some((u, g)) = own {
                    rustix::fs::chownat(&cur, comp, Some(uid(u)), Some(gid(g)), AtFlags::SYMLINK_NOFOLLOW)?;
                }
            }
            cur = openat2(&cur, comp, OFlags::RDONLY | OFlags::DIRECTORY).with_context(|| format!("enter {comp}"))?;
        }
        Ok(())
    }

    pub fn mkdirs_for(&self, rel: &str, own: Option<(u32, u32)>) -> Result<()> {
        self.mkdirs(split(rel).0, own)
    }

    /// Remove an entry; a folder with its content (nested subvolumes are destroyed).
    pub fn remove(&self, rel: &str) -> Result<()> {
        #[cfg(feature = "overlayfs")]
        if crate::storage::contains_managed_tree(&self.path.join(rel)) {
            if self.parent(rel).is_err() {
                return Ok(());
            }
            return crate::storage::delete_tree(&self.path.join(rel));
        }
        // A parent that is missing, a file or a link: nothing of that name is in the tree.
        let (d, n) = match self.parent(rel) {
            Ok(x) => x,
            Err(_) => return Ok(()),
        };
        remove_at(&d, n)
    }

    /// Remove a folder only if it is empty. Returns true if removed.
    pub fn rmdir(&self, rel: &str) -> bool {
        match self.parent(rel) {
            Ok((d, n)) => rustix::fs::unlinkat(&d, n, AtFlags::REMOVEDIR).is_ok(),
            Err(_) => false,
        }
    }

    /// A new file at `rel` (an existing entry there is removed first). Never through a link.
    pub fn create(&self, rel: &str, mode: u32) -> Result<File> {
        self.mkdirs_for(rel, None)?;
        let (d, n) = self.parent(rel)?;
        if lstat_at(&d, n).is_some() {
            remove_at(&d, n)?;
        }
        let fd = rustix::fs::openat(
            &d,
            n,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(mode as _),
        )
        .map_err(|e| anyhow!("create {rel}: {e}"))?;
        Ok(File::from(fd))
    }

    /// Clone a new APFS file without a byte copy. Existing entries stay untouched.
    #[cfg(target_os = "macos")]
    pub fn clone_new_file(&self, src: &File, rel: &str) -> Result<Option<File>> {
        let (dir, name) = self.parent(rel)?;
        if lstat_at(&dir, name).is_some() {
            return Ok(None);
        }
        match crate::sys::apfs_clone_at(src.as_fd(), dir.as_fd(), name) {
            Ok(()) => {}
            Err(e) if matches!(e.raw_os_error(), Some(libc::EXDEV | libc::ENOTSUP)) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let fd = rustix::fs::openat(&dir, name, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())?;
        crate::sys::apfs_unseal_fd(fd.as_fd())?;
        let mode = rustix::fs::fstat(&fd)?.st_mode;
        rustix::fs::fchmod(&fd, Mode::from_raw_mode(mode | 0o200))?;
        let fd = rustix::fs::openat(&dir, name, OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())?;
        Ok(Some(File::from(fd)))
    }

    /// Replace a private index file with a clone. Working files must keep their inode.
    /// Hard-linked or flagged destinations use the existing copy path.
    #[cfg(target_os = "macos")]
    pub fn replace_index_file(&self, src: &File, rel: &str, finish: impl FnOnce(&File) -> Result<()>) -> Result<bool> {
        use std::os::macos::fs::MetadataExt as _;
        use std::os::unix::fs::MetadataExt;
        let (dir, name) = self.parent(rel)?;
        let old = match rustix::fs::openat(
            &dir,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => File::from(fd),
            Err(_) => return Ok(false),
        };
        let meta = old.metadata()?;
        if !meta.is_file() || meta.nlink() != 1 || meta.st_flags() != 0 {
            return Ok(false);
        }
        let temp = format!(".layr-copy-{}", crate::ids::new_id());
        match crate::sys::apfs_clone_at(src.as_fd(), dir.as_fd(), &temp) {
            Ok(()) => {}
            Err(e) if matches!(e.raw_os_error(), Some(libc::EXDEV | libc::ENOTSUP)) => return Ok(false),
            Err(e) => return Err(e.into()),
        }
        let result = (|| -> Result<bool> {
            let fd = rustix::fs::openat(&dir, temp.as_str(), OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())?;
            crate::sys::apfs_unseal_fd(fd.as_fd())?;
            let mode = rustix::fs::fstat(&fd)?.st_mode;
            rustix::fs::fchmod(&fd, Mode::from_raw_mode(mode | 0o200))?;
            let prepared = File::from(rustix::fs::openat(
                &dir,
                temp.as_str(),
                OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?);
            // Keep destination attributes, ACL and origin identity, just as an in-place
            // copy does. COPYFILE_METADATA replaces attributes without copying data.
            crate::sys::apfs_copy_metadata_fd(old.as_fd(), prepared.as_fd())?;
            finish(&prepared)?;
            let current = rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
            if current.st_ino != meta.ino() || current.st_dev as u64 != meta.dev() || current.st_nlink != 1 {
                bail!("index destination changed during copy: {rel}");
            }
            rustix::fs::renameat(&dir, temp.as_str(), &dir, name)?;
            Ok(true)
        })();
        if result.is_err() {
            let _ = rustix::fs::unlinkat(&dir, temp.as_str(), AtFlags::empty());
        }
        result
    }

    /// An existing regular file opened for writing, or None (absent, a link, not a file).
    pub fn open_write(&self, rel: &str) -> Result<Option<File>> {
        let (d, n) = match self.parent(rel) {
            Ok(x) => x,
            Err(_) => return Ok(None),
        };
        let fd = match rustix::fs::openat(
            &d,
            n,
            OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(_) => return Ok(None),
        };
        let f = File::from(fd);
        if !f.metadata()?.is_file() {
            return Ok(None);
        }
        Ok(Some(f))
    }

    /// A regular file opened for reading, never through a link.
    pub fn open_read(&self, rel: &str) -> Result<File> {
        let (d, n) = self.parent(rel)?;
        let fd = rustix::fs::openat(&d, n, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK, Mode::empty())
            .map_err(|e| anyhow!("open {rel}: {e}"))?;
        let f = File::from(fd);
        if !f.metadata()?.is_file() {
            bail!("{rel} is not a regular file");
        }
        Ok(f)
    }

    pub fn read_link(&self, rel: &str) -> Result<String> {
        let (d, n) = self.parent(rel)?;
        let t = rustix::fs::readlinkat(&d, n, Vec::new()).map_err(|e| anyhow!("readlink {rel}: {e}"))?;
        Ok(t.to_string_lossy().into_owned())
    }

    pub fn symlink(&self, rel: &str, target: &str) -> Result<()> {
        self.mkdirs_for(rel, None)?;
        let (d, n) = self.parent(rel)?;
        if lstat_at(&d, n).is_some() {
            remove_at(&d, n)?;
        }
        rustix::fs::symlinkat(target, &d, n).with_context(|| format!("symlink {rel}"))?;
        Ok(())
    }

    /// Link a copied entry within the tree. Neither parent path can follow a symbolic link.
    pub fn hard_link(&self, from: &str, to: &str) -> Result<()> {
        let (source, source_name) = self.parent(from)?;
        self.mkdirs_for(to, None)?;
        let (destination, destination_name) = self.parent(to)?;
        rustix::fs::linkat(&source, source_name, &destination, destination_name, AtFlags::empty())
            .with_context(|| format!("link {from} to {to}"))?;
        Ok(())
    }

    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        #[cfg(feature = "overlayfs")]
        if crate::storage::contains_managed_tree(&self.path.join(from)) {
            self.mkdirs_for(to, None)?;
            let _ = (self.parent(from)?, self.parent(to)?);
            return crate::storage::rename_tree(&self.path.join(from), &self.path.join(to));
        }
        let (df, nf) = self.parent(from)?;
        self.mkdirs_for(to, None)?;
        let (dt, nt) = self.parent(to)?;
        rustix::fs::renameat(&df, nf, &dt, nt).with_context(|| format!("move {from} to {to}"))?;
        Ok(())
    }

    pub fn chown(&self, rel: &str, u: u32, g: u32) -> Result<()> {
        if rel.is_empty() {
            #[cfg(target_os = "linux")]
            rustix::fs::chownat(&self.root, "", Some(uid(u)), Some(gid(g)), AtFlags::EMPTY_PATH)?;
            #[cfg(target_os = "macos")]
            rustix::fs::fchown(&self.root, Some(uid(u)), Some(gid(g)))?;
            return Ok(());
        }
        let (d, n) = self.parent(rel)?;
        rustix::fs::chownat(&d, n, Some(uid(u)), Some(gid(g)), AtFlags::SYMLINK_NOFOLLOW)
            .with_context(|| format!("chown {rel}"))?;
        Ok(())
    }

    /// Set the mode of a file or folder (a link is left alone).
    pub fn chmod(&self, rel: &str, mode: u32) -> Result<()> {
        let fd = if rel.is_empty() {
            self.dir("").ok()
        } else {
            let (d, n) = self.parent(rel)?;
            rustix::fs::openat(&d, n, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK, Mode::empty()).ok()
        };
        if let Some(fd) = fd {
            rustix::fs::fchmod(&fd, Mode::from_raw_mode((mode & 0o7777) as _))?;
        }
        Ok(())
    }

    /// Names in a folder of the tree.
    pub fn list(&self, rel_dir: &str) -> Result<Vec<String>> {
        let d = openat2(&self.root, rel_dir, OFlags::RDONLY | OFlags::DIRECTORY)?;
        list_fd(&d)
    }

    pub fn create_subvolume(&self, rel: &str) -> Result<()> {
        #[cfg(feature = "overlayfs")]
        {
            let _ = self.parent(rel)?;
            return crate::storage::create_subvolume(&self.path.join(rel));
        }
        #[cfg(not(feature = "overlayfs"))]
        let (d, n) = self.parent(rel)?;
        #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
        {
            crate::sys::subvol_create(d.as_fd(), n)
        }
        #[cfg(target_os = "macos")]
        {
            let _ = (d, n);
            crate::storage::create_subvolume(&self.path.join(rel))
        }
    }

    /// Destroy every subvolume nested in the tree (before the tree root is deleted).
    pub fn destroy_nested(&self) -> Result<()> {
        let d = openat2(&self.root, "", OFlags::RDONLY | OFlags::DIRECTORY)?;
        destroy_nested_in(&d)
    }
}

fn uid(u: u32) -> rustix::fs::Uid {
    rustix::fs::Uid::from_raw(u)
}

fn gid(g: u32) -> rustix::fs::Gid {
    rustix::fs::Gid::from_raw(g)
}

fn list_fd(dir: &OwnedFd) -> Result<Vec<String>> {
    let mut v = Vec::new();
    for e in rustix::fs::Dir::read_from(dir)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if name != "." && name != ".." {
            v.push(name);
        }
    }
    Ok(v)
}

fn is_subvol_root(st: &rustix::fs::Stat, parent_dev: rustix::fs::Dev) -> bool {
    cfg!(all(target_os = "linux", not(feature = "overlayfs"))) && st.st_ino == 256 && st.st_dev != parent_dev
}

/// Remove `name` in the folder `dir`, recursively, never following links.
fn remove_at(dir: &OwnedFd, name: &str) -> Result<()> {
    let st = match lstat_at(dir, name) {
        Some(st) => st,
        None => return Ok(()),
    };
    if !is_dir(&st) {
        rustix::fs::unlinkat(dir, name, AtFlags::empty())?;
        return Ok(());
    }
    let pst = rustix::fs::fstat(dir)?;
    let sub =
        rustix::fs::openat(dir, name, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())?;
    if is_subvol_root(&st, pst.st_dev) {
        // O(1) unless it holds nested subvolumes itself.
        match crate::sys::subvol_destroy(dir.as_fd(), name) {
            Ok(()) => return Ok(()),
            Err(e) if crate::storage::is_not_empty(&e) => {
                destroy_nested_in(&sub)?;
                drop(sub);
                return crate::sys::subvol_destroy(dir.as_fd(), name);
            }
            Err(e) => return Err(e),
        }
    }
    for child in list_fd(&sub)? {
        remove_at(&sub, &child)?;
    }
    drop(sub);
    rustix::fs::unlinkat(dir, name, AtFlags::REMOVEDIR)?;
    Ok(())
}

fn destroy_nested_in(dir: &OwnedFd) -> Result<()> {
    let pst = rustix::fs::fstat(dir)?;
    for child in list_fd(dir)? {
        let st = match lstat_at(dir, &child) {
            Some(st) => st,
            None => continue,
        };
        if !is_dir(&st) {
            continue;
        }
        if is_subvol_root(&st, pst.st_dev) {
            remove_at(dir, &child)?;
        } else if let Ok(sub) = rustix::fs::openat(
            dir,
            child.as_str(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            destroy_nested_in(&sub)?;
        }
    }
    Ok(())
}

fn path_flags() -> OFlags {
    #[cfg(target_os = "linux")]
    {
        OFlags::PATH
    }
    #[cfg(target_os = "macos")]
    {
        OFlags::RDONLY
    }
}
fn entry_flags() -> OFlags {
    #[cfg(target_os = "linux")]
    {
        OFlags::PATH | OFlags::NOFOLLOW
    }
    #[cfg(target_os = "macos")]
    {
        // O_SYMLINK opens the link itself. Combining it with O_NOFOLLOW returns ELOOP.
        OFlags::RDONLY | OFlags::SYMLINK | OFlags::NONBLOCK
    }
}
#[cfg(target_os = "macos")]
fn openat2(dir: impl AsFd, path: &str, flags: OFlags) -> io::Result<OwnedFd> {
    crate::fsutil::check_rel(path).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    let parts: Vec<_> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut at = rustix::io::dup(dir)?;
    for (i, part) in parts.iter().enumerate() {
        let f = if i + 1 == parts.len() { flags } else { OFlags::RDONLY | OFlags::DIRECTORY };
        at = rustix::fs::openat(&at, *part, f | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())?;
    }
    if parts.is_empty() {
        return Ok(rustix::fs::openat(&at, ".", flags | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())?);
    }
    Ok(at)
}
