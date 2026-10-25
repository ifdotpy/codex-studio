#![cfg(unix)]
#![allow(clippy::disallowed_methods, clippy::unwrap_used)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

fn fixture(root: &Path) {
    fs::create_dir_all(root.join("tools")).unwrap();
    fs::write(
        root.join("tools/systemctl"),
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOME/calls\"\n",
    )
    .unwrap();
    fs::set_permissions(
        root.join("tools/systemctl"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
}

// Every service witness that drives the CLI through systemctl is Linux-only.
#[cfg(target_os = "linux")]
fn cli(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_studio-runtime-server"))
        .args(args)
        .env_clear()
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("PATH", root.join("tools"))
        .env("CODEX_AGENTS_PYTHON", "/nonexistent")
        .output()
        .unwrap()
}

/// The node of the test tooling: `CODEX_NODE` as the test recipes set it,
/// else the first `node` on this process's PATH.
#[cfg(target_os = "linux")]
fn test_node() -> std::path::PathBuf {
    std::env::var_os("CODEX_NODE")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::split_paths(&std::env::var_os("PATH")?)
                .map(|directory| directory.join("node"))
                .find(|candidate| candidate.is_file())
        })
        .expect("node is on PATH or in CODEX_NODE")
}

#[test]
#[cfg(target_os = "linux")]
fn service_status_uses_only_the_selected_user_manager_without_python() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let result = cli(root.path(), &["service", "status"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::read_to_string(root.path().join("calls")).unwrap(),
        "--user status codex-studio.service\n"
    );
    assert!(!root.path().join("config").exists());
}

