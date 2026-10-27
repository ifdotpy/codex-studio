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
    pub(crate) process_group: i32,
    pub(crate) session: i32,
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

    pub(crate) fn is_terminal(self) -> bool {
        matches!(self.state, 'Z' | 'X')
    }

    /// Terminal rows may retain the kernel's unavailable (-1) group IDs.
    /// Live rows must still provide unsigned IDs before a scan can use them.
    pub(crate) fn live_group_ids(self) -> Result<Option<(u32, u32)>> {
        if self.is_terminal() {
            return Ok(None);
        }
        let group = u32::try_from(self.process_group)
            .map_err(|_| Error::Refused("Cannot verify process identity"))?;
        let session = u32::try_from(self.session)
            .map_err(|_| Error::Refused("Cannot verify process identity"))?;
        Ok(Some((group, session)))
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
    if stat.live_group_ids()?.is_none() && !allow_zombie {
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
    if !member_is_current(pid, pgid, sid, expected_start)? {
        return Ok(false);
    }
    match kill_process_group(native_pid(pgid)?, signal) {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::SRCH) => Ok(false),
        Err(error) => Err(std::io::Error::from(error).into()),
    }
}

/// A captured group is the sole native member/PGID/SID ownership model for
/// supervisor close and PTY teardown. It does not own escaped groups/sessions.
#[derive(Clone)]
pub(crate) struct GroupMember {
    pub(crate) pid: u32,
    pub(crate) start_time: String,
}

#[derive(Clone)]
pub(crate) struct VerifiedProcessGroup {
    pub(crate) id: u32,
    pub(crate) session: u32,
    pub(crate) members: Vec<GroupMember>,
}

