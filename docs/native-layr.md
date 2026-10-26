# Native Layr workspaces

The `layr-native` workspace mode runs agents on the host.
The chat selector calls this mode **Layr**.
**Layr VM** uses `layr` and runs in the Studio Linux VM.
**WT** uses Git worktrees.

## Store and installation

Studio owns one host store at `<Studio state>/layr-native/store`.
`NATIVE_LAYR_DIRECTORY` in `runtime-store/src/path.rs` owns the installation directory.
`STORE_DIRECTORY` in `layr_native.rs` owns the store subdirectory.
The state directory must stay outside project folders.
The project ID is the SHA-256 of the canonical source folder path.
Later chats for that folder reuse its main line.

Studio installs Layr 0.2.2 at `<Studio state>/layr-native/bin/layr`.
It never selects `~/.cargo/bin/layr` for runtime operations.
The installer verifies the vendored source manifest and runs Cargo with the lock file and four jobs.
Build output stays below the Studio state directory.
The first native turn installs the binary if it is absent.
To install it before a chat, run:

```sh
studio-runtime-server native-layr install --state /path/to/studio-state
studio-runtime-server native-layr paths --state /path/to/studio-state
```

Before the vendored update is available, select the approved upstream source explicitly:

```sh
studio-runtime-server native-layr install --state /path/to/studio-state \
  --source /Users/igor/Projects/layr
```

The host must use APFS on macOS or btrfs on Linux.
The new chat selector reads `nativeLayr.ready` and `nativeLayr.reason` from `/api/ui-summary`.
Linux defaults to WT unless its native capability is ready.
The probe requires a btrfs store and a listening root daemon at `<Studio state>/layr-native/layr.sock`.
On macOS, commands run as the Studio user with `LAYR_ROOT` set to the owned store.
`LAYR_DIRECT=1` selects the macOS backend without another service.
On Linux, Layr requires a root daemon for btrfs operations.
Studio sets `LAYR_DIRECT=0` and `LAYR_SOCKET=<Studio state>/layr-native/layr.sock` for Linux commands and agent shells.
The daemon checks the Studio user's identity for each command.
It uses the same Studio-owned store.
Studio does not modify mounts or install the root service.

Install the host binary first.
On Linux, start its daemon with the paths from `native-layr paths`:

```sh
sudo env LAYR_ROOT=/path/to/studio-state/layr-native/store \
  LAYR_SOCKET=/path/to/studio-state/layr-native/layr.sock \
  /path/to/studio-state/layr-native/bin/layr daemon
```

The daemon must run before a native chat or migration on Linux.
Version checks of an installed Linux binary also use this daemon.
macOS does not require this step.

## First import

The native import includes the whole project folder, including Git-ignored data and `.env` files.
These files enter Layr history in the first main state.
The VM archive rules remain unchanged.

Studio selects all paths and rejects case or Unicode collisions before it writes imported entries.
It sorts path references and checks sibling names once per directory.
The validator retains tables only for the current directory and its parents.
It makes a staging tree outside the checkout with APFS clones on macOS or btrfs reflinks on Linux.
It runs `layr init <project> --from <folder> --include-ignored`.
It then permits the owner to save directly in protected main.
No tar archive is created.

Build output becomes a warm Layr cache outside history.
The shared build-folder owner is `vm_preparation::regenerable_path`.
It covers `node_modules`, `.venv`, `venv`, `__pycache__`, `.pytest_cache`, `.mypy_cache`, `.ruff_cache`, `.turbo`, and `.next`.
A `target` folder becomes a cache only beside `Cargo.toml`.
Studio omits cache content from the initial state, registers each cache, clones its files, and saves the warm caches.
New lines receive these caches without a rebuild.

Studio appends `!*` to a cloned `.layrignore` beside each ignore file and at each cache parent.
It then adds rules that ignore the cache folders again.
The source ignore files remain unchanged.
This makes ignored data visible to Layr while build output remains a cache.

