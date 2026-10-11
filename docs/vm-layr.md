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
build makes them again, or they come from a layr layer. `.worktrees` and the Studio store
stay too. A folder without Git is taken whole, without regenerable folders.
The project ID determines a Linux user for all leads of that project.
`project.ensure` returns the project ID, main path, user, home, and current state ID.
Later chats use the existing VM main line.
The Mac folder stays the working copy for native chats; Mac folder sync keeps it equal to
the VM main line ([vm-mac-sync.md](vm-mac-sync.md)).

`project.import` and `project.ensure` also set the project's layr rules once
(docs/btrfs-local-vcs.md, decision 23): main stays protected, but its owner, the
lead, may also commit in it; the `studio-agents` group, which holds every agent
user, may not change roles, replicate, change remotes, import or export records,
or make backups. Workers own their lines; the lead merges a reviewed state with
`layr merge <line> --expect <state>`.

An explicit re-import requires a new request ID and the expected main state ID.
It creates an `import-*` line for review.
It does not replace the main line.
Use `layr export` when you need a normal copy outside the VM.

### Migration

`codex_layr_migration.py PROJECT_FOLDER...` (installed app scripts, Studio Python, VM
running) moves a project and its Git worktrees into layr:

- It imports the project with the code rule above, with Mac folder sync paused
  (`sync/config.json`); the user turns sync on per project.
- Each worktree of the project's repositories (in `.worktrees` at any depth or elsewhere)
  becomes a line `wt-<repository>-<folder>` that starts at main. The repository folder of
  the line takes the worktree's files: committed and uncommitted work, and files that Git
  ignores such as `.env`, without `.git` and regenerable folders (guest method
  `sync.mac.worktree`).
- The Mac worktrees stay. `layr-projects/<id>/worktrees.json` records each moved
  worktree with its line and state; a repeated run skips them. `--dry-run` lists what
  would move.

### Sidebar readiness

On macOS, each project with a folder on this server has `layrReadiness` in its
project entity. A new chat binds the logical project, so its layr project ID is
the SHA-256 of that project ID (`project_layr_id` in `codex_vm_agents.py`, the
input of `project_id`). The state comes from the host records in
`linux-vm/layr-projects/<id>/` (`import_readiness`); Studio does not call the VM
or read the mount table:

- `ready`: `project.json` exists, so a layr chat starts without an import.
- `importing`: another holder has `import.lock`, and `project.json` is missing.
- `failed`: `import-error.json` has the reason of the last attempt, or an import
  receipt has no result after its attempt ended.
- `not_imported`: no record; the first layr chat imports the Mac folder.

The sidebar shows the LAYR chip only in the `ready` state; the other states show nothing.

Each import attempt removes `import-error.json` when it gets the project lock and
writes it again if the attempt fails. The project entity changes when an attempt
gets the lock, when it ends, at backend start, and with each project write.
Linux servers send no `layrReadiness`.

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

The repository contains the clean layr source at `workspaces/runtime/apps/layr`.
Its manifest records the commit, Git tree, and SHA-256 hashes.
The guest installer verifies those hashes before a build.
It uses Rust 1.90.0 and the committed Cargo lock file.

Run this command to select another committed layr revision:

```sh
python3 workspaces/runtime/apps/server/src/update-vm-layr.py --source ~/Projects/layr --revision <commit>
```

The command reads the Git commit through `git archive`.
It does not read or change that repository's work files.

## Checks

Run each command separately:

```sh
python3 -B -m unittest discover -s workspaces/runtime/apps/vm-guest -p test_layr_admin.py -v
python3 workspaces/runtime/apps/server/src/codex_python.py --exec workspaces/runtime/apps/server/src/test_codex_linux_vm.py
python3 -B workspaces/runtime/apps/server/tests/linux-vm-layr-host-contract.py -q
npm run typecheck:runtime
```

Live checks require the existing Studio VM on this Mac.
Back up its disks before guest changes.
Do not restart it while a provider is active.
The live check must include a non-root layr command, a rejected Mac write, and a restart.
