//! Workspace mode names accepted for new chats and managed workers.
use serde_json::{Value, json};

/// Workspace choices for a new chat, in selector order.
pub const CHAT_WORKSPACE_MODES: &[&str] = &["layr-native", "layr", "worktree"];
/// Additional workspace choice for a worker that uses its parent's folder.
pub const SHARED_WORKSPACE_MODE: &str = "shared";
/// Default workspace mode for a new chat on macOS and Linux.
pub const DEFAULT_CHAT_WORKSPACE_MODE: &str = "layr-native";

pub fn accepts_chat(mode: &str) -> bool {
    CHAT_WORKSPACE_MODES.contains(&mode)
}

pub fn accepts_worker(mode: &str) -> bool {
    accepts_chat(mode) || mode == SHARED_WORKSPACE_MODE
}

pub fn update_openapi(document: &mut Value) {
    for (name, worker) in [("CreateAgentRequest", true), ("CreateLeadRequest", false)] {
        let mut modes = CHAT_WORKSPACE_MODES.to_vec();
        if worker {
            modes.push(SHARED_WORKSPACE_MODE);
        }
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
