## Problem

On the live host, one interval rule started a command every 4 minutes, 352 commands in 24 hours. Its owner had been failed since 10-13: the Claude thread resume failed with ENOENT, and the failed start kept its input pending. The owner could not act on a wake. The rule tick paused a rule only for a stopped, deleted or re-epoched owner, so the rule kept running. This is one way the state grew to about 90k monitor rows.

## Owner semantics in the code

- `RuleSnapshot::tick` already pauses a rule with an explicit pause (`STOPPED`, manual resume) when the owner is deleted, has `autoWake` false, or has a new epoch. An agent archive sets all three.
- A wake reaches an agent through `event_write::queue_agent`. It does not queue an agent with `nativeFailureHold`. It queues a failed agent, which then only repeats the failed start of its held input.
- A failed agent without held input (for example a model failure) can run a new turn on a wake. Its rules keep running, and so do the rules of an idle owner.

## Fix

- The tick pauses an active rule while its owner:
  - is failed and has input that a failed start kept pending at its epoch (one indexed `EXISTS` on `runtime_events`, read only for a failed owner),
  - has `nativeFailureHold`, or
  - is an archived conversation.
- The rule stores the cause in `ownerHold` (`failed_input_held`, `native_failure_hold`, `archived`) and a plain reason in `error`, so the rules panel shows it as paused with the reason.
- A paused rule with `ownerHold` stays in the tick. The tick resumes the rule when the owner can run again. When the owner is stopped or deleted, it turns the pause into the existing explicit pause.
- Until the tick pauses it, an event rule of a held or archived owner starts no check.
- A manual pause or resume through `/api/rules` removes `ownerHold`, so the tick never overrides the owner's choice.
- The persisted rule schema owns the `ownerHold` codes, and a unit test checks the parity.

On a read-only copy of the live data, 1 of 29 active rules pauses: the rule from the report.

## Witnesses

- `rules_tick_atomic::a_rule_pauses_while_its_owner_cannot_run_and_resumes_when_it_can`:
  - For each of the three holds: no command over three ticks, the rule paused with `ownerHold` and the reason, no wake, then resume and one check after the owner can run.
  - An idle owner and a failed owner without held input still get their check.
  - It fails at f3deac8e5.
- `rules_owner_hold::a_manual_pause_or_resume_replaces_the_owner_hold_pause` (runtime-http) fails without the route change.
- `rules_engine::tests::owner_hold_codes_match_the_persisted_rule_schema` and `an_event_of_a_held_owner_starts_no_check`.
- A new short tier step runs the two integration witnesses (1.7 s).

## Checks

- `cargo nextest` passes 56 of 56: `rules_tick_atomic::`, `rules_engine_corpus::`, `rules_engine::tests::`, `monitor_flood_and_missed_rules::`, `monitor_wake_guarantees::` and `wave7_p15`. The frozen rules corpus is unchanged.
- runtime-schema and runtime-store rule tests pass, 37 of 37. `rules_owner_hold::` and `studio_errors::` (runtime-http) pass.
- `contracts:check`, `cargo clippy -p studio-runtime-store -p studio-runtime-services -p studio-runtime-http --all-targets -- -D warnings`, `cargo fmt --check` and oxfmt: clean.
- I did not run `just test-fast` and `just test-comprehensive`. The batch gate runs them.
