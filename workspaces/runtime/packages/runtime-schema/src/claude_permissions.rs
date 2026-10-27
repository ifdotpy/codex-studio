use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Claude session permission modes accepted by settings and child creation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    Auto,
    Plan,
    BypassPermissions,
}

impl PermissionMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Auto => "auto",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
        }
    }
}

pub const PERMISSION_MODES: &[&str] = &[
    PermissionMode::Default.as_str(),
    PermissionMode::AcceptEdits.as_str(),
    PermissionMode::Auto.as_str(),
    PermissionMode::Plan.as_str(),
    PermissionMode::BypassPermissions.as_str(),
];
