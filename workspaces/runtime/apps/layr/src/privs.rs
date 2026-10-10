//! Acting as a user while the service runs as root.
//!
//! - File access: `setfsuid`/`setfsgid` and the user's groups change only the identity used
//!   for permission checks. Commands that read or write paths named by the caller outside the
//!   store use them, so the service cannot reach files the caller cannot reach.
//! - Child processes: a clean environment (never the service's own), the user's groups, then the
//!   user's gid and uid, all set in the child before `exec`.

use crate::ctx::Ctx;
use crate::sys;
use anyhow::{anyhow, bail, Result};
use std::process::Command;

fn root() -> bool {
    rustix::process::geteuid().is_root()
}

pub fn as_caller_fs<T>(ctx: &Ctx, f: impl FnOnce() -> Result<T>) -> Result<T> {
    as_user_fs(ctx.caller.uid, ctx.caller.gid, f)
}

/// Run `f` with the file permissions of `uid` (its groups included), then restore root's.
pub fn as_user_fs<T>(uid: u32, gid: u32, f: impl FnOnce() -> Result<T>) -> Result<T> {
    if !root() || uid == 0 {
        return f();
    }
    let mut target_groups = user(uid)?.groups;
    if !target_groups.contains(&gid) {
        target_groups.push(gid);
    }
    let old_groups = sys::groups()?;
    sys::set_groups(&target_groups)?;
    let (old_uid, old_gid) = match sys::set_fs_ids(uid, gid) {
        Ok(x) => x,
        Err(e) => {
            let _ = sys::set_groups(&old_groups);
            return Err(e);
        }
    };
    let r = f();
    let ids_ok = sys::set_fs_ids(old_uid, old_gid).is_ok();
    let groups_ok = sys::set_groups(&old_groups).is_ok();
    if !ids_ok || !groups_ok {
        return Err(anyhow!("cannot restore file system credentials"));
    }
    r
}

/// Check that the caller can write into `dir` (it exists and is writable for the caller).
pub fn check_caller_writable(ctx: &Ctx, dir: &std::path::Path) -> Result<()> {
    as_caller_fs(ctx, || {
        let probe = dir.join(format!(".layr-probe-{}-{}", std::process::id(), rand::random::<u32>()));
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().write(true).create_new(true).custom_flags(libc::O_NOFOLLOW).open(&probe)?;
        std::fs::remove_file(&probe)?;
        Ok(())
    })
    .map_err(|e| anyhow!("permission denied: cannot write {} ({e})", dir.display()))
}

pub struct User {
    pub uid: u32,
    pub gid: u32,
    pub name: String,
    pub home: String,
    pub groups: Vec<u32>,
}

/// A user and the groups it belongs to.
pub fn user(uid: u32) -> Result<User> {
    match sys::user_by_uid(uid) {
        Some(u) => {
            let groups = sys::user_groups(&u.name, u.gid);
            Ok(User { uid, gid: u.gid, name: u.name, home: u.home, groups })
        }
        None => Ok(User { uid, gid: uid, name: format!("uid{uid}"), home: "/".into(), groups: vec![uid] }),
    }
}

/// Make `cmd` run as `uid` with a clean environment: HOME, USER, LOGNAME, PATH, and `env`.
/// When the service is root and `uid` is not, the child sets the user's groups, gid and uid
/// before `exec`; nothing of the service's environment or groups reaches it.
pub fn command_as(cmd: &mut Command, uid: u32, env: &[(String, String)]) -> Result<()> {
    let u = user(uid)?;
    cmd.env_clear();
    cmd.env("HOME", &u.home).env("USER", &u.name).env("LOGNAME", &u.name).env("PATH", "/usr/local/bin:/usr/bin:/bin");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let euid = rustix::process::geteuid().as_raw();
    if euid == 0 && uid != 0 {
        sys::switch_user_before_exec(cmd, u.uid, u.gid, u.groups);
    } else if euid != uid && euid != 0 {
        bail!("cannot run a command as uid {uid}");
    }
    Ok(())
}

/// Selected variables of the caller's environment.
pub fn pick_env(ctx: &Ctx, keys: &[&str]) -> Vec<(String, String)> {
    keys.iter().filter_map(|k| ctx.env(k).map(|v| (k.to_string(), v.to_string()))).collect()
}
