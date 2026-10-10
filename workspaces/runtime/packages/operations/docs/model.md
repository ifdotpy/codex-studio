# Operation lifecycle model

## Selected behavior slice

This model covers one mutating tool request after authorization and input
validation: scoped reservation, one execution claim, result recording, and
recovery. `studio-operations` makes the lifecycle decisions. Python owns
authorization, SQLite reads and writes, record and result shapes, timing, and
provider effects. The transition function returns a state and effect intent; it
does not perform effects or claim provider-level exactly-once execution.

The boundary input consists of the operation's canonical request identity and
content fingerprint, attempt account/agent/thread epoch, optional process
generation on response events, event ID, and expected state revision. The
adapter canonicalizes content and derives IDs before calling the pure API. The
private PyO3 module accepts Rust `State` and `Event` as serde JSON strings and
returns a serde JSON `Decision` in-process. This is FFI data encoding, not a
subprocess or public wire protocol. Epoch and generation identify an attempt;
neither is part of operation lookup identity.

The traced Python caller path is `Runtime.request` → `reserve_tool_request` →
executor queue → `Runtime.dynamic` → `begin_tool_request` → handler/effect →
`finish_tool_request` → saved response/reconciliation:

- Request admission verifies the account connection, reserves `item/tool/call`,
  then queues the handler (`codex_runtime.py:8154-8194`).
- Reservation derives a scoped key, compares signature and actor, stores the
  operation record and aliases (`codex_tool_requests.py:316-380`). Its durable
  fields are described by `ToolRequestRecord` (`codex_records.py:600-630`).
- The handler rereads the receipt, checks epoch/turn scope, and claims queued
  execution once (`codex_runtime.py:8581-8615`; `codex_tool_requests.py:382-394`).
- Completion saves result/outcome and protects settled outcomes from late
  transport errors (`codex_runtime.py:8846-8863`; `codex_tool_requests.py:396-439`).
  Recovery refreshes saved results and operation evidence (`codex_tool_requests.py:442-478`).
- Status/cancel resolves the exact receipt; a missing one reports unknown, queued
  cancellation proves not-applied, and running cancellation only records the
  request (`codex_tool_requests.py:542-595`).

## States

Each state below is an operation lifecycle status. The operation also retains
its immutable scoped identity, content fingerprint, revision, and the
generation/epoch captured for the current attempt.

