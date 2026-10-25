//! Host Layr workspaces. The Studio state directory owns the store and binary.
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use studio_runtime_store::path_identity::CanonicalPath;

pub use studio_runtime_store::NATIVE_LAYR_DIRECTORY;
/// Store directory below the native installation directory, outside project checkouts.
pub const STORE_DIRECTORY: &str = "store";
/// Host Layr release required by Studio's command contract.
pub const LAYR_VERSION: &str = "0.2.2";

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
mod git_guard;
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
mod inventory;
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
        let state = tempfile::tempdir().expect("fixture operation succeeds");
        let intent = json!({"line":"worker"});
        let first = operation(state.path(), "create-worker", &intent, || {
            Ok(json!({"stateId":"full-state"}))
        })
        .expect("fixture operation succeeds");
        let again = operation(state.path(), "create-worker", &intent, || {
            panic!("must not repeat")
        })
        .expect("fixture operation succeeds");
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
        let state = tempfile::tempdir().expect("fixture operation succeeds");
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
        let state = tempfile::tempdir().expect("fixture operation succeeds");
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
        assert!(
            result
                .expect_err("operation must fail")
                .to_string()
                .contains("line moved")
        );
    }

    #[test]
    fn project_identity_is_shared_by_migration_and_chats_with_a_logical_binding() {
        let mut record = json!({"hostProjectPath":"/project"});
        let before = project_id(&record).expect("fixture operation succeeds");
        record["projectId"] = json!("logical-project");
        assert_eq!(
            project_id(&record).expect("fixture operation succeeds"),
            before
        );
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
                .expect("fixture operation succeeds")
                .starts_with("/studio/layr-native/bin:")
        );
    }

    #[test]
    #[ignore = "requires the vendored Layr host binary and APFS or btrfs"]
    fn real_host_import_worker_merge_archive_and_warm_caches() {
        use std::{fs, path::PathBuf};
        let binary = PathBuf::from(
            std::env::var_os("STUDIO_LAYR_NATIVE_TEST_BINARY")
                .expect("Set STUDIO_LAYR_NATIVE_TEST_BINARY"),
        );
        let state = tempfile::tempdir().expect("fixture operation succeeds");
        let source = tempfile::tempdir().expect("fixture operation succeeds");
        fs::write(source.path().join("code.txt"), "base\n").expect("fixture operation succeeds");
        fs::write(source.path().join(".gitignore"), "node_modules/\n.env\n")
            .expect("fixture operation succeeds");
        fs::write(
            source.path().join(".layrattributes"),
            "node_modules regenerable\n",
        )
        .expect("fixture operation succeeds");
        fs::write(source.path().join(".env"), "TEST_VALUE=local\n")
            .expect("fixture operation succeeds");
        fs::create_dir(source.path().join("node_modules")).expect("fixture operation succeeds");
        fs::write(source.path().join("node_modules/old"), "warm import\n")
            .expect("fixture operation succeeds");
        let git = |args: &[&str]| {
            crate::bounded_child::run(crate::bounded_child::Request {
                program: std::ffi::OsStr::new("git"),
                git_directory: Some(source.path().as_os_str()),
                args,
                environment: &[],
                input: None,
                stdout_limit: 64 * 1024,
                stderr_limit: 64 * 1024,
                timeout: std::time::Duration::from_secs(60),
            })
            .expect("fixture Git command succeeds");
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
        fs::create_dir_all(
            binary_path(state.path())
                .parent()
                .expect("fixture operation succeeds"),
        )
        .expect("fixture operation succeeds");
        fs::copy(binary, binary_path(state.path())).expect("fixture operation succeeds");
        let mut lead = json!({"id":"lead","isLead":true,"autoWake":true,"cwd":source.path()});
        configure_new_chat(
            &mut lead,
            CanonicalPath::existing(source.path())
                .map(CanonicalPath::into_path_buf)
                .expect("fixture operation succeeds")
                .to_str()
                .expect("fixture operation succeeds"),
        );
        let ready =
            runtime::prepare_files(state.path(), &lead).expect("fixture operation succeeds");
        for (key, value) in ready.as_object().expect("fixture operation succeeds") {
            lead[key] = value.clone();
        }
        let main = PathBuf::from(lead["cwd"].as_str().expect("fixture operation succeeds"));
        assert!(main.join(".env").is_file());
        assert_eq!(
            fs::read_to_string(main.join("node_modules/old")).expect("imported cache"),
            "warm import\n"
        );
        assert!(
            run(state.path(), &main, &["cache", "list"])
                .expect("cache list")
                .lines()
                .any(|line| line == "node_modules\tcache")
        );
        let real_git: PathBuf = serde_json::from_slice(
            &fs::read(binary_path(state.path()).with_file_name("git-real.json"))
                .expect("saved real Git path"),
        )
        .expect("real Git path JSON");
        for program in [binary_path(state.path()).with_file_name("git"), real_git] {
            let result =
                crate::bounded_child::run_without_git_environment(crate::bounded_child::Request {
                    program: program.as_os_str(),
                    git_directory: Some(main.as_os_str()),
                    args: &[
                        "-c",
                        "user.name=Fixture",
                        "-c",
                        "user.email=fixture@example.test",
                        "commit",
                        "--allow-empty",
                        "-m",
                        "Must refuse",
                    ],
                    environment: &[],
                    input: None,
                    stdout_limit: 4096,
                    stderr_limit: 4096,
                    timeout: std::time::Duration::from_secs(30),
                });
            assert!(
                matches!(result,Err(crate::bounded_child::ChildError::Failed(ref reason)) if reason.contains("layr save -m")),
                "native Git commit guard: {result:?}"
            );
        }
        let mut worker = json!({"id":"worker","role":"implementer","autoWake":true,"isLead":false});
        configure_child(&mut worker, &lead, &lead);
        let ready =
            runtime::prepare_files(state.path(), &worker).expect("fixture operation succeeds");
        for (key, value) in ready.as_object().expect("fixture operation succeeds") {
            worker[key] = value.clone();
        }
        let cwd = PathBuf::from(worker["cwd"].as_str().expect("fixture operation succeeds"));
        assert_eq!(
            fs::read_to_string(cwd.join("node_modules/old")).expect("imported warm cache"),
            "warm import\n"
        );
        fs::write(cwd.join("code.txt"), "worker result\n").expect("fixture operation succeeds");
        run(state.path(), &cwd, &["add", "code.txt"]).expect("fixture operation succeeds");
        run(state.path(), &cwd, &["save", "-m", "Worker result"])
            .expect("fixture operation succeeds");
        let state_id =
            run(state.path(), &cwd, &["rev-parse", "head"]).expect("fixture operation succeeds");
        assert_eq!(
            evidence(state.path(), &worker, &state_id).expect("fixture operation succeeds")["stateId"],
            state_id
        );
        assert!(evidence(state.path(), &worker, &state_id[..8]).is_err());
        merge(state.path(), &lead, &worker, &state_id, "accept-worker")
            .expect("fixture operation succeeds");
        assert_eq!(
            fs::read_to_string(main.join("code.txt")).expect("fixture operation succeeds"),
            "worker result\n"
        );
        let mut reviewer =
            json!({"id":"reviewer","role":"reviewer","autoWake":true,"isLead":false});
        configure_child(&mut reviewer, &lead, &lead);
        let ready =
            runtime::prepare_files(state.path(), &reviewer).expect("fixture operation succeeds");
        for (key, value) in ready.as_object().expect("fixture operation succeeds") {
            reviewer[key] = value.clone();
        }
        assert_eq!(reviewer["cwd"], reviewer["layrScratch"]);
        assert_eq!(
            Path::new(
                reviewer["layrReadCwd"]
                    .as_str()
                    .expect("fixture operation succeeds")
            ),
            main
        );
        let args = vec!["try".into(),state_id.clone(),"--".into(),"/bin/sh".into(),"-c".into(),
            "printf local > review-check; printf note > \"$TMPDIR/note\"; if printf forbidden > \"$1/code.txt\"; then exit 7; fi".into(),
            "review-check".into(),main.to_string_lossy().into_owned()];
        reviewer_try(state.path(), &reviewer, &args).expect("fixture operation succeeds");
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
        .expect("fixture operation succeeds");
        assert!(
            !environment
                .lines()
                .any(|line| line.starts_with("LAYR_ROOT=")
                    || line.starts_with("LAYR_DIRECT=")
                    || line.starts_with("LAYR_SOCKET=")),
            "Reviewer check inherited the runtime store environment"
        );
        assert_eq!(
            fs::read_to_string(main.join("code.txt")).expect("fixture operation succeeds"),
            "worker result\n"
        );
        assert!(!main.join("review-check").exists());
        assert!(
            PathBuf::from(
                reviewer["layrScratch"]
                    .as_str()
                    .expect("fixture operation succeeds")
            )
            .join("note")
            .is_file()
        );
        save(state.path(), &worker, "worker-turn-end").expect("fixture operation succeeds");
        remove(state.path(), &worker, "archive-worker").expect("fixture operation succeeds");
        assert!(!cwd.exists());
        assert!(
            run(
                state.path(),
                &main,
                &["show", &format!("{state_id}:code.txt")]
            )
            .expect("fixture operation succeeds")
            .contains("worker result")
        );
        let worktree = tempfile::tempdir().expect("fixture operation succeeds");
        git(&[
            "worktree",
            "add",
            "-b",
            "migration-fixture",
            worktree
                .path()
                .to_str()
                .expect("fixture operation succeeds"),
        ]);
        fs::write(worktree.path().join("code.txt"), "migrated worktree\n")
            .expect("fixture operation succeeds");
        fs::write(worktree.path().join(".env"), "TEST_VALUE=migrated\n")
            .expect("fixture operation succeeds");
        let plan = migration::migrate(state.path(), source.path(), true)
            .expect("fixture operation succeeds");
        assert_eq!(
            plan["lines"]
                .as_array()
                .expect("fixture operation succeeds")
                .len(),
            1
        );
        let imported = migration::migrate(state.path(), source.path(), false)
            .expect("fixture operation succeeds");
        assert_eq!(imported["projectId"], lead["layrProjectId"]);
        let migrated = main.parent().expect("fixture operation succeeds").join(
            imported["lines"][0]["line"]
                .as_str()
                .expect("fixture operation succeeds"),
        );
        assert_eq!(
            fs::read_to_string(migrated.join("code.txt")).expect("fixture operation succeeds"),
            "migrated worktree\n"
        );
        assert_eq!(
            fs::read_to_string(migrated.join(".env")).expect("fixture operation succeeds"),
            "TEST_VALUE=migrated\n"
        );
        let again = migration::migrate(state.path(), source.path(), false)
            .expect("fixture operation succeeds");
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
    let state = CanonicalPath::existing(state_dir).map(CanonicalPath::into_path_buf)?;
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
        let directory = tempfile::tempdir().expect("fixture operation succeeds");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("fixture operation succeeds");
        store.transaction_with(|tx| -> Result<(), StoreError> {
            let record = json!({"id":"native-save-agent","rootId":"native-save-agent","epoch":0,
                "status":"idle","autoWake":true,"workspaceMode":"layr-native","layrReady":true,"cwd":"/native/main"});
            AgentRepository::put(tx,&AgentRecord::from_value(record).map_err(StoreError::from)?)?;
            stage_turn_save(tx,"native-save-agent","turn-1")?;
            let id = AgentId::from("native-save-agent");
            let mut saved = AgentRepository::get(tx,&id)?.expect("fixture operation succeeds").value();
            assert_eq!(saved["layrTurnSaves"]["turn-1"]["status"],"pending");
            saved["layrTurnSaves"]["turn-1"] = json!({"status":"saved","stateId":"exact-state"});
            AgentRepository::put(tx,&AgentRecord::from_value(saved).map_err(StoreError::from)?)?;
            stage_turn_save(tx,"native-save-agent","turn-1")?;
            assert_eq!(AgentRepository::get(tx,&id)?.expect("fixture operation succeeds").value()["layrTurnSaves"]["turn-1"]["stateId"],"exact-state");
            Ok(())
        }).await.expect("fixture operation succeeds");
    }
}

