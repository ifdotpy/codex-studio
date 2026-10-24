//! Cross-language runtime values. This file is the owner: the web, Swift, and
//! VM guest copies are rendered from it by `studio-contracts`
//! (`pnpm run contracts:generate`), and `pnpm run contracts:check` rejects drift.

// Command monitor output matching and bounded observation. Byte limits count UTF-8.
/// Patterns one output monitor may watch.
pub const MONITOR_OUTPUT_MAX_PATTERNS: usize = 16;
/// Largest monitor pattern, in UTF-8 bytes.
pub const MONITOR_OUTPUT_MAX_PATTERN_BYTES: usize = 256;
/// Largest match count a monitor may wait for (`i32::MAX`).
pub const MONITOR_OUTPUT_MAX_MATCH_COUNT: usize = 2147483647;
/// Seconds between two wakes of one output monitor.
pub const MONITOR_OUTPUT_WAKE_COOLDOWN_SECONDS: f64 = 1.0;
/// Event ids one monitor retains for duplicate suppression.
pub const MONITOR_OUTPUT_RETAINED_EVENT_IDS: usize = 32;
/// Output tail a monitor result keeps, in Unicode code points.
pub const MONITOR_OUTPUT_TAIL_CODEPOINTS: usize = 12000;
/// Longest blocking monitor wait, in milliseconds.
pub const MONITOR_WAIT_MAX_MILLISECONDS: usize = 10000;
/// Concurrent blocking monitor waits.
pub const MONITOR_WAIT_SLOTS: usize = 64;

/// Most error ids accepted in one atomic Studio error dismissal request. Unit: ids.
pub const STUDIO_ERRORS_DISMISS_MAX_IDS: usize = 200;

// Native terminal discovery before a stop.
/// Terminal entries per discovery page.
pub const NATIVE_TERMINAL_ITEMS_PER_PAGE: usize = 100;
/// Discovery pages per stop operation.
pub const NATIVE_TERMINAL_MAX_PAGES: usize = 100;

