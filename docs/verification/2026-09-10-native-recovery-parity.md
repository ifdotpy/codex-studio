# Native error and recovery handling, 2026-09-10

Reference: Codex CLI `rust-v0.153.4`, commit
`042fb41b7c813ac7999105e886b2b7aa715b5081`.

The [previous error audit](2026-09-08-native-error-parity.md) covers all 18
`CodexErrorInfo` variants, transport errors, authentication, quotas, permission
requests, hooks, and tool failures. The installed CLI schema still matches those
18 variants. This change covers the additional user recovery states in
`tui/src/chatwidget/protocol.rs` and `tui/src/app/safety_buffering.rs`.

| Native input | Studio behavior |
| --- | --- |
| `model/safetyBuffering/updated` | Show an active safety check in the current chat. Keep waiting dismisses the card without stopping the turn. |
| Server-provided `fasterModel` | Offer an explicit retry with confirmation. No local model selection after a policy refusal. |
| Response or tool activity | Remove the retry offer. Check native history before interruption and again before the fork. |
| `model/verification` | Show the native recommendation for Trusted Access checks. Do not infer entitlement from a model name. |
| `item/autoApprovalReview/started` | Show the pending tool review. |
| `item/autoApprovalReview/completed` | Preserve denial, timeout, abort, and rationale. Quiet successful review. No client-side permission grant. |
| `activeTurnNotSteerable` | Preserve the warning without failing the active turn, for both enum representations. |
| `thread/closed` or `notLoaded` | Schedule an immediate read of the exact native turn. Unloading alone does not prove completion. |
| Unknown error variant | Keep the native message and details through the existing generic handler. |

## Retry behavior

The HTTP action records a stable receipt before submission. The native sequence is:
read the latest turn and its items, interrupt that turn, verify its interrupted
status and complete item history, fork with `beforeTurnId` and
`deferGoalContinuation`, and start the original input once on the offered model.
Reasoning effort is `low`, as in the CLI flow. Native permission settings remain
in effect. The Studio agent, team, pending messages, and visible history retain
their identities.

The retry rejects incomplete history or history that already contains a response
or tool execution. This is stricter than the CLI's visible-message guard because
Studio manages external work. A final policy error removes the offer.

Each native request has one submission and a late-response callback. After 15
seconds the UI shows an unconfirmed receipt. It does not resubmit the operation.
Concurrent clicks and browser retries return the same receipt. A restart retains
the receipt without automatic replay. Cancel model change prevents subsequent
steps, but does not reverse a request already sent. Successful late fork receipts
remain stored even after cancellation. Cancellation does not stop team workers or
monitors. Once `turn/start` is submitted, cancellation must use the normal turn
controls.

## Verification

- `tests/native-error-schema.mjs`: all 18 variants in the installed schema.
- `tests/native-error-contract.py`: 32 cases for errors, account and turn scope,
  terminal state, and permission resolution.
- `tests/native-safety-contract.py`: 14 cases for safety state, response races,
  complete history, simultaneous clicks, late receipts, restart, cancellation,
  permission preservation, and completion delivery before the start reply.
- `tests/native-safety-integration.py`: the installed app-server consumes safety
  metadata from a loopback Responses server, emits the notification, creates one
  fork, and submits one retry. No OpenAI provider request.
- `tests/native-safety-update-contract.py`: behavior with the update bindings and
  rejection of an unknown live handler.
- `tests/turn-recovery-contract.py`, `tests/capacity-retry-contract.py`, and
  `tests/protocol-reader-contract.py`: existing recovery and transport behavior.
- `tests/native-error-ui.mjs`: actual notification handlers, SQLite, HTTP actions,
  desktop and 390px layout, safety confirmation and dismissal, authentication,
  capacity, quota, and precaution UI. Error details remain bounded.
- `npm --prefix web run build`: TypeScript and production renderer build.

This verifies the supported app-server boundary. It does not prove that OpenAI
will offer another model for a particular request or account. It does not change
native retry or policy decisions.

## Desktop delivery

The application package passes `desktop/package-test.mjs` with an isolated state
directory and a hidden window. The candidate is at
`desktop/dist/Codex Studio-darwin-arm64/Codex Studio.app`.

The active installation still uses its older bundled server. The guarded update
rejects that server because its handlers do not match the reviewed baseline.
The temporary dispatch hold expired. Both team leads received a notice to resume
work. Installation requires an idle backend with no active turns, monitors, or
terminal commands. The update did not stop user commands.
