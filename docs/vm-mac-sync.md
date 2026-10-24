# Mac folder sync for layr projects

Status: implemented (2026-10-10), decision 24 in
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
- dependency folders (`node_modules`, `.venv`): each side installs its own;
- folders that Git ignores (build output, caches);
- special files (devices, sockets, FIFOs) and files over 256 MiB, with a notice. A
  synced file that grows over the limit keeps its last synced version in the VM; it
  is not a deletion.

Files that Git ignores, such as `.env`, do sync: agents need the same local
configuration as the user. Symbolic links sync as links, to any target, and are never
followed. The executable bits of files sync in both directions; a content change keeps
the other side's bits unless the Mac changed them.

## A sync round

Rounds are serialized per project. Each step has an operation ID and a receipt stage,
so a lost response repeats the same step with the same ID and never applies it twice.

1. **Detect.** Compare the Mac folder with the manifest (size and time first, SHA-256
   to confirm). File events start a round after 2 s of quiet; a full scan runs at
   start, after dropped events, and every 10 minutes as a safety net.
2. **Inbound.** Upload the changed files and the deletions with the tree upload
   protocol. The guest broker applies them to the `mac` line as the project user and
   saves state Sm (label `mac-sync`).
3. **Merge.** As the project user: `layr merge --ff-only mac --expect Sm` into main. The
   `mac` line starts each round at main, so the merge is a fast-forward. When main moved
   during the round, or holds uncommitted lead work, the fast-forward is refused. A
   three-way merge is never used: on a conflict layr would commit markers into a line
   without checks. Main stays unchanged, the `mac` line goes back to main, and the next
   round sends the same Mac files again and folds them onto the new main file by file.
4. **Outbound.** For each path that changed from A to main': when the Mac file still has
   its agreed content, or the content that main took from this round, write the main'
   version with an atomic inode exchange. The displaced inode stays in recovery storage.
   When the user changed it again during
   the round, skip it; the next round uploads it, with the content main took as its base.
   A Mac edit that main did not take (a refused merge or a conflict) is never written
   over.
5. **Agree.** A := main'; update the manifest; reset the `mac` line to main'.

## Conflicts

A conflict is always per file: the Mac and main changed the same lines of one file
since their agreed content, one side deleted a file that the other changed, or one side
has a file where the other has a folder.

- Studio never writes conflict markers into the Mac folder.
- In the conflict state, other paths keep syncing in both directions; conflicted
  paths wait. A held path is not sent again until the Mac file changes.
- The user chooses per path:
  - **Keep Mac version**: the next round sends the Mac file on top of main.
  - **Keep VM version**: Studio writes main's file into the Mac folder.
  - **Ask the agent**: Studio puts the Mac version in the `mac-conflict` line at main and
    sends the project's lead chat a message with the state to read
    (`layr show <state>:<path>`). The lead commits one combined file to main; the next
    round writes it to the Mac. A new Mac edit of the path ends the handoff and is never
    overwritten; it is sent as usual and can become a new conflict.
- Unmerged handoffs stay in the `mac-conflict` line; a handoff after a merge starts the
  line again at main.
- A project imported before `importStateId` existed starts with a choice for every
  path that differs from main (`choices`); the same three answers apply.

## Git

- Git stays on the Mac: `.git` never syncs. Accepted agent work appears in the Mac
  folder as uncommitted changes; the user or a Mac agent commits and pushes.
- For a synced project, `layr push` from the VM is off: two publishers would fork the
  history. Decision 19 (one squashed commit per task) applies to unsynced projects.

## Safety

- The VM stopped or unreachable: rounds wait; Mac edits stay detectable by the
  manifest; nothing is lost.
- A round that would delete more than 25 % of the files or more than 500 files on
  either side pauses for the user's confirmation (an accidental `rm -rf`, a wrong path,
  an agent's mass delete in main).
- An uncertain guest response keeps the exact operation ID and payload in `pending.json`.
  This includes every `outcome_unknown` response, regardless of its error text.
  A later round repeats that request until the guest gives a proven result.
  A definite refusal ends the operation. A failed fold leaves nothing in the `mac` line.
