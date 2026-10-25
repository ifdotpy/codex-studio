# Native Layr chats

This guidance applies when `workspaceMode` is `layr-native`. The UI label is `Layr`.
Agents run as native host processes on macOS APFS or Linux btrfs.
The project lives in a host layr store. The lead owns its main line.
The original project folder is separate. Studio does not synchronize it with main.

## Store and roles

| Value        | Runtime source                                                           |
| ------------ | ------------------------------------------------------------------------ |
| Store        | `<Studio state>/layr-native/store`                                       |
| Binary       | `<Studio state>/layr-native/bin/layr`, version 0.2.0                     |
| Environment  | `LAYR_ROOT` selects the store. `LAYR_DIRECT=1` selects direct execution. |
| Command path | The managed binary folder comes first in `PATH`.                         |

Use the assigned environment. Do not install over the user's layr binary or import the project again.

| Role        | Folder and authority                                                                      |
| ----------- | ----------------------------------------------------------------------------------------- |
| Lead        | Main in the host store. The lead owns main.                                               |
| Implementer | Its own `agent-<sha256(agent ID)>` line, with warm caches from main.                      |
| Reviewer    | Main as the current folder, under a read-only sandbox. No worker line or turn-end writes. |

## Task recipes

| Task                                          | Action                                                       |
| --------------------------------------------- | ------------------------------------------------------------ |
| Find your workspace                           | Read the assigned store, line, and readiness result.         |
| Start implementation                          | Wait for readiness. Change only your assigned host line.     |
| Create an implementer's line                  | Studio uses `layr line new <name> <base>`.                   |
| Inspect another agent's result                | Read the exact submitted state with the runtime's read tool. |
| Save a named result                           | Use `layr add <paths>` and `layr save -m "<message>"`.       |
| Submit the task revision                      | Use the full named head state ID from `layr rev-parse head`. |
| Check the exact result as lead or implementer | Use `layr try <state> -- <command>`.                         |
| Accept an implementer's result                | Merge with `layr merge <line> --expect <state>`.             |
| End a lead or implementer turn                | Studio repairs caches and saves an automatic state.          |
| Archive a worker                              | Studio deletes its line. Its states remain.                  |

Use [the layr task recipes](../../layr/SKILL.md) for recovery, checks, queues, probes, overlays, and warm caches.
Save overlay edits as named states before the turn ends. Automatic states omit private overlay files.
An automatic state is separate from the named head. It does not replace a named task result.

Acceptance refuses when the source line no longer matches the reviewed state.
Resolve conflicts in an owned task line before acceptance when main requires checks.
Git is the remote bridge. Use it only when the task permits an export or push.

## Open implementation facts

The `layr-native-mode` worker owns this integration.
These facts still require confirmation from that worker:

| Fact                                          | Status                                       |
| --------------------------------------------- | -------------------------------------------- |
| Import and readiness result fields            | Open. Do not edit before readiness.          |
| Native read tool name and reviewer try access | Open. Keep the reviewer's read-only sandbox. |
| Worker workspace selection field              | Open. Read the current spawn schema.         |
| Mode defaults and platform refusal rules      | Open. Read the chat's saved mode.            |

Existing sessions retain their saved role and tool contracts.
