"""Canonical limits for federation messages, batches, and HTTP bodies."""

from codex_agent_message_limits import MAX_AGENT_MESSAGE_CODEPOINTS

MAX_MESSAGE_CODEPOINTS = MAX_AGENT_MESSAGE_CODEPOINTS
MAX_BATCH_ITEMS = 50
MAX_BATCH_BYTES = 200 * 1024
MAX_ROOM_MEMBERS = 50
MAX_QUEUE_ROWS = 10_000
MAX_QUEUE_BYTES = 32 * 1024 * 1024
MAX_ROOM_RATE = 30
PROTOCOL_VERSION = 1
FEDERATION_BODY_LIMIT = 256 * 1024
FEDERATION_PATHS = (
    "/api/federation/v1/pair",
    "/api/federation/v1/status",
    "/api/federation/v1/message",
    "/api/federation/v1/pull",
)
