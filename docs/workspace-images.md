# Workspace images: implementation contract

Status: in progress (2026-10-04). Design and measurements:
[agent workspace isolation](research/2026-10-03-agent-workspace-isolation.md), section
"Implementation spec". This file fixes the module boundaries and the shared interface.

## Scope

- Replace the git worktree of a new implementer with an image workspace.
- macOS: APFS clone of an ASIF base image, attached read-write with `--nobrowse`.
- Linux: overlayfs. The lower is a frozen base version (read-only btrfs snapshot, or a plain
  copy on other file systems). The upper is one folder per agent.
- Windows: not supported. Use the existing git worktree path there.
- Out of scope now: process sandbox, Seatbelt, separate users, EndpointSecurity, VMs.
  Agents keep today's permissions (`turn_permissions`).
- Existing agents with a ready git worktree keep it. The worktree code stays as the fallback
  when `supported()` is false.

## Modules and owners

| File                                  | Content                                                                                                    |
| ------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| `scripts/codex_workspace_images.py`   | Public API, store, locks, git steps (base git setup, snapshot, collect, protection refs). Platform neutral |
| `scripts/codex_workspace_macos.py`    | macOS backend: ASIF base, clone, attach, eject, FSEvents delta, private size                               |
| `scripts/codex_workspace_linux.py`    | Linux backend: namespace holder, overlay mount, base versions, rsync delta, size                           |
| `scripts/codex_runtime.py` and others | Caller integration: spawn, read-only phase, switch notice, collect, archive, disk                          |

The common module selects the backend by `sys.platform`. Backends implement the interface in
`codex_workspace_images.py` (the engine owner writes it first and shares it).

The common module owns Git setup, snapshots, protection refs, and collection. A backend implements
`current_event_id(repo_root)`, `open_base_staging(repo_root, repo_key, version)`,
`copy_base_tree(repo_root, destination, *, excludes)`, `seal_base(staging)`,
`remove_base_version(handle)`,
`clone_workspace(base_image, agent_dir)`,
`mount_workspace(layer, mount, *, base_image=None)`,
`sync_delta(repo_root, target_repo, token, *, excludes)`,
`unmount_workspace(mount, *, force=False)`, `remove_layer(agent_dir)`, `private_bytes(path)`, and
`exec_prefix()`. Base creation opens writable staging, copies the tree, then applies common Git setup
before sealing. Staging has `root`, `versionPath`, and `token`. The sealed result has `image`,
`versionPath`, and `token`. `excludes` contains paths relative to the repository root, including
`.worktrees` and the workspace store when it is inside the repository. Backend copies skip Git object
stores and base-copy `.git` metadata. Delta sync can copy updated Git config and index files. Common
builds a private standalone `.git` directory for each repository.
This also converts linked worktree and submodule `.git` files, so agent HEAD, index, and refs never
point into the user's Git metadata. Common refreshes each private Git config and reapplies alternates,
`core.checkStat=minimal`, and `core.trustctime=false` after each delta.

`sync_delta` returns `token`, repo-root-relative `changedPaths`, and `historyLost`. It may also
return `scanPaths` for folders that need a rescan, and `refreshBase` when history loss or a root
rescan requires a new base. The common module records nested repository paths and dirty paths during
the base build. A workspace start checks only those paths and delta paths. Collection uses the
repository list in `agent.json`; it does not walk the tree.

## Store

Default `~/.local/state/codex-agents/workspaces`, override `CODEX_WORKSPACE_STORE`.
macOS: `tmutil addexclusion` on the store.

- `bases/<repo-key>/` : base versions and `base.json` (repo root, version, nested repositories,
  dirty paths, object store exclusions, git HEAD per repo,
  delta token: FSEvents event id on macOS).
- `agents/<agent-id>/` : agent layer (`workspace.asif` on macOS, `u/` and `w/` on Linux) and
  `agent.json` (repo root, base version, snapshot commit, start commit, state).
- `mnt/<agent-id>/` : mount point. Volume layout: `repo/`, `home/`, `tmp/`.

`repo-key`: stable hash of the resolved repository root.
All state files are plain JSON, written atomically. Every operation is idempotent by agent id
or repo key, so a retry after a crash or a lost response adopts or completes the earlier work.

## Public API (codex_workspace_images.py)

- `supported(repo_root) -> (bool, reason)`
- `base_status(repo_root) -> {"state": "missing"|"building"|"ready"|"failed", "version", "error"}`
- `start_base_build(repo_root, on_done=None) -> base_status` : starts a background build if no
  current base exists. Safe to call many times. Also refreshes a stale base when the delta is
  large. `on_done(base_status)` runs once from the build thread when the build ends (ready or
  failed), or at once when the base is already ready or failed. No polling.
- `create_workspace(repo_root, agent_id, *, start_commit=None, restore_heads=None) -> dict` with `mount`,
  `repoPath` (`<mount>/repo`), `branch` (`codex-agent/<id>`), `startCommit`, `snapshotCommit`
  (None when the user tree was clean). The default start commit is the user's current HEAD.
  It syncs the user's current refs into private Git metadata, updates only paths committed since
  the base in the private index, then applies fresh edits.
  Steps: clone or overlay, mount, fresh user edits, reset or create the agent branch, snapshot commit.
  With `start_commit` different from the user's HEAD, the tree
  is reset to that commit instead (ignored build output stays).
  `restore_heads` maps repository paths from `agent.json` (for example `.` or `packages/lib`)
  to commits collected from those repositories. Restore checks out each commit without a snapshot.
- `ensure_mounted(agent_id) -> dict` : mount again after a restart or reboot.
- `collect(agent_id) -> dict` : fetch the agent branch into the user repository (nested
  repositories first) as `refs/studio/agents/<id>/raw`, then replay the agent commits onto the
  start commit (the snapshot's parent, or `startCommit` when there is no snapshot) with `git merge-tree` and `git commit-tree`. The result goes to
  `refs/heads/codex-agent/<id>` in the user repository, so the lead merges the same branch name as
  today. Each repository result includes its path, branch, and head. On a conflict the branch is
  not moved, and the result reports the conflicting path, conflict, and raw ref.
  The user's working tree and HEAD never change.
- `remove_workspace(agent_id, *, force=False) -> {"freedBytes", "state"}` : eject or unmount,
  stop processes that hold the mount (`lsof -t +f -- <mount>`, macOS) when `force`, delete the
  layer, drop protection refs that nothing uses.
- `workspace_bytes(agent_id)` and `base_bytes(repo_root)` : private bytes.
- `list_workspaces()` : for maintenance and disk reports.
- `exec_prefix()` : `[]` on macOS. On Linux the `nsenter` argv that joins the Studio namespace.
  Every process that must see an agent mount (provider processes, Studio git calls on agent
  paths) starts with this prefix.

## Runtime behavior

1. Multi agent mode turns on: `start_base_build(lead repo)`. An implementer spawn also calls it.
2. Implementer spawn when the base is not ready: the agent starts at once in the user folder with
   `sandboxPolicy: {"type": "readOnly"}` (and the Claude equivalent), whatever the YOLO setting.
3. Base ready: `create_workspace`, set the agent `cwd` to `repoPath/<relative project path>`,
   restore normal permissions, and deliver one notice (native delivery): new path, write access,
   snapshot commit. The new `cwd` applies from the next turn.
4. After each implementer turn: the existing checkpoint, then `collect`. The lead merges
   `codex-agent/<id>` as today.
5. Archive: `collect`, then `remove_workspace`. Restore: `create_workspace` and check out the
   saved branch.
6. Disk report and maintenance include image workspaces and bases.
