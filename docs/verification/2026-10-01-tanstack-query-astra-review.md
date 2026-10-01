# Cold Pivot1 review: REVISE / selectively retain HTTP Query

Candidate: 9fd09f3b66a3b37e3b7e12732c399124ed298a55.
Fixed original baseline: 2470fb5ac877559c70a74a8c76b3617db66cf141.
Read-only review of complete diff, README/HTTP inventory, outbox preflight, actual App/composer/message callers and installed Query/RxDB implementations. Candidate source and build remain unchanged and git status is clean.

**Do not accept this candidate as-is.** Query offers useful consolidation for HTTP reads, but the overall experiment is not a demonstrated simplification: production grows, the bundle grows, and the outbox still requires substantial custom lifecycle machinery. Retain HTTP Query selectively after correcting complaint-version handling. Resolve the intermittent durable-recovery gate before accepting the outbox. A narrower alternative is direct RxDB ownership of the outbox view, retaining durable safety improvements; Query can remain responsible for HTTP requests where it replaces a real owner. No contract/dependency pivot was implemented by this review.

## Prioritized findings

**P2: Full snapshots hide newer confirmed complaint replies.** At web/src/components/UserMessages.tsx:260-271, hasSnapshotDetail selects message unconditionally. onResponse (337-347) writes a newer result to Query but rendering ignores it. Baseline retained the greatest version. Actual production component, hidden Chromium: full snapshot v1 plus successful reply returning/caching v2 with one response → zero visible .complaint-response elements. It persists while replication lags or refresh fails, and a subsequent reply uses stale version1. Correct by reconciling greatest version across snapshot, confirmed reply and GET, including late responses. Workaround: wait for projection catch-up/remount; do not resend an already confirmed reply.

**P2: Advancing a complaint summary version no longer reloads its detail.** UserMessages.tsx:262-269 keys workspace+ID only and has no version invalidation. Baseline GET effect depended on message.version. Actual component fixture: summary v1 loads detail1; rerender summary v2 → exactly one GET total and detail remains v1. Global focus/reconnect refetch is disabled, so the mounted view can remain stale indefinitely. Refetch on observed version advances, including a trailing read when the advance arrives during an older request; protect newer confirmed results from downgrade. Workaround: remount/reload.

**P2: Reconnect no longer limits parallel rooms to four.** At outbox/query.ts:243-247, scheduleQueued submits every queued document via Promise.all. Query scopes serialize within each room but impose no global room cap. Baseline send.ts bounded drain workers with Math.min(4, rooms.length), explicitly to bound reconnect traffic. With five or more queued rooms and held responses, the candidate can issue all room attempts simultaneously, increasing reconnect load on a weak connection/server. Restore a bounded dispatcher without starving independent rooms and add a held five-plus-room test. This is a source-confirmed behavioral regression; the reviewer did not independently measure its large-backlog impact.

**P1 acceptance gap: durable recovery has contradictory evidence.** Two uninstrumented failures waited30s at tests/sync-recovery-browser.mjs:251 for receipt-check queued→accepted after correcting the receipt and pageshow. The original baseline line118 fixture-ID defect was distinct and corrected; it does not excuse this failure. The owner's final aggregate with failure-only diagnostics passed all3 checks, so its diagnostic catch did not capture a failure and no cause was established. A passing aggregate and the SQLite lost-response pass are evidence, not closure of this intermittent recovery boundary. Resolve the cause and rerun the normal path before acceptance.

**Outbox observer hazard, not proven line251 cause.** outbox/query.ts:304-307 removes an active Query on every connect/wake. Installed Query5.104.0 core reproduction: subscribe QueryObserver to queued data, remove exact Query, set accepted data → cache accepted, observer queued, no notification. However my actual useOutbox fixture did NOT freeze: one wake, two wakes, and8 quiet wakes all rebound, retained one observer and observed accepted afterward. Report this as a dangerous API interaction/suspicion, not a confirmed explanation of the earlier test failure. An active-observer guard is sensible, but its passing test alone cannot close the original failure.