- A legacy `recover.json` record stops sync. The original request or a final receipt
  must establish the result before another operation can start. Equal hashes alone
  cannot prove that an old operation has stopped.
- A scan error stops the round before any upload. An unreadable folder is not a deletion.
- A case or Unicode form rename keeps the file on the Mac. A name collision waits.
- A Mac save during a round remains recoverable, including a save after the last content check.
  Replacement exchanges the current inode with the staged VM inode. Deletion moves the current
  inode into recovery storage. Creation uses a no-replace rename.
  The filesystem must support these atomic operations. Recovery storage must use the same filesystem.
  An unsupported operation stops the round. No copy-and-delete fallback runs.
- Displaced inodes remain in private `.studio-sync-preserved-<uuid>` directories.
  Git projects use their Git metadata directory. Other projects use the target file's parent directory.
  Each directory has a Git exclusion before it receives content, including before a later Git initialization.
  `record.json` records the path, operation phase, original hash, and conflict state.
  Files and records receive filesystem synchronization before the round reports success.
  Studio never removes these directories or their original inodes automatically.
- A concurrent save can change the displaced inode through an existing open descriptor.
  The save stays in `original`, not necessarily at the current project path.
  A detected change stops later rounds and reports the recovery directory in `reason`.
  Every round checks retained originals before bootstrap, uploads, or outbound changes.
  This check does not depend on file events, including an empty event set.
  It inspects external Git worktree metadata and walks project directories without reading ordinary file content.
  The guard cannot promise that the visible project path always contains the latest uncoordinated save.
  It protects the displaced bytes instead. Storage grows until the user reviews retained data.
- An old `.name.studio-sync-<8 hex>` temporary file stops sync for review.
  It can contain the only original file. Studio does not delete it automatically.
- Changes to project paths never follow symbolic links through parent directories.
- Sync writes in the VM run as the project user, never as root, so layr rules apply.

### Review retained data

This procedure requires an operator. The normal conflict buttons do not clear a retained-inode error.

1. Pause the project and wait for its active round to finish.
2. Close editors and other processes that can write through an old file descriptor.
3. Stop the backend through its normal controls before changing its sync records.
4. Read the reported recovery directory and its `record.json`.
5. Compare the current project file with the retained `original` without changing either file.
6. Make independent copies of both versions before selecting the desired project content.
7. Restore the selected content to the project path.
8. Set that recovery record's `phase` to `reviewed` only after completing the comparison.
9. Clear the matching entry from `status.json`'s `preservedConflicts` array.
10. Restart the same backend with the same runtime state identity.
11. Run the preview before resuming the project.

The `reviewed` phase permits sync without deleting the retained inode.
An incomplete `prepared` record requires the same review. Never infer completion from equal hashes alone.
Keep independent copies if you later remove recovery storage yourself.

## Watcher

`studio-fs-events` (`workspaces/client/apps/desktop/native/fs-events.swift`), a small
signed helper in the app, reports FSEvents file events for every synced folder as JSON
lines: `{"path"}` per changed path, `{"rescan"}` when macOS dropped events or the root
changed, and `{"ready"}` once. It exits when the backend closes its input.

- A round starts 2 s after the last event of a folder and reads only the named paths.
- A dropped-events report, a helper start, and a change of the folder list cause a
  full scan of the affected folders.
- Every 10 s the backend asks the VM for the main state (`sync.mac.head`); a round
  without a Mac scan runs only when main moved.
- A full scan runs every 10 minutes as a safety net, and every 10 s when the helper is
  missing.

## User interface

- The project entity carries `layrSync` (`codex_vm_agents.project_sync`, host records
  only) next to `layrReadiness`; the backend updates the entity when a round or a
  choice changes what the sidebar shows.
- One chip follows the project name, so the name keeps its room. A sync problem
  (error, conflict, paused) comes first, then an import that is not ready, then the
  sync state. `SYNC n` counts the files that wait for a choice. The tooltip and the
  project button's description carry both reports.
- The project menu, section **Mac folder sync**: **Choose versions (n)** opens the
  dialog with Keep Mac, Keep VM and Ask agent per path; **Sync n Mac deletions**
  confirms a deletion pause; **Pause sync** and **Resume sync**. In the combined
  sidebar each action goes to the server that syncs the folder.
