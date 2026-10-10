//! File operations inside a folder tree that another user can change at the same time (a line
//! belongs to its owner, the service runs as root).
//!
//! Every path is resolved from a descriptor of the tree root with `openat2` and
//! `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`: a folder that the owner replaces with a link fails
//! the resolution instead of sending a root write outside the tree. The last component is then
//! used with `*at` calls that never follow a link.

use crate::fsutil::{meta_of, Meta};
use anyhow::{anyhow, bail, Context, Result};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, ResolveFlags};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};

fn resolve() -> ResolveFlags {
    ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS
}

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
        let fd = rustix::fs::open(path, OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())
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
        if rel.is_empty() {
            let f = File::from(self.root.try_clone().ok()?);
            return f.metadata().ok().map(|m| meta_of(&m));
        }
        let (d, n) = self.parent(rel).ok()?;
        // Open the entry itself (no follow) to get std metadata.
        let fd = rustix::fs::openat(&d, n, OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty()).ok()?;
        File::from(fd).metadata().ok().map(|m| meta_of(&m))
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
            Mode::from_raw_mode(mode),
        )
        .map_err(|e| anyhow!("create {rel}: {e}"))?;
        Ok(File::from(fd))
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

    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        let (df, nf) = self.parent(from)?;
        self.mkdirs_for(to, None)?;
        let (dt, nt) = self.parent(to)?;
        rustix::fs::renameat(&df, nf, &dt, nt).with_context(|| format!("move {from} to {to}"))?;
        Ok(())
    }

    pub fn chown(&self, rel: &str, u: u32, g: u32) -> Result<()> {
        if rel.is_empty() {
            rustix::fs::chownat(&self.root, "", Some(uid(u)), Some(gid(g)), AtFlags::EMPTY_PATH)?;
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
            rustix::fs::fchmod(&fd, Mode::from_raw_mode(mode & 0o7777))?;
        }
        Ok(())
    }

    /// Names in a folder of the tree.
    pub fn list(&self, rel_dir: &str) -> Result<Vec<String>> {
        let d = openat2(&self.root, rel_dir, OFlags::RDONLY | OFlags::DIRECTORY)?;
        list_fd(&d)
    }

    pub fn create_subvolume(&self, rel: &str) -> Result<()> {
        let (d, n) = self.parent(rel)?;
        crate::sys::subvol_create(d.as_fd(), n)
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

fn is_subvol_root(st: &rustix::fs::Stat, parent_dev: u64) -> bool {
    st.st_ino == 256 && st.st_dev != parent_dev
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
            Err(e) if crate::btrfs::is_not_empty(&e) => {
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
