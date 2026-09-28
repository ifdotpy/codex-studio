# Agent delivery latency

Initial source base: `e1f92cf`. The correction below uses merged base `28d90c3`.
This worktree has no live installation or paid model test.
Run `python3 -B tests/delivery-latency-fixture.py` for the isolated fake-native fixture.

## Post-commit correction

The lead installed the initial change at 07:06:49 UTC. Six live events then had agent message enqueue to native submission p50 1,886 ms and p90 2,408 ms. Enqueue to dispatch reservation took p50 1,310 ms and p90 2,138 ms. These are small samples, but they disprove the earlier fixture's claim about live speed. A fast worker could run before its caller committed an event and find no pending row. The first fixture held `Runtime.lock` through every enqueue, which hid that race. The live agent message caller also holds that lock, so the race alone does not prove the cause of its live delay. The new marks distinguish scheduling, worker entry, lock wait, and selection on the next live installation.

`Runtime.db` now schedules fast dispatch only after the connection context commits. A fast pass that reserves no batch sets `changed` for the scheduler. A duplicate event or skipped fast route also sets `changed`. The shared candidate reservation code still applies all holds and receipts. The fixture holds the first busy event's transaction open for 80 ms without `Runtime.lock`. It asserts that the fast worker enters after the transaction reaches commit. This assertion would fail against the initial source because its fast worker enters during the open transaction and its timing fields are absent.

The paired heavy fixture uses 600 agents with 8 KiB JSON records, 240 prior 4 KiB event payloads, 20 busy recipients, and 10 released idle recipients. The periodic scheduler is stopped in this isolated fixture; it calls a full pass directly for the comparison. Across three paired runs, median full pass time was 95 ms. The two lock sections took 19 ms and 39 ms. The full pass does not explain a 1.3 s lock wait in this fixture. The correction keeps the existing active-agent scan and capacity rules.

Three paired runs gave these median p50 / p90 values in milliseconds. Each pair used the same synthetic input.

| Segment | Busy full pass | Busy fast | Released idle full pass | Released idle fast |
| --- | ---: | ---: | ---: | ---: |
| Enqueue to reservation | 254 / 374 | 55 / 67 | 469 / 519 | 32 / 52 |
| Reservation to start | <1 / <1 | <1 / <1 | <1 / <1 | <1 / <1 |
| Start to validation | 379 / 494 | 84 / 102 | 171 / 223 | 39 / 62 |
| Validation to program ready | 1 / 1 | 1 / 1 | 1 / 1 | <1 / <1 |
| Program to repair ready | 0 / 0 | 0 / 0 | 663 / 756 | 282 / 306 |
| Repair to prepared | 0 / 0 | 0 / 0 | 52 / 89 | 26 / 46 |
| Prepared to connected | <1 / <1 | <1 / <1 | 2 / 2 | 2 / 4 |
| Connected to submitted | 297 / 445 | 53 / 63 | 58 / 86 | 33 / 55 |
| Total | 934 / 1,088 | 200 / 226 | 1,428 / 1,557 | 438 / 449 |

Across all 30 recipients, the median full pass p50 / p90 was 1,023 / 1,479 ms; corrected fast dispatch was 219 / 445 ms. Within the fast path, queued to scheduled was 0.3 / 0.5 ms for busy and 0.3 / 0.4 ms for idle. Scheduled to worker entry was below 0.1 ms at both percentiles. Worker entry to `Runtime.lock` acquisition was 26 / 35 ms for busy and 4 / 26 ms for idle. Lock acquisition to reservation was 27 / 31 ms for busy and 28 / 32 ms for idle. `fastQueuedAt`, `fastScheduledAt`, `fastEnteredAt`, and `fastLockedAt` are monotonic marks in event metadata. `fastSkipReason` records `autoWake`, `scheduleOverride`, `closed`, or `disabled` when the route is skipped.

The correction changes `scripts/codex_runtime.py|Runtime|db` (its wrapped generator code), `enqueue`, `dispatch_after_user_batch`, and `dispatch_candidates`. It adds `schedule_fast_dispatch`, `dispatch_fast`, and `dispatch_lock`. The fixture changes `tests/delivery-latency-fixture.py|module|measure`, adds `MeasuredLock.__init__`, `MeasuredLock.__enter__`, and `MeasuredLock.__exit__`, and adds module imports `contextlib.nullcontext` and `threading` plus the module name `MeasuredLock`. Existing Runtime instances create no new direct field. Each thread's existing `_callback_db` local object lazily gains `after_commit_dispatch`; the existing `_dispatch_executor` remains lazy. To hot patch `Runtime.db`, swap `Runtime.db.__wrapped__.__code__`: the `@contextmanager` wrapper closure points to that generator object. The other changed methods accept ordinary code swaps. Running scheduler frames need no change.

After the correction, all 12 requested contracts passed: critical steer, runtime, team delivery, queue order, send default, radio runtime, prepare steer, tool request, protocol reader, native release, context repair, and capacity retry. The new fixture passed its delayed-commit assertion. The corrected source has no live latency sample; its live result remains unverified.

The fixture creates 600 agents in one team. It sends one agent message to each of 20 busy recipients and 10 idle recipients with released native threads. The other 570 agents supply the scan load. The comparison uses the same changed source with the per-agent route disabled for the full scheduler mode. It measures event enqueue to the return of the native submission call. The native model step after submission is outside this measure.

Three paired runs gave these medians of each run's percentiles, in milliseconds:

| Recipient | Full pass p50 | Full pass p90 | Per agent p50 | Per agent p90 |
| --- | ---: | ---: | ---: | ---: |
| All 30 | 269 | 493 | 153 | 555 |
| Busy 20 | 254 | 292 | 135 | 174 |
| Released idle 10 | 471 | 496 | 527 | 627 |

The per-agent route meets the fixture target of p50 below 300 ms and p90 below 1 s. The released idle group still pays for `thread/resume`. Its p90 increased in these runs. An earlier run under heavier host load measured full pass p50 1,605 ms and p90 1,687 ms, then per-agent p50 259 ms and p90 749 ms. These are fake-native timings, not live delivery evidence.

Segment medians from the three paired runs follow. Each cell is p50 / p90 in milliseconds. Monotonic marks are in `runtime_event_meta.record.timing`.

| Segment | Busy full | Busy per agent | Idle full | Idle per agent |
| --- | ---: | ---: | ---: | ---: |
| Enqueued to dispatch picked | 87 / 185 | 31 / 61 | 68 / 136 | 22 / 61 |
| Dispatch picked to start began | 0.2 / 0.5 | 0.2 / 0.3 | 0.4 / 0.6 | 0.2 / 0.9 |
| Start began to validated | 50 / 71 | 57 / 97 | 60 / 80 | 35 / 80 |
| Validated to program ready | 0.7 / 0.9 | 0.7 / 4.1 | 0.3 / 1.0 | 0.6 / 1.1 |
| Program ready to repair ready | 0 / 0 | 0 / 0 | 268 / 293 | 356 / 451 |
| Repair ready to prepared | 0 / 0 | 0 / 0 | 6 / 47 | 18 / 52 |
| Prepared to connected | 0.1 / 0.2 | 0.2 / 0.2 | 1.3 / 7.1 | 1.3 / 2.8 |
| Connected to submitted | 67 / 117 | 24 / 62 | 29 / 59 | 23 / 80 |

The idle preparation work dominates its delay. Preparing before reservation could start a native resume for an event that a radio, budget, team, or capacity check later holds. The measured idle p90 remains below the target, so this change keeps preparation after reservation.

The fixture also makes 20 sender tool reservations after delivery. In per-agent mode, median p50 was 1.3 ms and p90 was 1.7 ms. The reservation lock wait p90 was 0.1 ms. The actor lookup uses the existing native thread scope index. These tool measurements do not include native model execution or handler work. Tool request records now hold monotonic reservation begin, lock acquisition, reservation end, handler start, and handler end marks.

The per-agent route calls the same candidate reservation code as the full pass. It keeps context, budget, account, radio, team, capacity, and exact receipt checks. A separate dispatch pool keeps starts from blocking event selection. A separate start pool keeps the existing general pool from delaying starts. Both pools are created on first use for existing runtime objects. User input keeps a 40 ms batch window so a following agent message can share its transcript batch.

Required contracts passed: critical steer, runtime, team delivery, queue order, send default, radio runtime, prepare steer, tool request, protocol reader, native release, context repair, and capacity retry. The radio runtime contract passed five repeated runs after the exact receipt race fix.

## Function and state inventory

| Module | Class | Changed or added functions |
| --- | --- | --- |
| `scripts/codex_runtime.py` | `Runtime` | `enqueue`, `dispatch_after_user_batch` (new), `dispatch_executor` (new), `delivery_executor` (new), `mark_event_timing` (new), `mark_event_timings` (new), `dispatch`, `dispatch_all` (new), `dispatch_candidates` (new), `start`, `start_accepted`, `request`, `dynamic`, `close` |
| `scripts/codex_radio.py` | module | `tick` |
| `scripts/codex_team_isolation.py` | module | `cancel_pending` |
| `scripts/codex_tool_requests.py` | `RequestMixin` | `tool_request_actor` (new), `reserve_tool_request`, `begin_tool_request`, `finish_tool_request` |
| `tests/critical-steer-contract.py` | `CriticalDelivery` | `test_timing_write_failure_after_submission_does_not_replay` (new) |
| `tests/queue-order-contract.py` | `QueueOrderContract` | `test_dispatch_wins_race_without_edit_or_cancel_of_reserved_input` |
| `tests/radio-runtime-contract.py` | `RadioRuntime` | `eventually_radio` (new), `active`, `test_direct_creation_starts_one_shared_conversation`, `test_completed_question_holds_floor_and_resumes_in_shared_chat`, `test_question_keeps_ordinary_queue_out_of_held_turn` |
| `tests/runtime-contract.py` | `RuntimeContract` | `test_restart_keeps_pending_events_without_replaying_unknown_work` |
| `tests/tool-request-contract.py` | `RequestContract` | `test_latency_separates_callback_reservation_and_execution_waits` |
| `tests/delivery-latency-fixture.py` | module | `percentile` (new), `measure` (new) |

New nested functions: `tests/critical-steer-contract.py|CriticalDelivery.test_timing_write_failure_after_submission_does_not_replay|fail_after_submission`; `tests/radio-runtime-contract.py|RadioRuntime.eventually_radio|ready`; `tests/radio-runtime-contract.py|RadioRuntime.active|ready`.

No existing module adds a module-level name or import except `sqlite3` in `tests/critical-steer-contract.py`. The fixture adds imports `importlib.util`, `json`, `Path`, `sys`, `tempfile`, `time`, and `Runtime`; it adds module names `spec`, `fixture`, `percentile`, and `measure`. Existing runtime objects add `_dispatch_executor` and `_delivery_executor` on first use. The fixture alone sets `_fast_delivery_enabled` to compare the full pass. The default is enabled without an instance field.
