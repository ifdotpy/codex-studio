---
name: layr
description: Task recipes for Layr 0.3.0 local version control and Layr Cloud changes, review, checks, queue, and release files.
---

# Layr by task

Use the real **Layr 0.3.0** CLI. Check `layr --version` and `layr --help`.
This skill requires the `change show --json` check-results addition from
`cloud/agent-json`; older 0.3.0 builds omit `versions[].checks`.
Report a missing command or field instead of treating missing evidence as success.
Studio's older bundled CLI uses different names; do not apply these recipes to it.

A line is a folder: `<root>/projects/<project>/lines/<line>`. Run Layr inside it.
Use `layr` for local version control. Revision names use lowercase `head`.
`head~2`, a line name, a tag, a state ID prefix, and `head@{n}` are revisions.
A line name selects its last named state, excluding unsaved files.
In Studio, use the assigned store and line. Do not import the project again.
Run write commands in your own line. Enter another line with `cd` only when you own it.
In native mode, a reviewer's `cwd` is its private `layrScratch` folder.
Its `layrReadCwd` points to main for trusted `layr_read` calls. Main stays read-only.
Claude's reviewer sandbox denies store writes and Unix sockets.
The old Git verbs are removed. Git commands inside the Git bridge retain Git names.

## Save and inspect work

| Task                               | Command                                               |
| ---------------------------------- | ----------------------------------------------------- |
| See changes                        | `layr status`, `layr diff`                            |
| Save a named state                 | `layr add -A && layr save -m "<message>"`             |
| Save an automatic state            | `layr save`                                           |
| Get the current state ID           | `layr rev-parse head`                                 |
| Read a file from a state or line   | `layr show <rev>:<path>`                              |
| Compare with another line          | `layr diff <other>...head`                            |
| Search old content                 | `layr grep <pattern> <rev>`, `layr log --grep <text>` |
| Read status as JSON                | `layr status --json`                                  |
| Save tracked files without staging | `layr save -a -m "<message>"`                         |
| Read named history as JSON         | `layr log --json`                                     |

Each named save starts a change. `layr save --amend -m "<message>"` makes a
new version of the current change, preserving its ChangeId. A separate named
save starts a separate change. Stage new files with `layr add` first.

## Recover from a mistake

Layr saves the working folder before changing operations and on `layr save`.
Automatic states retain uncommitted work.

| Task                           | Command                                                |
| ------------------------------ | ------------------------------------------------------ |
| Undo the last Layr operation   | `layr undo` (`layr undo -n` previews it)               |
| Undo an older operation        | `layr op log`, then `layr undo <operation>`            |
| List automatic states          | `layr log --auto --format='%h %ar %s' -n 20`           |
| Inspect a state                | `layr show --stat <state>`, `layr show <state>:<path>` |
| Restore a file                 | `layr restore --source <state> -- <path>`              |
| Start a line from old work     | `layr line new <name> <state>`                         |
| Keep unfinished work for later | `layr park`                                            |
| Inspect parked work            | `layr park list`, `layr park show`                     |
| Restore and retain parked work | `layr park apply`                                      |
| Restore and remove parked work | `layr park pop`                                        |

Use the listed state ID. Each changing operation can change what `head@{n}`
selects. `layr undo` can also undo an undo.
Checkpoint numbers change after new saves. Inspect the operation before you select it.
Park and amend refuse in a line with overlays. Save overlay changes as named states.

## Check an exact state

`layr try <rev> -- <command>` runs in a temporary copy and records a check for
that state. It leaves your line unchanged. `layr try <rev> --check <name>` uses
`check.<name>` from project configuration. A failed command returns nonzero.

Use the configured check when a protected line requires it. A different command does not satisfy that check.
Attest stores the payload digest. It does not store the file contents or run a check.
An approval applies to one exact state. Review a changed state again.

| Task                                 | Command                                       |
| ------------------------------------ | --------------------------------------------- |
| Define a named check                 | `layr config check.unit '<command>'`          |
| Run the configured check for a state | `layr try <state> --check unit`               |
| Attach evidence from a file          | `layr attest origin <state> --payload <file>` |
| Inspect all evidence for the state   | `layr attestations <state> --json`            |
| Withdraw your approval               | `layr approve --withdraw <state>`             |

