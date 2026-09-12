# Harness token efficiency: external mechanisms

Access date: 2026-09-12. Studio source: `670062f73559459895a7e8b7c76a2e31039ecad7`.
This is source inspection, not an execution benchmark. No foreign code was run.
No private Studio data was sent to these sources.
The prior local September 9 draft was historical input, not a current measurement.

## Most useful transfers

| Priority | Mechanism | Studio experiment | Main risk |
|---|---|---|---|
| 1 | Bounded tool output with complete artifact access | Return command status, selected diagnostics, byte counts, and an exact log reference before model input | An important failure can be outside the excerpt |
| 2 | Stable instructions and tool definitions | Remove duplicate instruction delivery; preserve order and exact content for unchanged prefixes | A shorter prefix does not necessarily cost less |
| 3 | Context limits with measured compaction progress | Measure native compaction cost, retained context, and later recovery work | Summaries can discard constraints or require extra calls |
| 4 | Small tool catalog with optional discovery | Compare role-specific catalogs and discovery overhead | Discovery adds a decision and can miss a required tool |
| 5 | Optional repository map with a token budget | Compare a task-specific map against targeted `rg` and file reads | Map generation and stale symbols can outweigh saved reads |

These are hypotheses to test. None establishes a Studio savings percentage.
Keep model, reasoning effort, task inputs, and acceptance requirements fixed.
Count lead, worker, summary, retry, and review usage through accepted completion.

## 1. pi: bound output and preserve a retrieval path

