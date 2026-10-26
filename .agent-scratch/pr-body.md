## Problem

The supervisor keeps a provider process when its account worker ends (#458). Its turn stays running and gets no disconnect receipt. If the owner stops the agent before a worker reattaches, the stop cannot send its `turn/interrupt`:

- Through the running server's account session, the request is bound to a scope with no live worker. The stop's receipt stays `unknown`.
- Without an account session, the receipt is proven `unsent`.

The step from #459 (`stopped_turns.rs`) selects only agents with a disconnect receipt. So the worker that reattaches to the same generation sends nothing, and the stopped turn and its commands keep running.

A backend restart is not affected. Startup recovery writes a disconnect receipt (source `restart`), so #459 ends the turn. The new full stack witness below proves this.

## Fix

- `stopped_turns.rs`: a reattached worker also selects a stopped agent whose own stop interrupt on this provider generation is `unknown` or `unsent`, also without a disconnect receipt. It sends the interrupt and the command cleanup under the stop's exact operation ids (`lifecycle-interrupt:` and `lifecycle-terminals:` with the stop's signature). The retained ledger then sends each request at most once. A request that an earlier worker wrote replays its result and is not sent again. The outcome settles on the stop's receipt through `SchedulerRepository`. A turn that a disconnect receipt already ended keeps that owner.
- `lifecycle_effects.rs` owns the mapping from a stop's interrupt ticket to its cleanup operation (`stop_terminals_operation`).
- `SchedulerRepository::settle_interrupt_result`: a proven `unsent` receipt may settle once more, to a different outcome. This happens when a reattached worker sends it under the same ticket.

## Witnesses

- `account_worker::tests::stop_after_reattach`:
  - `stop_while_a_retained_provider_has_no_live_worker_interrupts_its_turn_once` (the production session, receipt `unknown`)
  - `stop_without_an_account_session_interrupts_a_retained_turn_once` (receipt `unsent`)
  - `stop_interrupt_with_a_lost_reply_is_not_sent_again_after_reattach`
  - The first two fail at 1af820616 (the reattached worker leaves the turn running). With the fix they pass. A mutation check confirms the third test: when the stop source uses another operation id, the interrupt goes out twice and the test fails.
  - The short tier selects all three witnesses (retained provider disconnect witnesses step). Each one runs in under 1 s.
- `wave7_p01_registry::backend_restart_keeps_live_work_and_ends_the_stopped_turn_once` (macOS and Linux): this witness uses the real process supervisor and a scripted Codex app-server (`tests/fixtures/restart-provider.mjs`). The backend stops, and the owner stops B while no backend runs. A new backend then attaches to the same generation. A's turn keeps running with no false disconnect and completes. B gets one interrupt and one command cleanup. Each thread gets exactly one turn start. The run takes about 1.6 s. The comprehensive tier runs it.

## Checks

- `cargo nextest -p studio-runtime-services --lib`: `stop_after_reattach::` and the crash matrix `every_scenario_recovers_at_every_bridge_boundary` pass (8 of 8). `account_worker::` and `lifecycle_effects::` pass (427 of 427 tests, with the corpus excluded).
- `cargo nextest -p studio-runtime-store --lib -E 'test(/scheduler/)'`: 93 of 93 pass.
- The runtime-server witness passes with `STUDIO_SUPERVISOR_BINARY` set.
- `cargo clippy -p studio-runtime-store -p studio-runtime-services -p studio-runtime-server --all-targets -- -D warnings`: clean. `cargo fmt --check`, oxfmt and oxlint: clean.
- `turn_lifecycle_corpus::python_written_turn_lifecycle_corpus` fails the same way on main 046021513 without this change: 13 green and 44 red cases, each with a new `tokenBudget` sync field. It is not related to this change.
- I did not run `just test-fast` and `just test-comprehensive`. The batch gate runs them.
