//! Linux identity proof compatible with Python's journal. No signal without a match.
use super::supervisor_daemon::{Error, Result};
use rustix::process::{Pid, Signal, getpgid, kill_process_group};
use std::{
    fs,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub(super) const RETRY: Duration = Duration::from_millis(20);
pub(super) fn start_time(pid: u32, allow_zombie: bool) -> Result<Option<String>> {
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
    // ps's lstart is the existing Linux v1 identity representation. Its output is tiny;
    // bound both execution and retained output instead of trusting an inherited PATH.
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
    let process = Pid::from_raw(pid as i32).ok_or(Error::Refused("Invalid child PID"))?;
    if pid != pgid || getpgid(Some(process)).map_err(std::io::Error::from)? != process {
        return Err(Error::Refused("Child process group identity changed"));
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

pub(super) fn valid_start(value: &str) -> bool {
    chrono::NaiveDateTime::parse_from_str(value, "%a %b %e %H:%M:%S %Y").is_ok()
        || value.split_once('.').is_some_and(|(seconds, micros)| {
            seconds.parse::<u64>().is_ok_and(|seconds| seconds > 0)
                && micros.len() == 6
                && micros.bytes().all(|c| c.is_ascii_digit())
        })
}
