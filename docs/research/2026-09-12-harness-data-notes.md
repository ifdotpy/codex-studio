# Harness usage data, September 12

This is a read-only measurement of managed Studio threads. No model call, runtime change, agent update, or deployment was made.
The [aggregate JSON](2026-09-12-harness-token-metrics.json) contains exact values, source hashes, and query provenance.
The [collector](2026-09-12-harness-metrics.py) emits no message bodies, commands, account identities, or agent names.

## Window and method

The window is September 11, 21:01:04.813625 UTC to September 12, 21:01:04.813625 UTC, with an exclusive upper bound.
The final collection took 1.93 seconds in one read transaction. Each query has a 20-second limit.
The connection uses SQLite `mode=ro`, `query_only=ON`, and a two-second busy timeout.
No backup, write transaction, or rollout import was started.

The collector reads `analytics_agents` and queries each registered agent through the existing `(agent, at)` indexes.
It reads delivered events through `(status, agent, created)`.
There are 123 registered analytics agents. The window contains activity for 22 agents.
The small analytics registry and history status tables are read in full. Historical payload tables are not scanned globally.

The analytics endpoint is not used. In `scripts/codex_analytics.py:411`, its response-ID query reads all historical turns despite the selected date window.
Lines 424 and 425 also read all notification and limit rows before the later filter.
This is a reason for the scoped collector, not a measured endpoint performance failure.

Usage selection follows the [analytics contract](../../ANALYTICS.md).
A response-ID record takes precedence over native usage notices for its agent, thread, and turn.
The collector checks response IDs outside the selected window for each relevant agent and thread.
It excludes all native notices for those turns, even if no response record matches a particular notice.
Incomplete history can therefore omit unmatched usage. It retains distinct native notices for turns without response records.
It sums recorded `delta` fields, which contain the provider's last-response usage. It does not derive charges from cumulative counter differences.

Item filters use their indexed `at` value: start time when available, then completion or observation time.
Event filters use event creation time. Event groups join by agent and turn ID.
A group describes only delivered events created inside this window. Associated usage is also limited to this window.
The group does not describe every input in the turn lifetime or prove the event caused all subsequent work.
The 2,604 event groups and 2,599 analytics turns therefore have different inclusion rules.
Late history imports or completion events can change a repeat query for the same window.

Source commit: `670062f73559459895a7e8b7c76a2e31039ecad7`.
The five recorded installed backend source hashes match the checkout sources.
This proves file identity at collection, not the exact instructions retained by every running native thread.

Reproduce the selected window from the repository root:

```sh
/opt/homebrew/bin/python3 -B docs/research/2026-09-12-harness-metrics.py \
  --end 1789246864.813625 \
  --output /tmp/harness-token-metrics.json
```

## Provider usage and coverage

| Measurement | Response-ID records | Legacy native notices |
|---|---:|---:|
| Selected samples | 13,901 | 26,293 |
| Input tokens | 2,294,591,107 | 4,449,335,066 |
| Cached input tokens | 2,231,587,840 | 4,310,252,672 |
| Uncached input tokens | 63,003,267 | 139,082,394 |
| Output tokens | 9,941,569 | 14,075,185 |
| Cache share of input | 97.25% | 96.87% |
| Samples without a baseline | 0 | 8 |

There are 55,330 raw usage rows. Selection excludes 15,136 provisional notices and retains 40,194 samples.
All selected samples contain input and cached-input counts. Cache-write counts are recorded as zero.
Cached input is a subset of input. Reasoning output is a subset of output.
The sums measure repeated provider processing, not unique context, a monetary bill, or avoided work.
Legacy `totalTokens` need not equal the independently recorded input and output sums. The collector preserves these counters without correction.

The stored model groups contain 35,051 Luna samples, 4,922 Astra samples, and 221 Sol samples.
These are observational groups with different tasks and thread histories. They are not a comparison of model efficiency.

