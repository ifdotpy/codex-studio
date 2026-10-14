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

/// Borrowed-field parse of one `/proc/<pid>/stat` record. Linux commands may
/// contain spaces and closing parentheses, so fields are read only after the
/// final command-name delimiter.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProcStat {
    pub(crate) state: char,
    pub(crate) process_group: u32,
    pub(crate) session: u32,
    pub(crate) start_ticks: u64,
}

impl ProcStat {
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        let tail = raw
            .rsplit_once(')')
            .ok_or(Error::Refused("Cannot verify process identity"))?
            .1;
        let mut fields = tail.split_whitespace();
        let state = fields
            .next()
            .and_then(|value| value.chars().next())
            .ok_or(Error::Refused("Cannot verify process identity"))?;
        let process_group = fields
            .by_ref()
            .nth(1)
            .and_then(|value| value.parse().ok())
            .ok_or(Error::Refused("Cannot verify process identity"))?;
        let session = fields
            .by_ref()
            .next()
            .and_then(|value| value.parse().ok())
            .ok_or(Error::Refused("Cannot verify process identity"))?;
        let start_ticks = fields
            .nth(15)
            .and_then(|value| value.parse().ok())
            .ok_or(Error::Refused("Cannot read kernel process start ticks"))?;
        Ok(Self {
            state,
            process_group,
            session,
            start_ticks,
        })
    }

    pub(crate) fn start_identity(self, boot_id: &str) -> String {
        format!("linux:{boot_id}:{}", self.start_ticks)
    }
}

pub(crate) fn boot_id() -> Result<String> {
    Ok(fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_owned())
}

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
    let stat = ProcStat::parse(&stat)?;
    if !allow_zombie && stat.state == 'Z' {
        return Ok(None);
    }
    Ok(Some(stat.start_identity(&boot_id()?)))
}

/// Signal a process group only while a captured member still has its exact
/// Linux boot/start identity and remains in the captured group and session.
/// Callers capture all group members before starting teardown so leader reap
/// cannot hide surviving jobs from the later KILL pass.
/// Verify a session member before signalling; the remaining verify-to-kill
/// window is inherent without pidfd and is narrower than Python's raw killpg.
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
    if !expected.starts_with("linux:") {
        return Err(Error::Refused(
            "Legacy child identity requires durable upgrade before signal",
        ));
    }
    match start_time(pid, false)?.as_deref() {
        None => return Ok("already-exited"),
        Some(actual) if actual == expected => {}
        Some(_) => return Err(Error::Refused("Child identity changed before close")),
    }
    let Some(process) = verify_group_for_close(pid, pgid)? else {
        return if start_time(pid, false)?.is_none() {
            Ok("already-exited")
        } else {
            Err(Error::Refused("Child identity changed before close"))
        };
    };
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

fn verify_group_for_close(pid: u32, pgid: u32) -> Result<Option<Pid>> {
    let raw = i32::try_from(pid).map_err(|_| Error::Refused("Invalid child PID"))?;
    let process = Pid::from_raw(raw).ok_or(Error::Refused("Invalid child PID"))?;
    if pid != pgid {
        return Err(Error::Refused("Child process group identity changed"));
    }
    match getpgid(Some(process)) {
        Ok(group) if group == process => Ok(Some(process)),
        Ok(_) => Err(Error::Refused("Child process group identity changed")),
        Err(rustix::io::Errno::SRCH) => Ok(None),
        Err(error) => Err(std::io::Error::from(error).into()),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn proc_stat_parses_borrowed_fields_after_the_last_command_delimiter() {
        let mut fields = vec!["1"; 20];
        fields[0] = "S";
        fields[2] = "22";
        fields[3] = "33";
        fields[19] = "1234";
        let raw = format!("42 (odd ) command) {}", fields.join(" "));

        let stat = ProcStat::parse(&raw).unwrap();
        assert_eq!(stat.state, 'S');
        assert_eq!(stat.process_group, 22);
        assert_eq!(stat.session, 33);
        assert_eq!(stat.start_ticks, 1234);
        assert_eq!(stat.start_identity("boot-id"), "linux:boot-id:1234");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn close_succeeds_when_the_verified_child_has_already_exited() {
        let mut child = Command::new("sh")
            .args(["-c", "exec sleep 30"])
            .spawn()
            .expect("blocking child starts");
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(2);
        let identity = loop {
            if let Some(identity) = start_time(pid, true).expect("process identity read") {
                break identity;
            }
            assert!(Instant::now() < deadline, "child identity did not appear");
            thread::sleep(RETRY);
        };
        child.kill().expect("stop blocking child");
        child.wait().expect("child exits");

        assert!(matches!(
            stop_verified(pid, pid, &identity),
            Ok("already-exited")
        ));
    }
}
