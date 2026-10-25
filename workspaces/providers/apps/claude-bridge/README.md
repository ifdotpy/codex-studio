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
opening input anchor remain uncertain. The bridge shows an uncertainty notice
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
The existing disk-full deferral remains available.

Rollback and fork boundaries use every native part of a composite input in
order, beginning at its earliest part. Forked history remaps every retained input
UUID. File checkpoints keep their own final-message identity and are not carried
across an unverified fork.

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