/// Largest integer a JavaScript number holds exactly (2^53 - 1).
pub use studio_client_domain::MAX_SAFE_INTEGER;
/// Largest user attachment, in bytes.
pub const MAX_USER_ATTACHMENT_BYTES: usize = 20971520;
/// Maximum attachment items admitted across one provider turn's input batch.
pub const MAX_TURN_ATTACHMENTS: usize = 8;
// Task result evidence: keep complete manifests for broad changes (the reported
// 73-file submission exceeded the former cap) without adding commit lookup semantics.
/// Most file paths in one task result submission, in paths.
pub const TASK_SUBMISSION_MAX_FILES: usize = 500;
/// Longest submitted file path, in Unicode code points before trimming.
pub const TASK_SUBMISSION_PATH_MAX_CODEPOINTS: usize = 4096;
/// Largest managed user message, in Unicode code points (independent of UTF-8 bytes).
pub const MAX_MANAGED_MESSAGE_CODEPOINTS: usize = 32000;
/// Display label of `MAX_USER_ATTACHMENT_BYTES`.
pub const USER_ATTACHMENT_LIMIT_LABEL: &str = "20 MiB";
/// Largest project secret value, in UTF-8 bytes.
pub const MAX_PROJECT_SECRET_VALUE_BYTES: usize = 16384;
/// Display label of `MAX_PROJECT_SECRET_VALUE_BYTES`.
pub const PROJECT_SECRET_VALUE_LIMIT_LABEL: &str = "16 KiB";
/// Deadline of one project-secrets guest RPC, in seconds.
pub const PROJECT_SECRETS_GUEST_TIMEOUT_SECONDS: u64 = 30;
/// Deadline of one `skills/extraRoots/set` call that registers the Chrome skill, in seconds.
pub const BROWSER_SKILL_TIMEOUT_SECONDS: u64 = 20;
/// Text that tells a lead, once, that Studio no longer uses its old
/// `PROGRESS.md` file. The `orchestration_context` topic `panel` returns it,
/// and the turn context delivers it to a lead that received the old file
/// guidance.
pub const PROGRESS_FILE_RETIRED_NOTICE: &str = "Studio no longer reads or shows PROGRESS.md. Do not create or update a progress file for Studio. Report status in the chat.";
/// Largest inline workspace preview, in bytes.
pub const MAX_WORKSPACE_PREVIEW_BYTES: u64 = 20971520;
/// Display label of `MAX_WORKSPACE_PREVIEW_BYTES`.
pub const WORKSPACE_PREVIEW_LIMIT_LABEL: &str = "20 MiB";
/// Deleted transcript items remembered per agent.
pub const TRANSCRIPT_MAX_TOMBSTONES: usize = 512;
/// Largest sync resource query, in bytes.
pub const MAX_SYNC_RESOURCE_QUERY_BYTES: usize = 65536;
/// Resources one sync request may name.
pub const MAX_SYNC_RESOURCE_COUNT: usize = 256;
/// Changes per sync batch.
pub const SYNC_BATCH_LIMIT: i64 = 100;
/// Entities per sync entity page.
pub const SYNC_ENTITY_PAGE_LIMIT: i64 = 500;
/// Maximum records in a complete state stream interval, in documents.
pub const SYNC_ENTITY_STREAM_MAX_DOCUMENTS: i64 = 512;
/// Maximum complete resources SSE event, including event/id/data lines, in UTF-8 bytes.
pub const SYNC_STREAM_EVENT_MAX_BYTES: usize = 262144;
/// Maximum changed and removed transcript records in a complete stream interval, in items.
pub const SYNC_TRANSCRIPT_STREAM_MAX_ITEMS: usize = 512;
/// Maximum voice records in a complete transient stream interval, in records.
pub const SYNC_VOICE_STREAM_MAX_RECORDS: i64 = 128;
/// Maximum records in a complete draft stream interval, in documents.
pub const SYNC_DRAFT_STREAM_MAX_DOCUMENTS: i64 = 512;
/// Seconds between sync resource heartbeats.
pub const SYNC_RESOURCE_HEARTBEAT_SECONDS: u64 = 15;

// Codex model/list catalog response contract; byte values count encoded UTF-8.
/// Largest model catalog frame, in bytes.
pub const MODEL_CATALOG_MAX_FRAME_BYTES: usize = 2097152;
/// Largest complete model catalog response, in bytes.
pub const MODEL_CATALOG_MAX_RESPONSE_BYTES: usize = 16777216;
/// Model catalog pages per read.
pub const MODEL_CATALOG_MAX_PAGES: usize = 100;
/// Models per catalog page.
pub const MODEL_CATALOG_ITEMS_PER_PAGE: usize = 100;
/// Deadline of one model catalog read, in seconds.
pub const MODEL_CATALOG_OPERATION_TIMEOUT_SECONDS: u64 = 30;
/// Longest wait for a model catalog read slot, in seconds.
pub const MODEL_CATALOG_ADMISSION_WAIT_SECONDS: u64 = 5;
/// Seconds a model catalog stays cached.
pub const MODEL_CATALOG_CACHE_TTL_SECONDS: u64 = 300;

// Codex account limits are refreshed on separate window and complete-read ages.
/// Seconds an account limit window stays fresh.
pub const ACCOUNT_LIMITS_WINDOW_FRESH_SECONDS: f64 = 60.0;
/// Seconds a complete account limits read stays fresh.
pub const ACCOUNT_LIMITS_FULL_READ_FRESH_SECONDS: f64 = 600.0;
/// Seconds before an account limits read that failed is retried.
pub const ACCOUNT_LIMITS_ERROR_RETRY_SECONDS: f64 = 30.0;

