## Goal

This PR follows #514 (liveness checker) and #503 (steer answered as its own prompt). A silent Claude turn that Claude already ended now settles once from native history, as the coordinator asked: "when the native transcript ends with an end_turn result that consumed every steer, the bridge completes the turn once."

The base is `feat/liveness-checker` (#514) merged with `fix/claude-steer-dequeued-prompt` (#503).

## Change

- **Bridge, `turn/reconcile` `{threadId, turnId}`.** For the active turn of the thread:
  - The bridge reads native history and reuses the restart reconciliation decision (`nativeTurnOutcome`, exported from `restart-reconcile.mjs`).
  - The turn completes only when all of these are true: the transcript chain of this turn ends with Claude's final answer (`end_turn`, no unresolved tool call), and every submitted steer of the turn is recorded in it.
  - Then the bridge marks those steers delivered (one `item/completed` each) and finishes the turn once, with the deferred result.
  - A busy turn is not changed: a running tool, a Studio wait, an input reservation, a CLI event in the last minute, or a turn that is finishing.
  - A turn that native history does not show ended stays open.
  - The bridge sends nothing to Claude.
- **Rust.**
  - `Method::TurnReconcile` (`turn/reconcile`).
  - The retained session accepts it only from the turn's actor, with a `liveness-turn:` operation and exactly `threadId` and `turnId`.
  - The account worker's silent turn check (#514) asks the bridge once per silence step. The check identity is the operation id, so a repeated pass replays the answer.
  - `completed` and `notActive` record nothing; the bridge's `turn/completed` settles the chat through the journal. `busy` counts as running. `unconfirmed` keeps a `turn_stalled` row that says native history does not show the end.

## Witnesses

These tests are in their own commits before each change:
- `steer.test.mjs` "a turn that Claude ended while a steer kept it open completes once from native history":
  - The scripted CLI answers the reply and the steer, with results that name no input, and writes the native transcript.
  - `turn/reconcile` completes the turn once and delivers the steer once. The inputs stay `["stuck", "steer text"]`, and a second call returns `notActive`.
- `steer.test.mjs` "a turn that native history does not show ended stays open".
- Both bridge tests fail without the bridge change, and the steer tests are in the short tier (#503).
- `account_worker::tests::silent_turns::a_silent_claude_turn_is_asked_to_reconcile_once` and `..._that_native_history_does_not_confirm_is_shown` (short tier through `silent_turns::`).

## Checks

- Bridge vitest: 208 of 208 pass. oxfmt and oxlint are clean.
- `cargo nextest`: `account_worker::`, `liveness::`, `studio_errors`, and runtime-providers: 443 of 444 passed before the row witness change (the change moves the silent chat past the check grace), then 3 of 3 for `studio_errors::`.
- `contracts:check`, the tooling tests (45 of 45), and `cargo clippy -p studio-runtime-providers -p studio-runtime-services -p studio-runtime-http -p studio-runtime-server --all-targets -- -D warnings` are clean.
- I did not run `just test-fast` and `just test-comprehensive`. The batch gate runs them.
