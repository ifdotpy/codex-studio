#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

use process_wrap::std::{CommandWrap, ProcessGroup};
use rustix::process::getpriority_process;
use studio_runtime_process::child_priority::{
    CHILD_PRIORITY_ENV, ChildPriority, priority_diagnostics,
};

// A pipe handshake makes reporting deterministic without delaying the parent.
// The nested shell is started after priority application and proves inheritance.
const PROBE: &str =
    "read -r ready; ps -o ni= -p $$; ionice -p $$; sh -c 'ps -o ni= -p $$; ionice -p $$'";

fn report(priority: ChildPriority, own_group: bool) -> String {
    let mut command = CommandWrap::with_new("/bin/sh", |command| {
        command
            .args(["-c", PROBE])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
    });
    if own_group {
        command.wrap(ProcessGroup::leader());
    }
    priority.apply_before_spawn(&mut command);
    let mut child = command.spawn().expect("harmless probe starts");
    child
        .stdin()
        .take()
        .expect("stdin")
        .write_all(b"ready\n")
        .expect("release probe");
    let output = child.wait_with_output().expect("probe completes");
    assert!(output.status.success());
    String::from_utf8(output.stdout).expect("probe text")
}

fn inherited_report() -> String {
    let nice = getpriority_process(None).expect("parent nice");
    let output = Command::new("ionice")
        .args(["-p", &std::process::id().to_string()])
        .output()
        .expect("parent I/O query");
    assert!(output.status.success());
    let io = String::from_utf8(output.stdout).expect("I/O class");
    format!("{nice}\n{}\n{nice}\n{}", io.trim(), io.trim())
}

fn normalized(report: &str) -> String {
    report.lines().map(str::trim).collect::<Vec<_>>().join("\n")
}

#[test]
fn background_child_and_grandchild_report_mild_cpu_and_best_effort_io() {
    let before = inherited_report();
    assert_eq!(
        normalized(&report(ChildPriority::Background, true)),
        "5\nbest-effort: prio 7\n5\nbest-effort: prio 7"
    );
    assert_eq!(inherited_report(), before, "backend scheduling unchanged");
}

#[test]
fn normal_leaves_both_inherited_priorities_unchanged() {
    assert_eq!(
        normalized(&report(ChildPriority::Normal, true)),
        inherited_report()
    );
}

#[test]
fn io_boundary_rejects_implicit_current_group_and_negative_ids() {
    for invalid in [0, -1] {
        assert_eq!(
            studio_runtime_priority_sys::set_group_io_best_effort(invalid)
                .expect_err("positive IDs only")
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
}

#[test]
fn missing_isolated_group_never_changes_parent_or_prevents_spawn() {
    let before = inherited_report();
    assert_eq!(
        normalized(&report(ChildPriority::Background, false)),
        before
    );
    assert_eq!(inherited_report(), before);
}

#[test]
fn environment_child_probe() {
    if std::env::var("STUDIO_PRIORITY_PROBE").as_deref() == Ok("failures") {
        report(ChildPriority::Background, false);
        let first = priority_diagnostics();
        assert_eq!(first.failure_count, 1);
        assert_eq!(
            first.first_failure.expect("first diagnostic").mechanism,
            "child process group"
        );
        report(ChildPriority::Background, false);
        let second = priority_diagnostics();
        assert_eq!(second.failure_count, 2);
        assert_eq!(second.first_failure, first.first_failure);
    } else if std::env::var("STUDIO_PRIORITY_PROBE").as_deref() == Ok("stream") {
        stream_priority_probe();
    } else if std::env::var_os("STUDIO_PRIORITY_PROBE").is_some() {
        println!("configured={:?}", ChildPriority::from_environment());
    }
}

#[test]
fn two_priority_failures_are_counted_without_emitting_io() {
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "environment_child_probe", "--nocapture"])
        .env("STUDIO_PRIORITY_PROBE", "failures")
        .output()
        .expect("failure probe completes");
    assert!(output.status.success());
    assert!(
        output.stderr.is_empty(),
        "spawn hook performs no diagnostic I/O"
    );
}