// A lead's pending child result waits at most this many seconds for sibling subagents that
// are still queued or running, so results that finish close together reach the lead in one
// turn. The orchestration setting chooses a value in [MIN, MAX]; 0 holds nothing.
/// Default child result coalescing wait, in seconds.
pub const CHILD_RESULT_COALESCE_DEFAULT_SECONDS: u64 = 10;
/// Smallest child result coalescing wait, in seconds.
pub const CHILD_RESULT_COALESCE_MIN_SECONDS: u64 = 0;
/// Largest child result coalescing wait, in seconds.
pub const CHILD_RESULT_COALESCE_MAX_SECONDS: u64 = 300;

// An idle provider thread (a Codex thread or a Claude session) is unloaded after this many
// minutes without a running turn, pending input, or active monitor, and an account's
// provider process stops once it holds no loaded thread. The next message resumes the
// thread. The orchestration setting chooses a value in [MIN, MAX]; 0 never unloads.
/// Default idle session unload delay, in minutes.
pub const IDLE_SESSION_UNLOAD_DEFAULT_MINUTES: u64 = 15;
/// Smallest idle session unload delay, in minutes.
pub const IDLE_SESSION_UNLOAD_MIN_MINUTES: u64 = 0;
/// Largest idle session unload delay, in minutes.
pub const IDLE_SESSION_UNLOAD_MAX_MINUTES: u64 = 1440;

// Turn start throttle per account, the same mechanism for every provider. Providers limit
// requests per account, so Studio starts at most BURST turns at once, waits PAUSE seconds
// after the start that ends a burst (BURST starts made, or the last free RUNNING slot
// taken), and keeps at most RUNNING turns running. A turn over the limit waits and starts
// later; it never fails. Studio counts turn starts, not the provider's own requests inside a
// turn. Each account setting chooses a value in [MIN, MAX]; a new account takes its
// provider's defaults. Units: turn starts, seconds, and running turns.
//
// Codex (measured 2026-10-19, one account): a burst of about 10 simultaneous requests is
// admitted, more get HTTP 429 with a 58-60 s Retry-After; the allowance refills at 17 or
// more requests per minute; 17 running turns showed no concurrency limit. Owner decision
// 2026-10-20: the 512-worker live run saw only 23 provider 429 events at 48 concurrent
// turns over 3 accounts, so the defaults are 16 per burst, a 30 s pause, and 32 running
// turns.
//
// Claude (measured 2026-10-19): no throttle was observed up to about 19 sequential starts
// per minute, a burst of 12, and 15 running turns. RUNNING mainly bounds host memory: each
// running Claude CLI turn holds about 250-350 MiB.
/// Default Codex turn starts per burst.
pub const ACCOUNT_START_BURST_DEFAULT_CODEX: u64 = 16;
/// Default Claude turn starts per burst.
pub const ACCOUNT_START_BURST_DEFAULT_CLAUDE: u64 = 32;
/// Fewest turn starts per burst.
pub const ACCOUNT_START_BURST_MIN: u64 = 1;
/// Most turn starts per burst.
pub const ACCOUNT_START_BURST_MAX: u64 = 512;
/// Default Codex pause after a burst, in seconds.
pub const ACCOUNT_START_PAUSE_DEFAULT_SECONDS_CODEX: u64 = 30;
/// Default Claude pause after a burst, in seconds.
pub const ACCOUNT_START_PAUSE_DEFAULT_SECONDS_CLAUDE: u64 = 5;
/// Shortest pause after a burst, in seconds.
pub const ACCOUNT_START_PAUSE_MIN_SECONDS: u64 = 0;
/// Longest pause after a burst, in seconds.
pub const ACCOUNT_START_PAUSE_MAX_SECONDS: u64 = 3600;
/// Default Codex running turns per account.
pub const ACCOUNT_RUNNING_TURNS_DEFAULT_CODEX: u64 = 32;
/// Default Claude running turns per account.
pub const ACCOUNT_RUNNING_TURNS_DEFAULT_CLAUDE: u64 = 64;
/// Fewest running turns per account.
pub const ACCOUNT_RUNNING_TURNS_MIN: u64 = 1;
/// Most running turns per account.
pub const ACCOUNT_RUNNING_TURNS_MAX: u64 = 512;