- The API answers at once: a Keep Mac choice and a deletion confirmation leave the
  round to the scheduler, which runs it within 10 s.

## Implementation

- Mac side: `workspaces/runtime/packages/runtime-services/src/mac_sync.rs` (choices,
  configuration, sidebar report), `mac_sync/round.rs` (the round) and
  `mac_sync/schedule.rs` (the schedule and the file event helper). The server starts the
  schedule on macOS next to the VM watchdog; it ticks every second and runs rounds (see
  Watcher) for each VM project with a Mac folder (`layr-projects/<id>/project.json` in the
  VM state directory, `linux-vm/`), only while the VM runs. Per project, the folder
  `layr-projects/<id>/sync/` holds:
  - `manifest.json`: agreed content and its base state per path;
  - `held.json`: Mac content in a conflict, or handed to the lead (`why`: `conflict` or
    `agent`, with the handoff's main state as `base`);
  - `pending.json`: the round to repeat after a lost reply;
  - `recover.json`: a legacy record of an operation whose request was discarded.
    Sync stops until its receipt can be reconciled. New rounds never create this record;
  - `allow-deletes.json`: the user's confirmation of the Mac deletions that paused the
    project, used by the next round;
  - `config.json`: `{"enabled": false}` pauses the project;
  - `status.json`: `state` (`syncing`, `synced`, `waiting`, `conflict`, `paused` or
    `error`), `conflicts`, `choices`, `withAgent`, `deletes`, `reason`, and `rescan` (paths
    that a refused merge or a choice left unsent). Internal `preserved` and
    `preservedConflicts` arrays name retained inodes and those that require operator review.
- Guest side: `workspaces/runtime/apps/vm-guest/mac_sync.py`, broker methods
  `sync.mac.apply`, `sync.mac.read`, `sync.mac.hashes`, `sync.mac.head` and
  `sync.mac.handoff`. Receipts per operation make a repeated call return the first
  result. The broker writes its receipt row before an operation and the response after
  it; a stop of the guest service or the broker between the two leaves the row without a
  result. `sync.mac.apply` then runs again with the same ID (it starts from main and keeps
  its own receipt); other methods answer `outcome_unknown`. The project user owns the `mac`, `mac-view` and `mac-conflict` lines; layr adds
  Mac files that Git ignores by name, so they reach main. The first round of a synced
  project denies `push` to the agents group.
- Preview: `studio-runtime-server mac-sync plan PROJECT --state DIRECTORY` reads an
  existing runtime state without opening its server database or starting a backend.
  It reads `sync.mac.head` and `sync.mac.preview`. The guest reads immutable Linux
  layr snapshots without creating users or lines or changing `mac-view`.
  An older guest rejects the new preview method. No fallback runs the old mutable hash method.
  The lists include content, file kind, and executable bit changes.
  The preview reports a pending operation separately. Its lists do not authorize a
  replacement operation or bypass the guards of a real round.
  Use the [isolated round procedure](verification/mac-sync-isolated-round.md) before a real guest trial.
- HTTP API: `POST /api/mac-sync/resolve` (`projectId`, `path`, `choice`: `mac`, `vm` or
  `agent`) and `POST /api/mac-sync/config` (`projectId`, `enabled`, `allowDeletes`). The
  lead message has one ID per handoff state, so a repeated request does not repeat it.
- `project.json` records `importStateId`, the base of a project's first round. A project
  imported before this change has none: its first round agrees only on files equal to
  main, and every other difference waits in `status.json` (`choices`).
- Checks: `runtime-services/tests/vm_macsync_round.rs` (Mac side, ported from the
  Python `test_codex_vm_mac_sync.py`), and the opt-in guest end-to-end tests
  `test_layr_e2e.py` and `test_mac_sync_e2e.py` with a real layr daemon.

## Phases

1. Manifest, scan, inbound into the `mac` line, merge, outbound and agree, with a
   polling scan. Tests with fakes, the guest end-to-end test, and a live round. (Done.)
2. The FSEvents helper. (Done.)
3. Conflict choices, the lead handoff, the push rule, pause and the deletion
   confirmation in the API. (Done.)
4. Sync state, conflict choices, pause and resume in the sidebar. (Done.)
