# Targeted runtime invalidations

**Status:** One tracked Chromium lifecycle passed on the `1ed0548b`-equivalent tree; this branch's exact-head full gate and targeted invalidation validation remain pending.
**Audience:** maintainers changing Canvas commit notifications or their consumers.

## Goal and boundaries

Use typed table-change hints for consumers with named durable input dependencies,
and avoid waking HTTP resource streams for proven receipt-only commits. A hint
only tells its consumer to reread authoritative state; it is not a receipt,
permission to replay, or a replacement for an explicit resource signal.

This is an invalidation-only optimization. Keep permission checks, durable request
identities, response payloads, event delivery, and HTTP behavior unchanged. Keep
all existing model-catalog, Mac sync, host-secret, and route-owned resource
notifications. Do not suppress or merge them into this classifier.

History import failures caused by a typed discovery bound or invalid budget
records block only the affected actor until relevant profile, agent, or budget
input changes. Budget state and usage commits use separate store observers and
resume budget-blocked actors without refreshing the agent roster. They do not
create timer retries or prevent other actors from importing. Transient store,
read, and concurrency failures retain the bounded retry deadline. The history
schedule owns typed per-blocked-actor inputs; unrelated roster commits must not
unblock it. Per-profile discovery-limit failures are cached until a structural
file or profile event invalidates them, avoiding repeated scans of the same
oversized profile. Exhausting only the current cross-profile fairness-round
budget yields once and resumes under a fresh budget rather than permanently
blocking a valid profile.

The history consumer observes `runtime_agents` for roster changes and both
`runtime_budget` and `runtime_budget_usage` for budget eligibility. A budget
invalid actor resumes only after one of those budget inputs changes. Discovery
limits remain cached per profile and reopen only after a structural change for
that profile; exhausting the cross-profile round budget starts a fresh round
without a retry timer.

## Named owners and consumers

- [`ObservedTable`](../../workspaces/runtime/packages/runtime-store/src/lib.rs)
  owns the closed 22-table set and row-change hooks. Callers register synchronous,
  infallible callbacks with `add_table_change_observer`; each callback only
  signals its consumer to reread current rows. The source-owned dependency groups
  are:

  | Consumer                                          | Observed tables                                                                                                                                                                 | Owner                                                                                                                                  |
  | ------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
  | Account registry scheduler                        | `runtime_agents`, `runtime_events`, `runtime_event_meta`, `runtime_rooms`, `runtime_operation_receipts`, `runtime_work`                                                         | [`main.rs`](../../workspaces/runtime/apps/runtime-server/src/main.rs), `REGISTRY_WAKE_TABLES`                                          |
  | Server orchestration                              | `runtime_server_outbox`, `runtime_access_clients`, `runtime_server_links`                                                                                                       | [`main.rs`](../../workspaces/runtime/apps/runtime-server/src/main.rs), `ORCHESTRATION_WAKE_TABLES`                                     |
  | Federation snapshot, pairing, and delivery queues | `runtime_federation_settings`, `runtime_federation_identity`, `runtime_federation_peers`, `runtime_federation_invites`, `runtime_federation_rooms`, `runtime_federation_outbox` | [`main.rs`](../../workspaces/runtime/apps/runtime-server/src/main.rs), `FEDERATION_WAKE_TABLES`                                        |
  | History roster and budget eligibility             | `runtime_agents`, `runtime_budget`, `runtime_budget_usage`                                                                                                                      | [`history_runtime.rs`](../../workspaces/runtime/apps/runtime-server/src/history_runtime.rs)                                            |
  | Monitor scheduling                                | `runtime_monitors`, `runtime_rules`, `runtime_agents`                                                                                                                           | [`monitor_runtime_wiring.rs`](../../workspaces/runtime/packages/runtime-services/src/monitor_runtime_wiring.rs), `MONITOR_WAKE_TABLES` |
  | Peer-link snapshot                                | `runtime_access_clients`                                                                                                                                                        | [`peer_link.rs`](../../workspaces/runtime/apps/runtime-server/src/peer_link.rs)                                                        |
  | Account auth-resume waiters                       | `runtime_usage_resumes`                                                                                                                                                         | [`account_worker.rs`](../../workspaces/runtime/packages/runtime-services/src/account_worker.rs), `StoreBridge`                         |

  These groups name dependencies rather than carrying row data. Register each
  consumer before it takes the snapshot it will maintain. Add a table only with
  a named consumer and coverage that proves the consumer rereads authoritative
  state after a hint.

