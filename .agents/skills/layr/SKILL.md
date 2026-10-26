---
name: layr
description: Task recipes for layr 0.2.0 on macOS APFS and Linux btrfs. Use in a layr line to save, recover, review exact states, merge, compare behavior, keep warm caches, or use the Git bridge.
---

# layr by task

Use layr for local version control. Use `layr --help` to list the commands.
A line is a folder: `<root>/projects/<project>/lines/<line>`.
Run write commands in your own line. Enter another line with `cd` only when you own it.

In Studio, use the assigned store and line. Do not import the project again.
In native mode, a reviewer's `cwd` is its private `layrScratch` folder.
Its `layrReadCwd` points to main for trusted `layr_read` calls. Main stays read-only.
Claude's reviewer sandbox denies store writes and Unix sockets.

These recipes require version 0.2.0. Check with `layr --version`.
The old Git verbs are removed. Use `head` for a layr revision.
Git commands inside the Git bridge retain Git names.

## Save and inspect

| Task                                             | Command                            |
| ------------------------------------------------ | ---------------------------------- |
| Inspect current changes and conflicts            | `layr status`, `layr diff`         |
| Read status as JavaScript Object Notation (JSON) | `layr status --json`               |
| Stage the task files                             | `layr add <path>`                  |
| Save the staged files as a named state           | `layr save -m "<message>"`         |
| Save changes to tracked files                    | `layr save -a -m "<message>"`      |
| Save a checkpoint of unfinished work             | `layr save`                        |
| Replace the last named state                     | `layr save --amend -m "<message>"` |
| Get the full state ID for submission             | `layr rev-parse head`              |
| Read a file from a state                         | `layr show <state>:<path>`         |
| Compare your changes with main                   | `layr diff main...head`            |
| Read named history as JSON, one object per line  | `layr log --json`                  |
| Read local line IDs, heads, owners, and paths    | `layr line list --json`            |

Named saves use the index. Stage new files explicitly.
An automatic save does not replace a named task result.
Revisions include `head~2`, a line, a tag, a state ID prefix, and `head@{n}`.

A line name selects its named head. It does not select unsaved files.
Status reports path overlaps with other lines. Their unsaved changes appear after their next automatic save.

## Recover work

| Task                               | Command                                      |
| ---------------------------------- | -------------------------------------------- |
| Inspect recent operations          | `layr op log`                                |
| Preview the last operation to undo | `layr undo -n`                               |
| Undo the last operation            | `layr undo`                                  |
| Undo a selected operation          | `layr undo <operation>`                      |
| Find checkpoints                   | `layr log --auto --format='%h %ar %s' -n 20` |
| Inspect a candidate state          | `layr show --stat <state>`                   |
| Restore one file from a state      | `layr restore --source <state> -- <path>`    |
| Recover old work in a new folder   | `layr line new <name> <state>`               |
| Keep unfinished work for later     | `layr park`                                  |
| Inspect parked work                | `layr park list`, `layr park show`           |
| Restore and retain parked work     | `layr park apply`                            |
| Restore and remove parked work     | `layr park pop`                              |

Use the full state ID from history for recovery. Checkpoint numbers change after new saves.
Undo can also reverse an undo. Inspect the operation before you select it.
Park and amend refuse in a line with overlays. Save overlay changes as named states.

## Check and approve exact states

| Task                                          | Command                                         |
| --------------------------------------------- | ----------------------------------------------- |
| Check the submitted state in a temporary copy | `layr try <state> -- <command>`                 |
| Define a named check                          | `layr config check.unit '<command>'`            |
| Run the configured check for an exact state   | `layr try <state> --check unit`                 |
| Inspect recorded checks                       | `layr attestations <state> --kind check --json` |
| Attach evidence from a file                   | `layr attest origin <state> --payload <file>`   |
| Approve the reviewed state                    | `layr approve <state>`                          |
| Inspect all evidence for the state            | `layr attestations <state> --json`              |
| Withdraw your approval                        | `layr approve --withdraw <state>`               |

Try preserves your line and records the command result for the exact state.
Use the configured check when a protected line requires it. A different command does not satisfy that check.

Attest stores the payload digest. It does not store the file contents or run a check.
An approval applies to one exact state. Review a changed state again.

Try runs as the caller. It does not isolate network access or system calls.

## Work in parallel and accept results

| Task                                   | Command                                     |
| -------------------------------------- | ------------------------------------------- |
| Create your line from main             | `layr line new <name> main`                 |
| Create several independent lines       | `layr line new agent main --count 4 --json` |
| List local lines                       | `layr line list`                            |
| Merge only the reviewed source state   | `layr merge <line> --expect <state>`        |
| Apply one state's change               | `layr pick <state>`                         |
| Apply your states on a new base        | `layr replay main`                          |
| Reverse one state's change             | `layr invert <state>`                       |
| Delete a merged line from another line | `layr line delete <line>`                   |

Batch creation makes `agent-1` through `agent-4`. Read the JSON paths before you assign work.
Only the owner changes a line. The lead owns main in Studio.

Native Studio archive pins the saved state at `pins/studio/archive/<sha256(agent ID)>` before line deletion.
The pin has no expiry. The receipt records `archiveRef` and `stateId`.
Studio does not remove these pins automatically.
After review or explicit discard, an operator can run `layr ref delete <archiveRef>` to permit normal garbage collection.

Submit the full saved state ID. Inspect that state before acceptance.
Expect pins the source state. A moved source line refuses the merge.

A merge can create a state with conflicts. Inspect status before you report success.
Resolve conflicts in an owned task line when main requires checks.
Edit the files, stage them, and save a named state.

