//! Byte limits of signed multi-server HTTP exchanges.

/// Largest signed multi-server request body, in bytes (256 KiB).
pub const MAX_REQUEST_BYTES: usize = 262144;
/// Largest signed multi-server response body, in bytes (1 MiB).
pub const MAX_RESPONSE_BYTES: usize = 1048576;