// Account groups and the account order of a project. An account can carry one group name
// (for example "personal"). A project's account order is a list of group names; the
// server default order applies to a project without its own order. Studio prefers the
// accounts of the first group, then the next group, and so on; accounts of other groups
// and accounts without a group come last. The order is a preference: every account stays
// selectable, and Studio uses the next account when a preferred one is not available.
// The decision cases are in `workspaces/client/contracts/account-preference.json`.
/// A valid account group name: lowercase letters, digits, "-" and "_", 1 to 32 characters.
pub const ACCOUNT_GROUP_PATTERN: &str = "^[a-z0-9][a-z0-9_-]{0,31}$";
/// Most group names in one account order.
pub const ACCOUNT_GROUPS_MAX: usize = 16;
/// Model of a new chat on a Claude account when the request names no model.
pub const DEFAULT_CLAUDE_LEAD_MODEL: &str = "claude-opus-5-5";
/// Reasoning effort of a new Claude chat when the request names no model and no effort.
pub const DEFAULT_CLAUDE_LEAD_EFFORT: &str = "medium";

// Persisted agent errors for a turn whose provider connection ended. Connection recovery,
// the scheduler, and the web "Check connection" action match these exact texts.
/// Turn error after the provider connection ended.
pub const PROVIDER_DISCONNECTED_TURN_ERROR: &str =
    "Codex disconnected. Review the transcript before resuming.";
/// Turn error after the backend restarted during the turn.
pub const BACKEND_RESTARTED_TURN_ERROR: &str =
    "Server restarted during a turn. Review history, then send a new instruction.";
/// Turn error after the Linux VM provider connection ended.
pub const VM_PROVIDER_DISCONNECTED_TURN_ERROR: &str = "Linux VM provider disconnected. The turn outcome is unknown. Review the transcript before resuming.";
// Persisted input and agent errors that Studio, not the agent's work, causes. The
// scheduler writes them, and the Studio error list classifies them.
/// Error of input whose turn/start answer was lost: the provider may have started it.
pub const PROVIDER_START_UNKNOWN_ERROR: &str = "turn/start response timed out; outcome unknown";
/// Error of input that the provider proved it never received.
pub const PROVIDER_START_NOT_SENT_ERROR: &str = "Provider did not send turn/start";
/// Hold reason of input whose thread resume answer was lost.
pub const PROVIDER_RESUME_UNKNOWN_ERROR: &str =
    "Provider thread resume outcome is unknown; input remains pending.";
/// Refusal of a monitor whose owner stopped its turn: the owner ended it, not Studio.
pub const MONITOR_OWNER_STOPPED_ERROR: &str = "The agent turn was stopped";
/// Seconds that the host retries one idempotent Linux VM provider journal read (next,
/// replay, eventRead, info, status, ack) after a result that the guest could not prove. A
/// guest supervisor stall shorter than this does not end the provider connection. Writes
/// are never retried.
pub const GUEST_PROVIDER_READ_RETRY_SECONDS: u64 = 30;