#[test]
#[cfg(target_os = "linux")]
fn service_install_retry_conflict_and_uninstall_preserve_state_and_never_restart() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let state = root.path().join("state with % and $ punctuation");
    fs::create_dir(&state).unwrap();
    fs::write(state.join("identity"), "keep").unwrap();
    let state_text = state.to_str().unwrap();
    for _ in 0..2 {
        let output = cli(root.path(), &["service", "install", "--state", state_text]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let unit = root.path().join("config/systemd/user/codex-studio.service");
    let content = fs::read_to_string(&unit).unwrap();
    assert!(content.contains("/current/bin/studio-runtime-server\" --port 4620"));
    assert!(content.contains("state with %% and $ punctuation"));
    assert!(!content.contains("python"));
    assert!(!content.contains("apps/server"));
    let conflict = cli(
        root.path(),
        &[
            "service",
            "install",
            "--state",
            root.path().to_str().unwrap(),
        ],
    );
    assert!(!conflict.status.success());
    assert_eq!(fs::read_to_string(&unit).unwrap(), content);
    let calls = fs::read_to_string(root.path().join("calls")).unwrap();
    assert_eq!(
        calls,
        "--user daemon-reload\n--user enable codex-studio.service\n".repeat(2)
    );
    for _ in 0..2 {
        let output = cli(
            root.path(),
            &["service", "uninstall", "--state", state_text],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(!unit.exists());
    assert_eq!(fs::read_to_string(state.join("identity")).unwrap(), "keep");
    assert_eq!(fs::read_dir(&state).unwrap().count(), 1);
    let calls = fs::read_to_string(root.path().join("calls")).unwrap();
    assert!(!calls.contains("restart"));
    assert!(!calls.contains("stop"));
    assert!(!calls.contains("start"));
}

#[test]
#[cfg(target_os = "linux")]
fn service_manager_failure_is_visible_and_does_not_remove_configuration() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    assert!(cli(root.path(), &["service", "install"]).status.success());
    fs::write(root.path().join("tools/systemctl"), "#!/bin/sh\nexit 1\n").unwrap();
    assert!(!cli(root.path(), &["service", "uninstall"]).status.success());
    assert!(
        root.path()
            .join("config/systemd/user/codex-studio.service")
            .exists()
    );
}

#[test]
fn command_installation_migrates_only_owned_links_and_preflights_conflicts() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let project = root.path().join("project");
    let binaries = project.join("bin");
    let destination = root.path().join("commands");
    fs::create_dir_all(&binaries).unwrap();
    fs::create_dir(&destination).unwrap();
    let source = env!("CARGO_BIN_EXE_studio-runtime-server");
    let relocated = binaries.join("studio-runtime-server");
    if let Err(error) = fs::hard_link(source, &relocated) {
        if error.kind() != std::io::ErrorKind::CrossesDevices {
            panic!("{error}");
        }
        fs::copy(source, &relocated).expect("cross-device copy");
    }
    for name in ["codex-process-supervisor", "codex-diagnostics"] {
        fs::write(binaries.join(name), "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(binaries.join(name), fs::Permissions::from_mode(0o755)).unwrap();
    }
    symlink(
        project.join("workspaces/runtime/apps/server/src/codex-canvas"),
        destination.join("codex-canvas"),
    )
    .unwrap();
    symlink(project.join("scripts/luna"), destination.join("luna")).unwrap();
    symlink(root.path().join("foreign"), destination.join("codex-chat")).unwrap();
    fs::write(destination.join("codex-diagnostics"), "unrelated").unwrap();
    let invoke = || {
        Command::new(binaries.join("studio-runtime-server"))
            .args(["install-cli", "--bin-dir"])
            .arg(&destination)
            .arg("--project")
            .arg(&project)
            .env_clear()
            .env("HOME", root.path())
            .env("PATH", root.path().join("tools"))
            .env("CODEX_AGENTS_PYTHON", "/nonexistent")
            .output()
            .unwrap()
    };
    assert!(!invoke().status.success());
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 4);
    assert_eq!(
        fs::read_link(destination.join("luna")).unwrap(),
        project.join("scripts/luna")
    );
    fs::remove_file(destination.join("codex-diagnostics")).unwrap();
    let first = invoke();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(String::from_utf8_lossy(&first.stdout).contains("Removed legacy Python command"));
    assert!(!destination.join("luna").is_symlink());
    assert_eq!(
        fs::read_link(destination.join("codex-chat")).unwrap(),
        root.path().join("foreign")
    );
    // install-cli links the canonical binary (macOS `/var` is `/private/var`).
    assert_eq!(
        fs::read_link(destination.join("codex-canvas")).unwrap(),
        fs::canonicalize(&binaries)
            .unwrap()
            .join("studio-runtime-server")
    );
    assert!(invoke().status.success());
    let output = Command::new(destination.join("codex-canvas"))
        .arg("--help")
        .current_dir(root.path())
        .env_clear()
        .env("HOME", root.path())
        .env("CODEX_AGENTS_PYTHON", "/nonexistent")
        .output()
        .unwrap();
    assert!(output.status.success());
}

#[test]
#[cfg(target_os = "linux")]
fn service_install_refuses_foreign_files_and_symlinks_before_manager_calls() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let directory = root.path().join("config/systemd/user");
    fs::create_dir_all(&directory).unwrap();
    let unit = directory.join("codex-studio.service");
    fs::write(&unit, "foreign service").unwrap();
    let result = cli(root.path(), &["service", "install"]);
    assert!(!result.status.success());
    assert_eq!(fs::read_to_string(&unit).unwrap(), "foreign service");
    assert!(!root.path().join("calls").exists());
    fs::remove_file(&unit).unwrap();
    let foreign = root.path().join("foreign");
    fs::write(&foreign, "untouched").unwrap();
    symlink(&foreign, &unit).unwrap();
    assert!(!cli(root.path(), &["service", "install"]).status.success());
    assert!(!cli(root.path(), &["service", "uninstall"]).status.success());
    assert_eq!(fs::read_to_string(&foreign).unwrap(), "untouched");
    assert!(!root.path().join("calls").exists());
}

#[cfg(target_os = "linux")]
struct DeliveryFixture {
    root: tempfile::TempDir,
    release_root: std::path::PathBuf,
    release: std::path::PathBuf,
}

