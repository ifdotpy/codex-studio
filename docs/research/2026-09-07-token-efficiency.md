# Token efficiency in Codex Studio

Date: 2026-09-07. Source baseline: `58128a0`. Research only; no runtime or model settings changed.

## Decision

Prioritize compact model-facing tool results, then fewer redundant wakeups and repeated instructions.
Keep full records in SQLite and the UI. Give the model exact references for additional reads.
Do not reduce the user's model defaults, remove required panel screenshots, weaken acceptance checks,
or repeat an uncertain operation to reduce apparent cost.

The largest confirmed defect is a duplicated task list, with all result and decision history.
The next is the shared status/peers response, which includes monitor logs and tool schemas.
These can be reduced without a summarization model or a different reasoning model.

## Evidence and limits

The read-only analytics snapshot covers 2026-09-06 21:00:54 UTC through
2026-09-07 21:00:54 UTC. It includes all managed accounts and teams.
The [aggregate measurements](2026-09-07-token-efficiency-metrics.json) retain the exact time bounds.

Usage follows the existing [analytics contract](../../ANALYTICS.md): when a turn has response-ID
records, exclude its native-notice counters. All 24,925 selected samples have response IDs.
The underlying history can still be incomplete. These figures are observed processing, not a bill
or unique retained context. Cached input is part of input; reasoning is part of output.

| Observed provider metric | Count |
|---|---:|
| Input tokens | 3,409,776,414 |
| Cached input tokens | 3,327,492,480 |
| Input outside the cache | 82,283,934 |
| Output tokens | 8,734,011 |
| Reasoning output tokens | 3,636,785 |
| Cached share of input | 97.59% |
| Mean input per recorded response | 136,801 tokens |

Cache reuse is already high. This does not make large contexts free. Shorter results can reduce
new input, repeated cached input, compaction pressure, and unnecessary read/interpret cycles.
No overall token-saving percentage is established by this audit.

### Tool bytes, with separate boundaries

| Tool | Boundary | Calls | Total output bytes | Largest output bytes |
|---|---|---:|---:|---:|
| commandExecution | app-server | 15,433 | 206,365,195 | 1,048,608 |
| orchestration_task | app-server | 550 | 193,943,521 | 1,231,977 |
| orchestration_status | app-server | 195 | 79,858,263 | 596,572 |
| orchestration_peers | app-server | 193 | 65,984,659 | 472,250 |
| orchestration_chat_read | app-server | 221 | 9,646,628 | 105,195 |
| exec | model | 18,540 | 166,166,665 | 759,312 |
| wait | model | 3,201 | 13,041,898 | 130,961 |

Do not add the two boundaries. One model `exec` can wrap several native calls and print only a subset.
Bytes are not tokenizer counts. A large native response can be reduced by caller code before the model
receives it. The model-boundary measurements nevertheless confirm substantial tool-output volume.
The detailed grouping query uses `payloadBoundary`, tool name, and the snapshot's timestamp bounds.

## Priorities

### 1. Compact task list and mutation receipts

[work_action](../../scripts/codex_work.py) returns the same complete list twice, as `items` and `tasks`.
Each entry includes description, every submitted result, and every acceptance/rejection decision.
Mutations also return the complete updated task through `work_view`.

A saved real list response contains 77 tasks and 1,202,958 UTF-8 bytes. Each copy is 601,468 bytes.
Within one copy, result history contributes 415,770 bytes and decisions 98,057 bytes.
A deterministic projection with `id`, `title`, `owner`, `status`, `version`, and `blockedBy` is
16,553 bytes for all 77 entries, a 98.62% reduction for that example's JSON payload.
This is a projected response-size reduction, not a measured end-to-end token reduction.

Proposed contract:

- `list`: one canonical array, status/owner filters, a cursor, and a default page of 20 tasks.
- `get(id)`: task details and the latest result/decision. Request older history explicitly.
- Mutation: `requestId`, `taskId`, `version`, resulting status, and references to created evidence.
- Full UI records and durable operation receipts remain unchanged.
- Version the model response and update consumers. Do not silently remove an alias used by existing callers.

