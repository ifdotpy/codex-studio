# Incremental sync measurements

## Compact sync v4: live v3 baseline

Measured 2026-10-23 at 19:50:42 UTC for 30 seconds against the running v3
server during normal compact-sync team work. These were two concurrent read-only
SSE subscriptions, with no generated activity and no server changes. Both
requested the same resources: state, drafts, terminal membership, the open lead's
transcript, queue and session cost, and one worker transcript as the desktop
prefetch target. Token rates were enabled. This is an illustrative active-team
sample, not a controlled before/after benchmark or a measurement of HTTP pulls.

| Accepted encoding | Actual content encoding | HTTP body bytes | Resources events / bytes | Token-rate events / bytes |
| ----------------- | ----------------------- | --------------: | -----------------------: | ------------------------: |
| identity          | identity                |         846,405 |             65 / 721,679 |              31 / 124,726 |
| br                | identity                |         862,707 |             66 / 737,981 |              31 / 124,726 |

The v3 server did not compress SSE even when Brotli was requested. The identity
sample averaged 28,214 B/s; earlier 30-second windows during the same work ranged
from 32,534 to 53,825 B/s. The two streams can coalesce changes differently, so
their byte counts need not match. Initial resource reconciliation was one 710 B
event in each stream. Counts exclude HTTP headers, transfer chunk framing and
TLS; all byte figures use UTF-8. There were no incomplete frames in this sample.

| Resource kind | References | Resource plus version JSON bytes, identity |
| ------------- | ---------: | -----------------------------------------: |
| state         |         65 |                                    629,433 |
| transcript    |         13 |                                     77,141 |
| session-cost  |         18 |                                      1,620 |
| queue         |          1 |                                         82 |
| drafts        |          1 |                                         38 |
| terminals     |          1 |                                         36 |

Resource byte attribution counts each resource reference and its corresponding
version object. It excludes the shared event envelope and framing, so it does
not sum to the event bytes. The selected chat transcript accounted for 31,407 B
over eight references. The one prefetched worker accounted for 45,734 B over five
references, or 5.4% of the entire stream (1,524 B/s). Prefetch remains enabled for
visible screens; this sample does not justify removing the existing bounded
desktop/mobile prefetch. Its HTTP catch-up traffic was not measured here.

| Entity collection | Documents | Serialized document bytes, identity |
| ----------------- | --------: | ----------------------------------: |
| agent             |        69 |                             326,369 |
| workspace         |        17 |                             214,785 |
| task              |        56 |                              37,081 |
| event             |        35 |                              19,172 |
| monitor           |        20 |                              17,496 |
| room              |         7 |                               6,361 |

Field churn compares consecutive received versions of the same entity. The first
version is excluded. Whole-value bytes measure the decoded entity value; changed
bytes measure a JSON object containing only changed top-level fields, including
their names. This describes redundancy, not an implemented patch wire format.

| Collection | Consecutive updates | Whole-value bytes | Changed-field bytes | Changed share |
| ---------- | ------------------: | ----------------: | ------------------: | ------------: |
| agent      |                  60 |           255,922 |              14,664 |          5.7% |
| workspace  |                  16 |           180,584 |             168,282 |         93.2% |
| monitor    |                   9 |             9,151 |               4,374 |         47.8% |
| task       |                  11 |             3,823 |               1,020 |         26.7% |

Workspace churn consisted entirely of `rateLimitsByAccount` (16 changes,
153,245 B) and `rateLimits` (10 changes, 15,047 B). A representative workspace
value contained 9,553 B of account limits and 1,489 B of default limits; other
values together occupied less than 100 B. Agent churn was mainly
`commandProgress` (11 changes, 4,850 B), `activity` (34 / 3,235 B), `tail`
(4 / 1,897 B), `contextUsage` (19 / 1,394 B), `lastAnswer` (2 / 1,386 B),
`lastEvent` (34 / 1,224 B), and `tokensUsed` (20 / 435 B). Monitor `tail`
accounted for 3,998 B over eight changes. Task `status`, `durationMs`, `finished`
and `outputTruncated` changed together 11 times.

The T2 optimized Rust Brotli replay of these same 96 recorded frames, retaining
the encoder dictionary and flushing each event, reduced 846,405 B to 46,666 B
(94.49%) at quality 3/window 20. It measured 2,286 microseconds of thread CPU
total (23.81 microseconds/event), 2,678,992 B peak encoder heap and 18 encoder
allocations. This is encoder replay evidence, not a live compressed connection;
output-buffer allocations are excluded from the encoder allocation count.

