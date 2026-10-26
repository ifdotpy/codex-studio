//! Platform-independent client rules shared by native and WebAssembly hosts.
pub mod drafts;
pub mod markdown;
pub mod protocol;

/// Largest integer a JavaScript number holds exactly (2^53 - 1).
pub const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
/// Largest native SSE event of any type, in bytes of UTF-8 text.
/// The native bridge counts it per side: the Rust parser counts buffered data and the
/// pending line; the web bridge counts the framed event it re-emits (`id:`, `event:`,
/// `data:` lines and terminators). Not the `resources` limit SYNC_STREAM_EVENT_MAX_BYTES,
/// because token-rates events are exempt from that limit and can exceed it.
pub const NATIVE_SSE_EVENT_MAX_BYTES: usize = 4 * 1024 * 1024;
