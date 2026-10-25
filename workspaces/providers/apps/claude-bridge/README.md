# Claude provider bridge

## Change Contract

This app owns the Node process that connects Studio's provider callers to the
Claude Agent SDK. Its public boundary is the bridge process protocol used by
the server, desktop package, and Linux guest. Keep SDK session state and
credentials within their owning process; do not let the bridge access Studio's
SQLite database or renderer state. Protocol behavior and dependency versions
are owned by this app's source and [`package.json`](package.json). Focused check:
`pnpm run test:server:bridge` from the repository root, as defined by the
[root manifest](../../../../package.json).

## Image input

The [image module](images.mjs) owns the default Claude image limits: 2000 pixels
per side, 5,242,880 base64 bytes, and the derived 3,932,160 raw bytes. The source
is Claude Code 2.1.302, `chunk-sp5b4hyj.js:475` and `chunk-hqtnbrjd.js:124661`.
Its separate 512,000-byte budget is best effort (`chunk-sp5b4hyj.js:453`).

The bridge uses sharp on macOS and Linux. Small PNG, JPEG, GIF, and WebP images
keep their bytes. A resize keeps PNG and WebP formats and selects the first
animation frame. A resized GIF becomes PNG. PNG compression tries a palette
before JPEG. JPEG compression tries qualities 80, 60, 40, and 20, then a final
resize to at most 1000 pixels wide at quality 20. The byte budget adds at most
one quality-90 conversion and five binary-search passes. These steps follow
`chunk-ft699e6v.js:460`, `:539`, `:578`, and `:650`.

The bridge verifies the final dimensions, format, raw bytes, and base64 bytes.
It refuses an unreadable image, an unsupported format, an unavailable codec,
or a result above the limits. The error reaches the send caller before an SDK
query or turn exists. The existing message delivery error displays the refusal
and keeps the draft. The source file never changes.

The bridge manifest pins sharp and its required native modules for darwin-arm64,
linux-arm64, and linux-x64. These are regular dependencies because the desktop
resource step uses `pnpm deploy --no-optional`. The Tauri package uses that same
resource step, then removes the Linux codec pairs from its Mac bridge directory.
It keeps the source manifests and lockfile for guest and standalone Linux
installs. The ARM64 guest launches its own
`/opt/codex-studio/claude_bridge/bridge.mjs`. Linux x64 modules support standalone
x64 servers.

Run `node testing/image-codec-witness.mjs` inside a deployed production bridge
to verify the codecs without a model request. Run
`node --test workspaces/client/apps/native/bridge-image-package.test.mjs` from
the repository root to verify the Mac prune and its native codec load.

## Steer completion and Stop

A successful result waits briefly for pending steer echoes. An echo during the
grace keeps the continuation in the same turn. When the grace expires, the turn
completes: unread input is withdrawn and reported for server redelivery with its
original identity; input already handed to the SDK remains submitted with an
unknown outcome and is never automatically resent. Before completing a turn
with such input, the bridge closes the CLI and reconciles its final native
history. A split input is delivered only if every part is recorded; no recorded
parts proves it unapplied, while partial or unavailable evidence stays unknown.
Malformed, torn, or changed native history and a bounded tail without the
opening input anchor, unique record UUIDs, or ancestry to the session root
remain uncertain. Malformed queued-command UUIDs also stay unknown.
Main-session attachments remain valid links in the native parent chain, and
complete large records use the [native reader](native-history.mjs) tail budget.
The bridge shows an uncertainty notice
and never authorizes redelivery from that evidence.
The next start resumes a fresh CLI, and retries reuse recorded user-message
identities even for auto-steered input. Query-closing controls refuse unsettled
unread input. [The lifecycle limits](stall-watch.mjs) own the echo grace and
interrupt/exit deadlines in milliseconds.

Stop finalizes an already deferred result as interrupted. If running Claude does
not end its turn by the interrupt deadline, the existing stall recovery closes
the CLI and reports a visible error. Only native history that proves input was
unread permits redelivery after that close; unavailable history stays uncertain.

Finalization has one shared outcome for SDK, Stop, and timer callers. An
unexpected final-save failure emits one failed completion and closes the query;
retries observe that failure without repeating finalization or usage publication.
The existing disk-full deferral remains available. Shutdown stops admitting new
finalizers through that shared path and cancels turn timers before closing the SDK.
It also disposes the tool watchdog; recovery already waiting on a save or deadline
stops without further notices, interrupts or terminal effects.

