use serde_json::{Value, json};

/// Resolve permissions from the current rows inside the child commit transaction.
/// Only the permission mode transfers; other Claude settings belong to the child.
pub(crate) fn inherit_claude_permissions(
    child: &mut Value,
    parent: &Value,
    root: &Value,
    spec: &Value,
) -> Result<(), &'static str> {
    if child["provider"] != "claude" {
        if spec.get("permission_mode").is_some() {
            return Err("permission_mode requires a Claude child");
        }
        return Ok(());
    }
    let requested = spec
        .get("permission_mode")
        .map(|value| {
            value
                .as_str()
                .filter(|mode| crate::claude_controls::PERMISSION_MODES.contains(mode))
                .ok_or("Invalid Claude permission_mode")
        })
        .transpose()?;
    let inherited = if parent["provider"] == "claude" {
        match parent.pointer("/claudeOptions/permissionMode") {
            Some(value) => value
                .as_str()
                .filter(|mode| crate::claude_controls::PERMISSION_MODES.contains(mode))
                .ok_or("Invalid parent Claude permission mode")?,
            None if parent["yoloMode"] == true => "bypassPermissions",
            None => "default",
        }
    } else if root
        .get("yoloMode")
        .and_then(Value::as_bool)
        .unwrap_or(true)
    {
        // Preserve the existing Codex-to-Claude root yolo mapping.
        "bypassPermissions"
    } else {
        "default"
    };
    let mode = if child["role"] == "reviewer" {
        "plan"
    } else {
        match requested {
            Some("bypassPermissions") if inherited != "bypassPermissions" => inherited,
            Some(mode) => mode,
            None => inherited,
        }
    };
    child["claudeOptions"] = json!({"permissionMode": mode});
    child["yoloMode"] = json!(mode == "bypassPermissions");
    Ok(())
}
