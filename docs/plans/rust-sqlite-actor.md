# Rust SQLite actor migration preflight

**Status:** Approved direction; implementation in progress (2026-10-16).
**Audience:** maintainers changing `RuntimeStore` ownership or callers.

## Goal and boundaries

Move the existing `canvas.sqlite3` connection ownership to `tokio-rusqlite`
0.8.0, reusing the workspace's pinned `rusqlite` 0.40.2 (satisfies the
wrapper's `^0.40.1` requirement). `sqlite_actor_store` becomes the sole Canvas
writer owner. Preserve one FULL-synchronous commit per transaction, durable
receipt and exact-request behavior, and unknown outcomes after a lost response.
Do not batch commits, change the database schema, or introduce a second state
owner.

This plan covers the main Canvas database only. `analytics.sqlite3` has a
separate writer purpose and remains separately owned; it must not be attached
to, transacted with, or silently routed through the Canvas actor. Supervisor
journals and other SQLite files also retain their owners.

## Current owner and call path

[`RuntimeStore`](../../workspaces/runtime/packages/runtime-store/src/lib.rs)
acquires the process lock in `open` and owns the `canvas.sqlite3` writer
through `sqlite_actor_store`. `transaction*` runs one synchronous
`rusqlite::Transaction` closure per actor job, commits once, then emits
generation and notifier updates. A failed or lost commit confirmation remains
`CommitOutcomeUnknown`. Writer admission is bounded by the source-owned
[`MAX_PENDING_WRITES`](../../workspaces/runtime/packages/runtime-store/src/sqlite_actor.rs)
constant; its permit remains held while a submitted callback is queued or
running, even if its waiter is cancelled. Reads still use the raw read pool
and a dedicated `version_reader`. The named
[`CANVAS_READER_CONNECTION_CAPACITY`](../../workspaces/runtime/packages/runtime-store/src/lib.rs)
owns the total of four Canvas readers, including the version-reader
reservation; the ordinary pool derives its capacity from that owner. The later
raw-read migration remains separate.

Each public transaction entrypoint boxes its owned SQL callback once, before
returning a small future that borrows the store. The private
`transaction_with_post_commit_boxed` owner retains admission, commit, post-commit,
and unknown-outcome handling. Dropping an unpolled future does not invoke the
callback, and future size does not grow with callback capture size; the
[`transaction_entry_futures_do_not_store_operation_capture` test](../../workspaces/runtime/packages/runtime-store/src/lib.rs)
covers all five entrypoints.

Account workers do not share an operation lock. Each worker runs its own store
operations one at a time, and the single writer actor admits writes in order: a
write admitted for a worker that is cancelled or times out still completes before
any later write, including the startup recovery of a replacement worker for the
same account. Cancellation while waiting for writer admission drops the caller
future and submits no SQL
([`cancellation_while_saturated_does_not_submit_sql`](../../workspaces/runtime/packages/runtime-store/src/sqlite_actor.rs)).

The runtime server opens one `Arc<RuntimeStore>` in
[`runtime-server/src/main.rs`](../../workspaces/runtime/apps/runtime-server/src/main.rs)
and passes it to runtime services and HTTP routes. The writer API is async;
service traits and their real HTTP or background callers must propagate that
boundary. Synchronous SQL helpers that receive a borrowed transaction remain
inside the actor closure. Filesystem, provider, VM, and network work stays
outside that closure. The production caller inventory below is a historical
pre-cutover scan, not a current ownership or completion checklist. The separate
raw-read migration is not in this first slice.

Examples of the real fan-out are
[`terminal_driver.rs`](../../workspaces/runtime/packages/runtime-services/src/terminal_driver.rs),
[`notification_dispatch.rs`](../../workspaces/runtime/packages/runtime-services/src/notification_dispatch.rs),
[`work_write.rs`](../../workspaces/runtime/packages/runtime-services/src/work_write.rs),
and [`routes/messages.rs`](../../workspaces/runtime/packages/runtime-http/src/routes/messages.rs).
Repository methods accept borrowed `Connection` or `Transaction` values, so
they can remain synchronous SQL helpers called inside actor closures. Current
service functions that start or return database work must become async up to
their real HTTP, background-task, or CLI caller.

