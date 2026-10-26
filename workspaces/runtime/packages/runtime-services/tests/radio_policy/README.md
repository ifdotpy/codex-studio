# Radio policy boundary

This corpus ports the pure admission policy used by `codex_radio.validate_event`
and its first-thread binding helper. It does not claim to port the whole radio
room state machine.

## Included rules

| Rule family   | Policy covered                                                             | Fast | Full |
| ------------- | -------------------------------------------------------------------------- | ---: | ---: |
| receipt       | Missing and duplicate active event receipts                                |    2 |    2 |
| room          | Direct-room participant count, identity, lead/project and room link        |    3 |    5 |
| membership    | Peer-team identity, current eligible membership, room member set           |    2 |    3 |
| availability  | Account transfer and native-thread block for either participant            |    4 |    6 |
| readiness     | Auto-wake and native failure hold for either participant                   |    4 |    6 |
| room-status   | Blocked/stopping refusal and a nonterminal speaking room                   |    2 |    3 |
| turn-identity | Active recipient, epoch, and existing thread identity                      |    3 |    4 |
| binding       | Exact event, epoch, thread, compaction identity; no write intent on denial |    8 |   12 |

`cases-full.json` contains only the additional cases; the full run includes both
files. Every case invokes the real Python fixture writer and
`codex_radio.validate_event`, then checks the Rust snapshot decision.

## Excluded stateful rules

These stay with the existing Python runtime and are deliberately outside this
snapshot API:

- Queue construction, `send`, `pass`, and `stop` (`manage` and `tick`) write
  rooms, messages, and runtime events, create receipts, cancel pending events,
  or invoke `runtime.interrupt`.
- Floor holding and answer-turn routing (`holds_floor`, `select_pending`,
  `route_question_answer`) read event metadata, question rows, and completed
  turn receipts, then choose which unrelated event may dispatch.
- Turn completion and transcript reconciliation (`observe_item`, `tick`,
  `_reconcile`) consume transcript items, event delivery status, completion
  rows, and messages and write room state and chat history.

They require distinct transaction and effect APIs over records not present in
`RadioAuthorizationRecords`; folding them into admission would provide an
incomplete snapshot and could cause duplicate sends, missed cancellations, or
premature transcript advancement. The account worker integration continues to
use the existing stateful owners for these rules.

## Registered differences

- Duplicate active receipts are rejected as ambiguous. Python currently selects
  the first matching room from a query without an order; Rust fails closed.
- Python may bind and save a first thread before it refuses a blocked or stopping
  room. Rust returns only `Deny` and therefore exposes no binding to persist.
  The corpus confirms Python's stored side effect and the Rust no-write intent.

Both differences are covered by named cases in the differential corpus.
