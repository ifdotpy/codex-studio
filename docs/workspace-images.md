# Image workspaces

Status: implementation contract. The workspace engine gives each agent a private copy of a
folder. Git supports base index refresh and staged-index deltas.

## Scope

- macOS uses an APFS clone of a base image and attaches it read-write with `--nobrowse`.
- Linux uses overlayfs. The lower layer is a frozen base version. The upper layer is private to
  one agent.
- Windows is not supported. Callers use the existing workspace fallback there.
- The engine does not add a process sandbox or change agent permissions.

## Copy behavior

The caller gives the folder root. It can be a Git repository or any other folder. The engine
copies the full folder contents, including `.git`, ignored files, object stores, and caches.
The only excluded paths are the workspace store when it is inside the root, and the root's
`.worktrees` directory. Git repositories are the root and nested folders with a `.git` entry.

The base builder copies the folder, then runs one change-detection pass before it seals the
base. Git repositories use `git diff --name-only -z <base HEAD> <current HEAD>` and
`git status --porcelain=v1 -z --untracked-files=all`. The engine also keeps paths that were
dirty when the base copy began. It stores each repository path, HEAD, dirty paths, and index
fingerprint in base metadata. Source Git reads set `GIT_OPTIONAL_LOCKS=0`.

Each agent starts from the base and uses the same detector. Git deltas copy or delete only
detected worktree paths. The engine also mirrors each repository's Git directory with
`rsync -a --delete`, excluding its `index`. This copies new objects, packs, refs, and `HEAD`.
If the source index fingerprint changed, it reads staged entries from the source and agent copies with
`git ls-files --stage -z`. It applies only changed entries with `git update-index --index-info`,
then copies their worktree paths and refreshes touched tracked paths. It does not run `git add`,
create commits, replay commits, or collect changes. These operations do not change the user's
Git metadata. Agent Git settings stay at their defaults.

Files ignored by Git stay at the base version when they change after base creation. Folders
without a root Git repository use `rsync -a --delete`, with the workspace store and root
`.worktrees` excluded. Nested repositories still receive Git index handling.

On macOS, the APFS clone keeps file inode and ctime data from the base volume. On Linux, the
overlay may keep or remap lower-layer inode data depending on filesystem and `xino` behavior;
measure this on the target filesystem before relying on first-status timing.

## Modules

| File                                   | Responsibility                                                        |
| -------------------------------------- | --------------------------------------------------------------------- |
| `scripts/codex_workspace_images.py`    | Public API, store, state, locks, base versions, and lifecycle.        |
| `scripts/codex_workspace_macos.py`     | APFS images, copy and delta operations, attach, detach, and size.     |
| `scripts/codex_workspace_linux.py`     | Base folders, `rsync` deltas, overlay mounts, detach, and size.       |
| `scripts/codex_runtime.py` and callers | Workspace selection, agent paths, archive, restore, and disk reports. |

## Store

The default store is `~/.local/state/codex-agents/workspaces`. Set
`CODEX_WORKSPACE_STORE` to use another location. macOS excludes the store from Time Machine.

- `bases/<folder-key>/` stores base versions and `base.json`.
- `agents/<agent-id>/` stores the agent layer and `agent.json`.
- `mnt/<agent-id>/` is the attached or mounted image. The copied folder is at `repo/`.
- Linux also stores namespace state in `linux-namespace.json`.

State files are JSON and use atomic writes. A base build is limited to one per folder. A failed
base can retry after five minutes. The caller can request an immediate retry with
`retry_failed=True`.

## Public API

- `supported(root) -> (bool, reason)` reports platform support.
- `base_status(root) -> {state, version, error}` reports the current base.
- `start_base_build(root, on_done=None, *, retry_failed=False) -> base_status` starts a build
  when needed. The callback receives the final base status. A ready base can refresh in the
  background when its change history needs a full copy.
- `create_workspace(root, agent_id) -> {mount, path}` creates and attaches a private copy. The
  returned `path` is `<mount>/repo`.
