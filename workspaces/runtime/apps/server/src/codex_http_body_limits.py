"""Canonical HTTP API request-body limits shared with the Rust edge."""

from codex_federation_limits import FEDERATION_BODY_LIMIT, FEDERATION_PATHS

DEFAULT_BODY_LIMIT = 256 * 1024
ASSET_BODY_LIMIT = 28 * 1024 * 1024
VOICE_AUDIO_BODY_LIMIT = 6 * 1024 * 1024
PROJECTS_BODY_LIMIT = 6 * 1024 * 1024

ASSET_PATHS = frozenset({"/api/assets"})
VOICE_AUDIO_PATHS = frozenset({"/api/voice/audio"})
PROJECTS_PATHS = frozenset({"/api/projects"})

PATH_LIMITS = {
    **{path: FEDERATION_BODY_LIMIT for path in FEDERATION_PATHS},
    **{path: ASSET_BODY_LIMIT for path in ASSET_PATHS},
    **{path: VOICE_AUDIO_BODY_LIMIT for path in VOICE_AUDIO_PATHS},
    **{path: PROJECTS_BODY_LIMIT for path in PROJECTS_PATHS},
}


def request_body_limit(path: str) -> int:
    return PATH_LIMITS.get(path, DEFAULT_BODY_LIMIT)
