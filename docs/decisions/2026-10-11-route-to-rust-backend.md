# The backend moves to Rust as a whole behind the HTTP API

- **Status:** Accepted, 2026-10-11, by the maintainer.
- **Audience:** maintainers and contributors planning or implementing the backend migration.

## In short

The Python backend will move to Rust as one runtime behind the existing HTTP API. First make that API, provider protocols, launch behavior, and existing state testable contracts. Keep the current PyO3 bridge at its present extent until the whole-runtime cutover.

## Context

Issue #37 measured the code added and removed in three command-level Rust changes. Reservation (#87, merged) added about 300 Rust bridge lines around about 110 Rust rule lines and removed 2 net Python lines. Settlement (#92, merged) added 266 bridge lines around 213 rule lines and removed 62 net Python lines. Restart recovery (not merged) added 194 bridge lines around 72 rule lines and would add 20 net Python lines. Paired measurements with equal stored records found neutral performance for all three; the issue comment records no timing values. These are measured line counts and paired performance results from issue #37 comment [6109811509](https://github.com/ifdotpy/codex-studio/issues/37#issuecomment-6109811509), not estimates of future work.

Source reading gives a different kind of evidence. The source-only `rust-order-survey` at revision `c6c92f90` estimated candidate decision cores at roughly 100–300 rule lines each. Candidate bridge estimates were commonly 200–450 lines; for example, recovery was estimated at 180–300 rule lines against 250–350 bridge lines, account transfer at 120–220 against 250–350, and scheduler admission at 120–220 against 300–450. These are manual source-based estimates, not measured extraction results. The source-only `process-boundary-survey` at revision `6185e26f` found three separate-program candidates: the provider supervisor (1,867 Python lines; 2,300–3,500 Rust lines estimated), the Mac host-exec service (about 600 Python lines), and a 169-line upgrade checker. Together they account for about 3% of the server by that survey's estimate; the supervisor also carries high process-recovery risk. The remaining runtime is primarily I/O and glue around SQLite, providers, files, and threads.

The source surveys report approximately 74,000 production Python lines. Their exact counts differ: `rust-order-survey` counted 74,361 lines in 167 top-level modules; `process-boundary-survey` counted 74,389 lines in 168. Both are source-only inventories at different revisions and counting boundaries, so this record uses the rounded figure. The source-only `http-boundary-plan` at revision `cea4e7e7` screened 425 server suites: 150 appeared to assert user-visible outcomes while reaching Python internals, 265 appeared to assert implementation details or mix both, and only 10 were candidates for boundary coverage. This is a static screening, not a semantic audit. The suite inventory therefore does not yet provide a dependable cross-language contract.

The natural externally visible boundary is the HTTP API used by the web and desktop clients. Provider protocols, the supervisor protocol, launchers, and state files are additional compatibility boundaries. The current OpenAPI document does not describe every header, error, stream frame, or recovery behavior. The existing state directory is one installation and must retain its identity. Runtime database commits remain WAL with `synchronous=FULL`, as decided in [the durability record](2026-10-11-runtime-commit-durability.md).

## Alternatives considered

### A. Move the whole runtime to Rust behind the HTTP API

**For:** This uses the existing client boundary and reaches the approved D-01 destination. Contract, provider, state, and launcher work benefits both the current Python backend and the replacement. One coordinated move avoids a prolonged period with multiple schema and ownership transitions.

**Against:** It concentrates behavior and state uncertainty into one replacement. A cutover failure could affect the whole runtime, and the Rust implementation size, duration, and performance are not known from Python line counts.

### B. Continue moving Python decision cores through PyO3

**For:** The existing binding is in use, typed, and already carries reservation and settlement decisions. Small Rust decisions can be tested independently.

**Against:** The measured commands added 194–300 bridge lines each, and only settlement removed substantial Python logic. Source surveys found little separable decision logic relative to the estimated bridge size. More commands would add binding and protocol maintenance while leaving most of the runtime in Python.

### C. Move selected standalone programs behind their existing protocols

**For:** The supervisor and host-exec already have process boundaries and local protocols. A bounded command such as the upgrade checker has low operational risk.

