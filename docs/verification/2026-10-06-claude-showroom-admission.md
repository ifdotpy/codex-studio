# Claude input admission and showroom recovery

## Reported failure

The `Тикет showroom#388` chat failed before Claude received its input. Studio
showed an account-change error, then a preparation timeout. Both failed turns
had only a user message but saved `startOutcome: accepted`.

The absence of assistant output does not prove that input was not submitted.
The old receipts remain unchanged. Recovery did not replay those messages.

## Confirmed defects

- Account validation failed before SDK input admission, but Studio saved an
  accepted outcome. The input recovery path therefore could not preserve the
  unsent message for a safe retry.
- The running bridge had older code than its installed file. Both versions
  reported capability 15, so the idle replacement check retained the old process.
- A hashed native message ID alone cannot identify the original input. A queued
  steer can use that UUID as its literal client ID. The rejection proof now also
  checks an immutable, non-enumerable original client ID.

## Change

`finishTurn` saves `not_applied` only when a structured preparation failure
matches the exact message still present in the input queue. The queue removes
an input before it yields that input to the SDK. Errors after admission retain
an accepted outcome.

Account failures preserve the original pending input and hold automatic retries.
An explicit continuation can retry that input with its original identity.
Preparation timeouts retain the existing single retry limit. Account identity,
provider, subscription, and API-key checks remain in effect.

The on-disk bridge reports capability 16. Replacement still requires proof that
the old bridge has no active agents, callbacks, requests, or background work.

## Live verification

- Backend PID 6414 and the existing state directory remained in use.
- Claude bridge PID 16753 retained its active sessions. Three idle functions
  changed in place after a source check and a V8 dry run: `checkAccount`,
  `finishTurn`, and `userMessage`. The active session generator did not change.
- The live edit response was lost. A separate source read confirmed the edit;
  the mutation was not repeated. The debugger listener then closed.
- Turn `eb652bd3-374c-45a3-aad7-9d8d1df84466` completed at
  `2026-10-06T10:20:56Z` with a real Claude answer and the existing chat history.
- Request `studio-showroom-resume-last-task-20261006` returned the chat to its
  original Wiki task. Turn `0fd056fe-822a-4422-9eb0-0793ada10df7` started
  successfully and used a tool without a preparation error.

Installed bridge SHA-256:
`6144eff4a4ef3cf08f280c23b6c63b67610716f2025d76a1042d9bc4ef82fa41`.

Live source SHA-256:
`e38a4a61c29c684d9c982715b72dc823da0a0879b913722ee3366ce3d131ee6f`.
The live source retains startup capability 15. The installed file uses 16 for
the next safe idle replacement.

## Checks

The auth admission, preparation receipt, Python input recovery, and idle bridge
control contracts pass. Negative cases cover post-admission errors, a distinct
queued steer, and a queued steer whose literal ID aliases the original native
UUID. Independent review passes after the alias correction.

The merge with `origin/main` preserves both image workspace cleanup and the
incoming agent-room synchronization. The image workspace and scoped agent
contracts pass. Staged-content lint and format checks pass.

## Remaining uncertainty

The original generic account error did not record which metadata field failed.
Its exact cause cannot be recovered from that receipt. Current probes through
the same account, resumed session, settings, and MCP tools return valid account
metadata. The fix preserves precise reasons for future account failures.

Fresh SDK preparation probes pass. They do not explain every earlier preparation
delay. The verified defects are the false admission receipt and the stale bridge
replacement check.