### Reproduce the traffic sample

The reusable [measurement script](../workspaces/tooling/apps/repository-checks/measure-sync-stream.ts)
opens plain and Brotli-requested streams for the same interval, reports event and
resource bytes/counts, and computes field churn for entity records. Supply an
explicit resource file to include the desired chat IDs:

```sh
node workspaces/tooling/apps/repository-checks/measure-sync-stream.ts \
  --url http://127.0.0.1:4620 --protocol 3 --seconds 30 \
  --resources-file /tmp/sync-resources.json --output /tmp/sync-report.json
```

The default protocol follows the current protocol owner; use `--protocol 3` only
for the historical server. Optional `--record /path/outside/checkout/events.sse`
saves the identity event stream for compression replay; it can contain private
conversation content. The measured raw recording and detailed report were kept
outside this repository in the operator's cache under
`codex-studio/compact-sync-baseline/`, not committed.

### View and publication audit

Inactive server screens retain state, draft and list membership subscriptions.
They release foreground transcript, queue, receipts and session-cost subscriptions
when the conversation unmounts, and now also release offscreen chat prefetch and
account-limit watchers. Returning restores the selected chat and bounded prefetch.
Closed workspace/task drawers and terminal output already release their detail
watchers; terminal membership remains available for the closed bar. Hidden Studio
settings tabs unmount their contents. Prefetch also releases its subscriptions on
document hiding or loss of network connectivity.

Entity and transcript caches already coalesce publication within a frame. Draft
query notifications now retain the latest result and reconcile it once per frame,
so an SSE batch does not publish each intermediate query result. This introduces
at most one frame of remote-draft display latency (the existing frame scheduler
uses a timer for hidden tabs); local typing remains immediate. It avoids repeated
decoding and reconciliation and retains one query-array reference without cloning
or serializing it. Focused tests cover hidden-screen release/restore and complete
draft publication/cancellation, alongside the existing entity/transcript batch
witnesses. A headless Playwright check against a private Vite dev server and
isolated API fixtures observes the actual requested resource sets across server
switches; it makes no live model request.

## Historical snapshot baseline

These measurements preserve the historical `/api/state` baseline; the endpoint
was removed in the pull request that completed the move to sync entities (round
3), `#11`.

Measured 2026-09-28 in isolated fixtures. No live database or backend was queried. The measurement script was removed and is available at commit `3507feea`.

| Measure                                                                      |                                   Before |                            After |
| ---------------------------------------------------------------------------- | ---------------------------------------: | -------------------------------: |
| Single agent change, raw                                                     |                     6,600,000 B snapshot |          1,005 B entity document |
| Single agent change, gzip                                                    | Not separately measured in task baseline |            266 B entity document |
| Initial load, raw                                                            |                6,600,000 B chat snapshot | 40,350 B for 40 synthetic agents |
| Initial entity load, gzip                                                    | Not separately measured in task baseline |                         10,614 B |
| Pulls at 100 changes/min, 2 windows                                          |              200 full-snapshot pulls/min |        200 incremental pulls/min |
| Snapshot serialize/hash or entity write/pull CPU per 100 changes             |                             1,582.509 ms |                        65.084 ms |
| `sync_entities` WAL writes at 100 changes/min                                |                          No entity table |                    824,032 B/min |
| Transcript `sync_entities` WAL writes at 54 stream updates/minute-equivalent |         Repeated full-payload projection |                    659,247 B/min |

The live baseline figures, 6.6 MB for `/api/state?view=chat` and 26.7 MB for `/api/state`, were supplied by the task. The synthetic before CPU runs the old full-payload deserialize/serialize/SHA-256 path against a 6.6 MB payload 100 times. In the final rebased run it measured 1,582.509 ms; after measured 65.084 ms for 100 entity writes, 200 checkpoint pulls, and client-envelope gzip measurement. The transcript WAL value is from a separate 40 KB / 54-update fixture at 0.75-second cadence, normalized to one minute; it is not additive to the agent-change case. These are local fixture/process measurements, not a live-server profile.

