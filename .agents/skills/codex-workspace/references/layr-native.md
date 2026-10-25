# Native Layr chats

This guidance applies when `workspaceMode` is `layr-native`. The UI label is `Layr`.
New lead chats default to this mode on macOS.
Linux defaults to WT until the native capability probe reports ready.
When ready, Linux defaults to Layr. An explicit native choice requires a ready probe.

Agents run as native host processes on macOS APFS or Linux btrfs.
The project lives in a host layr store. The lead owns its main line.
The original project folder is separate. Studio does not synchronize it with main.

## Store and roles

| Value           | Runtime source                                                                         |
| --------------- | -------------------------------------------------------------------------------------- |
| Store           | `<Studio state>/layr-native/store`                                                     |
| Binary          | `<Studio state>/layr-native/bin/layr`, version 0.2.0                                   |
| Environment     | `LAYR_ROOT` selects the store. macOS uses `LAYR_DIRECT=1`. Linux uses `LAYR_DIRECT=0`. |
| Linux socket    | `LAYR_SOCKET=<Studio state>/layr-native/layr.sock`.                                    |
| Worker selector | `workspace: "layr-native"`. Reviewers inherit this mode.                               |
| Ready mode      | `workspaceMode: "layr-native"`, `executionMode: "native"`.                             |
| Command path    | The managed binary folder comes first in `PATH`.                                       |

Use the assigned environment. Do not install over the user's layr binary or import the project again.

| Role        | Folder and authority                                                                                 |
| ----------- | ---------------------------------------------------------------------------------------------------- |
| Lead        | Main in the host store. The lead owns main.                                                          |
| Implementer | Its own `agent-<sha256(agent ID)>` line, with warm caches from main.                                 |
| Reviewer    | Main stays read-only. The private `layrScratch` folder permits writes. No worker line or turn saves. |

A native Layr reviewer cannot select `shared` as its workspace.

Read `layrReady: true` before you edit. For a lead or implementer, `cwd` names the assigned line folder.
The `layrProjectId`, `layrLine`, and `layrStateId` fields identify its project, line, and base state.
A reviewer's `cwd` is its private `layrScratch` folder. Its `layrReadCwd` points to main for trusted reads.
The project ID comes from the canonical original host folder.

## Task recipes

| Task                                          | Action                                                                      |
| --------------------------------------------- | --------------------------------------------------------------------------- |
| Find your workspace                           | Read the assigned store, line, and readiness result.                        |
| Start implementation                          | Wait for readiness. Change only your assigned host line.                    |
| Create an implementer's line                  | Studio uses `layr line new <name> <base>`.                                  |
| Inspect another agent's result as reviewer    | Use `layr_read` to read the exact submitted state.                          |
| Save a named result                           | Use `layr add <paths>` and `layr save -m "<message>"`.                      |
| Submit the task revision                      | Use the full named head state ID from `layr rev-parse head`.                |
| Check the exact result as lead or implementer | Use `layr try <state> -- <command>`.                                        |
| Check the exact result as reviewer            | Call `layr_read` with `args: ["try", "<state>", "--", "<command>", ...]`.   |
| Accept an implementer's result                | Merge with `layr merge <line> --expect <state>`.                            |
| End a lead or implementer turn                | Studio repairs caches and saves an automatic state.                         |
| Archive a worker                              | Studio saves, pins the exact state, then runs `layr line delete -D <line>`. |

Use [the layr task recipes](../../layr/SKILL.md) for recovery, checks, queues, probes, overlays, and warm caches.
Save overlay edits as named states before the turn ends. Automatic states omit private overlay files.
An automatic state is separate from the named head. It does not replace a named task result.
Studio blocks the next turn until the previous turn save completes.

Acceptance refuses when the source line no longer matches the reviewed state.
Resolve conflicts in an owned task line before acceptance when main requires checks.
Git is the remote bridge. Use it only when the task permits an export or push.

Archive creates `pins/studio/archive/<sha256(agent ID)>` without an expiry.
The receipt records `archiveRef` and `stateId`. The pin retains unmerged automatic states through garbage collection.
Studio does not remove archive pins automatically.
After review or explicit discard, an operator can run `layr ref delete <archiveRef>` to permit normal garbage collection.

## Reviewer checks

Use `layr_read` for history and exact-state checks.
Codex permits writes in the private scratch folder. Main stays read-only.
Claude uses the same scratch working directory. Its sandbox denies store writes and Unix sockets.
Direct shell commands can need store writes that the reviewer's sandbox refuses.
The try path uses a runtime sandbox wrapper inside the temporary state copy.
It uses `sandbox-exec` on macOS and Bubblewrap on Linux.
The command can write only to that copy and the private scratch folder. It has no network access.
On Linux, the try sandbox hides the store and the daemon socket.

## Linux service

Linux btrfs operations require a root daemon.
The daemon uses the same managed binary, store, and socket as the runtime.

| Task                                      | Action                                                                                          |
| ----------------------------------------- | ----------------------------------------------------------------------------------------------- |
| Prepare Linux                             | Install the managed binary first. Start that binary as root with the assigned store and socket. |
| Start a native chat or migration          | Keep the daemon active before setup.                                                            |
| Check an installed Linux binary's version | Keep the daemon active. The version check also uses it.                                         |

Studio does not change mounts or install the root service.
The `layr_native::socket_path` function owns the Linux socket path.
macOS uses direct execution and needs no daemon.

Existing sessions retain their saved role and tool contracts.
