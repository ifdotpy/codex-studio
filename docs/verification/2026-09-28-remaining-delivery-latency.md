# Remaining delivery latency

Source baseline: `b2e874a`. The fixture and Git benchmark use temporary directories. No live backend was changed.

## Live baseline

The accepted fast path was installed at 07:41:55 UTC. In 45 events over ten minutes, it entered for every event. The live marks showed these p50 and p90 times in milliseconds:

| Segment | p50 | p90 |
| --- | ---: | ---: |
| Enqueue to dispatch pick | 240 | 2,900 |
| Fast entry to runtime lock | 61 | 1,524 |
| Runtime lock to dispatch pick | 53 | 1,233 |
| Start to preparation end | 334 | 23,800 |
| Preparation end to native submission | 343 | 2,758 |

The longest preparation samples were first worker turns in chrompile. Their start to preparation end time was about 24 seconds. Idle Claude leads took 2.5 to 3 seconds. The live process has not run this revision, so no live after value exists.

## Dispatch fixture

`tests/delivery-latency-fixture.py` creates 600 agents with 8 KiB records, 240 prior events with 4 KiB text, 20 busy recipients, and 10 released idle recipients. It includes an open sender transaction for 80 ms. The scheduler does one full pass before the measured events. The values below are medians of p50 or p90 from three paired no-index and indexed runs, in milliseconds. Both runs use the same delivery code; the no-index run disables only the new indexes. This isolates the database change.

| Recipient and segment | Before p50 | Before p90 | After p50 | After p90 |
| --- | ---: | ---: | ---: | ---: |
| Busy, enqueue to dispatch pick | 103.4 | 160.7 | 64.7 | 121.1 |
| Busy, pick to start | 0.8 | 1.6 | 0.6 | 1.2 |
| Busy, start to validation | 168.3 | 244.7 | 118.3 | 184.3 |
| Busy, validation to program | 1.5 | 5.2 | 1.3 | 2.9 |
| Busy, preparation end to connect | 0.3 | 0.4 | 0.3 | 0.5 |
| Busy, connect to submission | 102.0 | 146.7 | 72.0 | 116.4 |
| Busy, enqueue to submission | 393.6 | 495.8 | 283.0 | 345.0 |
| Idle, enqueue to dispatch pick | 128.0 | 196.8 | 70.5 | 124.5 |
| Idle, pick to start | 0.6 | 1.2 | 0.6 | 1.2 |
| Idle, start to validation | 112.8 | 184.8 | 115.8 | 158.4 |
| Idle, validation to program | 0.8 | 4.1 | 1.7 | 4.0 |
| Idle, program to repair and preparation | 569.9 | 682.3 | 593.2 | 659.4 |
| Idle, repair end to preparation end | 46.9 | 117.5 | 39.1 | 120.2 |
| Idle, preparation end to connect | 2.8 | 8.0 | 3.1 | 6.8 |
| Idle, connect to submission | 100.8 | 135.9 | 92.5 | 131.5 |
| Idle, enqueue to submission | 1,009.2 | 1,125.0 | 929.0 | 1,048.0 |
| All 30, enqueue to submission | 441.0 | 1,042.0 | 303.0 | 942.0 |

The indexed active-agent scan fell from p50/p90 11.5/15.5 to 0.7/1.2 ms for busy recipients. The workspace-operation scan fell from 10.8/12.8 to less than 0.1/0.1 ms. The indexed fast entry to lock was 58.5/83.7 ms for busy and 34.0/80.4 ms for idle recipients. A later three-pair run had indexed all-event p50 values 248, 396, and 604 ms and p90 values 777, 1,187, and 1,707 ms. Host load makes the total target unproven. The marks isolate the scan gain even in that run.

The fast lock mark now records the owner stack name after 50 ms and at 250 ms intervals. Fixture samples include `codex_runtime.py|records`, `codex_runtime.py|agent`, `codex_runtime.py|start`, and `codex_work.py|index_item`. The new start marks split repair, worktree setup, thread preparation, transcript, native parameters, commit, and native submission. The live process can use them to identify its 1 to 3 second tails after installation.

## First worker preparation

A new worker worktree begins at HEAD. Its first checkpoint now records the HEAD tree without `git add`. If an executable post-checkout hook exists, the normal snapshot captures any hook changes. A regression test changes a tracked file in that hook and checks the stored tree. Existing worktrees still use the exact snapshot path.