Layr creates its own root `.git` bridge.
Studio retains the complete source `.git` tree in `.studio-native-import/git`, including local refs and unpushed objects.
A source that already contains `.studio-native-import` is refused.
Nested `.git` directories enter history.
An invalid Git marker remains plain versioned data, with all its bytes preserved.
The `plainGitMetadata` report lists each path and the reason. Studio does not read its repository status.
Internal submodule pointers and `core.worktree` use the cloned metadata.
Git pointers, common directories, and object alternates inside the whole project are admitted.
This boundary also applies when one imported worktree refers to another repository in the project.
Stores outside the project cause refusal. The report lists their paths.
An operator can select `native-layr migrate --without-git <project>` to import that project as a plain folder.
This option omits only the top-level `.git`, without a backup. Nested repositories retain normal admission.
The report and import receipt record this choice. The source folder remains unchanged.
The option cannot change a completed import that retained Git metadata.

Studio excludes registered linked worktree folders from main.
Ordinary files and folders under `.worktrees` remain versioned data.
The `worktreeData` report lists them.
The migration command imports their files into separate lines.
It includes their ignored data and warm build caches.
The `cachePathChanges` report lists paths whose role differs between main and an imported line.
On APFS, a line can keep versioned files at a path that is a cache in main.
Main retains its cache. The imported line reports `not a cache: has versioned files`.
On btrfs, this case still causes refusal because the inherited cache is a subvolume.
It needs a future `layr cache materialize` command. Native Linux has structural checks only on this Mac.
Use `--skip-worktree <path>` to omit an operator-selected temporary worktree. The report records the skipped path.
The original worktree folder remains unchanged.

The free-space preflight counts metadata, cross-volume copies, Git fetch output, and Git import allocations.
Reports separate these costs as `metadataBytes`, `transientMetadataBytes`, `crossVolumeBytes`, `gitTransferBytes`, and `gitHeadBytes`.
The metadata allowance is 2,862 bytes per entry, including directories.
It is twice the larger measured peak per entry, rounded up to a byte.
The APFS cache fixture has 4,096 files and 4,161 entries.
Its net free-space delta is 3,297,280 bytes. Its peak is 4,079,616 bytes, or 980.44 bytes per entry.
Every source and main cache clone ID matches.
The current migration fixture has 4,096 versioned files, 4,096 cache files, and 8,517 source entries.
A new line costs 3,092,480 bytes net and 3,960,832 bytes at peak.
A migrated line costs 11,362,304 bytes net and 12,185,600 bytes at peak.
These are 363.09 and 1,334.07 net bytes per entry, respectively.
The migrated-line peak is 1,430.74 bytes per entry. The allowance is twice this value, rounded up.
The temporary line allowance is 931 bytes per main entry, twice its measured peak per entry.
Layr 0.2.2 uses the APFS whole-directory clone call when hard links and nested layers do not require a fallback.
The measured fixture has neither condition. The directory clone still needs metadata for its new tree.
The measured bytes per entry do not show a file-by-file fallback.
The estimate charges only the part of that temporary allowance above the final line's metadata allowance.
For a nested repository, each line also retains the other project folders from main.
Their entries receive the same metadata allowance and appear as `inheritedEntries` and `inheritedMetadataBytes`.
The report separates the actual main counts as `mainSize` and the original source counts as `sourceSize`.
If native main exists, its actual tree determines each line clone cost.
Same-volume clones add no payload bytes to this estimate.
The estimate follows the required host Layr version.
Layr 0.2.1 requires a full HEAD allowance and an allowance for copies into existing HEAD files.
From 0.2.2, matching regular source files can share blocks with HEAD.
The estimate reads source blob hashes without filters or Git writes to identify files that can share blocks.
Missing files, modified files, and symlinks require new HEAD bytes.
A margin retains at least 64 MiB or ten percent of the estimate, whichever is larger.
A same-volume clone failure causes refusal, without a byte-copy fallback.
Cross-volume files require a byte copy and sufficient space before import.
Keep the source quiet during import. Studio checks its identities before and after the clones.
Source Git reads set `GIT_NO_LAZY_FETCH=1` and `GIT_OPTIONAL_LOCKS=0`.
Missing partial-clone objects cause an unknown-size refusal, without fetching into the source.
Clears use directory descriptors and refuse symlink ancestors. They unlink links without visiting their targets.
A copied submodule config link becomes a regular private file before any edit.
Attribute copies use descriptors or no-follow link calls on APFS and btrfs.
Full-tree snapshots preserve empty directories, directory modes, and file mode changes in each imported head.
Capacity checks run before the pending receipt. A shortage can retry with the same request after capacity increases.