**Against:** The three identified programs make up about 3% of the server. The supervisor has difficult child-process and recovery guarantees; host-exec tests do not currently exercise its running socket process. A program-by-program route does not remove the need for the whole-runtime move.

### D. Keep the Python runtime and improve its contracts without replacing it

**For:** This avoids the whole-runtime cutover and can still improve API and provider tests for the current backend.

**Against:** It does not reach the already approved Rust backend destination. It is a fallback only if evidence at a decision gate shows the chosen route cannot meet its compatibility, platform, cost, or performance requirements; returning to it requires a new maintainer decision.

## Decision

Adopt option A. Freeze the PyO3 bridge at its current extent: reservation and settlement remain in Rust; add no further native commands, decision cores, protocol versions, or separate-program rewrites. First make the HTTP boundary the executable specification through HTTP-only contract tests, provider replay fixtures, and one complete API schema authority. Then move the whole runtime behind that boundary.

Open the existing state directory in place. Only one backend process may own it at a time; stop the old backend before the replacement starts. Preserve state identity, formats, request identities, receipts, recovery behavior, and current transaction durability. The WAL and `synchronous=FULL` decision remains unchanged.

## Not decided yet

- The HTTP server stack, OpenAPI producer, and SQLite crate are choices for the implementation gate. The 2026-10-11 [M-05.1 crate discussion](https://github.com/ifdotpy/codex-studio/issues/37#issuecomment-6108655771) noted `rusqlite` 0.40.2 as synchronous, MIT-licensed, and having four direct dependencies before feature selection; it recommended disabling default features and not using bundled SQLite while Python also links SQLite. It described SQLx as async-first with a larger dependency graph. These are dated candidate facts, not a crate selection; recheck maintenance, release, license, platform, security, dependency, and removal facts at the gate. `utoipa` and `aide` were named as OpenAPI candidates, but the supplied survey established no current facts or choice for either.
- The frozen-schema compatibility rule remains open: decide whether compatibility means canonical OpenAPI bytes and hash or semantic parity, and define hash and mismatch behavior across release and rollback.
- The fate of suites that assert Python internals remains open. Classify them by purpose: retain Python regression coverage, replace externally meaningful assertions with boundary contracts, or retire only after equivalent external behavior is covered.

## When to revisit

Return to the maintainer for a new decision if contract work shows that the HTTP API cannot express a stable, externally testable contract without preserving substantial hidden Python behavior; if the existing database or files cannot be opened safely with a supported Rust stack on required platforms; if schema parity requires duplicate declarations; if measured runtime, memory, or fast-suite behavior conflicts with the stated priorities; or if a significant feature depends on Python-only in-process integration that cannot be preserved through the accepted provider and process boundaries. These are decision triggers, not claims that blockers exist.

## Consequences

- The first implementation work is contract and compatibility evidence, not another Rust bridge or service split.
- Python tests remain useful while Python owns the runtime. Convert user-visible behavior to process-level contracts selectively; do not mechanically port every internal assertion.
- The implementation gate must settle the schema rule, stack and crate choices, platform support, and rollback plan using current facts before backend coding.
- The replacement carries a large cutover risk. Stop and return for a new decision if a stated revisit condition is met; do not create an unapproved mixed-runtime split.

## Related sources

- [Workspace and Rust migration plan](../plans/rust-workspaces.md): D-01–D-07, R-16, R-17, R-24, and M-05–M-09.
- Issue #37: [measured command results](https://github.com/ifdotpy/codex-studio/issues/37#issuecomment-6109811509), [migration-strategy finding](https://github.com/ifdotpy/codex-studio/issues/37#issuecomment-6109905330), and [maintainer decision](https://github.com/ifdotpy/codex-studio/issues/37#issuecomment-6110789725).
- [Runtime database commit durability](2026-10-11-runtime-commit-durability.md).
- [Testing tiers](../testing.md).
- `workspaces/runtime/packages/operations-python/src/lib.rs` (current PyO3 protocol constant).
- `workspaces/runtime/apps/server/src/studio_api/` and `workspaces/client/apps/web/src/generated/` (current API schema and generated client types).