Pick, replay, and invert apply the shared files. They do not apply private overlay changes.

## Keep main ready through the queue

| Task                                | Command                              |
| ----------------------------------- | ------------------------------------ |
| Require the named check on main     | `layr protect main --check unit`     |
| Queue an exact source state         | `layr queue add <state> --into main` |
| Inspect entries and failure reasons | `layr queue list --into main --json` |
| Process queued entries              | `layr queue run --into main`         |
| Remove an obsolete entry            | `layr queue remove <entry>`          |

Configure checks before you process the queue. The queue checks the exact merge result.
A queued entry retains its source state if the worker line moves.
Conflicts or failed required checks leave main unchanged. Inspect each entry's result.

Ordinary merges do not run probes. The queue runs probes only when `queue.probes` is true.

## Find faults and review revisions

| Task                                            | Command                                                         |
| ----------------------------------------------- | --------------------------------------------------------------- |
| Find the first bad state, including checkpoints | `layr bisect run --auto --good <good> --bad <bad> -- <command>` |
| Inspect completed searches                      | `layr bisect list --json`                                       |
| Find a change ID                                | `layr change list --json`                                       |
| Read versions and evidence for one change       | `layr change show <change>`                                     |
| Compare two versions of the change              | `layr change diff <change> --from v1 --to v2 --json`            |
| Run a configured behavior probe                 | `layr probe run <state> --probe <probe> --json`                 |
| Compare stored probe results                    | `layr probe diff <base> <state> --json`                         |

Bisect needs a command that passes at good and fails at bad. The fault must remain present after its first occurrence.
It uses temporary copies and can reuse exact-state results for the same command.

A change keeps its ID through amend, pick, and replay.
Change diff compares each version against its own parent. Base changes stay outside that comparison.

Define probes in the revision's `layr.toml` with `run`, `outputs`, and `kind`.
Run the probe for both states before you compare them.
Probe diff and change show read stored results. They do not run probe commands.

## Keep warm caches

A cache holds files that tools can make again, such as `target` or `node_modules`.
Ignore the folder in `.gitignore` or `.layrignore` before you register it.
Alternatively, save `<folder> regenerable` in `.layrattributes`.

| Task                                              | Command                   |
| ------------------------------------------------- | ------------------------- |
| Register a cache folder, with config permission   | `layr cache add <folder>` |
| Check registered folders                          | `layr cache list`         |
| Repair a cache after a tool replaces its folder   | `layr cache repair`       |
| Save cache snapshots for new lines and try copies | `layr save --caches`      |

Caches stay outside source history and diffs. Keep dependency lockfiles in source history.
Save warm caches before parallel line creation. Repair them after tools replace their folders.

## Keep .env private with overlays

| Task                                                          | Command                                     |
| ------------------------------------------------------------- | ------------------------------------------- |
| Create a private overlay for its owner                        | `layr overlay create secrets --mode follow` |
| Move .env out of shared files and remove affected checkpoints | `layr overlay move --purge secrets .env`    |
| Save the private files                                        | `layr save -m "Save private settings"`      |
| Inspect overlays and their readers                            | `layr overlay list --json`                  |
| Add an existing readable overlay to your line                 | `layr overlay use --add secrets`            |
| Read the overlay's named history                              | `layr overlay log secrets`                  |
| Save the exact shared state and overlay stack                 | `layr view save --json`                     |
| Check a saved stack                                           | `layr try views/<view> -- <command>`        |

New files go to the shared set unless you move them explicitly.
Earlier shared named states retain a moved secret. Purge removes only affected automatic states of the current line.
The Git bridge exports shared files only. A private overlay has its own readers and history.

Automatic saves omit overlay files. Save private edits with a named save before the turn ends.

Expect pins the shared source state. It does not pin the overlay stack.
Use a saved view for a check of an exact stack. Its check applies to that view.
Use `view_id` from the saved view JSON with the `views/` prefix.
Routing patterns, ephemeral overlays, and folder overlays are unavailable in 0.2.0.

## Use the Git bridge

| Task                                         | Command                                                 |
| -------------------------------------------- | ------------------------------------------------------- |
| Read configured Git remotes                  | `layr git remote -v`                                    |
| Update the local Git mirror without a push   | `layr git sync`                                         |
| Name exported source content across machines | `layr rev-parse <state>^{tree}`                         |
| Export the line history when authorized      | `layr git push origin <branch>`                         |
| Export one task commit when authorized       | `layr git push --squash -m "<message>" origin <branch>` |

In a project with a Git store, layr updates Git HEAD when the line head moves.
A Git folder made by the line owner stays under that owner's control.
Git sync requires the line's Git mirror. A line made before Git setup can have no mirror.

Use layr for source changes. Tools can read the Git mirror.
The top-level push command sends to layr remotes. Use the Git bridge for a Git remote.

In a Studio VM project with host sync, use the host bridge policy in the workspace reference.

## Explain a refusal

| Task                                      | Action                                             |
| ----------------------------------------- | -------------------------------------------------- |
| Check a user's permission                 | `layr access check <user> <action>`                |
| Another user owns the line                | Use your assigned line.                            |
| Main requires review                      | Review the exact state and use merge with expect.  |
| A merge has conflicts                     | Resolve them in an owned line before acceptance.   |
| The store cannot represent two path names | Use distinct names or a case-sensitive APFS store. |

APFS clones folders and verifies copies. It does not make atomic APFS volume snapshots.
Pause file changes while you save an exact input tree.
Use the saved state for checks while other work continues.