Later chats use the existing main line.
They do not copy changes from the original folder.

The native main line is the source of truth for these chats.
The original Mac folder remains separate.
To export a normal folder or send reviewed work to a Git remote, run from main:

```sh
layr export /path/to/new-copy
layr git push origin main
```

A push still requires the user's authorization.

## Agents and tasks

The lead uses `projects/<project>/lines/main` in the host store.
An implementer uses `agent-<SHA-256 of agent ID>`.
Studio creates this line with `layr line new <name> main`.
Workspace readiness and cleanup use the existing runtime hooks.
The agent record has `workspaceMode: layr-native`, `executionMode: native`, and `layrReady: true` after preparation.
`layrProjectId`, `layrLine`, `layrStateId`, and `cwd` identify the association.

Use a named state for task submission:

```sh
layr add path/to/file
layr save -m 'Complete the task'
layr rev-parse head
```

Submit the full state ID.
Studio rejects a prefix or a state that is not the worker's named head.
Acceptance runs `layr merge <line> --expect <state>` in main.
A changed worker head prevents acceptance.
Lead and reviewer results do not cause a merge.

At turn end, Studio runs `layr cache repair` and `layr save`.
An unchanged line without automatic states resolves to its current named head.
This automatic state preserves unfinished work separately from the named head.
Archive saves remaining work and creates `pins/studio/archive/<SHA-256 of agent ID>` at that exact state.
It then calls `layr line delete -D <line>`.
The archive receipt records `archiveRef` and `stateId`.
The pin has no expiry and keeps unmerged automatic states through garbage collection.
Studio never removes archive pins automatically.
After review or explicit discard, an operator can remove a pin with `layr ref delete <archiveRef>`.
Later garbage collection can then apply the normal retention rules.
Archive of a lead retains main.

Each filesystem effect has a durable receipt below `layr-native/receipts`.
A repeated request returns its saved result.
A request with an unknown outcome stays held for inspection.
Studio does not repeat an uncertain merge, import, save, or deletion.
An explicitly failed quiet-point save can retry with the same request ID.

## Warm caches

Mark cache folders in `.layrattributes` and ignore them in Git:

```text
node_modules regenerable
```

Build in main, then run:

```sh
layr save --caches
```

A new worker line receives the saved cache folders from main.
Existing worker lines keep their own folders.
Layr decides whether a folder contains versioned files and can become a cache.
Studio does not choose build commands or automatically publish caches.

## Reviewers

A native reviewer uses its private scratch folder as its working directory.
Its `layrReadCwd` points to main for trusted state reads.
It owns no Layr line.
Codex permits writes only in the private reviewer scratch folder.
Claude uses the same scratch working directory and a sandbox that denies writes to the store.
The `layr_read` tool reads states outside that sandbox through a command allowlist.
It permits `show`, `diff`, `log`, and the existing history read commands.
It does not permit changes to main or worker lines.
Reviewer turn completion does not save or repair main.

The host user owns the entire store.
Native mode does not create separate Unix users as the VM mode does.
The native `layr_read` tool also accepts `try <state> -- <command>`.
Studio runs the command in a temporary Layr copy.
On macOS, `sandbox-exec` permits writes only in that copy and the private scratch folder.
On Linux, Bubblewrap starts with an empty root and must be installed.
It mounts only read-only toolchains, the temporary copy, and private scratch.
The Studio state directory and daemon socket stay hidden.
The check has no network access.
The check cannot change main or the worker lines.

Reviewer checks receive an explicit environment allowlist.
It contains `PATH`, `LANG`, `TERM`, and available Rust toolchain selectors.
For try commands, `HOME` and `TMPDIR` point to scratch.
Claude reviewer sessions also retain Studio's provider account selectors, including `CLAUDE_CONFIG_DIR` when configured.
Claude keeps its provider `HOME` for authentication. Try commands keep scratch `HOME`.
Lead and implementer environments keep their existing rules.

Linux reviewer checks cannot use Git alternates that point into the hidden store.
For these states, `git log` in a try copy can fail. Use `layr_read` history commands instead.
The sandbox does not expose the store to repair these Git paths.
Linux still mounts broad `/usr` and `/opt` toolchain directories read-only.
Native reviewers can also access the browser server through `node_repl`, as current WT reviewers can.
These limits remain outside the current reviewer isolation changes.

