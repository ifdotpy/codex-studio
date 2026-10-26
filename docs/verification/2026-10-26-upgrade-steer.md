# Upgrade canary steer reply

Task: N (`work-1792988769932549888-4`).
Source: `c5858de87f297196f189f1d976f19c1f6bd7a62d`.
Date: 2026-10-26 UTC.

## Result

The sixth run reproduces the missing `STEERED` reply.
The provider receives the steer in the original turn before completion.
The model replies with only `STEER_BASE`.
The evidence excludes a late or dropped steer in this failure.

The original prompt requires exactly `STEER_BASE`.
The steer asks the model to add `STEERED` to that reply.
These output requirements conflict.
The transcript does not expose the model's reason for omitting `STEERED`.

The change makes the first reply instruction conditional on no later replacement.
The steer replaces that instruction with a complete, exact `STEERED` reply.
The monitor wait, delivery path, receipt check, and final token assertion remain the same.

## Runs

All six runs use the installed build `29bb1077e` as N and the prepared package `c5858de87` as N+1.
The package path is `/private/tmp/studio-package-c5858de87/Codex Studio.app`.
Each run uses the default Codex account and `gpt-6-luna` at low effort.
Each run keeps its clone with `--keep`.

| Run | ID         | Clone folder           | Steer | Overall result                        |
| --- | ---------- | ---------------------- | ----- | ------------------------------------- |
| 1   | `f22c1ba5` | `studio-canary-IRmIYV` | Pass  | Missing Claude file login             |
| 2   | `93f943e3` | `studio-canary-hFFiYo` | Pass  | Pass                                  |
| 3   | `aa7183b4` | `studio-canary-YxzdtZ` | Pass  | Pass                                  |
| 4   | `3f84705c` | `studio-canary-qBwY4S` | Pass  | Pass                                  |
| 5   | `c781565a` | `studio-canary-QcEpA2` | Pass  | Pass                                  |
| 6   | `801265ba` | `studio-canary-XXLveN` | Fail  | Missing token; live app stamp changed |

The clone folders remain below the platform temporary directory from `node -p 'require("node:os").tmpdir()'`.
The reports remain at `/private/tmp/steer-upgrade-canary-N-<run>.json`.
Each report contains the exact chat ID, message ID, build hashes, handles, and check results.

The command for run 6 is:

```sh
env -u CODEX_AGENTS_SUPERVISOR_MODE just upgrade-canary \
  --to '/private/tmp/studio-package-c5858de87/Codex Studio.app' \
  --keep --no-claude \
  --report-json /private/tmp/steer-upgrade-canary-N-6.json \
  --request-id steer-upgrade-canary-N-6
```

Run 1 omits `--no-claude`; runs 2 to 6 include it.
No run installs the package on the live host.
During run 6, the live app stamp changes to `64f41a6ce9ee94bf7146d3f807c1dbbc462d4706`.
This investigation does not change the live app or backend.
The clone finishes on the expected `c5858de87` package.

## Failure evidence

- Chat: `34660547-911c-47af-981e-574ce53271da`.
- Thread: `01a1768c-ce42-7ad1-9148-2867141638f5`.
- Original turn: `01a1768c-ce75-7bb0-8506-1550ab54e5c4`.
- Steer message: `59ae473a-471f-4e7a-b70b-e932715a0da4`.
- Native transcript: `homes/codex/sessions/2026/10/26/rollout-2026-10-26T05-51-41-01a1768c-ce42-7ad1-9148-2867141638f5.jsonl` in the clone.

| Time (UTC)   | Evidence                                                                       |
| ------------ | ------------------------------------------------------------------------------ |
| 04:51:41.817 | Native `task_started`, original turn                                           |
| 04:51:50.741 | Steer accepted, `runtime_event_meta.acceptedAt`                                |
| 04:51:54.529 | Native `item_completed`, `UserMessage`, exact steer `client_id`, original turn |
| 04:52:29.577 | Native `UserMessage` for `monitor_exit`, original turn                         |
| 04:52:31.051 | Native final reply: `CANARY_801265ba_STEER_BASE`                               |
| 04:52:31.086 | Native `task_complete`, original turn                                          |

The clone's `runtime_events` row for the steer has status `delivered` and no error.
Its transcript contains one user item with the exact steer message ID and text.
The provider acknowledges the steer about 36.6 seconds before turn completion.
The original API acceptance receipt retains `status: queued`; that receipt is not the later delivery status.
All other Codex scenario checks pass, including the monitor and queued message.

## Source comparison

`git diff ade3a475a c5858de87` shows no change in the steer, queue, input, or reservation paths.
The provider workspaces and upgrade canary also have no change.
The account worker, notification dispatch, message admission, and scheduler service have no delivery change.
Two related files differ:

- `account_worker/preparation/snapshot.rs`: test fixtures only.
- `runtime-store/src/repositories/scheduler/lifecycle.rs`: verified authentication failure repair only.

The retained transcript does not support a queue regression as the cause of this failure.

## Checks and limits

- Before the fix: the new prompt contract test fails on the unconditional exact `STEER_BASE` instruction.
- After the fix: all 29 tests in `upgrade-canary/tests/canary.test.mjs` pass.
- Tooling suite: all 160 tests pass with `pnpm run test:tooling`.
- That pnpm command automatically populates this checkout's dependencies from its local store: 724 packages reused, zero downloads.
- The integration owner runs `just test-fast` on the combined commit. This worker does not run that gate.
- The six live runs use the original prompts. No live run uses the new prompts because the run limit is reached.
- The prompt test checks the actual scenario caller. It does not prove future model compliance.
