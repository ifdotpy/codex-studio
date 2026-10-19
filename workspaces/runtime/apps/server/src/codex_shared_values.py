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
# Bounded deadline for one skills/extraRoots/set call that registers the Chrome skill.
BROWSER_SKILL_TIMEOUT_SECONDS = 20
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
# Open-receipt field that names one journal handle instance: the supervisor journal's
# random identity plus the handle row's creation time. Event sequences and saved
# applied-event cursors are meaningful only inside one journal epoch.
SUPERVISOR_JOURNAL_EPOCH_FIELD = "journalEpoch"
# supervisor_state key of the random identity created with each supervisor journal.
SUPERVISOR_JOURNAL_ID_KEY = "journalId"
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
# The virtiofs tag of the Rosetta for Linux directory share (docs/linux-vm-workspaces.md,
# "x86_64 binaries"). The VM helper attaches the share with this tag; the guest mounts it.
LINUX_VM_ROSETTA_TAG = "rosetta"

# Persisted agent errors for a turn whose provider connection ended. Connection recovery,
# the scheduler, and the web "Check connection" action match these exact texts.
PROVIDER_DISCONNECTED_TURN_ERROR = "Codex disconnected. Review the transcript before resuming."
BACKEND_RESTARTED_TURN_ERROR = "Server restarted during a turn. Review history, then send a new instruction."
VM_PROVIDER_DISCONNECTED_TURN_ERROR = ("Linux VM provider disconnected. The turn outcome is unknown. "
                                       "Review the transcript before resuming.")
# Seconds that the host retries one idempotent Linux VM provider journal read (next, replay,
# eventRead, info, status, ack) after a result that the guest could not prove. A guest supervisor
# stall shorter than this does not end the provider connection. Writes are never retried.
GUEST_PROVIDER_READ_RETRY_SECONDS = 30

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

# A lead's pending child result waits at most this many seconds for sibling subagents
# that are still queued or running, so results that finish close together reach the lead
# in one turn. The orchestration setting chooses a value in [MIN, MAX]; 0 holds nothing.
CHILD_RESULT_COALESCE_DEFAULT_SECONDS = 10
CHILD_RESULT_COALESCE_MIN_SECONDS = 0
CHILD_RESULT_COALESCE_MAX_SECONDS = 300

# An idle provider thread (a Codex thread or a Claude session) is unloaded after this many
# minutes without a running turn, pending input, or active monitor, and an account's provider
# process stops once it holds no loaded thread. The next message resumes the thread. The
# orchestration setting chooses a value in [MIN, MAX]; 0 never unloads.
IDLE_SESSION_UNLOAD_DEFAULT_MINUTES = 15
IDLE_SESSION_UNLOAD_MIN_MINUTES = 0
IDLE_SESSION_UNLOAD_MAX_MINUTES = 1440

# Turn start throttle per account, the same mechanism for every provider. Providers limit
# requests per account, so Studio starts at most BURST turns at once, waits PAUSE
# seconds after the start that ends a burst (BURST starts made, or the last free
# RUNNING slot taken), and keeps at most RUNNING turns running. A turn over the
# limit waits and starts later; it never fails. Studio counts turn starts, not the
# provider's own requests inside a turn. Each account setting chooses a value in
# [MIN, MAX]; a new account takes its provider's defaults. Units: turn starts,
# seconds, and running turns.
ACCOUNT_START_BURST_MIN = 1
ACCOUNT_START_BURST_MAX = 512
ACCOUNT_START_PAUSE_MIN_SECONDS = 0
ACCOUNT_START_PAUSE_MAX_SECONDS = 3600
ACCOUNT_RUNNING_TURNS_MIN = 1
ACCOUNT_RUNNING_TURNS_MAX = 512
# Codex (measured 2026-10-19, one account): a burst of about 10 simultaneous requests
# is admitted, more get HTTP 429 with a 58-60 s Retry-After; the allowance refills at
# 17 or more requests per minute; 17 running turns showed no concurrency limit.
# 8 per burst with a 30 s pause is about 16 starts per minute.
ACCOUNT_START_BURST_DEFAULT_CODEX = 8
ACCOUNT_START_PAUSE_DEFAULT_SECONDS_CODEX = 30
ACCOUNT_RUNNING_TURNS_DEFAULT_CODEX = 16
# Claude (measured 2026-10-19): no throttle was observed up to about 19 sequential
# starts per minute, a burst of 12, and 15 running turns. RUNNING mainly bounds host
# memory: each running Claude CLI turn holds about 250-350 MiB.
ACCOUNT_START_BURST_DEFAULT_CLAUDE = 32
ACCOUNT_START_PAUSE_DEFAULT_SECONDS_CLAUDE = 5
ACCOUNT_RUNNING_TURNS_DEFAULT_CLAUDE = 64


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

# Free space of the volume that holds the state directory, in bytes available to Studio
# (shutil.disk_usage().free; statvfs f_bavail * f_frsize). The desktop warns below WARNING and
# CRITICAL. Below DISK_FULL_ENTER the disk is full: Studio starts no new turn, and an agent whose
# turn failed for lack of space waits. The full state ends at or above DISK_FULL_EXIT; Studio then
# resumes held agents. The gap between the two values keeps the state from flapping.
DISK_SPACE_WARNING_FREE_BYTES = 15 * 1024**3
DISK_SPACE_CRITICAL_FREE_BYTES = 5 * 1024**3
DISK_FULL_ENTER_FREE_BYTES = 1024**3
DISK_FULL_EXIT_FREE_BYTES = 2 * 1024**3
# Seconds between free-space checks while the disk is full. An observed "disk full" write error
# also keeps the full state for at least this time, even when the sample shows free space.
DISK_FULL_RECHECK_SECONDS = 15
# Automatic continuations after consecutive turns that failed because the disk was full.
DISK_FULL_MAX_RESUMES = 5
# Case-insensitive regular expression for provider or local error text that means the disk is
# full: POSIX ENOSPC in Node, Python, and Rust messages, and SQLite SQLITE_FULL.
DISK_FULL_ERROR_PATTERN = r"ENOSPC|No space left on device|database or disk is full|SQLITE_FULL"

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

# Image workspace mount identity (docs/workspace-images.md, "Mount identity"). Studio writes the
# marker file at the root of each agent volume when it attaches the volume. The marker names the
# agent and the (st_dev, st_ino) of the image file at attach time. MOUNT_STATUSES are the closed
# results of one mount point check; the PROBLEM subset needs an operator and is never repaired
# automatically.
IMAGE_WORKSPACE_MARKER_NAME = ".studio-workspace.json"
IMAGE_WORKSPACE_MARKER_SCHEMA = 1
IMAGE_MOUNT_STATUSES = ("mounted", "detached", "plain-folder", "foreign-volume",
                        "image-replaced", "attached-elsewhere", "image-missing")
IMAGE_MOUNT_PROBLEM_STATUSES = ("plain-folder", "foreign-volume", "image-replaced",
                                "attached-elsewhere", "image-missing")
