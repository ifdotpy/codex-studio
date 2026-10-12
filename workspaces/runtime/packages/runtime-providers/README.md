# studio-runtime-providers

## Change Contract

- **Responsibility:** Synchronous JSON-line framing, per-connection lifecycle, request tracking, and ordered handoff of provider callbacks for Codex app-server and the Claude bridge.
- **Public interface:** Pure `SessionState` events/actions, split-I/O `ConnectionDriver<R, W, H>`, `Handler`, `CallbackDispatcher`, `LineDecoder`, `encode_line`, typed `Frame`/`Method`/`RequestId`, and `PendingBook` state transitions.
- **Boundaries:** No process spawning, Tokio, HTTP, SQLite, account discovery, turn orchestration, repair, or Claude Node bridge implementation. The caller owns the transport halves and supplies monotonic deadline values.
- **Invariants:** Initialize is written first and `initialized` follows its reply; each connection has a unique generation; IDs are registered before writes; deadlines and transport loss make unresolved requests unknown without retries; late matched replies remain evidence; callback delivery is FIFO and bounded.
- **Configuration owner:** The provider process owner supplies the duplex byte stream and timeout clock. The frame-size maximum is owned by `src/codec.rs`.
- **Focused check:** `cargo nextest run -p studio-runtime-providers`.

## Python wire inventory

The implementation below records the current Python/Node contract from source,
not a promise that provider CLIs will keep the same method set. Source paths are
repository relative; line numbers refer to the source revision used to build
this crate.

