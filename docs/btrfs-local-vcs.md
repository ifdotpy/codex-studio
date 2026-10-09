# Local version control on btrfs: design contract

Status: design, not implemented (2026-10-09). Owner of the decisions: the user. Technical advice
and prototypes: [agent workspace isolation research](research/2026-10-03-agent-workspace-isolation.md).

## Intent

Git and GitHub stay as the legacy layer for the remote: push, pull, pull requests, review and CI.
Everything local moves into a Studio utility built on btrfs: workspaces, history of agent work,
undo, merge into the user's folder, and transfer between machines. Local branches and worktrees are
no longer used.

The utility is called `layr`. Its commands follow the git verbs (`status`, `diff`, `merge`, `log`),
so models learn it quickly, and it has equal convenience. It does not take the name `git`.

## Decisions (2026-10-09)

1. The utility is `layr`, with git-like commands and its own implementation. It is not installed as
   `git` (changed on 2026-10-09 from a drop-in `git` replacement). Real git stays available for the
   remote bridge and for tools that call it.
2. Platform: btrfs in the Studio Linux VM on macOS. Layr on Linux hosts comes later
   ([Linux VM workspaces](linux-vm-workspaces.md)). APFS (macOS projects such as Xcode) is
   deferred. Windows is not supported.
3. Snapshot rules and metadata design: as in this document.
4. The workflow is fully agentic: the user does not read code or open an IDE. The main line of a
   project belongs to the lead. The lead accepts a worker result after its own review, merges it
   into the main line and resolves conflicts.