For the 40-agent fixture, initial raw transfer fell by about 164×, and one agent change fell from 6.6 MB to 1,005 B. Two open windows each pull once for each of 100 visible changes, so the count remains 200 pulls/min; the bytes and server work per pull are bounded by changed entities. Agent WAL growth while writing/upserting entity versions was 824,032 B/min; SQLite reported 819,200 B in committed WAL frames at checkpoint. The separate transcript fixture measured 659,247 WAL B/min while tracking per-item, order, and metadata hashes with NULL payloads. These are independent synthetic loads and exclude unrelated application writes.

The production mobile startup fixture also passed: 9,448 ms cold composer, 370 ms warm draft, and a 4,372,195 B gzip full-state fixture versus a 17,780 B gzip compact snapshot. Those HTTP snapshot measurements validate the fixture, while the raw entity seed measurement above validates the new pull wire format.

## V2 view-scoped replay and field audit

The global view now carries `agentSummary`; an open conversation subscribes to
`agentDetail`. The existing publisher writes both projections inside the source
transaction. A detail-only change retains the summary's sequence. The
[OpenAPI resource schemas](../workspaces/runtime/packages/runtime-http/openapi.json)
own the collection filters through `x-studio-entity-scope`, and
`AgentSummaryEntityDto` owns the exact list field set. Generated TypeScript and
Rust readers consume those owners. Tasks, monitors, command output, detailed
costs, desktop and terminal data enter the renderer through mounted resource
subscriptions. Mobile navigation back to the list releases conversation detail
after the subscription grace period.

The field audit followed `Sidebar`, `SidebarRow`, `chatStatusModel`, the chat
catalog/read-state helpers and desktop notifications. At this revision the
summary has 47 fields:

- Identity, hierarchy and routing: `id`, `kind`, `source`, `name`, `isLead`,
  `parentId`, `rootId`, `role`, `serverId`, `provider`, `model`, `accountKey`,
  `remoteAnchor`, `remoteOrigin`, `remoteWorker`, `sharedRoomId`, `movedTo`,
  `moveImportPending`.
- List placement and ordering: `created`, `updated`, `archived`, `pinned`, `cwd`,
  `project`, `projectId`, `projectFolder`, `projectFolderRevision`.
- Operational attention: `status`, `inFlight`, `autoWake`, `epoch`, `empty`,
  `stall`, `parkedEvent`, `capacityRetry`, `hasApproval`, `hasQuestion`,
  `hasBackgroundWork`.
- Result/read identity: `threadId`, `turnId`, `lastCompletedTurn`,
  `lastCompletedTurnStatus`, `readState`, `readStateSupported`, `lastReadAt`,
  `hasUnread`, `unreadCount`.

`overview`, `lastAnswer`, `tail`, `commandProgress`, context usage and execution
settings remain in the detail schema. Summaries contain the completed-turn and
read identities needed for unread indicators, and pending-input/background-work
flags needed when panel records are absent.

The offline replay used the existing 120-second v4 capture from
2026-10-24 16:24:59 UTC. It modeled a background window with no open conversation
or mounted panels, preserving the token-rate frames. It used the server's Brotli
quality/window owners, one encoder dictionary for the entire connection, and a
flush after every event. This is a projection replay, not a live-network claim.
The historical recording does not contain the new attention flags; the replay
treats absent pending-input flags as false and derives background-work from recorded
command progress.

| Measured quantity                              | Recorded baseline | View-scoped replay |
| ---------------------------------------------- | ----------------: | -----------------: |
| Identity SSE bytes                             |           398,548 |             49,600 |
| Brotli SSE bytes                               |            31,476 |              6,828 |
| SSE events                                     |               140 |                 88 |
| Agent value bytes before duplicate suppression |           301,258 |             55,483 |

The Brotli reduction is 4.61×. The replay suppresses 60 unchanged summaries among
73 recorded agent updates and excludes 42 panel/detail records or hints. It
implements whole-entity projection comparison; it does not use field patches.

Reproduce with the private capture kept outside the checkout:

```sh
node workspaces/tooling/apps/repository-checks/replay-view-scoped.ts \
  /path/to/events.sse /path/to/report.json
```

The script reads the committed OpenAPI field/scope owners and the server encoder
settings, verifies Brotli decompression against both inputs, and writes aggregate
measurements without storing conversation text in this repository. The browser
witness is `tests/sync/view-scoped-subscriptions-browser.spec.ts`; it checks the
real renderer's summary-only list, detail subscription on open, and removal
after returning to the mobile list and advancing the grace period.