Inspected pi-mono commit `71dca871bc80b6bc97be37f0ca3189399d651fff`.
Its shared output limit is 2,000 lines or 50 KiB, whichever comes first.
The result records original and returned sizes, truncation cause, and partial-line state.
These are byte and line limits, not token counts.
Source: [truncate.ts, lines 1 to 40](https://github.com/badlogic/pi-mono/blob/71dca871bc80b6bc97be37f0ca3189399d651fff/packages/agent/src/harness/utils/truncate.ts#L1-L40).

The Bash tool returns the tail and includes a path to full output when truncated.
Its notice identifies the returned line range or oversized final line.
Source: [bash.ts, description and result assembly](https://github.com/badlogic/pi-mono/blob/71dca871bc80b6bc97be37f0ca3189399d651fff/packages/coding-agent/src/core/tools/bash.ts#L234-L331).

Transfer: preserve the complete artifact, then return a small result tailored to the command.
Include failures from anywhere in the log, not only its tail.
Test long single lines, Unicode, failures in the middle, and retrieval after restart.
Measure subsequent detail reads: a smaller first result can produce more model turns.
Do not copy pi's numeric limits without Studio measurements.

Studio already applies a similar projection to its own dynamic tool results in
[`model_tool_result`](../../scripts/codex_efficiency.py).
That method does not wrap every native shell result.
Reducing a UI record after app-server delivery cannot remove text already received by the model.

## 2. Aider: spend a bounded context budget on relevant symbols

Inspected Aider commit `5dc9490bb35f9729ef2c95d00a19ccd30c26339c`.
`get_ranked_tags` creates a definition/reference graph and applies personalized PageRank.
Open files, mentioned paths, and identifiers influence ranking.
`get_ranked_tags_map_uncached` searches the ranked prefix for a map near the configured token budget.
The implementation permits a 15% tolerance; the budget is not an exact maximum.
Tag extraction and map rendering have caches.
Source: [repomap.py, ranking and budget selection](https://github.com/Aider-AI/aider/blob/5dc9490bb35f9729ef2c95d00a19ccd30c26339c/aider/repomap.py#L365-L708).

Transfer: offer a compact symbol map when an agent needs repository orientation.
Keep exact paths and source revision, and allow focused reads of full definitions.
Do not inject the whole map into every turn or implement a semantic index by default.
Aider contains hand-tuned weights; those values do not prove an optimum for Studio.
Compare map tokens plus follow-up reads against ordinary search on the same unfamiliar-repository tasks.

## 3. pi: preserve recent context and account for summary calls

The inspected compaction code reserves 16,384 tokens and retains about 20,000 recent tokens by default.
It uses provider usage where available, with estimates for later messages.
It excludes tool-result entries as cut points.
A split turn can require separate history and turn-prefix summaries.
The returned result includes summary usage, retained history, and read/modified file lists.
Source: [compaction.ts, defaults, cut points, and summary assembly](https://github.com/badlogic/pi-mono/blob/71dca871bc80b6bc97be37f0ca3189399d651fff/packages/agent/src/harness/compaction/compaction.ts#L157-L815).

Transfer: record compaction overhead and verify that outstanding work, tool identities, and file references survive.
A lower post-compaction input count alone is insufficient evidence of a cheaper accepted result.
Do not import these defaults or add a second summary layer around native Codex compaction without evidence.

## 4. OpenHands: distinguish hard limits from soft triggers

Inspected OpenHands software-agent-sdk commit `76e9e25078ed0ff7970f2c75e451274d3ed32bf2`.
`LLMSummarizingCondenser` distinguishes token pressure from a soft event-count trigger.
It preserves an initial prefix and recent suffix, then uses valid manipulation boundaries to select the middle.
It rejects condensation when too few events would be removed.
Its summary model can differ from the model used for token counts.
Source: [llm_summarizing_condenser.py, configuration and selection](https://github.com/OpenHands/software-agent-sdk/blob/76e9e25078ed0ff7970f2c75e451274d3ed32bf2/openhands-sdk/openhands/sdk/context/condenser/llm_summarizing_condenser.py#L38-L398).

Transfer: log why compaction occurred and whether it produced useful progress.
Avoid repeated summaries triggered only by event counts while useful context still fits.
The event-reduction guard does not prove a token reduction or adequate recall.
Studio does not own the native Codex history transform, so this first becomes an observability experiment.

## 5. Anthropic: retrieve detail when required

The article recommends lightweight references such as paths and queries, with detail retrieved when needed.
It also recommends a minimal, unambiguous tool set and prompt changes guided by observed failures.
It explicitly distinguishes minimal information from merely short instructions.
Source: [Effective context engineering for AI agents, published 2025-09-29](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents).

Transfer: keep compact durable facts and evidence references in task state.
Use full logs, historical complaints, and source bodies only when the next decision needs them.
Preserve safety boundaries and unresolved questions; do not shorten them solely to meet a token target.
Use a focused worker brief instead of copying unrelated history.
A smaller lead context does not establish smaller total team usage.

## 6. OpenAI: separate cache reuse from context reduction

Current documentation requires a matching rendered prefix at an eligible cache boundary.
Tool definition content and order, developer messages, and relevant settings can change that prefix.
Compaction can reduce subsequent cache reuse.
The guide also documents cache-write charges and minimum-length effects for supported models.
Source: [Prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching).

Transfer: keep unchanged instructions and catalogs stable, and append new state rather than rewrite earlier messages.
Report total input, cached input, output, and compaction usage separately.
Do not convert cache share into token reduction or a ChatGPT plan-credit saving.
Do not add padding or change cache controls based only on API pricing examples.

## 7. Codex: deferred dynamic tools are present, but gated

Inspected tag `rust-v0.154.0`, commit `6b9826e3aa83b1a5947db50f4332cb9c65f1b340`.
The canonical dynamic function schema has optional wire field `deferLoading`, represented internally as a Boolean with default `false`.
Namespaces contain functions; the flag belongs to each function, not the namespace itself.
Legacy input accepts an optional Boolean and otherwise derives it from the inverse of `exposeToContext`, defaulting to `false`.
Source: [protocol dynamic_tools.rs, lines 18 to 130](https://github.com/openai/codex/blob/6b9826e3aa83b1a5947db50f4332cb9c65f1b340/codex-rs/protocol/src/dynamic_tools.rs#L18-L130).

The handler converts the flag into deferred exposure and provides search metadata.
It clears the outgoing Responses marker because exposure controls discovery; search results restore that marker.
Source: [dynamic.rs, lines 50 to 106](https://github.com/openai/codex/blob/6b9826e3aa83b1a5947db50f4332cb9c65f1b340/codex-rs/core/src/tools/handlers/dynamic.rs#L50-L106).

The registry adds tool search only when a searchable deferred tool exists and the search gate passes.
The gate requires both `model_info.supports_search_tool` and the provider's `namespace_tools` capability.
Source: [spec_plan.rs, registration at 373 and gate at 629](https://github.com/openai/codex/blob/6b9826e3aa83b1a5947db50f4332cb9c65f1b340/codex-rs/core/src/tools/spec_plan.rs#L373-L642).

This updates the September 9 uncertainty: this Codex revision implements deferred dynamic tool discovery.
The root investigation also generated matching experimental schema from installed `codex-cli 0.154.0`.
An actual Studio session with deferred registration and successful discovery remains unverified.
Do not enable the flag for every model and provider based only on schema acceptance.

Studio registers dynamic tools in [`new_thread_params`](../../scripts/codex_runtime.py).
Test a small deferred catalog through discovery, invocation, resume, model switch, and unsupported-capability fallback.
Measure discovery calls and failures as well as reduced initial schema input.
Keep common tools directly available during the experiment.

## Evidence needed before a savings claim

1. Identify the actual model-input boundary and usage coverage.
2. Compare matched accepted tasks, not arbitrary equal-duration windows.
3. Include all team members and post-summary recovery calls.
4. Retain complete artifacts and test exact retrieval after interruption.
5. Check answer quality, missed constraints, retries, total tokens, and elapsed time together.

No foreign benchmark, source constant, serialized byte count, or reduced lead context establishes the expected Studio gain.