## First shippable slice

A single route or table family cannot safely take over the writer: every
`RuntimeStore::transaction*` call writes the same `canvas.sqlite3` file. The
first shippable slice is therefore the Canvas writer ownership cutover plus
all production transaction callers. Partial callsite migration is draft work
only; it must not ship with both the old `Mutex<Connection>` writer and the
actor able to write the same file. This slice converts writer APIs only; raw
read APIs remain an explicit later removal phase.

The actor store exposes true async methods with the existing operation names:
`open`, all `transaction*`, `checkpoint_wal*`, and writer-diagnostics
registration. Transaction callbacks remain synchronous `FnOnce` closures with
`Send + 'static` bounds, and return owned `Send + 'static` results. No raw
connection, transaction, read lease, or borrowed row crosses an await boundary. Repository
DTOs remain typed; `serde_json::Value` stays only in existing opaque JSON
fields. A transaction closure cannot await or perform provider, filesystem,
or network effects.

HTTP mutation lifetime ownership is a separate boundary from SQLite writer
ownership. After response-safety and the existing trust, body, and schema
boundary accept an API `POST`, `PUT`, `PATCH`, or `DELETE`, `runtime-http`
admits the request once into its `TaskTracker` before mutation-sync and the
handler. If the client disconnects, dropping the response waiter must not cancel
the admitted operation. The owner must not replay the request, create a new
identity, buffer another body copy, or bypass validation. Cold entity-pull
maintenance is a narrow GET exception: its projection commit and cache mark
remain owned together after the response waiter is dropped. Ordinary read-only
GET, HEAD, and OPTIONS work stays on the request future. Admission and shutdown
close share a short gate so no task can be registered after drain begins. When
the shutdown signal arrives, the server closes mutation admission and the sync
resource hub before awaiting Axum's graceful serve future, so open event streams
can exit instead of consuming the shutdown watchdog. After `serve` resolves, it
drains tracked requests plus durable account login/cancellation work while
account workers and providers remain live, then stops those dependencies.
This graceful drain is bounded by the existing 20-second
`SERVER_SHUTDOWN_GRACE` watchdog; it is not an unconditional completion
guarantee. If the watchdog forces process exit, submitted operations retain
their existing durable-intent, unknown-outcome, receipt, and recovery behavior.
No automatic replay is added. A task panic is logged with its route and request
identity by the task owner and returned through the existing internal error
response contract; response-safety still attaches API headers. Task join
failures are surfaced with context.
The tracker is not a durable queue and does not promise completion after process
termination. Existing receipt-backed retry and unknown-outcome rules remain
authoritative.

### Credential identity file and journal lifetime

[`Credentials`](../../workspaces/runtime/packages/runtime-services/src/federation_credentials.rs)
owns initial native identity creation and journaled local-name renames in its
own `TaskTracker`. Cancellation of a route or response waiter does not cancel
the credential operation: it retains the identity-cache lock through secure
file creation or replacement, notification, and journal settlement/recovery.
The request-ID journal remains authoritative for a rename retry and rejects a
different label under the same ID.

Shutdown closes HTTP mutation admission first. After admitted HTTP requests
drain, `HttpState::shutdown_mutations` closes credential admission; retained
service shutdown then drains the credential `TaskTracker` before stopping its
remaining service dependencies. The existing `SERVER_SHUTDOWN_GRACE` watchdog
bounds this sequence. A forced exit may leave a pending rename journal, which
the existing credential recovery path must finish from the same request ID;
it must not repeat a different rename or discard an uncertain file write.

The source-level cancellation coverage is
[`canceled_initial_identity_creation_finishes_before_later_rename`](../../workspaces/runtime/packages/runtime-services/src/federation_credentials.rs)
and
[`canceled_rename_retains_serialization_until_file_write_and_commit_finish`](../../workspaces/runtime/packages/runtime-services/src/federation_credentials.rs).
The HTTP drain ordering is covered by
[`accepted_mutation_can_start_credentials_after_http_close_and_drains_first`](../../workspaces/runtime/packages/runtime-http/src/router.rs).

