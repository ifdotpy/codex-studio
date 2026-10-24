# Compact sync protocol v4

Owner: lead of the compact-sync effort. Integration branch: `feat/compact-sync`.
The owner lifted the bug-fix freeze for this work and allows a breaking wire change.
Old clients receive `426 Upgrade Required` from the version check; no dual format.

## Goals

- Minimal bytes on the wire; usable on slow and lossy links.
- Point updates: only changed records travel.
- Simple, testable code; no speculative machinery. No new allocations or
  copies on hot paths compared with v3; measure before and after.
- The client applies every change of one SSE event in one batch (one store
  publication), never record by record.

## Wire contract

`SYNC_PROTOCOL_VERSION` becomes `4` (owner: `runtime-schema/src/sync_protocol.rs`).
OpenAPI stays the owner of every DTO; Rust and TypeScript types stay generated.

### Entity change (replaces `SyncEntity` everywhere: stream, pull, `_syncEntities`)

```json
{"op":"upsert","collection":"agent","id":"<id>","seq":42,"value":{...}}
{"op":"delete","collection":"agent","id":"<id>","seq":43}
```

- `value` is the structured typed DTO of that collection, never a JSON string.
- `collection` and `id` appear once; the client derives `entity:<collection>:<id>`.
- Within one batch the server sends at most one change per key (the latest).
- A list leaving membership is not a delete; `delete` means the entity is gone.

### Stream size

One limit, `SYNC_STREAM_EVENT_MAX_BYTES` (owner: `runtime-schema/src/shared_values.rs`),
bounds the complete serialized SSE event in UTF-8 bytes including `event:`, `id:`,
`data:` lines. It replaces `SYNC_ENTITY_STREAM_MAX_BYTES`,
`SYNC_TRANSCRIPT_STREAM_MAX_BYTES`, and `SYNC_DRAFT_STREAM_MAX_BYTES`. A larger
event carries revisions only, and the client pulls the interval over HTTP
(Brotli JSON). The HTTP compression threshold is a separate contract.
The limit applies to `resources` events only. `token-rates`, heartbeat,
session, acknowledgement, and `api-schema` events are exempt; `token-rates` stays
bounded by its schema `maxItems`.

### Stream compression

`/api/sync/stream` is Brotli-encoded when the request accepts `br`. The encoder
flushes after every event, so each event reaches the client at once, and keeps
its dictionary for the lifetime of the stream. Without `br` the stream is plain.

### Token rates

The `token-rates` event carries a full snapshot only as the first frame of a
stream; later frames carry changed records and removed agent ids.

### Live subscription changes

One SSE stream per client. The first event names a `sessionId`. The client changes
its resource set with `POST /api/sync/subscriptions {sessionId, gen, add, remove}`;
the stream answers with an acknowledgement for `gen`, followed by initial data of
added resources. The client drops data of removed resources received before that
acknowledgement. An unknown or expired session answers so that the client reopens
the stream with its full set and cursors, the v3 recovery path. Set changes are
coalesced on the client. If the resource set changes before a session is announced,
the client also reopens the stream with its full set and cursors.

## Read ownership and recovery

The server owns each value and publishes its typed entity changes through the
resource stream. The client must not turn a server revision notification, timer,
or reconnect into an automatic HTTP read. HTTP reads are for initial state when
the stream has no snapshot, explicit user actions (including opening a bounded
detail view), and recovery when the applied cursor is missing, the interval was
oversized, or persistence failed. Oversized and unbounded values need a bounded
detail or cursor contract before they can be pushed as entities. Mutations and
their lost-response recovery keep their exact request identities.

`rateLimit` is the existing streamed per-account limits collection. `studioError`
is the streamed Studio Errors collection: the entity publisher derives rows from
the server-owned error sources in the source transaction, publishes upserts and
tombstones, and the initial subscription snapshot supplies the current list. The
Studio Errors GET remains available for explicit recovery only. User refresh of
limits may make one explicit read; no idle limits hydration or watcher is used.

The following inventory records the client read paths found during the T17 audit.
Assigned T18/T19 work is listed as such; all other follow-up items remain explicit
open/action or recovery reads until a typed bounded owner is implemented.