## Work in parallel

| Task                                      | Command                                                |
| ----------------------------------------- | ------------------------------------------------------ |
| List lines                                | `layr line list --json`                                |
| Start your line                           | `layr line new <name>`, then `cd ../<name>`            |
| Merge a reviewed state                    | `layr merge <line> --expect <state>`                   |
| Take one state                            | `layr pick <rev>`                                      |
| Delete your unused line from another line | `layr line delete <name>` (`-D` permits unmerged work) |
| Apply your states on a new base           | `layr replay main`                                     |
| Reverse one state's change                | `layr invert <state>`                                  |

A merge writes conflicts into its result. `layr status` shows `UU` paths.
Resolve them, then `layr add <paths> && layr save -m "<message>"`.
Only a line's owner changes it. A protected target requires its policy and
`--expect`; the refusal names the ready command, `layr merge <line> --expect <state>`.
Use the Cloud queue when the project requires queued review.
A merge, pick or invert stops while the head has open conflicts. `layr save -m` refuses
a changed file with conflict markers unless `--allow-conflict-markers` is given.
Batch creation makes `agent-1` through `agent-4`. Read the JSON paths before you assign work.
Pick, replay, and invert apply the shared files. They do not apply private overlay changes.
Inspect status before you report success.
Expect pins the source state. A moved source line refuses the merge.

Native Studio archive pins the saved state at `pins/studio/archive/<sha256(agent ID)>` before line deletion.
The pin has no expiry. The receipt records `archiveRef` and `stateId`.
Studio does not remove these pins automatically.
After review or explicit discard, an operator can run `layr ref delete <archiveRef>` to permit normal garbage collection.

APFS clones folders and verifies copies. It does not make atomic APFS volume snapshots.
Pause file changes while you save an exact input tree.
Use the saved state for checks while other work continues.

## Layr Cloud: choose the remote

Inspect `layr remote list --json`. A `layr+https://` URL identifies a gateway.
Use Layr changes and the queue for a project with a Layr Cloud remote.
Use `gh` for review and merge only in a GitHub-only project. A Git mirror does
not make a Layr Cloud project GitHub-only.

The owner provisions the person files, accepted project pin, and access before
an agent starts. Credentials arrive only through the environment:
`LAYR_PERSON_DIR`, `LAYR_PERSON_LOGIN`, and `LAYR_PERSON_KEY_FILE` (a private,
caller-owned mode-0600 **person age identity file**). Never give a paper key to
an agent machine. Do not create person keys, print key files, or put credentials
in source, command arguments, records, or configuration. Gateway writes also
require an owner-provisioned, project-scoped write grant via `LAYR_GATEWAY_TOKEN_FILE` from the environment. A person session supplies
read access; it does not replace this write grant.

The commands below use shell variables supplied for the task:
`LAYR_SITE` (HTTPS gateway origin), `LAYR_CLOUD_URL` (`layr+https://host/project`),
`LAYR_REMOTE` (remote name), `LAYR_CHANGE` (ChangeId), `LAYR_STATE` (reviewed exact
state), `LAYR_COMMENT` (comment ID), `LAYR_QUEUE_ENTRY` (queue entry ID),
`LAYR_RELEASE` (`release/<version>`), and `LAYR_ARTIFACT` (built file path).
These are addresses and identifiers, never credential values.

Verify the provisioned identity, then add the remote **once** if absent:

<!-- recipe: cloud-identity -->

```sh
layr person show --site "$LAYR_SITE" --json
```

<!-- recipe: cloud-remote -->

```sh
layr remote add "$LAYR_REMOTE" "$LAYR_CLOUD_URL"
layr remote list --json
```

If there is no local project yet, set `LAYR_PROJECT_COPY` to its new local name:

<!-- recipe: cloud-clone -->

```sh
layr clone "$LAYR_CLOUD_URL" "$LAYR_PROJECT_COPY"
```

This creates an `origin` remote; enter its line folder. For an existing
project, the remote must refer to that project's UUID. A pin mismatch is a stop.

## Open, update, and read a change

There is no `change open` command. Save a named state on your owned line and
push it; the store exposes its ChangeId as the change. `LAYR_MESSAGE` is your
English change title.

<!-- recipe: cloud-open -->

