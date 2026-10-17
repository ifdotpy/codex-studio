//! The only module with `unsafe` code: system calls that `rustix` and the standard library do not
//! wrap. Every other module uses safe wrappers.
//!
//! - btrfs ioctls: create, snapshot and destroy subvolumes, read and set subvolume flags,
//!   subvolume information,
//!   and `FICLONERANGE`;
//! - `fork` (the service forks one process per request while it is single threaded);
//! - switching a child process to a user before `exec` (groups, gid, uid);
//! - `setfsuid`/`setfsgid` (file permission checks as a user, for the current thread);
//! - user database lookups (`getpwuid_r`, `getpwnam_r`, `getgrouplist`).

use anyhow::{anyhow, Context, Result};
use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::process::Command;

// ----- btrfs ioctls -----

const SNAP_CREATE_V2: libc::c_ulong = 0x5000_9417;
const SUBVOL_CREATE_V2: libc::c_ulong = 0x5000_9418;
const SNAP_DESTROY_V2: libc::c_ulong = 0x5000_943f;
const SUBVOL_GETFLAGS: libc::c_ulong = 0x8008_9419;
const SUBVOL_SETFLAGS: libc::c_ulong = 0x4008_941a;
const GET_SUBVOL_INFO: libc::c_ulong = 0x81f8_943c;
const FICLONERANGE: libc::c_ulong = 0x4020_940d;
pub const SUBVOL_RDONLY: u64 = 1 << 1;