| Resource / caller | Read or behavior | Disposition |
| --- | --- | --- |
| Limits (`App.tsx`, `useAccountLimits.ts`, `useLimitEligibility.ts`) | `/api/limits` hydration and resource watcher | `rateLimit` entities; explicit user refresh only. |
| Studio Errors (`StudioErrors.tsx`) | `/api/studio-errors` on mount, every 120 s, open, and after actions | `studioError` initial snapshot/upserts/tombstones; GET only for explicit recovery. |
| Accounts (`Accounts.tsx`) | `/api/accounts` after account resource changes | T18 bounded account entities; interactive Claude login progress and its POST workflow remain explicit. |
| Models (`WorkerModelPicker.tsx`, New Chat settings) | `/api/models` on model resource changes | T19 per-model entities. `/api/models` remains an explicit recovery/read path; startup before a default account/catalog exists returns `catalogPending`, not an unknown-account error. |
| Account login (`ClaudeSignIn.tsx`, `ClaudeAddSignIn.tsx`) | `/api/accounts/claude/login` during interactive sign-in | Keep as user-started login workflow and its explicit progress reads. |
| Costs and session cost (`Usage.tsx`) | `/api/costs`, session-cost resource read | T18 bounded costs/session-cost entities. |
| Desktop state (`SupervisorRecoveryNotice.tsx`, `NativeRuntimeStatus.tsx`) | `/api/desktop` after desktop resource changes | T18 bounded desktop entities. |
| Queue (`useMessageQueue.ts`) | `/api/queue` after queue resource changes | T18 queue entities; preserve durable request identity and uncertain-send reconciliation. |
| Terminal summary (`TerminalDock.tsx`) | `/api/terminals` after terminal resource changes | T18 bounded terminal-summary entities. |
| Terminal output (`TerminalDock.tsx`) | `/api/terminals/output(offset)` | Explicit open/pagination and recovery read; output is unbounded and needs cursor/size bounds. Terminal input, resize, and media stay on their interactive protocols. |
| Message receipts (`useMessageReceipts.ts`) | `/api/messages/receipts` in requested-ID chunks of 100 | Explicit receipt lookup/recovery only; history can grow and needs a bounded cursor contract. |
| Voice status (`RealtimeVoice.tsx`) | POST `/api/voice/records` after voice resource changes | Status projection needs a typed stream owner; start/end/status POSTs and media lifecycle remain explicit. |
| Retained-worktree costs (`RetainedWorktrees.tsx`) | `/api/retained-worktrees(agent)` after costs changes | Explicit panel-open/recovery read until a bounded owner and size policy are implemented. |
| Background task detail (`BackgroundTasks.tsx`) | `/api/task` when a task is opened | Explicit open/recovery read; detail is not a stream hint refresh. |
| Workspace (`Workspace.tsx`) | `/api/changes`, `/workspace`, `/plan`, `/checkpoints`, `/capabilities` after workspace changes | Explicit panel-open/recovery reads; do not hydrate filesystem diffs from a generic revision hint. Bounded typed resources require separate ownership. |
| Server discovery (`useServerDiscovery.ts`) | Re-load discovery after multi-server changes | Existing explicit discovery workflow; do not use a resource hint to repeat the full load. |
| Vault setup (`VaultSetup.tsx`) | POST `/api/vault/setup/facts` every 2 s after the user reveals keys | Separate readiness-event follow-up. The POST polling workflow is scoped to the explicit setup action; do not misclassify it as a GET or remove it without a readiness owner. |
| Server account limits (`ServerAccountLimits.tsx`) | Full account GET on settings selection; local age timer | Explicit settings selection; local age display timer is not a network read. |
| Non-network timers | Account sign-in, native notice, capacity retry, agent progress, session activity, usage age, reasoning duration, sign-in card, background-task progress, dictation elapsed | Local display/progress timers; keep local. |
| Draft sync interval | Drains locally queued draft writes | Local write scheduling, not a state refetch; keep. |
| Resource-event peer heartbeat | BroadcastChannel coordination | Local cross-tab coordination, not HTTP polling; keep. |
| Resource watcher `.refresh()` handlers | Handler-dependent | Audit the handler's owner; do not make an HTTP read on a revision hint. Explicit user refresh remains allowed. |

## Ordering invariants (unchanged from v3)

Startup moves legacy workspace rate-limit snapshots to per-account entities and
removes the legacy workspace fields in
[`migrate_workspace_limits`](../../workspaces/runtime/packages/runtime-store/src/repositories/account_state.rs).
These committed changes use the shared entity/transcript sequence allocator.
Frozen v3 transcript fixtures therefore account for the startup allocation when
checking newly assigned cursors; existing transcript cursors remain unchanged.

- A notification never advances the applied cursor; only a fully applied interval does.
- A stale HTTP response cannot overwrite a newer change or revive a deleted entity.
- Unsubscribing cancels reads and rejects late results by subscription generation.
- Server restart, lost tombstones, and queue overflow force explicit recovery.
