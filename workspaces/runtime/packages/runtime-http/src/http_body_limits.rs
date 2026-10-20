//! HTTP API request-body limits.

/// Largest request body of an endpoint without its own limit, in bytes (256 KiB).
pub(crate) const DEFAULT_BODY_LIMIT: usize = 262_144;
/// Largest `/api/assets` upload, in bytes (28 MiB).
pub(crate) const ASSET_BODY_LIMIT: usize = 29_360_128;
/// Largest `/api/voice/audio` upload, in bytes (6 MiB).
pub(crate) const VOICE_AUDIO_BODY_LIMIT: usize = 6_291_456;
/// Largest `/api/projects` request, in bytes (6 MiB).
pub(crate) const PROJECTS_BODY_LIMIT: usize = 6_291_456;
/// Deadline for reading one complete request body, in seconds.
pub(crate) const REQUEST_BODY_READ_TIMEOUT_SECONDS: u64 = 10;

pub(crate) fn request_limit(path: &str) -> usize {
    if studio_runtime_services::federation_limits::FEDERATION_PATHS.contains(&path) {
        return studio_runtime_services::federation_limits::FEDERATION_BODY_LIMIT;
    }
    match path {
        "/api/assets" => ASSET_BODY_LIMIT,
        "/api/voice/audio" => VOICE_AUDIO_BODY_LIMIT,
        "/api/projects" => PROJECTS_BODY_LIMIT,
        _ => DEFAULT_BODY_LIMIT,
    }
}
