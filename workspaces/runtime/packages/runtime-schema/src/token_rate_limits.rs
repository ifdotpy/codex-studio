//! Token-rate snapshot limits. This file is the owner; `openapi.json`
//! (`TokenRateSnapshot`, `TokenRateRecord`) is checked against it and the web
//! copy at the end of `api.ts` is rendered by `studio-contracts`.

/// Most agent records in one complete token-rate snapshot.
pub const TOKEN_RATE_MAX_ENTRIES: usize = 4096;
/// Longest agent, thread, or turn identifier in a snapshot, in Unicode code points.
pub const TOKEN_RATE_IDENTIFIER_MAX_CODEPOINTS: usize = 200;
