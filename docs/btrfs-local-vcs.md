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
2. Platform: btrfs on Linux, and the Studio Linux VM on macOS
   ([Linux VM workspaces](linux-vm-workspaces.md)). APFS (macOS projects such as Xcode) is
   deferred. Windows is not supported.
3. Snapshot rules and metadata design: as in this document.
4. The workflow is fully agentic: the user does not read code or open an IDE. The main line of a
   project belongs to the lead. The lead accepts a worker result after its own review, merges it
   into the main line and resolves conflicts.
5. Export: one commit per task. The lead decides when a task goes to the remote.
6. For Linux projects on macOS, the main line in the VM is the source of truth. A one-way mirror
   writes it to a normal folder on the Mac (decision changed on 2026-10-09 from "the Mac folder is
   the source of truth", to remove two-way sync).
7. The current work continues: the image workspace engine ([image workspaces](workspace-images.md),
   overlayfs on Linux) and [Linux VM workspaces](linux-vm-workspaces.md) (git status change
   detector). The utility later replaces their internals behind the same public API.
8. No staged rollout: all parts of the utility are built together.
9. All agents run in the VM: the lead, workers and reviewers. The Mac is reached only through the
   general `host_exec` tool, for work that needs macOS (Xcode builds and tests, signing, simulators).
   This replaces the earlier decision of the same day to keep the lead on the Mac.

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

The agent skill and instructions teach `layr`. Its output uses git formats where a format exists, so
model habits carry over. Real git stays in each line: the git object store is a nested subvolume,
so tools that call `git rev-parse` or `git describe` keep working. A local commit made with real git
is harmless, because `layr` keeps the states.

| git verb                                 | Utility behavior                                                                                                                           | Cost               |
| ---------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ | ------------------ |
| `status`                                 | Snapshot the line, then `btrfs send --no-data -p <base state>`. Filter ignored paths with the `.gitignore` rules.                          | O(changes), 0.03 s |
| `diff`                                   | Unified diff only for paths in the change list.                                                                                            | O(changes)         |
| `add`, `commit`                          | `add` records paths in a staged set. `commit` saves a state: the full line, or the parent state plus staged paths as reflink copies.       | O(1) or O(staged)  |
| `log`, `show`                            | Read the state graph. `show <state>:<path>` reads the file from the state folder.                                                          | O(1) per record    |
| `checkout`, `switch`, `restore`, `reset` | A whole line: a new writable snapshot of the target state. One path: a reflink copy from the state. Save the line first (snapshot rule 2). | O(1) per path      |
| `stash`                                  | Save a state and restore the line.                                                                                                         | O(1)               |
| `branch`, `tag`                          | Create, list or delete lines. A tag is a named state.                                                                                      | O(1)               |
| `merge`, `cherry-pick`, `rebase`         | The merge engine (below) applies change lists onto the target line.                                                                        | O(changes)         |
| `blame`                                  | Walk the versions of one file through the state graph, on request.                                                                         | O(history of file) |
| `clean`                                  | Delete untracked, not ignored paths from the change list.                                                                                  | O(changes)         |
| `fetch`, `pull`, `push`, `remote`        | The remote bridge (below).                                                                                                                 | O(changes)         |
| other verbs                              | Not provided by `layr`. Real git keeps working in the line for tools that need it.                                                         | as git             |

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

## Main line on macOS

The main line of every project lives on btrfs in the Studio VM. All agents work in the VM, in
their own lines, with `layr` and Linux builds and tests running natively. No agent writes on the
Mac directly. The Mac gets two kinds of copies:

- the mirror of the main line, for the user (below);
- build slots, for `host_exec`.

### `host_exec`: macOS commands for agents in the VM

One general tool. Studio does not know what the command does.

1. Each agent that needs macOS gets a fixed slot on the Mac: a folder at a stable path, plus its own
   `DerivedData` and package folders next to it. A stable path keeps Xcode builds warm: Xcode does
   not reuse `DerivedData` at another path (measured in the research document).
2. Before the command, the utility syncs the agent line to the slot: the change list since the
   last synced state (`btrfs send --no-data`), then only those files. The first use copies the
   whole line.
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
host to the guest (`connect(toPort: 4050)` in `desktop/native/linux-vm/main.swift`). The guest side
needs either a Virtualization.framework listener for guest connections (`setSocketListener`) or
reverse requests on the existing connection, with request ids and receipts like the other guest
calls.

One-way mirror, VM to Mac:

- After every operation that changes the main line (merge, restore, import), the utility writes the
  changed files to a normal folder on the Mac. The change list comes from `btrfs send --no-data`,
  so the cost is O(changes).
- Each file is written to a temporary name and renamed into place. An intent log records the
  pending writes, so a crash resumes or rolls back the mirror update.
- The mirror records its main-line state id. A full verification (hash compare) runs in the
  background at a low frequency and repairs differences from the VM.

Edits made on the Mac:

- FSEvents watches the mirror folder. Files written by the mirror itself carry a token (path, size,
  modification time, hash) and are ignored.
- Any other change is a manual edit. Studio does not sync it automatically. It offers an explicit
  import: the merge engine merges the mirror changes into the main line, with the mirror state id as
  the base. The next mirror update then includes the result.

Ownership guarantees:

- The files are on the user's disk: the VM disk is a file in the Studio store, and the mirror is a
  normal folder.
- The mirror is readable without Studio, is backed up by Time Machine, and stays if Studio is
  removed.
- Every task also goes to the remote through the remote bridge.
- The VM disk is standard btrfs and can be opened by any Linux system.

Mirror rules for differences between btrfs and APFS:

- Names that differ only in letter case, or only in Unicode normalization, cannot coexist on APFS.
  The mirror reports them and writes neither, instead of losing one of them silently.
- The executable bit and symbolic links are mirrored. Hard links become copies. Linux extended
  attributes are not mirrored.

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

## Linux hosts without btrfs

| Option                               | How                                                                            | Root                                       | Capabilities                                |
| ------------------------------------ | ------------------------------------------------------------------------------ | ------------------------------------------ | ------------------------------------------- |
| **btrfs on a loop file**             | a sparse file in the Studio store, `mkfs.btrfs`, mounted through a loop device | once, at setup (a systemd mount unit)      | all btrfs functions                         |
| btrfs partition or disk              | a separate partition for the Studio store                                      | at setup                                   | all btrfs functions, no extra layer         |
| VM, as on macOS                      | KVM with QEMU, Firecracker or cloud-hypervisor                                 | no, but access to `/dev/kvm` (group `kvm`) | all, like macOS                             |
| ZFS host                             | ZFS snapshot, clone and `zfs send`                                             | delegation with `zfs allow`                | the same as btrfs, as a second backend      |
| ext4 or XFS without any of the above | overlayfs, as in the current workspace engine                                  | no                                         | layers only: no state history and no `send` |

Containers do not add btrfs. Docker and Podman on ext4 store layers with overlay2. A rootless
container cannot mount btrfs: the kernel allows only some file systems in a user namespace (for
example tmpfs, overlay and FUSE). A privileged container is the loop option with the rights of the
container engine.

With the loop option, the model is the same as on macOS: the main line lives in the btrfs store, and
a one-way mirror writes it to the user's folder on ext4 or XFS. Linux has no persistent change
journal like FSEvents, so manual edits in the mirror are detected with inotify while Studio runs and
with one `rsync` comparison after a restart (200,000 files: 1.2 s, measured in the Linux readiness
checks of the research document). They are imported only on an explicit request.

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

Recommendation: the loop file by default; the VM when root is not allowed but KVM is; overlayfs as
the reduced mode; a ZFS backend only when users on ZFS appear.

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

- The reverse guest to host channel for `host_exec`, and slot management (create, reuse, remove,
  disk budget).
- Index emulation for partial `add` (hunks, `add -p`).
- The first set of git verbs that `layr` implements.
- Directory-level merge cases: rename against change, folder delete against a new file inside,
  symlinks, file modes.
- The access model in the VM: agents own their lines; the `layr` service owns states, the log and
  metadata.

Measurements:

- `host_exec` with simulators, UI tests and a large Xcode project.
- The one-way mirror at chromium scale: first write, update after a merge, background verification.
- The loop-file option on a real ext4 host.

Later:

- Replace overlayfs with writable snapshots for Linux workspaces
  ([image workspaces](workspace-images.md) uses overlayfs today).
- APFS backend (deferred).
