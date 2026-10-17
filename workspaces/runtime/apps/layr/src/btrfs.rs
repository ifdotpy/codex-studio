//! btrfs operations: subvolumes, snapshots, reflinks (through `sys` and `rustix`), and the
//! `btrfs` command for send streams.

use crate::sys;
use anyhow::{anyhow, bail, Context, Result};
use std::fs::File;
use std::io::Read;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::Command;

const BTRFS_SUPER_MAGIC: i64 = 0x9123_683e;
const FIRST_FREE_OBJECTID: u64 = 256;

thread_local! {
    // New snapshots can still be discarded by this command. Do not persist their diffs.
    static NEW_SNAPSHOTS: std::cell::RefCell<std::collections::HashSet<PathBuf>> = Default::default();
}

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
pub fn is_supported(path: &Path) -> bool {
    match rustix::fs::statfs(path) {
        Ok(st) => st.f_type as i64 == BTRFS_SUPER_MAGIC,
        Err(_) => false,
    }
}

/// True when `path` is the root of a subvolume.
pub fn is_subvolume(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match std::fs::symlink_metadata(path) {
        Ok(m) => m.is_dir() && m.ino() == FIRST_FREE_OBJECTID && is_supported(path),
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
        .with_context(|| format!("snapshot {} to {}", src.display(), dst.display()))?;
    if readonly {
        NEW_SNAPSHOTS.with(|paths| paths.borrow_mut().insert(dst.to_path_buf()));
    }
    Ok(())
}

/// Delete the subvolume at `path`. Nested subvolumes must be deleted first.
pub fn delete_subvolume(path: &Path) -> Result<()> {
    let (dir, name) = split_parent(path)?;
    sys::subvol_destroy(dir.as_fd(), &name).with_context(|| format!("delete subvolume {}", path.display()))
}

/// Make a subvolume read-only (a writable snapshot becomes a state after its changes).
pub fn set_readonly(path: &Path) -> Result<()> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let flags = sys::subvol_flags(f.as_fd())?;
    sys::subvol_set_flags(f.as_fd(), flags | sys::SUBVOL_RDONLY).with_context(|| format!("make {} read-only", path.display()))
}