5. Export: one commit per task. The lead decides when a task goes to the remote.
6. The main line in the VM is the source of truth. The Mac sees it through a read-only network
   share. There is no mirror copy. `layr export` makes a normal copy on request, and backups use
   `btrfs send` and the remote (changed on 2026-10-09: first from "the Mac folder is the source of
   truth", then from a one-way mirror, to remove sync and the extra copy).
7. Chat creation selects `layr`, `image`, or `worktree`. On macOS, `layr` is the default.
   Layr runs the lead and all agents in the VM. Native image workspaces use macOS ASIF only.
   Native Git worktrees remain available. Linux servers use worktrees until host layr support arrives.
8. No staged rollout: all parts of the utility are built together.
9. All agents run in the VM: the lead, workers and reviewers. The Mac is reached only through the
   general `host_exec` tool, for work that needs macOS (Xcode builds and tests, signing, simulators).
   This replaces the earlier decision of the same day to keep the lead on the Mac.
10. Everything local goes through `layr`, reading commands included. The long-term ambition is a
    `layrhub` that replaces GitHub for these projects.
11. The read-only share also exposes `states/`, so the project history is browsable as folders.

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

## Command line: `layr`

The agent skill and instructions teach `layr`, and agents use it for all local work, reading
included (user decision 2026-10-09: "everything on layr"). Its output uses git formats where a
format exists, so model habits carry over. Real git stays in each line only for tools that call it
themselves (for example `git describe` in a build script) and for the remote bridge: the git object
store is a nested subvolume.

Priority comes from the git commands that agents ran on this Mac in 14 days (605 Codex and 30
Claude sessions, 108,811 shell commands, 23.3 percent with git):

| Verbs                                                                                                                     | Share of git calls | Cumulative |
| ------------------------------------------------------------------------------------------------------------------------- | -----------------: | ---------: |
| `diff` 23.7, `status` 18.1, `show` 12.0, `log` 8.8, `rev-parse` 8.1                                                       |              70.7% |      70.7% |
| `add` 3.9, `fetch` 3.7, `commit` 3.6, `worktree` 2.8, `merge-base` 2.1, `merge` 1.5, `branch` 1.5, `push` 1.3, `grep` 1.0 |              21.4% |      92.1% |
| about 30 more verbs, each below 1 percent                                                                                 |               7.9% |       100% |

Frequent forms: `diff --stat`, `--check`, `--name-only`, `--cached` and ranges; `status --short`
and `--branch`; `show <rev>`, `<rev>:<path>` and `--stat`; `log --oneline` and `--format`;
`rev-parse HEAD`, `--short` and `--show-toplevel`. The first set of `layr` covers the first group
with these forms, then the second group. `worktree` and `branch` map to lines.

| git verb                                 | Utility behavior                                                                                                                           | Cost                      |
| ---------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------- |
| `status`                                 | Snapshot the line, then `btrfs send --no-data -p <base state>`. Filter ignored paths with the `.gitignore` rules.                          | O(changes), 0.03 s        |
| `diff`                                   | Unified diff only for paths in the change list.                                                                                            | O(changes)                |
| `add`, `commit`                          | `add` records paths in a staged set. `commit` saves a state: the full line, or the parent state plus staged paths as reflink copies.       | O(1) or O(staged)         |
| `log`, `show`                            | Read the state graph. `show <state>:<path>` reads the file from the state folder.                                                          | O(1) per record           |
| `checkout`, `switch`, `restore`, `reset` | A whole line: a new writable snapshot of the target state. One path: a reflink copy from the state. Save the line first (snapshot rule 2). | O(1) per path             |
| `stash`                                  | Save a state and restore the line.                                                                                                         | O(1)                      |
| `branch`, `tag`                          | Create, list or delete lines. A tag is a named state.                                                                                      | O(1)                      |
| `merge`, `cherry-pick`, `rebase`         | The merge engine (below) applies change lists onto the target line.                                                                        | O(changes)                |
| `blame`                                  | Walk the versions of one file through the state graph, on request.                                                                         | O(history of file)        |
| `clean`                                  | Delete untracked, not ignored paths from the change list.                                                                                  | O(changes)                |
| `fetch`, `pull`, `push`, `remote`        | The remote bridge (below).                                                                                                                 | O(changes)                |
| `rev-parse`, `merge-base`, `grep`        | State ids, the line root, ancestry in the state graph, search in a state folder.                                                           | O(1) or O(files searched) |
| other verbs                              | Added by use frequency. Until then, real git works in the line for tools that need it.                                                     | as git                    |

A switch of a line replaces the working subvolume with a rename. Processes that are already
running keep the old one.

## Staging (index)

The index is a writable btrfs line next to the agent line, created as a snapshot of the parent
state.

- `add <path>`: a reflink copy from the agent line into the index line.
- Partial add: the agent passes the hunks as a patch (non-interactive form of `add -p`), and the
  utility applies it to the index copy of the file.
- `rm <path>`: delete in the index line.
- `diff --cached`: the change list from the parent state to a snapshot of the index line.
- `diff`: the change list from the index line to the agent line. Both lines changed some files
  independently, so the candidates are confirmed by content (only the candidates).
- `commit`: a read-only snapshot of the index line becomes the new state.

Rule: a `btrfs send` change list is a candidate list. Between two snapshots where the same path was
created independently (different inodes), confirm candidates by content.

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
  meta/git/                     git object store for the remote bridge
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

## Access model in the VM

- The `layr` service runs as root. It owns `states/`, `meta/` and the line folders: `states/` and
  `meta/` have mode `0700`, the `lines/` folder `0711` (no listing).
- Each agent has its own Linux user. It owns only its line (mode `0700`).
- An agent reads history only through `layr` (`log`, `show`, `diff`), not through the state paths.
- Creating, deleting and renaming lines and states is a `layr` operation.

A state snapshot keeps the owner of the line root, so its owner could clear the read-only flag if it
could reach the path. The `0700` root folder above `states/` is what prevents this, so it is a hard
rule.

## Main line on macOS

The main line of every project lives on btrfs in the Studio VM. All agents work in the VM, in
their own lines, with `layr` and Linux builds and tests running natively. No agent writes on the
Mac directly. The Mac gets two kinds of copies:

- build slots, for `host_exec`;
- a copy made by `layr export`, only on request.

The user views files through a read-only network share (below).

### `host_exec`: macOS commands for agents in the VM

One general tool. Studio does not know what the command does.

1. Each project has a pool of slots on the Mac. A slot is a folder at a stable path, with its own
   `DerivedData` and package folders next to it. A stable path keeps Xcode builds warm: Xcode does
   not reuse `DerivedData` at another path (measured in the research document).
2. An agent leases a free slot for a command. The slot records the state it holds. Before the
   command, the utility syncs the slot to the agent's current state: the change list between the
   two states (`btrfs send --no-data`, also between sibling lines of one base), confirmed by
   content, then only those files. The first use of a slot copies the whole state.
3. The command runs on the Mac in the slot, in the user's session, so GUI tools and simulators are
   available.
4. After the command, the Mac side lists files that the command changed in the slot (FSEvents or a
   compare with the start marker, on the Mac itself; the VM's view of a shared folder can have
   stale attributes). Changed sources go back into the agent line. Build outputs stay outside the
   slot folder. Artifacts are returned as files.
5. Output paths are mapped from the slot path to the line path in the VM, including the short
   forms of macOS path aliases (`/tmp` is `/private/tmp`). Slots should use a path without
   aliases.
6. One command at a time per slot. The agent waits for the result, so there is one writer.

The channel is a request from the guest to the host. The VM helper today only connects from the
host to the guest (`connect(toPort: 4050)` in `desktop/native/linux-vm/main.swift`).
Recommendation (not tested): reverse requests on the existing connection. The guest sends a request
frame with a request id; the host runs it and answers with a receipt, like the other guest calls.
This needs no new Virtualization.framework listener, keeps one connection to supervise, and reuses
the existing retry and receipt rules. A Virtualization.framework listener for guest connections
(`setSocketListener`) is the alternative.

Slot pool rules: a disk budget per project, least recently used slots are removed first, and a slot
that holds a state from an older base is reset by a full copy.

### Viewing from the Mac: read-only network share

- The VM serves the main line over SMB (convenient in Finder) or NFS (simpler and faster). The Mac
  mounts it without root.
- The share is read-only. Finder and other Mac programs cannot write into the main line, for
  example `.DS_Store` files.
- The server listens only on the VM's internal network interface, not on the local network.
- The share is for viewing single files. Builds on the Mac use `host_exec` slots, not the share.
- The share is available only while the VM runs.
- The VM also shares `states/` read-only, so every state of the project is a folder in Finder.

### Export and backups

- `layr export <folder>` writes a normal copy of a state to a folder on the Mac, for use without
  Studio or without the VM.
- Backups are files: one full `btrfs send` stream and incremental streams after it, written once
  into a backup folder on the Mac. Time Machine backs up that folder; the VM disk image itself is
  excluded from Time Machine. The same files can go to another machine or later to `layrhub`.
- Schedule: an incremental stream for every named state and at least hourly for the main line; a
  new full stream weekly. Streams are compressed with zstd. A manifest stores the SHA-256 of each
  stream.
- Retention: the last two full chains.
- Restore: check the manifest, receive the full stream, then the incremental streams in order.
  `btrfs receive` rejects a corrupted stream (CRC32C per command) and a stream whose parent is
  missing.

### Ownership guarantees

- The files are on the user's disk: the VM disk is a file in the Studio store.
- They are visible in Finder through the share and can be exported to a normal folder at any time.
- Every task is also on the remote.
- The VM disk is standard btrfs and can be opened by any Linux system.

### Name rules between btrfs and APFS

These rules apply to `layr export` and to `host_exec` slots:

- Names that differ only in letter case, or only in Unicode normalization, cannot coexist on APFS.
  The copy reports them and writes neither, instead of losing one of them silently.
- The executable bit and symbolic links are copied. Hard links become copies. Linux extended
  attributes are not copied.

## Review

| Step             | How                                                                                                                                                                      |
| ---------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| see the diff     | `layr diff <worker base>..<worker line>` in the VM: the btrfs change list plus a unified diff of those files                                                             |
| read whole files | directly in the worker state folder in the VM, or `layr show <line>:<path>`                                                                                              |
| run tests        | Linux: in a temporary writable snapshot of the worker line, deleted afterwards, so the worker line does not change. macOS: `host_exec` in a slot synced to that snapshot |
| decide           | accept: `layr merge <worker line>` into the main line. Return: a message to the worker through the existing Studio messages                                              |
| conflicts        | the merge result is a state with conflict markers and a conflict list. The lead fixes the files, and the next state clears the list                                      |

Reviewer agents use the same steps with read-only access. No branches, fetches or worktrees are
used for review.

## Merge engine

1. Find the common ancestor state in the graph.
2. Get the change list of each side against the ancestor with `btrfs send --no-data -p`. btrfs
   reports renames at inode level.
3. A path changed on one side only: take that side. A file is a reflink copy, so no data is copied.
4. A path changed on both sides: three-way content merge of text files (`merge-file` algorithm).
5. Conflicts: same region in both text versions, delete against change, and binary files changed on
   both sides. A merge never stops on a conflict. The result is a normal state that contains the
   conflicts: text files get conflict markers, and the state metadata lists each conflicted path
   with the ids of the base, ours and theirs versions. `layr status` shows these paths as unmerged.
   A later state that resolves them clears the list. Work can continue on top of a state with
   conflicts.
6. The git object store is never merged as blocks. History goes through the remote bridge.

Parsing rules for the change lists:

- `btrfs send` expresses a rename as `link <new> dest=<old>` followed by `unlink <old>`.
- Deleted and new entries pass through orphan names `o<inode>-<generation>-<sequence>`. A folder
  delete is a rename of the folder to an orphan name, then deletes inside it. The parser maps
  orphan names back to the real path.
- Many editors and `sed -i` write a new inode. A rename combined with such an edit is a delete and
  an add; only a plain move is detected as a rename.

Handled cases (all passed, see "Design experiments"): rename against change, folder delete against
a new file inside, symbolic links (ours only, theirs only, both), mode against content, file to
folder, add/add with the same and with different content, rename/rename, case-only rename, folder
delete, edits of different lines, delete against change.

## Exploration groups

Several agents can try the same task in parallel lines with a common base state. The group has an
epoch counter. When the user or the lead accepts one result, that line is merged and the epoch
increments. The other lines of the group become stale: the utility refuses to merge them and
offers to archive them. This is an optional policy for "best of N" work. Normal lines use the merge
engine.

## Remote bridge

- Import (`fetch`, `pull`): `git fetch` into the project git object store. The new state is a snapshot of the
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

## Linux hosts without btrfs

| Option                               | How                                                                            | Root                                       | Capabilities                           |
| ------------------------------------ | ------------------------------------------------------------------------------ | ------------------------------------------ | -------------------------------------- |
| **btrfs on a loop file**             | a sparse file in the Studio store, `mkfs.btrfs`, mounted through a loop device | once, at setup (a systemd mount unit)      | all btrfs functions                    |
| btrfs partition or disk              | a separate partition for the Studio store                                      | at setup                                   | all btrfs functions, no extra layer    |
| VM, as on macOS                      | KVM with QEMU, Firecracker or cloud-hypervisor                                 | no, but access to `/dev/kvm` (group `kvm`) | all, like macOS                        |
| ZFS host                             | ZFS snapshot, clone and `zfs send`                                             | delegation with `zfs allow`                | the same as btrfs, as a second backend |
| ext4 or XFS without any of the above | Git worktrees until host layr support arrives                                  | no                                         | Git history                            |

Containers do not provide btrfs storage. A loop file or a separate btrfs disk is still required.

With the loop option, the main line lives in the btrfs store on the same machine. The user can read
it at its path directly, or through a read-only bind mount at a convenient place. `layr export`
makes a normal copy on ext4 or XFS on request. There is no mirror and no sync.

Measurement, 2026-10-09: throwaway OrbStack machine (17 vCPU, 15 GB RAM), btrfs-progs 6.17.1, one
run, `drop_caches` before each cold case. The loop file lived on the machine's own btrfs root
(`nodatacow`), not on ext4.

| Operation (200,000 small files)             |  Native btrfs | Loop, direct I/O off | Loop, direct I/O on |
| ------------------------------------------- | ------------: | -------------------: | ------------------: |
| create files + `sync`                       |       19.63 s |               9.51 s |             12.28 s |
| walk with `stat`, cold / warm               | 1.30 / 0.45 s |        2.27 / 0.51 s |       5.10 / 0.59 s |
| `git init` + `add` + `commit`               |       18.25 s |              14.80 s |             16.32 s |
| `git status`, cold / warm                   | 0.56 / 0.15 s |        1.08 / 0.13 s |       0.89 / 0.14 s |
| `rg` over all files, cold                   |        1.45 s |               3.05 s |              2.40 s |
| read-only snapshot, first / after 100 edits | 0.11 / 0.18 s |        0.85 / 0.20 s |       0.81 / 0.16 s |
| change list (`send --no-data`)              |        0.01 s |               0.01 s |              0.01 s |
| writable snapshot                           |        0.11 s |               0.03 s |              0.10 s |
| write 1 GiB sequential + `fsync`            |        1.25 s |               3.52 s |              1.45 s |
| read 1 GiB sequential, cold                 |        0.50 s |               0.93 s |              0.70 s |
| reflink copy 1 GiB                          |        0.00 s |               0.00 s |              0.00 s |
| full `send` of the snapshot                 |       16.86 s |              14.36 s |             14.88 s |

Findings:

- The btrfs functions that the utility depends on (snapshots, change lists, reflink, `send`) cost
  the same on a loop file.
- Warm work costs the same.
- Cold reads cost about 1.7 to 2 times more on a loop file (walk, `git status`, `rg`). With direct
  I/O the cold walk is about 4 times slower, because small reads lose the host page cache.
- Direct I/O helps large sequential writes (1.45 s against 3.52 s).
- The faster file creation on a loop file is buffering in the host page cache, not faster storage.
- Default: direct I/O off, because cold metadata reads matter more for git and search than large
  writes. Measure again on a real ext4 host before release.

Future host options: a loop file with root setup, or a VM with KVM access.
Until host layr support arrives, Linux servers use Git worktrees.

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

## `host_exec` prototype (2026-10-09)

Throwaway OrbStack machine with btrfs. The OrbStack `mac` command stood in for the reverse channel,
and the OrbStack shared folder stood in for the file transfer. Project: a copy of
`CallScribe.xcodeproj` (52 files, 6 remote Swift packages). One run per step.

| Step                                                               | Result                                                             |
| ------------------------------------------------------------------ | ------------------------------------------------------------------ |
| call overhead (`mac true`)                                         | 0.05 s                                                             |
| first copy of the line to the slot                                 | 0.02 s                                                             |
| cold build with package resolve                                    | 92.15 s, once per slot                                             |
| build without changes                                              | 2.99 s                                                             |
| snapshot + change list + copy of 3 changed Swift files to the slot | 0.24 s                                                             |
| build after the 3 edits                                            | 4.10 s                                                             |
| a Mac command changes sources, copy back into the line             | 0.04 s + 0.02 s                                                    |
| compile error output                                               | path mapped to `/data/line/CallScribe/UI/MenuBarView.swift:179:19` |

Findings: the edit to build cycle is about 4.3 s, and about 0.3 s of it is the sync. Change
detection must run on the Mac: the VM's view of the shared folder returned a stale modification
time. The compiler printed `/tmp/...`, so path mapping must handle aliases. Not tested: large Xcode
projects, simulators and UI tests through `host_exec`, the real vsock channel.

## Toward layrhub

The ambition is a `layrhub` that later replaces GitHub for these projects. Requirements that apply
now:

- Global ids: state and record ids are UUIDv7.
- Signed records: each machine signs its log records with Ed25519, as Studio servers already do for
  multi-server requests.
- Transport by streams: clone is a full stream, pull and push are incremental streams. Clients that
  got their states from the hub share its lineage, so `send -p` works between them.
- A client without that lineage (an outside contributor, another hub) needs a fallback transport by
  content: the change list as files.
- Line ownership, review records and merge decisions are log records, so the hub can show and audit
  them.

## Design experiments (2026-10-09)

Throwaway OrbStack machines (Ubuntu, kernel 7.0, btrfs), root for the `layr` service, one run each.

Access model (agent `igor` against the store, other agent `agent2`):

| Action by the agent                                             | Result                                                                        |
| --------------------------------------------------------------- | ----------------------------------------------------------------------------- |
| write, delete and snapshot in its own line                      | allowed                                                                       |
| list or read states, clear read-only on a state, delete a state | denied                                                                        |
| remove `states/`, read or append the operation log              | denied                                                                        |
| read or write the other agent's line                            | denied                                                                        |
| delete or rename its own line subvolume                         | denied                                                                        |
| `rm -rf /studio` as the agent                                   | states, log and the other line intact; the damaged line restored from a state |

Staging on 200,000 files:

| Step                             | Time                                      |
| -------------------------------- | ----------------------------------------- |
| index line from the parent state | 0.013 s                                   |
| add a file, a new file, a delete | 0.001 to 0.002 s each                     |
| partial add of one hunk by patch | 0.003 s                                   |
| `diff --cached` / `diff`         | 0.017 s / 0.008 s + 0.005 s content check |
| commit (read-only snapshot)      | 0.025 s                                   |

The commit contained exactly the staged first hunk; the second hunk stayed in the line.

Merge cases: 14 of 14 passed, one merge took 0.16 s.

Backups on 200,000 files plus a 50 MiB binary file:

| Step                                                                      | Result                                   |
| ------------------------------------------------------------------------- | ---------------------------------------- |
| full stream                                                               | 15.01 s, 169.1 MiB (64.1 MiB with zstd)  |
| 5 incremental streams of 100 edited files (one also 64 KiB of the binary) | 0.03 to 0.07 s each, up to about 0.1 MiB |
| restore: full + 5 incremental streams on another btrfs                    | 46.13 s + 4.87 s, content equal          |
| corrupted stream / stream with a missing parent                           | rejected / rejected                      |

`host_exec` slot pool (`CallScribe` copy, OrbStack `mac` as the channel):

| Step                                                   | Sync            | Xcode build |
| ------------------------------------------------------ | --------------- | ----------- |
| empty slot: full copy and cold build                   | 0.05 s          | 63.20 s     |
| slot to agent A                                        | 1 file, 0.28 s  | 8.59 s      |
| slot from agent A to agent B (sibling line, same base) | 3 files, 0.26 s | 9.72 s      |
| slot from agent B back to agent A (A changed again)    | 3 files, 0.17 s | 8.63 s      |
| same state again                                       | 0 files, 0.05 s | 4.58 s      |

The slot held exactly the target state after each switch.

## Prior art and what this design takes from it

| Project                                                                                         | What it does                                                                                              | Taken                                                                          |
| ----------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------ |
| Jujutsu (jj)                                                                                    | git-compatible; the working copy is always a commit; operation log with undo; conflicts stored in commits | undo by line pointers; conflicts as part of a state; snapshot before commands  |
| [BranchFS, branch context](https://arxiv.org/html/2602.08199) (March 2026)                      | FUSE copy-on-write branches for agents; first commit wins; epochs invalidate siblings                     | exploration groups with an epoch. Not taken: FUSE and file-level copy-on-write |
| [Pulumi Neo with Kopia](https://www.pulumi.com/blog/neo-kopia-workspace-snapshots/) (Sept 2026) | workspace snapshots instead of git for agents; regenerable caches are not saved                           | regenerable data as a separate layer with its own replication policy           |
| [BtrFsGit](https://github.com/koo5/BtrFsGit), [btrbk](https://github.com/digint/btrbk)          | git-like commands and incremental replication for btrfs subvolumes, for backups                           | finding the common parent by subvolume UUIDs for `send -p`                     |
| [Morph Infinibranch](https://cloud.morph.so/web/product/devboxes)                               | snapshot and branch of whole running VMs                                                                  | not taken: this design branches files, not processes                           |

No project found on 2026-10-09 combines a git-like local tool, btrfs states, a real merge and
replication between machines.

## Open items

Design:

- The reverse guest to host channel: implement and measure the recommended reverse requests.
- The `layrhub` protocol: signed records, stream transport, the fallback transport by content.

Measurements:

- `host_exec` with simulators, UI tests and a large Xcode project.
- The read-only share: SMB against NFS for Finder use on a large tree, and the `states/` share.
- The loop-file option on a real ext4 host.

Later:

- Layr on Linux hosts.
- APFS backend (deferred).
