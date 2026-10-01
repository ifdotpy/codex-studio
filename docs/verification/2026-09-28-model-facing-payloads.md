# Model-facing payloads: measured changes

Measurements for task `210e155e-e54d-4246-8ec1-d57822c441cb`, 2026-09-28.
The live SQLite database was opened with `mode=ro`. The backend and installed
application were not changed or restarted. Live result and event distributions
below are the pre-change saved-record baseline; post-change behavior is measured
with the contracts against the updated source, since the live backend was not
patched.

## Saved tool-result baseline

UTF-8 bytes of `runtime_tool_results.result`, grouped by the tool name in the
matching `runtime_tool_requests.record`. This is the full saved response before
the model-facing projection. All available rows were included; mean, p95 and
maximum use the saved rows present at measurement time.

| Tool                      |   Rows | Mean B |   p95 B |     Max B |
| ------------------------- | -----: | -----: | ------: | --------: |
| `orchestration_task`      | 13,356 | 10,571 |  12,384 | 1,257,983 |
| `orchestration_peers`     |  1,980 | 26,366 |  87,739 |   465,453 |
| `orchestration_status`    |  2,601 | 40,856 | 152,416 |   546,275 |
| `orchestration_context`   |    237 |  4,198 |  17,477 |    19,772 |
| `orchestration_chat_read` |  2,752 | 52,315 | 119,647 |   169,060 |

These values include JSON wrappers and time metadata. They describe durable
results, not the exact bytes sent to a model after projection. The existing
model-facing threshold remains 16,000 bytes for input-text payloads, with an
excerpt up to 4,000 bytes. The full saved result remains available by
`outputRef` through `orchestration_read`.

## Saved event baseline

UTF-8 bytes of `runtime_events.text`, measured per event kind from the live
database. Counts, means and maxima are provided for every kind present.

| Event kind            |   Rows | Mean B |   Max B |
| --------------------- | -----: | -----: | ------: |
| `agent_message`       | 23,624 |    957 |  11,963 |
| `browser_recovery`    |      2 |    659 |     659 |
| `chat_review`         |     44 |  7,407 |  18,673 |
| `child_result`        |  7,919 |    614 |  11,719 |
| `complaint`           |     25 |  1,191 |   1,989 |
| `complaint_response`  |    114 |    682 |   2,800 |
| `followup`            |  6,450 |    680 |   5,519 |
| `harness_notice`      |      2 |    703 |     703 |
| `monitor_exit`        | 12,776 |  3,490 |  45,418 |
| `rule`                |    274 |    658 |  12,234 |
| `user`                |  2,547 |    498 |   8,851 |
| `user_task_completed` |      8 |    510 |     684 |
| `work_decision`       |  2,004 |    795 |   9,736 |
| `work_ready`          |     19 |    117 |     136 |
| `work_released`       |     10 |    253 |     375 |
| `work_review`         |  2,741 |  2,567 | 127,947 |

The event projection keeps its 3,000-byte per-event and 24,000-byte per-batch
limits. `user`, `followup`, and `work_decision` are never truncated because they
carry instructions. Other clipped events retain identity/status when they fit,
a truncation marker, and an `event:<id>` reference for `orchestration_read`.
Child completion events now store the full child result instead of slicing it
at 16,000 characters before delivery.

## Post-change fixture measurements

The size-budget contract used Python 3.14.7 and measured serialized UTF-8 bytes
on the returned model payloads. Synthetic load: 40,000-byte tool detail; 32
large work-review events; 60 tasks; and 30 workers.

| Default model output                        | Measured B | Budget |
| ------------------------------------------- | ---------: | -----: |
| Large tool result preview                   |      4,338 | 16,000 |
| 32-event batch                              |     23,134 | 24,000 |
| 60-task first page                          |      2,578 | 13,000 |
| Peer directory page with 30 workers         |      4,059 | 13,000 |
| Status output after generic tool projection |      6,568 | 16,000 |

