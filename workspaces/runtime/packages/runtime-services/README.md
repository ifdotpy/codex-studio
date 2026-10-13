# `studio-runtime-services`

## Change Contract

- **Responsibility:** the runtime's application rules: what a request or an
  event does to state, in which order, and what it returns. One leaf module
  per service.
- **Public interface:** each service module's functions and port traits.
- **Allowed dependencies:** `studio-runtime-store`, `studio-runtime-providers`,
  `studio-runtime-process`, `studio-operations`; Tokio and the existing
  `futures-util` for the provider session and account tasks.
- **Forbidden dependencies:** `studio-runtime-http`, Axum, the server
  application, the Python runtime.
- **Invariants:** application decisions remain synchronous; the provider session
  and account tasks own asynchronous orchestration. No service holds a store transaction
  across a call into a provider or process port; an outcome that cannot be
  observed is reported as unknown and never retried here.
- **Focused check:** `cargo nextest run -p studio-runtime-services`.

## Pattern

Transcript search uses Unicode lowercase matching, so `ß` and `SS` do not match as Python casefold would.

A service module separates pure decisions from effects. Pure functions take
snapshots and explicit clocks and return decisions; they are tested directly.
Effects go through the store's repositories or through a narrow port trait
declared in the module, with an in-memory fake for tests. A route module in
`studio-runtime-http` converts the request DTO to the service input, calls the
service inside `tokio::task::spawn_blocking`, and converts the result or error
to the response DTO.

## Terminals

On Unix, `terminal_driver` persists terminal records and bounded output history;
`terminal_pty` runs the local PTY session through the `PtyHost` port. The PTY
child is a session and process-group leader. Close, server shutdown, and final
session-owner drop signal the group and wait for the child. Reader events pass
through the driver's incremental UTF-8 decoder before storage, and each committed
change wakes `ChangeNotificationHub`. Terminal sessions do not start a model turn.

The PTY adapter uses `pty-process` 0.5.3 (MIT). Its blocking API supplies PTY
allocation, resize, byte I/O, and child wait; `rustix` 1.1.5 supplies the safe
process-group signal. Windows live terminal actions remain unavailable.

### Accidental Python behavior reproduced

Terminal output cursors count Unicode scalar values, not UTF-8 bytes. Rust keeps
that offset convention for existing Python-written terminal history.

Add each service with an explicit `pub mod` line in alphabetical order; a
directory parity test checks that module declarations match the source files.

- No build-script scanner: explicit declarations keep modules visible to Rust tooling.
- No `automod`: its upstream is archived, and rustfmt skips macro-declared modules.

Event writes persist durable pending or cancelled rows and signal the shared HTTP wake handle after commit.
Event consumption (scheduler) is a separate service and a cutover gate.
Active event-rule delivery (`rule-driver/event-rules`) is also a separate service and cutover gate.

Dependency direction:

```text
operations ← store, providers, process ← services ← http ← runtime-server
```

Profile text uses Rust `trim()`; it intentionally does not emulate Python `str.strip()` for U+001C–U+001F.

## Progress panel files

The progress panel operations mirror the Python file checks: progress and agent
directories must be ordinary directories
(`workspaces/runtime/apps/server/src/codex_progress.py:22-57`); the progress
file must be a regular file and is bounded and checked for a stable read
(`workspaces/runtime/apps/server/src/codex_progress.py:115-149`); layout files
are checked as regular files before and after opening
(`workspaces/runtime/apps/server/src/codex_progress_layout.py:36-59`). Layout
writes flush the temporary file and recheck the progress revision before
replacement (`workspaces/runtime/apps/server/src/codex_progress_layout.py:203-231`).
This Rust implementation uses ordinary path operations and does not use
anchored `openat` access. On Windows, progress
read and layout write report the named 501 dependency
`resource-policy/windows-file-identity` until compatible file identity support
is implemented.

