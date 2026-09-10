# Account transfer verification

Scope: transfer a managed team between native account profiles while retaining
its chat, agent IDs, relationships, queued messages, work, panel, and context.
Baseline: `38dd383`. Native binary: `codex-cli 0.153.4`.

## Results

- `tests/account-transfer-contract.py`: 14 cases passed. Covers native idle and
  unloaded states, active turns, native background commands, uncertain message
  delivery, late fork receipts, cancellation, restart, new workers, queue
  isolation, failed-turn continuation, explicit pause, and history conflicts.
- `tests/runtime-accounts-contract.py`: 11 existing cases passed.
- `tests/account-transfer-native.py`: two transfers passed through the installed
  Codex binary, between isolated local profiles. Both original context markers
  and dynamic tool definitions reach the final Responses request. The source
  uses paginated history; the return transfer includes native fork ancestry.
- The native test with `STUDIO_TRANSFER_COST_SCANNER` reports 15 tokens on each
  account after one model response on each. Imported context does not become
  usage on the destination. All model requests use a loopback fixture.
- `tests/account-transfer-ui.mjs`: production React build passed. Covers the
  existing-chat account menu, team progress, cancellation, persisted progress
  after reload, retry, and the destination identity. Desktop and mobile layout
  load without JavaScript errors.
- Main-checkout TypeScript and production builds passed. The served bundle was
  checked through port 4620. The route rejects a missing token with HTTP 403
  and an unknown account with HTTP 400.

## Installation

The Python runtime was updated through exact bytecode guards. Existing native
connections remained unchanged for all four accounts. Original bound methods
and their globals were preserved; no Runtime class or app-server was replaced.
The interface was built from the current main checkout, preserving its unrelated
changes. No owner conversation was transferred as part of installation.

The running checkout also contains a newer account-specific cost scanner than
this branch's baseline. Its scoped integration change is recorded in
[the cost scanner patch](2026-09-10-account-transfer-cost-scanner.patch).
The patch was applied to that checkout and its helper was rebuilt. It lets
ancestor lookup read `.studio-imports` while billable-file discovery continues
to exclude that directory. The native optional-cost test verifies the resulting
binary, not a source-text assertion. This patch does not include the scanner's
separate source import or claim that import belongs to this change.

## Boundaries

- Running turns, native background commands, and managed monitors finish before
  their agent moves. Other team members can move first. An indefinite watch can
  keep its member waiting; the menu shows the wait.
- Completed work and manual pauses remain idle. A confirmed failed turn resumes
  from saved context without replaying the previous input or command.
- Unknown native mutations retain their receipt and original session. They are
  not repeated automatically. A process restart cannot recover a lost native
  fork ID without further evidence; cancellation keeps the original agent.
- Canonical JSONL histories, including paginated ancestry, are verified.
  Unsupported compressed or conflicting histories fail without overwriting data.
- These checks do not establish shared prompt caching, transferred account-wide
  memories, or live cloud inference after an owner-account transfer.