The window contains 825 observed compactions.
The count combines completed native compaction items and snapshots without a matching native compaction turn, as the current analytics implementation does.
Coverage retains seven lifetime capture errors. The latest is an `analytics_limit` disk I/O error at Unix time `1789233643.824995`.
This does not prove the disk is currently unavailable or identify missing token counts.
History status contains 54 `current` records and 102 `identityChanged` records. Neither proves complete coverage for every managed thread.

## Tool payloads

| Boundary and class | Calls | Output bytes |
|---|---:|---:|
| Model tools, all observed | 13,018 | 103,492,186 |
| Model `exec` | 12,930 | 103,443,484 |
| Model `wait` | 88 | 48,702 |
| Native tools, all observed | 52,658 | 586,612,797 |
| Native `commandExecution` | 38,723 | 525,913,228 |
| Native `fileChange` | 4,671 | 34,595,630 |

`exec` can contain several native calls. Its returned text can filter or truncate their output.
The two boundaries have different coverage and may describe overlapping content. Do not add them or divide them to infer savings.
The model-boundary measurements come from imported rollout protocol records, not a tokenizer or per-tool provider attribution.

For model `exec` output:

- Median: 1,773 bytes.
- 95th percentile: 40,693 bytes.
- Maximum: 46,747 bytes.
- Above 16 KiB: 2,248 calls, 70,476,077 bytes.
- Those calls contain 68.13% of observed `exec` output bytes and form 17.39% of observed `exec` calls.
- Above 64 KiB: zero observed calls.

The 16 KiB population is a useful first audit set. Its byte count is not a removable token budget.
An output can contain necessary evidence. A shorter output can also require more tool calls or model turns.
The recorded zero `failed` model calls does not prove all nested commands succeeded: the importer uses only the outer `is_error` flag.
Similarly, zero `truncated` flags does not prove native or wrapper output is complete.
See [model payload capture](../../scripts/codex_analytics.py), method `analytics_model_payload`.

Native command output has 36,981 byte measurements across 38,723 calls.
Its median is 4,893 bytes, 95th percentile 37,300 bytes, and maximum 1,048,609 bytes.
There are 681 measured outputs above 64,000 bytes, containing 210,913,599 bytes.
The decimal 64,000-byte threshold preserves the earlier report's definition. It differs from 64 KiB, which is 65,536 bytes.
Native command output forms 89.65% of observed native tool output.

The window also contains 133 native `orchestration_panel` calls and 16 `orchestration_panel_feed` calls.
These are observed historical calls inside the window. They do not imply the current catalog still advertises these tools.

## Delivered events and turn groups

| Measurement | Count |
|---|---:|
| Delivered agent messages | 2,534 |
| Explicit progress messages | 880 |
| Progress messages with a key and integer version | 694 |
| Messages without an importance field | 677 |
| Delivered monitor exits | 928 |
| Successful monitor exits | 536 |
| Groups containing only monitor exits | 522 |
| Groups containing only successful monitor exits | 272 |
| Groups containing only agent messages | 561 |
| Groups containing only followups | 445 |

Delivered monitor tails contain 2,681,855 UTF-8 bytes before event formatting.
This is event storage text size. It is not the model's final input size.
Messages without importance metadata are not classified as disposable progress.
The event groups show where to inspect behavior, not which wakes are unnecessary.

All 272 successful-monitor-only groups have observed usage in the window:

| Measurement | Response-ID records | Legacy native notices |
|---|---:|---:|
| Samples | 722 | 1,524 |
| Input tokens | 114,944,594 | 253,332,967 |
| Uncached input tokens | 2,554,962 | 4,894,567 |
| Output tokens | 507,724 | 650,151 |

The combined observed input is 368,277,561 tokens, including 7,449,529 uncached input tokens.
These tokens cover every observed response in those turns, including useful work after command completion.
They are not an estimate of savings from suppressed notifications.
A successful command can release dependent work or require evidence review. A comparison must preserve those actions.

## Historical comparison

Earlier local audit drafts cover different workloads, capture coverage, agent populations, and model assignments.
They are not a matched baseline for this snapshot.
Neither an increase nor a reduction in daily totals establishes a regression or improvement.
