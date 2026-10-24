//! Kernel process identity for supervisor ownership and signals.
//!
//! Linux: `linux:<boot_id>:<start ticks>` from `/proc`. Ticks count from boot,
//! and the boot ID is random per boot.
//!
//! macOS: `macos:<kern.bootsessionuuid>:<secs>.<usecs:06>` from libproc
//! `PROC_PIDTBSDINFO`. The start instant is the calendar time of `fork`.
//! The kernel creates a new random boot session UUID at each boot, so a token
//! from an earlier boot never matches a process of this boot. In one boot, the
//! kernel does not reuse a PID while its process exists, and a later process
//! with the same PID starts later; a false match needs a backward calendar
//! step that lands on the same microsecond. `kern.boottime` is not used as a
//! fallback because the kernel moves it when the calendar clock is set.
use super::supervisor_daemon::{Error, Result};
use rustix::process::{Pid, Signal, getpgid, getsid, kill_process_group};
#[cfg(target_os = "linux")]
use std::fs;
use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};
use studio_runtime_providers::supervisor_protocol::{StartMatch, compare_start, is_native_start};

pub(super) const RETRY: Duration = Duration::from_millis(20);

/// Borrowed-field parse of one `/proc/<pid>/stat` record. Linux commands may
/// contain spaces and closing parentheses, so fields are read only after the
/// final command-name delimiter.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProcStat {
    pub(crate) state: char,
    pub(crate) process_group: u32,
    pub(crate) session: u32,
    pub(crate) start_ticks: u64,
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
pub(crate) fn boot_id() -> Result<String> {
    Ok(fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_owned())
}

/// The boot session UUID cannot change while this process runs, so the first
/// successful read is kept.
#[cfg(target_os = "macos")]
pub(crate) fn boot_id() -> Result<String> {
    static BOOT_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if let Some(value) = BOOT_ID.get() {
        return Ok(value.clone());
    }
    let value = studio_runtime_proc_sys::boot_session_uuid()?;
    Ok(BOOT_ID.get_or_init(|| value).clone())
}

/// Microseconds in one second: the unit split of a macOS start instant.
#[cfg(target_os = "macos")]
pub(crate) const MICROS_PER_SECOND: u64 = 1_000_000;

/// The one owner of the macOS native start identity format.
#[cfg(target_os = "macos")]
pub(crate) fn macos_start_identity(boot_id: &str, seconds: u64, microseconds: u64) -> String {
    format!("macos:{boot_id}:{seconds}.{microseconds:06}")
}

/// `<secs>.<usecs:06>`: the tail of a native macOS identity.
fn decimal_start(value: &str) -> bool {
    value.split_once('.').is_some_and(|(seconds, micros)| {
        seconds.parse::<u64>().is_ok_and(|seconds| seconds > 0)
            && micros.len() == 6
            && micros.bytes().all(|c| c.is_ascii_digit())
    })
}