impl VerifiedProcessGroup {
    pub(crate) fn signal(&self, signal: Signal) -> Result<bool> {
        for member in &self.members {
            if signal_verified_group_member(
                member.pid,
                self.id,
                self.session,
                &member.start_time,
                signal,
            )? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn has_live_anchor(&self) -> Result<bool> {
        for member in &self.members {
            if member_is_current(member.pid, self.id, self.session, &member.start_time)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn native_pid(value: u32) -> Result<Pid> {
    i32::try_from(value)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or(Error::Refused("Invalid process identity"))
}

fn member_is_current(pid: u32, pgid: u32, sid: u32, expected: &str) -> Result<bool> {
    if start_time(pid, false)?.as_deref() != Some(expected) {
        return Ok(false);
    }
    let member = native_pid(pid)?;
    let identities = getpgid(Some(member))
        .and_then(|group| getsid(Some(member)).map(|session| (group, session)));
    let (group, session) = match identities {
        Ok(ids) => ids,
        Err(rustix::io::Errno::SRCH) => return Ok(false),
        Err(error) => return Err(std::io::Error::from(error).into()),
    };
    Ok(group == native_pid(pgid)?
        && session == native_pid(sid)?
        && start_time(pid, false)?.as_deref() == Some(expected))
}

#[derive(Clone, Copy)]
pub(crate) enum GroupScope {
    Session(u32),
    Group(u32),
}

impl GroupScope {
    fn includes(self, group: u32, session: u32) -> bool {
        match self {
            Self::Session(expected) => session == expected,
            Self::Group(expected) => group == expected,
        }
    }
}

/// A failed enumeration cannot prove an empty group or completed teardown.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn capture_groups(scope: GroupScope) -> Result<Vec<VerifiedProcessGroup>> {
    let scope_id = match scope {
        GroupScope::Group(id) | GroupScope::Session(id) => id,
    };
    let _scope_pid = native_pid(scope_id)?;
    let mut groups = std::collections::BTreeMap::<(u32, u32), Vec<GroupMember>>::new();
    let boot = boot_id()?;
    #[cfg(target_os = "linux")]
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let raw = match fs::read_to_string(entry.path().join("stat")) {
            Ok(raw) => raw,
            Err(error) if process_gone_error(&error) => continue,
            Err(error) => return Err(error.into()),
        };
        let stat = ProcStat::parse(&raw)?;
        let Some((group, session)) = stat.live_group_ids()? else {
            continue;
        };
        if scope.includes(group, session) {
            if group == 0 || session == 0 {
                return Err(Error::Refused("Invalid process group identity"));
            }
            groups
                .entry((group, session))
                .or_default()
                .push(GroupMember {
                    pid,
                    start_time: stat.start_identity(&boot),
                });
        }
    }
    #[cfg(target_os = "macos")]
    for pid in studio_runtime_proc_sys::all_pids()? {
        let raw = native_pid(pid)?;
        let ids =
            getpgid(Some(raw)).and_then(|group| getsid(Some(raw)).map(|session| (group, session)));
        let (group, session) = match ids {
            Ok((group, session)) => (
                u32::try_from(group.as_raw_nonzero().get())
                    .map_err(|_| Error::Refused("Invalid process group"))?,
                u32::try_from(session.as_raw_nonzero().get())
                    .map_err(|_| Error::Refused("Invalid process session"))?,
            ),
            Err(rustix::io::Errno::SRCH) => continue,
            Err(error) => return Err(std::io::Error::from(error).into()),
        };
        if !scope.includes(group, session) {
            continue;
        }
        let Some(info) = studio_runtime_proc_sys::bsd_info(pid, false)? else {
            continue;
        };
        if !info.is_zombie() && info.process_group == group {
            groups
                .entry((group, session))
                .or_default()
                .push(GroupMember {
                    pid,
                    start_time: macos_start_identity(
                        &boot,
                        info.start_seconds,
                        info.start_microseconds,
                    ),
                });
        }
    }
    Ok(groups
        .into_iter()
        .map(|((id, session), mut members)| {
            members.sort_by_key(|member| member.pid);
            VerifiedProcessGroup {
                id,
                session,
                members,
            }
        })
        .collect())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn capture_groups(_scope: GroupScope) -> Result<Vec<VerifiedProcessGroup>> {
    Err(Error::Refused(
        "Native process group enumeration is unavailable on this device",
    ))
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
/// Completed group close proof. Journal strings are converted at the boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GroupCloseResult {
    AlreadyExited,
    Terminated,
    Killed,
}

impl GroupCloseResult {
    pub(super) fn journal_outcome(self) -> &'static str {
        match self {
            Self::AlreadyExited => "already-exited",
            Self::Terminated => "terminated",
            Self::Killed => "killed",
        }
    }
}

/// TERM grace before escalation, and KILL confirmation budget.
const GROUP_TERM_GRACE: Duration = Duration::from_millis(1500);
const GROUP_KILL_WAIT: Duration = Duration::from_secs(1);

#[allow(
    clippy::disallowed_methods,
    reason = "termination runs on supervisor startup or connection blocking workers"
)]
pub(super) fn stop_verified(pid: u32, pgid: u32, expected: &str) -> Result<GroupCloseResult> {
    if !is_native_start(expected) {
        return Err(Error::Refused(
            "Child identity is not a native kernel identity",
        ));
    }
    if pid != pgid {
        return Err(Error::Refused("Child process group identity changed"));
    }
    match start_time(pid, false)?.as_deref() {
        None => {
            return if group_exit_verified(pid, pgid, expected)? {
                Ok(GroupCloseResult::AlreadyExited)
            } else {
                Err(Error::Refused(
                    "Child leader identity is unavailable while process group members remain",
                ))
            };
        }
        Some(actual) if actual == expected => {}
        Some(_) => return Err(Error::Refused("Child identity changed before close")),
    }
    let process = verify_group(pid, pgid)?;
    let session = getsid(Some(process)).map_err(std::io::Error::from)?;
    let session = u32::try_from(session.as_raw_nonzero().get())
        .map_err(|_| Error::Refused("Invalid process session"))?;
    let mut groups = capture_groups(GroupScope::Group(pgid))?;
    // The leader must still pin this group while its ownership is captured.
    if start_time(pid, true)?.as_deref() != Some(expected) {
        return if group_exit_verified(pid, pgid, expected)? {
            Ok(GroupCloseResult::AlreadyExited)
        } else {
            Err(Error::Refused("Child identity changed before close"))
        };
    }
    let Some(group) = groups.pop() else {
        return if group_exit_verified(pid, pgid, expected)? {
            Ok(GroupCloseResult::AlreadyExited)
        } else {
            Err(Error::Refused("Child process group identity changed"))
        };
    };
    if group.session != session || !groups.is_empty() {
        return Err(Error::Refused("Child process session identity changed"));
    }
    for (signal, timeout, outcome) in [
        (Signal::TERM, GROUP_TERM_GRACE, GroupCloseResult::Terminated),
        (Signal::KILL, GROUP_KILL_WAIT, GroupCloseResult::Killed),
    ] {
        let signalled = group.signal(signal)?;
        let deadline = Instant::now() + timeout;
        loop {
            if group_exit_verified(pid, pgid, expected)? {
                return Ok(outcome);
            }
            if !signalled || !group.has_live_anchor()? {
                // A member can finish between the scan and identity check.
                // Only a new empty scan can turn that race into close proof.
                if group_exit_verified(pid, pgid, expected)? {
                    return Ok(outcome);
                }
                return Err(Error::Refused(
                    "Process group remains without a verified original member",
                ));
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(RETRY);
        }
    }
    Err(Error::Refused(
        "Could not terminate verified child process group",
    ))
}

/// Every close completion uses the same proof: the recorded group has no
/// live members and the recorded leader has exited. Moving out of the group
/// never grants authority to signal the leader's new group.
fn group_exit_verified(pid: u32, pgid: u32, expected: &str) -> Result<bool> {
    if !capture_groups(GroupScope::Group(pgid))?.is_empty() {
        return Ok(false);
    }
    match inspect_start(pid, expected)?.0 {
        StartMatch::Gone => Ok(true),
        StartMatch::Exact => Err(Error::Refused(
            "Recorded child remains live outside its process group",
        )),
        StartMatch::Different => Err(Error::Refused("Child identity changed during close")),
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
    #[cfg(target_os = "linux")]
    fn terminal_proc_stat_with_kernel_unavailable_group_ids_can_be_classified() {
        // do_task_stat leaves PGID/SID at -1 when exit has released sighand.
        // A group scan must classify these terminal rows before requiring live IDs.
        for state in ["Z", "X"] {
            let mut fields = vec!["1"; 20];
            fields[0] = state;
            fields[2] = "-1";
            fields[3] = "-1";
            fields[19] = "1234";
            let raw = format!("42 (exited) {}", fields.join(" "));
            let stat = ProcStat::parse(&raw).expect("valid terminal kernel record");
            assert_eq!(stat.state.to_string(), state);
            assert_eq!(stat.process_group, -1);
            assert_eq!(stat.session, -1);
            assert!(stat.is_terminal());
            assert_eq!(
                stat.live_group_ids().expect("terminal classification"),
                None
            );
            assert_eq!(stat.start_identity("boot-id"), "linux:boot-id:1234");
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn live_proc_stat_unavailable_ids_cannot_supply_group_ownership() {
        for state in ["R", "S", "?"] {
            for (group, session) in [("-1", "33"), ("22", "-1"), ("-1", "-1")] {
                let mut fields = vec!["1"; 20];
                fields[0] = state;
                fields[2] = group;
                fields[3] = session;
                let raw = format!("42 (live) {}", fields.join(" "));
                let stat = ProcStat::parse(&raw).expect("signed kernel fields");
                assert!(!stat.is_terminal());
                assert!(stat.live_group_ids().is_err());
            }
        }
        assert!(capture_groups(GroupScope::Group(0)).is_err());
        assert!(capture_groups(GroupScope::Session(0)).is_err());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn terminal_leader_keeps_exact_start_anchor_without_live_identity() {
        use rustix::process::{WaitId, WaitIdOptions, waitid};
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("owned fixture");
        let pid = child.id();
        let anchor = start_time(pid, true).expect("captured native start");
        waitid(
            WaitId::Pid(native_pid(pid).expect("fixture pid")),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
        )
        .expect("fixture exited without reaping");
        let retained = start_time(pid, true).expect("terminal anchor");
        let live = start_time(pid, false).expect("live identity");
        child.wait().expect("reap owned fixture");
        assert!(anchor.is_some());
        assert_eq!(retained, anchor);
        assert_eq!(live, None);
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
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    struct Blocker(std::process::Child);

    #[cfg(any(target_os = "linux", target_os = "macos"))]
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl Drop for Blocker {
        fn drop(&mut self) {
            let _kill = self.0.kill();
            let _wait = self.0.wait();
        }
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn captured_group_refuses_changed_boot_start_group_or_session() {
        let (_blocker, pid, identity) = Blocker::start();
        let group = capture_groups(GroupScope::Group(pid))
            .expect("capture owned group")
            .pop()
            .expect("live owned group");
        let mut stale = group.clone();
        let stale_start = identity.replacen(
            &boot_id().expect("boot identity"),
            "00000000-0000-0000-0000-000000000000",
            1,
        );
        stale.members[0].start_time = stale_start.clone();
        assert!(!stale.signal(Signal::KILL).expect("stale boot refused"));
        assert!(stop_verified(pid, pid, &stale_start).is_err());
        stale.members[0].start_time = format!("{identity}0");
        assert!(!stale.signal(Signal::KILL).expect("stale start refused"));
        stale = group.clone();
        stale.id += 1;
        assert!(!stale.signal(Signal::KILL).expect("changed group refused"));
        stale = group;
        stale.session += 1;
        assert!(!stale.signal(Signal::KILL).expect("changed session refused"));
        assert_eq!(
            start_time(pid, false).expect("read live child").as_deref(),
            Some(identity.as_str()),
            "no stale ownership proof may signal the disposable child"
        );
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
            Ok(GroupCloseResult::Terminated
                | GroupCloseResult::Killed
                | GroupCloseResult::AlreadyExited)
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
            Ok(GroupCloseResult::AlreadyExited)
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
