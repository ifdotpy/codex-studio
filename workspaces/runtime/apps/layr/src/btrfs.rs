//! btrfs operations: subvolumes, snapshots, reflinks (through `sys` and `rustix`), and the
//! `btrfs` command for send streams.

use crate::sys;
use anyhow::{anyhow, bail, Context, Result};
use std::fs::File;
use std::os::fd::AsFd;
use std::path::Path;
use std::process::{Command, Stdio};

const BTRFS_SUPER_MAGIC: i64 = 0x9123_683e;
const FIRST_FREE_OBJECTID: u64 = 256;

#[derive(Debug, Clone)]
pub struct SubvolInfo {
    pub uuid: String,
    pub received_uuid: Option<String>,
    /// The subvolume this one is a snapshot of.
    pub parent_uuid: Option<String>,
    pub readonly: bool,
}

fn fmt_uuid(b: &[u8; 16]) -> Option<String> {
    if b.iter().all(|x| *x == 0) {
        return None;
    }
    Some(uuid::Uuid::from_bytes(*b).hyphenated().to_string())
}

fn split_parent(path: &Path) -> Result<(File, String)> {
    let parent = path.parent().ok_or_else(|| anyhow!("no parent: {}", path.display()))?;
    let name = path.file_name().ok_or_else(|| anyhow!("no name: {}", path.display()))?;
    let dir = File::open(if parent.as_os_str().is_empty() { Path::new(".") } else { parent })
        .with_context(|| format!("open {}", parent.display()))?;
    Ok((dir, name.to_string_lossy().into_owned()))
}

/// True when `path` lives on a btrfs file system.
pub fn is_btrfs(path: &Path) -> bool {
    match rustix::fs::statfs(path) {
        Ok(st) => st.f_type as i64 == BTRFS_SUPER_MAGIC,
        Err(_) => false,
    }
}

/// True when `path` is the root of a subvolume.
pub fn is_subvolume(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match std::fs::symlink_metadata(path) {
        Ok(m) => m.is_dir() && m.ino() == FIRST_FREE_OBJECTID && is_btrfs(path),
        Err(_) => false,
    }
}

pub fn create_subvolume(path: &Path) -> Result<()> {
    let (dir, name) = split_parent(path)?;
    sys::subvol_create(dir.as_fd(), &name).with_context(|| format!("create subvolume {}", path.display()))
}

/// Snapshot `src` (a subvolume) to `dst`. The destination must not exist.
pub fn snapshot(src: &Path, dst: &Path, readonly: bool) -> Result<()> {
    let _t = crate::trace::span(format!("snapshot {}", dst.display()));
    let s = File::open(src).with_context(|| format!("open {}", src.display()))?;
    let (dir, name) = split_parent(dst)?;
    sys::subvol_snapshot(s.as_fd(), dir.as_fd(), &name, readonly)
        .with_context(|| format!("snapshot {} to {}", src.display(), dst.display()))
}

/// Delete the subvolume at `path`. Nested subvolumes must be deleted first.
pub fn delete_subvolume(path: &Path) -> Result<()> {
    let (dir, name) = split_parent(path)?;
    sys::subvol_destroy(dir.as_fd(), &name).with_context(|| format!("delete subvolume {}", path.display()))
}

pub fn subvol_info(path: &Path) -> Result<SubvolInfo> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let raw = sys::subvol_info(f.as_fd()).with_context(|| format!("subvolume info {}", path.display()))?;
    let flags = sys::subvol_flags(f.as_fd())?;
    Ok(SubvolInfo {
        uuid: fmt_uuid(&raw.uuid).unwrap_or_default(),
        received_uuid: fmt_uuid(&raw.received_uuid),
        parent_uuid: fmt_uuid(&raw.parent_uuid),
        readonly: flags & sys::SUBVOL_RDONLY != 0,
    })
}

/// Delete a subvolume and every subvolume nested inside it, or a plain folder tree. The tree is
/// walked through descriptors (no link is followed), because a line belongs to its owner.
pub fn delete_tree(path: &Path) -> Result<()> {
    let _t = crate::trace::span(format!("delete {}", path.display()));
    if std::fs::symlink_metadata(path).is_err() {
        return Ok(());
    }
    if is_subvolume(path) {
        // O(1) in the usual case. Only a subvolume with nested subvolumes (a working folder with
        // its .git or build layers) answers ENOTEMPTY; then the nested ones are found and
        // destroyed first, through descriptors.
        let (dir, name) = split_parent(path)?;
        match sys::subvol_destroy(dir.as_fd(), &name) {
            Ok(()) => return Ok(()),
            Err(e) if is_not_empty(&e) => {
                let t = crate::tree::Tree::open(path)?;
                // The usual nested subvolume first (the .git layer), then a full search.
                if t.stat(".git").map(|m| m.is_dir() && m.ino == FIRST_FREE_OBJECTID).unwrap_or(false) {
                    t.remove(".git")?;
                    if sys::subvol_destroy(dir.as_fd(), &name).is_ok() {
                        return Ok(());
                    }
                }
                t.destroy_nested()?;
                return delete_subvolume(path);
            }
            Err(e) => return Err(e.context(format!("delete subvolume {}", path.display()))),
        }
    }
    let parent = path.parent().ok_or_else(|| anyhow!("no parent: {}", path.display()))?;
    let name = path.file_name().ok_or_else(|| anyhow!("no name: {}", path.display()))?.to_string_lossy().into_owned();
    crate::tree::Tree::open(parent)?.remove(&name)
}

pub fn is_not_empty(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<std::io::Error>().map(|x| x.raw_os_error() == Some(39)).unwrap_or(false))
}

/// Reflink the whole content of `src` into `dst`.
pub fn clone_file(src: &File, dst: &File) -> std::io::Result<()> {
    rustix::fs::ioctl_ficlone(dst, src).map_err(std::io::Error::from)
}

/// Reflink a byte range. Offsets and length must be aligned to the block size, except a range
/// that ends at the end of the source file.
pub fn clone_range(src: &File, src_off: u64, len: u64, dst: &File, dst_off: u64) -> std::io::Result<()> {
    sys::clone_range(src, src_off, len, dst, dst_off)
}

/// Run `btrfs send` with no file data and return the raw stream.
pub fn send_no_data(parent: &Path, child: &Path) -> Result<Vec<u8>> {
    let _t = crate::trace::span("send --no-data");
    let out = Command::new("btrfs")
        .arg("send")
        .arg("-q")
        .arg("--no-data")
        .arg("-p")
        .arg(parent)
        .arg(child)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
        .context("run btrfs send")?;
    if !out.status.success() {
        bail!(
            "btrfs send --no-data -p {} {} failed: {}",
            parent.display(),
            child.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// Start a full data `btrfs send` (with an optional parent) writing to stdout of the child.
pub fn send_command(parent: Option<&Path>, child: &Path) -> Command {
    let mut c = Command::new("btrfs");
    c.arg("send").arg("-q");
    if let Some(p) = parent {
        c.arg("-p").arg(p);
    }
    c.arg(child);
    c
}
