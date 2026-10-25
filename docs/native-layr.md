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

Studio installs Layr 0.2.0 at `<Studio state>/layr-native/bin/layr`.
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

The first turn uses the VM code archive rules from `vm_preparation.rs`.
The archive keeps code, Git history, and ignored files such as `.env`.
It excludes regenerable folders, ignored data folders, and linked worktrees.
Nested repositories keep their own Git rules.
A Git object store outside the project causes refusal.

Studio extracts the archive into a temporary folder outside the checkout.
It runs `layr init <project> --from <folder> --include-ignored`.
It then permits the owner to save directly in protected main.
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
This automatic state preserves unfinished work separately from the named head.
Archive saves remaining work and calls `layr line delete -D <line>`.
Layr retains the states.
Archive of a lead retains main.

Each filesystem effect has a durable receipt below `layr-native/receipts`.
A repeated request returns its saved result.
A request with an unknown outcome stays held for inspection.
Studio does not repeat an uncertain merge, import, save, or deletion.

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

A native reviewer uses main as its read-only working directory.
It owns no Layr line.
Codex permits writes only in the private reviewer scratch folder.
The `layr_read` tool reads states outside that sandbox through a command allowlist.
It permits `show`, `diff`, `log`, and the existing history read commands.
It does not permit changes to main or worker lines.
Reviewer turn completion does not save or repair main.

The host user owns the entire store.
Native mode does not create separate Unix users as the VM mode does.
The native `layr_read` tool also accepts `try <state> -- <command>`.
Studio runs the command in a temporary Layr copy.
On macOS, `sandbox-exec` permits writes only in that copy and the private scratch folder.
On Linux, Bubblewrap provides the same restriction and must be installed.
The check has no network access.
The check cannot change main or the worker lines.

## Migration

The migration keeps the original project and its Git worktrees.
Each worktree becomes a native line with a stable name.
The line replaces only its repository subtree.
The import includes uncommitted files and ignored files such as `.env`.
It excludes the worktree's `.git` pointer and regenerable folders.
Prunable worktrees are reported and skipped.
Completed receipts make a repeated migration idempotent.

Inspect the migration first:

```sh
python3 workspaces/runtime/apps/native-layr/codex_layr_native_migration.py \
  --state /path/to/studio-state --dry-run /path/to/project
```

Then omit `--dry-run` to import the project.
The wrapper uses `studio-runtime-server native-layr migrate`.
It starts no backend and opens no Studio database.
`STUDIO_RUNTIME_SERVER` can select the exact server binary.

## Checks

Run the focused Rust tests:

```sh
cargo test -j 4 -p studio-runtime-services --lib layr_native
```

Run the real host lifecycle check with a Studio-built Layr 0.2.0 binary:

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
