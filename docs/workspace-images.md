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

## Store

Default `~/.local/state/codex-agents/workspaces`, override `CODEX_WORKSPACE_STORE`.
macOS: `tmutil addexclusion` on the store.

- `bases/<repo-key>/` : base versions and `base.json` (repo root, version, git HEAD per repo,
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
- `start_base_build(repo_root) -> base_status` : starts a background build if no current base
  exists. Safe to call many times. Also refreshes a stale base when the delta is large.
- `create_workspace(repo_root, agent_id, *, start_commit=None) -> dict` with `mount`,
  `repoPath` (`<mount>/repo`), `branch` (`codex-agent/<id>`), `startCommit`, `snapshotCommit`
  (None when the user tree was clean). Steps: clone or overlay, mount, fresh user edits,
  `checkout -b`, snapshot commit. With `start_commit` different from the user's HEAD, the tree
  is reset to that commit instead (ignored build output stays).
- `ensure_mounted(agent_id) -> dict` : mount again after a restart or reboot.
- `collect(agent_id) -> dict` : fetch the agent branch into the user repository (nested
  repositories first) as `refs/studio/agents/<id>/raw`, then replay the commits after the
  snapshot onto the start commit with `git merge-tree` and `git commit-tree`. The result goes to
  `refs/heads/codex-agent/<id>` in the user repository, so the lead merges the same branch name as
  today. On a conflict the branch is not moved, and the result reports the conflict and the raw ref.
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