Suggested target: list pages below 8 KiB for ordinary titles, excluding explicitly requested details.
Validate stale versions, task ownership, dependencies, and unknown outcomes with existing contracts.

### 2. Separate peers, status, monitor details, and tool discovery

The [dynamic-tool handler](../../scripts/codex_runtime.py) treats `orchestration_status` and
`orchestration_peers` as the same broad bundle: team, peer directory, rooms, profiles, and workspace
schemas. Status adds recent chat bodies. `team()` also includes monitor records with output tails.

Two saved examples:

| Response | UTF-8 bytes | Main fields |
|---|---:|---|
| status | 513,018 | monitors 364,830; recent chats 107,018; tool schemas 10,025 |
| peers | 436,810 | monitors 395,656; rooms 16,083; tool schemas 10,025 |

A directory-only projection from either example is 6,587 bytes. This excludes room discovery and
is a lower-bound example, not a complete replacement contract.

Proposed contract:

- Peers: agent identities, roles, state, readable room IDs, and a cursor. Default to the current team;
  retain an explicit cross-team discovery option.
- Status: active/waiting/failed counts and compact changed records, with a `since_revision` cursor.
- Monitor detail: state, process identity, actual exit code, output reference, and a bounded excerpt.
- Tool catalog and worker profiles: explicit versioned reads. Do not attach schemas to every status response.
- Never label absent or pending receipts as completed. A smaller response must preserve uncertainty.

Use a separate model projection instead of shrinking the UI snapshot. The UI still needs its full
background-job controls, room navigation, and historical records.

### 3. Bounded output with durable references

The model-boundary `exec` output reaches 759,312 bytes in one observed call.
The native command output reaches 1,048,608 bytes. Generic character slicing is insufficient:
it can cut a JSON object, hide a failure, or force another full read.

Use a structured result envelope for managed tools:

```json
{
  "requestId": "stable-id",
  "state": "completed",
  "exitCode": 1,
  "summary": "Two checks failed",
  "excerpt": "bounded relevant output",
  "outputRef": "artifact-id",
  "bytes": 820000,
  "truncated": true,
  "nextCursor": "opaque-cursor"
}
```

This is a proposal, not the current API. Use byte/range reads, search, and focused line retrieval for
full output. Keep immutable logs and exact receipts. Failure summaries should retain the named failing
checks and enough surrounding output to locate the evidence. Use deterministic extraction where possible.
Do not call another LLM solely to summarize every command result.

For native `exec_command` inside `functions.exec`, first use exposed output limits and focused printing.
Studio does not directly own every model-level tool result. Measure that boundary before promising
universal output enforcement. Managed orchestration tools are the first implementation target.

### 4. Coalesce informational events without delaying decisions

The snapshot records 1,372 agent-message events, 412 child results, and 193 work-review events,
compared with 101 user events. Counts are events, not proof of duplicate delivery or model invocations.
The runtime already batches up to 32 pending events when starting a turn. It has no deliberate
short coalescing interval for an otherwise idle lead.

Proposed contract:

- Informational progress updates carry task ID, source, version, and importance.
- Consecutive updates for the same task can share one wakeup and one compact change set.
- Questions, blockers, permission decisions, failures, and explicit user messages remain immediate.
- Preserve individual event identities and delivery receipts even when the model sees a batch.
- Prefer one canonical submitted result. Link its message and child completion to that result instead
  of reinjecting the full report through several paths.

Trial a configurable short window, for example 0.5 to 2 seconds, only for informational events.
Measure lead wakeups, response input, duplicate report reads, and decision latency.
Do not suppress events through fuzzy text matching. A repeated message can contain a changed decision.

### 5. Versioned task context and capability instructions

