# Agent orchestration at 512 workers

## Change Contract

- **Audience:** maintainers completing the handed-off Rust performance changes.
- **Responsibility:** record the owner-approved S1 requirements, delivery order,
  and acceptance evidence for a responsive shared Rust backend.
- **Boundaries:** retain the existing state directory, server identity, SQLite
  ownership, permission checks, command identities, and uncertain outcomes.
  Electron and a database replacement are outside this change.
- **Configuration:** existing schema and runtime owners remain authoritative;
  no new dependency or externally visible contract is approved implicitly.
- **Verification:** focused deterministic regressions, independent source review,
  the exact-commit `just test-fast` gate, and separate live load verification.

## Approved specification S1

The owner approved S1 on 2026-10-18 and requested incremental delivery to
`origin/main` as each tested slice becomes ready. Implementation is in progress;
this plan does not claim that the target has already been demonstrated.

### Requirements

- **R1 — Delivery order:** integrate `fix/provider-session-loss-recovery`, then
  `perf/blocking-pool-discipline`, then `feat/idle-session-unload`, then
  `perf/account-worker-throughput`. Preserve the handed-off commits and draft
  throughput test. Verify each integrated slice before publishing it.
- **R2 — Concurrency:** slow preparation must not delay independent starts or
  tool callback responses. Correlate responses by RPC identity and retain
  ordered durable journal settlement. Removing the logical store operation
  semaphore requires authoritative epoch, attempt, and authorization fencing
  at commit; actor FIFO alone does not serialize logical operations.
- **R3 — Recovery:** stop, deletion, disconnect, and actual backend restart must
  settle or recover operations without duplicates or resurrecting deleted
  agents. Missing native history and lost responses remain uncertain until
  authoritative evidence resolves them. A stale child response must not kill
  the account worker. Consolidate disconnect/restart text under one owner.
- **R4 — Simplicity:** fix measured bottlenecks with the existing lifecycle and
  diagnostics components. Reuse typed structures and named contract owners.
  New dependencies or material contract changes require a Pivot1 decision.
- **R5 — Timeout:** preserve the existing request deadline origin and duration.
  Any change to separate queue and post-write deadlines is deferred until
  measurements establish a need and the owner approves that contract.
- **R6 — Shared runtime:** use one backend and the existing real state identity.
  Coordinate authorized restarts and at most three simultaneous heavy Rust
  builds. Stop ordinary checks and builds before load runs.
- **R7 — Event ownership:** wake work from agent/provider events, relevant
  committed changes, and actual operation or idle-expiration deadlines.
  Retry backoff follows failures. Do not introduce periodic discovery scans
  or unconditional provider polling; idle unloading must follow this rule.

### Acceptance

- **A1 — Deterministic regressions:** cover the codec frame boundary, parent
  notice storage failure, read cancellation/admission, callbacks during paused
  preparation, concurrent starts, stop-before-submission fencing, deletion
  during recovery, and completion after an actual restart. Preserve exact
  request identities and unknown outcomes in the retry matrix.
- **A2 — Real load:** exercise 30, then 128, then 512 concurrently active real
  workers on the shared server. Each stage permits two short exchanges per
  worker and at most five minutes, using the cheapest available Luna without
  fast mode. Include tool response delivery. Queued agent records and simulated
  provider responses do not prove concurrent real-model operation.
- **A3 — Latency:** creation, message submission, and stop API p95 must be at
  most one second. Ready-message to provider stdin p95 must be at most two
  seconds. Also report preparation, queue wait, and complete delivery latency;
  separate provider generation time and external rate limits. After the load,
  queues drain with no lost/duplicate messages or SQLite busy/locked errors.
- **A4 — Browser and cleanup:** verify responsive Chrome input plus chat
  creation, prompting, and deletion. Clean up every test-owned agent, process,
  and generator; retain evidence and leave the verified Rust build running.

One complete load staircase plans 670 workers and 1,340 short worker exchanges.
The owner did not specify a separate currency ceiling. Announce each live run's
duration and CPU/memory/call budget before starting it. Keep these load runs
outside ordinary tests, with tracked identities and bounded cleanup.

## Initial source findings

These findings are source review results, not measured throughput evidence:

- The first branch counts the line delimiter against the codec payload limit
  and can suppress a local parent-event write failure. Correct both before
  delivery and add focused regressions.
- The blocking-pool branch collects the complete monitor receipt directory for
  every recovery batch. Keep bounded recovery work and validate cancellation
  under the actual runtime pool configuration.
- The idle-unload branch introduces a periodic provider sweep. Replace its
  discovery cadence with relevant events and the earliest idle deadline.
- The throughput WIP contains eleven committed files and an untracked
  `account_worker/tests/throughput.rs` draft. The handoff's file count is stale.
- Restart completion returns a dependency error in `recovery_store.rs`.
  Existing completion/delivery seams and the Python oracle define the parity
  boundary; no new automatic replay guarantee is approved.
- The reported long journal transaction needs queue/hold/commit measurements.
  Repeated receipt-prefix compaction is a source-level candidate, not a proven
  root cause. Stream clones use shared buffers and must not be called deep
  copies without evidence.
