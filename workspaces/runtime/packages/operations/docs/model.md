# Operation lifecycle model

## Selected behavior slice

This model covers one mutating tool request after authorization and input
validation: scoped reservation, one execution claim, result recording, and
recovery. Python remains authoritative for admission, persistence, dispatch,
permissions, and effects. The Rust transition function proposes a state and
effect intent; it neither performs them nor claims provider-level exactly-once
execution.

The boundary input consists of the operation's canonical request identity and
content fingerprint, attempt account/agent/thread epoch, optional process
generation on response events, event ID, and expected state revision. The
adapter canonicalizes content and derives IDs before calling the pure API. This
crate does not define that encoding or a wire protocol. Epoch and generation
identify an attempt; neither is part of operation lookup identity.

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

| State                     | Meaning                                                                                                                                                                                                                                                 | Python basis and contract test                                                                                                                                                                                                                    |
| ------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Absent`                  | No active reservation is present in this snapshot. A truly new slot has revision zero; a compacted settled request retains a typed tombstone with identity, content, attempt, and outcome. A nonzero revision without that metadata cannot be admitted. | `reserve_tool_request` creates a `queued` record only when no record exists (`codex_tool_requests.py:333-368`); missing lookup returns `not_found/unknown` (`:567-575`). Tests `tool-request-contract.py:153-162`.                                |
| `Reserved`                | Identity and content are saved; execution is not claimed yet.                                                                                                                                                                                           | Record starts `queued` / `pending` (`codex_tool_requests.py:339-368`). Tests `tool-request-contract.py:93-111, 175-188`.                                                                                                                          |
| `Dispatched`              | Exactly one handler claim succeeded; its result may still be pending.                                                                                                                                                                                   | `begin_tool_request` moves only a non-cancelled `queued` record to `running` (`codex_tool_requests.py:382-394`). Tests `tool-request-contract.py:93-111, 141-151, 175-188, 242-255`.                                                              |
| `CancelRequested`         | Cancellation arrived after the execution claim; outcome remains pending.                                                                                                                                                                                | Running cancellation only sets `cancelRequested`; the stage/outcome remain running/pending (`codex_tool_requests.py:577-582`). Test `tool-request-contract.py:141-151`.                                                                           |
| `NotApplied`              | Positive evidence says no effect was applied.                                                                                                                                                                                                           | Queued cancellation and exact pre-write/read-only failures may settle `not_applied` (`codex_tool_requests.py:81-121, 577-582`). Tests `tool-request-contract.py:133-139, 175-188`; `request-reconciliation-contract.py:60-75`.                    |
| `Applied`                 | A successful result or authoritative committed-operation receipt proves application.                                                                                                                                                                    | Successful result maps to applied (`codex_tool_requests.py:81-90`); committed receipt takes precedence over inferred failure (`:419-439`). Tests `tool-request-contract.py:93-111, 141-151, 227-240`; `request-reconciliation-contract.py:89-96`. |
| `Unknown`                 | The effect may have happened; safe retry is prohibited.                                                                                                                                                                                                 | Unclassifiable mutating failures map to unknown (`codex_tool_requests.py:91-121`); missing receipt reports unknown (`:567-575`). Tests `tool-request-contract.py:153-162, 175-188`; `request-reconciliation-contract.py:78-96`.                   |
| `CancelledBeforeDispatch` | Reservation was cancelled while still queued; no handler can claim it.                                                                                                                                                                                  | Queued cancel writes cancelled/not_applied, and begin refuses it (`codex_tool_requests.py:382-394, 577-582`). Tests `tool-request-contract.py:133-139, 175-188`.                                                                                  |

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
| `AppliedResponse`                                                          | Record a valid successful result (`codex_tool_requests.py:81-90, 396-439`). Tests `tool-request-contract.py:141-151, 227-240`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| `DefinitiveRejection`                                                      | Record an outcome only when the adapter has positive evidence of pre-effect rejection. Python recognizes exact read-only and selected pre-write failures (`codex_tool_requests.py:91-121`). Tests `request-reconciliation-contract.py:60-75`.                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `Failure(Timeout/ConnectionLost/InvalidPostExecutionResponse/ProcessLost)` | Preserve uncertainty after dispatch. Generation is optional for compatibility and checked when supplied. Mutating failures default to unknown (`codex_tool_requests.py:91-121`); startup recovery maps interrupted running to unknown (`:217-231`). Tests `tool-request-contract.py:153-162, 175-188`; `native-action-receipts-contract.py:66-89`.                                                                                                                                                                                                                                                                                                                    |
| `AppliedEvidence`                                                          | Resolve unknown from exact committed-operation receipt or late valid result (`codex_tool_requests.py:419-439, 442-478`). Tests `request-reconciliation-contract.py:89-96, 215-231`; `tool-request-contract.py:153-162, 309-350`.                                                                                                                                                                                                                                                                                                                                                                                                                                      |
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
| `ReconcileEvidence` intent   | Persist the outcome supported by exact evidence, without another dispatch. Mirrors result/operation receipt reconciliation (`codex_tool_requests.py:442-478`); tests `request-reconciliation-contract.py:89-96, 215-231`.                                                                                                                                                                                                                                                                                  |
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
  identity-matched positive evidence; no fresh ID/re-dispatch transition exists.
- **R-17 — atomic intent boundary:** A decision carries the expected state
  revision, next revision, and all persistence/effect intents together. The pure
  crate has no persistence/effect implementation and makes no provider
  exactly-once claim.
- **R-18 — identity/generation/cancellation:** Duplicate events are idempotent;
  out-of-order, old supplied process-generation, and old account/thread-epoch events
  cannot regress the operation. Cancellation while reserved prevents dispatch;
  cancellation after dispatch cannot erase pending or unknown outcome.

Property tests check: settled states never regress; at most one `Dispatch`
intent is emitted for one operation identity; and `Unknown` leaves only after an
evidence event.

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
  prevents false `NotApplied`.
- `native-action-receipts-contract.py:32-89` — exact retry returns same receipt;
  changed identity/content rejects; uncertain response/restart retains receipt.

The fixture is declarative JSON with only named strings, integer epochs and
generations, and expected enum names. It is not a wire-protocol definition.

## Divergences and approval questions (AC-05)

These source behaviors do not compose into one unambiguous general contract.
Rust choices below are conservative and **need maintainer approval before they
can be treated as compatibility decisions**. The lead has given the dated
decisions below; each remains pending maintainer confirmation in the PR.

1. **Request-ID scope differs by call site.** Tool requests key by account/thread
   plus call ID, or by a tool-namespaced explicit request ID; aliases can locate
   that same canonical request (`codex_tool_requests.py:233-255, 316-376`). Native
   actions key globally by request ID and sign agent/action/context
   (`codex_native_action_receipts.py:6-35`). **Lead decision 2026-10-10, pending
   maintainer confirmation in the PR:** model identity as a closed typed
   call-site scope plus request ID. Epoch and generation are attempt attributes,
   not identity. This prevents a same-ID/same-content retry after an epoch change
   from dispatching again. Existing alias normalization stays in the adapter.
   A test and fixture cover newer-epoch replay.
2. **Unknown may be cleared by different strengths of evidence.** General
   operation evidence is retained by request reconciliation
   (`request-reconciliation-contract.py:89-96, 215-231`), while review recovery
   marks an absent deterministic child as `not_applied`
   (`codex_tool_requests.py:455-478`). **Lead decision 2026-10-10, pending
   maintainer confirmation in the PR:** unknown leaves only on a typed positive
   proof, with closed kinds mirroring queued cancellation, exact pre-write or
   read-only rejection, and deterministic review-child absence. The first two
   are direct no-dispatch/pre-effect observations. Review-child absence is
   weaker: it infers non-execution from a registry query, but Python currently
   accepts it for review. A missing operation/receipt is never proof.
3. **Generation is enforced inconsistently.** Turn recovery checks saved versus
   live supervisor generation and event/epoch identity (`codex_turn_recovery.py:342-376,