// Free space of the volume that holds the state directory, in bytes available to Studio
// (statvfs f_bavail * f_frsize). The desktop warns below WARNING and CRITICAL. Below
// DISK_FULL_ENTER the disk is full: Studio starts no new turn, and an agent whose turn
// failed for lack of space waits. The full state ends at or above DISK_FULL_EXIT; Studio
// then resumes held agents. The gap between the two values keeps the state from flapping.
/// Free bytes below which the desktop warns (15 GiB).
pub const DISK_SPACE_WARNING_FREE_BYTES: u64 = 16106127360;
/// Free bytes below which the desktop shows a critical warning (5 GiB).
pub const DISK_SPACE_CRITICAL_FREE_BYTES: u64 = 5368709120;
/// Free bytes below which the disk counts as full (1 GiB).
pub const DISK_FULL_ENTER_FREE_BYTES: u64 = 1073741824;
/// Free bytes at or above which the full state ends (2 GiB).
pub const DISK_FULL_EXIT_FREE_BYTES: u64 = 2147483648;
/// Seconds between free-space checks while the disk is full. An observed "disk full" write
/// error also keeps the full state for at least this time, even when the sample shows free
/// space.
pub const DISK_FULL_RECHECK_SECONDS: u64 = 15;
/// Automatic continuations after consecutive turns that failed because the disk was full.
pub const DISK_FULL_MAX_RESUMES: u64 = 5;
/// Case-insensitive regular expression for provider or local error text that means the
/// disk is full: POSIX ENOSPC in Node, Python, and Rust messages, and SQLite SQLITE_FULL.
pub const DISK_FULL_ERROR_PATTERN: &str =
    "ENOSPC|No space left on device|database or disk is full|SQLITE_FULL";

// Image workspace mount identity (docs/workspace-images.md, "Mount identity"). Studio
// writes the marker file at the root of each agent volume when it attaches the volume. The
// marker names the agent and the (st_dev, st_ino) of the image file at attach time.
/// File name of the image workspace marker.
pub const IMAGE_WORKSPACE_MARKER_NAME: &str = ".studio-workspace.json";
/// Schema version of the image workspace marker.
pub const IMAGE_WORKSPACE_MARKER_SCHEMA: u64 = 1;
/// The closed results of one mount point check.
pub const IMAGE_MOUNT_STATUSES: &[&str] = &[
    "mounted",
    "detached",
    "plain-folder",
    "foreign-volume",
    "image-replaced",
    "attached-elsewhere",
    "image-missing",
];
/// The mount results that need an operator; they are never repaired automatically.
pub const IMAGE_MOUNT_PROBLEM_STATUSES: &[&str] = &[
    "plain-folder",
    "foreign-volume",
    "image-replaced",
    "attached-elsewhere",
    "image-missing",
];

// In-place VM guest updates (docs/linux-vm-workspaces.md, "Guest updates"). Provisioning
// writes the Mac's update key (root only) and the record of the parts that only
// provisioning installs; the root broker reads both. Paths in the guest.
/// Guest path of the Mac's update key.
pub const GUEST_UPDATE_KEY_PATH: &str = "/etc/codex-studio/update-key";
/// Guest path of the provisioning payload record.
pub const GUEST_PAYLOAD_RECORD_PATH: &str = "/var/lib/codex-studio/guest-payload.json";
/// Largest raw file part in one `update.stage` request, in bytes (base64 must fit the
/// 2 MiB guest frame).
pub const GUEST_UPDATE_PART_BYTES: usize = 1048576;
/// The virtiofs tag of the Rosetta for Linux directory share (docs/linux-vm-workspaces.md,
/// "x86_64 binaries"). The VM helper attaches the share with this tag; the guest mounts it.
pub const LINUX_VM_ROSETTA_TAG: &str = "rosetta";
/// Prefix of the guest native provider handle of one agent: the handle is this prefix, then
/// the agent ID (docs/linux-vm-workspaces.md). The host builds the handle; the guest checks
/// that a handle names its own agent.
pub const LINUX_VM_PROVIDER_HANDLE_PREFIX: &str = "linux-worker:";
/// The longest time that a sign-in request stays open while its command still listens for
/// the sign-in redirect, in seconds from the request's creation (docs/vm-browser-bridge.md,
/// "Card lifetime"). The host bridge (`runtime-services/src/browser_host.rs`) and the VM
/// guest bridge (`vm-guest/browser_bridge.py`) move the deadline of such a request forward
/// up to this bound, so the person can still answer the card after the 600 s request time.
/// It must stay below the 3600 s for which both bridges keep a closed request
/// (`KEEP_SECONDS`), so the server still reads the final state of a request at this bound.
pub const SIGN_IN_OPEN_LIMIT_SECONDS: u64 = 1800;
/// The error code of a sign-in bridge step that sent nothing to the sign-in command because
/// the listening ports were unknown (a failed or slow probe). The request stays open and a
/// retry is safe. The host bridge (`browser_host::NOT_SENT_CODE`), the VM guest bridge and
/// the server's answers (`browser_answer`) use it.
pub const SIGN_IN_NOT_SENT_CODE: &str = "not_sent";
/// The state of a sign-in request that waits on the open request of another chat with the
/// same Linux user, sign-in host and client ID (its `joinedTo`), instead of a card of its own
/// (docs/vm-browser-bridge.md, "One card for the same sign-in"). The host bridge
/// (`browser_host::JOINED`), the VM guest bridge and the card poll (`browser_cards`) use it.
pub const SIGN_IN_JOINED_STATE: &str = "joined";
/// The `closeReason` of a joined sign-in request whose sign-in finished on the card that it
/// waited on. The card poll tells the joined chat to run its command again.
pub const SIGN_IN_JOINED_SIGNED_IN_REASON: &str = "joined-signed-in";
/// A pending sign-in card alerts the owner only after it stayed open this long, in seconds:
/// on a Mac host the browser can finish the sign-in at once without the person. The desktop
/// alerts (`web/src/desktop/desktopAlerts.ts`) and the phone pushes (`runtime-services`
/// `web_push`) use it.
pub const SIGN_IN_ALERT_GRACE_SECONDS: u64 = 10;
/// The one reminder of an open sign-in card comes this long before the card closes, in
/// seconds. The desktop alerts and the phone pushes use it.
pub const SIGN_IN_REMINDER_SECONDS: u64 = 300;

