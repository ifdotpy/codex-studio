# Image workspaces

Status: implementation contract. The workspace engine gives each agent a private copy of a
folder. It does not interpret Git or other folder contents.

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
`.worktrees` directory.

The base builder records a change token before it copies the folder. It then applies changes
from that token to the copy. It repeats this pass so changes during the first copy are included.
Platform backends handle file events or use `rsync`. They do not inspect Git data.

An agent workspace starts from the base and receives the changes since the base token. The
result matches the source folder at the end of the sync passes, except for the excluded paths.
The copied `.git/index` keeps its original stat data. The agent's first Git command can recheck
files after the copy.

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

Record these values on macOS for both 50,000 and 200,000 paths:

- Time for `create_workspace` to return.
- Time for the first `git status` inside the agent after editing one file in the source folder
  and running `git add` there.

The benchmark uses the copied index as-is. Do not add a Git-specific workaround.
