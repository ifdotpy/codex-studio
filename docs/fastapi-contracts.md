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

| ID   | Requirement                                                                                                                                                                                                                                                                                                                                                                                                                                                       | Acceptance evidence                                                                                                                                                                                              |
| ---- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| R-01 | Replace the Python-to-web request boundary with a real FastAPI application composed from ten domain routers. Each domain owns `scripts/studio_api/<domain>/{__init__.py,models.py,router.py,test_*.py}` and exports `create_router(context: ApiContext) -> APIRouter`.                                                                                                                                                                                            | `scripts/studio_api/app.py` assembles all ten routers; isolated route and component-discovery checks pass. No generic API dispatcher owns migrated routes.                                                       |
| R-02 | Use strict Pydantic request and response models, closed `StrEnum`/`Literal` values for closed domains, and typed Python modules. Query parameters must be registered with FastAPI route parameters or models so OpenAPI includes them. `JsonValue`/unknown data is limited to documented provider or extensible payload boundaries. Generated OpenAPI and TypeScript types are the shared contract; do not hand-maintain duplicate DTOs, field lists, or schemas. | Isolated model/schema suites and strict TypeScript/API contract checks pass, including generated query parameter types.                                                                                          |
| R-03 | Preserve methods, paths, statuses, error shapes, null-versus-absent behavior, query parsing behavior, streams, attachments, cache/ETag, compression, and static-asset behavior for supported valid requests. Newly rejected invalid types, enum values, and unknown body fields fail safely with HTTP 400 before side effects. Unknown `/api` paths return the existing 404 shape; static serving is not an API fallback.                                         | Route registration, focused HTTP compatibility and boundary tests, and assigned renderer flows pass; the live update guard is separately checked under R-08.                                                     |
| R-04 | Preserve authentication, origin, workspace, token, and remote federation checks at the same effective boundary. Reject invalid input before any stateful or expensive side effect.                                                                                                                                                                                                                                                                                | Isolated domain and route tests cover trust checks, stale workspace, federation origin, invalid input, and rejection before service effects.                                                                     |
| R-05 | Preserve caller-supplied operation identities and never mint a replacement identity when a caller supplied one. A timeout, disconnect, output validation failure, or lost response never authorizes replay with a new ID. Input validation maps to the existing safe HTTP 400 before side effects; output validation after service execution maps to HTTP 500 with uncertain outcome and never `outcome: not_applied`.                                            | Retry contract and renderer checks pass for exact-ID recovery, changed-content refusal, and no duplicate message/write after uncertain HTTP responses.                                                           |
| R-06 | Keep SQLite and orchestration writes in their existing service/runtime owners. The HTTP layer validates, authenticates, delegates to those services, and preserves service receipts.                                                                                                                                                                                                                                                                              | Domain tests exercise existing service and receipt surfaces; persistence remains in the existing runtime, canvas, and sync owners.                                                                               |
| R-07 | Keep runtime state outside the checkout and preserve the existing state-directory identity. Do not start a second backend against an occupied directory or interrupt active agents, monitors, terminals, or waves.                                                                                                                                                                                                                                                | HTTP/performance fixtures used fresh temporary state. No live backend was started, reset credit redeemed, or active backend restarted for this evidence.                                                         |
| R-08 | Do not apply a source update that changes an already-running legacy HTTP server into FastAPI in place. Deployment of the server architecture change waits for active work to become idle and a planned restart.                                                                                                                                                                                                                                                   | `tests/http-timeout-live-update-contract.py` passes all six isolated cases, including refusing the legacy HTTP patch when the running runtime has no compatible legacy handler. Live restart remains unverified. |

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
| Session and sync               | `/api/session`; `/api/sync/identity`, `/protocol`, `/pull`, `/generations`, `/stream`                                                                                                                                                                                             | Session returns the token; stream validates query/header protocol versions and may return 426; pull uses exact cursor/scope/reset query semantics; workspace identity gates writes. |
| State and observability        | `/api/state`, `/worktree-disk`, `/costs`, `/session-cost`, `/desktop`, `/diagnostics`                                                                                                                                                                                             | `state?view=chat` excludes work; diagnostics/desktop have runtime-dependent fields; worktree scan and costs may initialize cached readers.                                          |
| Terminal                       | `/api/terminals`, `/terminals/output`                                                                                                                                                                                                                                             | Output supports history mode and offset/limit query values.                                                                                                                         |
| Runtime workspace and settings | `/api/tool-requests`, `/analytics`, `/accounts/claude/login`, `/accounts`, `/projects`, `/questions`, `/workspace`, `/workspace/tasks`, `/work`, `/queue`, `/messages/receipts`, `/changes`, `/plan`, `/checkpoints`, `/capabilities`, `/skills`, `/panel`, `/profiles`, `/rules` | Workspace/plan/checkpoints/capabilities/rules/changes include conditional ETag behavior; analytics accepts query-driven reports and optional export.                                |
| History and search             | `/api/transcript`, `/transcript/page`, `/transcript/item`, `/transcript/search`, `/transcript/stream`, `/search`, `/search/item`, `/task`, `/complaint`, `/agent-chat`, `/import`                                                                                                 | Transcript and sync/terminal streams remain streams; task/chat/import queries preserve current defaults and parsing.                                                                |
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

A follow-up retains strict validation of every response and the existing JSON
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

## Implementation evidence ledger

| Area                                              | Evidence state                                                                                                                                                                                                                                                                    |
| ------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| FastAPI app and ten routers                       | `scripts/studio_api/app.py` assembles the ten domain routers; isolated app-route verification passes.                                                                                                                                                                             |
| Models, OpenAPI, generated TypeScript             | Lead-reported local API checks, strict TypeScript, and generated contract-type checks pass. The initial API model suite and mypy run preceded the numeric analytics-rate correction; the latest 20-test insights suite and full build include that correction.                    |
| HTTP routes, auth, and caller integration         | Lead-reported API/full-build checks pass. Assigned browser flows pass for workspace/sidebar/project/inbox/task feed (5/5), limits (28 cases), terminal/navigation/worker model (8 fixtures), message metadata, markdown images, skill autocomplete, and draft/sync/message retry. |
| Exact request identity and retry                  | Isolated service/API and renderer evidence passes same-ID recovery and no-duplicate retry scenarios. A response loss remains an uncertain outcome until the existing receipt surface is read.                                                                                     |
| Architecture live-update guard                    | `tests/http-timeout-live-update-contract.py`: 6/6 pass. FastAPI without a compatible legacy handler rejects the legacy handler patch before mutation.                                                                                                                             |
| Isolated performance                              | Paired Canvas-only legacy/FastAPI fixture measured above. FastAPI p50 increased on all three routes; there is no live-user measurement.                                                                                                                                           |
| Scoped repository checks                          | Lead-reported changed-file lint/format, hook fixture, API check, and current-revision full build pass. Whole-repository lint had seven existing warnings across two unchanged files; format reported 38 unchanged files.                                                          |
| Startup against existing state after idle/restart | Not run. Existing occupied state, active work, and restart safety were not inspected or changed; no live-startup claim is made.                                                                                                                                                   |
