//! The only module with `unsafe` code: system calls that `rustix` and the standard library do not
//! wrap. Every other module uses safe wrappers.
//!
//! - btrfs ioctls: create, snapshot and destroy subvolumes, subvolume flags and information,
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

pub struct RawSubvolInfo {
    pub tree_id: u64,
    pub generation: u64,
    pub uuid: [u8; 16],
    pub parent_uuid: [u8; 16],
    pub received_uuid: [u8; 16],
}

pub fn subvol_info(fd: BorrowedFd) -> io::Result<RawSubvolInfo> {
    // SAFETY: all-zero bytes are a valid value of this plain C structure.
    let mut a: Box<SubvolInfoArgs> = Box::new(unsafe { std::mem::zeroed() });
    ioctl_ptr(fd, GET_SUBVOL_INFO, &mut *a)?;
    Ok(RawSubvolInfo { tree_id: a.treeid, generation: a.generation, uuid: a.uuid, parent_uuid: a.parent_uuid, received_uuid: a.received_uuid })
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
    // SAFETY: the pointer and length describe a valid slice of gid_t for the call.
    let rc = unsafe { libc::setgroups(groups.len(), groups.as_ptr()) };
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
    let rc = unsafe { libc::getgrouplist(c.as_ptr(), gid, groups.as_mut_ptr(), &mut n) };
    if rc < 0 {
        return vec![gid];
    }
    groups.truncate(n as usize);
    groups
}
