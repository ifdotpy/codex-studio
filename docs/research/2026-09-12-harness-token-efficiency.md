# Studio harness token efficiency

Research date: 2026-09-12. Inspected source: `670062f73559459895a7e8b7c76a2e31039ecad7`.
This investigation does not change the runtime, agent models, or active work.

## Recommendation

Optimize total team tokens through accepted completion, while preserving the required result and useful response time.
Start with large **model-facing** command results and notifications that require no further action.
Then test deferred tools and remove duplicate initial instructions.
Do not impose a worker quota, universal output size, or fixed compaction interval.
Choose these settings from matched task results, including failures and recovery work.

The strongest new evidence is model-boundary output coverage and installed Codex support for deferred dynamic tools.
Earlier [implemented reductions](2026-09-07-token-efficiency-implementation.md) already provide paged tasks, compact receipts, output references, versioned context, and selective monitor notifications.
Those mechanisms should be reused.

## Current measurements

Window: September 11, 21:01:04 UTC to September 12, 21:01:04 UTC, across 22 observed agents.
The collector uses a bounded, read-only SQLite snapshot.
The [data notes](2026-09-12-harness-data-notes.md), [aggregate JSON](2026-09-12-harness-token-metrics.json), and [collector](2026-09-12-harness-metrics.py) define the populations and queries.

| Observation | Measured value | Interpretation |
|---|---:|---|
| Usage samples with a response ID | 13,901 | Stronger identity than legacy notices, but incomplete coverage |
| Input tokens in those samples | 2,294,591,107 | Includes reused context on each response |
| Uncached input in those samples | 63,003,267 | Separate from total input and plan credits |
| Output tokens in those samples | 9,941,569 | Includes the reported reasoning subset; do not add that subset again |
| Median input per response-ID sample | 168,640 tokens | Extra model calls can repeat a large context |
| Model-facing `exec` output | 103,443,484 bytes across 12,930 calls | Direct output optimization population |
| `exec` outputs above 16 KiB | 2,248 calls, 70,476,077 bytes | 17.4% of calls produce 68.1% of these output bytes |
| `exec` output median / p95 / maximum | 1,773 / 40,693 / 46,747 bytes | Tail cases deserve inspection before a new default |
| Observed compactions | 825 | A signal to investigate, not 825 proven unnecessary operations |
| In-window groups containing only successful monitor events | 272 | Candidates for a wake-policy audit |

Another 26,293 selected samples use legacy notices. The collector excludes 15,136 provisional notices under the existing response-ID precedence rule.
Do not treat this mixed coverage as a complete bill or a complete model trace.
There are 102 history-import records marked `identityChanged` and seven historical capture errors.
These counts are coverage warnings, not proof of 102 currently broken chats or a current disk failure.

The event groups contain only delivered inputs created inside this window.
They do not establish every cause of a turn throughout its lifetime.
For the 272 monitor groups, associated response-ID samples contain 2,554,962 uncached input and 507,724 output tokens.
That includes subsequent useful work in the same turns. It is **not** an estimate of removable spend.

## 1. Reduce large results before they reach the model

Inspect the 2,248 large `exec` results first. Classify source dumps, build logs, searches, structured API output, and repeated reads.
Return the fields required for the next decision, plus an exact reference to complete evidence.
For a build, that usually means exit status, failing targets, selected diagnostics, and the full log path.
For source discovery, use paths and matching symbols before reading complete files.
For structured results, filter and aggregate inside the code tool before printing.