#[cfg(target_os = "linux")]
impl DeliveryFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path());
        let tools = root.path().join("tools");
        let release_root = root.path().join("release space $%\" path");
        let release = release_root.join("releases/fixture-commit");
        fs::create_dir_all(release.join("bin")).unwrap();
        let binary = release.join("bin/studio-runtime-server");
        fs::write(&binary, "fixture executable; not invoked").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        // The bypass marker exists only beside a disposable copy, never beside
        // the production checkout's delivery script.
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("deliver.sh"),
            root.path().join("deliver.sh"),
        )
        .unwrap();
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("service-layout.json"),
            root.path().join("service-layout.json"),
        )
        .unwrap();
        fs::write(
            root.path().join(".codex-studio-delivery-test"),
            "fixture-only\n",
        )
        .unwrap();
        let fixture = Self {
            root,
            release_root,
            release,
        };
        for (name, script) in [
            (
                "git",
                r#"#!/bin/sh
case "$*" in
  *--show-toplevel*) printf '%s\n' "$PROJECT";;
  *--verify*) echo fixture-commit;;
  *archive*) echo archive >> "$HOME/build-calls"; exit 97;;
  *) exit 97;;
esac
"#,
            ),
            (
                "systemctl",
                r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/calls"
case "$*" in *NRestarts*) echo 0;; *MainPID*) echo 1234;; esac
"#,
            ),
            (
                "readlink",
                r#"#!/bin/sh
case "$1" in
  -f) printf '%s\n' "${HEALTH_BINARY:-$RELEASE/bin/studio-runtime-server}";;
  *) /usr/bin/readlink "$@";;
esac
"#,
            ),
            ("curl", "#!/bin/sh\nexit 0\n"),
            ("sleep", "#!/bin/sh\nexit 0\n"),
            (
                "journalctl",
                r#"#!/bin/sh
printf '%s\n' "$@" > "$HOME/journal-args"
IFS= read -r invocation < "$HOME/journal-invocation"
case " $* " in
  *" _SYSTEMD_INVOCATION_ID=$invocation "*) /bin/cat "$HOME/journal";;
  *) echo 'stale success from an old invocation';;
esac
"#,
            ),
            (
                "systemd-run",
                r#"#!/bin/bash
if [ "${1:-}" = --version ]; then
  echo "systemd ${SYSTEMD_VERSION:-259}"
  exit 0
fi
printf '%s\n' "$@" > "$HOME/run-args"
if [ -n "${RUN_FAILURE:-}" ]; then
  echo 'fixture unit failed' >&2
  exit "$RUN_FAILURE"
fi
environment=()
while [ "$#" -gt 0 ]; do
  case "$1" in
    --setenv=*)
      assignment=${1#--setenv=}
      if [[ $assignment != *=* ]]; then assignment="$assignment=${!assignment}"; fi
      environment+=("$assignment");;
    --) shift; break;;
    --*) ;;
    *) break;;
  esac
  shift
done
IFS= read -r invocation < /proc/sys/kernel/random/uuid
invocation=${invocation//-/}
printf '%s\n' "$invocation" > "$HOME/journal-invocation"
/usr/bin/env -i "${environment[@]}" INVOCATION_ID="$invocation" CODEX_STUDIO_DELIVER_TEST_IN_PROCESS=1 PROJECT="$PROJECT" RELEASE="$RELEASE" "$@" > "$HOME/journal" 2>&1
exit $?
"#,
            ),
        ] {
            fixture.tool(name, script);
        }
        // Whitelist host utilities: a missing mock must never reach the real
        // systemd-run, systemctl or journalctl, even on a host running Studio.
        for name in [
            "dirname", "mkdir", "flock", "rm", "tar", "cp", "mv", "ln", "ls", "tail", "date",
            "touch", "find", "sort",
        ] {
            std::os::unix::fs::symlink(Path::new("/usr/bin").join(name), tools.join(name)).unwrap();
        }
        std::os::unix::fs::symlink(test_node(), tools.join("node")).unwrap();
        fixture
    }

    fn tool(&self, name: &str, script: &str) {
        let path = self.root.path().join("tools").join(name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn command(&self) -> Command {
        let mut command = Command::new("/bin/bash");
        command
            .arg(self.root.path().join("deliver.sh"))
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env_clear()
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("CODEX_STUDIO_SERVER_ROOT", &self.release_root)
            .env("PROJECT", env!("CARGO_MANIFEST_DIR"))
            .env("RELEASE", &self.release)
            .env("PATH", self.root.path().join("tools"));
        command
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.root.path().join(name)).unwrap()
    }
}