Account turn preparation uses a typed immutable prepared view keyed by the exact
agent, epoch, attempt, provider, workspace, catalog, and path identities. Filesystem,
VM, and catalog work runs in an async preparation stage outside the writer actor;
the final synchronous transaction reloads current rows and revalidates those
identities, versions, permissions, and paths before writing the existing receipt
IDs. A stale view or failed precondition commits no submission. Cancellation before
actor admission submits no SQL; cancellation after submission leaves an unknown
outcome resolved through the original receipt without automatic resend. Preserve
the current preparation failpoint boundaries and test each boundary against both
successful commit and stale identity.

The optional Python tool's `PATH` lookup and initial turn preparation/compile run
on the bounded blocking pool before the final writer transaction. `CommitGuard`
retains the resolved optional executable path as part of the preparation guidance
snapshot; the final writer reuses that value and does not resolve `PATH` again.
The final CPU recompile inside the writer remains intentional: it compares the
prepared submission with the current durable identity. Keep that comparison
explicit when describing this path; do not claim all compilation is outside the
writer or remove the identity check as part of the async cutover.

The owner-defined
[`MAX_PENDING_WRITES`](../../workspaces/runtime/packages/runtime-store/src/sqlite_actor.rs)
owns the bound of 16 admitted writer jobs (queued plus running). Acquire its
owned semaphore permit before calling `tokio-rusqlite`, then move that permit
into the actor callback so cancellation of the response future cannot release
capacity while SQL is still queued or running. Admission waits cancellably
before enqueue; a caller cancelled while waiting has submitted no SQL. Once
enqueued, cancellation cannot retract the actor job; the outcome is unknown and receipt-backed retry
must resolve through the original identity and content. Never retry
automatically. The first slice preserves separate `analytics.sqlite3` writer
ownership and the dedicated `version_reader`; the ordinary Canvas reader pool
uses the remainder of the source-owned total-reader budget.

The `RuntimeLock` lease must remain alive for every queued or running actor
callback. Each callback retains its own lease reference until its synchronous
SQL closure returns. After the final callback, dropping the
`tokio-rusqlite` sender leaves only the library-managed native connection-close
tail: no application SQL or callback may run after the lock and identity
leases end. This preserves the previous release boundary, where `RuntimeLock`
was declared before the synchronous connection fields and therefore released
before connection drop; the actor makes that final native close asynchronous.

## Event-driven background ownership

Background services wait on their owning change signal or the next durable
deadline, with bounded retry backoff after failed reads or external work; they
do not use periodic polling as a substitute for invalidation. A table observer
is only an invalidation hint: it fires after a known successful commit or an
ambiguous commit that may have landed. Consumers reread authoritative rows; the
hint is neither a receipt nor permission to replay a mutation.

- The peer-link owner observes `runtime_access_clients`, refreshes its peer
  snapshot on that targeted hint, and also watches child exit. Failed peer
  snapshot reads and failed starts use bounded backoff; a failed peer update
  triggers child restart, and an exited child restarts with bounded backoff.
  Shutdown awaits the child owner, within the existing server shutdown watchdog.
- Provider-version checks wake when the active account/session profile set or
  the `checkProviderUpdates` preference changes. The upstream release cache
  expires after the source-owned 24-hour TTL in
  [`provider_versions.rs`](../../workspaces/runtime/packages/runtime-services/src/provider_versions.rs);
  this TTL is unrelated to credential refresh.
- A turn held by the account auth gate waits for an account or cached auth
  status change, never for a deadline. Active VM Claude credential refresh is
  separate security maintenance, with its cadence owned by
  [`VM_CREDENTIAL_REFRESH_INTERVAL`](../../workspaces/runtime/packages/runtime-services/src/account_worker.rs).
- Continuation and federation schedulers combine commit/change wakes with the
  earliest durable work deadline and per-item retry times. They retain
  backoff after failure rather than treating an unrelated wake as proof that
  the failed read or delivery succeeded.

## Contracts and failure cases

- Open the same state directory and preserve `runtime.lock`, Canvas schema,
  WAL state, and row identities. Actor startup must not create a second store
  or backend.