Git worktree add still performs the checkout and runs the post-checkout hook. A read-only audit of `codex-agents`, `lumina`, and `chrompile` found no configured `core.hooksPath`, executable non-sample hook, configured filter, or tracked `.gitattributes` filter, encoding, or ident rule. A local shared clone of chrompile had 22,292 tracked files. Its normal worktree add took 42.45 and 25.62 seconds in prior runs. `checkout.workers=8` took 9.82 seconds; `checkout.workers=16` took 6.53 seconds. Both parallel checkouts had a clean status and all 22,292 files. The source uses 16 workers for a new worktree. The two-step no-checkout and reset method was not retained: it took 38.88 and 36.05 seconds in the same clone and omits the checkout hook.

The separate 1,000-file fake-native preparation fixture measured 2,008.6 ms for a first worker. It split into 249.9 ms before checks, 129.7 ms to connect, 263.1 ms for worktree list, 717.2 ms for worktree add, 566.6 ms for checkpoint, and 84.1 ms for the remaining native steps. A separate old snapshot operation took 213.9 ms. Host load affects these elapsed values. The fake Claude lead took 20.3 ms to prepare; it cannot explain the live Claude bridge's 2.5 to 3 second delay. The new marks will separate connection, native resume, and context checks in live results.

## Admission and submission

Busy turns keep the active native tool schema. Claude has no Codex rollout header. A first Codex thread has no header to refresh. Dispatch skips the native tool gate for those cases unless an update notice or refresh ticket exists. It retains account reservation, context, radio, budget, team, workspace, and capacity checks. The fixture still measured 25 to 40 ms for the native tool gate on a released idle Codex thread. Native submission itself took a few milliseconds with the fake server. The longer preparation and transcript stages remain visible in the new marks.

Two partial SQLite expression indexes are created on the first scoped dispatch and then reused: `runtime_agent_dispatch_active` and `runtime_agent_dispatch_workspace`. The ready flag is set after the database transaction commits. Existing runtime objects create the flag lazily. The changes remain installable by function replacement in the running process. `Runtime.dispatch_lock` is a contextmanager and needs its wrapped function code replaced.

## Checks and limits

The source has no live after measurement and has not met the requested p50 under 300 ms and p90 under 1 second consistently in the fixture. A busy recipient meets both in the first three paired runs. Released idle preparation and host lock contention still dominate the tail. Native model step-boundary latency is outside these measurements.

The required critical steer, runtime, team delivery, queue order, send default, radio runtime, prepare steer, tool request, protocol reader, native release, context repair, and capacity retry contracts passed. The native tools, Claude provider, workspace, critical runtime, and account project contracts also passed. Two native release tests first failed because their fixture marked an agent idle before its fake native start had an accepted turn; its helper now waits for that turn. One capacity test first read an uncertain event before asynchronous persistence; it now waits for that state. A different capacity test timed out once under the combined suite's host load and passed on an isolated full run. The final fixture smoke and syntax checks are recorded with the submission.

## Function and state inventory

| Module | Class | Changed or added functions |
| --- | --- | --- |
| `scripts/codex_runtime.py` | `Runtime` | `prepare`, `prepare_locked`, `sample_dispatch_lock_holder` (new), `ensure_dispatch_indexes` (new), `dispatch_lock`, `dispatch_candidates`, `start` |
| `scripts/codex_workspace.py` | `WorkspaceMixin` | `capture_checkpoint` |
| `tests/delivery-latency-fixture.py` | `MeasuredLock` | `__repr__` (new), `acquire` (new), `__enter__`, `release` (new), `__exit__` |
| `tests/delivery-latency-fixture.py` | module | `measure` |
| `tests/remaining-delivery-latency-fixture.py` | module | `git`, `spans`, `measure_worker`, `measure_claude` (all new) |
| `tests/critical-runtime-contract.py` | `CriticalRuntimeContract` | `test_worker_checkout_runs_post_checkout_hook` (new) |
| `tests/workspace-contract.py` | `WorkspaceContract` | `test_first_checkpoint_uses_head_tree_without_snapshot` (new), `test_failed_first_checkpoint_does_not_stop_the_worker` |
| `tests/claude-provider-contract.py` | `ClaudeProvider` | `test_native_command_has_its_own_queued_batch` |
| `tests/native-release-contract.py` | `NativeReleaseContract` | `lead`, `worker` |
| `tests/capacity-retry-contract.py` | `CapacityContract` | `test_new_message_uses_its_own_reservation_after_unknown_retry` |

`scripts/codex_runtime.py` gains module import `sys`. Its existing objects create `_delivery_timing` as a thread local and `_dispatch_indexes_ready` as a Boolean on first use. The new fixture adds imports `importlib.util`, `json`, `Path`, `subprocess`, `sys`, `tempfile`, `time`, and `Runtime`; module names `spec` and `fixture`; and the four functions listed above. Existing event metadata gains monotonic timing fields and `fastLockOwners`. No other module-level name or lazy field was added.