#[test]
#[cfg(target_os = "linux")]
fn delivery_runs_in_a_journalled_unit_before_building_and_returns_failure() {
    let fixture = DeliveryFixture::new();
    // Force the build path, where this fixture's git archive fails deterministically.
    fs::remove_file(fixture.release.join("bin/studio-runtime-server")).unwrap();
    let output = fixture.command().output().unwrap();
    assert!(!output.status.success());
    assert!(
        fixture.root.path().join("run-args").exists(),
        "delivery ran in its caller instead of a transient unit"
    );
    let args = fixture.read("run-args");
    for required in [
        "--user",
        "--wait",
        "--collect",
        "--same-dir",
        "--expand-environment=no",
        "--property=StandardOutput=journal",
        "--property=StandardError=journal",
    ] {
        assert!(
            args.lines().any(|line| line == required),
            "missing {required}: {args}"
        );
    }
    assert!(
        !args
            .lines()
            .any(|line| line == "--pipe" || line == "--scope")
    );
    let unit = args
        .lines()
        .find_map(|line| line.strip_prefix("--unit="))
        .unwrap();
    assert!(unit.starts_with("codex-studio-deliver-"));
    assert!(String::from_utf8_lossy(&output.stdout).contains(unit));
    assert!(fixture.read("journal-args").contains(unit));
    assert!(fixture.read("journal").contains("exit code"));
    assert_eq!(fixture.read("build-calls"), "archive\n");
    assert!(!fixture.root.path().join("calls").exists());
}

#[test]
#[cfg(target_os = "linux")]
fn delivery_rejects_inherited_guards_without_unit_membership_or_fixture_marker() {
    let fixture = DeliveryFixture::new();
    let output = fixture
        .command()
        .env("CODEX_STUDIO_DELIVER_IN_UNIT", "1")
        .env("INVOCATION_ID", "0123456789abcdef0123456789abcdef")
        .env(
            "CODEX_STUDIO_DELIVER_UNIT",
            "codex-studio-deliver-inherited.service",
        )
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("delivery guard requires verified unit membership")
    );
    assert!(!fixture.root.path().join("run-args").exists());
    // A test flag alone must not bypass handoff from a production script.
    fs::remove_file(fixture.root.path().join(".codex-studio-delivery-test")).unwrap();
    let output = fixture
        .command()
        .env("CODEX_STUDIO_DELIVER_TEST_IN_PROCESS", "1")
        .env("RUN_FAILURE", "37")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(37));
    assert!(fixture.root.path().join("run-args").exists());
}

