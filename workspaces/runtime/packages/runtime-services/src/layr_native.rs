//! Host Layr workspaces. The Studio state directory owns the store and binary.
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Directory below the Studio state root for all native Layr installation data.
pub const NATIVE_LAYR_DIRECTORY: &str = "layr-native";
/// Store directory below the native installation directory, outside project checkouts.
pub const STORE_DIRECTORY: &str = "store";
/// Host Layr release required by Studio's command contract.
pub const LAYR_VERSION: &str = "0.2.0";

pub fn store_path(state_dir: &Path) -> PathBuf {
    state_dir.join(NATIVE_LAYR_DIRECTORY).join(STORE_DIRECTORY)
}

pub fn binary_path(state_dir: &Path) -> PathBuf {
    state_dir.join(NATIVE_LAYR_DIRECTORY).join("bin/layr")
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

pub mod install;
mod capability;
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
    serde_json::Map::from_iter([
        ("LAYR_ROOT".into(), json!(store_path(state_root))),
        ("LAYR_DIRECT".into(), json!("1")),
        ("PATH".into(), json!(path)),
    ])
}
pub mod migration;

#[cfg(test)]
mod receipt_tests {
    use super::*;

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
        assert_eq!(env["LAYR_DIRECT"], "1");
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
        configure_new_chat(&mut lead, source.path().to_str().unwrap());
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
        println!("native host evidence store: {}", state.path().display());
    }
}
