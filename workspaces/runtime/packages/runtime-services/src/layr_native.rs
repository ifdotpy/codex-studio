//! Host Layr workspaces. The Studio state directory owns the store and binary.
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub use studio_runtime_store::NATIVE_LAYR_DIRECTORY;
/// Store directory below the native installation directory, outside project checkouts.
pub const STORE_DIRECTORY: &str = "store";
/// Host Layr release required by Studio's command contract.
pub const LAYR_VERSION: &str = "0.2.1";

pub fn store_path(state_dir: &Path) -> PathBuf {
    state_dir.join(NATIVE_LAYR_DIRECTORY).join(STORE_DIRECTORY)
}

pub fn binary_path(state_dir: &Path) -> PathBuf {
    state_dir.join(NATIVE_LAYR_DIRECTORY).join("bin/layr")
}

/// Dedicated Linux root-daemon socket for this Studio state directory.
pub fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join(NATIVE_LAYR_DIRECTORY).join("layr.sock")
}

pub fn is_native(record: &Value) -> bool {
    record["workspaceMode"] == "layr-native"
}

/// Creation records intent only. `workspace_ready_git` owns filesystem effects.
pub fn configure_new_chat(record: &mut Value, source: &str) {
    record["workspaceMode"] = json!("layr-native");
    record["executionMode"] = json!("native");
    record["hostProjectPath"] = json!(source);
    record["layrReady"] = json!(false);
    record["worktree"] = json!(false);
    record["worktreeReady"] = json!(false);
    record["workspaceInGit"] = json!(false);
}

