//! Federation queue limits that the store enforces. The other federation
//! limits live in `studio_runtime_services::federation_limits`.

/// Most queued outbox messages per federation peer.
pub const MAX_QUEUE_ROWS: u64 = 10000;
/// Most encoded bytes of queued outbox messages per federation peer (32 MiB).
pub const MAX_QUEUE_BYTES: u64 = 33554432;