#[repr(C)]
struct VolArgsV2 {
    fd: i64,
    transid: u64,
    flags: u64,
    unused: [u64; 4],
    name: [u8; 4040],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Timespec {
    sec: u64,
    nsec: u32,
}

#[repr(C)]
struct SubvolInfoArgs {
    treeid: u64,
    name: [u8; 256],
    parent_id: u64,
    dirid: u64,
    generation: u64,
    flags: u64,
    uuid: [u8; 16],
    parent_uuid: [u8; 16],
    received_uuid: [u8; 16],
    ctransid: u64,
    otransid: u64,
    stransid: u64,
    rtransid: u64,
    ctime: Timespec,
    otime: Timespec,
    stime: Timespec,
    rtime: Timespec,
    reserved: [u64; 8],
}

#[repr(C)]
struct CloneRange {
    src_fd: i64,
    src_offset: u64,
    src_length: u64,
    dest_offset: u64,
}

fn vol_args(fd: i64, flags: u64, name: &str) -> Result<Box<VolArgsV2>> {
    let b = name.as_bytes();
    if b.is_empty() || b.len() >= 4040 || b.contains(&0) || b.contains(&b'/') {
        return Err(anyhow!("bad subvolume name {name:?}"));
    }
    let mut a = Box::new(VolArgsV2 { fd, transid: 0, flags, unused: [0; 4], name: [0; 4040] });
    a.name[..b.len()].copy_from_slice(b);
    Ok(a)
}

fn ioctl_ptr<T>(fd: BorrowedFd, req: libc::c_ulong, arg: &mut T) -> io::Result<()> {
    // SAFETY: `fd` is a valid open descriptor for the duration of the call, `arg` points to a
    // properly sized and aligned argument structure of the request.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), req as _, arg as *mut T) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Create the subvolume `name` in the folder `dir`.
pub fn subvol_create(dir: BorrowedFd, name: &str) -> Result<()> {
    let mut a = vol_args(0, 0, name)?;
    ioctl_ptr(dir, SUBVOL_CREATE_V2, &mut *a).with_context(|| format!("create subvolume {name}"))
}

/// Snapshot the subvolume `src` as `name` in the folder `dir`.
pub fn subvol_snapshot(src: BorrowedFd, dir: BorrowedFd, name: &str, readonly: bool) -> Result<()> {
    let mut a = vol_args(src.as_raw_fd() as i64, if readonly { SUBVOL_RDONLY } else { 0 }, name)?;
    ioctl_ptr(dir, SNAP_CREATE_V2, &mut *a).with_context(|| format!("snapshot to {name}"))
}

/// Destroy the subvolume `name` in the folder `dir`.
pub fn subvol_destroy(dir: BorrowedFd, name: &str) -> Result<()> {
    let mut a = vol_args(0, 0, name)?;
    ioctl_ptr(dir, SNAP_DESTROY_V2, &mut *a).with_context(|| format!("delete subvolume {name}"))
}

pub fn subvol_flags(fd: BorrowedFd) -> io::Result<u64> {
    let mut flags: u64 = 0;
    ioctl_ptr(fd, SUBVOL_GETFLAGS, &mut flags)?;
    Ok(flags)
}

#[cfg(target_os = "linux")]
pub fn filesystem_uuid(fd: BorrowedFd) -> io::Result<[u8; 16]> {
    #[repr(C)]
    struct FsInfo {
        max_id: u64,
        devices: u64,
        uuid: [u8; 16],
        rest: [u8; 992],
    }
    let mut info = FsInfo { max_id: 0, devices: 0, uuid: [0; 16], rest: [0; 992] };
    ioctl_ptr(fd, 0x8400_941f, &mut info)?;
    Ok(info.uuid)
}

#[cfg(target_os = "linux")]
/// Set the flags of a subvolume (`SUBVOL_RDONLY` makes it read-only).
pub fn subvol_set_flags(fd: BorrowedFd, flags: u64) -> io::Result<()> {
    let mut f: u64 = flags;
    ioctl_ptr(fd, SUBVOL_SETFLAGS, &mut f)
}

pub struct RawSubvolInfo {
    pub tree_id: u64,
    pub generation: u64,
    pub uuid: [u8; 16],
    pub parent_uuid: [u8; 16],
    pub received_uuid: [u8; 16],
    pub ctransid: u64,
    pub stransid: u64,
}

pub fn subvol_info(fd: BorrowedFd) -> io::Result<RawSubvolInfo> {
    // SAFETY: all-zero bytes are a valid value of this plain C structure.
    let mut a: Box<SubvolInfoArgs> = Box::new(unsafe { std::mem::zeroed() });
    ioctl_ptr(fd, GET_SUBVOL_INFO, &mut *a)?;
    Ok(RawSubvolInfo {
        tree_id: a.treeid,
        generation: a.generation,
        uuid: a.uuid,
        parent_uuid: a.parent_uuid,
        received_uuid: a.received_uuid,
        ctransid: a.ctransid,
        stransid: a.stransid,
    })
}

/// Produce a version 1 metadata stream through an open pipe.
#[cfg(target_os = "linux")]
pub fn send_no_data(src: BorrowedFd, out: BorrowedFd, parent_root: u64) -> io::Result<()> {
    #[repr(C)]
    struct SendArgs {
        send_fd: i64,
        clone_sources_count: u64,
        clone_sources: u64,
        parent_root: u64,
        flags: u64,
        version: u32,
        reserved: [u8; 28],
    }
    // BTRFS_SEND_FLAG_NO_FILE_DATA. Version zero selects the original protocol.
    let mut args = SendArgs {
        send_fd: out.as_raw_fd() as i64,
        clone_sources_count: 0,
        clone_sources: 0,
        parent_root,
        flags: 1,
        version: 0,
        reserved: [0; 28],
    };
    ioctl_ptr(src, 0x4048_9426, &mut args)
}

/// Reflink a byte range from `src` into `dst`.
pub fn clone_range(src: &impl AsFd, src_off: u64, len: u64, dst: &impl AsFd, dst_off: u64) -> io::Result<()> {
    let mut a = CloneRange { src_fd: src.as_fd().as_raw_fd() as i64, src_offset: src_off, src_length: len, dest_offset: dst_off };
    ioctl_ptr(dst.as_fd(), FICLONERANGE, &mut a)
}

// ----- processes -----

pub enum Fork {
    Child,
    Parent(i32),
}

/// `fork`. Only call it while the process has one thread (the service loop).
pub fn fork() -> io::Result<Fork> {
    // SAFETY: the service calls this from its single-threaded main loop, so the child is a
    // complete copy of a process with one thread and may run normal Rust code.
    match unsafe { libc::fork() } {
        -1 => Err(io::Error::last_os_error()),
        0 => Ok(Fork::Child),
        pid => Ok(Fork::Parent(pid)),
    }
}

/// Make the child of `cmd` drop to a user before `exec`: its groups, then gid, then uid.
#[cfg(target_os = "linux")]
pub fn switch_user_before_exec(cmd: &mut Command, uid: u32, gid: u32, groups: Vec<u32>) {
    use std::os::unix::process::CommandExt;
    let gids: Vec<rustix::process::Gid> = groups.iter().map(|g| rustix::process::Gid::from_raw(*g)).collect();
    // SAFETY: the closure runs in the child between fork and exec. It only makes the
    // async-signal-safe system calls setgroups, setgid and setuid (raw, through rustix, which
    // does not allocate here) and reads values that were moved into it before the fork.
    unsafe {
        cmd.pre_exec(move || {
            rustix::thread::set_thread_groups(&gids)?;
            rustix::thread::set_thread_gid(rustix::process::Gid::from_raw(gid))?;
            rustix::thread::set_thread_uid(rustix::process::Uid::from_raw(uid))?;
            if rustix::process::geteuid().as_raw() != uid {
                return Err(io::Error::other("uid did not change"));
            }
            Ok(())
        });
    }
}

/// Set the file system uid and gid of the calling thread; returns the previous ids.
#[cfg(target_os = "linux")]
pub fn set_fs_ids(uid: u32, gid: u32) -> Result<(u32, u32)> {
    // SAFETY: setfsgid/setfsuid have no memory arguments; the second call with the same value
    // returns the current id, which confirms the change.
    unsafe {
        let old_gid = libc::setfsgid(gid) as u32;
        if libc::setfsgid(gid) as u32 != gid {
            return Err(anyhow!("cannot set the file system gid to {gid}"));
        }
        let old_uid = libc::setfsuid(uid) as u32;
        if libc::setfsuid(uid) as u32 != uid {
            libc::setfsgid(old_gid);
            return Err(anyhow!("cannot set the file system uid to {uid}"));
        }
        Ok((old_uid, old_gid))
    }
}

/// Set the supplementary groups of the process (the service child is single threaded).
pub fn set_groups(groups: &[u32]) -> io::Result<()> {
    // Darwin stores at most NGROUPS_MAX (16) supplementary groups. Its directory
    // service can return more memberships than the kernel credential can contain.
    #[cfg(target_os = "macos")]
    let groups = &groups[..groups.len().min(16)];
    // SAFETY: the pointer and length describe a valid slice of gid_t for the call.
    let rc = unsafe { libc::setgroups(groups.len() as _, groups.as_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn groups() -> io::Result<Vec<u32>> {
    // SAFETY: a first call with a zero size returns the count; the second fills a buffer of
    // that size.
    unsafe {
        let n = libc::getgroups(0, std::ptr::null_mut());
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut v = vec![0u32; n as usize];
        if n > 0 && libc::getgroups(n, v.as_mut_ptr()) < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(v)
    }
}

// ----- the user database -----

pub struct UserInfo {
    pub uid: u32,
    pub gid: u32,
    pub name: String,
    pub home: String,
}

fn passwd(by_uid: Option<u32>, by_name: Option<&str>) -> Option<UserInfo> {
    let mut buf = vec![0u8; 16384];
    // SAFETY: getpwuid_r/getpwnam_r write into `pw` and `buf` (sizes passed) and set `result`
    // to `&pw` on success; the strings are read before `buf` goes out of scope.
    unsafe {
        let mut pw: libc::passwd = std::mem::zeroed();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = match (by_uid, by_name) {
            (Some(u), _) => libc::getpwuid_r(u, &mut pw, buf.as_mut_ptr() as *mut libc::c_char, buf.len(), &mut result),
            (None, Some(n)) => {
                let c = CString::new(n).ok()?;
                libc::getpwnam_r(c.as_ptr(), &mut pw, buf.as_mut_ptr() as *mut libc::c_char, buf.len(), &mut result)
            }
            _ => return None,
        };
        if rc != 0 || result.is_null() {
            return None;
        }
        Some(UserInfo {
            uid: pw.pw_uid,
            gid: pw.pw_gid,
            name: CStr::from_ptr(pw.pw_name).to_string_lossy().into_owned(),
            home: CStr::from_ptr(pw.pw_dir).to_string_lossy().into_owned(),
        })
    }
}

pub fn user_by_uid(uid: u32) -> Option<UserInfo> {
    passwd(Some(uid), None)
}

pub fn user_by_name(name: &str) -> Option<UserInfo> {
    passwd(None, Some(name))
}

/// The groups of a user (its primary group included).
pub fn user_groups(name: &str, gid: u32) -> Vec<u32> {
    let c = match CString::new(name) {
        Ok(c) => c,
        Err(_) => return vec![gid],
    };
    let mut groups: Vec<libc::gid_t> = vec![0; 1024];
    let mut n: libc::c_int = groups.len() as libc::c_int;
    // SAFETY: `groups` has room for `n` entries; getgrouplist writes at most `n` and stores the
    // real count in `n`.
    let rc = unsafe { libc::getgrouplist(c.as_ptr(), gid as _, groups.as_mut_ptr().cast(), &mut n) };
    if rc < 0 {
        return vec![gid];
    }
    groups.truncate(n as usize);
    groups
}

#[cfg(target_os = "macos")]
pub fn switch_user_before_exec(cmd: &mut Command, uid: u32, gid: u32, mut groups: Vec<u32>) {
    use std::os::unix::process::CommandExt;
    // Match Darwin's fixed kernel group limit, as in set_groups.
    groups.truncate(16);
    // SAFETY: the child calls only async-signal-safe credential syscalls before exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setgroups(groups.len() as _, groups.as_ptr()) != 0 || libc::setgid(gid) != 0 || libc::setuid(uid) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::geteuid() != uid {
                return Err(io::Error::other("uid did not change"));
            }
            Ok(())
        });
    }
}
#[cfg(target_os = "macos")]
pub fn set_fs_ids(uid: u32, gid: u32) -> Result<(u32, u32)> {
    // The daemon forks a single-threaded command child. macOS has no setfsuid;
    // temporary effective IDs restrict all its file access, with saved root retained.
    // SAFETY: these syscalls have no pointer arguments. Restore root before changing gid.
    unsafe {
        let old = (libc::geteuid(), libc::getegid());
        if uid == 0 && libc::seteuid(0) != 0 {
            return Err(io::Error::last_os_error().into());
        }
        if libc::setegid(gid) != 0 {
            return Err(io::Error::last_os_error().into());
        }
        if libc::seteuid(uid) != 0 {
            let error = io::Error::last_os_error();
            libc::setegid(old.1);
            return Err(error.into());
        }
        Ok(old)
    }
}

#[cfg(target_os = "macos")]
fn cpath(path: &std::path::Path) -> io::Result<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))
}
#[cfg(target_os = "macos")]
pub fn apfs_supported(path: &std::path::Path) -> bool {
    let Ok(path) = cpath(path) else {
        return false;
    };
    // SAFETY: statfs initializes a valid native structure; path is NUL-terminated.
    unsafe {
        let mut stat: libc::statfs = std::mem::zeroed();
        libc::statfs(path.as_ptr(), &mut stat) == 0
            && CStr::from_ptr(stat.f_fstypename.as_ptr()).to_bytes() == b"apfs"
            && libc::pathconf(path.as_ptr(), libc::_PC_CASE_SENSITIVE) == 1
    }
}
#[cfg(target_os = "macos")]
pub fn apfs_clone(src: &std::path::Path, dst: &std::path::Path) -> io::Result<()> {
    use rustix::fs::{open, openat, Mode, OFlags};
    use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};
    use std::path::Component;
    fn open_parent(path: &std::path::Path) -> io::Result<std::os::fd::OwnedFd> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut dir = open(if path.is_absolute() { "/" } else { "." }, flags, Mode::empty())?;
        for part in path.parent().ok_or_else(|| io::Error::other("missing clone parent"))?.components() {
            match part {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => dir = openat(&dir, name, flags, Mode::empty())?,
                _ => return Err(io::Error::other("invalid clone path component")),
            }
        }
        Ok(dir)
    }
    let source_dir = open_parent(src)?;
    let source = openat(
        &source_dir,
        src.file_name().ok_or_else(|| io::Error::other("missing clone source name"))?,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    let destination = open_parent(dst)?;
    let name = CString::new(dst.file_name().ok_or_else(|| io::Error::other("missing clone destination name"))?.as_bytes())?;
    // SAFETY: the descriptors stay live. Each path component was opened without following
    // links. The destination is a single name. fclonefileat also supports macOS 14.
    if unsafe { libc::fclonefileat(source.as_raw_fd(), destination.as_raw_fd(), name.as_ptr(), 0x0002) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
#[cfg(target_os = "macos")]
pub fn apfs_marker(path: &std::path::Path, value: Option<&[u8]>) -> io::Result<Vec<u8>> {
    apfs_attribute(path, c"com.layr.subvolume", value)
}
#[cfg(target_os = "macos")]
pub fn apfs_attribute(path: &std::path::Path, name: &CStr, value: Option<&[u8]>) -> io::Result<Vec<u8>> {
    let path = cpath(path)?;
    // SAFETY: paths and buffers remain live for the calls, the buffer length is bounded.
    unsafe {
        if let Some(v) = value {
            if libc::setxattr(path.as_ptr(), name.as_ptr(), v.as_ptr().cast(), v.len(), 0, libc::XATTR_NOFOLLOW) != 0 {
                return Err(io::Error::last_os_error());
            }
            return Ok(Vec::new());
        }
        let mut v = vec![0u8; 4096];
        let n = libc::getxattr(path.as_ptr(), name.as_ptr(), v.as_mut_ptr().cast(), v.len(), 0, libc::XATTR_NOFOLLOW);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        v.truncate(n as usize);
        Ok(v)
    }
}
#[cfg(target_os = "macos")]
pub fn apfs_immutable(path: &std::path::Path, readonly: bool) -> io::Result<()> {
    use std::os::macos::fs::MetadataExt;
    let flags = std::fs::symlink_metadata(path)?.st_flags();
    let next = if readonly { flags | libc::UF_IMMUTABLE } else { flags & !libc::UF_IMMUTABLE };
    if next == flags {
        return Ok(());
    }
    let flags = next;
    let path = cpath(path)?;
    // SAFETY: the path is valid; lchflags changes the entry itself, never the target of a link.
    if unsafe { libc::lchflags(path.as_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn apfs_publish(src: &std::path::Path, dst: &std::path::Path) -> io::Result<()> {
    let (src, dst) = (cpath(src)?, cpath(dst)?);
    // SAFETY: both paths are valid C strings. RENAME_EXCL never replaces an existing entry.
    if unsafe { libc::renamex_np(src.as_ptr(), dst.as_ptr(), libc::RENAME_EXCL) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn apfs_copy_metadata(src: &std::path::Path, dst: &std::path::Path) -> io::Result<()> {
    let (src, dst) = (cpath(src)?, cpath(dst)?);
    // SAFETY: both paths are valid C strings. NOFOLLOW applies to both final entries.
    if unsafe {
        libc::copyfile(src.as_ptr(), dst.as_ptr(), std::ptr::null_mut(), libc::COPYFILE_METADATA | libc::COPYFILE_NOFOLLOW)
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn apfs_clone_at(src: std::os::fd::BorrowedFd<'_>, dir: std::os::fd::BorrowedFd<'_>, name: &str) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let name = CString::new(name)?;
    // SAFETY: both descriptors are live; name is one NUL-terminated component.
    if unsafe { libc::fclonefileat(src.as_raw_fd(), dir.as_raw_fd(), name.as_ptr(), 0x0002) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
#[cfg(target_os = "macos")]
pub fn apfs_unseal_fd(fd: std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: fd is live; fstat initializes the structure before its flags are read.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd.as_raw_fd(), &mut st) != 0 {
            return Err(io::Error::last_os_error());
        }
        if st.st_flags & libc::UF_IMMUTABLE != 0 && libc::fchflags(fd.as_raw_fd(), st.st_flags & !libc::UF_IMMUTABLE) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn apfs_xattrs(path: &std::path::Path) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let cp = cpath(path)?;
    // SAFETY: the path and the allocated buffers remain valid during each call.
    unsafe {
        let n = libc::listxattr(cp.as_ptr(), std::ptr::null_mut(), 0, libc::XATTR_NOFOLLOW);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut names = vec![0u8; n as usize];
        let n = libc::listxattr(cp.as_ptr(), names.as_mut_ptr().cast(), names.len(), libc::XATTR_NOFOLLOW);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        names.truncate(n as usize);
        let mut result = Vec::new();
        for name in names.split(|b| *b == 0).filter(|s| !s.is_empty()) {
            let cn = std::ffi::CString::new(name)?;
            let n = libc::getxattr(cp.as_ptr(), cn.as_ptr(), std::ptr::null_mut(), 0, 0, libc::XATTR_NOFOLLOW);
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut value = vec![0u8; n as usize];
            let n = libc::getxattr(cp.as_ptr(), cn.as_ptr(), value.as_mut_ptr().cast(), value.len(), 0, libc::XATTR_NOFOLLOW);
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            value.truncate(n as usize);
            result.push((name.to_vec(), value));
        }
        Ok(result)
    }
}

#[cfg(target_os = "macos")]
pub fn apfs_remove_attribute(path: &std::path::Path, name: &CStr) -> io::Result<()> {
    let path = cpath(path)?;
    // SAFETY: both strings remain live; NOFOLLOW addresses the link itself.
    if unsafe { libc::removexattr(path.as_ptr(), name.as_ptr(), libc::XATTR_NOFOLLOW) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
#[cfg(target_os = "macos")]
pub fn apfs_mtime(path: &std::path::Path, sec: i64, nsec: i64) -> io::Result<()> {
    let path = cpath(path)?;
    let times = [libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT }, libc::timespec { tv_sec: sec, tv_nsec: nsec }];
    // SAFETY: the path and two timespec values remain live for the call.
    if unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn peer_credentials(sock: &std::os::unix::net::UnixStream) -> io::Result<(u32, u32)> {
    use std::os::fd::AsRawFd;
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: the socket is live, and both output pointers refer to writable uid/gid values.
    if unsafe { libc::getpeereid(sock.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((uid, gid))
}

#[cfg(target_os = "macos")]
pub fn apfs_owners_enabled(path: &std::path::Path) -> io::Result<bool> {
    let path = cpath(path)?;
    let mut st = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: statfs initializes the structure on success.
    if unsafe { libc::statfs(path.as_ptr(), st.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let st = unsafe { st.assume_init() };
    Ok(st.f_flags & (libc::MNT_IGNORE_OWNERSHIP as u32) == 0)
}

#[cfg(target_os = "macos")]
pub fn apfs_has_acl(path: &std::path::Path) -> io::Result<bool> {
    let path = cpath(path)?;
    // SAFETY: CHECK inspects only the live source string and does not use a destination.
    let result = unsafe {
        libc::copyfile(
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            libc::COPYFILE_CHECK | libc::COPYFILE_ACL | libc::COPYFILE_NOFOLLOW,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(result as u32 & libc::COPYFILE_ACL != 0)
}
#[cfg(target_os = "macos")]
pub fn apfs_clear_acl(path: &std::path::Path) -> io::Result<()> {
    unsafe extern "C" {
        fn acl_init(count: libc::c_int) -> *mut libc::c_void;
        fn acl_set_link_np(path: *const libc::c_char, kind: libc::c_int, acl: *mut libc::c_void) -> libc::c_int;
        fn acl_free(ptr: *mut libc::c_void) -> libc::c_int;
    }
    let path = cpath(path)?;
    // SAFETY: an empty ACL is allocated, applied without following a link, and freed once.
    unsafe {
        let acl = acl_init(0);
        if acl.is_null() {
            return Err(io::Error::last_os_error());
        }
        let result = acl_set_link_np(path.as_ptr(), 0x100, acl);
        let error = io::Error::last_os_error();
        acl_free(acl);
        if result != 0 {
            return Err(error);
        }
    }
    Ok(())
}

// ----- OverlayFS mount and metadata operations -----
#[cfg(feature = "overlayfs")]
fn overlay_path(path: &std::path::Path) -> io::Result<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::other("NUL in path"))
}
#[cfg(feature = "overlayfs")]
pub fn overlay_mount(
    source: Option<&std::path::Path>,
    target: &std::path::Path,
    kind: Option<&CStr>,
    flags: libc::c_ulong,
    options: Option<&str>,
) -> io::Result<()> {
    let source = source.map(overlay_path).transpose()?;
    let target = overlay_path(target)?;
    let options = options.map(CString::new).transpose().map_err(|_| io::Error::other("NUL in mount options"))?;
    // SAFETY: all strings and optional pointers remain valid for mount.
    let rc = unsafe {
        libc::mount(
            source.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            target.as_ptr(),
            kind.map_or(std::ptr::null(), |s| s.as_ptr()),
            flags,
            options.as_ref().map_or(std::ptr::null(), |s| s.as_ptr().cast()),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
#[cfg(feature = "overlayfs")]
pub fn overlay_unmount(path: &std::path::Path) -> io::Result<()> {
    let p = overlay_path(path)?;
    // SAFETY: p is a terminated path. Never use lazy detach for managed data.
    if unsafe { libc::umount2(p.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
#[cfg(feature = "overlayfs")]
pub fn overlay_attribute(path: &std::path::Path, name: &CStr, value: Option<&[u8]>) -> io::Result<Vec<u8>> {
    let p = overlay_path(path)?;
    if let Some(value) = value {
        // SAFETY: path, name, and the input buffer are valid for the call.
        if unsafe { libc::lsetxattr(p.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(Vec::new());
    }
    // SAFETY: null buffer requests the attribute size.
    let n = unsafe { libc::lgetxattr(p.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = vec![0; n as usize];
    // SAFETY: the output buffer has the supplied capacity.
    let n = unsafe { libc::lgetxattr(p.as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(n as usize);
    Ok(buf)
}
#[cfg(feature = "overlayfs")]
pub fn overlay_remove_attribute(path: &std::path::Path, name: &CStr) -> io::Result<()> {
    let p = overlay_path(path)?;
    // SAFETY: both strings remain valid for the call.
    if unsafe { libc::lremovexattr(p.as_ptr(), name.as_ptr()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
#[cfg(feature = "overlayfs")]
pub fn overlay_xattrs(path: &std::path::Path) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let p = overlay_path(path)?;
    // SAFETY: null buffer requests the list size.
    let n = unsafe { libc::llistxattr(p.as_ptr(), std::ptr::null_mut(), 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut names = vec![0u8; n as usize];
    // SAFETY: names is writable for its supplied length.
    let n = unsafe { libc::llistxattr(p.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    names.truncate(n as usize);
    names
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|n| {
            let name = CString::new(n)?;
            Ok((n.to_vec(), overlay_attribute(path, &name, None)?))
        })
        .collect()
}
#[cfg(feature = "overlayfs")]
pub fn overlay_mtime(path: &std::path::Path, sec: i64, nano: i64) -> io::Result<()> {
    let p = overlay_path(path)?;
    let times = [libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT }, libc::timespec { tv_sec: sec, tv_nsec: nano }];
    // SAFETY: the two timespec entries and path remain valid for the call.
    if unsafe { libc::utimensat(libc::AT_FDCWD, p.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