455-480`), but general `finish_tool_request` receives request key, result,
   and optional outcome without event generation (`codex_tool_requests.py:396-440`).
   **Lead decision 2026-10-10, pending maintainer confirmation in the PR:**
   completion and uncertainty events accept optional generation. A present
   generation must match the dispatch attempt; absence follows the Python
   call-site rule. Making generation mandatory for every mutating completion is
   a proposed M-05 tightening, not current behavior. Tests cover both forms.
4. **Error classification is not uniform.** Some exact validation strings and
   read-only errors prove `not_applied`; other mutation failures, even when they
   resemble validation, remain `unknown` (`codex_tool_requests.py:81-121`;
   `request-reconciliation-contract.py:60-96`). **Lead decision 2026-10-10,
   pending maintainer confirmation in the PR:** Rust receives typed
   `DefinitiveRejection` proof and never classifies human-readable text. The
   adapter owns current Python classification.
5. **Cancel stage names differ by timing and recovery.** Queued cancellation is
   `cancelled/not_applied`; running cancellation only sets a flag, while restart
   changes queued to cancelled and running to interrupted/unknown
   (`codex_tool_requests.py:217-231, 577-582`). **Lead decision 2026-10-10,
   pending maintainer confirmation in the PR:** persisted stage vocabulary is
   adapter-owned and not part of the pure Rust state API.
6. **Compaction is not defined by the selected Python path.** The current
   request record is replayed from its saved identity/signature
   (`codex_tool_requests.py:316-368`), but this path does not specify dropping
   those fields. The lead directed that a compacted slot retain its revision;
   independent review showed revision alone cannot distinguish a same-ID retry
   from a new request and can permit a second dispatch. Rust therefore retains
   a typed tombstone with identity/content/attempt/settled outcome and rejects
   nonzero empty slots lacking that metadata. This choice needs maintainer
   confirmation in the PR.

## Preflight contract

The logical command identity is a typed call-site scope plus request ID and
canonical content fingerprint. A retry after a lost successful response returns
the saved state/result, including if the attempt epoch advanced; changed content
rejects before an effect. The caller retains the authoritative state revision
and checks it atomically while persisting state and
intents. No replay is authorized for `Unknown`; only exact same-ID recovery is
allowed. The evaluator has no legacy wire versions yet; serialization is
optional and protocol-version compatibility belongs to the later bridge. The
caller stamps authenticated actor, time, and input generation; this pure layer
does not invent provenance or clocks.

For a never-seen empty operation slot, expected revision is zero. Compaction
retains a typed tombstone containing the scoped identity, content fingerprint,
attempt, settled outcome, and caller-assigned revision. This lets an exact retry
return the saved outcome and rejects changed content. A nonzero empty slot
without tombstone metadata is rejected; revision alone cannot prove which
identity was previously stored. Admission returns the expected revision and
next revision. This metadata-preserving compaction choice addresses the
re-admission hazard identified in review and awaits maintainer confirmation in
the PR.

There is no external timeout budget in this slice: deadlines/timeouts arrive as
typed events. Client/intermediary timeout sizing, durable presentation stages,
and async-operation policy remain with the Python caller and are not inferred by
this library.

## Verification and isolation evidence

The production crate imports only `std::fmt`; `transition` uses in-memory
values and checked integer revision arithmetic. A source audit of `src/` and
`tests/` found no process, network/socket, filesystem, clock, or randomness
APIs. The only fixture read is compile-time `include_str!` of
`tests/fixtures/python-contract-cases.json`, parsed from memory by test-only
`serde_json`. Thus crate-controlled test behavior has no subprocess or socket
path and no runtime file access outside that embedded fixture. `strace` is not
installed in the verification environment, so this claim is supported by the
source/API audit rather than a syscall trace.
