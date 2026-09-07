# Native error coverage, Codex 0.153.4

## Scope and sources

Studio must display native failures and recovery states without replaying commands,
changing account scope, or treating an acknowledgement timeout as a rejection.
The real callers are app-server notifications, responses, and server requests.
Tests inject those messages through Runtime and through the pipe reader.

Installed CLI: `codex-cli 0.153.4`.
Source: `openai/codex`, tag `rust-v0.153.4`, commit
`042fb41b7c813ac7999105e886b2b7aa715b5081`.
The existing checkout is older; this review uses `git show` at the exact tag.
The installed CLI's generated schema independently confirms 18 error variants.

Native source anchors under `codex-rs/`:

- `app-server-protocol/src/protocol/v2/shared.rs`: `CodexErrorInfo`.
- `app-server-protocol/src/protocol/v2/notification.rs`: errors, warnings, authentication recovery, request resolution.
- `app-server-protocol/src/protocol/v2/thread_data.rs`: `TurnError` and public policy details.
- `tui/src/chatwidget/protocol.rs`: transient errors, terminal errors, warnings.
- `tui/src/chatwidget/turn_runtime.rs`: error-specific user guidance.
- `protocol/src/error.rs`: native retry classification.

The [app-server documentation](https://learn.chatgpt.com/docs/app-server)
defines the client/server boundary. The installed schema is the versioned wire reference.

## Coverage

| Boundary | Studio behavior | Evidence |
| --- | --- | --- |
| Native retries and provider failures | Native Codex retains its retry policy. `willRetry` changes the visible activity without completing the turn or submitting another request. Model progress clears the temporary state. | Native error contract and browser test |
| All 18 `CodexErrorInfo` variants | Preserve message, code, HTTP status, additional details, and policy details. Give error-specific guidance. Unknown variants retain their message and details. | Installed schema comparison and error contract |
| Terminal failure | Preserve the error in history. A missing terminal error uses the exact turn's earlier error. A failed turn cannot appear successful. | Error, runtime, recovery contracts |
| Pending work after terminal failure | Retain pending messages. Wait for an explicit user message or explicit resume before another turn. Do not stop workers or commands. | Error contract |
| Authentication recovery | Show restoration in progress and completion within the same turn. Do not choose another account. | Error contract and browser test |
| Warnings and configuration | Deduplicate thread warnings. Store startup warnings before any thread exists. Account notices stay scoped to the current account connection. | Error contract |
| MCP startup and sign-in failures | Preserve a visible warning. Account-level ready/success removes the corresponding active warning. | Error contract |
| Permission request resolved by Codex | Remove the pending request using account, connection, thread, and RPC identities. Do not infer approval or send another reply. | Error and account contracts |
| Unsupported native request | Return JSON-RPC `-32601`. Do not create an approval card that cannot be answered. | Error contract |
| Explicit JSON-RPC rejection | Preserve the error code and data in `NativeRpcError`; retain compatible serialized text. | Pipe-reader contract |
| Lost acknowledgements | Preserve exact futures and durable request receipts. Timeout remains unknown. Do not replay uncertain mutations. | Turn-start, prepare/steer, tool-request, harness-response contracts |
| Connection loss and stale lifecycle state | Existing account isolation, disconnect handling, and exact-turn recovery remain in effect. | Account, runtime, recovery contracts |
| Command, file change, and connected-tool failures | Existing native item status, error, exit code, and output remain visible. A command failure is not a successful tool call. | Runtime and monitor contracts |
| Presentation | Error details are collapsed. Long JSON stays within the screen and a bounded detail area. Notices do not become tool-call cards. | Desktop and mobile browser test |

## Limits of the claim

This establishes the public error boundary for Studio's supported native paths.
It does not establish complete equivalence with every Codex CLI feature or every
possible host failure.

- Provider retries, token refresh for normal native login, tool execution, and
  compaction remain native code. Tests do not force real provider outages or use
  paid model requests.
- External-token refresh callbacks and client attestation are not implemented by
  Studio. They now fail explicitly instead of waiting for an unusable approval.
  Native account profiles remain the supported authentication path.
- Windows sandbox installation and native realtime error screens are outside
  this macOS client's supported paths. Studio voice uses its separate service.
- The previous SQLite I/O incident is not closed by this change. Lifecycle
  recovery can reconcile a terminal turn. It does not prove that every callback
  or transient output can be reconstructed after arbitrary storage failure.
- Existing live pipe-reader frames retain their original exception class during
  an in-process update. Their JSON error payload still reaches the same formatter.
  New source starts use `NativeRpcError`.

## Checks

Run from the repository with Python 3.14 and Node:

```sh
python3.14 -B tests/native-error-contract.py
python3.14 -B tests/native-error-update-contract.py
node --experimental-strip-types tests/native-error-schema.mjs
python3.14 -B tests/protocol-reader-contract.py
python3.14 -B tests/runtime-contract.py
python3.14 -B tests/turn-start-contract.py
python3.14 -B tests/turn-recovery-contract.py
python3.14 -B tests/runtime-accounts-contract.py
python3.14 -B tests/prepare-steer-contract.py
python3.14 -B tests/harness-response-contract.py
python3.14 -B tests/tool-request-contract.py
python3.14 -B tests/monitor-lifecycle-contract.py
npm --prefix web run build
node tests/native-error-ui.mjs
```

The schema check starts only schema generation, not a model session.
The browser test uses an isolated state directory and a hidden browser.

Verified in this change: 174 backend tests, all 18 installed schema variants,
the TypeScript/build check, and the desktop/mobile native-error browser test.
The watchdog installer fixture now reconstructs the exact historical dispatcher,
including removal of the later failure-hold condition. Its original hash guard
and unknown-version rejection remain unchanged.

Live activation remains separate from these test results. The runtime updater
checks six exact method hashes before replacing any method. It preserves native
connections, pools, pending futures, and current turn identities.