/// libproc reports `ESRCH` for a zombie unless asked to include zombies, and
/// its status is the same kernel `p_stat` that `ps -o stat` shows as `Z`, so
/// Python's extra `ps` read adds no information.
#[cfg(target_os = "macos")]
pub(crate) fn start_time(pid: u32, allow_zombie: bool) -> Result<Option<String>> {
    if pid == 0 {
        return Ok(None);
    }
    let Some(info) = studio_runtime_proc_sys::bsd_info(pid, allow_zombie)? else {
        return Ok(None);
    };
    if !allow_zombie && info.is_zombie() {
        return Ok(None);
    }
    Ok(Some(macos_start_identity(
        &boot_id()?,
        info.start_seconds,
        info.start_microseconds,
    )))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn start_time(pid: u32, _allow_zombie: bool) -> Result<Option<String>> {
    if pid == 0 {
        return Ok(None);
    }
    Err(Error::Refused(
        "Not implemented: requires native process identity for this platform",
    ))
}

#[cfg(target_os = "linux")]
pub(crate) fn start_time(pid: u32, allow_zombie: bool) -> Result<Option<String>> {
    if pid == 0 {
        return Ok(None);
    }
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => s,
        Err(e) if process_gone_error(&e) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let stat = ProcStat::parse(&stat)?;
    if !allow_zombie && stat.state == 'Z' {
        return Ok(None);
    }
    Ok(Some(stat.start_identity(&boot_id()?)))
}

pub(super) fn process_gone_error(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
        || error.raw_os_error().is_some_and(|code| {
            rustix::io::Errno::from_raw_os_error(code) == rustix::io::Errno::SRCH
        })
}

/// Signal a process group only while a captured member still has its exact
/// native boot/start identity and remains in the captured group and session.
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
    Ok((compare_start(expected, actual.as_deref()), actual))
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
#[allow(
    clippy::disallowed_methods,
    reason = "termination runs on supervisor startup or connection blocking workers"
)]
pub(super) fn stop_verified(pid: u32, pgid: u32, expected: &str) -> Result<&'static str> {
    if !is_native_start(expected) {
        return Err(Error::Refused(
            "Child identity is not a native kernel identity",
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
            Err(rustix::io::Errno::SRCH) => {
                return match start_time(pid, false)? {
                    None => Ok("already-exited"),
                    Some(actual) if actual == expected => Err(Error::Refused(
                        "Verified child process group disappeared before child exit",
                    )),
                    Some(_) => Err(Error::Refused("Child identity changed before close")),
                };
            }
            Err(e) => return Err(std::io::Error::from(e).into()),
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match start_time(pid, false)? {
                None => return Ok(outcome),
                Some(actual) if actual == expected => {}
                Some(_) => return Err(Error::Refused("Child identity changed during close")),
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

/// Whether `value` is a start token of the deleted Python supervisor on macOS:
/// `<secs>.<usecs:06>` of the same libproc `PROC_PIDTBSDINFO` start instant as
/// a native macOS identity, without its boot session UUID. Such a token never
/// authorizes ownership or a signal. It can only show that its process ended.
pub(super) fn legacy_python_start(value: &str) -> bool {
    decimal_start(value)
}

/// Whether the process that a legacy Python start token names has ended: its
/// PID is gone, or the PID runs with another start instant. A live process with
/// the same start instant has not ended.
#[cfg(target_os = "macos")]
pub(super) fn legacy_start_ended(pid: u32, legacy: &str) -> Result<bool> {
    if !legacy_python_start(legacy) {
        return Ok(false);
    }
    Ok(match start_time(pid, false)? {
        None => true,
        Some(actual) => actual.rsplit_once(':').map(|(_, start)| start) != Some(legacy),
    })
}

/// The Python supervisor recorded `ps -o lstart` text on other platforms, so a
/// bare epoch token there cannot prove that its process ended.
#[cfg(not(target_os = "macos"))]
pub(super) fn legacy_start_ended(_pid: u32, _legacy: &str) -> Result<bool> {
    Ok(false)
}

pub(super) fn valid_start(value: &str) -> bool {
    let boot_uuid =
        |boot: &str| boot.len() == 36 && boot.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-');
    value.strip_prefix("linux:").is_some_and(|rest| {
        rest.split_once(':')
            .is_some_and(|(boot, ticks)| boot_uuid(boot) && ticks.parse::<u64>().is_ok())
    }) || value.strip_prefix("macos:").is_some_and(|rest| {
        rest.split_once(':')
            .is_some_and(|(boot, start)| boot_uuid(boot) && decimal_start(start))
    })
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    #[cfg(target_os = "linux")]
    fn proc_stat_parses_borrowed_fields_after_the_last_command_delimiter() {
        let mut fields = vec!["1"; 20];
        fields[0] = "S";
        fields[2] = "22";
        fields[3] = "33";
        fields[19] = "1234";
        let raw = format!("42 (odd ) command) {}", fields.join(" "));

        let stat = ProcStat::parse(&raw).expect("fixture proc stat is valid");
        assert_eq!(stat.state, 'S');
        assert_eq!(stat.process_group, 22);
        assert_eq!(stat.session, 33);
        assert_eq!(stat.start_ticks, 1234);
        assert_eq!(stat.start_identity("boot-id"), "linux:boot-id:1234");
    }

    #[test]
    fn valid_start_accepts_only_native_tokens() {
        let boot = "1CCC2169-B973-447A-B702-3F226EECA807";
        assert!(valid_start(&format!("macos:{boot}:1790895321.190147")));
        assert!(valid_start(&format!("linux:{boot}:1234")));
        assert!(!valid_start("1790895321.190147"));
        assert!(!valid_start("Fri Oct  2 00:55:21 2026"));
        assert!(!valid_start(&format!("macos:{boot}:1790895321.19014")));
        assert!(!valid_start("macos:short:1790895321.190147"));
        assert!(!valid_start(&format!("macos:{boot}:1234")));
    }

    #[test]
    fn legacy_python_start_is_only_the_bare_macos_start_instant() {
        let boot = "1CCC2169-B973-447A-B702-3F226EECA807";
        assert!(legacy_python_start("1790895321.190147"));
        assert!(!legacy_python_start(&format!(
            "macos:{boot}:1790895321.190147"
        )));
        assert!(!legacy_python_start("Fri Oct  2 00:55:21 2026"));
        assert!(!legacy_python_start("1790895321.19014"));
        assert!(!legacy_python_start("0.190147"));
    }

    /// A child in its own group that blocks until the test kills it.
    #[cfg(target_os = "macos")]
    struct Blocker(std::process::Child);

    #[cfg(target_os = "macos")]
    impl Blocker {
        fn start() -> (Self, u32, String) {
            use std::os::unix::process::CommandExt;
            let child = Command::new("/bin/sleep")
                .arg("30")
                .process_group(0)
                .stdin(std::process::Stdio::null())
                .spawn()
                .expect("blocking child starts");
            let pid = child.id();
            let identity = start_time(pid, false)
                .expect("process identity read")
                .expect("live child has an identity");
            (Self(child), pid, identity)
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for Blocker {
        fn drop(&mut self) {
            let _kill = self.0.kill();
            let _wait = self.0.wait();
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_identity_is_stable_for_a_live_child_and_gone_after_exit() {
        let (mut blocker, pid, identity) = Blocker::start();
        let info = studio_runtime_proc_sys::bsd_info(pid, false)
            .expect("libproc read")
            .expect("live child");
        assert_eq!(
            identity,
            format!(
                "macos:{}:{}.{:06}",
                boot_id().expect("boot id"),
                info.start_seconds,
                info.start_microseconds
            )
        );
        assert!(valid_start(&identity));
        assert_eq!(
            start_time(pid, false).expect("second read").as_deref(),
            Some(identity.as_str())
        );
        assert_eq!(
            start_time(pid, true)
                .expect("zombie-tolerant read")
                .as_deref(),
            Some(identity.as_str())
        );
        assert_eq!(
            inspect_start(pid, &identity).expect("inspect"),
            (StartMatch::Exact, Some(identity.clone()))
        );
        blocker.0.kill().expect("stop child");
        // Before the reap the child is a zombie: gone for signals, still
        // identifiable for the spawn path that reads it with allow_zombie.
        let deadline = Instant::now() + Duration::from_secs(5);
        while start_time(pid, false).expect("read").is_some() {
            assert!(Instant::now() < deadline, "killed child stayed live");
            thread::sleep(RETRY);
        }
        assert_eq!(
            start_time(pid, true).expect("zombie read").as_deref(),
            Some(identity.as_str())
        );
        blocker.0.wait().expect("reap child");
        assert_eq!(start_time(pid, false).expect("read after reap"), None);
        assert_eq!(start_time(pid, true).expect("read after reap"), None);
        assert_eq!(
            inspect_start(pid, &identity).expect("inspect").0,
            StartMatch::Gone
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn reused_pid_with_another_start_is_never_signalled() {
        let (blocker, pid, identity) = Blocker::start();
        let tail = identity.rsplit_once(':').expect("native token").1;
        let (seconds, micros) = tail.split_once('.').expect("decimal token");
        let micros: u64 = micros.parse().expect("micros");
        // The same PID and boot with a different start is a reused PID.
        let stale = macos_start_identity(
            &boot_id().expect("boot id"),
            seconds.parse().expect("seconds"),
            (micros + 1) % MICROS_PER_SECOND,
        );
        assert_eq!(
            inspect_start(pid, &stale).expect("inspect").0,
            StartMatch::Different
        );
        // A token from another boot is a reused PID too.
        let other_boot = identity.replacen(
            &boot_id().expect("boot id"),
            "00000000-0000-0000-0000-000000000000",
            1,
        );
        assert_eq!(
            inspect_start(pid, &other_boot).expect("inspect").0,
            StartMatch::Different
        );
        assert!(matches!(
            stop_verified(pid, pid, &stale),
            Err(Error::Refused("Child identity changed before close"))
        ));
        let sid = u32::try_from(
            getsid(Pid::from_raw(i32::try_from(pid).expect("pid")))
                .expect("session")
                .as_raw_nonzero()
                .get(),
        )
        .expect("sid");
        assert!(
            !signal_verified_group_member(pid, pid, sid, &stale, Signal::KILL)
                .expect("verify member")
        );
        assert_eq!(
            start_time(pid, false).expect("read").as_deref(),
            Some(identity.as_str()),
            "the refused child must still be alive"
        );
        assert!(
            signal_verified_group_member(pid, pid, sid, &identity, Signal::TERM)
                .expect("verified signal")
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while start_time(pid, false).expect("read").is_some() {
            assert!(
                Instant::now() < deadline,
                "verified TERM did not stop child"
            );
            thread::sleep(RETRY);
        }
        drop(blocker);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn verified_close_terminates_a_live_macos_group() {
        let (blocker, pid, identity) = Blocker::start();
        assert_eq!(
            verify_group(pid, pid)
                .expect("group")
                .as_raw_nonzero()
                .get(),
            i32::try_from(pid).expect("pid")
        );
        assert!(matches!(
            stop_verified(pid, pid, &identity),
            Ok("terminated" | "killed" | "already-exited")
        ));
        drop(blocker);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
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

    #[test]
    fn vanished_process_lookup_errors_are_limited_to_not_found_and_esrch() {
        assert!(process_gone_error(&std::io::Error::from(
            rustix::io::Errno::SRCH,
        )));
        assert!(process_gone_error(&std::io::Error::from(
            std::io::ErrorKind::NotFound,
        )));
        assert!(!process_gone_error(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        )));
    }
}
