# Final independent Pivot1 review — 67aa0e0

Status: final read-only review complete. Matched combined build and targeted sequential checks PASS; bundle independently measured. Prior intermittent failures remain unexplained and are not labeled fixed.

Candidate: 67aa0e0a2d437060a6e8f9c7aa87872fd87dc435.
Fixed original baseline: 2470fb5ac877559c70a74a8c76b3617db66cf141.
Original cold candidate: 9fd09f3b66a3b37e3b7e12732c399124ed298a55.

## Decision

**Selectively keep useful HTTP Query conversions; revise or selectively revert the Query outbox layer. Do not describe the broad migration as a demonstrated simplification or approve the whole experiment for merge on current evidence.**

The reviewed fixes restore important invariants and are approved narrowly. Query does replace real request bookkeeping in skill catalog, progress retrieval, usage/resource reads and several smaller components. However, the finished renderer grows by290 production lines; the outbox grows83.7%; non-outbox changed modules save only17 lines after necessary complaint corrections. Custom scheduling and multiple observable/cache owners remain. Reduced periodic outbox discovery is a measured fixture benefit, but no controlled request/render/CPU benchmark establishes an overall performance win.

A smaller next design is RxDB directly owning both durable outbox records and their observable view, preserving all new identity/workspace/format/receipt safety improvements. Query should own requests where it genuinely replaces a lifecycle owner. Persisted mutations are also a valid alternative sole-owner design, not a drop-in fix: their adapter and migration must be proved and measured first. This review implements neither pivot.

## Findings closed and evidence limits retained

- Original full-snapshot-over-newer-reply defect and missing summary-version refresh: corrected with greatest-version reconciliation and coalesced trailing reads.
- Follow-up orphan GET after unmount: independently reproduced at b023189; corrected by stable workspace/ID lifecycle cleanup and ownership checks before additional refetch.
- Follow-up full snapshotv3→v2 regression: independently reproduced at b023189; corrected by retaining greatest full snapshot in Query cache. Expanded test covers older full snapshots, older summaries and stale GETs while retaining response content.
- Active same-key Query removal: guarded while observers exist. Core negative control and mounted-query identity test support this narrow fix. My original actual hook rebound on1/2/8 wakes and did not freeze; this was never proof of the original recovery timeout's cause.
- Reconnect fanout: restored per-tab max4 via FIFO permits around delivery attempts, with same-room Query scopes. Permits release in finally before retry backoff; durable status and workspace are read after acquiring a permit, then CAS is rechecked before POST. Independent held-five-room test passed progress during503backoff, cap, ordering, eventual drain and cancellation while waiting. Cross-tab global leasing was not added.
- Ownership fixture: obsolete modal/status selectors were adapted to the baseline's existing inline UI. Recipient permissions, empty-send disabled state, exact retry, conflict/fresh identity and history content remain. Late-response observer now targets the other complaint explicitly and a unique returned marker must appear only under its owner.

**Still open: recovery acceptance uncertainty.** Two original uninstrumented queued→accepted timeouts at sync-recovery-browser line251 remain unexplained. Later passing diagnostics never captured that failure state. The author also reports a later chained controls timeout waiting for rendered status after the durable record was accepted; standalone rerun passed. These are not waived by the baseline's distinct line118 fixture-ID bug or known mobilefont16 baseline failure. Green runs strengthen positive evidence but do not identify a cause or demonstrate those intermittent failures were fixed.

**Remaining unproven race:** outbox Query data has two writers, readOutbox queryFn and RxDB subscription setQueryData. staleTime Infinity does not order an already-running read against a newer subscription update. No stale overwrite was independently reproduced; do not present this as a confirmed bug or either timeout's cause. Removing the duplicate observable owner eliminates this class of integration concern.

## Complete production accounting

Committed physical lines, including comments/blanks/declarations; tests, fixtures, benchmarks, verifiers, docs and build/packaging tools excluded. Counts use git objects, not mutable working files. All production changes are in web/src.

