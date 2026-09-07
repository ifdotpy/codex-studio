# Official OpenAI sources: token efficiency for Codex app-server harnesses

Date: 2026-09-07

Scope: official OpenAI documentation checked on this date. The API guidance
below describes the Responses API. The app-server guidance describes the Codex
JSON-RPC host protocol. It does not prove that app-server exposes every
Responses API field.

## Findings

### Keep the stable prefix stable

- [Prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching)
  treats the rendered prefix as one match. It includes instructions, developer
  messages, tool definitions, and conversation history.
- A change before the cache breakpoint prevents reuse after that point. The
  documented prefix inputs include the model, tool names, descriptions, schemas
  and order, `parallel_tool_calls`, `text.format`, `reasoning.effort`,
  `text.verbosity`, and `context_management`.
- Put stable instructions, shared references, and stable tool definitions first.
  Put user-specific and time-varying data later. Append new content instead of
  rewriting earlier content.
- `prompt_cache_key` influences routing. It does not guarantee a cache hit.
  Measure `usage.input_tokens_details.cached_tokens` and cache-write tokens.
- Cache rules are model-specific. Current GPT-5.6 guidance documents explicit
  breakpoints, a 30 minute TTL, a 1,024 token minimum, and separate cache-read
  and cache-write accounting. Confirm these rules for the selected model.

### Reduce tool metadata before changing cache structure

- [Tool search](https://developers.openai.com/api/docs/guides/tools-tool-search)
  defers function schemas with `defer_loading:true` and loads only the tools
  that match the request. The docs describe hosted and client-executed modes.
- Tool search is documented for `gpt-5.4` and later. The docs recommend
  namespaces or MCP servers, clear high-level descriptions, and fewer than ten
  functions per namespace as a starting point.
- Discovered tools are injected at the end of the context. This can reduce the
  initial tool-definition payload and preserve the earlier cached prefix.

This applies to the Responses API. The app-server page does not state that its
JSON-RPC methods support `tool_search` or `defer_loading`. Verify the actual
Codex implementation before adding these fields.

### Compact only when context growth requires it

- [Compaction](https://developers.openai.com/api/docs/guides/compaction) can
  reduce a long context while preserving state. Server-side compaction uses
  `context_management` and `compact_threshold`. A standalone
  `/responses/compact` call is also documented.
- A compaction item is encrypted and opaque. The standalone endpoint returns a
  new compacted window. Pass that output as-is; do not prune it by hand.
- The input to standalone compaction must already fit the model context window.
  Compaction changes the rendered context. It can therefore change the first
  differing token and reset later cache reuse. This is a reason to compact at a
  deliberate threshold, not evidence that every turn needs compaction.

The page examples use current reasoning models, including GPT-5.3-Codex. Exact
model support and thresholds remain model and API-version dependent.

### Separate reasoning tokens from visible output

- [Reasoning models](https://developers.openai.com/api/docs/guides/reasoning)
  count hidden reasoning tokens as output. They occupy context and are billed
  as output, although the API does not return their text.
- `max_output_tokens` caps reasoning tokens, visible output, and other generated
  tokens together. A low cap can return `status: incomplete` before visible
  text, while input and reasoning work still incur cost.
- Inspect `usage.output_tokens_details.reasoning_tokens`. Set
  `reasoning.effort` from quality and latency tests. Preserve the selected
  model's supported effort levels and the user's defaults.
- Set `text.verbosity` for visible answer length. Use explicit word, section,
  table, or JSON limits when the product needs a small answer. Verbosity does
  not cap hidden reasoning; `max_output_tokens` covers both.

The [Responses API reference](https://developers.openai.com/api/reference/cli/resources/responses/methods/create)
documents `max_output_tokens`, `text.verbosity`, `prompt_cache_key`, cache
options, `truncation`, and usage fields. These are direct API controls, not
documented app-server controls.

### Previous response state reduces transport work, not billing

The [conversation state guide](https://developers.openai.com/api/docs/guides/conversation-state)
documents `previous_response_id`. It reduces the client payload for a follow-up,
but previous input tokens are still billed as input. Prompt-cache reuse and
conversation state are separate mechanisms. Use the token-counting endpoint in
[Counting tokens](https://developers.openai.com/api/docs/guides/token-counting)
when exact input size matters. Local tokenizers do not account for all tool,
file, image, formatting, cache, and model-specific reasoning behavior.

## App-server versus direct Responses API

The [Codex app-server guide](https://learn.chatgpt.com/docs/app-server) documents
an experimental JSON-RPC host protocol with thread, turn, model, account, and
approval methods. It documents supported reasoning efforts and
`thread/tokenUsage/updated` notifications, plus ChatGPT or API-key auth, but
it does not document `prompt_cache_key`, raw Responses `usage`,
`max_output_tokens`, `tool_search`, or Responses compaction fields as app-server
protocol controls.

The authentication and accounting paths differ:

| Path | Documented account and cost surface | Token controls in the cited docs |
| --- | --- | --- |
| App-server with ChatGPT auth | [Codex pricing](https://learn.chatgpt.com/docs/pricing) says ChatGPT Work and Codex share plan usage, credits, and limits. App-server exposes `account/rateLimits/read` and `account/usage/read`. | Reasoning effort and token-usage notifications are available. Raw Responses cache controls are not established by this contract. Do not infer cache reuse from plan usage alone. |
| App-server with an API key | The same pricing page says local Codex API-key use follows API token pricing and does not use ChatGPT multipliers. | The app-server docs still do not define the underlying Responses request fields. Inspect the implementation or provider logs. |
| Direct Responses API | [API pricing](https://developers.openai.com/api/docs/pricing) and the Responses reference define input, cached input, cache-write, output, and reasoning accounting. | The caller can use prompt-cache options, tool search where supported, compaction, `reasoning.effort`, `text.verbosity`, `max_output_tokens`, `previous_response_id`, and token counting. |

The app-server protocol can hide provider details. “Not documented” means that
the cited public contract does not expose the field. It does not prove that a
particular Codex build lacks internal caching or compaction.

## Implications for the current harness audit

- Prioritize large payloads and repeated event data before broad cache-break
  changes. A stable prefix cannot make a duplicated multi-megabyte tool result
  cheap to serialize, display, or transfer.
- Keep required time awareness, panel screenshots, and Luna Max defaults. Place
  changing time data after stable instructions when the request path permits.
  Do not remove time data only to chase an undocumented app-server cache hit.
- Treat workspace tool schemas, status tails, peer data, and repeated `items`
  or `tasks` arrays as payload-shaping issues. Tool search is a possible direct
  Responses API design, not an app-server assumption.
- Use raw Responses usage only when the harness actually receives it. For
  ChatGPT-authenticated app-server runs, use observed token-usage notifications
  and supported rollout records. Studio already imports response-ID usage with
  cached-input counters. Those support the measured cache share in the companion
  audit. Account limits and local bytes alone do not establish cache reuse.

## Limits of these sources

- OpenAI docs change. Recheck model support, thresholds, prices, and auth modes
  before implementation or release decisions.
- These sources do not define Codex desktop or app-server internal prompt
  assembly. They cannot prove which local messages, tools, screenshots, or
  status events reach the model.
- The documented Responses API usage fields cannot be assumed to be present in
  ChatGPT-managed Codex traffic.