Rollback and fork boundaries use every native part of a composite input in
order, beginning at its earliest part. Forked history remaps every retained input
UUID. File checkpoints keep their own final-message identity and are not carried
across an unverified fork. Every retained input mapping is checked before a
native fork is submitted. Legacy delivered inputs without supported provenance
require recovery instead of creating an unvalidated native branch.

## Restart recovery

Session load checks stale `inProgress` turns against the native transcript.
A confirmed final assistant reply becomes `completed` with the final text.
A confirmed incomplete prompt or tool call becomes `interrupted`.
Unreadable, changed, unrelated, or unsupported native evidence becomes
`uncertain`. Automatic recovery must not continue an uncertain turn.
Recovery also accepts an initial input receipt or marks a steer delivered only
when all its native UUIDs appear in order on the proven transcript chain. Partial
or unproven inputs keep their ambiguous receipts, including after another restart.
The reader shares the bounded file and identity checks used for historical
Bash receipts. The mirror journal still discards a torn final record.

A malformed complete journal row requires native transcript evidence before
repair. Automatic repair supports complete text histories with exact message
identities, content, and parent links. Valid patch sequences must remain
contiguous. A corrupt complete final row, tool history, compaction, or uncertain
control state requires manual repair. The bridge returns a clear error and
writes a `.repair.json` marker. A confirmed repair preserves the original
journal in `.jsonl.corrupt` and returns `journalRepair.skippedRows` with the
thread history.

## Account identity

`STUDIO_CLAUDE_ACCOUNT` contains the email. Rust also passes
`STUDIO_CLAUDE_CREDENTIAL_IDENTITY`, which includes the organization UUID and
account UUID when available. The format owner is
`runtime-services/src/account_driver/claude_identity.rs`. Both languages test
`claude_identity_cases.json` for parity.

The bridge reads UUIDs from SDK metadata or the native `.claude.json` account
record. The SDK's `organization` field is a display name, not an identity.
`account/read` reports the UUIDs and `_credentialIdentity`.

An email-only pin can gain UUIDs once. The bridge saves this evidence in
`<account-server-root>/claude-account-identity` before it submits input or
returns usage. A restart preserves the pin. A changed or missing UUID then
blocks the request. The Rust registry uses the same one-way upgrade rule.
Codex identity and the existing Claude `accountId` remain unchanged.

## Token usage

The bridge emits `thread/tokenUsage/updated` observations using the
[generated usage contract](usage-contract.generated.d.mts). The
[SDK-typed normalizer](usage.mjs) finalizes response usage from stream events
at `message_stop`, publishes cumulative model totals immediately on each SDK
result, and reports unfinished responses as gaps. Assistant message usage is
provisional and serves only the idle-compaction cache report.

`pnpm --filter studio-claude-bridge run typecheck` checks the normalizer and its
SDK fixtures with the [strict configuration](tsconfig.json). Package checks
and the short tier run this check before their Vitest witnesses.

Response context windows are resolved per model from the current SDK query.
Exact model keys take precedence over pricing aliases; aliases with conflicting
window candidates remain unknown. A persisted query-wide window has unknown model provenance and is not used to
seed response accounting after a restart. Background responses retain the
turn that created their parent tool, even after that turn completes. Shutdown
waits for the SDK consumer within the short shutdown drain deadline, reports remaining gaps,
and flushes queued notifications before exiting. Every query removal, including
resume, rollback, settings changes, move import and unsubscribe, uses the same
cleanup path to report unfinished responses exactly once, even if SDK close is
stuck. Intentional shutdown leaves an
unfinished turn in progress for restart reconciliation rather than publishing a
terminal failure. The bridge integration tests
and inline scripted SDK remain outside the strict TypeScript program because
the shared bridge harness is untyped; SDK data fixtures and normalizer unit tests
remain strictly checked.
Per-thread operation ordering consumes `provider-method-policy.generated.mjs`,
rendered by `studio-contracts` from the Rust
[method policy](../../../runtime/packages/runtime-schema/src/provider_method_policy.rs).
Refresh it with `pnpm run contracts:generate`; `pnpm run contracts:check` rejects drift.
