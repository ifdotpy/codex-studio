# Local version control on btrfs: design contract

Status: design, not implemented (2026-10-09). Owner of the decisions: the user. Technical advice
and prototypes: [agent workspace isolation research](research/2026-10-03-agent-workspace-isolation.md).

## Intent

Git and GitHub stay as the legacy layer for the remote: push, pull, pull requests, review and CI.
Everything local moves into a Studio utility built on btrfs: workspaces, history of agent work,
undo, merge into the user's folder, and transfer between machines. Local branches and worktrees are
no longer used.

The utility is a drop-in replacement for `git`. Agents and tools keep calling `git`. The utility
implements the commands on btrfs states and has equal convenience.

## Decisions (2026-10-09)

1. The utility is a drop-in `git` replacement with its own implementation.
2. Platform: btrfs on Linux, and the Studio Linux VM on macOS
   ([Linux VM workspaces](linux-vm-workspaces.md)). APFS (macOS projects such as Xcode) is
   deferred. Windows is not supported.
3. Snapshot rules and metadata design: as in this document.

## Data model

| Term         | Meaning                                                                                                                                                     |
| ------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| State        | A read-only btrfs snapshot plus a metadata record: id, parents, time, author (agent or user), description, agent turn. Replaces a commit.                   |
| Line         | A writable btrfs snapshot with an owner (agent or user) and a pointer to its current state. Replaces a branch and a worktree.                               |
| Operation    | One recorded action: save, merge, restore, import, export, delete. Every operation can be undone.                                                           |
| Project root | The project folder is a btrfs subvolume. Its git object store, if any, is a nested subvolume, so project snapshots and `btrfs send` never include git data. |

A project can have more separate layers: nested subvolumes for data that must be included or
excluded as a unit. The git object store is one. Regenerable data (for example `target/`,
`node_modules/`, `.venv/`) is another, so a policy can include it for a live agent move and
exclude it from long-term replication. A state records the snapshot of each layer.

A state is a browsable folder. Reading an old file, or searching old code with `rg`, needs no
checkout.

## Command line: drop-in `git`

Agents get the utility as `git` in `PATH`. Output uses git formats, so existing tools and model
habits work.

| git command                              | Utility behavior                                                                                                                                        | Cost               |
| ---------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------ |
| `status`                                 | Snapshot the line, then `btrfs send --no-data -p <base state>`. Filter ignored paths with the `.gitignore` rules.                                       | O(changes), 0.03 s |
| `diff`                                   | Unified diff only for paths in the change list.                                                                                                         | O(changes)         |
| `add`, `commit`                          | `add` records paths in a staged set. `commit` saves a state: the full line, or the parent state plus staged paths as reflink copies.                    | O(1) or O(staged)  |
| `log`, `show`                            | Read the state graph. `show <state>:<path>` reads the file from the state folder.                                                                       | O(1) per record    |
| `checkout`, `switch`, `restore`, `reset` | A whole line: a new writable snapshot of the target state. One path: a reflink copy from the state. Save the line first (snapshot rule 2).              | O(1) per path      |
| `stash`                                  | Save a state and restore the line.                                                                                                                      | O(1)               |
| `branch`, `tag`                          | Create, list or delete lines. A tag is a named state.                                                                                                   | O(1)               |
| `merge`, `cherry-pick`, `rebase`         | The merge engine (below) applies change lists onto the target line.                                                                                     | O(changes)         |
| `blame`                                  | Walk the versions of one file through the state graph, on request.                                                                                      | O(history of file) |
| `clean`                                  | Delete untracked, not ignored paths from the change list.                                                                                               | O(changes)         |
| `fetch`, `pull`, `push`, `remote`        | The remote bridge (below).                                                                                                                              | O(changes)         |
| any other command                        | Passthrough to real git on the hidden git mirror of the project, with a log entry. This keeps tools such as `git rev-parse` and `git describe` working. | as git             |

A switch of a line replaces the working subvolume with a rename. Processes that are already
running keep the old one.

## Snapshot rules

All snapshots are O(1). A change check is O(1): compare the subvolume generation number with the
generation at the previous snapshot.

1. At the end of every agent turn. This is the main undo point and a quiet point: the agent's tools
   do not run.
