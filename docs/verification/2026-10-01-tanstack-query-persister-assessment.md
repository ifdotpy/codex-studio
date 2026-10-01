# Persisters as an alternative outbox owner — read-only Pivot1 addendum

**Yes, persisters are a valid alternative to examine. They can remove the RxDB-to-Query bridge if persisted mutation records become the single outbox authority. The stock paused-mutation example does not preserve Studio's current delivery contract by itself.** This does not justify the existing646-line adapter: some of that duplication can be removed while keeping RxDB, and an alternative should be measured rather than dismissed or assumed smaller.

This assessment checks current official sources (2026-10-01), the installed Query5.104.0 core, and Studio's committed caller contract. No persister package was installed, no product code changed, and no durable-store pivot was authorized/implemented.

## What native persistence removes, and what remains

| Concern                      | Persisted mutations can supply                                                       | Minimum Studio adapter still required                                                                                        |
| ---------------------------- | ------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------- |
| Durable source/view          | Serialize mutation state; hydrate it; expose MutationCache/useMutationState directly | Put immutable ID/body/workspace and domain state into persisted records; uniquely index business IDs                         |
| Scheduling                   | Per-room scope, retry/backoff, transport pause/resume                                | Four active attempts limit; distinguish user pause/cancel from transport pause; check current durable state before POST      |
| Commit before composer clear | Promise-returning onMutate is awaited before mutationFn                              | Await an actual successful transaction for this submission; propagate failure; gate POST and clear until commit              |
| Reload                       | Restore state and supply default mutationFn                                          | Recover online in-flight/uncertain operations, not only offline-paused ones; retry same ID/body/workspace                    |
| Multi-tab                    | Persistence to shared storage                                                        | Atomic per-command admission/control, conflict handling, cross-tab change delivery and oldest-room ordering                  |
| Retention                    | Configurable cache lifetime/buster                                                   | Never expire/drop pending or uncertain commands on age, deploy or restore error; retain terminal receipts until acknowledged |
| Migration                    | Hydrate a supported saved format                                                     | Explicit resumable import of legacy RxDB records, duplicate checks, sole-writer handoff and rollback policy                  |

If Query becomes the sole owner, it could eliminate the RxDB outbox collection, readOutbox projection query, RxDB→setQueryData subscription, queued-ID discovery bridge, and connection retry machinery specific to that bridge. Mutation scopes/retry stay useful. **RxDB itself still remains a dependency for projections/drafts**, so moving this collection does not remove RxDB from the bundle.