## Migration

The migration keeps the original project and its Git worktrees.
Each worktree becomes a native line with a stable name.
The line replaces only its repository subtree.
The import includes uncommitted files and all ignored data.
It excludes the worktree's `.git` pointer. Build output becomes warm caches.
Prunable worktrees are reported and skipped.
Completed receipts make a repeated migration idempotent.

For a project with running WT chats:

1. Wait for the current turns to finish.
2. Pause those chats in Studio before the import.
3. Run the dry run below and inspect each worktree-to-line entry.
4. Run the migration while the original project and worktrees are quiet.
5. Create a new **Layr** chat for the same original project folder.
6. Review each imported line before you run its printed merge command.

Each selected Git worktree becomes a separate Layr line.
Existing WT chats keep their Git working directories.
The script does not pause chats or change their workspace mode.
Keep those chats paused after the import.
The new chat uses native main.
Completed receipts do not copy later edits from the old Git paths.
Parent Git metadata comes from the first main import. Later refs and objects remain in the original Git store.
Inspect the unpushed commit report before you retire that store.
The report prints the command list for each project.
After import, its merge commands pin each imported state with `--expect`.

The dry run starts no backend and writes no project or store files.
For each project it reports files, logical bytes, tracked bytes, cache bytes, estimated new bytes, and lines to create.
The totals include main and the worktree sources.
The report lists `.worktrees` containers, their versioned data, and any linked worktree without an admitted parent.
If a worktree cannot be sized, `sizesComplete` is false and its refusal includes its source path.
It lists nested repositories, their type, branch, uncommitted changes, untracked files, and unpushed commits.
Unpushed commits are local branch or detached-HEAD commits absent from all remote-tracking refs.
The command does not fetch remote refs.
Each repository report explains its Git metadata and worktree treatment.
Each worktree also has a `classification` and HEAD-specific commit counts.
Class `a` has uncommitted or untracked files.
Class `b` has HEAD commits absent from remote-tracking refs, local branches, or the parent repository HEAD.
Class `c` is clean and has none of these commits.
The `worktreeSummary` counts these classes. All worktrees remain selected by default.
Git status does not count ignored data. Such data can differ even in a clean worktree.
Studio also compares flagged index files by bytes to detect edits hidden by assume-unchanged or skip-worktree.
Use `--worktrees all` to keep the default selection.
Use `--worktrees dirty` to select uncommitted files, untracked files, and HEAD commits outside all local branches and remote-tracking refs.
A clean worktree on a named local branch can be skipped even when its commits are not pushed.
Each skipped entry records its HEAD commit and the reason.
Its Git store remains in main. Its ignored files remain in the original worktree.
If main already exists, Studio verifies that it retains each skipped HEAD commit.
The `selectionEstimates` report shows the costs and line counts for both modes.
Use explicit `--skip-worktree` paths for other operator exclusions.
Refusals include external Git object paths and prevent a real import.

Inspect the migration first:

```sh
python3 workspaces/runtime/apps/native-layr/codex_layr_native_migration.py \
  --state /path/to/studio-state --dry-run /path/to/project
```

Then omit `--dry-run` to import the project.
The wrapper uses `studio-runtime-server native-layr migrate`.
Use `--help` for the operator steps without starting the migration.
It starts no backend and opens no Studio database.
`STUDIO_RUNTIME_SERVER` can select the exact server binary.

## Checks

Run the focused Rust tests:

```sh
cargo test -j 4 -p studio-runtime-services --lib layr_native
```

Run the real host lifecycle check with a Studio-built Layr 0.2.2 binary:

```sh
STUDIO_LAYR_NATIVE_TEST_BINARY=/path/to/studio-state/layr-native/bin/layr \
  cargo test -j 4 -p studio-runtime-services --lib \
  layr_native::receipt_tests::real_host_import_worker_merge_archive_and_warm_caches \
  -- --ignored --nocapture
```

The check uses a separate state directory and store.
It verifies import exclusions, warm caches, exact-state merge, deletion, and retained state reads.
It does not prove a live model turn or the HTTP path.
On Linux, run this isolated check as root on btrfs.
