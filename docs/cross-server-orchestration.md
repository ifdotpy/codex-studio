# Cross-server orchestration

Pair the servers with the owner-pairing API before use. See the multi-server
credential contract. The channel uses signed requests through Tailscale Serve.
The lead cannot supply an address or credentials through an agent tool.

## Worker control

`orchestration_spawn` accepts `server` for the batch or each worker. Omit it to
use the lead's server. Use one server per batch. Give each remote worker an
absolute `cwd` on that server. The remote server uses its own repository,
accounts, provider catalog, and workspace adapter. The lead's model, reasoning,
Fast mode, and permission defaults apply unless the worker spec changes them.

Each remote worker has a local proxy with the same UUID. The remote server has
an explicit remote-parent link. Existing `orchestration_send`,
`orchestration_interrupt`, and messages use that link. The task board stays on
the lead's server. Workers submit results through `orchestration_task`. Review
decisions return to the worker. Worker directory reads and complaints also use
the home team. Native input still uses the destination's existing delivery path.
Remote input keeps its delivery mode. Remote input rejects attachments before
acceptance. An input cannot resume a worker after a concurrent stop changes its
epoch. Explicit parent resume rebinds the remote link to the parent's new epoch.
A resume that arrives before its stop or parent update waits for those operations.
Input for one worker follows queue order. A later input waits for the earlier
input receipt, including during an offline retry. Other workers can proceed.

The home server reserves the batch's execution slots before it sends the
request. A remote worker requires home admission before each subsequent turn.
An offline or unknown admission cannot start a native turn. Terminal native
state releases the slot. Remote proxies never start local model sessions.
A terminal snapshot releases its native slot after a parent resume, even when
the snapshot has the previous parent epoch. It cannot release a newer admission.

## Server tools

The lead can use `orchestration_servers` with these actions:

| Action     | Parameters                                             | Result                                |
| ---------- | ------------------------------------------------------ | ------------------------------------- |
| `list`     | None                                                   | Local identity and paired servers     |
| `projects` | `server`                                               | Registered remote projects            |
| `folders`  | `server`, absolute `cwd`                               | Up to 200 immediate subfolders        |
| `git`      | `server`, absolute `cwd`, `argv`                       | Exit code and up to 64 KiB of output  |
| `fetch`    | `server`, `agent_id`, `branch`, optional `destination` | Verified branch in local `FETCH_HEAD` |
| `receipt`  | `server`, `request_id`                                 | Saved remote request state and result |

Use a stable `request_id` for server operations. For `receipt`, this field
identifies the saved remote request. A queued response includes that identity.
`orchestration_request` also retains the native tool receipt.

The Git tool accepts complete forms: `status --porcelain=v1`, `branch --list`,
`rev-parse HEAD`, `rev-parse --show-toplevel`, `log -n COUNT` (1 to 50), and
`show --no-patch FULL_COMMIT_HASH`. It disables optional Git locks, hooks,
fsmonitor, and the pager. Other commands fail before execution.
The commands disable signature programs, external diffs, textconv, lazy object
fetch, and external Git transports. Local fetch rejects URL rewrites that match
the bundle path. Only the local file transport is permitted for fetch.
Before each command, Git reads the effective config keys. The read includes
repository config includes and worktree scope. An explicit allowlist permits
data keys. Known helper keys receive safe overrides. All other keys cause a
refusal that names the key. Config include paths and URL rewrites cause refusal.
Overrides use `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_n`, and `GIT_CONFIG_VALUE_n`.
The command never puts a config key into `-c key=value` text.
An unreadable config list prevents the command. Alternate-ref programs,
credential helpers, SSH programs, filters, and automatic maintenance are disabled.

Fetch uses a Git bundle through the paired channel. The server keeps the
export for the original request. Each chunk and the complete bundle have a
SHA256 checksum. The home server supplies known commits. The export excludes
those commits when they exist on the remote server. A commit already present
requires no bundle. The bundle has a 256 MiB limit. Local Git verifies it before
fetch. Fetch changes `FETCH_HEAD`; branch review and merge remain separate
operations. Task acceptance keeps a remote workspace for review on its server.
Chunk receipts store metadata, without the chunk data. A durable release request
deletes the export after fetch. The scheduler deletes exports older than 24 hours.

## Receipts and offline state

SQLite stores the outbound queue, inbound receipts, and remote-parent links.
Each envelope binds one request identity to its exact body and paired server.
A duplicate with changed content fails. A duplicate cannot create another
worker, message, stop generation, or task result.

The scheduler retries the same envelope after connection loss. It does not
repeat native model input. Source servers queue child results, messages, task
submissions, and stops until the destination returns. A late state or result
cannot reopen a stopped home team. A monotonic sequence rejects old snapshots.
Transport errors affect one envelope. Retry delays increase from 5 seconds to
300 seconds. After eight unknown input receipts or transport errors, the home
server closes that input with a terminal unknown receipt. It sends the lead a
notice with the request identity and `outcome unknown, inspect the worker`.
The home server never retries that input. Later input for the worker can then proceed.
Completed queue and inbox rows expire after 7 days. Compact identity
and fingerprint records prevent expired requests from executing again.

An unfinished inbound receipt stays unknown unless an exact committed effect
proves the result. Spawn, input, task, message, stop, and state receipts can use
that evidence. Missing evidence never permits another mutation. A crash before
a fetch receipt can leave its result unknown; inspect `FETCH_HEAD` before use.
A stop marker does not prove a native stop. The stop receipt stays unknown until
the worker has no active native turn or input.

## Checks

Run `python3 scripts/codex_python.py --exec tests/multi-server-orchestration-contract.py`.
The tests use two isolated SQLite runtimes and a fake paired transport. They
cover native tool calls, dropped replies, duplicates, offline queues, restart,
home concurrency, task review, messages, stops, access checks, and Git fetch.
These checks do not establish live Tailscale or live model behavior.
