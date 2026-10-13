# Context repair integration contract

For the runtime integrator. This slice preserves existing Python receipt meaning
without sending a model request or modifying native history.

## Change contract

The pure cores are `context_repair::decide` and
`historical_input_receipts::HistoricalBatch`. Checked constructors consume raw
stored records, including absent and null fields, and freeze them before native
reads. The effect adapter is `context_repair::recover`. It uses `RuntimeStore`
transactions and rechecks the complete snapshot and cached connection identity
before writing. No provider call occurs in a transaction.

Run `cargo nextest run -p studio-runtime-services --test context_repair_corpus`
with the isolated equipped Python selected by `CODEX_AGENTS_PYTHON` or
`STUDIO_HTTP_BACKEND_PYTHON`. The oracle imports the shared provider-home guard.
It never constructs Runtime, opens real provider history, or runs a provider.
The extracted recurring decision block is pinned by its enclosing function hash.
Historical batch tests call Python's actual capture and acceptance functions.
The default fail-point witness uses SQLite in memory. Set
`STUDIO_RUNTIME_CORPUS_FULL=1` for the durable restart matrix, or run
`just test-rust-runtime-corpora -p studio-runtime-services --test context_repair_corpus`; this is an
explicit opt-in run because filesystem durability can exceed the fast budget.

## Integration ports

Call `recover(store, agent, reader, manifests, now)` from the owning account
worker after draining its existing event stream. `HistoryReader::identity` must
return cached account, connection and generation without I/O. `read` returns
bounded native turns, native thread state, and the owning supervisor's operation
rows, all from that same transport identity. Return a sentinel row beyond the
per-batch operation bound; never silently truncate a journal scan into apparently
unique evidence. Missing pages or an incomplete history read must return an error. Arm reads through the account
worker's existing cancellation and deadline mechanism. Supply the unchanged
identity only while that connection remains current.

`DeliveredManifestWriter::remember(tx, agent, events)` is called after delivery
updates and before commit. Integrate the shared turn preparation acceptance
writer here; do not add a second manifest implementation. The provided default
returns `turn-prepare/delivered-manifest-writer` and rolls back. A failed or lost
store commit is never permission to submit input again.

Absent receipt tables return `store-bootstrap/context-repair-schema` without
writes. The renderer projection and event changes commit together. A replay after a
successful but lost response sees settled rows and emits no effects. A stopped,
changed, foreign, or malformed capture cannot release an uncertain input.

## Ownership of release decisions

Codex history reconciliation owns exact `not_applied` plus idle and identified
history, and inherited saved `submitted=False` plus the same idle/identity
checks. A native user item without identity prevents either inference.
The account worker owns transport `NotSent` and account-scoped proven-unsent
recovery. Claude owns exact frozen input rejection retry; this module never
requeues Claude input. Inherited Claude rejection without its saved frozen
request remains a visible unresolved obligation.

Missing, preparing, duplicate or contradictory native history stays uncertain.
The duplicate/contradiction hold is intentionally stricter than Python's
last-match acceptance. No absence-based replay is introduced.

## Explicit repair HTTP operation

`POST /api/context-repair` returns the genuine saved no-op result for a worker
without a native thread, a Claude worker, or a worker without candidate legacy
history. Active repair receipts and waits remain blocked. A native history
rewrite is partial and returns a named 501,
`context-repair/native-history-rewrite`, without writing.

The optional oversized-event/role-suffix rewrite, offline-steer correction and
obsolete failure-marker restoration repair Python-produced states. They can be
removed after a drained one-way cutover. Preserve original rollout bytes and
completed repair archives. Forks with unknown outcomes, source cleanup and late
terminal callbacks must be drained or explicitly held; a completed fork alone
is not proof that those obligations finished. These lifecycle effects are not
implemented by this slice. Recurring uncertain-input reconciliation remains
necessary after cutover.

## Mutation evidence

`mutations.json` names each seeded rule change. Use the unchanged shared runner
with `--manifest` pointing here and `--filter binary(context_repair_corpus)`.
The runner builds a disposable copy and counts only failed tests as detections.
Build failures and unavailable fixture infrastructure do not count.