2. Before every utility command that changes the line (`checkout`, `restore`, `reset`, `merge`).
3. On `commit`: a named state with a description.
4. Every 10 minutes during a turn, only if the generation changed. This is the safety net for long
   turns. Such a state is marked "taken while processes ran", because it is crash-consistent only.

Retention:

- Named states stay while a line, the operation log, a tag or the export table refers to them.
- Automatic states: the last 50 per line, and one per hour for the last day.
- When a line is merged or archived, its automatic states are deleted.

## Metadata and replication

The source of truth is an append-only operation log. Everything else can be rebuilt from it.

```text
/studio/projects/<project-id>/
  states/<state-id>             read-only snapshots
  lines/<line-id>               writable snapshots
  meta/log/<machine-id>.jsonl   operation log, one file per machine
  meta/index.sqlite             query index, rebuilt from the logs
  meta/git/                     hidden git mirror for the remote bridge and passthrough
```

A log record contains: id (UUIDv7), machine id, operation, parent state ids, line id, btrfs
subvolume UUID, author, description, time.

Write order:

1. Create the snapshot.
2. Append the record and run `fsync`.
3. Update the index.

After a crash, only a snapshot without a record can exist. Cleanup deletes it. A record without a
snapshot cannot exist.

Replication between machines uses the paired server channel
([multiple Studio servers](multi-server.md)):

- Logs: each machine writes only its own file, so there are no write conflicts. Machines exchange
  missing records by per-machine sequence numbers. The union of all records is the full log.
- Data: for each state that the peer lacks, `btrfs send -p <latest common state>`. btrfs records
  `received_uuid` on the peer. The index maps that UUID to the state id.
- Regenerable layers: sent for a live agent move to another machine (warm builds). Not sent for
  backup or long-term replication; the peer rebuilds them.
- Divergence: when two machines continue one line, the graph gets two heads. The merge engine joins
  them, and the result is a normal merge operation in the log.

The log can only grow, so merging logs cannot corrupt them. The index can be rebuilt at any time.

Each record also stores the line pointers before and after the operation. States never change, so
undo of any operation only restores the previous pointers: O(1). Undo is itself a new record, so
it can be undone too.

## Merge engine

1. Find the common ancestor state in the graph.
2. Get the change list of each side against the ancestor with `btrfs send --no-data -p`. btrfs
   reports renames at inode level.
3. A path changed on one side only: take that side. A file is a reflink copy, so no data is copied.
4. A path changed on both sides: three-way content merge of text files (`merge-file` algorithm).
5. Conflicts: same region in both text versions, delete against change, and binary files changed on
   both sides. A merge never stops on a conflict. The result is a normal state that contains the
   conflicts: text files get conflict markers, and the state metadata lists each conflicted path
   with the ids of the base, ours and theirs versions. `git status` shows these paths as unmerged.
   A later state that resolves them clears the list. Work can continue on top of a state with
   conflicts.
6. The git object store is never merged as blocks. History goes through the remote bridge.

Cases still to handle: rename against change, folder delete against a new file inside, symlinks,
file modes, case-insensitive paths from macOS users.

## Exploration groups

Several agents can try the same task in parallel lines with a common base state. The group has an
epoch counter. When the user or the lead accepts one result, that line is merged and the epoch
increments. The other lines of the group become stale: the utility refuses to merge them and
offers to archive them. This is an optional policy for "best of N" work. Normal lines use the merge
engine.

## Remote bridge

- Import (`fetch`, `pull`): `git fetch` into the hidden mirror. The new state is a snapshot of the
  previous imported state plus the paths from `git diff --name-only <old> <new>`. Cost O(changes).
- Export (`push`, pull request): build the git tree from the tree of the previous export, with
  `git hash-object` only for changed paths, then `git commit-tree` and push to a branch that exists
  only on the remote. Ignored paths are never exported.
- A table maps state ids to git commits. The same state always gives the same tree, so an export
  can be repeated safely.

## Platform scope

| Platform               | States    | Change list             | Between machines |
| ---------------------- | --------- | ----------------------- | ---------------- |
| Linux, btrfs           | snapshots | `btrfs send --no-data`  | `send/receive`   |
| macOS, Studio Linux VM | snapshots | the same, inside the VM | the same         |
| macOS, APFS (deferred) | none yet  | none yet                | none yet         |

`btrfs send` and `receive` need `CAP_SYS_ADMIN`. The Studio VM has it. A bare Linux server needs a
privileged helper.