- Set `synchronous=FULL` on the single Canvas writer connection. Each call
  still performs its own SQLite transaction and commit; no batching or reduced
  durability. Advance commit generation and fire notifiers only after a
  successful commit.
- Preserve immediate-transaction isolation, rollback on closure error,
  `CommitOutcomeUnknown` on failed commit confirmation, and receipt lookup on
  retry. Same request identity plus same content returns its prior result;
  changed content remains a refusal. No command, message, or charge may repeat.
- Preserve the source-owned total Canvas reader limit, including the dedicated
  `version_reader` used for data-version/generation reads. Derive the ordinary
  pool capacity through
  [`READ_CONNECTION_POOL_CAPACITY`](../../workspaces/runtime/packages/runtime-store/src/lib.rs),
  with read-only/query-only mode and fail-fast nested-read behavior. The later
  async raw-read slice must detect a nested logical read before it waits for an
  admission slot; thread-local tracking alone is insufficient once work moves
  between Tokio tasks and actor threads.
- A caller cancelled before admission/submission performs no SQL. A caller
  cancelled after its job enters the actor queue may drop its response receiver,
  but the job remains eligible to run; report an unknown outcome. Receipt-backed
  retries resolve through the original identity and content. For writes without
  a durable receipt, follow their existing recovery path and do not retry
  automatically.
- FULL or other commit failure must not be translated into a definite rollback
  unless SQLite proves rollback. Keep existing explicit unknown-outcome error
  mapping and preserve the committed receipt if the commit succeeded before
  response loss.

## Migration and rollback

1. Keep the approved workspace dependency and make `sqlite_actor_store` the
   only Canvas writer owner. Start it from `RuntimeStore::open`; keep bootstrap,
   recoveries, normal writes, generation
   updates, commit callbacks, checkpoint requests, and diagnostics on that
   owner. Keep analytics separately owned.
2. Complete conversion of every production Canvas writer caller and propagate
   async results to each real route, worker, or CLI entry point. Do
   not retain a second synchronous writer API as a compatibility path. Keep
   the dedicated version reader and enforce the existing total reader budget
   by deriving ordinary pool capacity from the total-reader owner and its reserved
   version-reader slot.
3. Remove the old writer mutex and synchronous write wrappers in the same
   cutover. Retain `analytics.sqlite3` access under its separate writer owner.
4. In a later named slice, migrate raw Canvas reads and snapshots, route
   generation reads through the approved reader owner without deadlock, and
   remove `ReadConnection` and `read()` APIs that expose raw connection leases.
5. A code rollback is allowed only after the actor-backed server is stopped and
   the same state directory is exclusively released. No schema migration or
   new persisted format is part of this slice; verify the old implementation
   can reopen the resulting state before rollout. Never run old and new
   backends against the same state directory.

## Acceptance tests

- A real HTTP write path and its service caller use the actor; one named
  repository/receipt scenario proves same-ID retry does not repeat the effect.
- One writer remains exclusive under concurrent calls; total Canvas reader
  connections stay within the source-owned limit, including generation reads.
  Admission
  saturation follows the named contract and never feeds unbounded queued work
  to `tokio-rusqlite`.
- Cancellation before submission, while queued, and after commit are separate
  cases. Before submission produces no SQL; a queued job may still commit after
  its caller cancels; after commit/lost reply, receipt-backed retry returns the
  original result once and does not repeat the effect.
- Nested reads fail before waiting, read snapshots remain consistent, and
  transactions cannot cross an await or leak a connection.
- Inject closure failure, FULL/disk-full commit failure, and response loss
  around commit. Assert rollback where known, `CommitOutcomeUnknown` where
  uncertain, FULL durability, one commit per request, and unchanged receipt
  semantics.
- Existing cross-runtime schema/state fixtures open in place, and
  `analytics.sqlite3` remains independently accessible while Canvas work is
  queued. The old implementation reopens the post-slice state after the new
  owner is fully stopped.

