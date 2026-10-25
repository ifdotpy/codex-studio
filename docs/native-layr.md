# Native Layr workspaces

The `layr-native` workspace mode runs agents on the host.
The chat selector calls this mode **Layr**.
**Layr VM** uses `layr` and runs in the Studio Linux VM.
**WT** uses Git worktrees.

## Store and installation

Studio owns one host store at `<Studio state>/layr-native/store`.
The managed `bin/layr` entry starts `bin/layr-real` with the real Git directory first in its child process's `PATH`.
Layr can maintain its Git object store and export commits through that entry.
The agent's `PATH` still selects the managed Git guard, which refuses Git writes in native lines.
An existing installation receives this entry when Studio checks its managed binary.
`NATIVE_LAYR_DIRECTORY` in `runtime-store/src/path.rs` owns the installation directory.
`STORE_DIRECTORY` in `layr_native.rs` owns the store subdirectory.
The state directory must stay outside project folders.
The project ID is the SHA-256 of the canonical source folder path.
Later chats for that folder reuse its main line.

Studio installs Layr 0.2.1 at `<Studio state>/layr-native/bin/layr`.
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

## Git tools in native lines

The managed `git` executable uses Layr for compatible read requests in native lines.
The `read_route` function in `layr_native/git-guard.sh` owns the command table.
It converts Git `HEAD` revisions to Layr `head` revisions before the path separator.
Arguments after `--` keep their original bytes.
The wrapper preserves the command's output and exit status. It does not retry failed reads.

Layr 0.2.1 returns state IDs for commit identities and supports porcelain v1 only.
The proxy uses Layr only for a small set of verified argument forms.
Other reads use the real Git executable against the Layr-managed `.git` directory.
This fallback also serves read commands that Layr does not implement, including `describe` and `cat-file`.
Git commit IDs therefore match the exported head, including `git rev-parse HEAD` and build stamps.
The fallback disables optional Git index refreshes.
Git owns path quoting for porcelain and `ls-files` without `-z`, all grep forms, and unverified history formats.
The only proxied history format is `log -1 --format=%ct`.
Flagged revision queries use Git unless the complete form has a verified Layr equivalent.

The managed Git index describes the exported head. It does not follow `layr add`.
The proxy refuses Git `diff --cached`, `diff --staged`, and porcelain v2 instead of reporting that index as Layr's stage.
Use `layr diff --cached` and `layr status --porcelain` for these reads.
Other real Git index reads describe the exported head, including `ls-files` without `-z`.

Git writes remain refused with a Layr command hint.
Read commands with `--output` also fail because that option writes a file.
Outside native lines, the wrapper passes the original arguments to real Git.
Explicit Git configuration and repository selectors use the real Git read path.

Tools that use libgit2 or gix read `.git` directly and do not call this wrapper.
They require the Layr-managed `.git` directory and its Git object store.

The guard prevents agent mistakes. It is not a security boundary.
It resolves a relative `GIT_DIR` environment value before any later `-C` option.
Git can resolve that value against a different final directory.
The guard does not check every `GIT_*` destination, including `GIT_INDEX_FILE`.
Git can run configured helpers, including `diff.external` and filesystem monitors.
These helpers can write files or run other commands.

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
The import includes uncommitted files and ignored files such as `.env`.
It excludes the worktree's `.git` pointer and regenerable folders.
Prunable worktrees are reported and skipped.
Completed receipts make a repeated migration idempotent.

For a project with running WT chats:

1. Wait for the current turns to finish.
2. Pause those chats in Studio before the import.
3. Run the dry run below and inspect each worktree-to-line entry.
4. Run the migration while the original project and worktrees are quiet.
5. Create a new **Layr** chat for the same original project folder.
6. Review each imported line before you run its printed merge command.

Each open Git worktree becomes a separate Layr line.
Existing WT chats keep their Git working directories.
The script does not pause chats or change their workspace mode.
Keep those chats paused after the import.
The new chat uses native main.
Completed receipts do not copy later edits from the old Git paths.
The report prints the command list for each project.
After import, its merge commands pin each imported state with `--expect`.

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

Run the real host lifecycle check with a Studio-built Layr 0.2.1 binary:

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

Check the managed Layr entry with a real Git repository and a local bare remote:

```sh
STUDIO_LAYR_NATIVE_TEST_BINARY=/path/to/layr-0.2.1 \
  CARGO_INCREMENTAL=0 cargo test -j 4 -p studio-runtime-services --lib \
  layr_native::git_guard::tests::real_managed_layr_ \
  -- --ignored --test-threads=1
```

These four checks cover named saves, automatic saves, Git sync, and Git push.
They also verify that the managed Git entry still refuses agent writes.

Check the proxy with real Layr, Git, Cargo, and pnpm callers:

```sh
STUDIO_LAYR_NATIVE_TEST_BINARY=/path/to/layr-0.2.1 \
  CARGO_INCREMENTAL=0 cargo test -j 4 -p studio-runtime-services --lib \
  layr_native::git_guard::tests::real_managed_git_proxy_ -- --ignored
```
