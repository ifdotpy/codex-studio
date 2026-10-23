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
    fs::copy(
        env!("CARGO_BIN_EXE_studio-runtime-server"),
        binaries.join("studio-runtime-server"),
    )
    .unwrap();
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

#[test]
#[cfg(target_os = "linux")]
fn delivery_and_service_install_share_the_same_escaped_release_command() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let tools = root.path().join("tools");
    let release_root = root.path().join("release space $%\" path");
    let release = release_root.join("releases/fixture-commit");
    fs::create_dir_all(release.join("bin")).unwrap();
    let binary = release.join("bin/studio-runtime-server");
    fs::write(&binary, "fixture executable; not invoked").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    for (name, script) in [
        (
            "git",
            "#!/bin/sh\ncase \"$*\" in *--show-toplevel*) printf '%s\\n' \"$PROJECT\";; *--verify*) echo fixture-commit;; *) exit 97;; esac\n",
        ),
        (
            "systemctl",
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOME/calls\"\ncase \"$*\" in *NRestarts*) echo 0;; *MainPID*) echo 1234;; esac\n",
        ),
        (
            "readlink",
            "#!/bin/sh\ncase \"$1\" in -f) printf '%s\\n' \"$RELEASE/bin/studio-runtime-server\";; *) exit 1;; esac\n",
        ),
        ("curl", "#!/bin/sh\nexit 0\n"),
        ("sleep", "#!/bin/sh\nexit 0\n"),
    ] {
        fs::write(tools.join(name), script).unwrap();
        fs::set_permissions(tools.join(name), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let install = Command::new(env!("CARGO_BIN_EXE_studio-runtime-server"))
        .args(["service", "install"])
        .env_clear()
        .env("HOME", root.path())
        .env("PATH", &tools)
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("CODEX_STUDIO_SERVER_ROOT", &release_root)
        .env("CODEX_AGENTS_PYTHON", "/nonexistent")
        .output()
        .unwrap();
    assert!(
        install.status.success(),
        "{}",
        String::from_utf8_lossy(&install.stderr)
    );
    let unit =
        fs::read_to_string(root.path().join("config/systemd/user/codex-studio.service")).unwrap();
    let project = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../..")
        .canonicalize()
        .unwrap();
    let delivery = Command::new("/bin/bash")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("deliver.sh"))
        .env_clear()
        .env("HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("CODEX_STUDIO_SERVER_ROOT", &release_root)
        .env("PROJECT", project)
        .env("RELEASE", &release)
        .env("CODEX_AGENTS_PYTHON", "/nonexistent")
        .env(
            "PATH",
            std::env::join_paths([tools.as_path(), Path::new("/usr/bin"), Path::new("/bin")])
                .unwrap(),
        )
        .output()
        .unwrap();
    assert!(
        delivery.status.success(),
        "{}",
        String::from_utf8_lossy(&delivery.stderr)
    );
    let dropin = fs::read_to_string(
        root.path()
            .join("config/systemd/user/codex-studio.service.d/zz-delivery.conf"),
    )
    .unwrap();
    let command = unit
        .lines()
        .find(|line| line.starts_with("ExecStart="))
        .unwrap();
    assert!(
        dropin.lines().any(|line| line == command),
        "{dropin} != {command}"
    );
    let calls = fs::read_to_string(root.path().join("calls")).unwrap();
    assert!(calls.contains("--user restart codex-studio.service"));
    assert!(!root.path().join(".local/state/codex-agents").exists());
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
