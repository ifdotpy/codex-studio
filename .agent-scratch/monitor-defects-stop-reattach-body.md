## Problem

The rest of #423. A lost Studio connection marks a running agent `interrupted` with `inFlight=false` and a `disconnectRecovery` receipt, while the process supervisor keeps the provider process, its generation, the turn and the turn's commands running. When the owner stops or deletes the agent in that window, no account worker is attached, so the stop commits the next epoch without `turn/interrupt`. The replacement worker of the same provider generation does not continue the stopped agent, and nothing ends its turn. The turn and its commands keep running in the app-server.

## Change

A new step at the end of each account worker's restart first pass (`account_worker/stopped_turns.rs`), after the background command check:

- It selects agents of the account with `autoWake=false`, `inFlight=false`, epoch = receipt epoch + 1, and a `disconnectRecovery` receipt that names a turn submitted on the worker's provider generation. The agent's `turnId` must still name that turn, or be cleared by a delete tombstone.
- It skips a turn that already finished (`lastCompletedTurn`), a turn whose stop already staged an interrupt (`startAttempt.interrupt` on the agent or in the receipt, unless proven unsent), and a turn of another generation, where the process that ran it is gone.
- For each selected turn it sends `turn/interrupt {threadId, turnId}` and then the provider's command cleanup (`thread/backgroundTerminals/clean` for Codex, through `ProviderKind::stop_terminals_method`). It sends them as the prior-epoch actor that the retained worker already authorizes for stopped and deleted agents (#423), under operation ids derived from agent, epoch, generation, thread and turn. A lost reply is replayed from the retained ledger, never sent again.
- It records the outcome as `disconnectRecovery.stopInterrupt`, so a later worker does not interrupt the turn again.

The stop path, the retained authorization rule and the provider launch are unchanged.

## Tests

`account_worker::tests::stop_after_reattach::{stop,delete}_before_worker_reattach_interrupts_the_turn_and_ends_its_commands`: a real account worker and fake app-server run a turn with a command; the provider generation disconnects; the agent is stopped or deleted with no worker attached; a replacement worker attaches to the same generation. The fake app-server must receive `turn/interrupt` and then `thread/backgroundTerminals/clean` once, the agent must stay paused and not wake, the receipt must record `accepted`, and a later worker of the same generation must send nothing.

- Red without the new step: `the reattached worker left the stopped turn running` (stop and delete).
- The first version also interrupted a turn whose stop had already sent its interrupt; `account_worker::tests::crash_matrix::every_scenario_recovers_at_every_bridge_boundary` caught that, and the stop's own interrupt receipt now excludes the turn.

Focused: `cargo nextest run -p studio-runtime-services --lib --test integration -E 'test(/account_worker::/) | test(/lifecycle_effects::/) | test(/wave7_p10_lifecycle/) | test(/startup_recovery/) | test(/recovery_store/) | test(/restart/)'`: 512 passed. `cargo clippy -p studio-runtime-services --all-targets`: no diagnostics in the changed files.

Full tiers: in the coordinator's batch gate.
