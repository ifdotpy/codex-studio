# Image workspaces

Status: implementation contract. The workspace engine gives each agent a private copy of a
folder. Git supports base index refresh and staged-index deltas.

## Scope

- macOS uses an APFS clone of a base image and attaches it read-write with `--nobrowse`.
- Image workspaces require macOS ASIF. Linux servers use Git worktrees.
- Layr chats run the lead and all agents in the VM on owned lines.
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
fingerprint in base metadata. It also fingerprints Git metadata outside object storage and
indexes. The index fingerprint uses its device, inode, size, modification time, and change time.
Git replaces its index atomically, so this check does not read the index file. Source Git reads
set `GIT_OPTIONAL_LOCKS=0`.

Each agent starts from the base and uses one source repository snapshot for status, index, and
Git metadata checks. It finds new nested repositories from dirty directory paths. Git deltas
copy or delete only detected worktree paths. The engine skips
index entry reads when the index fingerprint matches. When it changes, the engine reads only
candidate paths from the source and agent indexes, then applies changed entries with
`git update-index --index-info`. It copies missing staged objects by object ID. It mirrors each
Git directory with `rsync -a --delete`, excluding `index`, only when Git metadata changes. It
then refreshes touched tracked paths whose index entries did not change. Git refreshes copied
staged paths during the first status. It does not run `git add`, create commits, replay commits,
or collect changes. These operations do not change the user's Git metadata. Agent Git settings
stay at their defaults.

Files ignored by Git stay at the base version when they change after base creation. Folders
without a root Git repository use `rsync -a --delete`, with the workspace store and root
`.worktrees` excluded. Nested repositories still receive Git index handling.

The APFS clone keeps file inode and ctime data from the base volume.

## Modules

| File                                                              | Responsibility                                                        |
| ----------------------------------------------------------------- | --------------------------------------------------------------------- |
| `workspaces/runtime/apps/server/src/codex_workspace_images.py`    | Public API, store, state, locks, base versions, and lifecycle.        |
| `workspaces/runtime/apps/server/src/codex_workspace_macos.py`     | APFS images, copy and delta operations, attach, detach, and size.     |
| `workspaces/runtime/apps/server/src/codex_runtime.py` and callers | Workspace selection, agent paths, archive, restore, and disk reports. |

## Store

The default store is `~/.local/state/codex-agents/workspaces`. Set
`CODEX_WORKSPACE_STORE` to use another location. macOS excludes the store from Time Machine.

- `bases/<folder-key>/` stores base versions and `base.json`.
- `agents/<agent-id>/` stores the agent layer and `agent.json`.
- `mnt/<agent-id>/` is the attached or mounted image. The copied folder is at `repo/`.

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
- `exec_prefix()` returns an empty list for macOS ASIF workspaces.

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

The scale benchmark was removed and is available at commit `3507feea`.

macOS runs on 2026-10-06, default Git settings:

| Tracked files | Case                      | Agent start | First `git status` |
| ------------: | ------------------------- | ----------: | -----------------: |
|        50,000 | No staged source change   |     2.610 s |            0.983 s |
|        50,000 | Source edit and `git add` |     3.757 s |            0.844 s |
|       200,000 | No staged source change   |    10.104 s |            4.661 s |
|       200,000 | Source edit and `git add` |    10.212 s |            4.013 s |

The scale test ran on a loaded Mac. At 50,000 files, the base build took 50.419 s. File copy
took 17.730 s, and index handling took 12.352 s. Staged agent start took 3.757 s: attach
1.005 s, source Git status 0.953 s, path copy 0.443 s, index entry reads 0.167 s, and index
delta handling 1.080 s. The first status took 0.844 s. The clean start took 2.610 s, and its
first status took 0.983 s. The fixture took 175.540 s.

At 200,000 files, the base build took 361.245 s. File copy took 87.022 s, and index handling
took 223.949 s. The remaining base steps included repository discovery 6.323 s and source Git
status 11.080 s. These timings overlap. Staged agent start took 10.212 s: attach 2.203 s,
source Git status 5.365 s, path copy 0.618 s, index entry reads 0.610 s, and index delta
handling 1.857 s. The first status took 4.013 s. The clean start took 10.104 s, and its first
status took 4.661 s. The fixture took 823.811 s. Neither staged start mirrored the Git
directory or restatted paths. The HEAD did not change, so no HEAD diff ran.