| State                     | Meaning                                                                                                                                                                                                                                                 | Python basis and contract test                                                                                                                                                                                                                                                                                                                                                        |
| ------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Absent`                  | No active reservation is present in this snapshot. A truly new slot has revision zero; a compacted settled request retains a typed tombstone with identity, content, attempt, and outcome. A nonzero revision without that metadata cannot be admitted. | `reserve_tool_request` creates a `queued` record only when no record exists (`codex_tool_requests.py:333-368`); missing lookup returns `not_found/unknown` (`:567-575`). Tests `tool-request-contract.py:153-162`.                                                                                                                                                                    |
| `Reserved`                | Identity and content are saved; execution is not claimed yet.                                                                                                                                                                                           | Record starts `queued` / `pending` (`codex_tool_requests.py:339-368`). Tests `tool-request-contract.py:93-111, 175-188`.                                                                                                                                                                                                                                                              |
| `Dispatched`              | Exactly one handler claim succeeded; its result may still be pending.                                                                                                                                                                                   | `begin_tool_request` moves only a non-cancelled `queued` record to `running` (`codex_tool_requests.py:382-394`). Tests `tool-request-contract.py:93-111, 141-151, 175-188, 242-255`.                                                                                                                                                                                                  |
| `CancelRequested`         | Cancellation arrived after the execution claim; outcome remains pending.                                                                                                                                                                                | Running cancellation only sets `cancelRequested`; the stage/outcome remain running/pending (`codex_tool_requests.py:577-582`). Test `tool-request-contract.py:141-151`.                                                                                                                                                                                                               |
| `NotApplied`              | Positive evidence says no effect was applied.                                                                                                                                                                                                           | Queued cancellation and exact pre-write/read-only failures may settle `not_applied` (`codex_tool_requests.py:81-121, 577-582`). Tests `tool-request-contract.py:133-139, 175-188`; `request-reconciliation-contract.py:60-75`.                                                                                                                                                        |
| `Applied`                 | A successful tool result normally establishes application; `orchestration_servers` may return its explicit `applied`, `not_applied`, or `unknown` outcome. A nested operation receipt alone does not prove the enclosing tool request succeeded.        | `request_result_outcome` maps ordinary success to applied but preserves the paired `orchestration_servers` outcome (`codex_tool_requests.py:85-94`); committed operation evidence beside inferred failure preserves unknown (`:414-478`). Tests `tool-request-contract.py:93-111, 141-151, 227-240`; `request-reconciliation-contract.py:89-96`; `server-exec-signed-integration.py`. |
| `Unknown`                 | The effect may have happened; safe retry is prohibited. A committed nested operation receipt alongside a failed/inferred rejection remains unknown and is surfaced as evidence, without claiming the tool request applied.                              | Unclassifiable mutating failures map to unknown (`codex_tool_requests.py:85-127`); a committed receipt prevents inferred `not_applied` (`:414-478`); missing receipt reports unknown (`:567-575`). Tests `tool-request-contract.py:153-162, 175-188`; `request-reconciliation-contract.py:89-96`; `request-rejection-outcome-contract.py`.                                            |
| `CancelledBeforeDispatch` | Reservation was cancelled while still queued; no handler can claim it.                                                                                                                                                                                  | Queued cancel writes cancelled/not_applied, and begin refuses it (`codex_tool_requests.py:382-394, 577-582`). Tests `tool-request-contract.py:133-139, 175-188`.                                                                                                                                                                                                                      |

`Absent` is represented by an empty operation slot in `State`; it is not a
persisted request state. A compacted settled slot has no active `Operation` but
retains `OperationTombstone`, so same-ID retries still return the saved outcome.
The domain outcome enum is `NotApplied`, `Applied`, or `Unknown`, corresponding
to R-15. `CancelRequested` keeps a pending outcome until a result or uncertainty
event arrives.

## Events

Every event carries a named event ID, operation identity, and attempt epoch.
Provider/recovery events may carry process generation. A supplied generation is
checked against the dispatch attempt; absence follows the current Python call
site's rule. Revision mismatches, identity mismatches, and stale supplied
generations/epochs are explicit rejections. The transition state does not keep
an unbounded event-ID set: duplicate delivery is idempotent from operation
identity, revision, and phase (for example, a second claim from `Dispatched`
cannot emit another intent, and a second settlement of a terminal outcome is a
no-op). Event IDs remain correlation data, not the deduplication store.

| Event                                                                      | Meaning, Python mapping, and contract test                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| -------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Admit`                                                                    | Reserve a new scoped request, or return the saved state for an identical retry, including when attempt epoch has advanced. Identity scope is a closed enum preserving existing key shapes: tool request uses account+thread and canonical call/request ID (explicit-ID alias normalization stays at the adapter); native action uses a global request ID. Content fingerprint covers tool+arguments+agent, or native action agent+action+context respectively. `reserve_tool_request` compares signature and actor before refresh/replay (`codex_tool_requests.py:316-368`); native action receipts compare their signature (`codex_native_action_receipts.py:6-35`). |
| `ClaimDispatch`                                                            | Claim execution once. `begin_tool_request` accepts only queued and uncancelled (`codex_tool_requests.py:382-394`). Tests `tool-request-contract.py:93-111, 133-139, 242-255`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `Cancel`                                                                   | Cancel before execution or record a cancellation request after execution began (`codex_tool_requests.py:542-582`). Tests `tool-request-contract.py:133-151`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `ServerRestarted`                                                          | Recovery decides from core state: a reserved request is cancelled before dispatch; a dispatched/cancel-requested request becomes interrupted/unknown. Python keeps the existing error/result payloads (`codex_tool_requests.py:217-235`). Tests `tool-request-contract.py:175-188`; Rust test `server_restart_settles_reserved_and_preserves_dispatched_uncertainty`.                                                                                                                                                                                                                                                                                                 |
| `AppliedResponse`                                                          | Record a valid successful result whose outcome is applied. The core settles a saved result from `Reserved` and resolves a late valid result from `Unknown`; the adapter does not branch on current state. Paired `orchestration_servers` outcomes of `not_applied` or `unknown` use rejection/failure events (`codex_tool_requests.py:81-94, 396-439`). Tests `tool-request-contract.py:141-151, 227-240`; `server-exec-signed-integration.py`; Python test `test_cached_success_before_local_dispatch_is_applied`; Rust tests `r18_saved_result_settles_reserved_but_not_absent_operation`, `late_valid_result_is_distinct_from_committed_operation_receipt`.        |
| `DefinitiveRejection`                                                      | Record an outcome only when the adapter has positive typed evidence of pre-effect rejection; the core applies that proof to either an active or unknown state. Python recognizes exact read-only and selected pre-write failures and sends the same proof without branching on lifecycle state (`codex_tool_requests.py:91-121, 414-478`). Tests `request-reconciliation-contract.py:60-75`; Rust test `exact_prewrite_rejection_resolves_unknown_in_core`.                                                                                                                                                                                                           |
| `Failure(Timeout/ConnectionLost/InvalidPostExecutionResponse/ProcessLost)` | Preserve uncertainty after dispatch; a saved failure result may also reconcile a newly reserved legacy receipt to unknown before local dispatch. Generation is optional for compatibility and checked when supplied. Mutating failures default to unknown (`codex_tool_requests.py:85-127, 382-394`). Restart recovery uses `ServerRestarted`. Tests `tool-request-contract.py:153-162, 175-188`; `native-action-receipts-contract.py:66-89`; Rust test `r15_cached_failure_before_local_dispatch_remains_unknown`.                                                                                                                                                   |
| `AppliedEvidence`                                                          | Resolve unknown from typed proof that establishes this request's application. The Python tool-request caller reports a late valid saved result through `AppliedResponse`; a committed operation receipt uses the separate `CommittedOperationReceipt` event and leaves an unresolved request unknown (`codex_tool_requests.py:414-478`). Tests `request-reconciliation-contract.py:89-96, 215-231`; `tool-request-contract.py:153-162, 309-350`; Rust tests `committed_receipt_with_inferred_failure_stays_unknown`, `late_valid_result_is_distinct_from_committed_operation_receipt`.                                                                                |
| `CommittedOperationReceipt`                                                | A nested operation committed under the same durable request key, but a failed/inferred tool result conflicts with that evidence. Preserve `Unknown`; do not settle `Applied` or `NotApplied` (`codex_tool_requests.py:157-165, 414-478`). Tests `request-reconciliation-contract.py:89-96`; `request-rejection-outcome-contract.py`; Rust test `committed_receipt_with_inferred_failure_stays_unknown`.                                                                                                                                                                                                                                                               |
| `NonExecutionEvidence`                                                     | Resolve unknown only with a typed positive proof tied to the same operation and attempt. Proof kinds mirror queued cancellation, exact pre-write/read-only rejection, and deterministic review-child absence (`codex_tool_requests.py:81-121, 455-478`). Tests `tool-request-contract.py:133-139`; `request-reconciliation-contract.py:60-75`. Review-child absence has no direct fixture in this selected contract file and remains an adapter proof.                                                                                                                                                                                                                |
| `ReceiptMissing`                                                           | Report absent lookup as unknown; do not infer non-execution (`codex_tool_requests.py:542-575`). Test `tool-request-contract.py:153-162`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |

