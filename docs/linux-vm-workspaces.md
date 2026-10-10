# Layr VM chats

Chat creation accepts `workspaceMode`: `layr`, `image`, or `worktree`.
On macOS, `layr` is the default. Linux servers support `worktree` only.

| Mode       | Lead         | Workers        | Reviewers                 |
| ---------- | ------------ | -------------- | ------------------------- |
| `layr`     | VM main line | Owned VM lines | Read-only VM lines        |
| `image`    | Native macOS | ASIF images    | Read-only selected folder |
| `worktree` | Native host  | Git worktrees  | Read-only selected folder |

A chat stores `workspaceMode` and derives `executionMode` (`vm` or `native`).
Workers inherit the chat mode. Native workers can select another native workspace per spawn.
VM agents stay on layr lines in the VM. The old per-worker Linux environment setting is unsupported.

## Project lifetime

The first layr chat imports the Mac project once. Later chats reuse its VM main line.
The VM main line is the source of truth. The Mac folder stays separate for native chats.
There is no automatic sync. Use `layr export` only when the user requests a copy.
The read-only share exposes main and states on the Mac.

`Client.ensure_layr_project(project_id, source, owner=None, request_id=...)` resolves the project association.
The client preserves the initial upload and import identities after a lost reply.
`project.ensure` reads the association. `project.import` creates it once.
An existing association never replaces main with the current Mac folder.

## Guest boundary

The guest service runs as `studio` on vsock port 4050.
The root broker uses `/run/codex-studio/layr-admin.sock` and accepts only the service user.
The broker owns exact request digests and durable receipts.
The guest service stays non-root with `NoNewPrivileges=true`.

`AgentHandlers` exports `METHODS` and `READ_METHODS`.
Its constructor accepts the private broker state path and the layr root.
Its `dispatch(request_id, method, params, emit)` handles line and provider operations.
The broker exposes no generic root shell command.

The project owner is `studio-p-<sha256(projectId)[:16]>`.
Each worker has a separate Linux user and a private home.
The lead provider runs as the project owner in main.
Worker providers run as their line owners. Reviewer lines are read-only at the filesystem level.
Provider argv and profile paths are fixed before the root broker drops privilege.

| Method                  | Behavior                                                      |
| ----------------------- | ------------------------------------------------------------- |
| `line.bind`             | Bind a lead to main                                           |
| `line.branch`           | Create an owned worker or read-only reviewer line             |
| `agent.context`         | Read the stored project, line, cwd, owner, UID, GID and home  |
| `line.save`             | Save one turn-end state with the exact turn receipt           |
| `line.evidence`         | Verify that the submitted state is the line head              |
| `line.merge`            | Merge with the reviewed state guard                           |
| `line.remove`           | Stop the worker provider and delete its line; keep states     |
| `agent.release`         | Stop the lead provider and release its association; keep main |
| `layr.provider.*`       | Start, inspect, stop or attach the owned provider             |
| `layr.credentials.sync` | Copy access credentials into the private agent profile        |
| `layr.exec`             | Run an owner command with a detached journal and deadline     |
| `layr.file.*`           | Read files through the line owner's permissions               |

## Turn states and task results

Studio saves a state after each completed, failed or interrupted agent turn.
The next provider turn waits until pending state receipts have a proven result.
A lost reply preserves the exact request ID. It never creates a second state.

The worker commits with layr and submits `layr rev-parse HEAD` as its revision.
Studio stores the project, line and state with the task result.
Acceptance calls `layr merge <line> --expect <reviewed state>`.
A changed source state rejects acceptance. The lead resolves conflicts in main.
After acceptance, an inactive worker can be archived. Archive removes its line and retains its states.
Restore requires a new worker from the retained state.

## Providers and credentials

The Mac still owns sign-in and refresh. The credential flow does not change.
Codex uses the existing external-auth bootstrap and host refresh path.
Claude uses an access-only guest profile. Guest profiles contain no refresh credentials.
Each account profile stays private to the Linux user who runs the provider.

Providers use the existing durable native supervisor transport.
A guest service or Studio restart can attach to the same live provider.
A VM reboot ends the process. An unproven result remains unknown.
A caller must recover its receipt before it retries a mutation.

Use `host_exec` for macOS commands. It uses a Mac slot and returns the result through layr.
The Mac native project folder is not that slot and does not receive automatic changes.
VM command monitors use guest Bash and the owned provider sandbox.
Mac-only browser paths are not injected into guest provider configuration.

Existing native sessions keep their frozen tools, role text, and original Source headers.
The frozen runtime references are not documentation that can be revised.

## Checks

Run checks one at a time:

```sh
python3 workspaces/runtime/apps/server/src/codex_python.py --exec workspaces/runtime/apps/server/tests/vm-agents-contract.py
python3 workspaces/runtime/apps/server/src/codex_python.py --exec workspaces/runtime/apps/server/tests/linux-vm-studio-native.py --state-dir <isolated-vm-state> --helper <native-helper>
```

The first suite uses a local native provider fixture. It sends no model request.
The second suite requires a provisioned isolated VM and explicit authority for live model requests.
Do not restart a VM or remove a journal that has an active or unknown provider result.
