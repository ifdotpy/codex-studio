# Remote spawn failure on igor-mbp

## Evidence (2026-10-08)

Base: `f5104ac79666d17ed4fc3ae39270349f0f1ffd56`.
The inspection uses SQLite read-only connections on both live servers.
The reproduction uses an isolated remote database copy and no scheduler.
No live database, backend, Serve setting, or pair record changes.

| Item               | Value                                  |
| ------------------ | -------------------------------------- |
| Home server        | macbook-pro-lumina                     |
| Remote server      | igor-mbp                               |
| Remote server ID   | `aac03447e0c7474ba6794e483a060f1b`     |
| Home lead          | `5db69b8d-b9d2-4d71-8cee-4d295df7d6ae` |
| Spawn request      | `b77bc02f-7951-5af2-8a7e-6d0c070613f4` |
| Remote-parent link | `2b35ff16-9580-592e-b7b9-5db7d0fb73e2` |
| Worker             | `c50824b4-563b-5a11-9415-6eaf13d8285d` |
| Remote anchor      | `81f70dbd-fbb1-57fa-9dc9-b1285adc2c09` |

At 13:25 UTC, the home outbox has 16 attempts and no result.
The remote inbox has state `running` and no result.
The remote database contains one agent, the idle anchor.

## Exception and cause

The exact envelope reproduces this exception at 13:27 UTC:

```text
_remote_spawn (codex_multi_server_orchestration.py:895)
  spawn_agents (codex_runtime.py:8087)
    resolve (codex_worker_accounts.py:55)
      catalog (codex_runtime.py:10540)
        runtime_catalog (codex_catalog.py:184)
          connect (codex_runtime.py:3565)
            executable_for (codex_native_runtime.py:437)
RuntimeError: No installed Codex version passed the protocol checks
```

The live `native-runtime/status.json` contains the same rejection.
Discovery selects `/opt/homebrew/lib/node_modules/@openai/codex/bin/codex.js`.
It rejects that launcher because `codex-code-mode-host` is absent beside it.
The actual Codex binary and helper exist in this directory:

```text
/opt/homebrew/lib/node_modules/@openai/codex/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin
```

Codex version: `0.161.0`.
The launcher starts the vendor binary, but Studio checks the launcher as a native binary.
A fake model catalog permits the spawn without this discovery step.

With the source fix, the same envelope succeeds through `receive()` in isolated remote state.
It commits one worker with status `queued` and an image workspace.
The same-ID retry returns the same `applied` receipt.
The scheduler is disabled, so this check does not send a model turn.

## Source changes

- Resolve a recognized npm launcher to its native platform bundle.
- Support nested dependencies, hoisted dependencies, and the older bundled vendor directory.
- Keep native companion, schema, and smoke checks before approval.
- Write unknown-outcome diagnostics to `orchestration-errors.log`, with owner-only access and a size limit.
- Record the request ID, action, exception type, safe message, and traceback frame locations.
- Omit source text, local variables, prompts, and credentials from diagnostics.
- Redact arbitrary exception messages. Preserve the exact fixed native failure message above.
- Close an unknown spawn after eight attempts, with one notice to the lead.
- Keep the receipt as `unknown`. Do not replay the spawn.
- Change an unconfirmed proxy from `starting` to `paused`, with the request ID and error.
- Release its local reservation. Keep its task claim until an operator checks the remote worker.
- Preserve a worker state and slot when the reverse channel already confirms that worker.

## Proposed recovery, not executed

Use these steps only after the fixed product is available through the normal update process.

1. Inspect the old worker ID and spawn receipt on igor-mbp again.
2. Check `runtime_agents`, `runtime_tool_results`, and the initial `runtime_events` for that worker.
3. If any worker effect exists, inspect that worker instead of creating another worker.
4. On the home server, let the next same-ID delivery close the old outbox request as terminal `unknown`.
5. Verify that the proxy is `paused`, its reservation is clear, and the lead has one error notice.
6. Keep the home receipt and remote inbox receipt. Never delete or reset their request identities.
7. If no worker effect exists, remove the unused remote anchor through `POST /api/conversation/delete`.
8. Keep the remote inbox and link records. The old `running` receipt prevents a second spawn with that ID.
9. Remove the old home proxy through the normal conversation API after the same absence check.
10. Create a replacement only after those checks, with one new stable request ID.

The remote `running` receipt is a quarantine record, not an active retry queue.
Closing the home outbox stops requests on both servers without reopening the remote effect.
The old request must continue to return `unknown`.
Do not edit either live database for this recovery.