Native mutation keys are categorization, not unique durable business IDs. Hydration builds mutation entries; it does not provide transactional duplicate-ID/body conflict checks. Functions are not serialized, so restored mutations require registered default mutation functions. Sources: [official mutation persistence guide](https://tanstack.com/query/latest/docs/framework/react/guides/mutations), [hydration implementation](https://raw.githubusercontent.com/TanStack/query/main/packages/query-core/src/hydration.ts).

## Immediate commit and failure handling

The stock async persister saves a whole client snapshot under one storage key, throttled by default at1000ms. Its save catches storage errors; without a retry handler it can complete without storing. The throttle returns immediately for a call already coalesced into a scheduled save, and catches thrown errors itself. Consequently **await persistQueryClientSave with this stock persister is not a per-submission durable commit acknowledgment**. Setting throttleTime:0 alone does not fix error propagation or coalesced-call semantics. Sources: [async persister implementation](https://raw.githubusercontent.com/TanStack/query/main/packages/query-async-storage-persister/src/index.ts), [async throttle](https://raw.githubusercontent.com/TanStack/query/main/packages/query-async-storage-persister/src/asyncThrottle.ts).

This is solvable, not a fundamental limitation of Query. Use a custom unthrottled, failure-propagating transactional persister/admission API. onMutate can await that write before permitting mutationFn and composer clear; storage failure keeps draft/assets and causes zero POSTs. A promise-returning custom IndexedDB persister is an official extension point. The commit must be the submitted command's transaction, not merely “a cache save was scheduled.” Automatic cache subscriptions cannot serve as that admission receipt. [Official persistence API/custom IndexedDB example](https://tanstack.com/query/latest/docs/framework/react/plugins/persistQueryClient).

The sync-storage persister is not a shortcut: its current source marks it deprecated and its wrapper schedules the write by timer. [Official sync persister source](https://raw.githubusercontent.com/TanStack/query/main/packages/query-sync-storage-persister/src/index.ts).

## Online response loss and restart

Default dehydration selects mutations whose state.isPaused is true. An online request awaiting its response is ordinarily pending and not paused. The current Studio outbox also executes with undefined mutation variables and loads the body from RxDB, so merely adding a persister and deleting RxDB would not even preserve its body.

Independent installed Query5.104.0 core check:

- Pending online mutation, networkMode:always, immutable body in variables: isPaused=false, default dehydrated mutation count0.
- Override predicate to retain pending: count1.
- Hydrate that snapshot and invoke resumePausedMutations: resumed calls0, restored status stillpending.

Thus saving all pending entries also requires explicit recovery classification/resumption. Persist the pre-POST intent, retain it across attempted/uncertain states, then reconcile/retry the same idempotent endpoint after reload. A server commit followed by browser crash must never manufacture a fresh ID. A domain “paused by user” record must remain paused when Query resumes transport-paused work. This is application behavior beyond the generic resume example; Query can host it but does not infer it.

## Multi-tab ownership

Whole-cache last-writer-wins snapshots are insufficient: two tabs with different local mutations writing the same key can overwrite each other's pending command set. Separate per-tab keys avoid that overwrite but do not preserve Studio's shared cancel/pause visibility and durable oldest-message checks. Query's experimental broadcast plugin subscribes to **QueryCache**, not MutationCache; it is not a durable command coordination mechanism. [Official broadcast implementation](https://raw.githubusercontent.com/TanStack/query/main/packages/query-broadcast-client-experimental/src/index.ts).

The minimum equivalent store needs atomic per-ID compare-and-update, immutable-body conflicts, attempted/cancel arbitration, monotonic receipts, and cross-tab discovery. Server idempotency prevents duplicate execution of the same ID; it does not recover a command dropped from browser storage or order different IDs. A custom IndexedDB record store can provide these, but that begins rebuilding capabilities already supplied by RxDB. Using RxDB as backing storage is possible, but then the adapter must genuinely eliminate redundant observable ownership rather than add another persisted cache.

## Retention, migration and removal

The standard restore path defaults maxAge to24hours and removes expired, buster-mismatched or malformed cache data. Those are cache semantics, unsuitable defaults for unsent user commands. Pending/uncertain retention and schema errors need a fail-visible, non-destructive policy; command persistence should be separate from disposable HTTP caches. [Official restore/save implementation](https://raw.githubusercontent.com/TanStack/query/main/packages/query-persist-client-core/src/persist.ts).

Migration would read legacy/version1 RxDB intents and atomically import unchanged ID/body/workspace, timestamps, attempted/user-control state and receipts. Before enabling a new sole sender, establish an idempotent import/handoff marker and prevent old tabs from continuing as incompatible writers. Do not delete the old records until the new transaction and resumable migration checkpoint succeed. Preserve originals for rollback, but do not keep two independently replaying durable authorities. This is a storage-contract pivot, not a dependency-only refactor.

Official persistence packages are additional MIT dependencies (React provider→persist core; async storage persister→core/persist core). A custom persister may avoid the stock async package; IndexedDB helpers/broadcast introduce their own surface if selected. Current upstream package metadata varies by package, so a concrete proposal should pin mutually compatible published versions and measure the built delta; none was installed for this assessment. [Provider manifest](https://raw.githubusercontent.com/TanStack/query/main/packages/react-query-persist-client/package.json), [async manifest](https://raw.githubusercontent.com/TanStack/query/main/packages/query-async-storage-persister/package.json).

## Recommendation

Do not bolt stock persistence onto the current bridge, creating a second durable copy. Either:

1. Simplify within the current contract: RxDB directly owns outbox records and observable view; remove the redundant outbox Query projection and use Query only where its request lifecycle pays for itself.
2. Treat persisted mutations as a genuine alternative sole-owner design. A small vertical-slice comparison must prove awaited failure-aware admission, lost-response/reload with one server event, same-ID conflict, two-tab updates/cancel/order, retained user pause and migrations before judging its source/bundle savings.

I favor option1 for this existing multi-tab application because RxDB is already required and provides the difficult durable coordination. Option2 can be appropriate, especially with a simpler single-tab contract, but no evidence yet shows it is smaller under the unchanged Studio contract. Native persistence removes serialization/hydration plumbing; it does not eliminate command identity, commit acknowledgment, uncertainty, multi-tab arbitration, controls or migration.

This comparison is source plus isolated core evidence, not a working persister prototype or a size estimate. It should be included in the final pivot decision without claiming the existing inflated adapter is necessary.
