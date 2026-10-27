use super::*;
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

/// The soft limit that `raise_open_file_limit` can reach on this machine.
fn reachable(hard: Option<u64>) -> u64 {
    hard.map_or(OPEN_FILE_SOFT_LIMIT, |hard| hard.min(OPEN_FILE_SOFT_LIMIT))
}

#[test]
fn a_launchd_soft_limit_of_256_is_raised_and_the_hard_limit_is_kept() {
    let before = getrlimit(Resource::Nofile);
    // launchd starts the server and the supervisor with a soft limit of 256.
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(256),
            maximum: before.maximum,
        },
    )
    .expect("lower the soft limit as launchd does");
    let raised = raise_open_file_limit().expect("a finite soft limit");
    let after = getrlimit(Resource::Nofile);
    assert_eq!(
        after.maximum, before.maximum,
        "the hard limit never changes"
    );
    assert_eq!(after.current, Some(raised));
    // macOS caps the soft limit at kern.maxfilesperproc (61440 on some Macs), so the
    // limit may stay below the target, but it must be far above 256.
    assert!(
        raised > 256 && raised <= reachable(before.maximum),
        "{raised}"
    );
    assert!(
        SOFT_LIMIT_STEPS.contains(&raised) || raised == reachable(before.maximum),
        "{raised}"
    );
}

#[test]
fn a_higher_soft_limit_is_never_lowered() {
    let before = getrlimit(Resource::Nofile);
    let Some(high) = before
        .maximum
        .map_or(Some(OPEN_FILE_SOFT_LIMIT * 2), |hard| {
            (hard > OPEN_FILE_SOFT_LIMIT).then_some(hard.min(OPEN_FILE_SOFT_LIMIT * 2))
        })
    else {
        return;
    };
    if setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(high),
            maximum: before.maximum,
        },
    )
    .is_err()
    {
        // This kernel caps the soft limit below the target; nothing to check.
        return;
    }
    assert_eq!(raise_open_file_limit(), Some(high));
}

#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "the test reads the limit of a real child process"
)]
fn a_child_process_inherits_the_raised_limit() {
    let before = getrlimit(Resource::Nofile);
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(256),
            maximum: before.maximum,
        },
    )
    .expect("lower the soft limit as launchd does");
    let raised = raise_open_file_limit().expect("a finite soft limit");
    // Provider processes (app-server, Claude bridge) are children of the supervisor or
    // the server and get its limit.
    let output = std::process::Command::new("/bin/sh")
        .args(["-c", "ulimit -n"])
        .output()
        .expect("child shell runs");
    let child: u64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("ulimit prints a number");
    assert_eq!(child, raised);
}
