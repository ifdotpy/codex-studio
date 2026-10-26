# Callback runtime integration contract

The callback service admits native approval and question cards, commits a user's
answer, and hands the exact prepared `Reply` to a retained account session. The
HTTP adapter is enabled with `HttpState::with_callback_runtime`; without it,
`POST /api/answer` and pending native deletion retain
`provider-session/question-answer`. Async questions, monitor approvals and
browser opening retain separate named dependencies. Local tool request reads
and cancellation keep their existing implementation and remote/payload refusals.

## Worker calls (reserved files; applied by their owner)

In `account_worker/callback.rs`, after journal admission and outside any store
transaction, call on the blocking bridge:

```rust
let decision = provider_callbacks::dispatch(store, &context, &frame, now)?;
provider_callbacks::bind_journal(store, &decision, &context, &rpc_id, cursor.0)?;
```

`context` comes from the retained account, connection generation, current actor
epoch, thread and turn. It never comes from tool arguments or an HTTP body.

- `Pending(card_id)`: the durable card owns the obligation. Settle the incoming
  journal and keep consuming session events; do not block for a human answer.
- `Answer(reply)`: prepare the frame with
  `OperationId(reply.identity.operation_id())`. Persist
  `provider_callbacks::delivery(store, &reply.identity, DeliveryOutcome::Submitted)`
  before the gated transport admission below. False means already written/retired: settle
  the journal without sending. Preserve the whole identity, including attempt.
- `Settled(identity)`: durable delivery or provider-resolution proof allows
  settling the incoming journal. Never regenerate its result or reexecute the
  tool. Resolution does not imply approval.
- `Unsupported`: preserve the worker's existing explicit unsupported response.

On transport outcomes, commit `delivery(..., Written | NotSent | Unknown)` for
the exact saved identity before settling the journal. A proven `NotSent` resets
human cards, or rotates the prepared tool reply's attempt. Never reuse the old
`Reply`; read the replacement. Submitted/Unknown is held, never retried.

In `account_worker/store.rs::compact`, after writing the durable ACK checkpoint,
call within that same transaction:

```rust
provider_callbacks::retire_checkpointed(tx, account, generation)?;
```

Retirement reads the existing worker checkpoint and the bound source cursor.
Only written reply payloads are removed; exact identity/signature tombstones
remain to suppress replays at later cursors. Unknown evidence is never pruned.
The function is bounded to 128 rows per call and may be repeated. Python-admitted
cards have no Rust source cursor: the answer transaction marks this provenance,
and their Written payloads retire immediately while retaining replay proof.
Resolution persists separate full-identity proof even before card admission;
deleting the visible card never removes that proof.

## HTTP answer mailbox

Construct `CallbackMailbox::new(wake)` and retain its receiver in the runtime
composition. Construct `CallbackRuntime::new(store.clone(), mailbox)` and attach
it to `HttpState`. The wake callback only signals the account owner; it performs
no store or provider work. The queue holds at most 128 replies; the HTTP blocking
bridge waits at most five seconds for the receipt. A disconnected/full queue is
NotSent; a lost receipt or timeout is Unknown.

The worker drains `CommittedReply` and prepares `reply.frame()` with
`OperationId(reply.identity.operation_id())`. Share **one Arc<CallbackRuntime>**
with HTTP and the retained worker. Immediately before native admission, on the
worker's blocking bridge, use:

```rust
let admitted = runtime.submit_committed(&reply, || {
    // Check the exact retained connection AND generation here, while holding
    // the lifecycle gate. The owner must not replace that session concurrently.
    Ok(handle.block_on(async {
        tokio::time::timeout(REPLY_DEADLINE, control.submit_frame(prepared)).await
    }))
})?;
```

`submit_committed` holds the lifecycle gate while it checks the durable exact
identity, body, Submitted state, card status, resolution tombstone and active
actor/turn, then performs bounded native admission. Its SQLite read transaction
ends before invoking the closure. `None` suppresses a resolved/already-written
reply: do not submit it or invent a Written receipt. Preserve the native typed
admission outcome inside `Some`; timeout is Unknown. After admission, release the
gate before waiting for the eventual native write receipt or other owner events.
Transport admission is the handoff linearization point; a resolution arriving
after that cannot recall bytes already handed to the provider.

**All competing lifecycle transitions must use this same gate.** Call
`runtime.resolve(&context, &rpc_id, now)` for provider resolution and
`runtime.recover_connection(account, connection, now)` for loss. Wrap the owner's
stop, deletion, completion, failure and session-replacement store/ownership
transition in `runtime.with_lifecycle(|| { ... })` on the blocking bridge. Do not
call the ungated low-level `provider_callbacks::resolve` concurrently with native
admission. Never wait for an HTTP mailbox receipt while holding the gate.

Mailbox replies already have Submitted committed by `CallbackRuntime`:
**do not submit their durable intent a second time**. Return `ReplyReceipt` with
the unchanged full identity and the typed native write outcome through
`CommittedReply.receipt`. If the HTTP waiter has gone away, its callback attempt
remains Unknown; do not infer failure from the closed receipt channel. Written
settles the old card but cannot revive a completed, failed or replaced actor turn.

After reconnect to a proven surviving session, `prepared_replies(store, context)`
returns only committed, unsent Prepared replies. A worker can deliver those using
the ordinary Answer path. It never returns Submitted/Unknown. On actual connection
loss use `runtime.recover_connection(account, connection, now)`; this expires
pending cards and holds uncertain answers. Resolution uses
`runtime.resolve(context, rpc_id, now)` and does not imply approval.

## Evidence

`wave7_p04_callback_runtime` compares complete Python answer, deletion and
resolution records (30 + 21 + 12 cases). `wave7_p04_review` adds four rejected or
unknown deliveries, twelve absent/null/invalid choices and three Python race cases.
Answer/delete comparisons include actors and the complete logical application
write set, including execution effects and published entities. Rust's protected
outbox has no Python counterpart; its exact identity, payload retirement and
replay invariants are asserted separately. Sync sequence/hash bookkeeping is
normalized to published entity values; the actor invalidation counter compares
increments. No shared no-write exclusion was changed.
The reviewed contract deliberately preserves the winner of completion/resolution
races where the Python oracle demonstrates a subsequent overwrite.

Both answer and cancellation restart at eleven distinct boundaries: before
admission, admission, journal binding, answer intent, Submitted persistence,
actual mailbox acceptance, native transport submission, typed receipt, Written
persistence, ACK persistence, and payload retirement. Real channel handoffs and
receipts replace the previous counter-only simulation. The existing seven SQL
abort cases prove rollback of whole-table snapshots; foreign receipts cover
agent, epoch, account, connection, generation, attempt, RPC, thread and turn.

`wave7_p04_callback_http` crosses actual middleware/generated DTOs and includes
an enabled Rust backend listening on an ephemeral loopback TCP port: native
answers succeed and explicit-null decisions refuse without writes.
`wave7-p04-native-http-contract.py` proves the corresponding null refusal through
the real Python HTTP backend with exact no-write snapshots. Standalone contracts
remain in `rust-questions-write-http-contract.py` and
`rust-tool-requests-http-contract.py`. The oracle imports the shared guard and
never starts a provider. These fixtures do not prove reserved account-worker
wiring or a live model request.