```sh
layr add -A
layr save -m "$LAYR_MESSAGE"
layr push "$LAYR_REMOTE"
layr change list --json
```

Read the ChangeId from `change list`; set `LAYR_CHANGE` to that full value.
After editing a fix, amend and push to update the **same** change:

<!-- recipe: cloud-update -->

```sh
layr add -A
layr save --amend -m "$LAYR_MESSAGE"
layr push "$LAYR_REMOTE"
```

Fetch signed records before reading remote checks or discussion:

<!-- recipe: cloud-status -->

```sh
layr fetch "$LAYR_REMOTE"
layr change show "$LAYR_CHANGE" --json
```

Read `versions[].state.id`, `versions[].checks` (`argv`, `command`, `exit`),
`versions[].attestations`, and top-level `comments` (`id`, `text`, `reply_to`,
`version`, `actor`, optional `path` and `line`). Check **the reviewed version**;
passing evidence for an older version does not approve the new one. An empty
`checks` array means no active result, not success. This is an explicit read at
a workflow boundary; never run it in a polling loop.

Reply using the comment ID from that JSON. `LAYR_REPLY` holds the reply text:

<!-- recipe: cloud-reply -->

```sh
layr comment "$LAYR_CHANGE" "$LAYR_REPLY" --reply-to "$LAYR_COMMENT" --json
layr push "$LAYR_REMOTE"
```

## Review, approve, and merge through the queue

The orchestrator fetches, inspects the exact state, and runs the required checks.
Use `layr show <state>` and `layr diff <base> <state>` for source review.
The reviewer must be an allowed approver distinct from the authors when policy
requires it. A worker never approves its own change to satisfy that requirement.

On a trusted machine whose local user is an allowed approver:

<!-- recipe: cloud-approve -->

```sh
layr approve "$LAYR_STATE" --json
layr push "$LAYR_REMOTE"
```

A person approval through the executor uses this alternative:

<!-- recipe: cloud-person-approve -->

```sh
layr request approve "$LAYR_STATE" --person --remote "$LAYR_REMOTE" --into main --json
```

This returns a request ID, not an applied approval. Retain that ID after a lost
response; do not create a second request to guess whether the first succeeded.
The trusted executor uses `layr requests apply --remote <remote> --json`, then
pushes its signed result. `change log <id> --json` includes `data.request_id`
and the resulting approval or refusal. Fetch and read that result before queueing.
Person comment and queue alternatives are `layr request comment <change> <text>
--reply-to <id> --person --remote <remote> --json`, `layr request queue-add <state>
--into main --person --remote <remote> --json`, and `layr request queue-remove
<entry> --person --remote <remote> --json`. They also need executor application.

Queue the reviewed exact state; later edits do not change this entry:

<!-- recipe: cloud-queue-add -->

```sh
layr queue add "$LAYR_STATE" --into main --json
layr push "$LAYR_REMOTE"
```

Read its ID into `LAYR_QUEUE_ENTRY`. To withdraw it:

<!-- recipe: cloud-queue-remove -->

```sh
layr queue remove "$LAYR_QUEUE_ENTRY" --json
layr push "$LAYR_REMOTE"
```

The trusted target-line owner or configured executor runs the queue. Run this
only on that machine, after fetching changes and approvals:

<!-- recipe: cloud-merge -->

```sh
layr fetch "$LAYR_REMOTE"
layr queue run --into main --json
layr push "$LAYR_REMOTE"
layr queue list --into main --json
```

`done` with `result` is merge evidence. `pending` is not completion; `failed`
includes a reason. Remove a failed entry before submitting a corrected state.
A configured executor can run automatically; do not run a second executor.
If the task uses Studio's local acceptance merge, inspect and accept the exact
submitted state through Studio too. A Cloud queue result does not complete a
Studio task automatically. Do not merge twice to recover an uncertain response.

Configure checks before you process the queue. The queue checks the exact merge result.
A queued entry keeps its source state if the worker line moves.
Conflicts or failed required checks leave main unchanged. Inspect each entry's result.
Require the named check on main with `layr protect main --check unit`.
Ordinary merges do not run probes. The queue runs probes only when `queue.probes` is true.

## Attach release files