/// Make a read-only subvolume writable again (btrfs does not rename a read-only subvolume).
pub fn set_writable(path: &Path) -> Result<()> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let flags = sys::subvol_flags(f.as_fd())?;
    sys::subvol_set_flags(f.as_fd(), flags & !sys::SUBVOL_RDONLY).with_context(|| format!("make {} writable", path.display()))
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
        // its .git folder or caches) answers ENOTEMPTY; then the nested ones are found and
        // destroyed first, through descriptors.
        let (dir, name) = split_parent(path)?;
        match sys::subvol_destroy(dir.as_fd(), &name) {
            Ok(()) => return Ok(()),
            Err(e) if is_not_empty(&e) => {
                let t = crate::tree::Tree::open(path)?;
                // The usual nested subvolume first (the .git folder), then a full search.
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

/// Ask the kernel for a metadata stream, or read an immutable pair from the cache.
pub fn send_no_data(parent: &Path, child: &Path) -> Result<Vec<u8>> {
    let _t = crate::trace::span("send --no-data");
    let p = File::open(parent)?;
    let c = File::open(child)?;
    if sys::filesystem_uuid(p.as_fd())? != sys::filesystem_uuid(c.as_fd())? {
        bail!("the snapshots are on different filesystems");
    }
    let is_new = NEW_SNAPSHOTS.with(|paths| paths.borrow().contains(parent) || paths.borrow().contains(child));
    let cache = if is_new { None } else { crate::send_cache::Cache::for_states(parent, child, &p, &c) };
    if let Some(data) = cache.as_ref().and_then(|cache| cache.read()) {
        return Ok(data);
    }
    let parent_id = sys::subvol_info(p.as_fd())?.tree_id;
    let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
    let data = std::thread::scope(|scope| {
        let writer = std::thread::Builder::new()
            .spawn_scoped(scope, move || {
                let result = sys::send_no_data(c.as_fd(), write.as_fd(), parent_id);
                drop(write);
                result
            })
            .context("start metadata stream thread")?;
        let mut data = Vec::new();
        let mut reader = File::from(read);
        let result = reader.read_to_end(&mut data);
        // Close before joining, including on a read error, so the producer cannot block.
        drop(reader);
        let sent = writer.join().map_err(|_| anyhow!("metadata stream thread failed"))?;
        result.context("read metadata stream")?;
        sent.context("send metadata stream")?;
        Ok::<_, anyhow::Error>(data)
    })?;
    if let Some(cache) = cache {
        // Cache failures do not change command results.
        let _ = cache.write(&data);
    }
    Ok(data)
}

/// Start a full data `btrfs send` (with an optional parent) writing to stdout of the child.
pub fn send_command(parent: Option<&Path>, child: &Path) -> Result<Command> {
    send_optimized(parent, child, &[], false)
}

/// Extra sources and compressed extents require an explicit agreement with the receiver.
pub fn send_optimized(parent: Option<&Path>, child: &Path, clones: &[PathBuf], compressed: bool) -> Result<Command> {
    let mut c = Command::new("btrfs");
    c.arg("send").arg("-q");
    if compressed && supports_compressed_send() {
        c.arg("--proto").arg("2").arg("--compressed-data");
    }
    if let Some(p) = parent {
        c.arg("-p").arg(p);
    }
    for source in clones {
        c.arg("-c").arg(source);
    }
    c.arg(child);
    Ok(c)
}

fn tools_version() -> Option<(u32, u32)> {
    static VERSION: std::sync::OnceLock<Option<(u32, u32)>> = std::sync::OnceLock::new();
    *VERSION.get_or_init(|| {
        let out = Command::new("btrfs").arg("version").output().ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8_lossy(&out.stdout).split_whitespace().find_map(|word| {
            let mut parts = word.strip_prefix('v')?.split(|c: char| !c.is_ascii_digit());
            Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
        })
    })
}

pub fn supports_compressed_receive() -> bool {
    tools_version().is_some_and(|v| v >= (6, 0))
}

// Before 6.11, receive searches clone sources only by received UUID, not local UUID.
pub fn supports_local_clone_receive() -> bool {
    tools_version().is_some_and(|v| v >= (6, 11))
}

fn supports_compressed_send() -> bool {
    supports_compressed_receive()
        && std::fs::read_to_string("/sys/fs/btrfs/features/send_stream_version")
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .is_some_and(|version| version >= 2)
}

/// An immutable source identity accepted by the receiver. Local UUIDs do not need a remote transaction ID.
pub fn clone_identity(path: &Path) -> Result<(String, Option<u64>)> {
    let f = File::open(path)?;
    if sys::subvol_flags(f.as_fd())? & sys::SUBVOL_RDONLY == 0 {
        bail!("clone source is not read-only");
    }
    let info = sys::subvol_info(f.as_fd())?;
    Ok(match fmt_uuid(&info.received_uuid) {
        Some(uuid) => (uuid, Some(info.stransid)),
        // A return stream carries the remote ctransid. The original local UUID
        // identifies its source without a transaction from the other filesystem.
        None => (fmt_uuid(&info.uuid).ok_or_else(|| anyhow!("clone source has no UUID"))?, None),
    })
}

/// The kernel writes its local ctransid even when the UUID comes from a received snapshot.
pub fn send_clone_identity(path: &Path) -> Result<(String, u64)> {
    let (uuid, _) = clone_identity(path)?;
    let f = File::open(path)?;
    Ok((uuid, sys::subvol_info(f.as_fd())?.ctransid))
}

/// Identity retained by btrfs snapshots, used to stage file moves.
pub fn file_identity(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::symlink_metadata(path)?.ino().to_le_bytes().to_vec())
}
