# Submission compiler corpus

## Change contract

This directory verifies the pure [turn preparation compiler](../../src/turn_prepare.rs)
for the account-worker adapter's implementer. The compiler consumes an immutable,
validated snapshot plus ordered events. It produces provider inputs, a renderer
transcript, metadata patches, a provisional manifest, complaint obligations, and
artifact intents. It performs no database, filesystem, provider, or clock I/O.
The limits and public types live in the Rust module; [cases.json](cases.json)
owns the witnesses and [mutations.json](mutations.json) owns seeded defects.

The account worker's `preparation::PreparationInput` is the intended caller.
This change verifies the compiler interface through a disposable corpus adapter;
it does not wire that worker or make the Rust runtime ready for real providers.
Thread/workspace preparation, authorization, budget admission, model settings,
Claude frozen retries, and transactional publication remain separate services.

## Adapter obligations

1. Gather schema-validated records and immutable guidance, including any frozen
   native role and progress-path observations. Normalize legacy mode-only root
   settings and worker statuses while gathering. The snapshot contains only
   delivered manifests, never a provisional or uncertain manifest. Select
   complaints and workers in their stored order; the compiler sorts cleanup IDs.
2. Call `compile`. `ArtifactsRequired` is a defer outcome with relative paths and
   exact bytes. Resolve paths beneath the adapter's state root, create exclusively,
   and verify byte equality on replay. A conflicting existing body or write failure
   cannot produce a submission. Recompile with the verified absolute paths in
   `artifact_paths`. The returned intents remain available for transactional audit.
3. Revalidate actor/operation/epoch/event order, ownership, admission and settings
   in the preparation transaction. Merge each metadata patch; do not replace
   unrelated metadata. Persist the transcript and its optional fulltext, manifest,
   and exact operation identity before one native submission. Identical compilation
   is deterministic; this function never authorizes resending an unknown request.
4. Feed the validated native receipt to `accept`, with the delivered manifest for
   that exact epoch read in the settlement transaction. Apply its manifest upsert,
   idempotent reminder insert and transcript turn association together. Rejected,
   unknown, empty-turn, or mismatched receipts produce no changes. A late lower
   compilation base produces the typed `stale` outcome, including two preparations
   compiled from the same base and settled replays. Apply no stale effects. The
   adapter reads the current manifest and applies this compare-and-set in one
   transaction. Receipt delivery itself remains the worker's responsibility.

Actor and ownership fields come from authenticated stored state; guidance and
resolved paths are adapter observations. Event text and attachment selections can
originate with callers. Timestamps and the operation identity are fixed by the
server before compilation. The compiler does not query those authorities again.

## Oracle and comparison

[oracle.py](oracle.py) imports the shared `oracle_guard` before importing
application code. A minimal host binds the real Python `model_turn_context`,
`model_known_context`, `model_event_text`, `bounded_event`, `Runtime.item`,
complaint selection/message, attachment methods, receipt clocks, and
`remember_context_manifest`. SQLite is in memory; guidance and child-result
artifacts live in disposable directories. No `Runtime` is constructed and
`Runtime.start` is never called. Process execution and network operations are
forbidden by that guard before any application import. Its self-test uses only
a synthetic forbidden path; provider-home paths are rejected before resolution.

The Rust test compares every output field, including types, missing fields,
array order, full original transcript inputs, metadata patches, artifact bytes,
and acceptance transitions. Failures identify the case and first differing path.
Only the four approved changes in [deviations.json](deviations.json) relax comparison.
The normalizer walks recorded literal and generated-object spans. It preserves
caller text, follow-up text, guidance, embedded JSON strings, and excerpts exactly;
it parses only newly generated context/summary objects. Every rewritten producer
payload compares byte for byte, including nested key order, duplicate-key position,
and exponent-form floats. Manifest values use a one-to-one token mapping. The
original parent excerpt must equal the first 20,000 characters of fulltext before
comparison. Oracle invariants and seeded defects defend these boundaries.

Acceptance is checked against actual manifest/reminder rows and transcript turn IDs
written by Python `Runtime.bind_start`, not its return value. The receipt identity
and stale-base gates are explicit adapter contract checks; Python's original writer
alone does not implement the new stale-base policy. Competing preparations are
accepted in both orders, followed by a fresh preparation and late replay.

Malformed non-string work-review checks/files members and array/object child IDs
or names are explicitly refused with `InvalidEvidence`; scalar child identities
use Python spelling. This authorized boundary is tested independently of Python's
more permissive stringification. There is no hard total prompt-byte ceiling:
synthetic budgets are soft when their reference floor exceeds the assigned share,
and context and bypass events are unbounded. An independent assertion preserves a
23,000-byte reference while enforcing the transcript's hard character ceiling.

### What this corpus does not establish

The host supplies agent lookup/naming, guidance acquisition, and progress-file
provisioning; it replaces item indexing and UI publication. It does not establish
metadata merge preservation, item/fulltext overwrite or deletion, missing versus
deleted owner behavior, real guidance origins, filesystem failure handling, search
or UI effects, or durable receipt transaction atomicity. It checks archive create,
identical replay, conflicting body, bytes, and cleanup only in disposable roots.
`Runtime.item` runs for new user items with supplied inputs; no-input, existing-item,
assistant/observer, `index_search=False`, and absent-input fallback branches are not
covered. Admission, authentication, operation uniqueness, and snapshot validity
remain caller duties. Snapshot equality is an immutability assertion, not proof of
no hidden I/O; source inspection is the current purity evidence.

The default nextest witness set spans every family. The full matrix adds boundary
combinations. Neither creates synthetic load. Run with disposable `HOME`,
`CODEX_HOME`, `CLAUDE_CONFIG_DIR`, `TMPDIR`, `XDG_CACHE_HOME`, and `PIP_CACHE_DIR`;
set `RUSTUP_HOME`, `CARGO_HOME`, `COREPACK_HOME`, and the equipped read-only
`CODEX_AGENTS_PYTHON` explicitly. Use an external task-owned `CARGO_TARGET_DIR`
and a system/Cargo-only `PATH`, and leave `CODEX_BIN` unset.

```sh
cargo nextest run -p studio-runtime-services --test turn_prepare_corpus
just test-rust-runtime-corpora
just test-rust-runtime-corpora-mutations --only TP1,TP2,TP3,TP4,TP5,TP6,TP7,TP8,TP9,TP10,TP11,TP12,TP13,TP14,TP15,TP16,TP17,TP18,TP19,TP20,TP21,TP22,TP23,TP24,TP25,TP26,TP27,TP28,TP29,TP30,TP31,TP32,TP33 --full
```

Announce the last two as manual heavy runs. The mutation command copies sources,
checks the untouched baseline, and modifies only its disposable copy. A compiler
or infrastructure failure is not a detection. Its reports retain wall/CPU times
and logs. `TURN_PREPARE_CASE` selects a named diagnostic case; it must not be set
for final corpus or mutation evidence.
