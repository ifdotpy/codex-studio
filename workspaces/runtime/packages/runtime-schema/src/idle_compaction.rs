//! Studio settings for Claude Code's idle compaction policy.
use crate::shared_values::{IDLE_COMPACTION_DEFAULT_MIN_TOKENS, IDLE_COMPACTION_MIN_TOKENS};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Off,
    Log,
    Compact,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    pub mode: Mode,
    pub min_tokens: u64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Mode::Off,
            min_tokens: IDLE_COMPACTION_DEFAULT_MIN_TOKENS,
        }
    }
}
impl Settings {
    pub fn valid(&self) -> bool {
        (IDLE_COMPACTION_MIN_TOKENS..=crate::shared_values::MAX_SAFE_INTEGER as u64)
            .contains(&self.min_tokens)
    }
    /// Environment override follows the same lower bound as the settings control.
    pub fn with_environment(self) -> Self {
        self.with_min_tokens_override(
            std::env::var("CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS")
                .ok()
                .as_deref(),
        )
    }
    fn with_min_tokens_override(mut self, value: Option<&str>) -> Self {
        if let Some(value) = value
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite())
        {
            self.min_tokens = value.max(IDLE_COMPACTION_MIN_TOKENS as f64).ceil() as u64;
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_minimum_and_environment_override_match_the_claude_policy() {
        assert_eq!(Settings::default().mode, Mode::Off);
        assert_eq!(
            Settings::default().min_tokens,
            IDLE_COMPACTION_DEFAULT_MIN_TOKENS
        );
        for (value, expected) in [
            ("99999", 100000),
            ("250000.5", 250001),
            ("NaN", 200000),
            ("inf", 200000),
            ("invalid", 200000),
        ] {
            assert_eq!(
                Settings::default()
                    .with_min_tokens_override(Some(value))
                    .min_tokens,
                expected
            );
        }
        let mut settings = Settings {
            min_tokens: IDLE_COMPACTION_MIN_TOKENS - 1,
            ..Settings::default()
        };
        assert!(!settings.valid());
        settings.min_tokens = IDLE_COMPACTION_MIN_TOKENS;
        assert!(settings.valid());
        assert!(serde_json::from_str::<Mode>("\"compact_on_return\"").is_err());
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CacheReport {
    pub request_id: String,
    pub ttl: String,
    pub warm: bool,
    /// Provider request activity time, in Unix milliseconds.
    pub last_activity_at: f64,
    pub context_tokens: u64,
    pub prefix: String,
    #[serde(default = "enabled")]
    pub compaction_enabled: bool,
    #[serde(default = "enabled")]
    pub idle_compaction_enabled: bool,
}
fn enabled() -> bool {
    true
}