pub fn configure_child(record: &mut Value, parent: &Value, root: &Value) {
    let source = root["hostProjectPath"]
        .as_str()
        .or_else(|| root["cwd"].as_str())
        .unwrap_or_default();
    configure_new_chat(record, source);
    record["cwd"] = parent["cwd"].clone();
    if let Some(project) = root.get("layrProjectId") {
        record["layrProjectId"] = project.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_creation_preserves_host_execution_and_source_identity() {
        let mut record = json!({"id":"lead", "cwd":"/code", "isLead":true});
        configure_new_chat(&mut record, "/code");
        assert_eq!(record["workspaceMode"], "layr-native");
        assert_eq!(record["executionMode"], "native");
        assert_eq!(record["hostProjectPath"], "/code");
        assert_eq!(record["layrReady"], false);
        assert_eq!(
            store_path(Path::new("/state")),
            Path::new("/state/layr-native/store")
        );
        assert_eq!(
            binary_path(Path::new("/state")),
            Path::new("/state/layr-native/bin/layr")
        );
    }

    #[test]
    fn native_child_inherits_project_and_keeps_reviewer_role() {
        let root =
            json!({"cwd":"/store/main", "hostProjectPath":"/code", "layrProjectId":"project"});
        let mut child = json!({"id":"worker", "role":"reviewer"});
        configure_child(&mut child, &root, &root);
        assert_eq!(child["role"], "reviewer");
        assert_eq!(child["hostProjectPath"], "/code");
        assert_eq!(child["layrProjectId"], "project");
        assert_eq!(child["worktree"], false);
    }
}

mod capability;
pub mod install;
pub use capability::{NativeLayrCapability, capability};
mod runtime;
pub use runtime::{
    NativeLayrError, agent_value, evidence, main_path, merge, operation, prepare, project_id,
    remove, run, save, save_turn,
};

/// Thread commands must resolve Studio's host binary before any user Cargo installation.
pub fn thread_environment(state_root: &Path) -> serde_json::Map<String, Value> {
    let bin = binary_path(state_root);
    let path = format!(
        "{}:{}",
        bin.parent().unwrap_or(Path::new("/")).display(),
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
    );
    let mut environment = transport_environment(state_root, cfg!(target_os = "linux"));
    environment.insert("PATH".into(), json!(path));
    environment
}

fn transport_environment(state_root: &Path, uses_service: bool) -> serde_json::Map<String, Value> {
    let mut environment = serde_json::Map::from_iter([
        ("LAYR_ROOT".into(), json!(store_path(state_root))),
        (
            "LAYR_DIRECT".into(),
            json!(if uses_service { "0" } else { "1" }),
        ),
    ]);
    if uses_service {
        environment.insert("LAYR_SOCKET".into(), json!(socket_path(state_root)));
    }
    environment
}
mod import;
pub mod migration;

#[cfg(test)]
mod receipt_tests {
    use super::*;

    #[test]
    fn linux_native_environment_selects_the_owned_privileged_service() {
        let env = transport_environment(Path::new("/studio"), true);
        assert_eq!(env["LAYR_ROOT"], "/studio/layr-native/store");
        assert_eq!(env["LAYR_DIRECT"], "0");
        assert_eq!(env["LAYR_SOCKET"], "/studio/layr-native/layr.sock");
    }

    #[test]
    fn completed_operations_replay_without_repeating_the_effect() {
        let state = tempfile::tempdir().unwrap();
        let intent = json!({"line":"worker"});
        let first = operation(state.path(), "create-worker", &intent, || {
            Ok(json!({"stateId":"full-state"}))
        })
        .unwrap();
        let again = operation(state.path(), "create-worker", &intent, || {
            panic!("must not repeat")
        })
        .unwrap();
        assert_eq!(first, again);
        assert!(
            operation(
                state.path(),
                "create-worker",
                &json!({"line":"other"}),
                || panic!("changed identity must not run")
            )
            .is_err()
        );
    }

    #[test]
    fn unknown_operations_stay_held_after_a_lost_result() {
        let state = tempfile::tempdir().unwrap();
        let intent = json!({"state":"reviewed"});
        assert!(matches!(
            operation(state.path(), "merge", &intent, || Err(
                NativeLayrError::Unknown("lost response".into())
            )),
            Err(NativeLayrError::Unknown(_))
        ));
        assert!(matches!(
            operation(state.path(), "merge", &intent, || panic!(
                "unknown merge must not repeat"
            )),
            Err(NativeLayrError::Unknown(_))
        ));
    }

    #[test]
    fn explicit_failures_replay_the_same_refusal() {
        let state = tempfile::tempdir().unwrap();
        let intent = json!({"state":"reviewed"});
        assert!(
            operation(state.path(), "merge", &intent, || Err(
                NativeLayrError::Refused("line moved".into())
            ))
            .is_err()
        );
        let result = operation(state.path(), "merge", &intent, || {
            panic!("failed merge must not repeat")
        });
        assert!(result.unwrap_err().to_string().contains("line moved"));
    }

    #[test]
    fn project_identity_is_shared_by_migration_and_chats_with_a_logical_binding() {
        let mut record = json!({"hostProjectPath":"/project"});
        let before = project_id(&record).unwrap();
        record["projectId"] = json!("logical-project");
        assert_eq!(project_id(&record).unwrap(), before);
    }

    #[test]
    fn native_environment_selects_the_managed_binary() {
        let env = thread_environment(Path::new("/studio"));
        assert_eq!(env["LAYR_ROOT"], "/studio/layr-native/store");
        assert_eq!(
            env["LAYR_DIRECT"],
            if cfg!(target_os = "linux") { "0" } else { "1" }
        );
        assert!(
            env["PATH"]
                .as_str()
                .unwrap()
                .starts_with("/studio/layr-native/bin:")
        );
    }

    #[test]
    #[ignore = "requires a real Layr 0.2.0 host binary and APFS or btrfs"]
    fn real_host_import_worker_merge_archive_and_warm_caches() {
        use std::{fs, path::PathBuf, process::Command};
        let binary = PathBuf::from(
            std::env::var_os("STUDIO_LAYR_NATIVE_TEST_BINARY")
                .expect("Set STUDIO_LAYR_NATIVE_TEST_BINARY"),
        );
        let state = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("code.txt"), "base\n").unwrap();
        fs::write(source.path().join(".gitignore"), "node_modules/\n.env\n").unwrap();
        fs::write(
            source.path().join(".layrattributes"),
            "node_modules regenerable\n",
        )
        .unwrap();
        fs::write(source.path().join(".env"), "TEST_VALUE=local\n").unwrap();
        fs::create_dir(source.path().join("node_modules")).unwrap();
        fs::write(source.path().join("node_modules/old"), "excluded\n").unwrap();
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(source.path())
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "-q"]);
        git(&["-c", "core.hooksPath=/dev/null", "add", "."]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-qm",
            "Base",
        ]);
        fs::create_dir_all(binary_path(state.path()).parent().unwrap()).unwrap();
        fs::copy(binary, binary_path(state.path())).unwrap();
        let mut lead = json!({"id":"lead","isLead":true,"autoWake":true,"cwd":source.path()});
        configure_new_chat(
            &mut lead,
            fs::canonicalize(source.path()).unwrap().to_str().unwrap(),
        );
        let ready = runtime::prepare_files(state.path(), &lead).unwrap();
        for (key, value) in ready.as_object().unwrap() {
            lead[key] = value.clone();
        }
        let main = PathBuf::from(lead["cwd"].as_str().unwrap());
        assert!(main.join(".env").is_file());
        assert!(!main.join("node_modules/old").exists());
        fs::create_dir_all(main.join("node_modules")).unwrap();
        fs::write(main.join("node_modules/warm"), "warm cache\n").unwrap();
        run(state.path(), &main, &["save", "--caches"]).unwrap();
        let mut worker = json!({"id":"worker","role":"implementer","autoWake":true,"isLead":false});
        configure_child(&mut worker, &lead, &lead);
        let ready = runtime::prepare_files(state.path(), &worker).unwrap();
        for (key, value) in ready.as_object().unwrap() {
            worker[key] = value.clone();
        }
        let cwd = PathBuf::from(worker["cwd"].as_str().unwrap());
        assert_eq!(
            fs::read_to_string(cwd.join("node_modules/warm")).unwrap(),
            "warm cache\n"
        );
        fs::write(cwd.join("code.txt"), "worker result\n").unwrap();
        run(state.path(), &cwd, &["add", "code.txt"]).unwrap();
        run(state.path(), &cwd, &["save", "-m", "Worker result"]).unwrap();
        let state_id = run(state.path(), &cwd, &["rev-parse", "head"]).unwrap();
        assert_eq!(
            evidence(state.path(), &worker, &state_id).unwrap()["stateId"],
            state_id
        );
        assert!(evidence(state.path(), &worker, &state_id[..8]).is_err());
        merge(state.path(), &lead, &worker, &state_id, "accept-worker").unwrap();
        assert_eq!(
            fs::read_to_string(main.join("code.txt")).unwrap(),
            "worker result\n"
        );
        let mut reviewer =
            json!({"id":"reviewer","role":"reviewer","autoWake":true,"isLead":false});
        configure_child(&mut reviewer, &lead, &lead);
        let ready = runtime::prepare_files(state.path(), &reviewer).unwrap();
        for (key, value) in ready.as_object().unwrap() {
            reviewer[key] = value.clone();
        }
        assert_eq!(reviewer["cwd"], reviewer["layrScratch"]);
        assert_eq!(Path::new(reviewer["layrReadCwd"].as_str().unwrap()), main);
        let args = vec!["try".into(),state_id.clone(),"--".into(),"/bin/sh".into(),"-c".into(),
            "printf local > review-check; printf note > \"$TMPDIR/note\"; if printf forbidden > \"$1/code.txt\"; then exit 7; fi".into(),
            "review-check".into(),main.to_string_lossy().into_owned()];
        reviewer_try(state.path(), &reviewer, &args).unwrap();
        let environment = reviewer_try(
            state.path(),
            &reviewer,
            &[
                "try".into(),
                state_id.clone(),
                "--".into(),
                "/usr/bin/env".into(),
            ],
        )
        .unwrap();
        assert!(
            !environment
                .lines()
                .any(|line| line.starts_with("LAYR_ROOT=")
                    || line.starts_with("LAYR_DIRECT=")
                    || line.starts_with("LAYR_SOCKET=")),
            "Reviewer check inherited the runtime store environment"
        );
        assert_eq!(
            fs::read_to_string(main.join("code.txt")).unwrap(),
            "worker result\n"
        );
        assert!(!main.join("review-check").exists());
        assert!(
            PathBuf::from(reviewer["layrScratch"].as_str().unwrap())
                .join("note")
                .is_file()
        );
        save(state.path(), &worker, "worker-turn-end").unwrap();
        remove(state.path(), &worker, "archive-worker").unwrap();
        assert!(!cwd.exists());
        assert!(
            run(
                state.path(),
                &main,
                &["show", &format!("{state_id}:code.txt")]
            )
            .unwrap()
            .contains("worker result")
        );
        let worktree = tempfile::tempdir().unwrap();
        git(&[
            "worktree",
            "add",
            "-b",
            "migration-fixture",
            worktree.path().to_str().unwrap(),
        ]);
        fs::write(worktree.path().join("code.txt"), "migrated worktree\n").unwrap();
        fs::write(worktree.path().join(".env"), "TEST_VALUE=migrated\n").unwrap();
        let plan = migration::migrate(state.path(), source.path(), true).unwrap();
        assert_eq!(plan["lines"].as_array().unwrap().len(), 1);
        let imported = migration::migrate(state.path(), source.path(), false).unwrap();
        assert_eq!(imported["projectId"], lead["layrProjectId"]);
        let migrated = main
            .parent()
            .unwrap()
            .join(imported["lines"][0]["line"].as_str().unwrap());
        assert_eq!(
            fs::read_to_string(migrated.join("code.txt")).unwrap(),
            "migrated worktree\n"
        );
        assert_eq!(
            fs::read_to_string(migrated.join(".env")).unwrap(),
            "TEST_VALUE=migrated\n"
        );
        let again = migration::migrate(state.path(), source.path(), false).unwrap();
        assert_eq!(imported["lines"], again["lines"]);
        println!("native host evidence store: {}", state.path().display());
    }
}