| Scope                                                                          | Original |  Final |          Delta |
| ------------------------------------------------------------------------------ | -------: | -----: | -------------: |
| All shipped JS/TS modules plus declarations: renderer, desktop, public scripts |      148 |    152 |             +4 |
| All shipped JS/TS/declaration LOC                                              |   34,150 | 34,440 |  +290 (+0.85%) |
| Renderer production modules                                                    |      138 |    142 |             +4 |
| Renderer production LOC                                                        |   32,274 | 32,564 |  +290 (+0.90%) |
| Changed production modules LOC                                                 |    6,565 |  6,855 |  +290 (+4.42%) |
| Changed HTTP/shared/UI excluding outbox                                        |    6,198 |  6,181 |   -17 (-0.27%) |
| Complete outbox implementation                                                 |      367 |    674 | +307 (+83.65%) |

The10 unchanged desktop/public modules contribute1,876 lines. Final outbox is send facade101 + model129 + query392 + store52 =674. The author's672-line figure is not the committed physical-line count. Original9fd was646; fixes add28 there. UserMessages is+109 relative to original, versus+7 in9fd.

Every changed module delta: api+2; Accounts+16; Analytics+2; BrowserAccessNotice-1; NativeRuntimeStatus-8; ProjectDirectoryPicker-12; Usage-49; UserMessages+109; WorkerModelPicker-13; Workspace+51; progressCache-53; PromptInput+3; useSkillAutocomplete-54; useTurnErrors-2; useWorkspaceResource-25; main+4; query/client+13; outbox aggregate+307.

Literal changed-file mechanism counts: useEffect25→23; new Map14→10; new Set8→6; setInterval4→2; setTimeout7→3. Added14 useQuery sites,5 fetchQuery sites,1 useMutation site and manual MutationCache construction. These are syntax counts, not active timers, subscriptions or independent owners: Query adds internal retry/GC scheduling.

Removed custom mechanisms include HTTP in-flight/TTL maps and outbox active/sending-room registries, drain flags and5-second interval. Retained/added custom mechanisms include resource revision/again refs; byte-bounded persisted progress cache and reader cancellation; turn-error caches; search sequence guards; Workspace pending/reload ordering; complaint confirmed-map/max-version cache reconciliation plus revision/cancel job; outbox firstAttempt promise attached to mutation, mutation scans, queued-ID discovery, connection retry, RxDB-to-Query bridge, and now module-global FIFO waiters/active counter. Do not claim native Query owns all scheduling.

## Bundle and performance

Matched final build read only after monitor a75c9850 exited0 and lead froze source/build. Main asset is index-Diycz03T.js. Python gzip level9/mtime0 per asset; build reporter uses a different gzip setting.

| Built JS                           |  Original | Final67aa |            Delta |
| ---------------------------------- | --------: | --------: | ---------------: |
| Main raw bytes                     | 1,328,374 | 1,367,105 | +38,731 (+2.92%) |
| Main gzip9 bytes                   |   406,099 |   417,444 | +11,345 (+2.79%) |
| All101 JS chunks raw bytes         | 5,017,896 | 5,056,627 |          +38,731 |
| Sum of all101 per-file gzip9 bytes | 1,423,634 | 1,434,866 | +11,232 (+0.79%) |

Main and aggregate gzip deltas differ slightly because other assets/import hashes change compression. The approximate10KB claim is directionally fair; exact final initial-main increase is11,345 bytes. Baseline indexHTML SHA2561004d4c113317353a7c9db5d25af39e05aa196fd807e57e066f472d864bfccd9; final23c729ebc881cc628850529fa09e6365142cbd821ff02a384cf06728f14534ab. Final mainSHA25609ac76d6fe05c7a62c24f866ae522801d96f5e01d8b9f0774299912f576866e1.

Workspace caller fixture verified4GETs and zero aborts across initial load, coalesced held refreshes and scope switch. It proves behavior, not acceleration versus baseline. Existing skill dedup already worked before Query. Removing an unused resourceCache Context map is not a runtime saving.

Independent original-candidate mobile outbox fixture observed3 collection query calls through first delivery and no further calls in5.5s, versus baseline's5-second discovery interval. Owner's final-limiter fixture reports the same idle count and13ms persistence/callback; original reviewer observed11ms. These are fixture measurements, not p95 latency or physical disk scans: RxDB can reuse cached queries.

