//! Agent capacity limits. This file is the owner; the web copies in
//! `apiSchema.ts` are rendered by `studio-contracts`, and the OpenAPI and
//! persisted-state schemas are checked against it.

/// Fewest concurrent subagents per lead chat (0 is single agent mode).
pub const MIN_SUBAGENT_CONCURRENCY: i64 = 0;
/// Default concurrent subagents per lead chat.
pub const DEFAULT_SUBAGENT_CONCURRENCY: i64 = 32;
/// Most concurrent subagents per lead chat.
pub const MAX_SUBAGENT_CONCURRENCY: i64 = 2048;
/// Fewest agent records per team.
pub const MIN_TEAM_AGENTS: i64 = 1;
/// Default agent records per team.
pub const DEFAULT_MAX_TEAM_AGENTS: i64 = 64;
/// Records a team keeps for its lead.
pub const LEAD_TEAM_RECORDS: i64 = 1;
/// Records a team keeps for one queued worker above its concurrency.
pub const QUEUED_WORKER_HEADROOM: i64 = 1;
/// Team records beyond the subagent concurrency: the lead plus one queued worker.
pub const TEAM_AGENT_HEADROOM: i64 = LEAD_TEAM_RECORDS + QUEUED_WORKER_HEADROOM;
/// Most agent records per team.
pub const MAX_TEAM_AGENTS: usize = (MAX_SUBAGENT_CONCURRENCY + TEAM_AGENT_HEADROOM) as usize;
/// Fewest concurrent provider processes in the whole runtime.
pub const MIN_GLOBAL_CONCURRENCY: i64 = 1;
/// Default concurrent provider processes in the whole runtime: one full team and its lead.
pub const DEFAULT_GLOBAL_CONCURRENCY: i64 = MAX_SUBAGENT_CONCURRENCY + LEAD_TEAM_RECORDS;
/// Most agent records in one account transfer: one full team.
pub const MAX_TRANSFER_AGENT_RECORDS: usize = MAX_TEAM_AGENTS;
