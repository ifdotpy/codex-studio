# layr in the Studio VM

The VM stores projects at `/var/lib/codex-studio/layr` on btrfs.
The root systemd service runs `layr daemon`.
Agents use `/run/layr/layr.sock` as their own Linux users.
The separate root broker accepts connections only from the `studio` user.
It permits specific project, line, provider, and slot operations.
It rejects a repeated request ID with different parameters.
An unfinished request keeps its unknown outcome after a restart.

## Project import

The first VM chat imports the Mac folder through the existing tree upload protocol.
`project.import` creates the layr project and its main line.

The import takes code, not data or build output (`archive_source` in
`codex_linux_workspace_sync.py`). For each Git repository it takes the files that Git
tracks or would track, the files that Git ignores (such as `.env`), and the Git metadata.
It leaves on the Mac the folders that Git ignores (data, downloads, copies of other
projects), except a folder that is itself a nested repository, which keeps its own rules,
and regenerable folders (`REGENERABLE`, and `target` next to a `Cargo.toml`): in the VM a
build makes them again, or they come from a layr cache. `.worktrees`, a worktree of a project
repository in any other folder, and the Studio store stay too. A submodule whose store is in
its parent's `.git` comes with it; a Git store outside the project is refused. A folder
without Git is taken whole, without regenerable folders and without the folders that its
`.gitignore` names.
The project ID determines a Linux user for all leads of that project.
`project.ensure` returns the project ID, main path, user, home, and current state ID.
Later chats use the existing VM main line.
The Mac folder stays the working copy for native chats; Mac folder sync keeps it equal to
the VM main line ([vm-mac-sync.md](vm-mac-sync.md)).

`project.import` and `project.ensure` also set the project's layr rules once
(docs/btrfs-local-vcs.md, decision 23): main stays protected, but its owner, the
lead, may also commit in it; the `studio-agents` group, which holds every agent
user, may not change roles, replicate, change remotes, or import and export records. Workers own their lines; the lead merges a reviewed state with
`layr merge <line> --expect <state>`.

An explicit re-import requires a new request ID and the expected main state ID.
It creates an `import-*` line for review.
It does not replace the main line.
Use `layr export` when you need a normal copy outside the VM.

### Migration

The host command `codex_layr_migration.py` was removed with the Python backend.
The guest method `sync.mac.worktree` still imports a supplied worktree into a new line from main.
It keeps the worktree's code and ignored files, without `.git` or regenerable folders.
It leaves the Mac worktree unchanged.

### Operator commands

The host command `codex_layr_exec.py` was removed with the Python backend.
The guest method `project.exec` still runs a command as the owner of main.
It supports a subpath below main and a deadline of 300 seconds, with a maximum of 1800 seconds.
It returns at most 512 KiB of output and the exit code.
The broker keeps the arguments and output in `layr-admin/commands/<id>/`.
A retry with the same request ID returns the saved result.
Only the guest service can reach the broker.

### Warm caches

A `.layrattributes` file in the project gives paths attributes: a line is a pattern of the
`.gitignore` form and attributes. `node_modules regenerable` marks the folders that a build
makes again; `-regenerable` unsets it. layr turns a regenerable folder into a cache only if
the ignore rules ignore it (layr `docs/design.md`, section 5) and the head of the line has no
files in it. The import leaves out the ignored folders, so the attribute changes only what
becomes a cache in the VM:

1. The file is part of main; on the Mac it comes with Mac folder sync.
2. A build in main, from the lead or `project.exec`, makes the
   folders; `layr save --caches` in main gives them to the head of main. A later save on the
   same head keeps the caches that earlier saves gave it and adds its own: the record of each
   save names all cache folders of main, with their subvolume IDs. A head that lost caches
   to an older layr (the second seed below replaced the list) gets them back from the next
   `layr save --caches` in main.
3. Each new line from main starts with a copy-on-write copy of the nearest saved folders.
   Lines made before the save build their own.

The import keeps a nested repository and the files that Git tracks in an ignored folder.
Such a folder has versioned files, so it stays a plain folder in each line.
`layr save --caches` and `layr cache list` show `not a cache: has versioned files` for it.
To make it a cache, remove its files from history first.
A record of an older layr that lists such a folder as a cache does not change this: a new line
takes the folder from the state. `layr cache repair` (at each turn end) or the next
`layr save --caches` makes such a cache in a working folder a plain folder again.