There are also two writers to outbox Query data: readOutbox queryFn and RxDB subscription setQueryData. staleTime:Infinity does not order them or prevent an already-running queryFn overwriting a newer subscription snapshot. readOutbox has no generation guard/consumed abort signal. A stale overwrite was NOT reproduced in the actual hook; retain this uncertainty when evaluating hydration/online fixes. A direct RxDB view removes that overlap.

## Production totals, including all adapter modules

Committed physical lines include comments/blanks and declarations. All web/src JS/TS production modules counted, excluding test entrypoints/tests/benchmarks/verifiers. No production JS outside that tree changed.

| Scope                           | Baseline | Candidate |         Delta |
| ------------------------------- | -------: | --------: | ------------: |
| All production renderer modules |      138 |       142 |            +4 |
| All production renderer LOC     |   32,274 |    32,434 | +160 (+0.50%) |
| Changed production modules LOC  |    6,565 |     6,725 | +160 (+2.44%) |
| HTTP/shared/UI excluding outbox |    6,198 |     6,079 | -119 (-1.92%) |
| Outbox total                    |      367 |       646 | +279 (+76.0%) |

New outbox total is facade101 + model129 + query364 + store52. Some growth buys useful format validation, frozen workspace and initialization recovery, independent of Query. It is incorrect to compare the101-line facade alone with367.

Per-module deltas: api+2; Accounts+16; Analytics+2; BrowserAccessNotice-1; NativeRuntimeStatus-8; ProjectDirectoryPicker-12; Usage-49; UserMessages+7; WorkerModelPicker-13; Workspace+51; progressCache-53; PromptInput+3; useSkillAutocomplete-54; useTurnErrors-2; useWorkspaceResource-25; main+4; query/client+13; outbox aggregate+279.

Across changed production files: literal useEffect calls25→20; new Map14→10; new Set8→6; setInterval4→2; setTimeout7→3. These are syntax counts, not actual active timer counts or independent mechanisms. Candidate adds14 useQuery sites,5 fetchQuery sites,1 useMutation site and manual MutationCache construction.

Real HTTP simplification: skill catalog TTL/in-flight machinery; turn-error in-flight map; progress request/deadline machinery; several component fetch/loading effects. The removed Workspace resourceCache Context map was not passed to its old actual callers, so its deletion is not evidence of runtime savings.

Custom mechanisms remain: resource revision/again refs, progress byte-bounded persistent cache and reader-count cancellation, turn-error checked/failed/result caches, search sequence guard, Workspace pending counter/reload ordering, replication/transcript/draft owners. Outbox removes active/sendingRooms registries, drain/re-drain flags and5s interval but adds mutation-cache scans, custom firstAttempt promise attached with any, first-settlement flags, RxDB→Query bridge, queued-ID detection, connection/subscription flags and separate initialization backoff. This is mixed consolidation, not elimination of custom lifecycle work.

## Build, requests, rendering and idle work

Baseline index HTML SHA2561004d4c113317353a7c9db5d25af39e05aa196fd807e57e066f472d864bfccd9 matches supplied artifact. Candidate index HTML SHA2569bf2ffd85d467fbba8fccf4590d905213498af6de6af9ffdc7fa7af96140adb1. Artifacts read, not rebuilt; owner's build log identifies candidate index-DvSMGf13.js.

| Built JS                      |  Baseline | Candidate |            Delta |
| ----------------------------- | --------: | --------: | ---------------: |
| Main raw bytes                | 1,328,374 | 1,365,674 | +37,300 (+2.81%) |
| Main gzip9 bytes              |   406,099 |   416,971 | +10,872 (+2.68%) |
| All raw bytes,101 chunks each | 5,017,896 | 5,055,196 |          +37,300 |
| All gzip9 bytes, per-file sum | 1,423,634 | 1,434,506 | +10,872 (+0.76%) |

