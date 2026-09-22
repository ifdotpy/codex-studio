# Account selection and transfer

A new chat appears from its creation response before its snapshot arrives.
That response lacked `empty`, so the account menu incorrectly required transfer
confirmation. Creation and account selection responses now include the current
server-computed eligibility. The server still checks that the chat has no work
before changing its account. The UI applies the account response immediately.

Transfer workers now reserve each destination independently. A pending native
receipt still blocks another fork into the same destination history database.
A saved, validated fork result commits without another catalog read or the
10-second retry delay. Completion updates its operation and UI summary together.

The scheduler recovers a pending transfer referenced by `accountTransferId` when
its display summary points to a terminal operation. Conflicting active identities
remain blocked. Restart no longer changes terminal transfer timestamps.
Subagents keep their existing accounts.

## Verification

- Transfer contracts: 32 tests passed.
- Transfer settings contracts: 7 tests passed.
- Account disconnect contracts: 7 tests passed.
- Live patch contracts: 7 tests passed, including rollback and active-worker guards.
- The browser creation test selected another account while withholding snapshots.
  The choice used `/api/agents/account`, created no native thread, and sent no
  transfer request. Observed account response and UI click time: 270 ms in this fixture.
- The transfer browser test passed confirmation, progress, retry, and cancellation checks.
- The native Codex fixture preserved paginated context and tools through two
  account hops. It made no cloud model requests.
- The production web build, signed application package, and package test passed.
- Live update `2026-09-22-transfer-speed-v1` applied to backend PID 55850 without restart.
- A same-account no-op on an existing empty chat returned `empty: true` and no
  native thread in 450.3 ms on the live backend. No user account assignment changed.

Fixture times do not establish production transfer latency. Active turns, native
queues, tools, and unknown receipts still require their existing safety boundaries.