- `ensure_mounted(agent_id) -> {mount, path}` reattaches an archived workspace. It preserves
  the `creating` state after an interrupted create.
- `archive_workspace(agent_id) -> {freedBytes, state}` stops processes that hold the mount,
  detaches it, and keeps the image. The returned state is `archived`.
- `remove_workspace(agent_id, *, force=False) -> {freedBytes, state}` stops processes that hold
  the mount, detaches it, and deletes the image and state.
- `workspace_bytes(agent_id)` and `base_bytes(root)` report private bytes.
- `list_workspaces()` includes archived workspaces.
- `exec_prefix()` returns the command prefix needed to access a Linux mount. It is empty on
  macOS.

Archive is reversible: call `ensure_mounted` to attach the same copy again. Remove deletes it.
The `force` argument to remove is retained for API compatibility. Removal always stops mount
holders so it can safely delete the image.

## Runtime behavior

1. Multi-agent mode starts a base build for the caller's folder.
2. The agent gets a private workspace when a base is ready. Otherwise, the caller uses its
   existing fallback behavior until the base is ready.
3. The agent's working directory uses the returned `path` and the project's relative path.
4. The runtime archives a workspace when it must free the mount but keep the agent's files.
   Restore calls `ensure_mounted` and uses the same path.
5. Removing an agent calls `remove_workspace`.
6. Disk reports include base images and archived workspaces.

## Measurements

Use `tests/workspace-images-scale-macos.py` on a real Mac with 50,000 and 200,000 paths. Record
agent-start time, which includes read-only Git status in the source, and the first default-config
`git status` time with no staged source changes and after a source edit and `git add`. Run the
immediate-write test for Git and plain folders. The change detector does not depend on FSEvents.
For Linux, use a temporary OrbStack btrfs virtual machine and record inode/ctime behavior and
the same first-status cases. Delete the virtual machine after the measurements.

macOS runs on 2026-10-06, default Git settings:

| Tracked files | Case                      | Agent start | First `git status` |
| ------------: | ------------------------- | ----------: | -----------------: |
|        50,000 | No staged source change   |     7.256 s |            1.924 s |
|        50,000 | Source edit and `git add` |    11.884 s |            0.436 s |
|       200,000 | No staged source change   |    20.105 s |            4.156 s |
|       200,000 | Source edit and `git add` |    30.764 s |            1.676 s |

At 50,000 files, the base build took 70.961 s. Its parallel file copy took 14.953 s, and Git
index refresh took 35.164 s. The staged agent start included 0.207 s to clone, 0.890 s to
attach, 1.213 s
for source Git status, 0.570 s to copy changed paths, 1.488 s to sync Git metadata, 3.753 s
to apply and refresh index entries, and 1.947 s to restat touched paths. These timings overlap:
the detector total includes repository snapshots and source Git status. The source HEAD did
not change, so the HEAD diff command did not run. The 50,000-file fixture took 387.418 s to
create on a loaded host.

The 200,000-file base build took 597.191 s. Parallel file copy took 287.579 s, and Git index
refresh took 233.682 s. The staged agent start included 0.017 s to clone, 4.403 s to attach,
3.903 s for source Git status, 0.669 s to copy changed paths, 2.982 s to sync Git metadata,
9.962 s to apply and refresh index entries, and 5.202 s to restat touched paths. The detector
took 5.555 s and included the 4.797 s repository snapshot. The source HEAD did not change, so
the HEAD diff command did not run. The 200,000-file fixture took 1,536.880 s to create on a
loaded host.

Linux measurement, OrbStack Ubuntu 24.04, Git 2.43, btrfs, one run:

| Tracked files | No staged change | After source `git add` |
| ------------: | ---------------: | ---------------------: |
|        50,000 |          0.359 s |                0.014 s |
|       200,000 |          2.307 s |                0.122 s |

The test copied files into a btrfs base, refreshed its index, snapshotted the base, then mounted
an overlay workspace. The sampled file kept the same inode and ctime in the base, snapshot, and
overlay. The device number changed at each layer. The source and workspace `git status
--porcelain=v2` output matched in both staged cases.