/// One allowlist for review checks and native Claude reviewer sessions.
pub(crate) fn reviewer_environment(
    state_dir: &Path,
    scratch: &str,
) -> serde_json::Map<String, Value> {
    let mut environment = serde_json::Map::new();
    environment.insert("PATH".into(), thread_environment(state_dir)["PATH"].clone());
    environment.insert("HOME".into(), json!(scratch));
    environment.insert("TMPDIR".into(), json!(scratch));
    environment.insert(
        "LANG".into(),
        json!(std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".into())),
    );
    environment.insert(
        "TERM".into(),
        json!(std::env::var("TERM").unwrap_or_else(|_| "dumb".into())),
    );
    for name in ["CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN"] {
        let value = std::env::var(name).ok().or_else(|| {
            let suffix = match name {
                "CARGO_HOME" => ".cargo",
                "RUSTUP_HOME" => ".rustup",
                _ => return None,
            };
            let folder = PathBuf::from(std::env::var_os("HOME")?).join(suffix);
            folder
                .is_dir()
                .then(|| folder.to_string_lossy().into_owned())
        });
        if let Some(value) = value {
            environment.insert(name.into(), json!(value));
        }
    }
    environment
}

/// A review check may change only its temporary copy and private scratch folder.
pub fn reviewer_try(
    state_dir: &Path,
    record: &Value,
    argv: &[String],
) -> Result<String, NativeLayrError> {
    if record["role"] != "reviewer" || argv.len() < 4 || argv[0] != "try" || argv[2] != "--" {
        return Err(NativeLayrError::Refused(
            "Use try <state> -- <command> for a reviewer check".into(),
        ));
    }
    let cwd = reviewer_read_cwd(state_dir, record)?;
    let scratch = record["layrScratch"]
        .as_str()
        .ok_or_else(|| NativeLayrError::Refused("Missing reviewer scratch folder".into()))?;
    #[cfg(target_os = "macos")]
    let wrapper = r#"scratch=$1; profile=$2; shift 3; export TMPDIR="$scratch"; exec /usr/bin/sandbox-exec -D "WORKSPACE=$PWD" -D "SCRATCH=$scratch" -p "$profile" "$@""#;
    #[cfg(target_os = "macos")]
    let profile = r#"(version 1)(deny default)(allow process*)(allow sysctl-read)(allow mach-lookup)(allow file-read*)(allow file-write* (subpath (param "WORKSPACE")) (subpath (param "SCRATCH")))"#;
    #[cfg(not(target_os = "macos"))]
    let wrapper = linux_reviewer_wrapper();
    #[cfg(not(target_os = "macos"))]
    let profile = "";
    let state = std::fs::canonicalize(state_dir)?;
    let state = state
        .to_str()
        .ok_or_else(|| NativeLayrError::Refused("Non-UTF-8 state path".into()))?;
    let environment: Vec<String> = reviewer_environment(state_dir, scratch)
        .into_iter()
        .map(|(key, value)| format!("{key}={}", value.as_str().unwrap_or_default()))
        .collect();
    let mut args = vec!["try", argv[1].as_str(), "--", "/usr/bin/env", "-i"];
    args.extend(environment.iter().map(String::as_str));
    args.extend([
        "/bin/sh",
        "-c",
        wrapper,
        "studio-review-try",
        scratch,
        profile,
        state,
    ]);
    args.extend(argv[3..].iter().map(String::as_str));
    run(state_dir, &cwd, &args)
}

pub(crate) fn reviewer_read_cwd(
    state_dir: &Path,
    record: &Value,
) -> Result<PathBuf, NativeLayrError> {
    if record["role"] != "reviewer" {
        return Err(NativeLayrError::Refused(
            "Read access requires a reviewer".into(),
        ));
    }
    runtime::associated_cwd(state_dir, record)
}

#[cfg(any(not(target_os = "macos"), test))]
fn linux_reviewer_wrapper() -> &'static str {
    r#"scratch=$1; state=$3; shift 3
set -- --tmpfs "$state" --bind "$PWD" /workspace --bind "$scratch" /scratch --proc /proc --dev /dev --chdir /workspace --setenv TMPDIR /scratch --setenv HOME /scratch --setenv PATH "$PATH" --unsetenv LAYR_SOCKET --unsetenv LAYR_ROOT --unsetenv LAYR_PROJECT --unsetenv LAYR_LINE --unsetenv LAYR_SESSION --unsetenv LAYR_DIRECT --remount-ro / -- "$@"
for root in /usr /bin /sbin /lib /lib64 /opt; do
  if [ -e "$root" ]; then set -- --ro-bind "$root" "$root" "$@"; fi
done
for file in /etc/ld.so.cache /etc/ld.so.conf /etc/passwd /etc/group /etc/nsswitch.conf /etc/localtime; do
  if [ -f "$file" ]; then set -- --ro-bind "$file" "$file" "$@"; fi
done
for root in "${CARGO_HOME:-}" "${RUSTUP_HOME:-}"; do
  case "$root" in /*) ;; *) continue ;; esac
  case "$root/" in "$state/"*) continue ;; esac
  case "$state/" in "$root/"*) continue ;; esac
  if [ -d "$root" ]; then set -- --ro-bind "$root" "$root" "$@"; fi
done
exec bwrap --unshare-all --die-with-parent --new-session --tmpfs / "$@""#
}

#[cfg(test)]
mod reviewer_isolation_tests {
    #[test]
    fn native_layr_linux_try_does_not_expose_the_host_or_daemon_socket() {
        let wrapper = super::linux_reviewer_wrapper();
        assert!(!wrapper.contains("--ro-bind / /"));
        assert!(wrapper.contains("--tmpfs /"));
        assert!(wrapper.contains("--unsetenv LAYR_SOCKET"));
        assert!(wrapper.contains("--chdir /workspace"));
    }
}

pub(crate) use runtime::stage_turn_save;

#[cfg(test)]
mod turn_save_tests {
    use super::*;
    use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, RuntimeStore, StoreError};

    #[tokio::test]
    async fn native_terminal_stages_a_save_without_resetting_a_completed_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(directory.path())).await.unwrap();
        store.transaction_with(|tx| -> Result<(), StoreError> {
            let record = json!({"id":"native-save-agent","rootId":"native-save-agent","epoch":0,
                "status":"idle","autoWake":true,"workspaceMode":"layr-native","layrReady":true,"cwd":"/native/main"});
            AgentRepository::put(tx,&AgentRecord::from_value(record).map_err(StoreError::from)?)?;
            stage_turn_save(tx,"native-save-agent","turn-1")?;
            let id = AgentId::from("native-save-agent");
            let mut saved = AgentRepository::get(tx,&id)?.unwrap().value();
            assert_eq!(saved["layrTurnSaves"]["turn-1"]["status"],"pending");
            saved["layrTurnSaves"]["turn-1"] = json!({"status":"saved","stateId":"exact-state"});
            AgentRepository::put(tx,&AgentRecord::from_value(saved).map_err(StoreError::from)?)?;
            stage_turn_save(tx,"native-save-agent","turn-1")?;
            assert_eq!(AgentRepository::get(tx,&id)?.unwrap().value()["layrTurnSaves"]["turn-1"]["stateId"],"exact-state");
            Ok(())
        }).await.unwrap();
    }
}

#[cfg(test)]
mod review_regressions {
    use super::*;
    use std::{fs, path::PathBuf};

    #[cfg(unix)]
    fn fast_fixture() -> (tempfile::TempDir, Value) {
        use std::os::unix::fs::PermissionsExt;
        let state = tempfile::tempdir().unwrap();
        fs::create_dir_all(binary_path(state.path()).parent().unwrap()).unwrap();
        fs::write(binary_path(state.path()),r#"#!/bin/sh
printf '%s\n' "$*" >> "$LAYR_ROOT/commands"
case "$3 $4" in
  'rev-parse head@{0}')
    if [ -f "$LAYR_ROOT/automatic" ]; then cat "$LAYR_ROOT/automatic"; else printf "head@{0}: line has only 0 automatic states\n" >&2; exit 128; fi ;;
  'rev-parse head') printf 'full-named-state\n' ;;
esac
"#).unwrap();
        fs::set_permissions(binary_path(state.path()), fs::Permissions::from_mode(0o755)).unwrap();
        let project = studio_runtime_store::sha256_hex(b"/fast-source");
        let line = format!("agent-{}", studio_runtime_store::sha256_hex(b"fast-worker"));
        let cwd = main_path(state.path(), &project)
            .parent()
            .unwrap()
            .join(&line);
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(main_path(state.path(), &project)).unwrap();
        let worker = json!({"id":"fast-worker","workspaceMode":"layr-native","role":"implementer","isLead":false,"layrReady":true,"layrProjectId":project,"layrLine":line,"cwd":cwd});
        (state, worker)
    }

    #[cfg(unix)]
    #[test]
    fn native_layr_unchanged_save_replays_only_explicit_failures() {
        let (state, worker) = fast_fixture();
        let cwd = fs::canonicalize(worker["cwd"].as_str().unwrap()).unwrap();
        let intent = json!({"agent":worker["id"],"cwd":cwd,"kind":"save"});
        assert!(
            operation(state.path(), "failed-save", &intent, || Err(
                NativeLayrError::Refused("old missing automatic state".into())
            ))
            .is_err()
        );
        assert_eq!(
            save(state.path(), &worker, "failed-save").unwrap()["stateId"],
            "full-named-state"
        );
        assert!(
            operation(state.path(), "unknown-save", &intent, || Err(
                NativeLayrError::Unknown("lost".into())
            ))
            .is_err()
        );
        assert!(matches!(
            save(state.path(), &worker, "unknown-save"),
            Err(NativeLayrError::Unknown(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn native_layr_archive_sets_the_nonexpiring_pin_before_deleting_the_line() {
        let (state, worker) = fast_fixture();
        fs::write(
            store_path(state.path()).join("automatic"),
            "full-automatic-state\n",
        )
        .unwrap();
        let receipt = remove(state.path(), &worker, "archive-with-pin").unwrap();
        assert_eq!(receipt["stateId"], "full-automatic-state");
        assert!(
            receipt["archiveRef"]
                .as_str()
                .unwrap()
                .starts_with("pins/studio/archive/")
        );
        let commands = fs::read_to_string(store_path(state.path()).join("commands")).unwrap();
        let commands: Vec<_> = commands.lines().collect();
        let pin = commands
            .iter()
            .position(|line| line.contains(" ref set "))
            .unwrap();
        let deletion = commands
            .iter()
            .position(|line| line.contains(" line delete "))
            .unwrap();
        assert!(pin < deletion);
        assert!(!commands[pin].contains("--ttl"));
        assert!(commands[pin].ends_with("full-automatic-state"));
    }

    fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Value, Value) {
        let state = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("code.txt"), "base\n").unwrap();
        fs::create_dir_all(binary_path(state.path()).parent().unwrap()).unwrap();
        fs::copy(
            std::env::var_os("STUDIO_LAYR_NATIVE_TEST_BINARY")
                .expect("Set STUDIO_LAYR_NATIVE_TEST_BINARY"),
            binary_path(state.path()),
        )
        .unwrap();
        let mut lead =
            json!({"id":"regression-lead","isLead":true,"autoWake":true,"cwd":source.path()});
        configure_new_chat(&mut lead, source.path().to_str().unwrap());
        let fields = runtime::prepare_files(state.path(), &lead).unwrap();
        for (key, value) in fields.as_object().unwrap() {
            lead[key] = value.clone();
        }
        let mut worker =
            json!({"id":"regression-worker","role":"implementer","isLead":false,"autoWake":true});
        configure_child(&mut worker, &lead, &lead);
        let fields = runtime::prepare_files(state.path(), &worker).unwrap();
        for (key, value) in fields.as_object().unwrap() {
            worker[key] = value.clone();
        }
        (state, source, lead, worker)
    }

    #[test]
    #[ignore = "requires real Layr 0.2.0 on APFS or root on btrfs"]
    fn native_layr_unchanged_turn_save_and_previously_failed_save_can_finish() {
        let (state, _source, _lead, worker) = fixture();
        let expected = run(
            state.path(),
            Path::new(worker["cwd"].as_str().unwrap()),
            &["rev-parse", "head"],
        )
        .unwrap();
        assert_eq!(
            save(state.path(), &worker, "unchanged-turn").unwrap()["stateId"],
            expected
        );
        let cwd = fs::canonicalize(worker["cwd"].as_str().unwrap()).unwrap();
        let intent = json!({"agent":worker["id"],"cwd":cwd,"kind":"save"});
        assert!(
            operation(state.path(), "previously-failed-save", &intent, || Err(
                NativeLayrError::Refused("head@{0}: line has only 0 automatic states".into())
            ))
            .is_err()
        );
        assert_eq!(
            save(state.path(), &worker, "previously-failed-save").unwrap()["stateId"],
            expected
        );
    }

    #[test]
    #[ignore = "requires real Layr 0.2.0 on APFS or root on btrfs"]
    fn native_layr_archive_pins_an_unmerged_automatic_state_without_expiry() {
        let (state, _source, lead, worker) = fixture();
        fs::write(
            PathBuf::from(worker["cwd"].as_str().unwrap()).join("code.txt"),
            "unmerged work\n",
        )
        .unwrap();
        let saved = save(state.path(), &worker, "archive-turn-save").unwrap();
        let receipt = remove(state.path(), &worker, "archive-worker-with-retention").unwrap();
        let reference = receipt["archiveRef"]
            .as_str()
            .expect("archive must keep a durable ref");
        let main = Path::new(lead["cwd"].as_str().unwrap());
        let pins: Value = serde_json::from_str(
            &run(
                state.path(),
                main,
                &["ref", "list", "pins/studio/archive/", "--json"],
            )
            .unwrap(),
        )
        .unwrap();
        let pin = pins
            .as_array()
            .unwrap()
            .iter()
            .find(|pin| pin["name"] == reference)
            .unwrap();
        assert!(pin.get("expires_at").is_none_or(Value::is_null));
        assert_eq!(
            run(state.path(), main, &["rev-parse", reference]).unwrap(),
            saved["stateId"].as_str().unwrap()
        );
        assert!(
            run(
                state.path(),
                main,
                &["show", &format!("{reference}:code.txt")]
            )
            .unwrap()
            .contains("unmerged work")
        );
        assert!(!Path::new(worker["cwd"].as_str().unwrap()).exists());
    }
}
