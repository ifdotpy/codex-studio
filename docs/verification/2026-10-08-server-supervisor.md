# Supervisor service verification, 2026-10-08

Source: `feat/server-supervisor`, based on `origin/main` at `2158bc12`.
The test servers had `f4ca0ef2` installed. The new command and helper were copied
to their existing resources. Their state directories stayed unchanged.
This computer's live backend was not changed.

## Live checks

Both servers had zero active agents, monitors, terminals, server commands, and
pending events before the first cutover. The command installed supervisor mode
without a desktop window.

| Server                              | First cutover backend PID | Planned restart backend PID | Supervisor PID | Test child PID |
| ----------------------------------- | ------------------------- | --------------------------- | -------------- | -------------- |
| `igor-mbp`, macOS, port 4620        | 28293 to 29215            | 29215 to 29694              | 29207          | 29619          |
| `kukuka-win`, WSL Ubuntu, port 4720 | 7629 to 9064              | 9064 to 9489                | 8821           | 9476           |

On macOS, an HTTP terminal request started `/bin/sleep 120` through its native
executor (PID 29616). The existing backend restart script restarted the backend.
At 19:11:18 UTC, the same supervisor, executor, and sleep child were alive.
The backend restored the terminal and attached to the same native executor.
The test closed its terminal through the HTTP API.

On WSL, a supervisor proxy started `/bin/sleep 120`.
`systemctl --user restart codex-studio` restarted only the backend.
At 19:09:28 UTC, the supervisor and sleep child kept their process identities.
The proxy attached to the existing child. The test stopped its child and retired
its handle.

The final helper was copied again before the last check. At 19:17:41 UTC on
macOS and 19:17:42 UTC on WSL, a repeat `enable` preserved backend PIDs 29694 and 9489. Both `/api/desktop` responses reported `supervisorMode=true` and supervisor
protocol 1. All activity counts were zero. Both command links existed in
`~/.local/bin/codex-supervisor`.

The first Linux attempt failed because the unit quoted `WorkingDirectory`.
The old backend stayed alive. The command now uses an unquoted absolute path
and validates both units with `systemd-analyze` before it writes them.

## Source checks

| Check                                    | Result          |
| ---------------------------------------- | --------------- |
| Supervisor service tests                 | 12 passed       |
| CLI installation contract                | 3 passed        |
| Process supervisor contract              | 61 passed       |
| Desktop recovery tests                   | 14 passed       |
| Strict mypy for the new helper and tests | Passed, 2 files |
| `npm run api:check`                      | Passed          |

The full runtime mypy check reported 29 errors in six existing files:
`codex_cache_paths.py`, `codex_executables.py`, `codex_private_paths.py`,
`codex_file_lock.py`, `codex_state.py`, and `codex_python.py`.
The lead assigned those errors to the Windows server task. This change does not
modify those files.

## Evidence limits

The live checks cover planned backend restarts, not host reboots or supervisor
crashes. They do not verify the paired-server exec tool after the cutover.
The macOS supervisor refused a standalone proxy while the backend owned its
attachment. The macOS check used the real terminal API instead.
The initial cutover requires an idle boundary and no new work during setup.