On those fixtures, the 40,112-byte tool content item became 4,338 bytes, saving
**35,774 bytes for that call**. The 32-event batch was 1,442,572 bytes with
event envelopes before projection and 23,134 bytes after, saving **1,419,438
bytes for that batch**. User, followup and work-decision fixtures larger than
these limits remain byte-for-byte present in the model event text.

The contracts also verify that an event preview resolves to its exact full saved
text, oversized child results remain intact, task cursors still page all items,
and truncated tool results expose a usable read reference. Chat model pages are
bounded at 13,000 bytes. No tool input schemas changed, so the catalog does not
need a schema refresh.

## Read-after-truncation cost estimate

The read-only sample used the 10,000 most recent tool requests, from 2026-09-26
01:49:54 UTC through 2026-09-28 16:20:33 UTC. For each call, the model-visible
saved transcript item was checked for a truncated-result reference. Later
`orchestration_read` calls were matched by agent, thread, and exact `output_ref`.

There were 24 truncated outputs in the 10,000-call sample (0.24%). Seven had
at least one later read of that exact result: **29.2%**. Those outputs produced
20 matching read calls, or **0.833 extra read steps per truncated output**.
Average bytes removed from the model-visible tool result were 26,599 per
truncated output. A read is counted only when its call also falls within the
10,000-request sample, so the follow-up rate is a lower bound.

Across the latest 1,000 `analytics_usage` rows, mean `cachedInputTokens` was
152,122. At the same rough 4 bytes per token estimate, one extra tool step
re-sends about **608,489 bytes** of cached context. Expected net per truncated
result is therefore 26,599 saved bytes minus 0.833 × 608,489, or approximately
**−480,475 bytes**. For the 24 observed truncations, that is about 638,376 B
saved minus 20 × 608,489 B of extra context, or **−11,531,404 bytes**. This
excludes the read response body, so the estimate is conservative about read
cost. It supports keeping the larger thresholds to avoid prompting extra reads.

## New-thread static text

The core `INSTRUCTIONS` text was restored verbatim. No instruction sentences
were removed, so no sentence-to-shared-skill relocation applies. The four
measured blocks (excluding tool definitions) remain unchanged: 3,384 B core
instructions, 4,985 B role skill, 15,447 B shared workspace skill, and 3,694 B
panel guidance; total **27,510 B**, about **6,878 rough tokens**. Tool schemas
are unchanged.

Peer/status responses still omit repeated policy/help sentences and null parent
IDs. Task, peer, and chat page interfaces retain stable IDs, status, cursors and
paging/detail operations.

The changes are server-side Python and are suitable for the existing live-patch
path. This task did not apply a patch or interact with the backend. The tool
schemas are unchanged; if a later change modifies them, the import-built `TOOLS`
catalog requires its documented manual refresh.

## Verification

Passed on Python 3.14.7: `limit-payloads-contract.py` (13),
`token-efficiency-contract.py` (15), `runtime-contract.py` (59),
`workspace-contract.py` (43), `agent-management-contract.py` (47),
`radio-runtime-contract.py` (7), `harness-response-contract.py` (10), and
`spawn-task-contract.py` (11), `agent-review-contract.py` (16),
`task-completion-recovery-contract.py` (3), and `tool-request-contract.py` (18).

On the clean main worktree at `f757ec208cc70fdd08825ae8ec37e65b279bb731`, both
remaining failures were rerun without this change and failed identically, so
they predate this branch. `radio-contract.py`:
`test_observed_turn_before_ack_mirrors_but_does_not_advance` observed `waiting`
where it expects `blocked`. `team-chat-isolation-contract.py`:
`test_dispatch_cancels_old_foreign_input_and_keeps_same_team_input` observed
zero dispatched batches rather than one. On this branch the team-chat suite had
7 passing tests and that one failure. The new instruction-event contract passed.