Delivery still reads/sorts the outbox collection for oldest-room intent, discovery builds mutation entries for queued records, and mutation lookup scans MutationCache. Max4 concurrent attempts is now restored, not a new speedup. No large-backlog memory/CPU/request benchmark or controlled render-count comparison proves global improvement. Existing component polling and AgentPanel's1-second progress poll remain.

## Ownership and safety boundaries

RxDB is still the sole durable outbox. Query caches/schedules ephemeral state; the backend's immutable request identity and uniqueness are final command authority. No Query persister is installed.

The committed path freezes businessID/body/workspace before first POST and awaits RxDB persistence before the actual composer onPersist clears draft/assets. Retry uses that exact body/ID. Unsupported durable formats fail visibly; legacy records remain readable. Attempted marking is the atomic cancel boundary. Pause stops later attempts, not in-flight HTTP; cancellation requires known unattempted state. Receipt IDs must match and confirmed receipt state is not downgraded. Ordinary mutations remain retry:false/networkMode:always; only the durable idempotent path opts into transient retries.

Scopes are perworkspace-room within one tab. Other tabs may attempt the same oldest ID; server dedup gives one logical event, not a Query cross-tab lock. The real SQLite lost-response test is evidence for one committed operation/history after reload, unlike a browser route mock. Multi-tab scope/control and workspace checks must remain when removing adapters.

Initialization and send retries are separate. A scheduling round permits initial attempt plus up to8 retries; wake/restart/discovery can start another round. Mutations outlive hooks, so current durable status/workspace verification remains necessary. HTTP resource keys include their relevant identity where implemented; origin/account-scoped keys are not universally workspace-scoped. No server schema or live state was changed by this experiment.

## Persister alternative

A sole persisted-mutation store could remove the RxDB outbox collection, readOutbox Query projection, subscription bridge and discovery plumbing; native hydration, scopes and retries are useful. RxDB remains required for other application projections/drafts, so the dependency cannot simply disappear.

However:

- Stock async persistence throttles whole-cache snapshots (default1s), coalesced callers can return before their own save, and storage errors can be swallowed. Awaiting the stock save is not a specific command's durable transaction acknowledgment. Use a failure-propagating transactional admission/persister; awaited onMutate can gate POST/clear.
- Default dehydration keeps paused mutations. Independent installed5.104 core check: pending online mutation saved0; custom pending predicate saved1, but hydrate+resumePausedMutations resumed0. Online lost-response recovery needs explicit pending/uncertain classification and the same storedID/body/workspace. Current mutations execute undefined variables and fetch bodies fromRxDB, so adding a persister alone saves no body.
- Whole-cache last-writer-wins storage can drop another tab's distinct command. BroadcastQueryClient observes QueryCache, not MutationCache; it is not atomic durable command coordination. Minimum adapter still needs unique immutable IDs, per-record atomic control/attempted changes, monotonic receipts, cross-tab notification and room ordering.
- Transport-paused is not user-paused. Retry/resume must preserve user controls. Default24h cache expiry/buster invalidation is inappropriate for unsent/uncertain commands.
- Moving the sole durable authority needs resumable legacy/version1 import with unchanged identity/body/workspace/control/receipts, committed handoff, old-tab writer policy and rollback. No double durable sender should be introduced.