Release publication needs explicit task authority. The release ref must already
name the approved state. The owner can create it with
`layr ref set "$LAYR_RELEASE" "$LAYR_STATE"`, then push. For an existing protected
ref, use its required policy and `--expect <old-state>`.

<!-- recipe: cloud-release -->

```sh
layr release attach "$LAYR_RELEASE" "$LAYR_ARTIFACT" --expect "$LAYR_STATE" --remote "$LAYR_REMOTE" --json
layr release files "$LAYR_RELEASE" --remote "$LAYR_REMOTE" --json
```

`--expect` binds the attachment to the reviewed release state. Output includes
file names, byte sizes, and SHA-256 values. The remote receives encrypted files.

## Find faults and review revisions

| Task                                            | Command                                                         |
| ----------------------------------------------- | --------------------------------------------------------------- |
| Find the first bad state, including checkpoints | `layr bisect run --auto --good <good> --bad <bad> -- <command>` |
| Inspect completed searches                      | `layr bisect list --json`                                       |
| Compare two versions of the change              | `layr change diff <change> --from v1 --to v2 --json`            |
| Run a configured behavior probe                 | `layr probe run <state> --probe <probe> --json`                 |
| Compare stored probe results                    | `layr probe diff <base> <state> --json`                         |

Bisect needs a command that passes at good and fails at bad. The fault must remain present after its first occurrence.
It uses temporary copies and can reuse exact-state results for the same command.

Change diff compares each version against its own parent. Base changes stay outside that comparison.
A change keeps its ID through amend, pick, and replay.

Define probes in the revision's `layr.toml` with `run`, `outputs`, and `kind`.
Run the probe for both states before you compare them.
Probe diff and change show read stored results. They do not run probe commands.

## Keep build output fast

Ignore regenerable folders in `.gitignore` or `.layrignore`. Add
`<folder> regenerable` to `.layrattributes` and save it, or use
`layr config regenerable` with `config` permission. `layr overlay list` identifies
cache folders. `layr overlay repair <overlay>` repairs a plain replacement folder.
`layr save --caches` gives new lines a warm cache copy.
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
Routing patterns, ephemeral overlays, and folder overlays are unavailable in the older bundled CLI.

## Studio development in lines

For Studio work in layr lines (warm `target/`, gate result reuse, disk cleanup),
read the "Developing in layr lines" section of `docs/testing.md` first.
Studio lines need layr 0.4.0 or later. Use the state ID from
`layr log --auto -n 1` to start a line with warm caches, not the line name.

## Diagnose a refusal and use a Git mirror

`layr access check <user> <action>` explains a refused action. Work in your own
line and preserve protected-line policy. A pin or credential error requires the
owner to fix provisioning; do not bypass it with another transport.

| Task                                      | Action                                             |
| ----------------------------------------- | -------------------------------------------------- |
| Another user owns the line                | Use your assigned line.                            |
| Main requires review                      | Review the exact state and use merge with expect.  |
| A merge has conflicts                     | Resolve them in an owned line before acceptance.   |
| The store cannot represent two path names | Use distinct names or a case-sensitive APFS store. |

A line keeps its `.git` folder read only (mode 0555). Git writes (commit, merge, reset,
checkout, stash) fail with `Permission denied`; Git reads work. Layr opens the folder
around its own writes. If Git history moves in a line anyway, layr refuses `save -m`,
`push`, `sync` and head moves, and names the commit to reset to.
`layr save` with no change prints the existing state ID. `layr --version` prints the
build id and date after the version.
In 0.3.0 the `.git` view follows the named line head for tools that read Git.
`layr git sync` explicitly refreshes that view without pushing.
`layr rev-parse '<rev>^{tree}'` names content across Layr and Git.
State IDs travel through Layr remotes; use full IDs for review.
When a Git mirror push is authorized, use `layr git push <remote> <branch>`
(or `layr git push --squash -m "<message>" <remote> <branch>`).
Read configured Git remotes with `layr git remote -v`.
In a Studio VM project with host sync, use the host bridge policy in the workspace reference.
A Git folder made by the line owner stays under that owner's control.
Git sync requires the line's Git mirror. A line made before Git setup can have no mirror.
Use layr for source changes. Tools can read the Git mirror.
The top-level push command sends to layr remotes. Use the Git bridge for a Git remote.
