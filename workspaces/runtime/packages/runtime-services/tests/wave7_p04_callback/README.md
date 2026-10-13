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
  before `control.submit_frame`. False means already written/retired: settle
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
The function is bounded to 128 rows per call and may be repeated.

## HTTP answer mailbox

Construct `CallbackMailbox::new(wake)` and retain its receiver in the runtime
composition. Construct `CallbackRuntime::new(store.clone(), mailbox)` and attach
it to `HttpState`. The wake callback only signals the account owner; it performs
no store or provider work. The queue holds at most 128 replies; the HTTP blocking
bridge waits at most five seconds for the receipt. A disconnected/full queue is
NotSent; a lost receipt or timeout is Unknown.

The worker drains `CommittedReply`, checks the exact retained connection and
generation, prepares `reply.frame()` with `reply.identity.operation_id()`, then
submits it. These replies already have Submitted committed by `CallbackRuntime`:
**do not submit their durable intent a second time**. Return `ReplyReceipt` with
the unchanged full identity and the typed native write outcome through
`CommittedReply.receipt`. If the HTTP waiter has gone away, its callback attempt
remains Unknown; do not infer failure from the closed receipt channel.

After reconnect to a proven surviving session, `prepared_replies(store, context)`
returns only committed, unsent Prepared replies. A worker can deliver those using
the ordinary Answer path. It never returns Submitted/Unknown. On actual connection
loss use `recover_connection(store, account, connection, now)`; this expires
pending cards and holds uncertain answers. Resolution uses
`resolve(store, context, rpc_id, now)` and does not imply approval.

## Evidence

`wave7_p04_callback_runtime` compares complete Python answer, deletion and
resolution records (30 + 21 + 12 cases), and exercises admission/store failures,
restart boundaries, attempt rotation, foreign receipts, late journal binding,
ACK retirement and mailbox outcomes. The corpus invokes the accepted real Python
oracle and imports its shared guard; it never starts a provider.
`wave7_p04_callback_http` crosses the actual HTTP middleware and generated DTOs
with an injected retained reply port. Standalone backend contracts remain in
`rust-questions-write-http-contract.py` and `rust-tool-requests-http-contract.py`.
These fixtures do not prove the reserved account-worker wiring or a live model
request. Run both crate tests with the CI nextest profile and the two contract
suites against Python and a rebuilt Rust server.
