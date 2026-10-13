//! Linux kernel identity, with legacy comparison only for durable journal upgrades.
use super::supervisor_daemon::{Error, Result};
use rustix::process::{Pid, Signal, getpgid, getsid, kill_process_group};
use std::{
    fs,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use studio_runtime_providers::supervisor_v1::{StartMatch, compare_start};

pub(super) const RETRY: Duration = Duration::from_millis(20);
pub(crate) fn start_time(pid: u32, allow_zombie: bool) -> Result<Option<String>> {
    if pid == 0 {
        return Ok(None);
    }
    if !cfg!(target_os = "linux") {
        return Err(Error::Refused(
            "Not implemented: requires macOS native process identity",
        ));
    }
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let tail = stat
        .rsplit_once(')')
        .ok_or(Error::Refused("Cannot verify process identity"))?
        .1;
    if !allow_zombie && tail.trim_start().starts_with('Z') {
        return Ok(None);
    }
    let ticks = tail
        .split_whitespace()
        .nth(19)
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or(Error::Refused("Cannot read kernel process start ticks"))?;
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    Ok(Some(format!("linux:{}:{ticks}", boot.trim())))
}

/// Signal a process group only while a captured member still has its exact
/// Linux boot/start identity and remains in the captured group and session.
/// Callers capture all group members before starting teardown so leader reap
/// cannot hide surviving jobs from the later KILL pass.
pub(crate) fn signal_verified_group_member(
    pid: u32,
    pgid: u32,
    sid: u32,
    expected_start: &str,
    signal: Signal,
) -> Result<bool> {
    if start_time(pid, false)?.as_deref() != Some(expected_start) {
        return Ok(false);
    }
    let member =
        Pid::from_raw(i32::try_from(pid).map_err(|_| Error::Refused("Invalid child PID"))?)
            .ok_or(Error::Refused("Invalid child PID"))?;
    let group =
        Pid::from_raw(i32::try_from(pgid).map_err(|_| Error::Refused("Invalid process group"))?)
            .ok_or(Error::Refused("Invalid process group"))?;
    let session =
        Pid::from_raw(i32::try_from(sid).map_err(|_| Error::Refused("Invalid process session"))?)
            .ok_or(Error::Refused("Invalid process session"))?;
    if getpgid(Some(member)).map_err(std::io::Error::from)? != group
        || getsid(Some(member)).map_err(std::io::Error::from)? != session
        || start_time(pid, false)?.as_deref() != Some(expected_start)
    {
        return Ok(false);
    }
    match kill_process_group(group, signal) {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::SRCH) => Ok(false),
        Err(error) => Err(std::io::Error::from(error).into()),
    }
}

pub(super) fn inspect_start(pid: u32, expected: &str) -> Result<(StartMatch, Option<String>)> {
    let actual = start_time(pid, false)?;
    let legacy = if actual.is_some() && !expected.starts_with("linux:") {
        let value = legacy_start(pid)?;
        // A PID replaced while ps ran cannot authorize an upgrade.
        if start_time(pid, false)? != actual {
            return Ok((StartMatch::Different, actual));
        }
        value
    } else {
        None
    };
    Ok((
        compare_start(expected, actual.as_deref(), legacy.as_deref()),
        actual,
    ))
}

fn legacy_start(pid: u32) -> Result<Option<String>> {
    // Read the historical Python token only during adoption, never before a signal.
    let mut child = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            return Err(Error::Refused("Process identity lookup timed out"));
        }
        thread::sleep(RETRY);
    };
    let mut output = String::new();
    child
        .stdout
        .take()
        .ok_or(Error::Refused("Missing identity output"))?
        .take(512)
        .read_to_string(&mut output)?;
    if !status.success() || output.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(output.trim().to_owned()))
}
pub(super) fn free_space(root: &Path, required: u64) -> Result<()> {
    let info = rustix::fs::statvfs(root).map_err(std::io::Error::from)?;
    if info.f_bavail.saturating_mul(info.f_frsize) < required {
        return Err(Error::Refused(
            "Insufficient free disk space for a durable supervisor receipt",
        ));
    }
    Ok(())
}
pub(super) fn stop_verified(pid: u32, pgid: u32, expected: &str) -> Result<&'static str> {
    let process = verify_group(pid, pgid)?;
    if !expected.starts_with("linux:") {
        return Err(Error::Refused(
            "Legacy child identity requires durable upgrade before signal",
        ));
    }
    for (signal, timeout, outcome) in [
        (Signal::TERM, Duration::from_millis(1500), "terminated"),
        (Signal::KILL, Duration::from_secs(1), "killed"),
    ] {
        if start_time(pid, false)?.as_deref() != Some(expected) {
            return Err(Error::Refused("Child identity changed before signal"));
        }
        match kill_process_group(process, signal) {
            Ok(()) => {}
            Err(rustix::io::Errno::SRCH) => return Ok("already-exited"),
            Err(e) => return Err(std::io::Error::from(e).into()),
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if start_time(pid, false)?.as_deref() != Some(expected) {
                return Ok(outcome);
            }
            thread::sleep(RETRY);
        }
    }
    Err(Error::Refused(
        "Could not terminate verified child process group",
    ))
}

pub(super) fn verify_group(pid: u32, pgid: u32) -> Result<Pid> {
    let raw = i32::try_from(pid).map_err(|_| Error::Refused("Invalid child PID"))?;
    let process = Pid::from_raw(raw).ok_or(Error::Refused("Invalid child PID"))?;
    if pid != pgid || getpgid(Some(process)).map_err(std::io::Error::from)? != process {
        return Err(Error::Refused("Child process group identity changed"));
    }
    Ok(process)
}

pub(super) fn valid_start(value: &str) -> bool {
    value.strip_prefix("linux:").is_some_and(|rest| {
        rest.split_once(':').is_some_and(|(boot, ticks)| {
            boot.len() == 36
                && boot.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-')
                && ticks.parse::<u64>().is_ok()
        })
    }) || chrono::NaiveDateTime::parse_from_str(value, "%a %b %e %H:%M:%S %Y").is_ok()
        || value.split_once('.').is_some_and(|(seconds, micros)| {
            seconds.parse::<u64>().is_ok_and(|seconds| seconds > 0)
                && micros.len() == 6
                && micros.bytes().all(|c| c.is_ascii_digit())
        })
}
