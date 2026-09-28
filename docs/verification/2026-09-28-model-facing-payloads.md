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

| Tool | Rows | Mean B | p95 B | Max B |
|---|---:|---:|---:|---:|
| `orchestration_task` | 13,356 | 10,571 | 12,384 | 1,257,983 |
| `orchestration_peers` | 1,980 | 26,366 | 87,739 | 465,453 |
| `orchestration_status` | 2,601 | 40,856 | 152,416 | 546,275 |
| `orchestration_context` | 237 | 4,198 | 17,477 | 19,772 |
| `orchestration_chat_read` | 2,752 | 52,315 | 119,647 | 169,060 |

These values include JSON wrappers and time metadata. They describe durable
results, not the exact bytes sent to a model after projection. The dynamic tool
path already clipped large model results at 16,000 bytes; the new threshold is
8,000 bytes for input-text payloads. The full saved result remains available by
`outputRef` through `orchestration_read`.

## Saved event baseline

UTF-8 bytes of `runtime_events.text`, measured per event kind from the live
database. Counts, means and maxima are provided for every kind present.

| Event kind | Rows | Mean B | Max B |
|---|---:|---:|---:|
| `agent_message` | 23,624 | 957 | 11,963 |
| `browser_recovery` | 2 | 659 | 659 |
| `chat_review` | 44 | 7,407 | 18,673 |
| `child_result` | 7,919 | 614 | 11,719 |
| `complaint` | 25 | 1,191 | 1,989 |
| `complaint_response` | 114 | 682 | 2,800 |
| `followup` | 6,450 | 680 | 5,519 |
| `harness_notice` | 2 | 703 | 703 |
| `monitor_exit` | 12,776 | 3,490 | 45,418 |
| `rule` | 274 | 658 | 12,234 |
| `user` | 2,547 | 498 | 8,851 |
| `user_task_completed` | 8 | 510 | 684 |
| `work_decision` | 2,004 | 795 | 9,736 |
| `work_ready` | 19 | 117 | 136 |
| `work_released` | 10 | 253 | 375 |
| `work_review` | 2,741 | 2,567 | 127,947 |

The model event projection now limits a large synthetic event to 1,000 bytes and
the event payload budget to 8,000 bytes per batch. Each clipped event includes
stable identity/status fields when they fit, a truncation marker, and an
`event:<id>` reference for `orchestration_read`. Child completion events now
store the full child result instead of slicing it at 16,000 characters before
delivery.

## Post-change fixture measurements

The size-budget contract used Python 3.14.7 and measured serialized UTF-8 bytes
on the returned model payloads. Synthetic load: 40,000-byte tool detail; 32
large work-review events; 60 tasks; and 30 workers.

| Default model output | Measured B | Budget |
|---|---:|---:|
| Large tool result preview | 2,153 | 10,000 |
| 32-event batch | 7,114 | 10,000 |
| 60-task first page | 2,574 | 10,000 |
| Peer directory page with 30 workers | 4,062 | 10,000 |
| Status output after generic tool projection | 7,049 | 10,000 |

On those fixtures, the 40,112-byte tool content item became 2,153 bytes, saving
**37,959 bytes for that call**. The 32-event batch was 1,442,892 bytes with
event envelopes before projection and 7,114 bytes after, saving **1,435,778
bytes for that batch**. Savings depend on result size; small outputs below the
projection limits are unchanged except for removed repeated fields.

The contracts also verify that an event preview resolves to its exact full saved
text, oversized child results remain intact, task cursors still page all items,
and truncated tool results expose a usable read reference. Chat model pages are
bounded at 9,000 bytes. No tool input schemas changed, so the catalog does not
need a schema refresh.

## New-thread static text

The four text blocks measured by the volume audit, excluding tool definitions:

| Block | Before B | After B |
|---|---:|---:|
| Core `INSTRUCTIONS` | 3,384 | 1,044 |
| Subagent role skill | 4,985 | 4,985 |
| Shared workspace skill | 15,447 | 15,447 |
| Panel guidance | 3,694 | 3,694 |
| **Total** | **27,510** | **25,170** |

The reduction is **2,340 bytes**, or approximately **585 tokens** using the
rough 4-bytes-per-token estimate. The after total is approximately **6,293
tokens**. Tool definitions are excluded from both totals and are unchanged.
Provider caching and tokenization vary, so these are text-size estimates rather
than billed token counts.

The core block now carries shared authority, scope, event and retry rules once;
tool-specific workflows remain in tool descriptions, and role duties remain in
their role skill. Peer and status responses no longer repeat policy/help
sentences or null parent IDs. The task, peer, and chat page interfaces keep their
stable IDs, status, cursors and paging/detail operations.

The changes are server-side Python and are suitable for the existing live-patch
path. This task did not apply a patch or interact with the backend. The tool
schemas are unchanged; if a later change modifies them, the import-built `TOOLS`
catalog requires its documented manual refresh.

## Verification

Passed on Python 3.14.7: `limit-payloads-contract.py` (12),
`token-efficiency-contract.py` (15), `runtime-contract.py` (59),
`workspace-contract.py` (43), `agent-management-contract.py` (47),
`radio-runtime-contract.py` (7), `harness-response-contract.py` (10), and
`spawn-task-contract.py` (11).

`radio-contract.py` has one failure when rerun alone:
`test_observed_turn_before_ack_mirrors_but_does_not_advance` observed `waiting`
where it expects `blocked`. `team-chat-isolation-contract.py` also has one
failure when rerun alone: `test_dispatch_cancels_old_foreign_input_and_keeps_same_team_input`
observed zero dispatched batches rather than one. The implementation changes do
not touch either contract's dispatch/status paths. These tests were not changed.
