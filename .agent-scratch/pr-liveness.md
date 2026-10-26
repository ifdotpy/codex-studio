## Goal

Find a stuck chat before its owner does. A liveness check runs every minute. Each stuck state is a row in the Studio errors list, with a code and a plain reason. Where it is safe, the check runs one automatic recovery through an existing idempotent path. No input is ever sent twice.

## Invariants

| Code | Stuck state | Recovery |
|---|---|---|
| `turn_stalled` | The chat is `running` and in flight, but its provider sent nothing for 10 minutes (`activity.at`, `lastEvent`) while no tool runs. | Codex: once per silence step (10, 20, 40, ... minutes), the account worker runs the existing turn recovery (`RecoveryJob::Turn`, `recover_turn`): thread read, exact turn read, thread read, then compare-and-apply. If native history shows the turn ended, the worker settles it once, publishes it and wakes the chat. If native history shows it running, there is no row. Claude: the bridge reports a turn that it holds as active, so the row names the stall. The bridge reconcile request follows in a separate PR on top of #503. |
| `input_not_started` | An owner message (`user`) is pending for more than 2 minutes while its chat is idle or interrupted. | The chat is queued once per message, as a new message would queue it (`event_write::queue_for_pending_input`). The registry wake follows. The receipt is written before the wake. The scheduler claims only pending input, and uncertain input is never claimed again. A held chat (stopped, native failure hold, start outcome hold, account transfer, failed, paused) is not woken, and its row says why. |
| `session_not_started` | Chats wait for their account session (`startWait.reason` `provider_session`) for more than 2 minutes. | None. The row names the account and the last start error. The account registry now records that error (a receipt per account, written only when the text changes) and clears it when the session starts. |
| `schedule_stalled` | A usage resume or capacity retry is 15 minutes past its due time, or an account transfer did not change for 15 minutes while a member is in a short step, waits idle for release, or is blocked or unknown. | None. The row shows it. |

## Measured on the live state (read-only, before this change)

- `turn_stalled`: 1 chat (Claude). The bridge kept the turn open after Claude answered a steer as its own prompt. This is the pattern that #503 fixes.
- `input_not_started`: 1 chat, held by a failed start. Earlier the same day, 2 chats were held by `nativeFailureHold`. No chat qualified for an automatic wake.
- `session_not_started`: 0.
- `schedule_stalled`: 1 account transfer with a blocked member, unchanged for 20 days. No overdue usage resume or capacity retry.

## Witnesses

These tests are in their own commit before the implementation:
- `studio_errors::liveness_rows_name_each_stuck_state` (runtime-http): each code appears as a row with its detail. A running tool and a young message are not stuck.
- `liveness::an_idle_chat_with_a_waiting_message_is_woken_once` (runtime-services): only the idle, unheld chat is queued, the input stays pending, and a second pass does not wake the same message again.
- `account_worker::tests::silent_turns::a_silent_turn_that_native_history_ended_is_settled_once`: the history reads settle the turn. No input or turn request is sent, and the next pass sends nothing.
- `account_worker::tests::silent_turns::a_silent_turn_that_native_history_shows_running_is_left_alone`: the turn keeps running, has no row, and one silence step is read once.

The short tier runs them in existing commands; the integration command count is unchanged.

## Checks

- `cargo nextest`: 460 of 460 pass in `account_worker::`, `lifecycle_effects::`, event_write, `studio_errors`, `liveness::` and `rules_tick_atomic::`.
- `cargo clippy -p studio-runtime-services -p studio-runtime-http -p studio-runtime-server --all-targets -- -D warnings`, `contracts:check`, oxfmt, and the tooling tests (45 of 45): clean.
- I did not run `just test-fast` and `just test-comprehensive`. The batch gate runs them.

The base is the combined branch `batch/studio-errors-and-rule-hold` (PRs #499 and #501), because the rows use its grouped Studio error list.
