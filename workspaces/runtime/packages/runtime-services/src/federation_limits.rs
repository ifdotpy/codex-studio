//! Federation protocol limits.

pub use studio_runtime_schema::federation_limits::{MAX_QUEUE_BYTES, MAX_QUEUE_ROWS};

/// Longest federation message, in Unicode code points (the agent message limit).
pub const MAX_MESSAGE_CODEPOINTS: usize = 12000;
/// Most messages in one federation batch.
pub const MAX_BATCH_ITEMS: usize = 50;
/// Largest federation batch, in bytes (200 KiB).
pub const MAX_BATCH_BYTES: usize = 204800;
/// Most members in one federation room.
pub const MAX_ROOM_MEMBERS: usize = 50;
/// Most inbound messages one federated room accepts from a peer per minute.
pub const MAX_ROOM_RATE: u64 = 30;
/// Seconds a federation invitation stays valid.
pub const INVITATION_TTL_SECONDS: u64 = 900;
/// Deadline of one Tailscale command, in seconds.
pub const TAILSCALE_COMMAND_TIMEOUT_SECONDS: u64 = 5;
/// Version of the federation protocol.
pub const PROTOCOL_VERSION: u64 = 1;
/// Largest federation HTTP request body, in bytes (256 KiB).
pub const FEDERATION_BODY_LIMIT: usize = 262144;
/// Federation HTTP endpoints, which take `FEDERATION_BODY_LIMIT`.
pub const FEDERATION_PATHS: [&str; 4] = [
    "/api/federation/v1/pair",
    "/api/federation/v1/status",
    "/api/federation/v1/message",
    "/api/federation/v1/pull",
];