The actor validation gate passed on 2026-10-16 at commit
[`5f545fdd`](https://github.com/ifdotpy/codex-studio/commit/5f545fdd):
`just test-fast` (6,000 server tests, 63 bridge tests, 2,200 Rust tests, 921 web
tests, 10 tooling tests, and 5 browser tests), strict workspace Clippy with
`-D warnings`, workspace doctests, and strict runtime mypy. Any later delivery
must rerun the required gate against its exact commit head. The tracked [Rust
chat lifecycle verifier](../../workspaces/client/apps/web/scripts/verify-rust-chat-lifecycle.mjs)
also passed once against real state on the `1ed0548b`-equivalent tree; this
limited smoke does not validate the current exact head. Raw-read migration and
targeted invalidation behavior validation remain pending.

## Historical production writer caller inventory

This bounded text census and its file groupings were prepared before the async
cutover. They are retained as migration history, not a current task assignment,
source of completion claims, or AST-exact caller count. Current call chains and
lane ownership must be read from source and the active integration review.

## Cross-crate async propagation owners

The writer cutover changes store calls from synchronous results to awaited
results. Propagate that async boundary through the owning public traits and
their implementations; do not hide it behind `block_on` or a synchronous
proxy. This map names contract owners and the main cross-crate edge, not a
second caller inventory. The historical production writer-file list below is
not a lane assignment.

| Stable contract owner                                                                                                   | Cross-crate propagation edge                                                                       |
| ----------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| `runtime-services::account_session_runtime::AccountSessionPort`                                                         | Runtime-service implementations and `runtime-server` adapters; HTTP session routes await the port. |
| `runtime-services::monitor_driver::MonitorNative`, `rules_tick::RuleMonitor`, and `monitor_runtime_wiring::MonitorPort` | Account-worker and runtime-server monitor adapters, through monitor service tasks.                 |
| `runtime-services::callback_runtime::CallbackReplyPort`                                                                 | Runtime-server callback broker and the callback runtime that persists replies.                     |
| `runtime-services::multi_server_discovery::ServerDiscoveryPort`                                                         | Runtime-server discovery implementation and the `runtime-http` multi-server route.                 |
| `runtime-http::agent_management::NativeContextRepairPort`                                                               | Runtime-server repair adapter through `runtime-services::context_repair`.                          |
| `runtime-services::account_transfer_driver::{TransferTransport, TransferExecutor}`                                      | `AccountTransferExecutor` and the `account_worker` before-start boundary.                          |
| `runtime-services::recovery_history::RecoveryReader` and `recovery_runtime::RetainedHistory`                            | Account-worker recovery adapters and their service callers.                                        |

These interfaces have separate caller lanes. Their listing does not mean all
implementations or callers have passed the integration gate, nor that an
individual lane can be built or shipped before its connected callers are
migrated. Synchronous callbacks that receive a borrowed
`rusqlite::Transaction` remain inside the actor operation and must not await.

| Lane | Domain                           | Production files                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| ---- | -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1    | Account and agent ownership      | `packages/runtime-services/src/account_selection.rs`, `packages/runtime-services/src/account_transfer_driver.rs`, `packages/runtime-services/src/account_worker/store.rs`, `packages/runtime-services/src/agent_management_store.rs`, `packages/runtime-services/src/agent_move.rs`, `packages/runtime-services/src/agents_create/operations.rs`, `packages/runtime-services/src/limit_reset.rs`, `packages/runtime-services/src/claude_auth_runtime.rs`, `packages/runtime-services/src/claude_controls/store.rs`                                                                                                                                                                                                                                                  |
| 2    | Provider callbacks and lifecycle | `packages/runtime-services/src/browser_answer.rs`, `packages/runtime-services/src/callback_runtime.rs`, `packages/runtime-services/src/lifecycle_effects.rs`, `packages/runtime-services/src/managed_child_create.rs`, `packages/runtime-services/src/native_actions.rs`, `packages/runtime-services/src/provider_callbacks/cards.rs`, `packages/runtime-services/src/provider_callbacks/recovery.rs`, `packages/runtime-services/src/provider_callbacks/storage.rs`, `packages/runtime-services/src/thread_fork_runtime.rs`, `packages/runtime-services/src/title_generation.rs`                                                                                                                                                                                   |
| 3    | Work, scheduling, and voice      | `packages/runtime-services/src/questions_write.rs`, `packages/runtime-services/src/radio_runtime.rs`, `packages/runtime-services/src/safety_buffering/executor.rs`, `packages/runtime-services/src/scheduler.rs`, `packages/runtime-services/src/rules_tick.rs`, `packages/runtime-services/src/work_write.rs`, `packages/runtime-services/src/voice.rs`, `packages/runtime-services/src/voice/provider.rs`                                                                                                                                                                                                                                                                                                                                                         |
| 4    | Workspace and history            | `packages/runtime-services/src/checkpoints.rs`, `packages/runtime-services/src/context_repair.rs`, `packages/runtime-services/src/continuation_driver.rs`, `packages/runtime-services/src/payload_store.rs`, `packages/runtime-services/src/project_locations.rs`, `packages/runtime-services/src/projects/operations.rs`, `packages/runtime-services/src/sync_documents.rs`, `packages/runtime-services/src/sync_policy.rs`, `packages/runtime-services/src/transcript.rs`, `packages/runtime-services/src/workspace_cleanup_runtime.rs`, `packages/runtime-services/src/workspace_operations_effects.rs`, `packages/runtime-services/src/workspace_ready_git.rs`                                                                                                  |
| 5    | Federation and multi-server      | `packages/runtime-services/src/federation.rs`, `packages/runtime-services/src/federation_credentials.rs`, `packages/runtime-services/src/federation_delivery.rs`, `packages/runtime-services/src/multi_server.rs`, `packages/runtime-services/src/multi_server_discovery.rs`, `packages/runtime-services/src/multi_server_orchestration.rs`, `packages/runtime-services/src/multi_server_pairing.rs`, `packages/runtime-services/src/peer_conversion.rs`, `packages/runtime-services/src/remote_transport.rs`                                                                                                                                                                                                                                                       |
| 6    | Monitor runtime                  | `packages/runtime-services/src/maintenance.rs`, `packages/runtime-services/src/monitor_driver.rs`, `packages/runtime-services/src/monitor_event_queue.rs`, `packages/runtime-services/src/monitor_output.rs`, `packages/runtime-services/src/monitor_receipts.rs`, `packages/runtime-services/src/monitor_runtime.rs`                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| 7    | HTTP route boundaries            | `packages/runtime-http/src/routes/branches.rs`, `packages/runtime-http/src/routes/catalog_agent.rs`, `packages/runtime-http/src/routes/checkpoints.rs`, `packages/runtime-http/src/routes/mac_sync.rs`, `packages/runtime-http/src/routes/messages.rs`, `packages/runtime-http/src/routes/multi_server.rs`, `packages/runtime-http/src/routes/panels.rs`, `packages/runtime-http/src/routes/projects.rs`, `packages/runtime-http/src/routes/rules.rs`, `packages/runtime-http/src/routes/sync_delta.rs`, `packages/runtime-http/src/routes/sync_documents.rs`, `packages/runtime-http/src/routes/tool_requests.rs`, `packages/runtime-http/src/routes/usage.rs`, `packages/runtime-http/src/routes/work_items.rs`, `packages/runtime-http/src/routes/work_write.rs` |
| 8    | Runtime host and VM              | `apps/runtime-server/src/account_registry.rs`, `apps/runtime-server/src/main.rs`, `apps/runtime-server/src/monitor_port.rs`, `apps/runtime-server/src/multi_server_discovery.rs`, `packages/runtime-services/src/linux_vm_provider.rs`, `packages/runtime-services/src/vm_preparation.rs`                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |

Review each lane's callers before claiming it is independently compilable:
service methods often feed both HTTP routes and background tasks, while a
single service call may in turn invoke a writer through another module. Lane
boundaries prevent overlapping file edits; they do not imply independently
landable commits. Tests-only files are excluded from this initial inventory
and should be updated in a separate test pass after production signatures
settle.

## Related contract

The existing decision [Runtime database commits stay fully durable](../decisions/2026-10-11-runtime-commit-durability.md)
remains in force. The actor changes connection ownership and admission; it does
not change durability or commit boundaries.