This is a valid alternative, not proven smaller. It requires an explicit storage-contract pivot; none was implemented. Detailed primary-source assessment: [persister assessment](2026-10-01-tanstack-query-persister-assessment.md). Sources: [mutation persistence](https://tanstack.com/query/latest/docs/framework/react/guides/mutations), [persistence API](https://tanstack.com/query/latest/docs/framework/react/plugins/persistQueryClient), [async persister](https://raw.githubusercontent.com/TanStack/query/main/packages/query-async-storage-persister/src/index.ts), [throttle](https://raw.githubusercontent.com/TanStack/query/main/packages/query-async-storage-persister/src/asyncThrottle.ts), [restore implementation](https://raw.githubusercontent.com/TanStack/query/main/packages/query-persist-client-core/src/persist.ts), [broadcast implementation](https://raw.githubusercontent.com/TanStack/query/main/packages/query-broadcast-client-experimental/src/index.ts).

## Dependency, platform and removal assessment

Still pinned ReactQuery/core5.104.0; lock entries253→255, ReactQuery depends on core, core has no runtime dependencies, and these installed manifests have no install hooks. React18/19 peer support fits React19.2.8. MIT notices must remain in distribution; no repository-license change is needed. No new service/account/telemetry destination/runtime fee appears. Current upstream documents Chrome91+, Safari/iOS15+, but this review did not exercise WebKit or a physical iPhone. [Official installation](https://tanstack.com/query/latest/docs/framework/react/installation), [manifest](https://github.com/TanStack/query/blob/main/packages/react-query/package.json).

Current official security review is unchanged since the cold assessment: the listed streamed-hydration advisory affects a package not installed here. Updated TanStack supply-chain postmortem says Query unaffected, while retaining a contradictory timeline mention; the affected-package advisory excludes these installed Query packages. This is not a comprehensive security audit or SLA. [Query security](https://github.com/TanStack/query/security), [postmortem](https://tanstack.com/blog/npm-supply-chain-compromise-postmortem), [advisory](https://github.com/TanStack/router/security/advisories/GHSA-g7cv-rxg3-hmpx).

Removal is practical because Query has no durable schema here and the original baseline remains fixed. Replacing only the outbox bridge need not remove HTTP Query. Preserve versioned record readability, identity/workspace checks, commit-before-clear and monotonic receipts; a blind revert would lose useful safety changes. Persister packages would add dependencies and need separate exact-version, license/security/bundle review; none were installed.

## Verification ledger

Independently verified before integration, and final committed production blobs inspected to match:

- HTTP0b5dada: complaint-detail-unmount-ui, complaint-detail-query-ui, complaint-ownership-ui PASS. Includes heldGET revisions1→2→3 with one trailing read, oldGETv4 after POSTv5, fullv3 after fullv2/older summary, no orphan GET, identity/permissions/conflicts/history. Test-only5bc9c5a portability diff reviewed.
- Outbox8c4a467: sync-recovery-browser PASS, including cap/backoff/order/drain/cancelwaiter. Guard source095feaa plus mounted Query-identity regression reviewed; own guarded actual-hook check passed.
- Original9fd: outbox-model-contract, workspace-resource-query-ui and mobile-send-reliability-browser independentlyPASS. These retain their exact older revision attribution.
- Matched combined67aa build and sequential test:reliability (draft recovery, sync recovery including limiter, real SQLite response-loss/reload), complaint-detail-query-ui, complaint-detail-unmount-ui, complaint-ownership-ui, outbox-controls-ui allPASS. Lead monitor a75c9850-399e-5454-8135-c974fc87123f exited0; reviewer read full relevant log tail at /home/alex/.local/state/codex-agents/monitor-logs/a75c9850-399e-5454-8135-c974fc87123f.log. Reviewer's independent tests above exercised identical final production blobs: UserMessages ae31947dde181c441a7a3966314e9d7baa976319, outbox/query71d6fdc822f780c6b22f70b9ddc62a2317ff99ce. No redundant rerun of the same combined chain was needed.
- Final worktree git status clean. Build output read and measured only after the monitor finished; reviewer did not build or modify it.

Original failure artifacts: /home/alex/.local/state/codex-agents/monitor-logs/aee0683f-4ea9-573e-8a4f-eb395478a853.log and /home/alex/.tmp-o6diag/combined-native.log. Failure-only passing aggregate: /home/alex/.tmp-o6diag/combined-final-failure-only.log. Baseline mobilefont16 failure is established separately. No live model request, reset-credit action, OS input or active backend state directory used. No source/build mutation, merge or publication by reviewer.

Final readiness distinction: targeted local source/caller verification is green and the narrow corrections are approved. There is no new blocking source defect identified in those corrections. The broader architectural objective is not demonstrated, and historical flaky recovery/UI evidence is unresolved. This is a recommendation to narrow the experiment, not an instruction to merge, publish, rewrite durable storage or waive evidence.
