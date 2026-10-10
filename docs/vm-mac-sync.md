# Mac folder sync for layr projects

Status: phase 1 implemented (2026-10-10), decision 24 in
[btrfs-local-vcs.md](btrfs-local-vcs.md).
Owner of the decisions: the user.

## Goal

The user keeps one working folder on the Mac, for example `~/Projects/lumina`, with
files that never go to Git. Agents work in the Studio VM on layr lines. Without sync
the two copies drift apart: Mac edits never reach the agents, and accepted agent work
never reaches the Mac. Mac sync keeps the Mac folder and the VM project equal in both
directions, without a silent overwrite on either side.

The read-only Finder view (`~/Studio/<name>`, [vm-layr.md](vm-layr.md)) stays. It shows
the VM main line and its states; the Mac folder stays the place where the user edits.

## Model

- **Mac folder**: the user's working copy. Git lives here.
- **main** (VM): the protected line where agents' reviewed work lands.
- **`mac` line** (VM): owned by the project user; it mirrors the Mac folder.
- **Agreed state A**: the last layr state that equals the Mac folder for every synced path.
- **Manifest** (Mac, in the VM state directory): for each synced path, the size,
  modification time and SHA-256 of its agreed content. It tells which Mac files
  changed since A without reading the VM.

## What syncs

Every file below the project folder, except:

- `.git` directories and files, `.worktrees`, and the Studio image store;
- layr layers (the project's regenerable paths such as `node_modules`, `target`,
  `.venv`): each side builds its own, and the VM keeps warm layers;
- folders that Git ignores (build output, caches);
- special files (devices, sockets, FIFOs) and files over 256 MiB, with a notice.

Files that Git ignores, such as `.env`, do sync: agents need the same local
configuration as the user. Symbolic links sync as links and are never followed.

## A sync round

Rounds are serialized per project. Each step has an operation ID and a receipt stage,
so a lost response repeats the same step with the same ID and never applies it twice.

1. **Detect.** Compare the Mac folder with the manifest (size and time first, SHA-256
   to confirm). File events start a round after 2 s of quiet; a full scan runs at
   start, after dropped events, and every 10 minutes as a safety net.
2. **Inbound.** Upload the changed files and the deletions with the tree upload
   protocol. The guest broker applies them to the `mac` line as the project user and
   saves state Sm (label `mac-sync`).
3. **Merge.** As the project user: `layr merge mac --expect Sm` into main. A clean merge
   gives main'. A conflict leaves main unchanged (layr refuses a conflicted merge into
   a protected line), and the project enters the conflict state below.
4. **Outbound.** For each path that changed from A to main': when the Mac file still has
   its agreed content, or the content uploaded in this round, write the main' version
   atomically (temporary file and rename, same mode). When the user changed it again
   during the round, skip it; the next round uploads it.
5. **Agree.** A := main'; update the manifest; reset the `mac` line to main'.

## Conflicts

- Studio never writes conflict markers into the Mac folder.
- In the conflict state, other paths keep syncing in both directions; conflicted
  paths wait.
- The project's lead chat gets a message to resolve the conflict in the `mac` line
  (`layr merge main`, fix, `layr commit`); Studio then merges `mac` into main and the
  round continues. A user who does not read code can choose per path in the UI:
  **Keep Mac version** or **Keep VM version**.

## Git

- Git stays on the Mac: `.git` never syncs. Accepted agent work appears in the Mac
  folder as uncommitted changes; the user or a Mac agent commits and pushes.
- For a synced project, `layr push` from the VM is off: two publishers would fork the
  history. Decision 19 (one squashed commit per task) applies to unsynced projects.

## Safety

- The VM stopped or unreachable: rounds wait; Mac edits stay detectable by the
  manifest; nothing is lost.
- A round that would delete more than 25 % of the files or more than 500 files on
  either side pauses for the user's confirmation (an accidental `rm -rf`, a wrong path).
- Mac writes never go through symbolic links and never leave the project folder.
- Sync writes in the VM run as the project user, never as root, so layr rules apply.

## Watcher

A small signed Swift helper in the app reports FSEvents file events as JSON lines to
the backend. A dropped-events flag or a helper restart triggers a full scan.

## User interface

- The sidebar shows the project's layr readiness and its sync state: synced, syncing,
  paused, conflict (count), or error with its reason.
- The project menu offers pause and resume, and the conflict choices.

## Implementation

- Mac side: `workspaces/runtime/apps/server/src/codex_vm_mac_sync.py`. The runtime
  maintenance tick starts rounds every 10 s for each VM project with a Mac folder
  (`layr-projects/<id>/project.json`), only while the VM runs. Per project, the folder
  `layr-projects/<id>/sync/` holds `manifest.json` (agreed content and its base state
  per path), `held.json` (sent content that is not agreed yet: a conflict, or Mac work
  waiting for a merge), `pending.json` (the round to repeat after a lost reply) and
  `status.json`. `config.json` with `{"enabled": false}` pauses the project.
- Guest side: `workspaces/runtime/apps/vm-guest/mac_sync.py`, broker methods
  `sync.mac.apply`, `sync.mac.read` and `sync.mac.hashes`. Receipts per operation make a
  repeated call return the first result. The project user owns the `mac` and `mac-view`
  lines; layr adds Mac files that Git ignores by name, so they reach main.
- `project.json` records `importStateId`, the base of a project's first round. A project
  imported before this change has none: its first round agrees only on files equal to
  main, and every other difference waits in `status.json` (`choices`).
- Checks: `test_codex_vm_mac_sync.py` (Mac side), and the opt-in guest end-to-end tests
  `test_layr_e2e.py` and `test_mac_sync_e2e.py` with a real layr daemon.

## Phases

1. Manifest, scan, inbound into the `mac` line, merge, outbound and agree, with a
   polling scan. Tests with fakes, the guest end-to-end test, and a live round. (Done.)
2. The FSEvents helper.
3. Conflict UI, the lead message, and the push rule.
4. Sync state in the sidebar.