The workspace file service resolves relative paths under the agent's `cwd`.
It also accepts absolute paths and follows symlinks without checking that the
resolved file stays inside `cwd`, matching Python's existing
[`workspace_path`](../../apps/server/src/codex_workspace.py#L567-L572)
rule. Keep this behavior deliberate; tightening it requires a separate
compatibility decision. The service pins it in
`workspace_files::tests::local_file_metadata_and_content_follow_symlinks`.

The plan that assigns services to slices is
[`docs/plans/rust-runtime-work-breakdown.md`](../../../../docs/plans/rust-runtime-work-breakdown.md).

Git changes use `std::process::Command` to preserve the Python runtime's system-Git status and patch output without a Git-library dependency.
`gix 0.89.0` (MIT/Apache-2.0; 199 normal dependency-tree entries; 9.8s clean check) and `git2 0.21.0` (MIT/Apache-2.0; 7 entries; 8.5s including libgit2 build) can inspect status and diffs but require custom rendering and can change CLI output.

## Remote transport

The outbound multi-server and federation clients keep their Python wire formats
separate. `multi-server/identity.json` stores `privateKey` as PKCS#8 PEM
(`BEGIN PRIVATE KEY`) and `publicKey` as Ed25519 SubjectPublicKeyInfo PEM
(`BEGIN PUBLIC KEY`), with `serverId` and `label`. The
`runtime_federation_identity` row with `id='local'` stores the same PEM formats
in `privateKey` and `publicKey`, with `stateId` and `label`. Federation response
wrappers are verified with the paired Ed25519 key; multi-server responses retain
Python's optional `serverId` and `publicKey` identity checks. Rust requires an
Ed25519 SubjectPublicKeyInfo algorithm identifier for stored and paired public
keys, which is stricter than Python's multi-server response field comparison.
Signature text uses strict, padded standard Base64. Federation response state is
checked when the caller supplies an expected state; a missing or null state is
accepted when there is no expected state.

Python's HTTPS clients use normal system CA trust and hostname verification;
they do not pin TLS certificates. Rust uses the platform verifier for the same
system roots. Peer identity is checked at the signed protocol layer with the
paired Ed25519 key. Multi-server identity-file loading returns the named
`UnsupportedPlatform` error on Windows.

The runtime-server binary does not install a `log` backend. ureq and
ureq-proto emit their wire traces through the `log` facade, so those records are
discarded in this binary; the service does not install or add a logging backend.
If a backend is added later, its target filters must exclude `ureq` and
`ureq_proto` because their trace output can contain request and response data.

The transport explicitly selects rustls' `ring` crypto provider through ureq's
`unversioned_rustls_crypto_provider` API. That API is unstable; the workspace
pins ureq to `=3.4.2` to keep this call aligned with the supported configuration.
ureq uses `rustls-no-provider` plus `platform-verifier`, so the lock file omits
the otherwise enabled `webpki-roots` bundle. The platform verifier brings
`rustls-platform-verifier`, `rustls-native-certs`, `openssl-probe` and
platform-specific certificate support crates into the lock graph.

## Account registry and limits read contract

This slice loads the persisted account registry and workspace limits once at
startup. Its state is immutable; request handlers project the validated
snapshot. `GET /api/accounts` is a persisted read with an authentication
recovery overlay from `runtime_usage_resumes`. Keeping the registry fresh
(refresh, discovery, and login receipts) belongs to the later account-refresh
slice. Until that exists, Rust shows the registry as Python last saved it. An
absent registry fails startup with a located error; this slice does not
synthesize account data.

Python's GET calls `Runtime.accounts_snapshot()` and
`AccountStore.snapshot(refresh=False)` (`codex_usage_resume.py:179-192`,
`studio_api/accounts/router.py:168-169`). The registry is loaded into Python's
in-memory `AccountStore` at construction and this GET does not refresh auth
files. Other Python calls can mutate the in-memory view without saving it:
`AccountStore.refresh` updates `status`, `error`, `accountId`, `email`, and
`plan` (and its private `_credentialIdentity`) at `codex_accounts.py:161-202`
without `_save`; `confirm_native_auth` changes `status`, `plan`, `error`, and
private `_authErrorKind` at `:238-253` without `_save`. The persisted Rust
projection can therefore lag those in-memory fields until Python next saves
the registry. The Rust read service does not open auth files, decode tokens,
or resolve profile directories.

| `GET /api/accounts` field              | Source                                                                                                                                                                                        | Absent, malformed, or refresh behavior                                                                                                                                                   |
| -------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `accounts[]`                           | Account rows from the startup registry, in persisted map order; underscore-prefixed private fields are removed.                                                                               | Missing registry fails startup. Malformed or schema-invalid registry fails startup with file and JSON pointer, without instance values. External changes are not observed until restart. |
| `accounts[].canAttemptNativeProof`     | Persisted Claude identity/status and keychain-error metadata plus deleted/disconnected/duplicate flags.                                                                                       | Omitted unless the saved row meets the proof eligibility conditions. No provider is started.                                                                                             |
| `accounts[].authenticationRecovery`    | A matching `runtime_usage_resumes` row with `status=scheduled`, `cause=auth`, and `failedAt` older than the Python `AUTH_WAIT_NOTICE_SECONDS` threshold (`codex_usage_resume.py:32,182-191`). | Omitted when there is no matching old scheduled auth resume. The message is `AUTHENTICATION_RECOVERY_MESSAGE` in the service.                                                            |
| `archivedAccounts[]`                   | Persisted rows with `deleted: true`, projected without private fields.                                                                                                                        | Empty when no archived rows exist.                                                                                                                                                       |
| `defaultAccountKey`                    | Registry `defaultAccountKey`.                                                                                                                                                                 | Missing or malformed registry fails startup.                                                                                                                                             |
| `logins[]`                             | Persisted login receipts; map key supplies `requestId`.                                                                                                                                       | Empty when absent. Invalid receipts fail startup.                                                                                                                                        |
| `supportsDisconnect`, `supportsDelete` | Static Rust API capabilities.                                                                                                                                                                 | Always `true`.                                                                                                                                                                           |

Each public account row is validated against the Python-exported registry schema and
the `Account` response model (`studio_api/accounts/models.py:57-77`): `id`,
`home`, `label`, `source`, and `status` come from required persisted row
fields; `provider`, `accountId`, `email`, `plan`, `error`, `disconnected`,
`deleted`, `duplicateOf`, and `claudeOptions` are optional persisted fields;
`canAttemptNativeProof` is projected from persisted Claude identity and
eligibility fields; and `authenticationRecovery` is the database overlay above.
Public extension fields are retained. Any malformed or missing required
registry field fails startup at its JSON pointer. Archived rows use the same
persisted public fields, including `deleted`, and omit private underscore
fields. Login receipt fields are read from the persisted receipt value, with
`requestId` supplied by its map key; a malformed receipt fails startup.

Limits restoration is startup-only, matching `Runtime.restore_rate_limits`
(`codex_runtime.py:9120-9138`). The Python route reads its in-memory cache at
`studio_api/accounts/router.py:220-228`; it does not query SQLite per request.
SQLite reads only the existing `sync_entities` workspace row. The storage
format is `AccountRateLimitsDto` and its nested models in
`studio_api/sync/models.py:1070-1117`; Python API restoration then validates
`UsageLimitsResponse` (`studio_api/accounts/models.py:362-369`). The reader
consumes the exported schemas in `runtime-schema/src/schemas/account-registry.schema.json`
and `runtime-schema/src/schemas/workspace-usage-limits.schema.json`; the latter validates
selected entries through its `UsageLimitsResponse` model.

| `GET /api/limits` field                             | Source                                                                                                                                 | Absent, malformed, or refresh behavior                                                                                                                                                                                      |
| --------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `accountKey`                                        | The matching persisted account cache entry; legacy `rateLimits` is the default entry only when `rateLimitsByAccount` has no `default`. | A missing or mismatched cache is uncached.                                                                                                                                                                                  |
| `data`                                              | Persisted `AccountRateLimitsDto.data`, accepted for serving only when it also passes Python's `UsageLimitsResponse` shape.             | Missing or null data is not restored. An invalid restored snapshot is treated as absent, as in Python's restore validation; it carries a safe diagnostic with workspace row, account key, JSON pointer, and schema keyword. |
| `at`, `readAt`, `checkedAt`, `processedAt`, `error` | Optional persisted cache metadata.                                                                                                     | Wrong types make that account cache absent and produce a value-free diagnostic.                                                                                                                                             |

A cached request can return a valid persisted response. A request that needs a
refresh returns 501 with dependency `provider-session/account-rate-limits`;
missing cache and intentional `data: null` are normal uncached states. An
unknown account with no valid cache returns JSON 400 `Unknown Codex account`.
Invalid workspace JSON/schema is reported as a safe `workspace/current`
diagnostic; persisted documents share the store-owned 64 MiB and 128-level
bounds, with one iterative depth scanner before recursive parsing. No debug
formatter retains or prints registry or limits values.

The exported `runtime-schema/src/schemas/account-registry.schema.json` is the registry
authority. The registry is one fail-closed authority:
invalid JSON or a schema violation stops startup with the path and first
location, without field contents. A literal unpaired surrogate produced by
Python's `set_name` writer is the one known product-writable registry shape
this Rust reader refuses rather than lossily decoding. Schema-tightening's
read-only pre-cutover state check is the guard; if it finds one, the lead owns
the one-time state upgrade. No Python truthiness or malformed-file
compatibility rules are reproduced.

`runtime-schema/src/schemas/workspace-usage-limits.schema.json` is a projection schema: it
validates the workspace usage fields this slice reads, while intentionally
ignoring unrelated workspace fields. It is not an allow-list for the complete
workspace format.

The limits restore policy intentionally differs from Python's
`Runtime.restore_rate_limits` (`codex_runtime.py:9125-9138`): Python's
single exception boundary aborts the full restore loop on the first invalid
entry. Rust isolates a refused account entry and continues restoring other
valid entries. The legacy `rateLimits` snapshot is considered only when the
raw `rateLimitsByAccount` map has no `default` key (`codex_runtime.py:9128-9129`);
a present but refused default entry does not revive legacy data.

The producer corpus includes a registry above the old 1 MiB ceiling, written
through `AccountStore.set_name` with its untrimmed rename receipt
(`codex_accounts.py:385,394,403`).

### Later account-refresh boundary

Python resolves Codex homes and rejects only a resolved `.claude` path
component (`codex_state.py:22-26,39-40`). Claude configuration is selected by
`profile_options` and `subscription_env`, with the `~/.claude` fallback
(`codex_claude.py:61-80,150-161; codex_claude_login.py:228-229`). This slice
intentionally defines no HOME/cwd allow-list or auth-file symlink policy; the
refresh owner must use the product's supplied configuration roots and own the
final auth-file boundary. Python permits external explicit/profile paths and
follows filesystem resolution before its `.claude` exclusion.

The Python account mutation points that a later owner must port are `refresh`,
`confirm_native_auth`, `default`, `disconnect`, `delete`, `reconnect`,
`set_name`, `register`, `register_claude`, `update_claude`, `discover`,
`start_login`, `login_receipts`, `cancel_login`, `login_completed`, and
`set_rate_limits`. This read slice implements none of them.

`GET /api/models` serves the Codex catalog through the Tokio session and a
temporary stdio process-group attachment. The cache returns pending immediately,
serves stale values during one refresh, and applies a single exchange deadline
to initialization and pagination. The process adapter stops and reaps the child
before a refresh result is published. Claude catalogs retain their named
`provider-service/claude-model-catalog` dependency. The temporary adapter requires
Unix; other platforms report the catalog unavailable.

`model_catalog.rs` owns pagination, validation, cache, and pending/unavailable
states. The async HTTP handler only runs the account snapshot read on the
blocking pool. `rust-models-http-contract.py` tests pending/cache behavior,
Python-written account state, and no provider start or registry write on refusal
against both backends.

The producer test uses Python `AccountStore` writes and the real
`Runtime.store_rate_limits` → `codex_sync_entities.patch`/`put`/`project` path
inside a disposable root and SQLite database. It does not start a provider or
make a network request. Run it with
`cargo nextest run -p studio-runtime-services --test account_policy_differential`.

### Account driver process boundary

The account driver's Claude metadata reader runs `claude auth status --json`
with an eight-second timeout. `process-wrap` places the command in a POSIX
process group, so timeout and output-limit termination cover its child
processes as well. Windows retains direct-child termination and is outside this
Rust cutover. The command's output is parsed in memory and is never logged.
On macOS, Rust currently reports the named reason
`macos-passive-auth-unavailable` and leaves the previous account status
unchanged. Python can read passive Keychain authentication through its
Security.framework helper; this slice does not add platform APIs or a new
dependency to reproduce that behavior.

## Provider session task

`provider_session_task::SessionHandle::open(factory, handle, options, config)`
validates the recovered book and the actual initialize command before opening
anything. It returns before attachment startup. The model catalog uses it for
ephemeral metadata requests.

The handle owns `events: mpsc::Receiver<SessionEvent>`,
`status: watch::Receiver<SessionStatus>`, and a cloneable `control: SessionControl`.
Clone control to select admission futures alongside event reception. Control has:

- `prepare(RequestSpec).await -> Result<PreparedRequest, SessionTaskError>`;
- `submit(PreparedRequest).await -> Result<RequestTicket, SessionTaskError>`;
- `prepare_frame(OperationId, &Frame)` and `submit_frame(PreparedFrame).await`
  for callback responses and recognized notifications;
- `settled(EventCursor).await` after the consumer commits durable settlement;
- `cancel()` to signal shutdown independently of every bounded lane.

The account supplies durable, unique request/operation identities, prepares before
committing Submitted, then submits once. `PreparedRequest::deadline_ms()` and
`recovery_record()` expose the original deadline and recovery intent before that
commit. Preparation starts the deadline; persistence or admission delay never
restarts it. The prepared record is intent, not evidence of a provider write.
`AdmissionUnknown` means the admission
receipt was lost; never resubmit. Other admission errors mean no write was admitted.
Accepted requests report Written, NotSent, or Unknown via `SessionOutcome::Write`.
Keep each admission future pinned/owned across unrelated select outcomes. Dropping
its owner before observing the result fences the session, including a buffered
result; ordinary polling by mutable reference does not fence it.

Written request outcomes include `recovery: Some(RecoveredRequest)` containing
`ticket`, `operation_id`, `thread_id`, `lightweight`, `deadline_ms`, `unknown`, and
`remote_id`.
Persist this correlation record with the write outcome; numeric requests need the
remote-to-local ID mapping after reattach. Supply these records through
`SessionTaskConfig.recovered`; the session installs the adapter mappings before
opening. Preserve the original deadline and update `unknown` when durable Unknown
is reported. Recovery does not start a fresh timeout or repeat prior Unknown.
Recovery restores serialized thread ownership; a lightweight request does not
reserve its thread. Conflicting recovered serialized owners are refused before
open. All 32 unresolved requests can be recovered; initialization has a separate
record slot. Merge late Written evidence by request/operation identity, preserving
any durable Unknown flag. A late write receipt must not resurrect a request already
durably resolved; restore only unresolved records.
Deadlines and journal timestamps use the supplied `SessionClock` epoch. Preserve
that epoch across reattachment in a running process; across runtime restart the
account/store layer must translate its durable deadline into the new clock epoch.
A missing remote ID after an uncertain write is still uncertainty, not evidence
of NotSent or permission to resend.

Crash window: Submitted is durable, but Written is not. This includes a crash
before submit reaches the supervisor and a crash after acceptance but before the
consumer persists Written. Restore the prepared record and keep the request Unknown;
never resend it. Without the missing numeric correlation, a replayed reply remains
an orphan envelope for durable reconciliation rather than a guessed local match.
Protocol v1 returns the stored remote ID with `duplicate: true`, without writing
stdin again, when a repeated operation ID has the same payload digest. However,
if that operation ID is absent it accepts and writes the request. Its
`operationStatus` lookup only supports `monitor:` identities, not arbitrary
request operation IDs. Therefore repeating a write is not a safe receipt lookup
for this crash window, and SessionControl exposes no such reconciliation call.

Replies and deadlines are pure `RequestOutcome` decisions. The shared clock in
OpenOptions timestamps arrival before the session consumes a journal entry;
`receive_at` keeps lateness independent of timer/consumer scheduling.

Each `SessionEvent` contains at most one original journal entry (including raw
stdout, stderr, exit, initialization and orphan replies) plus bounded outcomes.
Never settle a cursor just because it was read. Only a contiguous prefix of
consumer-settled entries is ACKed. Control.settled success records that intent;
it does not claim the supervisor already accepted the ACK. The settlement window
is 64 entries, unresolved requests 32, each external lane 16, and pending callbacks 34. Each admitted request reserves its prepared write in its bounded record; callback
capacity cannot prevent a serialized successor from being released. There are two
retained event slots and one bounded pure transition batch. Non-shutdown select
arms are unbiased; ACKs and writes alternate when both are ready, as do request
and callback writes.

Readiness is AwaitingReply, then InitializedWritePending, then Ready only after
the initialized write receipt. The initialized operation is scoped to the actual
child generation. The session owns the one SessionState and its deadline source.

`shutdown().await` on the handle signals stop and joins the adapter before returning
`SessionExit`. `completion().await` joins without initiating stop; cancelling either
wait retains the join owner. Persist `exit.adapter.recovery_needed`, final outcomes,
and `undelivered`; also drain buffered consumer events. After joining the adapter,
the session drains its journal lane into this handoff: at most 17 envelopes (one
retained transition plus 16 buffered entries). Retained control outcomes go in
`exit.outcomes`. Evidence stays task-owned before fallible processing, including
across an injected panic; terminal cleanup does not execute more protocol writes.
Shutdown never ACKs or waits for room in the consumer lane.

A consumer can commit durable settlement and receive `settled` success, then shut
down before the ACK is sent or its receipt is observed. Shutdown can succeed with
this prefix still unacknowledged. `SessionExit.pending_acknowledgement` returns the
highest contiguous locally settled cursor without a confirmed ACK (also after a
lost ACK receipt). Reconcile redelivery against durable settlement on the next
attachment, then authorize that prefix again. Neither local settlement nor a
successful shutdown proves supervisor ACK completion; worker/store crash-window
and durable replay reconciliation tests belong to the account-worker stage.
The adapter's documented blocking-job cleanup limitation still applies. Dropping a handle only signals
cancellation; explicit shutdown/completion is required for the join barrier.

## Account worker

[`start_account_worker`](src/account_worker.rs) owns one account on a Tokio task.
The server must create and share one `StoreBridge` per `RuntimeStore`; its startup
recovery runs before account admission, and its semaphore bounds `spawn_blocking`
SQLite jobs. Account commands, session requests, effects, and deferred journal
entries all have explicit capacity limits. Dropping the account handle signals
sticky cancellation. `shutdown` waits up to `SHUTDOWN_GRACE` for the worker,
then releases account ownership. An already started blocking job retains the
bridge permit until it finishes; replacement recovery queues behind that job.
Other accounts sharing the bridge also wait. Worker reads use the same permit;
independent store readers do not need it. The account registry mutex is released
before recovery or session startup awaits.

Work follows claim → typed preparation → session prepare → durable Submitted →
submit. The submission transaction rechecks obsolete complaints/work notices,
publishes their retirement, and records transcript input and links before any
provider write. A new event for an active turn stays pending and is claimed only
after that turn settles. Journal handling commits lifecycle and notification
writes, replaces the stream buffer, then calls `settled(cursor)`. Terminal
notices and transcript `turnStatus` marks share the lifecycle receipt transaction.
A replay returns saved decisions without repeating transcript or search effects.

Recovery restores only unresolved request records into `RecoveredRequest`.
Submitted without a durable Written receipt stays Unknown, including when no
write reached the provider. It is never resent. Late replies can refine that
state; unknown numeric correlation remains an orphan for later reconciliation.
The bounded shutdown handoff persists delivered/undelivered evidence and adapter
recovery identity. Local settlement does not prove that the supervisor accepted
its ACK; journal receipts make redelivery safe.

The paused-time scripted seam tests in
[`account_worker/tests.rs`](src/account_worker/tests.rs) exercise the actual
session and store bridge. They cover normal completion, pending input during an
active turn, interrupt before the start reply, queued cancellation, provider
exit, generation change, duplicate/stale/out-of-order notifications, deadline
expiry and late replies, full-queue shutdown, and restart between Submitted and
Written. `just test-rust-runtime-corpora` compares Python-written pre-states and
real lifecycle/dispatch transactions and every before/after bridge boundary in
the scripted scenarios. The default suite keeps representative crash witnesses;
full matrix timing separates scenario replay from replacement recovery. The
separately selected
`just test-rust-runtime-corpora-mutations` tests the same wiring against seeded
regressions, including transcript notices and the context exception's boundary.

The bridge rechecks account ownership in claim and submission transactions.
Lifecycle rejection/NotSent settlement retires transport receipts atomically.
Stop and notification interrupts are durable pending effects. Each replacement
recovers proven-unsubmitted claims for its own account, restores the stored
attempt for journal delivery, and defers callback admission until initialization.
Only explicit admission refusal proves NotSent; unrelated failures preserve Unknown.

Acknowledged journal prefixes are compacted into one cursor checkpoint. Unresolved
requests and pending effects remain indexed by account. Journal receipts retain a
digest, not another copy of the provider payload. Stream deltas commit in the same
transaction as cursor eligibility. Their pending text is bounded by
`STREAM_FLUSH_BYTES` in [`store.rs`](src/account_worker/store.rs); canonical
transcript/search rewriting occurs on completion, that threshold, or Python's
`codex_streaming.FLUSH_SECONDS` cadence. Assistant text is never truncated. The
command-output buffer uses Python's character-tail rule; command item dispatch is
still a named gap of the notification service.

A flushed base normally comes from the canonical item. Only an `item/started`
that replaces this visible text needs a temporary base receipt. Its hash and the
base row commit together; the next flush or close deletes the row, and recovery
prunes streams no longer active. No blob store or schema was added. A flush costs
O(total item text); per-delta work handles bounded preview and pending text rather
than copying the accumulated answer. The corpus compares these exceptional bases
to Python's live buffer before excluding the validated internal receipt rows.

The scheduler split itself moved functions without changing their bodies. Later
compatibility corrections are proved by named Python-written cases:
`disconnect_without_start_attempt` covers absent top-level attempts and nested
disconnect identity; `reattach_running` and `reattach_restart` cover nested
accepted attempts and generation lookup. Missing optional policy fields use Null
instead of panicking; the same Python-written cases exercise their absence.
`claim_deleted` covers ordinary deleted rows; `reconcile_resolved_active`,
`reconcile_resolved_idle`, and `reconcile_preserves_selected_progress` cover
retirement metadata and preparation policy. Worker-only rehydration leaves the
repository's exact `reattach_*` corpus output unchanged.

Catalog refreshes share the semaphore in [`model_catalog.rs`](src/model_catalog.rs).
Registry reads prune removed accounts and replaced homes. The service retains
refresh task handles; server shutdown cancels and drains them. Catalog requests
drain session events while awaiting command receipts, preserving bounded-lane
progress. The ephemeral adapter also bounds the child reap after sending kill.

### Known gaps before server wiring

The worker is **not eligible for wiring to a real provider** until start-time
context preparation is implemented. [`preparation.rs`](src/account_worker/preparation.rs)
defines a typed input (event intent plus stored agent and state root) and output
(provider parameters plus prepared submission), currently passing event text
through. Python `Runtime.start` in `codex_runtime.py:7046` reads event metadata,
message clocks, and unanswered complaints before appending context.
`EfficiencyMixin.model_turn_context` in `codex_efficiency.py:715` records the
context manifest after reading known context/epoch, root agent mode, role skill,
progress-file guidance, plan, complaints and finished-worktree reminders.
The registered `context_preparation_unported` class permits only those missing
preparation fields and appended text; base text, input identity, links, search
metadata, notices, terminal state and dispatch writes compare exactly.

Active-turn steer is a separate follow-up requiring a differential corpus against
Python selection/submission policy. Its touch points are scheduler
`selection::claim` (currently refuses in-flight claims), attempt reconciliation,
`SessionControl::prepare/submit` with `Method::TurnSteer`, and lifecycle settlement.
Stage 5 intentionally has no `claim_steer` API.

Preparation currently refuses Claude turns, new threads, assets, pending settings,
and unprepared workspaces instead of guessing their provider parameters. Native
history reconciliation for held generations and numeric orphan replies remains
separate. [`callback.rs`](src/account_worker/callback.rs) is the typed callback seam.
Approvals, questions and dynamic tool calls currently receive a typed unsupported
JSON-RPC error after their journal entry commits. The response has its own
Submitted/Written receipts; its cursor is acknowledged only after Written, and
unknown response writes are never resent. Dynamic tools and approvals must be
served by their real services before a real provider can be wired; explicit
rejection does not make this a complete provider runtime.
The server factory/TurnRuntime caller is not part of this stage.
The old provider process/session/turn-driver thread code and tests are removed;
the model catalog uses the accepted async session and ephemeral stdio adapter.

## Submit gate corpus boundary

`tests/submit_gates` compares event authorization, execution revalidation and the
freeze block against real Python functions or statements compiled from their AST.
The execution fixture reads raw stored JSON: it does not project lead-mode fields
through `Runtime.agent`/`mode_fields`. The compared effort/service-tier values are
the immediate projection after freezing, not evidence of a complete `prepare`
or provider turn. The consumer owns mode projection, preparation, attempt/epoch
checks and the atomic write. Selected execution fields are checked again when
applying a selection, including fixed retries; a concurrent change requires a
new selection. Catalog generation is checked only when admission needed a proof.

A separate Python preparation witness executes the real missing-`nativeEffort`
validation block for an unloaded agent: omission resolves the catalog default
(`medium` in the fixture); explicit null remains null. Callers must finish that
preparation before using the final wire settings. This slice does not port full
preparation or start a provider. Digests of the enclosing `Runtime.start` and
`prepare_locked` ASTs make control-flow drift fail until these extraction
boundaries are re-derived. Budget initialization and history-backed accounting
remain outside the corpus; every present budget retains the named coverage hold.