Gzip uses deterministic Python gzip level9 per asset, different from the build reporter's default. The approximate10KB claim is valid, and initial JS increases. Query adds2 installed packages; lock entries253→255, replacing no existing dependency.

Independent production Workspace fixture PASS: mount1 read, held refresh1 plus trailing1 after8 further revisions, agent-scope switch1 =4 GETs, zero aborts. It verifies preserved coalescing, not a baseline speedup. Skill same-scope dedup already existed.

Independent outbox reliability fixture PASS:3 full-collection calls through first delivery, then delta0 over5.5s; offline persistence/callback/return11ms;1 record341 serialized UTF-8 bytes. Baseline source ran collection discovery every5s. These are fixture API-call counts, not necessarily disk scans (RxDB caches queries), live timings or broad scale evidence.

Delivery still scans/sorts the full collection for oldest queued message. Baseline drain bounded concurrent rooms at4; candidate scheduleQueued Promise.all plus per-room scopes has no global room limit. N-room backlog may initiate N concurrent room attempts, each with identity/session/message requests. Every discovered pending ID also creates a mutation, and pending lookup scans MutationCache. No larger-backlog benchmark establishes an overall request/memory improvement.

Polling remains: session cost2/10s, account costs5/60s, accounts30s, runtime10s. Actual AgentPanel retains1s polling and8s deadline around migrated progressCache. No controlled before/after rerender, CPU, p50/p95/p99 or broad HTTP-count measurement demonstrates a general performance win. Eight quiet useOutbox resume events produced eight hook rerenders in the reviewer fixture; this is candidate-only observation, not a regression measurement.

## Durable and cache ownership

- RxDB remains the only durable client outbox; no Query persister is installed. Query is ephemeral; SQLite/server immutable request identity is final command authority.
- persistIntent freezes exact body/ID/workspace, checks existing content, and commits before App's onPersist clears composer/assets. No new ID is manufactured during retry. Legacy versionless records work; unsupported formats remain and fail visibly. Collection-wide decoding means one malformed record can block discovery; no per-record quarantine exists.
- Delivery checks current workspace, obtains session token, atomically sets attempted before POST and reuses stored body/header. Ordinary mutations default retry:false/networkMode:always. Only durable outbox opts into transient retry against the same idempotent endpoint.
- Cancel requires attempted=false; legacy uncertainty fails closed. Pause prevents future delivery attempts but cannot retract in-flight HTTP. Resume reuses the record; display edits preserve body. Receipt ID must match; confirmed state cannot be downgraded by a stale queue receipt.
- Query scopes serialize per workspace/room within a tab. Durable oldest-queued checks handle exhausted retries. Across tabs both may race the same oldest ID; backend uniqueness, not Query scope, gives one logical command. The real SQLite lost-response fixture remains essential.
- At most8 retries plus initial attempt occur per scheduling round; resume/restart/reconnect/queued-ID changes can create another round. Initialization retries separately. Mutations may outlive the hook; durable controls/workspace verification are the safety boundary.
- HTTP keys scope resources/costs/accounts/analytics/complaints. Models/directories/browser/native-status have origin/account keys without workspace. syncDatabase is page-lived and workspace replacement requires reload, limiting the current exposure. Do not claim universal scope enforcement. Default5min Query GC replaces some former count bounds.
- No server API/schema migration and no live state changes were introduced by Query itself.

## Third-party assessment, primary sources checked2026-10-01

