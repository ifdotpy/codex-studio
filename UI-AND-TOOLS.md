# Chat interface and Codex tool check

Owner requirements, 2026-09-05: create a chat with one click, generate its title
from the task, distinguish leads in the database, and allow only Astra or Sol
as a lead. The canvas is a secondary view for a team of dozens of agents.

## Interface decisions

References inspected:

- [ChatGPT canvas](https://openai.com/index/introducing-canvas/): a separate work surface beside the conversation.
- [Claude Projects](https://www.anthropic.com/news/projects): conversations share a project; artifacts occupy a separate panel.
- [Open WebUI chat features](https://docs.openwebui.com/features/chat-conversations/chat-features/): a model selector, formatted responses, code blocks, and conversation controls.

Applied decisions:

- The left sidebar lists explicit lead conversations. It does not infer the role from missing parent links.
- The center contains one conversation and one composer. The first message is the task.
- The right panel contains workers and command results. On narrow screens it opens with **Team**.
- There is no lead creation form. There are no name, task, effort, capacity, or token-budget fields in the chat interface.
- The server uses the configured defaults. Capacity and budgets remain available in the command-line controller.
- Markdown uses Marked and DOMPurify. Raw HTML cannot add forms, scripts, event handlers, or remote tracking images.
- Tool activity stays in expandable message groups. Queued user messages remain visible before the model starts its next turn.
- The canvas has one control, **Fit**. Nodes support drag, keyboard activation, and a click to open the conversation.
- Explicit parent edges replace manual graph wiring. Legacy shared chats remain readable and writable through **Other sessions** and their existing CLI tools.

These are design choices, not a claim that this client has every feature of the reference products.

## Tool evidence

`tests/tool-parity.py` starts the installed Codex 0.153.4 against a local Responses
fixture. It captures actual native and managed tool manifests, including tools
inside code mode. This check makes no model inference call.

The comparison retains every non-collaboration tool in that fixture. Six native
collaboration tools are replaced by the managed dispatcher. It adds spawn, follow-up,
status, interrupt, monitor, monitor cancellation, and conversation title tools.

The check found that `features.multi_agent=false` alone leaves Astra's model-selected
v2 agents available. Managed threads now also set `features.multi_agent_v2=false`
and `agents.enabled=false`. This prevents native workers from bypassing the queue
and parent wake mechanism. The controls apply only to managed threads.

Source: [Codex 0.153.4 tool registry](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/core/src/tools/spec_plan.rs)
and [agent version selection](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/core/src/config/mod.rs).
Built-in tools, MCP tools, extension tools, and dynamic tools enter the same registry.
[App-server documentation](https://learn.chatgpt.com/docs/app-server) describes the dynamic tool and approval protocol.

A live Astra test generated the title `Orchestration runtime test`, completed two
reviewer tasks, received their results after the first turn ended, and received a
monitor exit event with code 0 and `MONITOR_OK`. All three events were delivered.

The same live thread listed 250 nested tools, including 228 MCP tools. Its list
included shell execution, patching, image inspection, web, image generation,
context tools, goal tools, resource reads, and plugin installation. Listing a tool
proves availability; this test did not execute external connector actions.
Direct code-mode, wait, clock, and user-input tools are separate from that nested list.

The client also answers `currentTime/read`, renders synchronous and asynchronous
questions, and routes command, file, permission, and MCP approval responses.
Device attestation and external authentication-token refresh remain specific to
the host. The client does not fabricate those responses.

Live evidence is in the task artifact directory:
`codex-runtime-live-h4kyxwca/result.json` and `tool-inventory-summary.json`.
The permanent tests reproduce the relevant checks without relying on that directory.

## Verification

- `tests/runtime-contract.py`: 24 runtime cases, including blank creation, model admission, role migration, async answers, 40-worker scheduling, monitor completion, cancellation, and parent wakes.
- `tests/canvas-contract.py`: 17 data and HTTP cases retained for legacy sessions, shared chats, and safe request handling.
- `tests/product-ui.mjs`: actual HTTP and SQLite fixture; lead filtering, 40 workers, Markdown safety, drafts, scoped canvas, one-click creation, retry identity, model guard, async answer, monitor, and stop.
- Headless Chrome: rendered at 1440 × 960 and 390 × 844; no horizontal page overflow.
- Live Codex: one Astra lead, two reviewers, one command monitor. No claim of a live 40-model load test.

The older UI tests for manual marquee selection, graph ports, and creation forms
were replaced because those controls were removed. Backend data tests remain.

Run `npm --prefix . ci` from `web/`. Run `npm --prefix . run vendor` after a pinned
Markdown dependency update. The committed vendor files let the Python server run
without Node.js or a frontend build on the user's machine.