The [thread setup](../../scripts/codex_runtime.py) always adds 5,364 bytes of Studio instructions and
4,267 bytes of panel guidance. The 22 initial dynamic-tool definitions occupy 15,732 JSON bytes.
Additional profile, account, native Codex, and repository instructions are outside these counts.
The [worker preamble](../../prompts/worker-preamble.md) belongs to `codex-swarm.mjs`; it is not proof
of another automatic injection into Studio app-server workers.

Keep a small stable core covering authority, receipts, uncertainty, ownership, and completion.
Provide the full panel catalog and detailed capability guides on demand. Do not remove panel style
and height validation from the harness. Capability descriptions must remain discoverable.
Keep tool order and schemas stable within a thread; avoid changing the prefix per call.

Each turn currently appends the full shared plan, if present, and every outstanding lead complaint.
No shared plans existed in this database snapshot, so plan repetition is a code finding without a
measured current saving. Complaint repetition is implemented and observed in earlier recovery work.
Send content once per version, then compact IDs/statuses. After compaction or a fresh thread, resend
what the agent needs. Complaint response enforcement must remain intact.

Time awareness is an explicit owner requirement. [codex_time.py](../../scripts/codex_time.py) appends
immutable timestamps and retains them for a replay. It does not rewrite the earlier prompt prefix.
Native current-time reminders are also enabled. Audit duplicate clock representation, but retain
one clear timestamp at every required message/tool boundary. Do not claim that any timestamp destroys
all prompt caching.

### 6. Keep programmatic background updates outside model turns

Monitors, watch gates, and panel feeds already support this design. Waiting time itself does not
consume generation tokens. Repeated model wakeups to poll or interpret unchanged state can consume tokens.
The 3,201 model `wait` calls are not all redundant: they can retrieve required command results.
Classify them before replacing them.

Use panel feeds for EC2 state, test counters, and build progress. The model creates the panel once,
then the background script updates its data. Keep the rendered image returned by an agent's panel `set`
call, as requested by the owner. Avoid extra `get` screenshots for unchanged content.
The observed 19 panel calls are a much smaller current opportunity than task/status output.

### 7. Bound worker lifetimes and test reasoning policies later

Studio already starts workers as separate native threads; it does not automatically fork the full lead
history. Preserve that useful boundary. Give a new bounded task its source revision, file scope,
acceptance criteria, dependencies, and relevant result references.
Do not carry unrelated completed task history into a new assignment simply to reuse an old worker.
Create a successor only after reconciling pending operations, monitors, leases, and ownership.
A numeric context threshold alone must not abandon an active command or unknown receipt.

Reasoning is 41.64% of observed output tokens, but this does not prove it is unnecessary. Preserve the
user's Luna Max default. Lower reasoning or another model is a separate opt-in experiment for a defined
class of mechanical tasks. Fast mode is not a demonstrated token-saving mechanism.

## Delivery order and acceptance

1. Add compact model views for task list/get/mutations and status/peers. Preserve UI and recovery APIs.
2. Add durable output references and bounded reads. Measure both native and model payload boundaries.
3. Add version-aware informational event batches and context injection. Then test worker-lifetime policies.

For comparison, use the same repository revision and recorded workload: a small patch, a multi-file
feature, and a multi-worker review. Retain model/effort/account settings. Compare tokens per accepted task,
uncached input, output/reasoning, cache ratio, compactions, wakeups, payload percentiles, and elapsed time.
Also compare correctness, missed evidence, additional reads, recovery, and user-facing response latency.
A smaller response that forces many detail calls can be a net loss.

Existing checks to extend: `workspace-contract.py`, `tool-request-contract.py`,
`send-transcript-contract.py`, `analytics-contract.py`, and the monitor/panel/complaint contracts.
Add failure cases for truncation, ambiguous receipts, cursor replay, stale versions, and scoped access.
Do not claim a global saving until those end-to-end measurements exist.

## Official product constraints

See the companion [official-source review](2026-09-07-token-efficiency-official-sources.md).
Direct Responses API controls must not be assumed to exist on Studio's ChatGPT-authenticated
Codex app-server connection. No direct model calls or model-setting changes were made for this audit.
