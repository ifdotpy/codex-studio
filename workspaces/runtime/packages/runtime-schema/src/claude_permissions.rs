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

/// A permission mode supported by Codex; Claude-only modes cannot be constructed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct CodexPermissionMode(PermissionMode);

impl CodexPermissionMode {
    pub const fn as_mode(self) -> PermissionMode {
        self.0
    }
}

impl TryFrom<PermissionMode> for CodexPermissionMode {
    type Error = PermissionMode;

    fn try_from(mode: PermissionMode) -> Result<Self, Self::Error> {
        match mode {
            PermissionMode::Default | PermissionMode::BypassPermissions => Ok(Self(mode)),
            _ => Err(mode),
        }
    }
}

impl From<CodexPermissionMode> for PermissionMode {
    fn from(mode: CodexPermissionMode) -> Self {
        mode.0
    }
}

impl<'de> Deserialize<'de> for CodexPermissionMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mode = PermissionMode::deserialize(deserializer)?;
        Self::try_from(mode)
            .map_err(|_| serde::de::Error::custom("unsupported Codex permission mode"))
    }
}

pub const PERMISSION_MODES: &[&str] = &[
    PermissionMode::Default.as_str(),
    PermissionMode::AcceptEdits.as_str(),
    PermissionMode::Auto.as_str(),
    PermissionMode::Plan.as_str(),
    PermissionMode::BypassPermissions.as_str(),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_permission_modes_reject_claude_only_modes() {
        for mode in ["default", "bypassPermissions"] {
            let parsed =
                serde_json::from_str::<CodexPermissionMode>(&format!("\"{mode}\"")).unwrap();
            assert_eq!(parsed.as_mode().as_str(), mode);
            assert_eq!(
                serde_json::to_string(&parsed).unwrap(),
                format!("\"{mode}\"")
            );
        }
        assert!(CodexPermissionMode::try_from(PermissionMode::Plan).is_err());
        for mode in ["acceptEdits", "auto", "plan", "unknown"] {
            assert!(serde_json::from_str::<CodexPermissionMode>(&format!("\"{mode}\"")).is_err());
        }
    }
}