Studio already retains full dynamic-tool responses before applying a 16,000-byte projection threshold.
Its `orchestration_read` supports bounded retrieval and search.
This mechanism does not control every native command or code-mode result.
Sources: [`model_tool_result` and `model_read`](../../scripts/codex_efficiency.py), [boundary contract](../../ORCHESTRATION.md#model-context-and-output-budgets).

The native command logs contain 525.9 MB, but that is a different population.
Codex separates telemetry output from model output, and code mode can combine several native calls.
The inspected native implementation applies a model output policy and supports `max_output_tokens`.
Never divide native bytes by model bytes to claim a saving.
Source: [Codex 0.154.0 `ExecCommandToolOutput`](https://github.com/openai/codex/blob/6b9826e3aa83b1a5947db50f4332cb9c65f1b340/codex-rs/core/src/tools/context.rs#L348-L475).

pi provides a useful implementation reference: bounded output, original-size metadata, and a full-output artifact.
Copy that structure, not its numeric defaults.
Preserve diagnostics from the middle of logs and test Unicode, long lines, restart, and access control.
Measure follow-up reads: excessive truncation can increase total tokens.
Source: [pinned pi output implementation](2026-09-12-harness-external-sources.md#1-pi-bound-output-and-preserve-a-retrieval-path).

## 2. Remove notifications that require no model decision

The current runtime supports `wake_on=failure`; successful results remain stored.
Use it only when success does not release dependent work or require interpretation.
For required completion events, return a compact receipt and evidence reference instead of repeating command text and log tails.
Source: [`_monitor_exit_event`](../../scripts/codex_runtime.py).

There are also 561 in-window groups containing only agent messages.
The data contains 880 progress messages, of which 694 have the versioned shape needed for replacement.
Another 677 agent messages lack importance metadata.
Their missing metadata does not make them disposable.

Make the existing progress fields easier to use, then compare batch windows under actual response-time requirements.
Questions, blockers, user input, failures, and review evidence must retain their delivery behavior.
The current batch waits one second and replaces progress only for matching sender, room, topic, and valid versions.
Source: [`progress_batch_ready` and `model_event_text`](../../scripts/codex_efficiency.py).

One result can also arrive through both `work_review` and `child_result`.
Link both events to the same submitted result identity when they represent the same evidence.
Then send status and a reference once, preserving distinct findings and submissions.
Similar wording alone is insufficient for deduplication.
Sources: [`work_action`](../../scripts/codex_work.py), [`parent_event`](../../scripts/codex_runtime.py).

## 3. Test deferred tool loading on compatible sessions

Current Studio catalogs contain 23 tools for leads and 20 for workers.
Their compact JSON sizes are 14,994 and 12,779 bytes, excluding native tools, plugins, and skills.
Panel tools have already been retired; reducing their schemas is no longer a new opportunity.
Source: [code measurements](2026-09-12-harness-code-metrics.json), [`tool_definitions`](../../scripts/codex_runtime.py).

The installed `codex-cli 0.154.0` generated an experimental schema containing `deferLoading` on dynamic functions, including functions inside namespaces.
The [schema extract](2026-09-12-harness-native-schema.json) records the command and schema hash.
Pinned Codex code implements deferred exposure, but discovery requires model search support and provider namespace capability.
This is stronger evidence than assuming a Responses API feature also exists in app-server.
It is not proof that discovery works in every active Studio session.
Source: [protocol and capability gates](2026-09-12-harness-external-sources.md#7-codex-deferred-dynamic-tools-are-present-but-gated).

Keep frequent worker operations directly available. Defer rare discovery, watch, and administrative operations only after checking actual use.
Test discovery, invocation, thread resume, model switch, and unsupported-capability fallback.
Keep tool order stable. Include discovery calls and mistaken tool selection in the cost comparison.
Do not modify existing active threads merely to refresh their catalog.

## 4. Deliver initial role and progress guidance once

`new_thread_params` supplies the role and progress guidance in developer instructions.
The first `model_turn_context` supplies both again because no delivered context manifest exists yet.
The role contract explicitly tests both role deliveries.
Sources: [`new_thread_params`](../../scripts/codex_runtime.py), [`model_turn_context`](../../scripts/codex_efficiency.py), [role contract](../../tests/role-skills-contract.py).

With the recorded source paths and a synthetic progress path, the repeated text is 7,703 bytes per lead and 5,048 per worker.
These are byte measurements, not model-token counts. Real path lengths change them slightly.
Sources: [code measurements](2026-09-12-harness-code-metrics.json), [reproduction](2026-09-12-harness-code-metrics.py).

Keep the initial authoritative developer instructions and record their confirmed preparation identity.
Skip only the proven duplicate initial delivery.
Continue to deliver updates, uncertain deliveries, replacement-thread context, and required context after compaction.
Do not remove role boundaries or recovery rules merely because another paragraph sounds similar.

## 5. Reduce repeated discovery and compaction recovery

Give each worker the current decision, owned files, relevant facts, exact evidence paths, and its acceptance check.
Keep full logs and historical discussion outside the brief.
Use one authoritative result record so review does not repeat the worker's exploration.
Choose team size from independent work and measured total team cost, not from a minimum worker count.
The current orchestrator skill already says to choose team size from useful work and resource limits.
Source: [orchestrator role](../../.agents/skills/codex-orchestrator/SKILL.md).

Aider's ranked repository map is an optional experiment for unfamiliar repositories.
It should carry a source revision and a context budget, and compete against targeted search.
Its hand-tuned ranking weights are not optimal Studio defaults.
pi and OpenHands preserve selected history during compaction and expose useful trigger or summary accounting.
Use these ideas to inspect native compaction, not to add a second lossy summary layer.
Sources: [pinned Aider, pi, and OpenHands analysis](2026-09-12-harness-external-sources.md).

## Evaluation and implementation order

Use matched tasks with the same source revision, model, effort, permissions, and acceptance checks.
Count all lead, worker, review, failed-attempt, discovery, and summary calls through completion.
Measure input, uncached input, output, latency, corrections, lost requirements, and compactions separately.
The target is lower tokens per accepted task without a worse required result or unacceptable delay.
Do not optimize cache percentage alone.
Cache reuse depends on the rendered prefix and its boundaries; compaction can alter that prefix.
Source: [OpenAI prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching).

| Step | First experiment | Evidence required before broader use |
|---|---|---|
| 1 | Summarize the largest model-facing result classes | Same diagnosis and retained evidence; fewer total tokens including detail reads |
| 2 | Suppress routine success wakes and improve progress metadata | No missed dependent work, questions, or failures; fewer model calls |
| 3 | Use a small deferred worker catalog | Correct discovery across capability and lifecycle boundaries; net lower input |
| 4 | Remove confirmed duplicate startup context | Role, uncertainty, resume, and compaction contracts remain correct |
| 5 | Test task briefs, optional maps, and compaction diagnostics | Less repeated discovery and recovery through accepted completion |

No overall saving percentage is established by this investigation.
Historical windows contain different workloads, models, and capture coverage; they are not an A/B test.
This investigation did not restart the backend or alter agent settings.
