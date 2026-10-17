"""Authoritative cross-language values for generated consumers."""

from typing import Final

PROCESS_SUPERVISOR_PROTOCOL_VERSION = 1
OPERATIONS_PYTHON_PROTOCOL_VERSION: Final = 4

MAX_USER_ATTACHMENT_BYTES = 20 * 1024 * 1024
# Managed user messages count Unicode code points, independent of UTF-8 bytes.
MAX_MANAGED_MESSAGE_CODEPOINTS = 32_000
MAX_PROJECT_SECRET_VALUE_BYTES = 16 * 1024
# Bounded deadline for a single project-secrets guest RPC.
PROJECT_SECRETS_GUEST_TIMEOUT_SECONDS = 30
MAX_PROGRESS_FILE_BYTES = 128 * 1024
MAX_WORKSPACE_PREVIEW_BYTES = 20 * 1024 * 1024
TRANSCRIPT_MAX_TOMBSTONES = 512
MAX_SYNC_RESOURCE_QUERY_BYTES = 65_536
MAX_SYNC_RESOURCE_COUNT = 256
SYNC_BATCH_LIMIT = 100
SYNC_ENTITY_PAGE_LIMIT = 500
SYNC_STREAM_BYTE_LIMIT = 1_048_576
SYNC_RESOURCE_HEARTBEAT_SECONDS = 15
# Native history pages and command results can exceed the former 1 MiB cap.
# This bounds one encoded transport envelope, including its JSON escaping.
MAX_SUPERVISOR_FRAME_BYTES = 32 * 1024 * 1024
PROGRESS_LAYOUT_FRESH_SECONDS = 90
PROGRESS_LAYOUT_MAX_CLIENTS = 32
PROGRESS_LAYOUT_MAX_BYTES = 65_536

# In-place VM guest updates (docs/linux-vm-workspaces.md, "Guest updates"). Provisioning writes
# the Mac's update key (root only) and the record of the parts that only provisioning installs;
# the root broker reads both. Paths in the guest.
GUEST_UPDATE_KEY_PATH = "/etc/codex-studio/update-key"
GUEST_PAYLOAD_RECORD_PATH = "/var/lib/codex-studio/guest-payload.json"
# The largest raw file part in one update.stage request (base64 must fit the 2 MiB guest frame).
GUEST_UPDATE_PART_BYTES = 1024 * 1024

# Codex model/list catalog response contract; byte values count encoded UTF-8.
MODEL_CATALOG_MAX_FRAME_BYTES = 2 * 1024 * 1024
MODEL_CATALOG_MAX_RESPONSE_BYTES = 16 * 1024 * 1024
MODEL_CATALOG_MAX_PAGES = 100
MODEL_CATALOG_ITEMS_PER_PAGE = 100
MODEL_CATALOG_OPERATION_TIMEOUT_SECONDS = 30
MODEL_CATALOG_ADMISSION_WAIT_SECONDS = 5
MODEL_CATALOG_CACHE_TTL_SECONDS = 300

# Codex account limits are refreshed on separate window and complete-read ages.
ACCOUNT_LIMITS_WINDOW_FRESH_SECONDS = 60
ACCOUNT_LIMITS_FULL_READ_FRESH_SECONDS = 600
ACCOUNT_LIMITS_ERROR_RETRY_SECONDS = 30


def user_attachment_limit_label() -> str:
    """Return the display label derived from the byte-valued attachment cap."""
    mib, remainder = divmod(MAX_USER_ATTACHMENT_BYTES, 1024 * 1024)
    if remainder:
        return f"{MAX_USER_ATTACHMENT_BYTES} bytes"
    return f"{mib} MiB"


def project_secret_value_limit_label() -> str:
    """Return the display label derived from the UTF-8 byte-valued secret cap."""
    kib, remainder = divmod(MAX_PROJECT_SECRET_VALUE_BYTES, 1024)
    if remainder:
        return f"{MAX_PROJECT_SECRET_VALUE_BYTES} bytes"
    return f"{kib} KiB"


def progress_file_limit_label() -> str:
    """Return the display label derived from the progress-file byte cap."""
    kib, remainder = divmod(MAX_PROGRESS_FILE_BYTES, 1024)
    if remainder:
        return f"{MAX_PROGRESS_FILE_BYTES} bytes"
    return f"{kib} KiB"


def workspace_preview_limit_label() -> str:
    """Return the display label derived from the inline preview byte cap."""
    mib, remainder = divmod(MAX_WORKSPACE_PREVIEW_BYTES, 1024 * 1024)
    if remainder:
        return f"{MAX_WORKSPACE_PREVIEW_BYTES} bytes"
    return f"{mib} MiB"

# Command monitor output matching and bounded observation. Byte limits count UTF-8.
MONITOR_OUTPUT_MAX_PATTERNS = 16
MONITOR_OUTPUT_MAX_PATTERN_BYTES = 256
MONITOR_OUTPUT_MAX_MATCH_COUNT = 2**31 - 1
MONITOR_OUTPUT_WAKE_COOLDOWN_SECONDS = 1.0
MONITOR_OUTPUT_RETAINED_EVENT_IDS = 32
MONITOR_OUTPUT_TAIL_CODEPOINTS = 12_000
MONITOR_WAIT_MAX_MILLISECONDS = 10_000
MONITOR_WAIT_SLOTS = 64

# Native terminal discovery before a stop: entries per page and pages per operation.
NATIVE_TERMINAL_ITEMS_PER_PAGE = 100
NATIVE_TERMINAL_MAX_PAGES = 100