// Claude Code 2.1.302 idle compaction policy. Counts are context tokens.
pub const IDLE_COMPACTION_DEFAULT_MIN_TOKENS: u64 = 200000;
pub const IDLE_COMPACTION_MIN_TOKENS: u64 = 100000;
/// Fraction of the observed cache lifetime before the idle timer fires.
pub const IDLE_COMPACTION_FIRE_FRACTION: f64 = 0.9;
pub const IDLE_COMPACTION_MIN_FIRE_FRACTION: f64 = 0.5;
pub const IDLE_COMPACTION_MAX_FIRE_FRACTION: f64 = 0.95;
/// Maximum permitted timer lateness, in seconds.
pub const IDLE_COMPACTION_MAX_LATE_SECONDS: f64 = 60.0;
/// One-hour prompt cache lifetime, in seconds.
pub const IDLE_COMPACTION_CACHE_SECONDS: f64 = 3600.0;

#[cfg(test)]
mod tests {
    use super::*;

    fn byte_label(bytes: u64) -> String {
        const KIB: u64 = 1024;
        const MIB: u64 = KIB * KIB;
        if bytes.is_multiple_of(MIB) {
            format!("{} MiB", bytes / MIB)
        } else if bytes.is_multiple_of(KIB) {
            format!("{} KiB", bytes / KIB)
        } else {
            format!("{bytes} bytes")
        }
    }

    #[test]
    fn limit_labels_follow_their_byte_values() {
        for (bytes, label) in [
            (
                MAX_USER_ATTACHMENT_BYTES as u64,
                USER_ATTACHMENT_LIMIT_LABEL,
            ),
            (
                MAX_PROJECT_SECRET_VALUE_BYTES as u64,
                PROJECT_SECRET_VALUE_LIMIT_LABEL,
            ),
            (MAX_WORKSPACE_PREVIEW_BYTES, WORKSPACE_PREVIEW_LIMIT_LABEL),
        ] {
            assert_eq!(byte_label(bytes), label);
        }
    }

    #[test]
    fn problem_statuses_are_mount_statuses() {
        assert!(
            IMAGE_MOUNT_PROBLEM_STATUSES
                .iter()
                .all(|status| IMAGE_MOUNT_STATUSES.contains(status))
        );
    }
}