| Area                                  | Current behavior and methods                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            | Fixture coverage                                                                                                                                                                                                                                                                                                                    |
| ------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Codex transport                       | `AppServer` launches `codex app-server --listen stdio://`; each UTF-8 JSON object is one newline-terminated frame. Writes are serialized by `write_lock`, and short OS writes are continued until complete. There is no length-prefix framing in this client. The complete set of statically named `.call()`/`.submit()` methods found under `server/src` is listed below; additional calls use dynamic helpers that accept `method`. See [codex_runtime.py](../../apps/server/src/codex_runtime.py#L437), [codex_runtime.py write/submit/read](../../apps/server/src/codex_runtime.py#L596), [codex_accounts.py](../../apps/server/src/codex_accounts.py#L731), [codex_terminals.py](../../apps/server/src/codex_terminals.py#L225), and [codex_native_runtime.py](../../apps/server/src/codex_native_runtime.py#L41).                                                                                                                                                                                                                                                                                                 | `codex-rpc-replies.json` records setup/query replies. Other transcripts exercise `thread/start`, `turn/start`, tool calls, command output, token usage, and a missing `turn/start` reply. The Python replay runner asserts each outgoing RPC is explicitly recorded or allowlisted and checks each response ID against its request. |
| Codex inbound method frames           | A method with `id` is dispatched as a provider request; a method without `id` is a notification. Consumed notifications include `item/started`, `item/completed`, `item/agentMessage/delta`, `item/reasoning/textDelta`, `item/commandExecution/outputDelta`, `command/exec/outputDelta`, `turn/started`, `turn/completed`, `turn/plan/updated`, `turn/diff/updated`, `thread/closed`, `thread/status/changed`, `thread/tokenUsage/updated`, `account/login/completed`, `account/updated`, `account/rateLimits/updated`, `model/safetyBuffering/updated`, and `provider/generationStarted`. Provider requests include `currentTime/read`, `item/tool/call`, `item/commandExecution/requestApproval`, `item/fileChange/requestApproval`, `item/permissions/requestApproval`, `item/tool/requestUserInput`, and `mcpServer/elicitation/request`. Runtime handlers are in [codex_runtime.py](../../apps/server/src/codex_runtime.py#L1382), [codex_runtime.py notification dispatch](../../apps/server/src/codex_runtime.py#L7632), and [codex_runtime.py request dispatch](../../apps/server/src/codex_runtime.py#L8292). | Recorded event frames cover item/turn lifecycle, output deltas, tool calls, and token usage. Permission, elicitation, login, account, safety-buffering, generation, and current-time request frames are not covered by these checked-in transcripts.                                                                                |
| Codex IDs, order, and responses       | `submit` increments a per-`AppServer` integer sequence (first ID is `1`) and inserts a future in `pending` before writing. Responses are matched by ID, independently of arrival order; duplicate or unrequested responses are ignored by this Python reader. The write lock serializes actual frames, but concurrent callers are not promised that wire order equals ID allocation order. Incoming stdout lines are processed in read order and notifications are enqueued in that order. The replay dispatcher preserves ordinary request input order, while concurrent `turn/start` requests may overlap. See [codex_runtime.py](../../apps/server/src/codex_runtime.py#L688), [codex_runtime.py](../../apps/server/src/codex_runtime.py#L596), and [provider_replay_dispatch.py](../../apps/server/tests/server/provider_replay_dispatch.py#L1).                                                                                                                                                                                                                                                                    | `child-completion-before-parent.json` asserts child completion before parent completion. `lost-turn-start-reply.json` and `restart-mid-turn-supervisor-replay.json` pin missing request IDs; the supervisor fixture also contains an allowed orphan response.                                                                       |
| Claude bridge transport               | Python starts Node with `node bridge.mjs <state-root>` and exchanges the same newline-delimited JSON objects over stdio. Both directions use envelopes `{method, params}` for notifications, `{id, method, params}` for requests, and `{id, result}` or `{id, error}` for replies; error envelopes may also contain `data`. The bridge reads each stdin line, parses JSON, and writes with `JSON.stringify(value) + "\n"`. No length-prefix frame is used. See [codex_claude.py](../../apps/server/src/codex_claude.py#L248), [bridge.mjs send/request](../../../providers/apps/claude-bridge/bridge.mjs#L200), and [bridge.mjs input dispatch](../../../providers/apps/claude-bridge/bridge.mjs#L2321).                                                                                                                                                                                                                                                                                                                                                                                                                | `claude-first-turn.json` records initialization and model-list replies plus SDK events, but not a complete byte transcript of all bridge requests and responses.                                                                                                                                                                    |
| Claude IDs, order, and errors         | Bridge-originated requests use `claude:` followed by `randomUUID()` and are matched from a `pending` map. Cancellation removes the pending entry; later replies no longer match it. For each thread, `turn/start`, `turn/steer`, `thread/resume`, `thread/fork`, `thread/read` (except lightweight reads), `thread/rollback`, `claude/settings`, `claude/state`, and `thread/compact/start` are sequenced through that thread's operation promise; unrelated threads and other methods may proceed independently. A request failure is returned as `{id,error:{code,message,data?}}`. See [bridge.mjs](../../../providers/apps/claude-bridge/bridge.mjs#L203), [bridge.mjs per-thread ordering](../../../providers/apps/claude-bridge/bridge.mjs#L2341), and [bridge.mjs response handling](../../../providers/apps/claude-bridge/bridge.mjs#L2360).                                                                                                                                                                                                                                                                    | The Claude fixture checks response templates and the first assistant result. UUID allocation, cancellation races, concurrent threads, and error `data` are not covered.                                                                                                                                                             |
| EOF, malformed input, and uncertainty | Codex JSON parse errors are logged and the reader continues unless transport failure is recorded. EOF or transport failure rejects every unresolved future with “outcome unknown” and clears the pending map. A deadline raises `ResponseTimeout` with outcome unknown. If a write may have partially reached the provider, the exact submitted request stays attached to `SubmissionUnknown`; a definite pre-write rejection is reported separately. Claude bridge stdin EOF runs bridge shutdown and the Python reader likewise sees unresolved replies as unknown. See [codex_runtime.py](../../apps/server/src/codex_runtime.py#L688), [codex_runtime.py response timeout](../../apps/server/src/codex_runtime.py#L706), [codex_runtime.py EOF settlement](../../apps/server/src/codex_runtime.py#L1382), and [bridge.mjs shutdown](../../../providers/apps/claude-bridge/bridge.mjs#L2387).                                                                                                                                                                                                                        | The lost-start and supervisor replay fixtures cover missing replies and orphan responses; there is no recorded malformed-line or partial-write transcript.                                                                                                                                                                          |

### Statically named outbound method inventory

The app-server requests identified from `AppServer` call sites, runtime
preparation and action helpers, provider-specific modules, and the native
executable compatibility list are:

`initialize`, `config/read`, `skills/extraRoots/set`, `model/list`,
`account/read`, `account/rateLimits/read`, `account/login/start`,
`account/login/cancel`, `account/rateLimitResetCredit/consume`,
`thread/loaded/list`, `thread/start`, `thread/resume`, `thread/read`,
`thread/turns/list`, `thread/items/list`, `thread/list`, `thread/name/set`,
`thread/unsubscribe`, `thread/fork`, `thread/archive`, `thread/unarchive`,
`thread/metadata/update`, `thread/settings/update`, `thread/compact/start`,
`thread/rollback`, `thread/backgroundTerminals/list`,
`thread/backgroundTerminals/terminate`, `thread/queue/list`, `thread/goal/set`,
`thread/realtime/start`, `thread/realtime/stop`,
`thread/realtime/appendSpeech`, `turn/start`, `turn/steer`, `turn/interrupt`,
`review/start`, `process/spawn`, `process/writeStdin`, `process/resizePty`,
`process/kill`, `command/exec`, `command/exec/write`, `command/exec/resize`,
`command/exec/terminate`, `mcpServerStatus/list`, `claude/state`,
`claude/settings`, `claude/activity`, `claude/commands`, `claude/stopTask`,
`claude/diagnostics`, `claude/moveVersions`, `claude/moveIdentity`,
`claude/movePreflight`, `claude/moveProof`, and `claude/moveExport`.

The selected `thread/start` versus `thread/fork`, `thread/resume` versus
`thread/start`, `thread/compact/start` versus `review/start`, and safety-retry
methods are chosen by explicit branches in [codex_account_transfer.py](../../apps/server/src/codex_account_transfer.py#L898),
[codex_runtime.py](../../apps/server/src/codex_runtime.py#L6140),
[codex_runtime.py](../../apps/server/src/codex_runtime.py#L11254), and
[codex_safety_buffering.py](../../apps/server/src/codex_safety_buffering.py#L153).
Monitor input selects `command/exec/write` or `command/exec/resize` in
[codex_rules.py](../../apps/server/src/codex_rules.py#L734); voice methods are
sent by [codex_native_voice.py](../../apps/server/src/codex_native_voice.py#L150).
The replay contract also explicitly permits `thread/metadata/update`.
Notifications sent by the client are `initialized` and replies to provider
requests use the same `{id,result}` / `{id,error}` envelope. Provider-initiated
requests are documented in the inbound row above.

## Rust API and dependencies

`LineDecoder::push` accepts arbitrary byte chunks and returns every complete
frame in order. Call `finish` at EOF to distinguish a clean stream end from a
truncated final line. `encode_line` emits one JSON object and a trailing LF.
The same codec serves both provider transports because both clients use JSON
lines; their method sets and ID forms are represented by `Method` and
`RequestId`. A `Frame::UnknownMethod` retains the whole source object, including
unrecognized fields.

Method names and frame envelopes are typed; request `params` and response `result` remain `serde_json::Value` in this slice.
Typed payloads could come from Codex app-server JSON Schema: [the CLI docs](../../../../docs/cli.md#L51) document `codex app-server generate-json-schema`, but schema generation was not verified here and no schema is checked into this repository.

Neither Python client imposes a line-size limit: Codex reads `stdout` by line
and writes `json.dumps(value) + "\n"` ([reader](../../apps/server/src/codex_runtime.py#L1382),
[writer](../../apps/server/src/codex_runtime.py#L596)); the Claude bridge uses
Node `readline` and `JSON.stringify(value) + "\n"` ([writer](../../../providers/apps/claude-bridge/bridge.mjs#L200),
[reader](../../../providers/apps/claude-bridge/bridge.mjs#L2321)). All 36 protocol
objects in the checked-in fixtures are at most 1,220 bytes. The Claude bridge
limits individual image bytes to 3,750,000 ([images.mjs](../../../providers/apps/claude-bridge/images.mjs#L12)),
which leaves substantial room below 16 MiB for base64 and the JSON envelope.
The repository specifies no maximum for Codex input text, so 16 MiB is an
explicit memory-safety boundary rather than a guarantee for arbitrarily large
valid Codex frames; larger frames fail as `FrameTooLarge` instead of being
truncated.

`PendingBook::allocate_numeric` implements Codex's positive sequence starting
at one. `register` accepts bridge-generated string IDs, whose UUID entropy is
owned by the provider adapter. `observe_response` distinguishes matched, late,
duplicate, and unknown IDs. `mark_connection_lost` moves pending IDs to
`Unknown`, closes that connection book, and does not schedule retries. A new
connection must have a new book.

This crate adds `thiserror` 2.0.17 (MIT OR Apache-2.0) for typed error
implementations. It also uses the workspace's `serde` 1.0.229 and
`serde_json` 1.0.151 (MIT OR Apache-2.0) for derives, JSON parsing,
serialization, and preservation of arbitrary provider values. There is no
handwritten JSON parser or serializer.

The conformance test reads the canonical checked-in fixtures in
`workspaces/runtime/apps/server/tests/fixtures/provider-replay/`; those files
remain the sole fixture copies. It structurally compares every recorded method
frame and reply template after a decode/encode cycle and validates the exact
method/ID assertions for missing and orphaned replies. The fixture assertions
do not prove behavior against a live CLI.

## Remaining work

- Provider process creation, restart, stdout/stderr draining, and retained
  process handles remain in [codex_runtime.py](../../apps/server/src/codex_runtime.py#L437)
  and [codex_process_supervisor.py](../../apps/server/src/codex_process_supervisor.py#L1).
- Account discovery and authentication remain in [codex_claude.py](../../apps/server/src/codex_claude.py#L1),
  [codex_accounts.py](../../apps/server/src/codex_accounts.py#L1), and their
  provider-specific helpers.
- The Claude Agent SDK adapter and its Node-side session, command, and event
  translation remain in [bridge.mjs](../../../providers/apps/claude-bridge/bridge.mjs#L1)
  and [commands.mjs](../../../providers/apps/claude-bridge/commands.mjs#L1).
- Turn logic and runtime event handling remain in [codex_runtime.py](../../apps/server/src/codex_runtime.py#L7526)
  and the focused modules under `workspaces/runtime/apps/server/src/`.

### Synchronous connection service

`SessionState` is the deterministic event/action layer. It writes initialization
first, emits `initialized` after the matching response, routes responses by
generation and ID, and emits unknown outcomes on deadlines or transport loss.
The caller supplies `now_ms` to `expire`, so timeout decisions need no sleeps.
Late replies for timed-out IDs remain evidence; canceled Claude IDs are removed
from `PendingBook`, and their later orphan replies are silently ignored.

`ConnectionDriver<R, W, H>` owns separate read and write halves. A dedicated
reader decodes frames, a write mutex serializes complete encoded lines, and a
bounded callback worker drains provider requests and notifications in FIFO order.
The reader does not execute user handlers. Claude requests use `claude:` IDs;
`turn/start`, `turn/steer`, `thread/resume`, `thread/fork`, and non-lightweight
`thread/read` are queued per thread, while independent threads proceed. The
driver's owner calls `pump_next` to apply incoming frames and deliver callback
work; it calls `close` after its transport owner has ended the read half.

### Session state transitions

The Python reader resolves responses as it reads them while `wait` can time out
independently (`codex_runtime.py:706,1382-1410`). This driver checks an injected
deadline at each pump boundary before consuming a queued frame. A response that
is queued at an elapsed deadline first makes unresolved work unknown; the
subsequent response is retained as late evidence. A response pumped before the
deadline resolves normally.

| State          | Accepted events                                                                                                                                             | Pending request disposition                                                                                                                                                                                     |
| -------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Initializing` | Initialize reply, inbound provider frame, deadline, transport loss, close. New ordinary submits are rejected.                                               | Successful initialize resolves its waiter, writes `initialized`, then enters `Ready`. Initialize error, deadline, or transport loss enters `Lost`; unresolved waiters resolve `Unknown`. Close enters `Closed`. |
| `Ready`        | Submit, matching/orphan/duplicate reply, provider request or notification, Claude cancel, deadline, transport loss, close.                                  | Matching replies resolve their waiter. Deadline or transport loss enters `Lost` and resolves every unresolved waiter `Unknown`; close does the same and enters `Closed`.                                        |
| `Lost`         | Late reply evidence, provider requests/notifications, repeated transport loss, and explicit close. New submits are rejected; unmatched replies are ignored. | Matched late replies resolve the retained ticket with `late: true`; inbound provider frames still dispatch in order. Close enters `Closed` and clears retained entries without resolving them twice.            |
| `Closed`       | Repeated close or any late frame.                                                                                                                           | No pending entries remain; all frames and duplicate close events are ignored.                                                                                                                                   |

Python write failures mark the transport failed and preserve the submitted
request as `SubmissionUnknown` (`codex_runtime.py:637-681,688-703`). Its reader
matches responses by ID and ignores unknown or duplicate IDs (`:1404-1410`).
Claude removes canceled request entries from its pending map
([bridge.mjs:208](../../../providers/apps/claude-bridge/bridge.mjs#L208)); a later
canceled reply is therefore an ignored orphan. This Rust slice serializes the
five required methods and requires a thread ID so each lane has a release key.
The Node bridge also sequences `thread/rollback`, `claude/settings`,
`claude/state`, and `thread/compact/start`; those remain in the Python/Node
adapter layer.

The vertical caller tests cover the request shapes used by Python for turn start
and interrupt ([runtime](../../apps/server/src/codex_runtime.py#L7129),
[interrupt](../../apps/server/src/codex_runtime.py#L10690)), account read
([runtime](../../apps/server/src/codex_runtime.py#L3712)), and model list
([catalog](../../apps/server/src/codex_catalog.py#L147)).
