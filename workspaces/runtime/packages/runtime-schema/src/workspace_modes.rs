//! Workspace mode names accepted for new chats and managed workers.
use serde_json::{Value, json};

/// Workspace choices for a new chat, in selector order.
pub const CHAT_WORKSPACE_MODES: &[&str] = &["layr-native", "layr", "worktree"];
/// Additional workspace choice for a worker that uses its parent's folder.
pub const SHARED_WORKSPACE_MODE: &str = "shared";
/// Default workspace mode for a new chat on macOS and Linux.
pub const DEFAULT_CHAT_WORKSPACE_MODE: &str = "layr-native";

pub fn worker_modes() -> Vec<&'static str> {
    CHAT_WORKSPACE_MODES
        .iter()
        .copied()
        .chain([SHARED_WORKSPACE_MODE])
        .collect()
}

pub fn accepts_chat(mode: &str) -> bool {
    CHAT_WORKSPACE_MODES.contains(&mode)
}

pub fn accepts_worker(mode: &str) -> bool {
    accepts_chat(mode) || mode == SHARED_WORKSPACE_MODE
}

pub fn update_openapi(document: &mut Value) {
    for (name, worker) in [("CreateAgentRequest", true), ("CreateLeadRequest", false)] {
        let modes = if worker {
            worker_modes()
        } else {
            CHAT_WORKSPACE_MODES.to_vec()
        };
        document["components"]["schemas"][name]["properties"]["workspaceMode"] = json!({
            "default": DEFAULT_CHAT_WORKSPACE_MODE, "enum": modes,
            "title": "Workspacemode", "type": "string"
        });
    }
}

pub fn check_openapi(document: &Value) -> Result<(), String> {
    let mut expected = document.clone();
    update_openapi(&mut expected);
    for name in ["CreateAgentRequest", "CreateLeadRequest"] {
        if document["components"]["schemas"][name]["properties"]["workspaceMode"]
            != expected["components"]["schemas"][name]["properties"]["workspaceMode"]
        {
            return Err(format!(
                "{name}.workspaceMode must match runtime-schema/src/workspace_modes.rs"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_tool_catalog_matches_workspace_mode_owner() {
        // The legacy catalog preserves old native prompt contracts and is not a new-chat consumer.
        let catalog: Value = serde_json::from_str(include_str!(
            "../../runtime-services/src/account_worker/preparation-resources.json"
        ))
        .expect("current tool catalog");
        let spawn = catalog["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .find(|tool| tool["name"] == "orchestration_spawn")
            .expect("spawn tool");
        assert_eq!(
            spawn.pointer("/inputSchema/properties/agents/items/properties/workspace/enum"),
            Some(&json!(worker_modes())),
            "runtime-schema/src/workspace_modes.rs owns the current workspace modes"
        );
    }

    #[test]
    fn new_workspaces_reject_removed_mode_and_accept_native_layr() {
        assert!(accepts_chat("layr-native"));
        assert!(!accepts_chat("image"));
        assert!(!accepts_chat("shared"));
        assert!(accepts_worker("shared"));
        let mut document = json!({});
        update_openapi(&mut document);
        check_openapi(&document).expect("generated mode contract");
        document["components"]["schemas"]["CreateLeadRequest"]["properties"]["workspaceMode"]["enum"] =
            json!(["image"]);
        assert!(check_openapi(&document).is_err());
    }
}