#[cfg(test)]
mod review_regressions {
    use super::*;
    use std::{fs, path::PathBuf};

    #[cfg(unix)]
    fn fast_fixture() -> (tempfile::TempDir, Value) {
        use std::os::unix::fs::PermissionsExt;
        let state = tempfile::tempdir().expect("fixture operation succeeds");
        fs::create_dir_all(
            binary_path(state.path())
                .parent()
                .expect("fixture operation succeeds"),
        )
        .expect("fixture operation succeeds");
        fs::write(binary_path(state.path()),r#"#!/bin/sh
printf '%s\n' "$*" >> "$LAYR_ROOT/commands"
case "$3 $4" in
  'rev-parse head@{0}')
    if [ -f "$LAYR_ROOT/automatic" ]; then cat "$LAYR_ROOT/automatic"; else printf "head@{0}: line has only 0 automatic states\n" >&2; exit 128; fi ;;
  'rev-parse head') printf 'full-named-state\n' ;;
esac
"#).expect("fixture operation succeeds");
        fs::set_permissions(binary_path(state.path()), fs::Permissions::from_mode(0o755))
            .expect("fixture operation succeeds");
        let project = studio_runtime_store::sha256_hex(b"/fast-source");
        let line = format!("agent-{}", studio_runtime_store::sha256_hex(b"fast-worker"));
        let cwd = main_path(state.path(), &project)
            .parent()
            .expect("fixture operation succeeds")
            .join(&line);
        fs::create_dir_all(&cwd).expect("fixture operation succeeds");
        fs::create_dir_all(main_path(state.path(), &project)).expect("fixture operation succeeds");
        let worker = json!({"id":"fast-worker","workspaceMode":"layr-native","role":"implementer","isLead":false,"layrReady":true,"layrProjectId":project,"layrLine":line,"cwd":cwd});
        (state, worker)
    }

    #[cfg(unix)]
    #[test]
    fn native_layr_unchanged_save_replays_only_explicit_failures() {
        let (state, worker) = fast_fixture();
        let cwd =
            CanonicalPath::existing(worker["cwd"].as_str().expect("fixture operation succeeds"))
                .map(CanonicalPath::into_path_buf)
                .expect("fixture operation succeeds");
        let intent = json!({"agent":worker["id"],"cwd":cwd,"kind":"save"});
        assert!(
            operation(state.path(), "failed-save", &intent, || Err(
                NativeLayrError::Refused("old missing automatic state".into())
            ))
            .is_err()
        );
        assert_eq!(
            save(state.path(), &worker, "failed-save").expect("fixture operation succeeds")["stateId"],
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
        .expect("fixture operation succeeds");
        let receipt =
            remove(state.path(), &worker, "archive-with-pin").expect("fixture operation succeeds");
        assert_eq!(receipt["stateId"], "full-automatic-state");
        assert!(
            receipt["archiveRef"]
                .as_str()
                .expect("fixture operation succeeds")
                .starts_with("pins/studio/archive/")
        );
        let commands = fs::read_to_string(store_path(state.path()).join("commands"))
            .expect("fixture operation succeeds");
        let commands: Vec<_> = commands.lines().collect();
        let pin = commands
            .iter()
            .position(|line| line.contains(" ref set "))
            .expect("fixture operation succeeds");
        let deletion = commands
            .iter()
            .position(|line| line.contains(" line delete "))
            .expect("fixture operation succeeds");
        assert!(pin < deletion);
        assert!(!commands[pin].contains("--ttl"));
        assert!(commands[pin].ends_with("full-automatic-state"));
    }

    fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Value, Value) {
        let state = tempfile::tempdir().expect("fixture operation succeeds");
        let source = tempfile::tempdir().expect("fixture operation succeeds");
        fs::write(source.path().join("code.txt"), "base\n").expect("fixture operation succeeds");
        fs::create_dir_all(
            binary_path(state.path())
                .parent()
                .expect("fixture operation succeeds"),
        )
        .expect("fixture operation succeeds");
        fs::copy(
            std::env::var_os("STUDIO_LAYR_NATIVE_TEST_BINARY")
                .expect("Set STUDIO_LAYR_NATIVE_TEST_BINARY"),
            binary_path(state.path()),
        )
        .expect("fixture operation succeeds");
        let mut lead =
            json!({"id":"regression-lead","isLead":true,"autoWake":true,"cwd":source.path()});
        configure_new_chat(
            &mut lead,
            source.path().to_str().expect("fixture operation succeeds"),
        );
        let fields =
            runtime::prepare_files(state.path(), &lead).expect("fixture operation succeeds");
        for (key, value) in fields.as_object().expect("fixture operation succeeds") {
            lead[key] = value.clone();
        }
        let mut worker =
            json!({"id":"regression-worker","role":"implementer","isLead":false,"autoWake":true});
        configure_child(&mut worker, &lead, &lead);
        let fields =
            runtime::prepare_files(state.path(), &worker).expect("fixture operation succeeds");
        for (key, value) in fields.as_object().expect("fixture operation succeeds") {
            worker[key] = value.clone();
        }
        (state, source, lead, worker)
    }

    #[test]
    #[ignore = "requires the vendored Layr on APFS or root on btrfs"]
    fn native_layr_unchanged_turn_save_and_previously_failed_save_can_finish() {
        let (state, _source, _lead, worker) = fixture();
        let expected = run(
            state.path(),
            Path::new(worker["cwd"].as_str().expect("fixture operation succeeds")),
            &["rev-parse", "head"],
        )
        .expect("fixture operation succeeds");
        assert_eq!(
            save(state.path(), &worker, "unchanged-turn").expect("fixture operation succeeds")["stateId"],
            expected
        );
        let cwd =
            CanonicalPath::existing(worker["cwd"].as_str().expect("fixture operation succeeds"))
                .map(CanonicalPath::into_path_buf)
                .expect("fixture operation succeeds");
        let intent = json!({"agent":worker["id"],"cwd":cwd,"kind":"save"});
        assert!(
            operation(state.path(), "previously-failed-save", &intent, || Err(
                NativeLayrError::Refused("head@{0}: line has only 0 automatic states".into())
            ))
            .is_err()
        );
        assert_eq!(
            save(state.path(), &worker, "previously-failed-save")
                .expect("fixture operation succeeds")["stateId"],
            expected
        );
    }

    #[test]
    #[ignore = "requires the vendored Layr on APFS or root on btrfs"]
    fn native_layr_archive_pins_an_unmerged_automatic_state_without_expiry() {
        let (state, _source, lead, worker) = fixture();
        fs::write(
            PathBuf::from(worker["cwd"].as_str().expect("fixture operation succeeds"))
                .join("code.txt"),
            "unmerged work\n",
        )
        .expect("fixture operation succeeds");
        let saved =
            save(state.path(), &worker, "archive-turn-save").expect("fixture operation succeeds");
        let receipt = remove(state.path(), &worker, "archive-worker-with-retention")
            .expect("fixture operation succeeds");
        let reference = receipt["archiveRef"]
            .as_str()
            .expect("archive must keep a durable ref");
        let main = Path::new(lead["cwd"].as_str().expect("fixture operation succeeds"));
        let pins: Value = serde_json::from_str(
            &run(
                state.path(),
                main,
                &["ref", "list", "pins/studio/archive/", "--json"],
            )
            .expect("fixture operation succeeds"),
        )
        .expect("fixture operation succeeds");
        let pin = pins
            .as_array()
            .expect("fixture operation succeeds")
            .iter()
            .find(|pin| pin["name"] == reference)
            .expect("fixture operation succeeds");
        assert!(pin.get("expires_at").is_none_or(Value::is_null));
        assert_eq!(
            run(state.path(), main, &["rev-parse", reference]).expect("fixture operation succeeds"),
            saved["stateId"]
                .as_str()
                .expect("fixture operation succeeds")
        );
        assert!(
            run(
                state.path(),
                main,
                &["show", &format!("{reference}:code.txt")]
            )
            .expect("fixture operation succeeds")
            .contains("unmerged work")
        );
        assert!(!Path::new(worker["cwd"].as_str().expect("fixture operation succeeds")).exists());
    }
}