Studio does not choose the build command or when a build is good enough to save.

### Seeded layers

Some regenerable folders cost hours to make again in the VM, such as a Chromium checkout with
its Git history and build output. `chromium regenerable seed` in `.layrattributes` makes the
first import send that folder from the Mac once, as a layer of main:

1. The new import records the plain paths (no patterns) marked `regenerable` and `seed` in
   `layr-projects/<id>/seeds.json`, before its receipt. Only these paths go: after the import,
   the VM main is the project, not the Mac folder. To seed a path later, re-import.
2. `seed_layers` (`codex_linux_vm_share.py`) makes a tar of the folder twice: first for its
   size and SHA-256, then for the upload, so the Mac keeps no copy. The tar has sorted names,
   no owners and no hard links; sockets and pipes stay out. An upload that stopped continues
   after the bytes that the guest has. A folder that changed between the two passes fails the
   SHA-256 check; the next attempt makes a new upload.
3. `project.seed` expands the upload once in the private upload folder, without the rsync
   copy of `upload.commit`; the root broker makes `main/<path>` a btrfs subvolume of reflinks
   of it, owned by the owner of main, and runs `layr save --caches` in main. A subvolume is
   already a layr cache, so the save makes no copy. Each seeded folder runs its own save, and
   each save keeps the caches of the earlier ones. The guest needs about twice the size of the
   folder on the data disk while the upload folder exists. An existing `main/<path>` is kept.
   The layer is an exact copy of the file modes: the extraction keeps the mode of each file
   (an ordinary upload gives each file owner read and write). The expanded folders keep owner
   rwx, so the guest service can remove them; the broker gives each folder of the copy its
   archive mode (`folder-modes.json` next to the expanded upload) before the owner of main
   gets the tree. A layer seeded before this rule can have other file modes than the Mac
   folder; host_exec then changes only the mode of such a file ([host-exec.md](host-exec.md)).
4. The sidebar shows the project as importing until the seed ends. A failed seed repeats
   alone on the next VM chat start. Each line from main then starts with a copy-on-write copy.

