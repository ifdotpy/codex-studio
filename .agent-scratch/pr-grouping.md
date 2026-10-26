## Problem

The Studio errors list showed one row per failed monitor. A rule that checks every 4 minutes while its owner cannot run fails the same way each time. On the live host, 110 of the 200 rows were `monitor_not_recorded`, and 74 of them came from one rule. The latest run of that rule succeeded, so those rows needed no action.

## Fix

- `studio_errors::list` groups the failed or lost runs of one rule, or of one command of an agent, by kind. Each group is one row with `count`, `firstAt` and `at`, plus the latest failure's monitor id and error. `ruleId` names the rule.
- A failure that started before the latest successful run of the same rule is left out.
- Dismissing a grouped row records the dismissal time. The row's failures so far stay hidden, and a later failure returns as a new row of one. Earlier dismissals of single monitor rows still hide those monitors.
- The OpenAPI schema adds `count`, `firstAt` and `ruleId` to `StudioError`. The generated client files come from `contracts:generate`.
- The list shows "N times, first to last" for a repeated row.

On a read-only copy of the live data, 263 monitor failure rows become 163 groups. 11 groups go away because a later run of the rule succeeded, including the 74 rows of the rule from the report.

## Witnesses

- `studio_errors::repeated_failures_of_one_rule_or_command_are_one_row` (runtime-http integration) checks the rule group, the later success, the command group, a separate kind, and the dismissal followed by a new failure. It fails at 768cbcb80.
- `StudioErrors.test.ts`: a repeated row shows its count and its range.
- The short tier now also runs the runtime-http `studio_errors::` witnesses, in the project preparation step. Before, it ran only the classifier unit tests.

## Checks

- `cargo nextest`: `studio_errors::` (runtime-http integration) and `studio_errors::tests::` (runtime-services lib) pass, 4 of 4. The changed short tier step passes, 10 of 10.
- `vitest`: `StudioErrors.test.ts` passes, 3 of 3. `tsc --noEmit` shows only the existing missing wasm client module (no `build:wasm` in this worktree).
- `contracts:check`, `cargo clippy -p studio-runtime-services -p studio-runtime-http --all-targets -- -D warnings`, oxfmt and oxlint: clean.
- I did not run `just test-fast` and `just test-comprehensive`. The batch gate runs them.
