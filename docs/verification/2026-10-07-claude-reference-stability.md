# Claude reference: session and process reliability

## Scope

The code review covers session storage, tool cancellation, uncertain input recovery, and process output after exit.
The Studio base commit is `ab5b0a0e5428f8804cf0c9856f46880e2715c2ed`.

The reference is the installed Claude CLI 2.1.291 for macOS ARM64.
Its executable SHA-256 is `9a1d2ed6bb4421e8fc80c892c0413f293be3ee50ae3d7dda1a7622197a056690`.
The extracted JavaScript is in `/Users/igor/Projects/claude-cli-extracted/2.1.291/readable`.
The comparison identifies mechanisms. It does not measure equal failure rates across the two products.

## Comparison and defects

### Session storage

The reference tracks partial transcript tails and repairs their boundary before a later append.
The relevant methods are `sealTornTailOnNextAppend`, `sealTornTailSync`, and `gSe` in `chunk-v1gtm86q.js`.
Their extracted locations start at lines 188320 and 190011.
Studio uses a snapshot and a sequence journal instead of the same transcript format.

The Studio failure cases were:

- A partial append followed by another append created an invalid JSON record.
- A metadata rename failure left the previous preview after an unchanged retry.
- Tail repair used a UTF-16 character offset as a file byte offset. Cyrillic text and emoji exposed the error.
- A failed rollback could survive a later persist call with no session changes.
- An error after successful journal clearance left an obsolete length for a later rollback.

The change uses byte offsets, repairs a failed append before another persist can succeed, and retries metadata writes.
After a compaction error, the store reads the journal state before it uses a rollback offset.
Failed writes still reject their promises. The change adds no file synchronization policy for power loss.

Files: `scripts/claude_bridge/session-store.mjs` and `scripts/claude_bridge/session-store.test.mjs`.

### Tool cancellation

The reference scopes control cancellation to the request identity.
See `chunk-k7j3g78v.js` at lines 2673 and 3615.
Studio already has a request cancellation function, but its SDK tool handler did not pass the abort signal.
An interrupt returned while an unanswered Studio tool call kept the turn open.

The handler now passes `extra.signal` to the existing request function.
Tests cover a pending call, an already aborted signal, and a late response after interruption.
A separate offline check used Claude Agent SDK 0.3.285 and MCP SDK 1.30.0 with an in-memory client.
The actual SDK handler received an AbortSignal and observed its cancellation.
This does not prove that an external tool side effect stops after cancellation.

Files: `scripts/claude_bridge/bridge.mjs` and `tests/claude-bridge-contract.py`.

### Uncertain input recovery

The reference's exact request identity rule informs the comparison. Studio adds its own durable receipts and paginated history checks.
Studio previously accepted the first matching turn without checking subsequent history pages.
A second turn with the same input identity therefore did not prevent recovery.
An empty page cursor could also authorize restoration of uncertain input after a child replacement.

Recovery now requires complete history before it accepts one matching receipt.
It rejects distinct or changed duplicate receipts, invalid page data, invalid cursors, and cursor cycles.
The existing 20-second deadline remains. Missing or null final cursors remain valid.
The change does not repeat an uncertain native request.

Files: `scripts/codex_turn_recovery.py` and `tests/turn-recovery-contract.py`.

### Process output after exit

The reference consumes child output and settles its callback on `close` in `chunk-h6hf13d1.js` at lines 36488 to 36490.
Another path waits for both `stdout.text()` and `exited` at line 36832.
Studio previously treated an observed process exit as terminal even while a reader still wrote its final journal record.

The supervisor now samples reader state before it queries the journal.
The proxy consumes records after the exit record until the journal is empty and both readers have stopped.
Tests cover a delayed write from a real child process, the empty-query race, and stderr after the exit record.
Tests also cover a missing exit record and compatibility with responses from older supervisors.
Older supervisors without the new reader fields retain their previous behavior.

Files: `scripts/codex_process_supervisor.py` and `tests/process-supervisor-contract.py`.

## Review and checks

Lead review requested extra cases for rollback failure, compaction errors, and the order of reader and journal observations.
The workers added those cases before the independent review.
The independent review found no concrete P1 or P2 defect in the four changes.

The combined integration check passed all 237 tests on 7 October 2026:

- Bridge unit suite: 37 tests.
- Bridge process contract: 44 tests.
- Turn recovery: 38 tests.
- Claude input recovery: 33 tests.
- Connection recovery: 23 tests.
- Restart recovery: 62 tests.
- Targeted JavaScript lint and format checks.
- Git hook checks against staged-content fixtures.
- `git diff --check`.

The lead's first full supervisor run had one failure and one cleanup error across 59 tests.
The affected cases are `test_runtime_restart_reattaches_turn_and_replays_buffered_events_once` and `test_adjacent_deltas_keep_each_journal_receipt`.
Both failures were fixture defects. The restart fixture now consumes its independent monitor notice before it asserts the original turn status.
The delta fixture verifies and removes its `None` server placeholders before cleanup.
All original receipt, output, and no-replay assertions remain. The worker reports 59 supervisor tests passed.
The final integrated run passed all 59 supervisor tests and all 6 response-map tests.
The total is 302 tests across the selected suites.
Python 3.14 emitted unclosed SQLite connection ResourceWarnings in the supervisor suite. The suite returned exit code 0.
Those warnings were not resolved by this change.

## Limits

All process tests use isolated state and fixture providers. No paid model request was sent.
The checks do not establish power-loss durability or statistical reliability parity with Claude CLI.
The running application was not restarted or patched during this work.
Supervisor deployment requires its documented idle transition in `docs/restart-recovery.md`.
EOF investigation of the Claude bridge found no demonstrated input replay defect, so that path remains unchanged.
