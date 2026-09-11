# Messages and role skills

## Result

Messages replaces Inbox, Agent chats, and the Complaint book screen.
The order is For you, To orchestrator, Team broadcast, and Between agents.
The selected conversation and its draft remain open behind the panel.
Replies retain their request identity after a lost response and panel closure.
Only the orchestrator sends conversational messages or tasks to the user.
Subagents send requests to the orchestrator, which decides whether to contact the user.
Native tool permissions still require the actual user approval.

Canvas and its controls are removed. Team retains worker status and chat links.
Plan displays native steps and explanation. Changes go through the agent chat.
Historical saved plan data remains stored but is excluded from model context.

## Harness

The server selects codex-orchestrator or codex-subagent from isLead.
Native thread instructions contain the selected SKILL.md.
Versioned turn context supplies it to existing threads, after changes, and after compaction.
Only confirmed event delivery establishes the known skill version.
The package includes both role skills and the shared workspace skill.

Worker schemas omit user-task and speech tools. Their server checks still apply
when an older thread uses a retained schema or workspace fallback.
Direct native worker questions are rejected. Structured worker questions become
messages to the orchestrator with their complete content and original identity.
Saved answer receipts and uncertain deliveries retain their previous meaning.

User-task history retains its original owner. The orchestrator reviews legacy worker tasks.
Only pending, unsent completion events change recipient. Their IDs remain unchanged.
Uncertain events are not replayed.

## Checks

The integrated source passes:

- Web production build.
- Role skills: 8 tests.
- Runtime: 49 tests.
- User tasks: 17 tests.
- Model context: 14 tests.
- Guarded model-view update: 3 tests.
- Messages browser test, including HTTP response loss and one stored reply.
- Agent conversation scope browser test.

The isolated source also passes the question history, complaint routing,
preparation, product UI, workspace UI, team navigation, and user-task UI checks.
Messages browser checks cover widths of 320, 390, 1280, and 1920 pixels.
The package test uses a hidden Electron window and verifies packaged role skills.
The backend survives the test window closure.

All state used by these checks is temporary. No test calls a live model.
The existing user backend and its active work remain untouched.
Backend changes take effect after a normal server restart.