- [`RuntimeStore::add_resource_change_observer`](../../workspaces/runtime/packages/runtime-store/src/lib.rs)
  is the broad Canvas resource invalidation source installed by
  [`HttpState`](../../workspaces/runtime/packages/runtime-http/src/router.rs).
  It wakes the existing `ChangeNotificationHub` consumed by
  [`sync_stream`](../../workspaces/runtime/packages/runtime-http/src/routes/sync_stream.rs),
  which invalidates resource references except the separately handled
  multi-server resource. The old general `set_commit_notifier` remains a distinct
  commit signal and must not be mistaken for this resource callback.
- For known commits, the store suppresses the broad resource wake only when the
  positive transaction change count exactly equals the update-hook count for
  `runtime_operation_receipts`, `runtime_native_action_receipts`, and
  `runtime_access_receipts`. These are the three audited receipt journals.
  Mixed writes, incomplete hook counts, or any other changed table keep the
  broad wake. An ambiguous commit always keeps it, including a receipt-only
  write, because the transaction may have changed resources.

## Notification contract

Typed table observers fire after a known commit that may have changed their
table, or after an ambiguous commit that may have landed. A known rollback and a
transaction with no row changes do not fire them. Callbacks remain short,
nonblocking invalidation hints and run outside the registration lock and SQL
transaction; they must not read SQLite or call providers.

For the HTTP broad resource wake, the store compares SQLite's transaction change
count with update-hook rows recorded for those three receipt tables. It suppresses
only a positive exact match. Zero changes and known rollback do not wake. A mixed
write, unknown or incomplete hook evidence, an absent hook for a changed row, or
an ambiguous commit wakes. In particular, an ambiguous commit wakes even when
the known hook count names only receipt tables. A canceled response waiter does
not cancel the submitted write or its notification decision. Never infer
“receipt-only” from an absent hook event when SQLite may have skipped row hooks.

The store verifies rowid storage for every typed observer table. SQLite's
authorizer disables the truncate optimization for `DELETE` on those tables so
row hooks see deleted rows. `total_changes` also provides a second count for
resource suppression: a missing or incomplete receipt update-hook count cannot
match a positive transaction count, so it keeps the broad wake. Trigger writes
to other tables likewise make the counts differ. Hook failure or uncertainty
fails open to a broad wake.

## Required fixtures