#[test]
#[cfg(target_os = "linux")]
fn delivery_refuses_systemd_without_literal_argument_support() {
    let fixture = DeliveryFixture::new();
    let output = fixture
        .command()
        .env("SYSTEMD_VERSION", "253")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("systemd 254 or newer is required"));
    assert!(!fixture.root.path().join("run-args").exists());
    assert!(
        fixture
            .command()
            .env("SYSTEMD_VERSION", "254")
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
#[cfg(target_os = "linux")]
fn delivery_uses_unique_units_and_replays_only_the_current_invocation() {
    let fixture = DeliveryFixture::new();
    let mut identities = Vec::new();
    for _ in 0..2 {
        let output = fixture.command().output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let args = fixture.read("run-args");
        let unit = args
            .lines()
            .find(|line| line.starts_with("--unit="))
            .unwrap()
            .to_owned();
        let invocation = fixture.read("journal-invocation").trim().to_owned();
        assert!(
            fixture
                .read("journal-args")
                .lines()
                .any(|line| line == format!("_SYSTEMD_INVOCATION_ID={invocation}"))
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("stale success"));
        identities.push((unit, invocation));
    }
    assert_ne!(identities[0].0, identities[1].0);
    assert_ne!(identities[0].1, identities[1].1);
}

#[cfg(target_os = "linux")]
fn invocation_record_budget() -> usize {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("deliver.sh"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("readonly KEPT_INVOCATION_RECORDS="))
        .expect("deliver.sh owns the invocation history budget")
        .parse()
        .unwrap()
}

#[test]
#[cfg(target_os = "linux")]
fn delivery_bounds_invocation_history_and_preserves_active_and_latest_records() {
    let fixture = DeliveryFixture::new();
    let kept = invocation_record_budget();
    let directory = fixture.release_root.join("delivery-invocations");
    fs::create_dir(&directory).unwrap();
    let active = directory.join("active-fixture");
    let active_file = fs::File::create(&active).unwrap();
    active_file.lock_shared().unwrap();
    active_file
        .set_times(fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
        .unwrap();
    let lock = fs::File::create(fixture.release_root.join("deliver.lock")).unwrap();
    // Successful, lock-refused, and failed-build attempts must all prune.
    let binary = fixture.release.join("bin/studio-runtime-server");
    for mode in 0..3 {
        // Seed excess history directly; each outcome still has to prune an
        // overfull directory, without launching a delivery for every old row.
        for index in 0..kept + 4 {
            let old = fs::File::create(directory.join(format!("old-{mode}-{index:032x}"))).unwrap();
            old.set_times(
                fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1)),
            )
            .unwrap();
        }
        if mode == 1 {
            lock.try_lock().unwrap();
        } else if mode == 2 {
            fs::remove_file(&binary).unwrap();
        }
        let output = fixture.command().output().unwrap();
        assert_eq!(output.status.success(), mode == 0);
        if mode == 1 {
            lock.unlock().unwrap();
        } else if mode == 2 {
            fs::write(&binary, "fixture executable; not invoked").unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let invocation = fixture.read("journal-invocation");
        let records: Vec<_> = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert!(active.exists(), "pruning removed an in-use record");
        assert!(
            records
                .iter()
                .any(|path| fs::read_to_string(path).unwrap() == invocation),
            "pruning removed the latest attempt"
        );
        assert!(
            records.len() <= kept + 1,
            "invocation history grew past its bound: {} > {}",
            records.len(),
            kept + 1
        );
    }
    drop(active_file);
    assert!(fixture.command().output().unwrap().status.success());
    assert!(!active.exists(), "inactive old record was never collected");
    assert!(fs::read_dir(directory).unwrap().count() <= kept);
}

#[test]
#[cfg(target_os = "linux")]
fn delivery_recovers_oversized_invocation_history_with_small_argument_limit() {
    let mut fixture = DeliveryFixture::new();
    // A longer disposable root and a fixture-only low stack limit reproduce
    // ARG_MAX with hundreds of files instead of tens of thousands. Node and
    // the rest of delivery retain their normal limits; only the ls wrapper
    // lowers its soft limit immediately before execing the real utility.
    let root = fixture.root.path().join("r".repeat(200));
    fs::rename(&fixture.release_root, &root).unwrap();
    fixture.release_root = root;
    fixture.release = fixture.release_root.join("releases/fixture-commit");
    let limit = Command::new("/bin/bash")
        .args(["-c", "ulimit -S -s 256; /usr/bin/getconf ARG_MAX"])
        .output()
        .unwrap();
    assert!(limit.status.success());
    let argument_limit: usize = String::from_utf8(limit.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let directory = fixture.release_root.join("delivery-invocations");
    fs::create_dir(&directory).unwrap();
    let count = argument_limit / (directory.as_os_str().len() + 34) + 32;
    for index in 0..count {
        fs::write(directory.join(format!("{index:032x}")), "old invocation\n").unwrap();
    }
    fs::remove_file(fixture.root.path().join("tools/ls")).unwrap();
    fixture.tool(
        "ls",
        r#"#!/bin/bash
ulimit -S -s 256
exec /usr/bin/ls "$@"
"#,
    );
    let output = fixture.command().output().unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let kept = invocation_record_budget();
    assert!(
        fs::read_dir(directory).unwrap().count() <= kept,
        "oversized history was not collected"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("delivered fixture-commit"));
}

#[test]
#[cfg(target_os = "linux")]
fn delivery_housekeeping_failure_warns_without_changing_unit_outcome() {
    for failed_unit in [false, true] {
        let fixture = DeliveryFixture::new();
        fs::remove_file(fixture.root.path().join("tools/tail")).unwrap();
        fixture.tool(
            "tail",
            &format!(
                r#"#!/bin/sh
case "$*" in *+{}*) echo 'fixture pruning failure' >&2; exit 77;; esac
exec /usr/bin/tail "$@"
"#,
                invocation_record_budget() + 1
            ),
        );
        let mut command = fixture.command();
        if failed_unit {
            command.env("RUN_FAILURE", "37");
        }
        let output = command.output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(if failed_unit { 37 } else { 0 }),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let logs = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            logs.contains("warning: could not prune delivery invocation history"),
            "{logs}"
        );
    }
}

