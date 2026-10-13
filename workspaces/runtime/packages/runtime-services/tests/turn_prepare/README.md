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
   sequence cannot replace a newer manifest; equal sequences follow Python's
   replacement rule. Receipt delivery itself remains the worker's responsibility.

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
Only the two approved changes in [deviations.json](deviations.json) relax comparison.
No model text, renderer input, timestamp, receipt identity, or evidence reference
is discarded by normalization.

The default nextest witness set spans every family. The full matrix adds boundary
combinations. Neither creates synthetic load. Run with disposable `HOME`,
`CODEX_HOME`, `CLAUDE_CONFIG_DIR`, `TMPDIR`, `XDG_CACHE_HOME`, and `PIP_CACHE_DIR`;
set `RUSTUP_HOME`, `CARGO_HOME`, `COREPACK_HOME`, and the equipped read-only
`CODEX_AGENTS_PYTHON` explicitly. Use an external task-owned `CARGO_TARGET_DIR`
and a system/Cargo-only `PATH`, and leave `CODEX_BIN` unset.

```sh
cargo nextest run -p studio-runtime-services --test turn_prepare_corpus
just test-rust-runtime-corpora
just test-rust-runtime-corpora-mutations --only TP1,TP2,TP3,TP4,TP5,TP6,TP7,TP8,TP9,TP10,TP11,TP12 --full
```

Announce the last two as manual heavy runs. The mutation command copies sources,
checks the untouched baseline, and modifies only its disposable copy. A compiler
or infrastructure failure is not a detection. Its reports retain wall/CPU times
and logs. `TURN_PREPARE_CASE` selects a named diagnostic case; it must not be set
for final corpus or mutation evidence.