Deterministic source fixtures cover the store-level cases below. The base actor
validation gate passed on 2026-10-16 at
[`5f545fdd`](https://github.com/ifdotpy/codex-studio/commit/5f545fdd), including
`just test-fast`, strict workspace Clippy, workspace doctests, and strict
runtime mypy. That result does not validate this branch's changes. Its build and
test gate must run before delivery against the exact commit head; live
idle-browser validation also remains pending:

- A no-op transaction and a known rollback produce no typed hint and no broad
  resource wake.
- A successful change wakes only consumers registered for that table; unrelated
  tables wake no targeted consumer. The source has table-mask coverage for all
  named observer tables and transaction-level routing fixtures for the account
  registry, orchestration, monitor, and history paths. History specifically
  verifies that agent changes dirty the roster while budget and usage changes
  dirty only budget eligibility via
  [`only_agent_commits_dirty_the_history_roster`](../../workspaces/runtime/apps/runtime-server/src/history_runtime.rs)
  and [`budget_commit_unblocks_only_budget_invalid_actors`](../../workspaces/runtime/packages/runtime-services/src/analytics_history_schedule.rs).
  Profile-scoped structural invalidation and fresh-round behavior are covered by
  [`profile_tree_change_unblocks_discovery_limits_without_roster_changes`](../../workspaces/runtime/packages/runtime-services/src/analytics_history_schedule.rs)
  and [`round_budget_exhaustion_resets_without_permanently_blocking_a_valid_profile`](../../workspaces/runtime/packages/runtime-services/src/analytics_history/discovery.rs).
  The store mask and rowid fixtures are
  [`table_change_mask_covers_each_named_observer_table`](../../workspaces/runtime/packages/runtime-store/src/lib.rs)
  and
  [`tracked_observer_tables_must_keep_rowid_storage`](../../workspaces/runtime/packages/runtime-store/src/lib.rs);
  consumer examples are
  [`unrelated_event_commit_does_not_wake_orchestration_but_outbox_commit_does`](../../workspaces/runtime/apps/runtime-server/src/main.rs),
  [`low_worker_rule_commit_does_not_wake_registry_but_agent_commit_does`](../../workspaces/runtime/packages/runtime-services/src/monitor_runtime_wiring.rs),
  and
  [`native_rollout_append_wakes_idle_history_runtime`](../../workspaces/runtime/apps/runtime-server/src/history_runtime.rs).
- Receipt-only writes suppress the resource wake; mixed receipt and event writes,
  a trigger that changes another table, a missing update hook, and a full-table
  receipt delete keep it. The fixture is
  [`resource_observer_suppresses_only_proven_receipt_only_writes`](../../workspaces/runtime/packages/runtime-store/src/lib.rs).
- Writes to `runtime_events`, `runtime_event_meta`, or `runtime_items` must keep
  the resource wake even when the same transaction updates a receipt. The event
  case is exercised in the receipt fixture; preserve the same invariant for
  event metadata and item writes in the HTTP integration coverage.
- An ambiguous commit that may have changed an observed table emits its typed
  invalidation hint and preserves the existing unknown-outcome result. The same
  fixture verifies the broad resource wake remains enabled even for an ambiguous
  receipt-only write:
  [`ambiguous_commit_wakes_table_observer_and_returns_unknown`](../../workspaces/runtime/packages/runtime-store/src/lib.rs).
- Cancel the response waiter after writer submission; once the actor finishes,
  verify the same table and HTTP wake decisions as for a live waiter. Do not
  replay the request or synthesize a new identity. Existing store coverage is
  [`canceled_write_response_keeps_commit_notification`](../../workspaces/runtime/packages/runtime-store/src/lib.rs).
- Verify model catalog, progress-file watching, explicit
  `ResourceNotifications.publish_once`, Mac sync, and host-secret resource
  signals still wake their subscribers independently. Token-rate updates remain
  a non-resource invalidation. These sources are outside SQLite classification.

## Validation and rollout

The source owners are `runtime-store` for table and resource observers,
`runtime-server`/`runtime-services` for dependency groups, and `runtime-http` for
the resource subscriber. Keep the HTTP classifier limited to the resource
observer; do not remove the separate broad commit signal, explicit route
notifications, or any non-SQL source. The tracked [Rust chat lifecycle
verifier](../../workspaces/client/apps/web/scripts/verify-rust-chat-lifecycle.mjs)
passed against real state on the `1ed0548b`-equivalent tree: one Luna chat was
created (195 ms), acknowledged (252 ms), answered (7,019 ms), and deleted
(1,443 ms). A 15-second steady window observed zero requests and zero errors;
the owned chat was deleted. This does not replace the current branch's
exact-head full gate or targeted before/after proof that receipt-only writes
stay quiet while event and resource writes still wake subscribers. No broader
performance claim is made.

An unrelated test-planner follow-up remains outside this slice:
[`run.py`](../../workspaces/runtime/apps/server/tests/server/run.py) reads
process-wide `RUSAGE_CHILDREN.ru_maxrss` as a suite peak. The reported 7.24 GB
cannot be attributed to the individual server suite from that measurement;
correcting the planner is separate work.
