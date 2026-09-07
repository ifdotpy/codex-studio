# Token efficiency implementation

Scope: the first six priorities in [the audit](2026-09-07-token-efficiency.md).
The maintained API contract is in [ORCHESTRATION.md](../../ORCHESTRATION.md#model-context-and-output-budgets).

## Changes

1. Brief task pages, explicit detail/history reads, and compact mutation receipts.
2. Separate peer directories, status changes, and on-demand capability details.
3. Bounded model output with durable references, Unicode range reads, and text search.
4. Versioned progress batches that preserve urgent events and original receipts.
5. Confirmed-delivery context versions and on-demand panel guidance.
6. Failure-only monitor notifications and guidance for script-driven panel feeds.

Full task evidence and tool responses remain in SQLite. UI transcript reads recover
model-projected content within the existing display allowance. Panel writes still
validate layout and return images. Model settings and permission rules do not change.
This layer does not impose a universal limit on native Codex tool output.

## Measurement

Read-only projection of the current Attar team's 77 task records, September 7:

| Representation | UTF-8 bytes |
|---|---:|
| Previous list, two complete arrays | 1,217,472 |
| All 77 brief records, one array | 16,556 |
| First page, 20 records and cursor | 4,472 |

The full brief directory is 98.64% smaller in this sample. This measures JSON bytes,
not token savings, model quality, latency, or billing. The fixture also checks that
paging returns every task identity and that detail/history reads retain evidence.

## Verification

The isolated contract suites cover:

- 13 efficiency cases: paging, stale cursors, exact retries, complete evidence,
  private references, Unicode output recovery, images and clocks, urgent delivery,
  distinct progress topics, ambiguous revisions, context after compaction, reordered
  delivery, cleared plans, completed UI tool output, and failure-only monitors.
- 48 runtime cases and 34 workspace cases.
- 18 request-ledger cases and 13 monitor-lifecycle cases.
- 21 panel cases, 20 panel-feed monitor cases, and 8 panel-feed cases.
- 7 complaint-routing cases and 6 time-awareness cases.

The panel guidance checks now read the guide through the actual dynamic tool.
The old panel retry test expected changed arguments with the same request ID to
return success. That test also fails on baseline `061bf16`. Its updated assertion
requires rejection of changed arguments and preserves the exact-retry test.

Independent review found two regressions during implementation: context versions
used creation order despite urgent delivery, and completed UI items did not recover
projected output. Both have regression tests through the affected caller paths.
The final focused review reported no remaining findings.

These tests use isolated state and fake model providers. They do not prove live
model token savings. Source installation does not activate an already-running
Python server. The running process requires a separate, verified update or a restart
after its active work finishes.