Pinned React Query5.104.0 has only query-core5.104.0 as runtime dependency; core has none. Neither installed manifest declares install scripts/optional dependencies. React peer18/19 fits React19.2.8. Upstream documents Chrome91+, Safari/iOS15+. Chromium/Electron fits; physical iPhone/WebKit remains unverified here. Active upstream maintenance/current5.104.0 is observable; it does not establish a fixed LTS/SLA. Sources: [official installation](https://tanstack.com/query/latest/docs/framework/react/installation), [package manifest](https://github.com/TanStack/query/blob/main/packages/react-query/package.json), [repository](https://github.com/TanStack/query).

Both packages are MIT and require copyright/license notice retention; installed LICENSE files permit use in this privately licensed repository without changing its license. Preserve notices in distributions. This review did not audit packaged notices or alter licensing.

The official Query security page lists a separate streamed-hydration XSS advisory (that package is not installed) and no SECURITY.md. TanStack's updated May2026 supply-chain postmortem says Query was unaffected by the Router/Start incident, and the advisory's affected package list excludes these installed Query packages. The postmortem retains a contradictory timeline mention of query-core removal; the updated all-clear and package advisory are stronger evidence, not a comprehensive audit. Exact pins/integrity constrain accidental changes, not publisher compromise. Sources: [Query security](https://github.com/TanStack/query/security), [official postmortem](https://tanstack.com/blog/npm-supply-chain-compromise-postmortem), [official advisory](https://github.com/TanStack/router/security/advisories/GHSA-g7cv-rxg3-hmpx).

No new remote service, account, telemetry destination, durable cloud copy or runtime fee appears in this integration. Costs are bundle/CPU/memory, maintenance and coupling. Removal is practical: original commit retained, Query has no disk schema, server API unchanged. Restore relevant HTTP modules/provider/client/package+lock or replace outbox adapter while preserving immutable IDs, versioned RxDB readability and workspace checks. Old code ignores additive version1 fields, but a blind revert drops new safety validation; retain those improvements deliberately.

## Independent evidence and limits

- PASS: node --experimental-strip-types tests/outbox-model-contract.mjs.
- PASS: TMPDIR=/dev/shm CHROME_BIN=/usr/bin/chromium node tests/workspace-resource-query-ui.mjs. Evidence /dev/shm/studio-workspace-query-rHczgK.
- PASS: TMPDIR=/dev/shm CHROME_BIN=/usr/bin/chromium node tests/mobile-send-reliability-browser.mjs; offline11ms,3 reads, idle delta0/5.5s,341bytes; cancellation, parallel rooms/tabs, late receipts, same-ID retries and workspace checks.
- Actual production component reproductions: /dev/shm/astra-query-review.mjs; successful combined reproduction before subsequent wake-only adaptation logged full snapshot/cache v2 but0 visible replies, summaryv2 with1GET/detailv1. Isolated cache directory /dev/shm/astra-query-review-r1FWIj.
- /dev/shm/astra-wake-only.mjs:8 pageshow events did NOT freeze actual outbox hook. Primitive QueryObserver remove/set reproduction does leave old observer stale.
- Supplied/owner evidence reviewed: npm ci/build PASS; draft recovery PASS; real SQLite lost-response reload PASS with one operation/history row; final failure-only aggregate PASS. Earlier failures remain /home/alex/.local/state/codex-agents/monitor-logs/aee0683f-4ea9-573e-8a4f-eb395478a853.log and /home/alex/.tmp-o6diag/combined-native.log; last passing aggregate /home/alex/.tmp-o6diag/combined-final-failure-only.log.
- Baseline mobile font16 failure is established baseline debt. WebKit/physical iPhone unavailable. No live model/reset-credit/OS-input/active state directory was used.
- Candidate is not wholly local-verified because its durable regression gate is unresolved. This review is local source/fixture evidence only, not deployed/live evidence. Corrections need final-head review and targeted reruns.

Recommendation remains REVISE with selective HTTP retention. Resolve complaint versions and durable recovery before acceptance. If outbox still needs dual observable writers and most custom scheduling after revision, selectively revert the Query outbox layer while preserving safety improvements. The ordinary Workspace mutation wrapper also retains its old pending/reload choreography and offers little demonstrated simplification. No merge or publication recommended at this candidate.
