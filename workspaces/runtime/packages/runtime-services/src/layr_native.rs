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
    let source = root["hostProjectPath"].as_str()
        .or_else(|| root["cwd"].as_str()).unwrap_or_default();
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
        assert_eq!(store_path(Path::new("/state")), Path::new("/state/layr-native/store"));
        assert_eq!(binary_path(Path::new("/state")), Path::new("/state/layr-native/bin/layr"));
    }

    #[test]
    fn native_child_inherits_project_and_keeps_reviewer_role() {
        let root = json!({"cwd":"/store/main", "hostProjectPath":"/code", "layrProjectId":"project"});
        let mut child = json!({"id":"worker", "role":"reviewer"});
        configure_child(&mut child, &root, &root);
        assert_eq!(child["role"], "reviewer");
        assert_eq!(child["hostProjectPath"], "/code");
        assert_eq!(child["layrProjectId"], "project");
        assert_eq!(child["worktree"], false);
    }
}