The layer needs the Mac folder only for that first send. Patches leave a line through Git
(the fork's remote or exported patch series), not through the layer.

### Compression

The data disk mounts with `compress=zstd:3`; provisioning sets the option on an existing disk
and remounts it. Only new writes are compressed. Do not recompress old data with
`btrfs filesystem defragment -c`: it unshares the blocks that states, lines and layers share.
On the codex-agents history (1,090 states of a 37 MB tree, one run each) the state data took
386 MB plain, 131 MB with zstd:1, 126 MB with zstd:3 and 115 MB with zstd:15, which wrote 40%
slower; zstd:1 and zstd:3 wrote as fast as plain. btrfs metadata (190 MB) does not compress.
In a 205 MB SQLite database zstd:1 made random updates 30% slower and reads equal.

### Sidebar readiness

On macOS, each project with a folder on this server has `layrReadiness` in its
project entity. A new chat binds the logical project, so its layr project ID is
the SHA-256 of that project ID (`project_layr_id` in `codex_vm_agents.py`, the
input of `project_id`). The state comes from the host records in
`linux-vm/layr-projects/<id>/` (`import_readiness`); Studio does not call the VM
or read the mount table:

- `ready`: `project.json` exists, so a layr chat starts without an import. The state also
  has `projectId`, the layr project that the layr menu acts on.
- `importing`: another holder has `import.lock`, and `project.json` is missing.
- `failed`: `import-error.json` has the reason of the last attempt, or an import
  receipt has no result after its attempt ended.
- `not_imported`: no record; the first layr chat imports the Mac folder.

The sidebar shows the LAYR chip only in the `ready` state, and only while this server has a
layr chat of the project (`has_layr_chats`): an imported project whose chats run natively gives
no layr information. The other states show nothing.

Each import attempt removes `import-error.json` when it gets the project lock and
writes it again if the attempt fails. The project entity changes when an attempt
gets the lock, when it ends, at backend start, and with each project write.
Linux servers send no `layrReadiness`.

## Reviewers

A reviewer line (role `reviewer`) is a read-only copy of the reviewed state: root owns it, its
btrfs subvolume is read-only, and an ACL lets the reviewer user read it. The reviewer reads other
states through layr, with its own Linux user:

- A Codex reviewer turn has no network access, so the Codex Linux sandbox refuses `connect` on
  every socket, also on `/run/layr/layr.sock` ("Operation not permitted"). Studio gives each VM
  reviewer the `layr_read` tool (`layr_read.rs` in `runtime-services`): it runs one read command
  of `COMMANDS` (`status`, `diff`, `show`, `log`, `rev-parse`, `merge-base`, `blame`, `grep`,
  `ls-files`), optionally after `--line <line>`, in the reviewer's line, as the reviewer's user,
  outside the sandbox (guest method `layr.exec`). `--project`, `-C` and `layr try` stay out:
  `try` runs any command. The guest request ID comes from the agent and the tool request, so a
  retry gets the first answer and the command does not run again.
- Every reviewer user is in the `studio-reviewers` group. Before a reviewer joins it, the broker
  adds the project's layr deny rule for that group (`REVIEWER_DENY` in `layr_agents.py`):
  `line.create`, `merge`, `line.manage`, `tag`, `push`, `config`, `gc` and `fsck`. A change inside a
  line belongs to its owner, and a reviewer owns no line. A Claude reviewer has no sandbox and
  reaches the socket itself; the deny rule applies to it too.
- The reviewer has a private writable folder, `~/.studio/scratch/<agent ID>` (mode 0700). Its
  processes get it as `TMPDIR`; the line record names it as `scratch`. A Codex reviewer turn uses
  the `workspaceWrite` sandbox with that folder as its only writable root. A reviewer of a guest
  without scratch folders keeps the `readOnly` sandbox.
- Acceptance of a reviewer's task result does not merge: a reviewer line has no changes.

## Task handoff

Each task that a VM lead creates gets a handoff folder,
`/var/lib/codex-studio/handoff/<project ID>/<task ID>` (guest method `handoff.ensure`). The task
records it as `handoff`, and the assignment event of the assignee names it. The lead puts files
for the assignee there: logs, timings, profile data, and copies of files from its line or home.

- Root owns `handoff/` (0711) and the project folder in it (group: the project's agents, 0750).
- The lead owns the task folder (group: the project's agents, mode 2750). Its default ACL gives
  the group read access to new files whatever the umask of the lead. A copy that keeps a private
  mode (`cp -p`) stays private; `chmod -R g+rX <folder>` opens it.
- Every agent of the project reads the folder; other projects' agents cannot pass the project
  folder. A Codex lead without full access gets the handoff folders as a writable root.
- The task ID of a create with a request ID comes from that ID, so a retry gets the same folder.
  When the guest cannot make the folder, the task has `handoffError` instead.
- Studio does not remove handoff folders. The lead removes the files that it no longer needs.

## Project secrets

Each layr project can have secrets: named values that every agent process of the project gets
as environment variables when it starts. The root broker keeps them outside every layr line.
An agent can read the variables it gets, so a secret stays out of the repository, its history,
backups and other projects, not out of reach of its own agents. A secret that you set reaches
the processes that start later. Set them with **Secrets…** in the LAYR menu or with
`codex_project_secrets.py`; see [project-secrets.md](project-secrets.md).

## Mac share

Samba serves each project's `main/` and `states/` folders.
It listens only on the guest loopback and accepts only the private `studio-view` credential.
The share never uses the VM network. The VM helper on the Mac listens on
`127.0.0.1:<port>` and carries each connection over vsock port 4052 to
`share_bridge.py` in the guest, which connects to Samba on the guest loopback.
macOS applies no Local Network permission to loopback, so the background backend
can mount the share without a permission for its Python.
The Mac saves the port once in `share-bridge.json`; if another program holds it,
the helper uses a free port, and `host.status` reports the port in use.
The Mac credential file has mode 0600 in the VM state directory.
No password appears in a process argument or a status response: Studio types it
on the controlling terminal that `mount_smbfs` reads.

The Mac mounts the share at `~/Studio/<name>` without root, where `<name>` is the
Mac project folder name in plain ASCII (`share.name`; the guest adds `-2`, `-3` for
another project with the same name). Finder shows the mounted share by this name.
The mount uses `nobrowse`: the folder is the view, and no `127.0.0.1` server appears
in the Finder sidebar. An older read-only mount of the same share at another address,
port, name or folder is unmounted, and its empty folder is removed; any other
filesystem there stays.
The Finder view is a convenience: when the mount fails, agents keep working in the VM,
and `mount_status` reports the failure with its reason.
The server and the Mac mount both reject writes.
The guest uses read-only bind mounts for the export paths.
The share rejects symbolic links and `.DS_Store` files.
It exposes no layr metadata or machine keys.
VM startup restores the guest exports and the registered Mac mounts.

`/api/desktop` reports the `linuxVm` status on macOS.
It includes the layr version, store size, disk space, guest share state, and Mac mount state.
Linux servers return `linuxVm: null`.

`GET /api/agents/finder-view?agent=<id>` tells where Finder shows the files of one chat.
A native chat returns its folder.
A VM chat returns the mount state of its project view: the `~/Studio` path when it is mounted,
or the saved failure reason when it is not.
The endpoint reads the saved records and the mount table only. It does not mount or call the VM.
The Project folder dialog uses it to reveal the read-only view in Finder.

## Source update

The repository contains the layr source at `workspaces/runtime/apps/layr`.
Its manifest records the base commit, its Git tree, and the SHA-256 hashes of the files.
The guest installer verifies those hashes before a build.
It uses Rust 1.90.0 and the committed Cargo lock file.
The source matches layr 0.2.0, commit `efa2fa142b44c278d264f59355d0d97d815fe5ba`.
It includes the `crates/layr-store` and `crates/layr-model` workspace members.
The layr-desk source keeps its existing revision.

Run this command to select another committed layr revision:

```sh
python3 workspaces/tooling/apps/repository-checks/update-vm-layr.py \
  --source /Users/igor/Projects/layr --revision efa2fa1
```

The helper is independent of the retired Python backend.
It removes only files named by the previous manifest.
The command reads the Git commit through `git archive`.
It does not read or change that repository's work files.
With `--name layr-desk`, the same command vendors the vault writer into
`workspaces/runtime/apps/layr-desk` ([vault.md](vault.md)).

Layr 0.2.0 removes the old command names. Use these commands:

- `layr line new <name> <state>` and `layr line delete -D <name>` manage lines.
- `layr save -m <message>` makes a named state.
- `layr save --force --note <text>` makes an automatic state, including unfinished changes.
- `layr rev-parse head` reads the named head. `head@{0}` reads the latest automatic state.
- `layr git push` exports to Git. `layr git sync` updates the local Git view.

Studio uses `status --json` for Mac sync and host command ignore checks.
The change lists use `staged`, `unstaged`, `untracked`, `conflicts`, and `ignored`.
JSON state IDs use UUID form. `rev-parse` prints the equivalent 32-character display form.
Ignored files do not make a line dirty.

After the updated payload reaches `/opt/codex-studio/vm`, the guest command to install it is:

```sh
sudo bash /opt/codex-studio/vm/guest/install-layr.sh
```

The installer verifies the manifest, builds with the pinned Rust toolchain, and restarts the layr daemon.
Run it only after VM commands finish.
The install keeps the existing store and its machine identity.

## Checks

Run each command separately:

```sh
STUDIO_GUEST_PYTHON=python3.13 node workspaces/runtime/apps/vm-guest/run-guest-suite.mjs
cargo test -j 4 --locked -p studio-runtime-services --lib layr_read::tests
```

The guest runner requires a built `codex-process-supervisor`.
Set `STUDIO_SUPERVISOR_BINARY` if its binary is outside the default Cargo target folder.

The layr tests run on Linux as root, with btrfs-progs and git (for example in an OrbStack
machine): `cargo test -j 4 --locked` in `workspaces/runtime/apps/layr`. The btrfs regression tests
(warm caches, regenerable folders, the git store) are in the upstream layr repository
(`tests/run.sh`); run them there as root before vendoring a new revision.

Live checks require the existing Studio VM on this Mac.
Back up its disks before guest changes.
Do not restart it while a provider is active.
The live check must include a non-root layr command, a rejected Mac write, and a restart.