## Prototype results (2026-10-09)

Throwaway OrbStack machine, Ubuntu 26.04, kernel 7.0, btrfs-progs 6.17.1, root. Project: 200,000
small files, one 100 MiB binary file, 300 MiB of ignored build output, a git repository. A second
btrfs file system on a loop device was "machine B".

Merge prototype (git data inside the project subvolume):

| Step                                            | Result                                                                                       |
| ----------------------------------------------- | -------------------------------------------------------------------------------------------- |
| read-only snapshot of the live folder           | 9.16 s right after 200,000 new files (flush), 0.02 to 0.14 s in steady use                   |
| writable snapshot for the agent                 | 0.11 s                                                                                       |
| agent change list (`send --no-data`)            | 0.03 s, 142 changed, 8 deleted, equal to a full content walk                                 |
| user change list                                | 0.01 s, equal to a full content walk                                                         |
| `git status` / `rsync` dry run on the same tree | 0.13 s / 1.54 s                                                                              |
| merge into the user folder                      | 0.18 s: 66 applied, 5 merged, 3 conflicts (2 content, 1 delete/modify)                       |
| 100 MiB binary after the reflink copy           | 0 KiB exclusive data                                                                         |
| user git data after the merge                   | unchanged; `git fetch` of the agent commit works                                             |
| full send / receive of the base                 | 11.72 s / 30.09 s, 710 MiB                                                                   |
| incremental send / receive of the agent layer   | 0.24 s / 0.90 s, 132 MiB: 116.9 MiB git data, 15 MiB build output, 4,096 bytes of the binary |
| machine B continues the agent work              | content equal, git works, build output present                                               |

Git object store as a nested subvolume:

| Step                                             | Result                                                                                                                                     |
| ------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------ |
| convert `.git` to a subvolume after `git gc`     | 0.02 s, git works                                                                                                                          |
| project snapshot in steady use                   | 0.014 s; the snapshot shows an empty `.git` folder                                                                                         |
| agent start (project snapshot + `.git` snapshot) | 0.052 s; `git status` clean                                                                                                                |
| `btrfs send` of the agent layer                  | no operation under `.git`                                                                                                                  |
| incremental stream                               | 10 MiB (132 MiB with git data inside)                                                                                                      |
| machine B                                        | no `.git` placeholder at all; history by `git fetch` into a new repository and index from HEAD: 9.67 s; worktree equal to the agent commit |

## Prior art and what this design takes from it

| Project                                                                                         | What it does                                                                                              | Taken                                                                          |
| ----------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------ |
| Jujutsu (jj)                                                                                    | git-compatible; the working copy is always a commit; operation log with undo; conflicts stored in commits | undo by line pointers; conflicts as part of a state; snapshot before commands  |
| [BranchFS, branch context](https://arxiv.org/html/2602.08199) (March 2026)                      | FUSE copy-on-write branches for agents; first commit wins; epochs invalidate siblings                     | exploration groups with an epoch. Not taken: FUSE and file-level copy-on-write |
| [Pulumi Neo with Kopia](https://www.pulumi.com/blog/neo-kopia-workspace-snapshots/) (Sept 2026) | workspace snapshots instead of git for agents; regenerable caches are not saved                           | regenerable data as a separate layer with its own replication policy           |
| [BtrFsGit](https://github.com/koo5/BtrFsGit), [btrbk](https://github.com/digint/btrbk)          | git-like commands and incremental replication for btrfs subvolumes, for backups                           | finding the common parent by subvolume UUIDs for `send -p`                     |
| [Morph Infinibranch](https://cloud.morph.so/web/product/devboxes)                               | snapshot and branch of whole running VMs                                                                  | not taken: this design branches files, not processes                           |

No project found on 2026-10-09 combines a drop-in `git` replacement, btrfs states, a real merge and
replication between machines.

## Open items

- Index emulation details for partial `add` (hunks, `add -p`).
- Which git porcelain and plumbing commands the utility implements first, and which use passthrough.
- The passthrough repository needs an index for the current state; build it on demand and cache it
  per state.
- How the user's macOS folder syncs with its VM copy in both directions (FSEvents to the VM, changed
  files back).
- Directory-level merge cases listed above.
- Replace overlayfs with writable snapshots for Linux workspaces
  ([image workspaces](workspace-images.md) uses overlayfs today).
- APFS backend (deferred).
