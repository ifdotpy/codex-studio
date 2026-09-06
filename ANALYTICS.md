# Context analytics

Studio records measurements locally in `canvas.sqlite3`. Open **Context analytics**
below the composer to inspect one agent, its team, or all managed agents.
The report separates provider token usage from tool payload sizes.

## What the numbers mean

| Measurement | Source and limit |
|---|---|
| Input, output, total tokens | Codex usage records for observed model responses. |
| Cached input tokens | A subset of input tokens. Do not add it to input again. |
| Reasoning output tokens | A subset of output tokens. Do not add it to output again. |
| Cache-write input tokens | The separate counter supplied by Codex, when available. |
| Context percentage | The last reported total divided by the reported model context window. |
| Model tool payload | The recorded arguments or result at the model tool boundary. |
| Native tool payload | The arguments or result that app-server exposes for a native tool. |
| Bytes and characters | UTF-8 bytes and Unicode characters in text or a compact JSON representation. |
| Duration | A provider duration when supplied; otherwise an observed interval when both endpoints exist. |
| Images | Observed count, decoded byte size for embedded base64 data, and available PNG dimensions. These are not image tokens. |

A model `exec` call can run several native tools. Its result can omit or truncate
their output. Adding that result to the native outputs counts some text twice.
Inspect these boundaries separately.

Codex does not provide exact token attribution to each tool or retained context
item. A tool result's byte count does not prove its token count, retained context
share, or price. Request usage includes instructions, conversation history,
messages, tools, and other model input. Studio does not assign that usage to the
last tool that ran.

Context changes after compaction. The sum of historical tool output is not the
current context size. Missing usage and hidden reasoning text remain unknown.

## History and coverage

Live app-server events supply current measurements. Local Codex rollout files
provide older history and additional model response identities and payloads.
Studio reads only the managed thread's file in its registered account profile.
The scan does not call a model or modify Codex session files.

Reports preserve source timestamps and measurement provenance. Old logs can lack
fields that newer Codex versions supply. Missing files, incomplete scans, and
unsupported records limit coverage. The report must not present missing values
as zero.

Cached input and repeated context count on each model response because the
provider reports them that way. This is cumulative processing, not unique text.
Repeated notices for the same response do not create another charge.

The native notice counter can reset after a backend restart while the response
record retains lifetime totals. Studio uses response IDs when those records exist
for a turn. Unreconciled notices remain available separately and do not add to
that turn's token total. Requests can appear after the log reader catches up.
For turns without response records, the report uses distinct native notices and
marks them as legacy usage. A partial log cannot prove complete lifetime usage.

The report also retains observed turn durations, time to first output, notification
counts, compaction snapshots, and account limit reports. Model, account, and agent
groups retain their own token sums. Monitor and approval states come from the
runtime. Their status counts describe stored state, not an invented event history.

The existing CodexBar cost display remains separate. It estimates API cost across
local logs; it is neither a ChatGPT bill nor a per-tool price.

## Storage and export

Analytics tables retain usage counters and payload measurements. They do not
duplicate full prompts, tool outputs, or image data. Call metadata can contain
commands, paths, and errors. Treat exported reports as local session data.

Use the report's JSON export for the selected scope and period. This retains
available detailed fields for analysis outside Studio.

The same data is available through `GET /api/analytics`. Query fields:

- `agent`: the managed agent ID.
- `scope`: `agent`, `team`, or `all`.
- `from`, `to`: optional Unix timestamps in seconds.
- `tool`: an optional tool name. This filters tool records, not provider tokens.
- `limit`, `offset`: the call-record page, with at most 500 rows per page.
- `export=1`: all selected records, including raw provider usage metadata.

Item date filters use the start time when known. Otherwise they use completion
time or the first observation. Imported history can establish an earlier start.

Normal responses contain at most 500 recent usage samples and 100 recent
non-tool item records. Exports include the full selected history.
Analytics adds no messages or tool definitions to model input.

## Checks

```sh
python3 -B tests/analytics-contract.py
python3 -B tests/analytics-history-contract.py
npm --prefix web run build
node tests/analytics-ui.mjs
```