All other state/event combinations return a typed rejection or an idempotent
no-op decision. They do not panic.

## Decisions and effect intents

| Decision / intent            | Meaning and Python mapping                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| ---------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Reserve` intent             | Persist identity/content and `Reserved` status atomically. Mirrors insertion in `reserve_tool_request` (`codex_tool_requests.py:339-368`); test `tool-request-contract.py:93-111`.                                                                                                                                                                                                                                                                                                                         |
| `ReturnExisting`             | Reuse the saved operation for exact identity/content; emits no dispatch intent. Mirrors replay refresh and return (`codex_tool_requests.py:333-338`); tests `tool-request-contract.py:93-111`; `native-action-receipts-contract.py:32-56, 66-78`.                                                                                                                                                                                                                                                          |
| `Dispatch` intent            | Persist the one execution claim before invoking the handler. Mirrors `begin_tool_request` (`codex_tool_requests.py:382-394`); tests `tool-request-contract.py:93-111, 242-255`.                                                                                                                                                                                                                                                                                                                            |
| `RequestCancellation` intent | Record cancellation while dispatched; do not claim non-execution. Mirrors running cancellation (`codex_tool_requests.py:577-582`); test `tool-request-contract.py:141-151`.                                                                                                                                                                                                                                                                                                                                |
| `PersistOutcome` intent      | Save `Applied`, `NotApplied`, or `Unknown` with the event/state revision. Mirrors `finish_tool_request` (`codex_tool_requests.py:396-439`); tests `tool-request-contract.py:133-162`.                                                                                                                                                                                                                                                                                                                      |
| `ReconcileEvidence` intent   | Persist the outcome supported by exact evidence, without another dispatch. A committed nested operation receipt persists `Unknown` when the enclosing tool result conflicts; a late valid result may persist `Applied` (`codex_tool_requests.py:414-478`); tests `request-reconciliation-contract.py:89-96, 215-231`.                                                                                                                                                                                      |
| `NoEffect`                   | Return an unchanged/no-effect decision for duplicate delivery or an already settled phase; never execute an effect. Mirrors terminal-result protection and single begin (`codex_tool_requests.py:382-394, 396-439`); tests `tool-request-contract.py:141-151, 242-255`.                                                                                                                                                                                                                                    |
| `ReceiptMissingUnknown`      | Return an unchanged absent/pending/unknown lookup result, with no outcome effect. Mirrors `request_action` not-found handling (`codex_tool_requests.py:567-575`); test `tool-request-contract.py:153-162`. If the same snapshot has an authoritative applied/not-applied outcome or tombstone, reject the contradictory missing-receipt event (`TransitionError::ConflictingEvidence`); Rust regression test `r15_missing_receipt_cannot_override_saved_definitive_outcome` covers both terminal outcomes. |
| `Reject(TransitionError)`    | Return a closed error code for changed content, mismatched scope/revision, invalid phase, stale supplied generation/epoch, or unsupported proof. Mirrors identity validation and transition guards (`codex_tool_requests.py:316-394`; `codex_native_action_receipts.py:6-35`); tests `tool-request-contract.py:93-111, 164-172`; `native-action-receipts-contract.py:45-64`.                                                                                                                               |

Every decision contains the expected prior revision for compare-and-persist and
the resulting state revision. Persistence of the state and intents is the
caller's atomicity boundary. Effects outside that transaction use stable
identities and reconciliation (R-17).

## Checkable invariants

- **R-14 — request identity:** For a request identity `(typed scope, request
ID)`, equal content returns its saved operation/state or compacted settled
  tombstone without a second dispatch intent; unequal content is rejected
  before any effect intent.
  An absent receipt leaves outcome `Unknown`, never `NotApplied`.
- **R-15 — outcome uncertainty:** Outcomes are closed to `NotApplied`, `Applied`,
  and `Unknown`. Timeout, connection loss, malformed post-dispatch response, or
  process loss cannot map to `NotApplied`. `Unknown` can leave only through
  identity-matched evidence that proves the tool result or non-execution. A
  committed nested operation receipt alongside an inferred failure preserves
  `Unknown`; no fresh ID/re-dispatch transition exists.
- **R-17 — atomic intent boundary:** A decision carries the expected state
  revision, next revision, and all persistence/effect intents together. The pure
  crate has no persistence/effect implementation and makes no provider
  exactly-once claim.
- **R-18 — identity/generation/cancellation:** Duplicate events are idempotent;
  out-of-order, old supplied process-generation, and old account/thread-epoch events
  cannot regress the operation. Cancellation while reserved prevents dispatch;
  cancellation after dispatch cannot erase pending or unknown outcome.

Property tests check: settled states never regress; at most one `Dispatch`
intent is emitted for one operation identity; and committed operation receipts
do not incorrectly settle an unknown tool result as applied.

## Existing Python scenarios replayed as neutral fixtures

`tests/fixtures/python-contract-cases.json` records inputs/expected states for
cases from:

- `tool-request-contract.py:93-111` — semantic retry returns one operation and
  changed content is rejected.
- `tool-request-contract.py:133-162` — queued cancellation is not applied,
  running cancellation remains pending, failures/missing receipts stay unknown.
- `tool-request-contract.py:175-188` — restart distinguishes queued,
  dispatched, and cached result.
- `tool-request-contract.py:242-255` — only one concurrent begin succeeds.
- `tool-request-contract.py:227-240` — a delayed saved result reconciles before
  status or cancellation is applied.
- `request-reconciliation-contract.py:89-96` — committed operation evidence
  prevents false `NotApplied` while preserving the request's unknown outcome.
- `native-action-receipts-contract.py:32-89` — exact retry returns same receipt;
  changed identity/content rejects; uncertain response/restart retains receipt.

The fixture is declarative JSON with only named strings, integer epochs and
generations, and expected enum names. It is not a wire-protocol definition.

## Confirmed divergences (AC-05)

The maintainer confirmed all six model decisions on 2026-10-10. The items below
record the accepted boundary and compatibility choices for this integration.

1. **Typed request-ID scope.** The core models tool requests as account/thread
   scope plus the canonical call/request ID. Existing alias normalization and
   tool-specific request prefixes stay in Python. Epoch and generation are
   attempt attributes, not identity, so an exact retry after an epoch change
   returns its saved receipt without dispatch. Native-action receipts use the
   distinct global request-ID scope in the model but remain Python-owned in this
   change.
2. **Typed evidence resolves uncertainty conservatively.** The confirmed
   review-child absence proof may settle `not_applied`. A committed operation
   receipt that arrives with an inferred failure is conflicting evidence: it
   preserves `unknown` and is exposed as `operationApplied=true`; it does not
   settle the tool request as applied. This preserves the original Python
   behavior (`d965e33e`) and both contract assertions. A missing receipt is
   never proof.
3. **Optional generation check.** Completion and uncertainty events accept an
   optional generation. When supplied it must match the dispatch attempt;
   absence follows the existing Python call site's rule. Requiring generation
   on every completion remains a later M-05 change.
4. **Typed rejection proof, unchanged classification.** Python's exact
   read-only and pre-write text classification remains the producer of
   `ReadOnlyRejection` or `ExactPreWriteRejection`. Its current settled and
   unknown outcomes stay the same; Rust consumes the typed proof and has no
   message-text table.
5. **Internal lifecycle state, compatible persisted labels.** Rust models
   cancellation and interruption as typed states. The adapter maps decisions to
   today's stored stage/outcome pairs: queued cancellation remains
   `cancelled/not_applied`, dispatched cancellation remains `running/pending`,
   and restart recovery remains `cancelled/not_applied` or
   `interrupted/unknown`. No new persisted status string is exposed.
6. **Compacted slots retain typed tombstones.** The core rejects a nonzero
   empty slot without identity, content, attempt, and settled-outcome metadata.
   The current tool-request store does not compact receipts, so no tombstone or
   database column is added here.

## Preflight contract

The logical command identity is a typed call-site scope plus request ID and
canonical content fingerprint. A retry after a lost successful response returns
the saved state/result, including if the attempt epoch advanced; changed content
rejects before an effect. Legacy records need no offline migration or added
revision key. The adapter derives revision from their stored lifecycle fields:
queued/pending is 0, running/pending is 1, running with cancellation requested
is 2, and settled stages are 3. It maps existing stage/outcome pairs to typed
core state before each transition. The caller stamps authenticated actor, time,
and available process generation; the core does not invent provenance or
clocks.

For a never-seen empty operation slot, expected revision is zero. If a caller
later compacts a settled operation, it must retain a typed tombstone containing
scoped identity, content fingerprint, attempt, settled outcome, and revision.
The current tool-request path retains its records and does not use compaction.

There is no external timeout budget in this slice: deadlines/timeouts arrive as
typed events. Client/intermediary timeout sizing, durable presentation stages,
and async-operation policy remain with the Python caller and are not inferred by
this library.

## Verification and isolation evidence

The Rust core's `transition` uses in-memory values and checked integer revision
arithmetic. The PyO3 binding only parses and serializes the core's serde types,
and catches Rust panics as Python exceptions. The Python adapter only converts
stored records to states/events and returned decisions to existing fields; it
does no I/O. The Python contract test reads
`tests/fixtures/python-contract-cases.json` directly and replays it through the
installed extension. The Rust fixture test embeds that same file at compile
time.
