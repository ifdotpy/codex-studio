//! Settings and timing contract for native background shells and managed monitors.
use serde::{Deserialize, Serialize};

/// Shortest enabled quiet episode, in milliseconds (Claude's silent-heron floor).
pub const QUIET_MIN_MS: u64 = 10_000;
/// Session inactivity required before a pressure cleanup, in milliseconds.
pub const PRESSURE_IDLE_MS: u64 = 30 * 60 * 1000;
/// Avoid repeated cleanup of a shell started soon after the previous cleanup, in milliseconds.
pub const PRESSURE_RESTART_GUARD_MS: u64 = 30 * 60 * 1000;
/// Maximum time between native shell policy observations, in milliseconds.
pub const TICK_MS: u64 = 2_500;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    pub quiet_threshold_ms: u64,
    pub pressure_cleanup: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            quiet_threshold_ms: 0,
            pressure_cleanup: true,
        }
    }
}
impl Settings {
    pub fn valid(&self) -> bool {
        self.quiet_threshold_ms == 0
            || (QUIET_MIN_MS..=crate::shared_values::MAX_SAFE_INTEGER as u64)
                .contains(&self.quiet_threshold_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_and_quiet_floor_match_claude() {
        assert_eq!(
            Settings::default(),
            Settings {
                quiet_threshold_ms: 0,
                pressure_cleanup: true
            }
        );
        for (value, valid) in [
            (0, true),
            (1, false),
            (9999, false),
            (10000, true),
            (u64::MAX, false),
        ] {
            assert_eq!(
                Settings {
                    quiet_threshold_ms: value,
                    pressure_cleanup: true
                }
                .valid(),
                valid
            );
        }
        assert!(
            serde_json::from_value::<Settings>(
                serde_json::json!({"quietThresholdMs": 10000,"pressureCleanup":true,"extra":false})
            )
            .is_err()
        );
    }
}