#[test]
#[cfg(target_os = "linux")]
fn delivery_propagates_failed_unit_status_and_refuses_missing_runner() {
    let fixture = DeliveryFixture::new();
    let output = fixture.command().env("RUN_FAILURE", "37").output().unwrap();
    assert_eq!(output.status.code(), Some(37));
    assert!(String::from_utf8_lossy(&output.stderr).contains("fixture unit failed"));
    assert!(!fixture.release_root.join("deliver.lock").exists());
    fs::remove_file(fixture.root.path().join("tools/systemd-run")).unwrap();
    let output = fixture.command().output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("systemd-run is required"));
    assert!(!fixture.root.path().join("calls").exists());
}

#[test]
#[cfg(target_os = "linux")]
fn delivery_unit_refuses_another_delivery_holding_the_release_lock() {
    let fixture = DeliveryFixture::new();
    let lock = fs::File::create(fixture.release_root.join("deliver.lock")).unwrap();
    lock.try_lock().unwrap();
    let output = fixture.command().output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("another delivery is running"));
    assert!(fixture.root.path().join("run-args").exists());
    assert!(!fixture.root.path().join("calls").exists());
}

#[test]
#[cfg(target_os = "linux")]
fn delivery_and_service_install_share_the_same_escaped_release_command() {
    let fixture = DeliveryFixture::new();
    let install = Command::new(env!("CARGO_BIN_EXE_studio-runtime-server"))
        .args(["service", "install"])
        .env_clear()
        .env("HOME", fixture.root.path())
        .env("PATH", fixture.root.path().join("tools"))
        .env("XDG_CONFIG_HOME", fixture.root.path().join("config"))
        .env("CODEX_STUDIO_SERVER_ROOT", &fixture.release_root)
        .output()
        .unwrap();
    assert!(
        install.status.success(),
        "{}",
        String::from_utf8_lossy(&install.stderr)
    );
    let unit = fixture.read("config/systemd/user/codex-studio.service");
    let delivery = fixture.command().output().unwrap();
    assert!(
        delivery.status.success(),
        "{}",
        String::from_utf8_lossy(&delivery.stderr)
    );
    let dropin = fixture.read("config/systemd/user/codex-studio.service.d/zz-delivery.conf");
    let command = unit
        .lines()
        .find(|line| line.starts_with("ExecStart="))
        .unwrap();
    assert!(
        dropin.lines().any(|line| line == command),
        "{dropin} != {command}"
    );
    assert!(
        fixture
            .read("calls")
            .contains("--user restart codex-studio.service")
    );
    assert!(
        !fixture
            .root
            .path()
            .join(".local/state/codex-agents")
            .exists()
    );
}

