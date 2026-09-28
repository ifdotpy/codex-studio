# Agent delivery latency

Source base: `e1f92cf`. This change has no live installation or paid model test.
Run `python3 -B tests/delivery-latency-fixture.py` for the isolated fake-native fixture.

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
