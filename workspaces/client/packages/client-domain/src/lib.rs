//! Platform-independent client rules shared by native and WebAssembly hosts.
pub mod drafts;
pub mod markdown;
pub mod protocol;

/// Largest integer a JavaScript number holds exactly (2^53 - 1).
pub const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