#[test]
#[cfg(target_os = "linux")]
fn delivery_test_mode_rolls_back_without_launching_a_unit() {
    for has_previous in [false, true] {
        let fixture = DeliveryFixture::new();
        let previous = fixture.release_root.join("releases/previous");
        if has_previous {
            fs::create_dir(&previous).unwrap();
            std::os::unix::fs::symlink(&previous, fixture.release_root.join("current")).unwrap();
        }
        // Fail only the final HTTP probe, after startup succeeded.
        fixture.tool(
            "curl",
            r#"#!/bin/sh
if [ -f "$HOME/first-probe" ]; then exit 1; fi
: > "$HOME/first-probe"
"#,
        );
        let output = fixture
            .command()
            .env("CODEX_STUDIO_DELIVER_TEST_IN_PROCESS", "1")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&output.stderr).contains("rolling back"));
        assert!(!fixture.root.path().join("run-args").exists());
        assert_eq!(
            fixture
                .read("calls")
                .lines()
                .filter(|line| line.contains(" restart "))
                .count(),
            2
        );
        if has_previous {
            assert_eq!(
                fs::read_link(fixture.release_root.join("current")).unwrap(),
                previous
            );
        } else {
            assert!(!fixture.release_root.join("current").exists());
            assert!(
                !fixture
                    .root
                    .path()
                    .join("config/systemd/user/codex-studio.service.d/zz-delivery.conf")
                    .exists()
            );
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
fn delivery_unit_finishes_health_checks_and_prunes_old_releases() {
    let fixture = DeliveryFixture::new();
    for (index, name) in ["previous", "discard", "keep"].iter().enumerate() {
        let path = fixture.release_root.join("releases").join(name);
        fs::create_dir(&path).unwrap();
        fs::File::open(&path)
            .unwrap()
            .set_times(
                fs::FileTimes::new().set_modified(
                    std::time::UNIX_EPOCH + std::time::Duration::from_secs(index as u64),
                ),
            )
            .unwrap();
    }
    let previous = fixture.release_root.join("releases/previous");
    std::os::unix::fs::symlink(&previous, fixture.release_root.join("current")).unwrap();
    // Keep the previous release even when it is older than the retention cutoff.
    let discard = fixture.release_root.join("releases/older-discard");
    fs::create_dir(&discard).unwrap();
    fs::File::open(&discard)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
        .unwrap();
    let output = fixture.command().output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_link(fixture.release_root.join("current")).unwrap(),
        fixture.release
    );
    assert!(previous.is_dir());
    assert!(!discard.exists());
    assert!(fixture.read("calls").contains("--user is-active --quiet"));
    let journal = fixture.read("journal");
    assert!(journal.contains("delivered fixture-commit"));
    assert!(journal.contains("exit code 0"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("delivered fixture-commit"));
}

#[test]
#[cfg(target_os = "linux")]
fn service_state_identity_stays_stable_when_a_symlinked_missing_tail_is_created() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let parent = root.path().join("actual-parent");
    fs::create_dir(&parent).unwrap();
    symlink(&parent, root.path().join("parent-link")).unwrap();
    let state = root.path().join("parent-link/state");
    let args = ["service", "install", "--state", state.to_str().unwrap()];
    assert!(cli(root.path(), &args).status.success());
    assert!(!state.exists());
    let unit = root.path().join("config/systemd/user/codex-studio.service");
    let first = fs::read_to_string(&unit).unwrap();
    assert!(first.contains(parent.to_str().unwrap()));
    fs::create_dir(&state).unwrap();
    assert!(cli(root.path(), &args).status.success());
    assert_eq!(fs::read_to_string(&unit).unwrap(), first);
    assert_eq!(fs::read_dir(&state).unwrap().count(), 0);
}