#[test]
fn group_priority_includes_a_grandchild_forked_before_application() {
    let mut command = CommandWrap::with_new("/bin/sh", |command| {
        command
            .args([
                "-c",
                "sh -c 'echo ready; read -r go; ps -o ni= -p $$; ionice -p $$' <&0 & wait",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
    });
    command.wrap(ProcessGroup::leader());
    let mut child = command.spawn().expect("isolated group starts");
    let mut stdout = BufReader::new(child.stdout().take().expect("stdout"));
    let mut ready = String::new();
    stdout
        .read_line(&mut ready)
        .expect("grandchild ready handshake");
    assert_eq!(ready, "ready\n");
    ChildPriority::Background.apply_after_spawn(child.try_inner_child().expect("native child"));
    child
        .stdin()
        .take()
        .expect("stdin")
        .write_all(b"go\n")
        .expect("release grandchild");
    let mut output = String::new();
    stdout
        .read_to_string(&mut output)
        .expect("grandchild report");
    assert!(child.wait().expect("child reaped").success());
    assert_eq!(normalized(&output), "5\nbest-effort: prio 7");
}

#[test]
fn environment_selects_background_by_default_and_normal_disables_it() {
    for (value, expected) in [
        (None, "Background"),
        (Some("normal"), "Normal"),
        (Some("background"), "Background"),
        (Some("invalid"), "Background"),
    ] {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args(["--exact", "environment_child_probe", "--nocapture"])
            .env("STUDIO_PRIORITY_PROBE", "1")
            .env_remove(CHILD_PRIORITY_ENV);
        if let Some(value) = value {
            command.env(CHILD_PRIORITY_ENV, value);
        }
        let output = command.output().expect("configuration probe");
        assert!(output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(&format!("configured={expected}"))
        );
    }
}

// A fresh executable per setting keeps the process-wide configuration immutable.
#[test]
fn stream_child_honors_default_background_and_normal_for_child_and_grandchild() {
    for value in [None, Some("background"), Some("normal")] {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args(["--exact", "environment_child_probe", "--nocapture"])
            .env("STUDIO_PRIORITY_PROBE", "stream")
            .env_remove(CHILD_PRIORITY_ENV);
        if let Some(value) = value {
            command.env(CHILD_PRIORITY_ENV, value);
        }
        let output = command.output().expect("stream priority probe completes");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

fn stream_priority_probe() {
    use studio_runtime_process::stream_child::{
        ChildEvent, ChildSpec, ExitReason, OutputStream, StreamChild,
    };
    let before = inherited_report();
    let root = tempfile::tempdir().expect("stream test root");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("probe runtime");
    let report = runtime.block_on(async {
        let mut child = StreamChild::spawn(ChildSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), PROBE.into()],
            cwd: root.path().into(),
            environment: [("PATH".into(), "/usr/bin:/bin".into())].into(),
            timeout: std::time::Duration::from_secs(30),
            inherited_lease: None,
        })
        .await
        .expect("stream probe spawns");
        // spawn returns only after the hook, so child and later grandchild cannot
        // report their inherited priorities before the parent has applied them.
        child
            .write(b"ready\n".to_vec(), true)
            .await
            .expect("release probe");
        let mut output = Vec::new();
        let mut completed = false;
        while let Some(event) = child.next_event().await {
            match event {
                ChildEvent::Output {
                    stream: OutputStream::Stdout,
                    bytes,
                } => output.extend(bytes),
                ChildEvent::Output {
                    stream: OutputStream::Stderr,
                    bytes,
                } => assert!(bytes.is_empty()),
                ChildEvent::Exited { code, reason } => {
                    assert_eq!(code, Some(0));
                    assert_eq!(reason, ExitReason::Completed);
                    completed = true;
                }
            }
        }
        child.close().await.expect("owner cleanup");
        assert!(completed);
        String::from_utf8(output).expect("priority report")
    });
    let expected = if std::env::var(CHILD_PRIORITY_ENV).as_deref() == Ok("normal") {
        before.clone()
    } else {
        "5\nbest-effort: prio 7\n5\nbest-effort: prio 7".into()
    };
    assert_eq!(normalized(&report), expected);
    assert_eq!(inherited_report(), before, "backend priorities unchanged");
}
