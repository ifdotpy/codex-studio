# FastAPI boundary migration contract

**Audience:** maintainers migrating the local Python HTTP boundary and renderer API.
**Status:** implementation complete; isolated migration evidence verified; live startup against existing state remains unverified.

This page records the compatibility contract and evidence for replacing the
Python `BaseHTTPRequestHandler` boundary with FastAPI. The implementation owns
the mechanically enforced details; route models, code, and tests are the
authority for exact fields and response shapes. The behavior inventory below
was read from the archived legacy handler and checked against the current
FastAPI route table and isolated contract tests. Evidence is local to the
fixtures named below; it does not establish a live deployment or restart.

## Requirements and acceptance contracts

| ID   | Requirement                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             | Acceptance evidence                                                                                                                                                                                              |
| ---- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| R-01 | Replace the Python-to-web request boundary with a real FastAPI application composed from ten domain routers. Each domain owns `scripts/studio_api/<domain>/{__init__.py,models.py,router.py,test_*.py}` and exports `create_router(context: ApiContext) -> APIRouter`.                                                                                                                                                                                                                                                  | `scripts/studio_api/app.py` assembles all ten routers; isolated route and component-discovery checks pass. No generic API dispatcher owns migrated routes.                                                       |
| R-02 | Use strict Pydantic request and response models, closed `StrEnum`/`Literal` values for closed domains, and typed Python modules. Query parameters must be registered with FastAPI route parameters or models so OpenAPI includes them. `JsonValue`/unknown data is limited to documented provider or extensible payload boundaries. Generated OpenAPI and TypeScript types are the shared contract; do not hand-maintain duplicate DTOs, field lists, or schemas.                                                       | Isolated model/schema suites and strict TypeScript/API contract checks pass, including generated query parameter types.                                                                                          |
| R-03 | Preserve methods, paths, statuses, error shapes, null-versus-absent behavior, query parsing behavior, streams, attachments, cache/ETag, compression, and static-asset behavior for supported valid requests, subject to the approved [response encoding exceptions](#approved-response-encoding-contract). Newly rejected invalid types, enum values, and unknown body fields fail safely with HTTP 400 before side effects. Unknown `/api` paths return the existing 404 shape; static serving is not an API fallback. | Route registration, focused HTTP compatibility and boundary tests, and assigned renderer flows pass; the live update guard is separately checked under R-08.                                                     |
| R-04 | Preserve authentication, origin, workspace, token, and remote federation checks at the same effective boundary. Reject invalid input before any stateful or expensive side effect.                                                                                                                                                                                                                                                                                                                                      | Isolated domain and route tests cover trust checks, stale workspace, federation origin, invalid input, and rejection before service effects.                                                                     |
| R-05 | Preserve caller-supplied operation identities and never mint a replacement identity when a caller supplied one. A timeout, disconnect, output validation failure, or lost response never authorizes replay with a new ID. Input validation maps to the existing safe HTTP 400 before side effects; output validation after service execution maps to HTTP 500 with uncertain outcome and never `outcome: not_applied`.                                                                                                  | Retry contract and renderer checks pass for exact-ID recovery, changed-content refusal, and no duplicate message/write after uncertain HTTP responses.                                                           |
| R-06 | Keep SQLite and orchestration writes in their existing service/runtime owners. The HTTP layer validates, authenticates, delegates to those services, and preserves service receipts.                                                                                                                                                                                                                                                                                                                                    | Domain tests exercise existing service and receipt surfaces; persistence remains in the existing runtime, canvas, and sync owners.                                                                               |
| R-07 | Keep runtime state outside the checkout and preserve the existing state-directory identity. Do not start a second backend against an occupied directory or interrupt active agents, monitors, terminals, or waves.                                                                                                                                                                                                                                                                                                      | HTTP/performance fixtures used fresh temporary state. No live backend was started, reset credit redeemed, or active backend restarted for this evidence.                                                         |
| R-08 | Do not apply a source update that changes an already-running legacy HTTP server into FastAPI in place. Deployment of the server architecture change waits for active work to become idle and a planned restart.                                                                                                                                                                                                                                                                                                         | `tests/http-timeout-live-update-contract.py` passes all six isolated cases, including refusing the legacy HTTP patch when the running runtime has no compatible legacy handler. Live restart remains unverified. |

## Field provenance and operation identity

Each request field is classified before implementation. Domain models must
retain this distinction when converting a Pydantic request with
`model_dump(mode="json", exclude_unset=True)`.

| Class                              | Examples                                                                                                                                                         | Boundary rule                                                                                                                                    |
| ---------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| Server-owned                       | Session token; resolved local origin; server workspace identity; request time; authenticated user actor; current runtime/account state; generated error details. | Derive on the server after the relevant trust check. Never accept a renderer value as authority.                                                 |
| Authenticated-context-derived      | `X-Canvas-Workspace` compared with the current sync workspace; request origin and peer identity for federation; authenticated session identity.                  | Verify against current server state before invoking the service. Preserve existing rejection status and text.                                    |
| Stored-state-derived               | Existing operation receipt, account/thread/epoch identity, current transfer state, reset credit provenance, sync generation, database sequence.                  | Read through the existing service and use its conflict/replay checks. Do not reconstruct from request data.                                      |
| Caller-controlled business data    | Requested agent/chat/project/account, action, content, paths, settings, cursor/page choices, explicit flags.                                                     | Validate type, enum, range, cross-field constraints, and exact allowed keys before side effects. Preserve established omitted/null distinctions. |
| Caller-controlled durable identity | `id`, `request_id`, message/event IDs, transfer request IDs, voice event IDs, sync document IDs.                                                                 | Pass the exact identity through unchanged. Validate it before mutation. Never substitute a fresh ID after uncertain outcome.                     |

Known durable identity and receipt boundaries in the current implementation:

| Operation family                                                 | Identity carried by request                                                                   | Authoritative recovery/receipt location                                                                                                                                                                                |
| ---------------------------------------------------------------- | --------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Runtime orchestration tool requests                              | Exact `request_id` from the tool call; request payload remains bound to that ID.              | `runtime_requests` and `runtime_tool_results` in the runtime SQLite database; recover with `orchestration_request`. A timeout is not proof of non-application.                                                         |
| Runtime work actions, user complaints, and runtime chat messages | Exact caller request/message ID (`id` or `request_id`, as currently defined by each service). | `runtime_operation_receipts` stores payload signatures/results; message/event state is in `runtime_chat_messages`, `runtime_events`, and `runtime_event_meta`. Same-ID changed-content conflicts remain service-owned. |
| Native Review/Compact and other native actions                   | `request_id` plus agent/action/context                                                        | `runtime_native_action_receipts` in runtime SQLite, managed by `codex_native_action_receipts`; query the same request ID.                                                                                              |
| User message delivery                                            | Message `id`; each delivery retains its existing target and status                            | Runtime delivery receipts and existing user delivery receipt endpoint; the canvas message mirror is not authoritative for managed delivery.                                                                            |
| Sync pull/push                                                   | Workspace identity, document identities, sequence/generation, and exact draft row identities  | `codex_sync` SQLite tables. Action receipts remain in runtime storage.                                                                                                                                                 |
| Limit reset redemption                                           | `request_id`, account ID, exact reset credit ID                                               | `runtime_limit_reset_attempts` and `runtime_limit_reset_requests` in the runtime database. Preserve attempted credit/account binding and do not redeem a second time after an uncertain response.                      |
| Account transfer and sign-in flows                               | Transfer/login `request_id` (UUID where currently required)                                   | `runtime_account_transfers` in runtime SQLite; Claude login receipt file under the existing state directory. Poll the existing status surface after response loss.                                                     |
| Voice approvals and event records                                | Existing session/event/request IDs                                                            | Voice SQLite tables (`voice_sessions`, `voice_records`, `voice_approvals`, `voice_deliveries`) and runtime answer receipt. Preserve uncertain approval as uncertain.                                                   |
| Terminal writes                                                  | Exact terminal/action request `id`, plus existing operation fields.                           | `canvas.sqlite3` table `user_terminal_receipts`, owned by `codex_terminals`; same-ID payload changes conflict.                                                                                                         |
| Monitors and transcript streams                                  | Existing monitor/agent ID, event ID, and cursor/revision.                                     | `runtime_monitors`, `runtime_events`, `runtime_event_meta`, `runtime_items`, and transcript revision rows in runtime/sync SQLite. Disconnect closes transport only; it does not imply cancellation or replay.          |

### Explicit open JSON payloads

Open JSON values are limited to existing extension boundaries: the shared
`ErrorResponse.details` and `ErrorResponse.metadata` fields; the voice record's
provider/event `payload`; and provider-owned app-server tool schemas/results
that already pass through the native protocol. Signed federation v1 bodies are
passed as raw bytes to `RemoteAccess`/federation protocol validation and are not
an excuse for routers to accept arbitrary body dictionaries. New open fields
require an explicit owner and a documented extension contract.

These locations are a preservation map, not permission to add a new receipt
store. The owning service remains authoritative. For any operation not named
above, the domain implementation must cite its existing identity and receipt
owner in its colocated tests before migration acceptance.

### Retry and timeout decisions

| ID            | Observable contract                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| AC-RETRY-01   | First application receives the original caller identity and records its result in the existing owner.                                                                                                                                                                                                                                                                                                                                                                                                                     |
| AC-RETRY-02   | After losing a successful response, a retry uses the same identity and exact business payload and recovers the prior result without repeating the command, message delivery, redemption, or write.                                                                                                                                                                                                                                                                                                                        |
| AC-RETRY-03   | Reusing an identity with changed business content fails closed with the service's established conflict/error response.                                                                                                                                                                                                                                                                                                                                                                                                    |
| AC-RETRY-04   | Server-owned provenance is refreshed/derived at the authenticated boundary; it is not replaced by client data during replay. Existing receipt checks decide whether refreshed context remains compatible.                                                                                                                                                                                                                                                                                                                 |
| AC-RETRY-05   | A stale workspace, sync generation, expected revision, account/thread/epoch, or other existing base/version continues to fail with the current status/body.                                                                                                                                                                                                                                                                                                                                                               |
| AC-RETRY-06   | Input-model validation finishes before the service call. Response-model validation failure is an output-contract failure only and never sets `not_applied`, manufactures a new request ID, or authorizes automatic retry.                                                                                                                                                                                                                                                                                                 |
| AC-TIMEOUT-01 | Preserve the existing transport behavior: POST body reads use a 10-second socket timeout and sync/transcript streams use a 20-second socket timeout. Ordinary GET has no explicit legacy server socket timeout; the renderer applies a 15-second timeout to bodyless GET requests in `web/src/api.ts`. Core owns FastAPI/Uvicorn timeout configuration and must prove equivalent per-request behavior. Keep service and client timeout settings owned by their current sources; do not widen one in this route migration. |
| AC-TIMEOUT-02 | Disconnecting an HTTP client does not implicitly cancel a command, native call, transfer, reset redemption, or committed sync write. Cancellation requires the existing explicit cancel route and identity.                                                                                                                                                                                                                                                                                                               |
| AC-TIMEOUT-03 | A response loss/restart is recovered by the named existing receipt surface. Unknown outcomes remain unknown until that receipt is read; they are not replayed.                                                                                                                                                                                                                                                                                                                                                            |
| AC-TIMEOUT-04 | Long-running/streaming work preserves prompt durable identity and existing cancellation semantics. Analytics export, sync stream, transcript stream, and terminal output must not be silently converted to buffered JSON.                                                                                                                                                                                                                                                                                                 |
| AC-TIMEOUT-05 | Measure representative payload size and p50/p95/p99 for isolated fixtures before selecting new outer/intermediary timeouts. The legacy fixture baseline below is measured; the same-fixture FastAPI comparison remains pending. Read-only live measurements are permitted but none is claimed here; no user-mutating native/model request or active-backend restart has been performed.                                                                                                                                   |

## Baseline route and behavior inventory

The archived legacy handler implements only `GET` and `POST`. The inventory
below follows its dispatch branches; current route registration is inspected
by `scripts/studio_api/verification/test_app_routes.py`. Query parameters are
deliberately not expanded into a new grammar here: handlers retain first-value
selection and route-specific defaults, with focused tests covering the
behavior the migration relies on.

### GET routes

| Route group                    | Paths                                                                                                                                                                                                                                                                             | Special behavior                                                                                                                                                                    |
| ------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Session and sync               | `/api/session`; `/api/sync/identity`, `/protocol`, `/pull`, `/stream`                                                                                                                                                                                                             | Session returns the token; stream validates query/header protocol versions and may return 426; pull uses exact cursor/scope/reset query semantics; workspace identity gates writes. |
| State and observability        | `/api/state`, `/worktree-disk`, `/costs`, `/session-cost`, `/desktop`, `/diagnostics`                                                                                                                                                                                             | `state?view=chat` excludes work; diagnostics/desktop have runtime-dependent fields; worktree scan and costs may initialize cached readers.                                          |
| Terminal                       | `/api/terminals`, `/terminals/output`                                                                                                                                                                                                                                             | Output supports history mode and offset/limit query values.                                                                                                                         |
| Runtime workspace and settings | `/api/tool-requests`, `/analytics`, `/accounts/claude/login`, `/accounts`, `/projects`, `/questions`, `/workspace`, `/workspace/tasks`, `/work`, `/queue`, `/messages/receipts`, `/changes`, `/plan`, `/checkpoints`, `/capabilities`, `/skills`, `/panel`, `/profiles`, `/rules` | Workspace/plan/checkpoints/capabilities/rules/changes include conditional ETag behavior; analytics accepts query-driven reports and optional export.                                |
| History and search             | `/api/transcript`, `/transcript/page`, `/transcript/item`, `/transcript/search`, `/search`, `/search/item`, `/task`, `/complaint`, `/agent-chat`, `/import`                                                                                                                       | Transcript and sync/terminal streams remain streams; task/chat/import queries preserve current defaults and parsing.                                                                |
| Files and local discovery      | `/api/monitor/log`, `/file-info`, `/file`, `/directories`                                                                                                                                                                                                                         | File content returns base64 JSON; monitor logs preserve their specialized response; directory lookup defaults to current working directory.                                         |
| Limits and model catalog       | `/api/limits`, `/models`                                                                                                                                                                                                                                                          | Cached limit query and worker catalog flag remain distinct; pending catalog has its existing 400 body.                                                                              |
| Canvas chat                    | `/api/messages`                                                                                                                                                                                                                                                                   | Read dispatches to canvas message history, distinct from POST's multi-owner dispatch.                                                                                               |

### POST routes

| Route group                       | Paths                                                                                                                                                                                                                                                                | Special behavior                                                                                                                                                                         |
| --------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Federation protocol               | `/api/federation/v1/pair`, `/status`, `/message`, `/pull`                                                                                                                                                                                                            | Only these paths bypass local write trust; retain the existing `RemoteAccess.request_origin` decision, bounded raw JSON, and existing federation protocol. Do not add a GET counterpart. |
| Sync and voice                    | `/api/sync/drafts`; `/api/voice/{status,start,end,record,records,speech,submit,audio,approvals,approval_speech,approve}`                                                                                                                                             | Draft writes require current workspace identity. Voice has action-specific required/optional fields and audio's larger size bound.                                                       |
| Reset and terminals               | `/api/limits/reset`; `/api/terminals/create`, `/input`, `/resize`, `/rename`, `/close`                                                                                                                                                                               | Reset consumes an exact credit once; terminal writes stay in the terminal manager.                                                                                                       |
| Federation and workspace controls | `/api/federation`, `/panel/layout`, `/peer-teams`, `/projects`, `/chats`, `/connections`                                                                                                                                                                             | Preserve conflict statuses, layout revision semantics, and the canvas/runtime ownership split.                                                                                           |
| Accounts and provider sessions    | `/api/accounts/claude/login`, `/login/code`, `/login/cancel`; `/accounts/discover`, `/register`, `/default`, `/login/cancel`, `/delete`, `/disconnect`, `/reconnect`, `/login`; `/claude/profiles`, `/claude/session`; `/agents/account-transfer`, `/agents/account` | Login/transfer IDs are durable; reconnect and account selection retain current transfer behavior.                                                                                        |
| Runtime work and conversation     | `/api/work`, `/queue`, `/plan`, `/annotation`, `/organization`, `/assets`, `/branch`, `/checkpoint`, `/checkpoint/preview`, `/checkpoint/restore`, `/tool-requests/cancel`, `/profiles`, `/rules`, `/monitor/input`, `/native-command`                               | Asset upload has a 28 MiB body limit; other payloads retain the existing 256 KiB limit except voice audio. Checkpoint/branch route ownership moves to history domain.                    |
| Messages and room management      | `/api/messages`, `/rename`, `/room/delete`, `/complaints`, `/conversation/delete`, `/leads`, `/conversation`, `/agents`, `/configure`                                                                                                                                | `/messages` routes by existing federation/runtime/canvas owner. Complaint and native action IDs preserve exact receipt semantics.                                                        |
| Recovery and execution            | `/api/connection-recovery`, `/context-repair`, `/capacity-retry`, `/usage-resume`, `/action`, `/import`, `/stop`, `/monitor/cancel`, `/questions/delete`, `/questions/defer`, `/answer`                                                                              | Native action validation returns `outcome: not_applied` for specified preflight failures; unknown execution result does not mean not applied.                                            |

All local POSTs require the existing trusted local origin/session token check,
valid JSON content type, bounded body, and—where applicable—matching workspace
header. The generic rejection statuses currently include 400, 403, 404, 409,
413, 415, 426, and 503; route-specific values and error bodies remain the
compatibility authority. Static paths are served only from the existing web
distribution allowlist; a missing root build returns 503.

The FastAPI route table also retains exactly one hidden POST compatibility
pattern, `/api/voice/{action:path}`, for legacy unknown voice actions. It must
stay out of OpenAPI; every other API route remains concrete so generated
clients cannot treat a generic dispatcher as part of the contract.

## Live update and rollout boundary

`scripts/codex_live_updates.py` applies versioned source patches to the existing
runtime and stores its receipt at `<state-directory>/live-update.json` while
the publication manifest is `scripts/studio-live-update.json`. A patch is not a
safe way to change a live server from the legacy handler to FastAPI. The
architecture migration must wait until active agents, monitors, terminals, and
waves are idle, then start through the ordinary supported startup path using
the same state directory. No second backend may open that occupied state.
`codex_source_inventory.source_files` includes nested implementation packages
in source identities and live-update input hashes, while excluding
`test_*.py` and the API-only test packages `studio_api/verification/` and
`studio_api/schema_tests/`.

The existing HTTP timeout live patch inspects `RequestHandlerClass.do_GET`.
It requires a matching legacy handler associated with that runtime and checks
loaded function signatures before comparing source hashes. The patch refuses
when no compatible handler exists; it does not weaken the reviewed-source hash
checks. `tests/http-timeout-live-update-contract.py` verifies this guard with
an isolated FastAPI-shaped runtime and verifies that rejected updates do not
change loaded functions. This is patch-safety evidence, not proof that the new
server has been started against an existing user state directory. The
runtime-load benchmark handler monkeypatch is fixture-only. The
`tests/sync-live-patch-http-contract.py` helper requires `_studio_lp_server` and
`_studio_lp_runtime` injected into `__main__`; it is explicitly excluded from
normal discovery until an owning fixture runner exists. Verify the default
discovery classification with `python3 tests/server/run.py --filter test_run.py`.
There is no in-tree caller that supplies those globals, so no standalone
invocation is valid. Keep it excluded until the fixture owner restores a runner
and documents that runner's command. Other tests should assert HTTP behavior
rather than preserve a legacy dispatch class solely for a source pattern.

## Isolated fixture comparison

Measured 2026-10-04 against the archived legacy handler and current FastAPI
server with a reconstructed, matching Canvas-only fixture. Each source tree
used a fresh temporary state directory with the same `Canvas` initialization;
no `Runtime`, app-server, model process, or live Studio backend was started.
For each route, one warmup was discarded and 300 sequential loopback GETs were
timed end-to-end, each over a new HTTP connection with compression disabled.
The table reports body bytes and p50/p95/p99 in milliseconds. Positive deltas
mean the FastAPI fixture took longer. The earlier legacy-only entry recorded
343 bytes for the state pull; that size was not reproduced by this matched
Canvas-only pair (337 bytes), so the paired measurement below is the comparison
for this fixture. These measurements include local connection setup and do not
establish live-user latency or production impact.

| GET route                                      | Body bytes legacy → FastAPI | p50 ms legacy → FastAPI (Δ) | p95 ms legacy → FastAPI (Δ) | p99 ms legacy → FastAPI (Δ) |
| ---------------------------------------------- | --------------------------: | --------------------------: | --------------------------: | --------------------------: |
| `/api/session`                                 |                     56 → 55 |        0.154 → 0.269 (+75%) |        0.259 → 0.366 (+41%) |        0.449 → 0.623 (+39%) |
| `/api/sync/identity`                           |                     89 → 84 |        0.346 → 0.512 (+48%) |        0.510 → 0.637 (+25%) |        0.675 → 0.834 (+24%) |
| `/api/sync/pull?scope=state&after=0&limit=100` |                   337 → 322 |       0.388 → 1.014 (+161%) |       0.543 → 1.208 (+123%) |       0.609 → 1.592 (+161%) |

The three measured FastAPI percentiles are higher in this fixture; the state
pull shows the largest relative change. This is an isolated-fixture regression
signal for follow-up, not a claim about end-user performance. Raw measurements
and environment details were retained outside the checkout in
`http-fastapi-paired-evidence.json` under the task cache.

### Response allocation optimization

That earlier follow-up retained strict validation of every response and the then-existing JSON
serializer. A bounded cache holds up to 256 response adapters, keyed by model
identity as well as type equality so reordered unions remain distinct. Input
dictionaries are copied only when adding the server-owned sync envelope.
There are no new dependencies, response-data caches, or changes to retry rules.

Against `f331343`, an isolated same-server ABBA comparison (before, after,
after, before) measured 600 requests per route per variant, excluding ten
warmups per block. Each GET opened a new connection with compression disabled;
all compared response bodies were byte-identical.

| Route                                          | p50 ms before → after | p95 ms before → after |
| ---------------------------------------------- | --------------------: | --------------------: |
| `/api/session`                                 |         0.269 → 0.257 |         0.344 → 0.309 |
| `/api/sync/identity`                           |         0.444 → 0.410 |         0.571 → 0.541 |
| `/api/sync/pull?scope=state&after=0&limit=100` |         0.974 → 0.568 |         1.172 → 0.760 |

The state-pull median fell about 42%. The small session/identity differences
varied across runs and should not be treated as established gains. This remains
a Canvas-only fixture measurement, not a live latency or throughput claim.
A warmed 1,000-call union-response microbenchmark reduced the `tracemalloc`
peak from 5,193 to 2,134 bytes; this measures traced temporary memory, not total
allocation count or process RSS, and excludes the retained adapter cache.

The script and raw samples are retained under
`/home/alex/.cache/codex-studio-fastapi/response-perf.py` and
`response-perf-results.json`. Run the script from this worktree with the prepared
Python environment and `PYTHONPATH=scripts`. The 266 backend tests and strict
mypy check of 62 files pass, including warmed-cache rejection, union ordering,
and sync-envelope isolation regressions.

### Three simplification passes after the main merge

The merge keeps main's persisted sidebar order, queue `send_now`, and smart
rename receipts available through the strict API and generated client. Request
identities and optional revision checks follow the actual runtime producers.
Older worker defaults may omit `daybreakEnabled`; task durations may contain
fractional milliseconds. Both shapes remain readable without changing stored
records.

Three follow-up passes preserve response validation and introduce no new
dependencies:

1. Declare sync-pull query fields directly on the route and remove the unused
   dependency model. Raw first-nonempty query parsing and the seven OpenAPI
   parameters remain unchanged.
2. Serialize the sync signature once while holding the existing locks, removing
   intermediate JSON encode/decode pairs and a connection-map copy.
3. Hoist projection limits and allowlists, and omit the first recursive copy of
   agent fields that are immediately rebuilt from those allowlists. Final DTO
   validation, truncation, and output ownership remain unchanged.

These measurements exercise different components and must not be added together
or interpreted as application-wide speedups:

| Component and fixture                                                                 | Before → after                                               | Interpretation                                                                                                                                                     |
| ------------------------------------------------------------------------------------- | ------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Sync pull, localhost HTTP, 100 documents, 270 measured requests per variant           | median 0.829 → 0.813 ms; p95 1.317 → 1.384 ms                | Mixed result; no established latency gain. The unused dependency and model are removed.                                                                            |
| Actual sync-signature callback, 24 accounts × 40 limit buckets, 7 batches × 300 calls | median 2.557 → 0.911 ms; traced peak 853,768 → 180,693 bytes | About 64% less callback time and 79% less peak traced temporary memory in this fixture. Signatures are byte-identical.                                             |
| Actual agent projection, 250 records, 10 paired rounds after 3 warmups                | median 98.099 → 49.724 ms per batch                          | About 49% less projection time. Timing excludes allocation tracing; a separate traced batch peaks at about 19.19 MB in both variants. Output values match exactly. |

The query measurement alternates requests between two isolated Uvicorn servers
and uses persistent HTTPX clients. The callback comparison loads the original
and optimized `ApiContext.sync` code with the same fixture, including lock-busy
checks. The projection comparison alternates order between the pre-optimization
merge `371177f` and `4fbc739`; it retains the output batch during the separate
memory sample. Traced peaks are not allocation counts or process RSS.

Local-only reproduction scripts and raw samples are retained in the task cache
`/home/alex/.cache/codex-studio-fastapi`: `sync-pull-query-benchmark.py` with
`sync-pull-query-benchmark-10f22b2.json`, `signature-perf.py` with
`signature-perf-results.json`, and `projection-perf.py` with
`projection-perf-results.json`. These cache artifacts are not shipped in Git
and are unavailable in a fresh checkout. In the original task environment, set
`PYTHONPATH=scripts` and a writable `TMPDIR`, then run each script with the
prepared API Python interpreter. The in-repository projection contract also
supports `python tests/sync-entity-project-allocation-contract.py --benchmark`
for a single-revision diagnostic; its timed loop includes `tracemalloc` and is
not directly comparable to the untraced paired timing above.

### Snapshot cache correctness follow-up

Project-tree verification exposed a race where an older SQLite read transaction
could populate the shared agent cache under a newer process revision. The cache
now uses an agent-record revision maintained in the same SQLite transaction as
inserts, updates, and deletes. Only explicit read-only transactions share cached
rows; writable and autocommit reads bypass that cache. The cache remains bounded
to four revisions and preserves reuse across unchanged read transactions.
A dedicated initialization lock also keeps the first provider-version status
read from waiting on the runtime writer lock.

`tests/runtime-read-lock-contract.py` covers stale-snapshot poisoning, snapshot
consistency, rollback, shared-cache reuse, copied-record isolation, autocommit
bypass, and actual API snapshot reads while the runtime lock is held.
An isolated WAL write benchmark (8,000 one-row commits, six samples, synchronous
FULL) measured medians of 7.099 s without the revision triggers and 7.029 s with
them. This fsync-dominated result shows no measurable difference in that fixture;
it does not establish zero CPU overhead. Its script and results are local-only
artifacts in the task cache's `tests-tmp` directory:
`runtime-agent-cache-write-benchmark.py` and
`runtime-agent-cache-write-benchmark-results.txt`.

### Three further passes: session dispatch, identity reads, validation adapters

The next three passes compare `cbb8d67` with `645cea8`:

1. The in-memory session route runs asynchronously and sends its token mapping
   directly through the existing strict response adapter. This removes threadpool
   dispatch and a model-to-dictionary round trip without changing response bytes,
   headers, or the registered response schema.
2. Once sync version tracking has initialized its existing SQLite reader, identity
   queries reuse that reader under its existing lock. Each request still queries
   the committed identity; there is no identity-value cache or new connection pool.
   Before initialization, the old connection path remains. Legacy streaming calls
   offload identity reads to the existing threadpool, and oversized events reuse
   the identity already read for that event.
3. Draft assumed-state JSON and receipt ID queries reuse two module-level
   validation adapters. Per-request strict validation, original error locations,
   and draft identity checks remain unchanged. Repeated draft payload parsing is
   retained to preserve those validation boundaries without added machinery.

A three-way localhost TCP comparison used isolated Canvas-only servers, fresh
connections, uncompressed responses, and six blocks in legacy/before/after/after/
before/legacy order. Each block had 20 warmups and 500 measured requests per route,
for 1,000 samples per variant and route. Identity was measured after a normal
state pull initialized the version reader. The legacy source was `6212c10`.
These are small synthetic responses, not production latency measurements.

| Route                        | Legacy p50 / p95 (ms) | Before p50 / p95 (ms) | After p50 / p95 (ms) |
| ---------------------------- | --------------------- | --------------------- | -------------------- |
| Session                      | 0.2459 / 0.3942       | 0.2162 / 0.3107       | 0.1764 / 0.2818      |
| Identity, initialized reader | 0.4894 / 0.6121       | 0.3756 / 0.5115       | 0.3032 / 0.3322      |
| State pull                   | 0.5279 / 0.7306       | 0.4871 / 0.6640       | 0.5606 / 0.6912      |

Session and initialized identity medians fell about 18% and 19% relative to the
previous FastAPI revision. State-pull results varied between blocks and the pooled
median increased in this run; no state-pull speedup is claimed. Legacy response
sizes differ from the typed API, so this comparison does not assert byte equality
between implementations. Before/after GET response sizes match.

An additional state-pull-only comparison used eight alternating blocks
(before/after/after/before/after/before/before/after), giving 2,000 samples per
variant. Medians were 0.5495 versus 0.5647 ms; p95 was 0.7434 versus 0.6546 ms.
This mixed result also does not establish a latency improvement. The measured
state-pull path does not call the changed identity helper.

A separate real TCP/SQLite write benchmark updated 100 drafts per request, each
with the exact prior assumed state. Four before/after/after/before blocks each
used one seed request, four warmups, and 50 measured updates, for 100 samples per
variant. Every response was successful with no conflicts. Median full-request
time fell from 31.699 to 6.665 ms (about 79% less time, 4.8x faster); p95 fell from
34.052 to 7.068 ms. Each final batch request was 49,930 bytes. This includes the
middleware, validation, and SQLite write transaction in a temporary Canvas DB;
it does not include a model or the full runtime.

The supporting draft-only microbenchmark uses different input: 100 rows,
42,840 bytes, 25 samples after four warmups. DTO validation took 25.185 versus
0.521 ms median. An in-process TestClient with a stub store took 27.058 versus
1.652 ms; this is not TCP or SQLite evidence and must not be mixed with the
full-request measurement above.

Local-only scripts and raw samples are retained in the same task cache described
above: `round2-http-perf.py` / `round2-http-perf-results.json`,
`round2-drafts-http-perf.py` / `round2-drafts-http-perf-results.json`,
`round2-pull-confirmation.py` / `round2-pull-confirmation-results.json`, and the
`pass6-validation-20261004-1151/bench-corrected*` artifacts. They are not shipped
in Git or available from a fresh checkout. The HTTP scripts capture the tested
revision and use separate temporary state directories; run them only in the
original task environment with its prepared API Python and writable `TMPDIR`.

Integrated verification passes 281 API tests, strict mypy for 64 files, the
three read-latency contracts, 19 SyncStore tests, sync HTTP/SSE and draft contracts,
API generation freshness, strict frontend build, generated type fixtures, and
staged-content hook fixtures. Independent review covers all three changes and
their validation-error and event-loop corrections.

## Compatibility with main `10f2048`

The subsequent main integration retains worktree preparation states in the typed
agent projection and UI. Snapshot-only models now describe start holds, recovery
outcomes, checkpoint errors, budget accounting modes, and supervisor identity; diagnostics attempts reuse
the same supervisor identity model. These explicit fields preserve the raw
snapshot/diagnostics producer shapes without broadening the durable sync payload
or disabling strict validation. The active-account UI uses the producer's
`archived` flag and handles nullable statuses.

The integrated tree passes 285 API tests, strict mypy for 64 files, generated API
freshness, the frontend build/type fixtures, and 233 frontend unit tests (one
skipped). Runtime (65), read-lock (7), worktree-disk (17, one skipped), preparation
(21), recovery (35), token-rate (28), lock-owner (2), scheduler (1), and startup
receipt selection (3) contracts also pass. Account-limit dots, team token rates,
and the individual token-rate browser scenarios pass. The latter waits for the
new SSE sample and matching conversation meter before checking tool-gap stability.
The benchmark values above remain
measurements of `645cea8`, before this main integration.

The supervisor suite required a local harness to relocate its hardcoded `/tmp`
fixtures to a private short `/var/tmp` directory because the original location
exceeded its user quota. The relocated run had 40 passes, one platform skip, and
two failures. Both failing tests also fail individually on clean main `10f2048`:
`test_legacy_launch_rejects_account_changes_and_unverified_pid` mocks the primary
identity probe but the legacy `ps` fallback still verifies the process;
`test_runtime_restart_reattaches_turn_and_replays_buffered_events_once` expects a
synthetic monitor to remain running although restart marks its unreconstructable
RPC future lost. These are recorded as existing-main fixture failures; the
supervisor suite is not claimed to pass.

## State measurements before direct encoding

Two state optimizations are integrated at `d4dfeb6`, compared with `29f678d`.
The existing two-second, generation-invalidated legacy state cache now retains
canonical encoded JSON and its digest instead of the raw object. Unchanged pulls
reuse that representation. Snapshot assembly computes agent membership once and
selects worker result files in one pass; chat snapshots stream histories and keep
only each worker's winning file. Canvas room metadata uses one SQL statement,
with correlated per-room subqueries still present. Entity initialization also
keeps chat nodes out of the agent fallback collection.

The following measurements use isolated localhost TCP servers and temporary
SQLite databases, fresh connections, uncompressed responses, and before/after/
after/before blocks. Each route has ten warmups and 150 samples per block,
300 samples per variant. Runtime scheduling is disabled and native model calls
are replaced by the existing FakeServer. These are fixture results, not live
user latency. These earlier measurements cover only the first two integrated
optimizations; direct JSON encoding is excluded from this comparison.

| Fixture and route               | Before p50 / p95 (ms) | After p50 / p95 (ms) |
| ------------------------------- | --------------------- | -------------------- |
| Empty `/api/state`              | 0.4973 / 0.5341       | 0.4309 / 0.6520      |
| Graph `/api/state`              | 3.3933 / 3.6446       | 3.4761 / 3.5656      |
| Runtime `/api/state`            | 9.2055 / 10.0028      | 8.8563 / 9.3096      |
| Runtime full legacy pull        | 3.9966 / 4.2124       | 1.5082 / 1.6073      |
| Runtime unchanged legacy pull   | 3.4829 / 3.5179       | 0.8250 / 0.9628      |
| Runtime entity reset page       | 3.1558 / 3.1965       | 2.9352 / 3.0576      |
| Populated `/api/state`          | 18.8643 / 20.1209     | 16.4674 / 19.5320    |
| Populated full legacy pull      | 5.6642 / 5.8474       | 1.7304 / 1.8815      |
| Populated unchanged legacy pull | 4.5578 / 4.9502       | 0.8794 / 0.9760      |

Graph contains 100 registered agents and 50 rooms with ten messages each.
Runtime contains 101 runtime agents. Populated contains 100 runtime agents,
1,200 tasks, 300 monitors, 120 requests, 300 work records, 120 rules, and 50 rooms
with ten messages each. Synthetic task records include their required command
kind, and the populated snapshot is validated before serving requests. Legacy
pulls use `scope=state`; full pulls request `after=0`, while unchanged pulls use
the primed checkpoint. The entity route requests `state:entities:v1`, reset
support and a 100-document page. Graph and populated entity timings are omitted:
the baseline incorrectly seeds chat nodes as agents and returns 400. The new
seed regression checks successful chat-only classification and orphan agents.

The main gain is in repeated legacy pulls. Populated `/api/state` median falls
about 13%, while graph state shows no gain and empty-state p95 increases. The
results do not establish a uniform improvement across workloads. An independent
method-only assembly fixture had byte-identical normalized snapshots and a
6.793 to 4.908 ms median; it excludes HTTP validation and encoding. With 9.8 MB
of result text and checks, its chat snapshot peak traced allocation increased
slightly from 591,843 to 648,312 bytes; memory reduction is not claimed.

Scripts and raw samples are local-only in the task cache, unavailable from a
fresh checkout: `state-round3-populated-http.py`,
`state-round3-two-iterations-http.json`, and
`tests-tmp/bench-state-snapshot-populated.py` with `populated-v3-*` reports.
The standalone store microbenchmark (`state-read-bench.py`,
`state-read-before.json`, `state-read-after.json`) uses a different synthetic
6.6 MB payload: unchanged pull median 7.774 to 0.050 ms. That number excludes
HTTP and must not be substituted for the full-request results above.

At `d4dfeb6`, integrated checks pass 285 API tests, strict mypy for 64 files,
27 Canvas contracts, seven runtime read-lock contracts, two state-seed/result
selection regressions, and the sync entity contract. Independent review covers
cache freshness, read-snapshot coherence, result-file ordering, room projection,
and bounded history retention. Canvas fixtures still emit existing shutdown
thread/file-descriptor warnings despite passing their assertions.

## Approved response encoding contract

On 2026-10-04 the user approved replacing the intermediate JSON-mode Python
tree and standard-library JSON encoder with direct Pydantic JSON encoding,
after the existing strict response validation. The change is integrated at
`cf505bf`; integrated measurements are recorded separately below. This decision explicitly narrows the representation
and numeric-error compatibility promise in R-03; it does not relax request
validation, authorization, or uncertain-write recovery.

The accepted differences are:

- Finite floats may use another equivalent JSON spelling, such as `1e-7`
  instead of `1e-07`. Strong ETags remain derived from the representation but
  may therefore change. A cached older representation receives 200 once, then
  its replacement validator can receive 304 normally.
- Non-finite values in ordinary float fields serialize as `null`, replacing
  the former nonstandard `NaN`, `Infinity`, and `-Infinity` tokens. The custom
  `JsonValue` boundary continues to reject non-finite values during validation.
- Integers beyond Python's configured string-conversion digit limit can be
  returned successfully instead of causing the former serialization error 500.
  This does not promise exact representation of huge integers in browser JavaScript.

Aliases, omitted-versus-null fields, ordinary values, Unicode, schema generation,
security headers, gzip, and response validation remain in scope. Weak validators
continue to ignore their explicitly excluded fields and recursively ignore object
key order. Their canonical JSON bytes now use the same encoder, so existing weak
validators may also refresh once; identity and gzip responses keep the same weak
validator. Canonicalization failures use the existing protected 500 response path.
No storage migration or new
dependency is introduced. These exceptions apply at the response encoding
boundary; calculations and persisted values are unchanged.

The alternative was to retain the standard encoder, or add compatibility guards
to direct encoding. An isolated prototype of the guarded path added a full-body
scan and increased populated `/api/state` median from 8.932 to 15.501 ms.
The unguarded prototype measured 8.968 to 7.825 ms and reduced temporary
serialization allocations; those figures precede integration and are not an
additional end-to-end gain to add to the state measurements above. The accepted
choice is the direct encoder with the explicit exceptions, without that guard
layer. The implementation remains reversible by restoring the previous response
encoding path; affected cache validators refresh normally after either change.

## Integrated three-iteration state measurements

The final isolated TCP comparison measures `29f678d` against `cf505bf`, including
encoded snapshot reuse, snapshot assembly, and direct response encoding. It uses
the same four fixture definitions and ABBA method as the earlier two-iteration
comparison: ten warmups and 150 measured requests per route/block, 300 per variant.
Each request uses a fresh loopback connection with identity encoding. Source
revisions, response sizes, per-block measurements and raw samples are retained
in `state-round3-final-http.json` beside `state-round3-populated-http.py` in the
local task cache. These artifacts are not available from a fresh checkout.

| Fixture and route               | Before p50 / p95 (ms) | After p50 / p95 (ms) |
| ------------------------------- | --------------------- | -------------------- |
| Empty `/api/state`              | 0.3523 / 0.5279       | 0.4800 / 0.5210      |
| Empty full legacy pull          | 0.4585 / 0.7477       | 0.5577 / 0.7019      |
| Empty unchanged legacy pull     | 0.4508 / 0.5895       | 0.5945 / 0.6909      |
| Empty entity reset page         | 0.5416 / 0.7076       | 0.7221 / 0.8042      |
| Graph `/api/state`              | 3.4636 / 3.5961       | 3.1868 / 3.2915      |
| Runtime `/api/state`            | 9.0328 / 10.2995      | 7.6705 / 8.2770      |
| Runtime full legacy pull        | 3.9122 / 4.0257       | 1.3899 / 1.7957      |
| Runtime unchanged legacy pull   | 3.3804 / 3.4363       | 0.9459 / 1.0258      |
| Runtime entity reset page       | 3.0616 / 3.1232       | 2.9480 / 2.9906      |
| Populated `/api/state`          | 18.6770 / 20.4052     | 14.6051 / 16.1625    |
| Populated full legacy pull      | 5.6730 / 5.8277       | 1.4563 / 1.5232      |
| Populated unchanged legacy pull | 4.5850 / 4.6803       | 0.9098 / 0.9596      |

Populated `/api/state` median falls about 22%, full legacy pull is about 3.9x
faster, and unchanged legacy pull about 5x faster. Runtime-only state improves
about 15%. These gains are measured together and are not sums of the individual
prototype gains. Empty-response medians increase in this run; the benefit is
workload-dependent. Graph/populated entity reset pages remain excluded because
the original revision rejects their chat seed, as explained above.

An empty-only confirmation used eight balanced before/after blocks with 300
samples per block, 1,200 per variant/route, retained in
`state-round3-empty-confirmation.json`. Empty `/api/state` remained slower:
p50 0.3778 to 0.4714 ms, p95 0.5445 to 0.5707 ms. Full legacy pull instead
improved from 0.6099 to 0.4687 ms median, unchanged pull from 0.6057 to 0.4829 ms,
and entity reset was effectively unchanged at 0.6558 versus 0.6554 ms.
Thus the broad empty-route regression in the first run is not stable across
routes; the approximately 0.09 ms empty-state regression remains a measured
limitation. No uniform latency improvement is claimed.

Final integrated verification passes 289 API tests, strict mypy for 64 files,
four sync read-latency tests, sync HTTP and protocol SSE contracts, nine mobile
state contracts, seven runtime read-lock tests, two state-seed/result-selection
tests, generated API freshness and frontend contract-type fixtures, and the
staged-content hook fixtures. Existing fixture shutdown/file-descriptor warnings
remain. No live backend, occupied state directory, model invocation, or reset
credit was used. Independent source review found no remaining issue after the
weak-ETag oversized-integer correction and reuse of the existing adapter cache.

## Final recovery integration with main `1f4bc09`

Full snapshots retain `connectionRecovery`, `lastContextRepairCheck`, and
`lastContextRepairWait` as named JSON-object receipts. They remain outside the
durable agent sync projection; chat snapshots still omit the two context-repair
receipts. The connection-recovery HTTP contract exercises actual recovery and
then both full and chat `/api/state`, preventing strict response-validation 500s
when these persisted receipts are present.

## Subsequent simplification audit

The audit at `d78152e` → `effe925` removed duplicate request helpers,
inherited queue fields, repeated offline schema normalization, and identical
exception handlers. Strict validation, request identities, query selection,
null/omission behavior, and generated TypeScript remain unchanged. Production
source is 31 lines shorter; tests add 41 lines, so code plus tests grows by ten
lines before documentation. New model inheritance for a few shared fields was
rejected as extra complexity. No material contract/dependency pivot was adopted.

All 292 API tests, strict mypy on 66 files, API freshness, frontend contract-type
checks, and pre-commit fixtures pass; independent source review has no findings.
Sequential local TCP ABBA runs (300 samples per variant/route across the four
fixtures above) measured populated state p50 14.5930 → 14.5252 ms. Results across
routes are mixed: populated full pull is 1.4088 → 1.4752 ms. An eight-block ASGI
fixture confirmation (1,600 samples per variant) measured GET capabilities
p50/p95 0.4070/0.4727 → 0.4052/0.4553 ms and POST agents
0.3326/0.4117 → 0.3346/0.4032 ms. These are not live-user latency measurements.

Empty-state TCP medians initially rose by about 0.053 ms with flat p95; a
same-source control had a flat median but p95 moved 0.5499 → 0.5694 ms.
Exporting both compared revisions changed
the result to 0.4867 → 0.3893 ms. No changed success-path code was identified.
This does not establish a stable code-induced regression or speedup; all runs
are retained, without selecting only favorable results. Local scripts and raw
artifacts are in the earlier cache directory under `simplify-round-http.py`,
`simplify-final-http.json`, `simplify-requests-abba.py`, and
`simplify-{requests-confirmation,empty-confirmation,empty-identical-control,empty-export-pair}.json`.

## Implementation evidence ledger

| Area                                              | Evidence state                                                                                                                                                                                                                                                                                            |
| ------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| FastAPI app and ten routers                       | `scripts/studio_api/app.py` assembles the ten domain routers; isolated app-route verification passes.                                                                                                                                                                                                     |
| Models, OpenAPI, generated TypeScript             | Lead-reported local API checks, strict TypeScript, and generated contract-type checks pass. The initial API model suite and mypy run preceded the numeric analytics-rate correction; the latest 20-test insights suite and full build include that correction.                                            |
| HTTP routes, auth, and caller integration         | Lead-reported API/full-build checks pass. Assigned browser flows pass for workspace/sidebar/project/inbox/task feed (5/5), limits (28 cases), terminal/navigation/worker model (8 fixtures), message metadata, markdown images, skill autocomplete, and draft/sync/message retry.                         |
| Exact request identity and retry                  | Isolated service/API and renderer evidence passes same-ID recovery and no-duplicate retry scenarios. A response loss remains an uncertain outcome until the existing receipt surface is read.                                                                                                             |
| Architecture live-update guard                    | `tests/http-timeout-live-update-contract.py`: 6/6 pass. FastAPI without a compatible legacy handler rejects the legacy handler patch before mutation.                                                                                                                                                     |
| Isolated performance                              | Initial migration and later optimization fixtures are reported separately above. Session, identity and draft measurements precede the separate state round, which improves repeated legacy pulls and populated snapshots. Workload-specific limits are recorded above; there is no live-user measurement. |
| Scoped repository checks                          | Lead-reported changed-file lint/format, hook fixture, API check, and current-revision full build pass. Whole-repository lint had seven existing warnings across two unchanged files; format reported 38 unchanged files.                                                                                  |
| Startup against existing state after idle/restart | Not run. Existing occupied state, active work, and restart safety were not inspected or changed; no live-startup claim is made.                                                                                                                                                                           |
