//! Token-rate snapshot limits. This file is the owner; `openapi.json`
//! (`TokenRateSnapshot`, `TokenRateValue`) is checked against it and the web
//! copy at the end of `api.ts` is rendered by `studio-contracts`.

/// Most entries in one token-rate map (workspace rates, teams, or one team's rates).
pub const TOKEN_RATE_MAX_ENTRIES: usize = 4096;
/// Most leaf rate values in one snapshot: workspace rates and team rates are two views of
/// the same agents.
pub const TOKEN_RATE_MAX_TOTAL_ENTRIES: usize = TOKEN_RATE_MAX_ENTRIES * 2;
/// Longest agent, thread, or turn identifier in a snapshot, in Unicode code points.
pub const TOKEN_RATE_IDENTIFIER_MAX_CODEPOINTS: usize = 200;
