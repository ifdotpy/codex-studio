# Agent workspace isolation, 2026-10-03

Research and measurements for one question: how can Codex Studio give each agent an isolated
workspace that starts at once, runs at native speed, and cannot damage the user's files?

The FSKit part has its own report: [FSKit overlay feasibility](2026-09-30-fskit-overlay-feasibility.md).

## Intent

The user runs many agents in YOLO mode and wants to give them full freedom. Three problems block that:

1. **Fear of damage.** An agent in YOLO mode can delete or break files outside its task.
2. **Slow, cold workspaces.** Each agent gets a new git worktree. It has no build output and no
   installed dependencies, so the first build starts from zero.
3. **Disk without control.** On 2026-09-30 this Mac had about 263 GB in more than 435 worktrees,
   spread over many `<repo>/.worktrees` folders. Nobody removed them.

| Location                        | Size   | Worktrees   |
| ------------------------------- | ------ | ----------- |
| `chrompile/.worktrees`          | 195 GB | 98          |
| `SAP/.worktrees`                | 26 GB  | 250         |
| `codex-agents/.worktrees`       | 17 GB  | 30          |
| `lumina/.worktrees`             | 17 GB  | 57          |
| `chrompile/chromium/.worktrees` | 5.9 GB | not counted |
| `engineering-skills/.worktrees` | 2.1 GB | not counted |

`occam/attar` is a symlink to `chrompile`, so its 195 GB is the same data. `du` counts APFS clones
as full copies, so real use can be lower.

## Requirements

These are the requirements after the design discussion. Each item records a decision of the user.

1. **Threat model: agent mistakes.** Not malicious code and not secret theft.
2. **Full freedom inside the workspace.** The agent can install packages, delete files and break the
   build. It cannot damage the host, the user's checkout or other agents.
3. **Instant start for any repository.** Start time must not depend on the number of files. A pool
   of prepared workspaces is not acceptable, because the agent must be universal. Waiting for a clone
   while the first model turn runs is not acceptable either.
4. **Native speed** for git, search and builds.
5. **The user's code stays where the user keeps it.** A central folder that must hold all user code
   is not acceptable.
6. **Data is readable without Studio**, with standard tools.
7. **Disk use is visible and bounded.** Removing an agent is cheap.
8. **Reuse** of tools, dependencies and build caches.
9. **Cross-platform.** macOS, Linux and Windows. No Linux VM on macOS, because macOS projects need
   native macOS builds (Xcode, Swift).

## Decision

| OS      | Mechanism                                                                                                                  | Status                                                                                  |
| ------- | -------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------- |
| macOS   | ASIF disk image as the base. Per agent: an APFS clone of the image file, attached read-write (shadow file as the fallback) | Main candidate. Open: base build time, build cache paths, builds under a strict sandbox |
| Linux   | overlayfs: lower is a read-only btrfs snapshot, upper is a folder per agent                                                | Main candidate on btrfs. Open: parallel metadata load, sandbox                          |
| Windows | Differencing VHDX or ProjFS                                                                                                | Not measured                                                                            |

**This design does not meet requirement 3 for the first agent of a repository.** The base build is
O(files): about 16.5 min for chromium on macOS. Starts after that are about 1 s. So the decision
needs one of these:

- the user accepts a one-time base build per repository (or per large change of it), or
- Studio builds the base in the background when a repository is first added, before any agent asks.

Without one of them, no measured option meets requirement 3 on macOS for large repositories.

**User decision (2026-10-03): build the base in the background when Multi agent mode is turned on.**
The trigger is the **Multi agent** / **Single agent** switch in the chat header
([ORCHESTRATION.md](../../ORCHESTRATION.md)). When the switch goes to Multi agent, Studio starts the
base build for the lead's repository if no current base exists.

**User decision (2026-10-03): an implementer that starts before the base is ready works read-only.**

1. Multi agent mode is turned on. The base build starts in the background.
2. An implementer starts at once in the user's current folder with the existing read-only sandbox
   (`sandboxPolicy: {"type": "readOnly"}`, `scripts/codex_runtime.py:3685`). It can read, search,
   inspect git history and plan. It cannot build or run tests, because they write files.
3. The base is ready. Studio clones the image file for the agent, copies the fresh user edits into it,
   makes the snapshot commit, and moves the agent to the image mount with write access (process
   sandbox limited to the mount and the agent's own folders).
4. Studio notifies the agent that its writable workspace is ready: the new path, the write access and
   the snapshot commit. Native delivery steers an active turn or starts an idle one
   ([ORCHESTRATION.md](../../ORCHESTRATION.md)). The new working folder and sandbox apply from the
   next turn, so the notice must tell the agent to continue its work there.

The process sandbox (requirement 2) is a separate and required part. The image isolates the
workspace, not the process. See "Limits and risks".

Common model for all platforms:

- **Base.** A read-only copy (macOS) or snapshot (Linux) of the repository at one point in time. It
  is a cache. It can be deleted and built again. One base serves all agents of that repository.
- **Agent layer.** The agent writes only to its own layer: a shadow file or a cloned image file
  (macOS), or an upper folder (Linux). One agent equals one file or one folder.
- **Fresh user edits.** At agent start, Studio copies the files that changed since the base was built
  into the agent layer. FSEvents usually gives this list in O(changes). See "Limits and risks", item 3.
- **Result.** The agent commits to a git branch. Studio fetches the branch into the user's repository.
- **Removal.** Detach and delete one file (macOS), or unmount and delete one folder (Linux).

## Merging agent work

Merge uses git, as with today's worktree branches (`codex-agent/<agent-id>`, see ORCHESTRATION.md).
The image type does not matter.

1. The agent commits inside its image on `codex-agent/<id>`. New objects go to the agent's `.git`.
   Old objects come from the user's repository through alternates.
2. Studio, outside the agent sandbox, runs `git fetch <agent mount> codex-agent/<id>:codex-agent/<id>`
   in the user's repository. Git copies only the agent's new objects.
3. The lead or the user merges, rebases or cherry-picks the branch. Conflicts are normal git conflicts.
4. After the fetch the agent image can be removed.

Two rules:

- **Separate the user's edits from the agent's work.** The fresh user edits copied in at start are
  uncommitted. Right after the copy, Studio makes one snapshot commit in the agent repository. The
  agent commits on top of it. At merge time Studio takes only the commits after the snapshot:
  `git rebase --onto <user HEAD> <snapshot> codex-agent/<id>`.
- **Checkpoint commit at the end of each turn**, so uncommitted agent work survives removal or a crash.

Nested repositories (149 in chromium) need one fetch for each nested repository that the agent changed.

## Folders without git

The workspace mechanism does not need git. Isolation, start, the read-only phase, the notice,
checkpoints, removal and the FSEvents delta work for any folder. Today an implementer outside a git
repository works directly in `cwd` with a warning ([ORCHESTRATION.md](../../ORCHESTRATION.md)), so it
has no isolation. With images it has the same isolation as in a git repository.

Only the return of the result needs another path. Studio has three versions of each file: the base
(the copy at base build time), the user's current file, and the agent's file.

1. Studio lists the files that the agent changed: FSEvents on the agent mount since the snapshot
   event id, with a compare against the base as the fallback.
2. If the user did not change a file since the base, Studio takes the agent's version.
3. If both changed it, Studio runs a three-way merge on the plain files (`git merge-file` or `diff3`
   work without a repository).
4. A conflict, and every binary file that both sides changed, goes to the user to choose.

The git-only parts of the design (alternates, `core.checkStat`, the snapshot commit) do not apply.

## Options and verdicts

| Option                                      | Start for chromium                          | Speed                    | Verdict                                                  |
| ------------------------------------------- | ------------------------------------------- | ------------------------ | -------------------------------------------------------- |
| git worktree (today)                        | full checkout, minutes                      | native                   | No isolation of `.git`, no warm build, disk spreads      |
| APFS clone of the tree (`cp -c -R`)         | 241 s                                       | native                   | Too slow to start. Delete takes 106 to 339 s             |
| APFS `clonefile()` of the directory         | 30 s                                        | native                   | Too slow to start for large repositories                 |
| Pool of prepared clones                     | 0 s                                         | native                   | Rejected: the agent must be universal                    |
| Clone in parallel with the first model turn | up to 30 s wait                             | native                   | Rejected: the workspace must be ready at once            |
| Local NFS overlay server                    | 0.06 s                                      | `git status` over 20 min | Rejected: too slow on large trees                        |
| FSKit overlay module                        | O(1)                                        | XPC call per cache miss  | Rejected: slow and buggy by Apple's own account          |
| Linux VM with overlayfs on macOS            | O(1)                                        | native Linux             | Rejected: no macOS builds                                |
| macOS VM per agent                          | APFS clone of VM disk                       | native                   | Not chosen: at most 2 macOS VMs at once, GBs of RAM each |
| OSTree                                      | O(1) hardlink checkout                      | native                   | Linux only. Opaque object store                          |
| ASIF image + shadow (macOS)                 | about 1 s after a 16.5 min base             | native                   | Fallback: needs the base file for the agent's life       |
| **APFS clone of the ASIF file (macOS)**     | **0.03 s clone + 1.3 s attach, after base** | **native**               | **Chosen for macOS**                                     |
| **overlayfs + btrfs snapshot (Linux)**      | **0.00 s mount, 0.49 s snapshot per base**  | **native for 1 agent**   | **Chosen**                                               |

## Prior art

| Project                                                                                                         | Mechanism                                                                                                                                               | Difference from our design                                                                                                                                  |
| --------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [AgentFS](https://turso.tech/blog/agentfs-overlay) (Turso)                                                      | Copy-on-write overlay: host folder read-only, agent changes in SQLite. macOS: localhost NFS server, `mount_nfs`, Seatbelt profile, no root. Linux: FUSE | Closest idea. Every file call goes through a user-space server. No published speed numbers. Not measured here                                               |
| [GhostVM](https://ghostvm.org/ghostvm-for-ai-agents), [Tart](https://github.com/cirruslabs/tart)                | Full macOS VM per agent, VM disk is an APFS clone                                                                                                       | Strong isolation and native macOS builds. Apple allows at most 2 macOS VMs at once ([Tart discussion](https://github.com/cirruslabs/tart/discussions/1054)) |
| [Claude Code sandbox](https://code.claude.com/docs/en/sandboxing), [Bazel](https://bazel.build/docs/sandboxing) | Seatbelt (`sandbox-exec`) on macOS, bubblewrap or namespaces on Linux                                                                                   | Limits where a process can write. No private copy of the workspace. We need this in addition to the image                                                   |
| [Bazel sandboxfs](https://blog.bazel.build/2018/04/13/preliminary-sandboxfs-support.html)                       | FUSE file system with a custom view of files                                                                                                            | FUSE on macOS needs a kernel extension or the slow FSKit backend                                                                                            |
| Meta EdenFS                                                                                                     | Virtual file system for source control. NFSv3 on macOS, FUSE on Linux, ProjFS on Windows                                                                | Needs its own source control (Sapling) for fast status                                                                                                      |
| [Apple DTS advice](https://developer.apple.com/forums/thread/828533) (Kevin Elliott, June 2026)                 | For build sandboxes: "you could 'clone' the entire hierarchy by simply cloning the disk image while it's unmounted", plus EndpointSecurity for access   | This is the clone variant. No product does it publicly                                                                                                      |

No public project uses a disk image with a shadow file, or a clone of an image file, for agent
workspaces, as far as the search on 2026-10-03 shows. `hdiutil -shadow` itself is old and well known for changing read-only images.

## Test environment

- Mac: Mac17,7, Apple M5 Max, 18 cores, 128 GB RAM, internal SSD 1.8 TB, 90 to 98 percent full
  during the tests. macOS 27.0 (2026-09-30) and 27.2 build 26B5091g (2026-10-03).
- Linux: temporary OrbStack 2.2.3 machine, Ubuntu 26.04.1 arm64, kernel 7.0.14-orbstack, btrfs root,
  17 vCPU, 15 GB RAM. It runs in a VM on the same Mac.
- Data:
  - `chromium/src`: 1,090,996 files on 2026-09-30, 1,589,950 on 2026-10-03, 1,832,041 non-object
    files at the final base build. 100 GB in total, `.git` 66 GB (`.git/objects` 61 GB), 149 nested
    git repositories.
  - `blink`: a copy of `chromium/src/third_party/blink` committed as a separate git repository.
    211,066 files, 1.7 GB of files, 524 MB of packed objects.
  - `trading-bot`: Rust workspace, 558 packages, about 9,400 source files, debug `target/` 3.3 GB.

### Method notes

- **True cold cache without root.** The page cache belongs to a file object. A clone of the image
  file (`cp -c`), or `clonefile()` of a tree, makes new files that have no cached pages. This tests
  cold reads without `sudo purge`.
- **"Cold mount"** means a new attach of an image whose file can still be in the host cache.
- **Linux cold** uses `echo 3 > /proc/sys/vm/drop_caches` in the guest. The Mac host can still cache
  the VM disk, so Linux cold numbers are optimistic.
- **sccache off** for build comparisons (`RUSTC_WRAPPER=`). With sccache on, the run order decides the
  result.
- Each number is one run. The Mac was busy: `chromium/src` changed 130 paths during one 16 minute
  build. Expect about plus or minus 30 percent.
- `tar` is not a valid read benchmark on macOS. It took 213 s on the native `blink` tree, so its cost
  is per file in `tar` itself. Use `rg` for read speed.

## Results

### APFS clone (macOS)

| Tree                  | Files     | Operation                               | Time                |
| --------------------- | --------- | --------------------------------------- | ------------------- |
| codex-agents worktree | 564       | `cp -c -R`                              | 0.06 s              |
| chromium/src          | 1,090,996 | `cp -c -R`                              | 240.9 s (sys 217 s) |
| chromium/src          | 1,090,996 | one `clonefile()` call on the directory | 29.8 s              |
| chromium/src          | 1,090,996 | `rm -rf` of the clone                   | 106.3 s             |
| chromium/src          | 1,832,041 | `rm -rf` of the clone (final run)       | 339.1 s             |

### Local NFS overlay (macOS)

Server: `nfsserve` 0.11.0 `mirrorfs` example (Rust, NFSv3, passthrough, no overlay logic), with the
per-write `sync_all()` removed and logs at WARN. Mount without root:
`mount_nfs -o nolocks,vers=3,tcp,rsize=131072,wsize=131072,actimeo=120,port=11111,mountport=11111,soft,nobrowse localhost:/ <dir>`.

| Operation                  | Native  | NFS                                       |
| -------------------------- | ------- | ----------------------------------------- |
| mount                      | none    | 0.03 to 0.06 s                            |
| chromium walk with `stat`  | 37.6 s  | 253.7 s cold, 264.2 s warm                |
| chromium read `base/`      | 3.4 s   | 7.5 s                                     |
| chromium `git status`      | 9.9 s   | more than 1200 s (stopped)                |
| create a small file        | 0.19 ms | about 3.5 ms (19 ms with per-write fsync) |
| server memory for chromium | none    | 789 MB                                    |

The warm walk is not faster: the walk takes longer than the 120 s attribute cache, so every `stat`
goes to the server again (about 230 µs per file). A better server is faster, but the kernel round
trip remains. This was not separated.

### FSKit (macOS)

See [FSKit overlay feasibility](2026-09-30-fskit-overlay-feasibility.md). Short form: possible from
macOS 26 with `FSPathURLResource`, about 121 µs per uncached call, no clonefile, known rename and
negative-lookup bugs, user approval in System Settings. Apple DTS advises against it for builds.

### Disk image with shadow: first chromium test (macOS, `.sparseimage`, hdiutil)

| Step                                         | Result                          |
| -------------------------------------------- | ------------------------------- |
| base: copy 1.59M files with one `tar` stream | 3,754 s (63 min)                |
| base: alternates + index refresh             | 22 s + 111 s                    |
| base image size                              | 42 GB                           |
| agent attach                                 | 0.60 s                          |
| walk with `stat`, cold mount / warm          | 47.5 s / 12.1 s (native 17.1 s) |
| first `git status` in a new agent            | 28 to 29.5 s (native 7.3 s)     |
| 10 attaches                                  | 0.60 to 0.81 s each             |
| 5 parallel `git status`                      | 51.9 s                          |
| remove agent                                 | detach 0.29 s + rm 0.03 s       |

### Subset `blink` (macOS, 211,066 files)

Base build:

| Method                                                           | Time                                |
| ---------------------------------------------------------------- | ----------------------------------- |
| one `tar` stream into `.sparseimage`                             | 484 s                               |
| 8 `tar` streams into `.sparseimage`                              | 280 s                               |
| 8 `tar` streams into ASIF                                        | 181 s                               |
| 16 `tar` streams into ASIF                                       | 160 s                               |
| `diskutil image create from <folder>` to ASIF                    | 99 to 132 s (copies `.git` too)     |
| index refresh, default `core.checkStat`                          | 185 s (git hashes every file again) |
| index refresh, `core.checkStat=minimal`, `core.trustctime=false` | 2.6 s                               |

Agent on ASIF:

| Operation                              | ASIF + shadow | Native       |
| -------------------------------------- | ------------- | ------------ |
| attach                                 | 0.7 to 1.0 s  | none         |
| walk with `stat`, cold mount / warm    | 8.5 s / 3.0 s | 4.6 to 4.9 s |
| `git status`, cold mount / warm        | 2.4 s / 2.1 s | 3.3 to 3.6 s |
| `rg` over all files, cold mount / warm | 3.2 s / 2.1 s | 4.5 s        |
| write 2,000 small files                | 0.37 s        | about 0.4 s  |
| shadow after these tests               | 18 MB         | none         |

Git settings for the cold start (ASIF):

| Base setting                                            | First `git status` in a new agent |
| ------------------------------------------------------- | --------------------------------- |
| none                                                    | 2.44 s                            |
| `core.untrackedCache=true`                              | 2.34 s                            |
| untracked cache + fsmonitor stub that reports no change | 2.23 s                            |

The cold cost is reading the index and metadata from a cold mount, not checking files. The stub
also hides real changes, so it is not usable.

**Isolation defect:** `diskutil image attach --shadow` on a `.sparseimage` ignores the shadow. The
agent wrote into the base, and a second agent saw the file. ASIF with `--shadow` is isolated.
`hdiutil attach -shadow` on a `.sparseimage` was isolated in the first test.

### Final chromium test (macOS, ASIF, 1,832,041 files)

Base build:

| Step                                                  | Time                            |
| ----------------------------------------------------- | ------------------------------- |
| create blank ASIF, 200 GB sparse                      | 0.8 s                           |
| list files except `.git/objects`                      | 70 s                            |
| copy with 16 `tar` streams                            | 860 s                           |
| alternates and git config for 150 repositories        | 51 s                            |
| index refresh (`checkStat=minimal`)                   | 8.4 s                           |
| **total**                                             | **about 16.5 min** (was 63 min) |
| image size                                            | 44 GB                           |
| FSEvents delta of the real checkout over those 16 min | 4.7 s, 130 changed paths        |

Agents:

| Operation                                    | Native                        | ASIF + shadow                    |
| -------------------------------------------- | ----------------------------- | -------------------------------- |
| attach                                       | none                          | 1.0 to 1.7 s                     |
| `git status`, warm                           | 7.7 s                         | 9.4 s (cold mount), 11.1 s again |
| `git status`, true cold                      | 9.3 s                         | 6.9 s                            |
| `rg` over all files, warm                    | 54.5 s                        | 90.2 s cold mount, 42.3 s again  |
| `rg` over all files, true cold               | 63.9 s                        | 54.7 s                           |
| 10 attaches                                  | none                          | 6.5 s in total                   |
| 10 parallel `git status`, true cold          | 31.7 s (one shared checkout)  | 40.2 s, 47.7 s again             |
| 10 parallel `rg`                             | 511.7 s (one shared checkout) | 425.8 s                          |
| memory free during 10 agents                 | none                          | 84 to 86 percent                 |
| shadow per agent after `git status` and `rg` | none                          | 36 MB, 377 MB for 10             |

The native parallel runs use one shared checkout, so they are a lower bound for native. Ten separate
native copies would cost more.

### Real build (macOS, trading-bot, sccache off)

Base: blank ASIF 100 GB sparse, `ditto` copy in 11.3 s, image 4.6 GB.

| Run                   | Native (`cp -c -R` copy) | ASIF + shadow         |
| --------------------- | ------------------------ | --------------------- |
| 1 build               | 46.4 s                   | 44.6 s                |
| 10 builds in parallel | 343.5 s                  | 399.7 s (+16 percent) |
| `target/` per agent   | 3.3 GB                   | 3.3 GB, shadow 3.2 GB |
| remove 10 agents      | 33.0 s                   | 9.0 s                 |

The 10 parallel image builds made 33 GB of shadows, 10 times one full `target/`. The direct check
of each exit code failed because of a quoting error in the script, so build success is shown
indirectly.

**Capacity defect:** `diskutil image create from <folder>` makes a volume with almost no free space.
The first build in it failed with `No space left on device`. Create a large blank sparse ASIF and copy
into it. Free space in a sparse image costs no disk.

### Fresh user edits into a stale base (macOS, blink)

| Step                                                                       | Result                    |
| -------------------------------------------------------------------------- | ------------------------- |
| attach stale base                                                          | 0.46 to 0.58 s            |
| FSEvents "changes since event id" for 100 edited, 10 new, 10 deleted files | 0.03 to 0.06 s, 120 paths |
| copy or delete those paths in the agent mount                              | 0.77 to 1.08 s            |
| byte compare of the 120 paths, checkout against agent                      | 120 of 120 equal          |

The tool is in the appendix. FSEvents keeps its history in a log on the volume, so the query should
also work after a restart of Studio. That was not tested.

### Shadow file against a clone of the image file (macOS)

Same bases as above (`chromium.asif` 44 GB, `trading-bot` ASIF). Clone variant: `cp -c base.asif
agent.asif` while the base is detached, then `diskutil image attach` read-write without a shadow. A
clone is a new file with no cached pages, so its first numbers are true cold. Apple DTS describes this
variant in [forum thread 828533](https://developer.apple.com/forums/thread/828533).

| Operation                                   | Shadow file          | Clone of the image file                           | Native                         |
| ------------------------------------------- | -------------------- | ------------------------------------------------- | ------------------------------ |
| create the agent layer                      | new shadow at attach | `cp -c` of 44 GB: 0.03 s                          | none                           |
| attach                                      | 1.0 to 1.7 s         | 1.28 s (read-write)                               | none                           |
| chromium `git status`, true cold            | 6.9 s                | 6.2 s                                             | 9.3 s                          |
| chromium `rg` over all files, true cold     | 54.7 s               | 40.5 s                                            | 63.9 s                         |
| 10 agents: create and attach                | 6.5 s                | 0.06 s + 9.6 s                                    | none                           |
| 10 agents: parallel `git status`, true cold | 40.2 s               | 36.7 s                                            | 31.7 s (one shared checkout)   |
| 10 agents: parallel `rg`                    | 425.8 s              | 417.9 s                                           | 511.7 s (one shared checkout)  |
| 1 cargo build (trading-bot)                 | 44.6 s, 50.2 s       | 48.9 s, 50.2 s                                    | 46.4 s                         |
| 10 cargo builds in parallel                 | 399.7 s              | 356.1 s, 10 of 10 built                           | 343.5 s                        |
| disk after 1 build                          | 3.2 GB (shadow size) | 3.19 GB (free space change)                       | 3.3 GB                         |
| disk after 10 builds                        | 33 GB                | 32.5 GB                                           | about 33 GB                    |
| isolation (other agent, base)               | isolated             | isolated                                          | none                           |
| checkpoint                                  | not tested           | detach 0.51 s + `cp -c` 0.03 s + attach 0.74 s    | none                           |
| restore                                     | not tested           | 1.39 s (detach, swap file, attach), file restored | none                           |
| remove one agent / 10 agents                | 0.3 to 1 s / 9.0 s   | 0.3 to 0.6 s / 7.0 to 8.6 s                       | 106 to 339 s per chromium tree |

Three build and clean cycles in one agent (aging test, trading-bot):

| Cycle | Shadow: build / space after build / after `cargo clean` | Clone: build / space after build / after `cargo clean` |
| ----- | ------------------------------------------------------- | ------------------------------------------------------ |
| 1     | 50.2 s / 3.2 GB / 0.65 GB                               | 50.2 s / 3.24 GB / 0.65 GB                             |
| 2     | 53.8 s / 3.7 GB / 1.2 GB                                | 51.2 s / 3.73 GB / 1.25 GB                             |
| 3     | 49.4 s / 4.3 GB / 1.6 GB                                | 49.7 s / 4.33 GB / 1.60 GB                             |

Both variants return most freed space to the host (TRIM passes through the image), and both keep
about 0.5 GB per cycle. Build time does not degrade over three cycles. Days of work were not tested.

Differences that decide the choice:

| Property                                               | Shadow file                                | Clone of the image file                    |
| ------------------------------------------------------ | ------------------------------------------ | ------------------------------------------ |
| agent survives loss or replacement of the base file    | no, base and shadow are a pair             | yes, the clone owns its blocks             |
| a new base version while old agents run                | old base must stay until they end          | old base can be deleted at once            |
| open the agent's data without Studio                   | `diskutil image attach base --shadow file` | `diskutil image attach agent.asif`         |
| exact disk cost per agent                              | file size of the shadow                    | `ATTR_CMNEXT_PRIVATESIZE` of the clone     |
| agent layer on another volume (for example a RAM disk) | possible                                   | no, a clone must stay on the base's volume |
| checkpoint and restore                                 | clone of the shadow file (not tested)      | clone of the agent file (tested)           |

**Result: use a clone of the image file on macOS.** The reasons are the independent agent file and
the fast creation. A general speed advantage is not proven: in this test 10 parallel Rust builds were
11 percent faster with the clone (356 s against 400 s, native 344 s), but in the independent Swift
test below the incremental build was faster with the shadow (1.54 s against 2.27 s). Keep the shadow
file as a fallback when the agent layer must live on another volume.

### Independent check: small Swift project (second agent)

A second agent repeated the comparison on 2026-10-03 with its own scripts. Evidence on this Mac:
`~/.local/share/codex-studio-evidence/2026-10-03-asif-comparison/` (`summary.json`, `results.json`,
logs). Data: 50,000 files, base image about 960 MB, a SwiftPM project of 100 Swift files. macOS 27.2,
busy host, caches not purged. Medians of three runs:

| Check                                   | Clone of the image file | Shadow file     |
| --------------------------------------- | ----------------------- | --------------- |
| `clonefile()`                           | 0.26 ms                 | none            |
| create and attach                       | 0.69 s                  | 0.78 s          |
| `git status`                            | 0.22 s                  | 0.23 s          |
| read                                    | 0.71 s                  | 0.66 s          |
| incremental build after one file change | 2.27 s                  | 1.54 s          |
| build after module cache reset          | 4.09 s                  | 3.64 s          |
| private bytes after build               | 25.2 MiB                | 28.7 MiB        |
| detach / delete file                    | 0.56 s / 0.02 s         | 0.54 s / 0.01 s |

Findings:

- **Isolation works for both.** Writes of one agent did not appear in another. The base checksum did
  not change.
- **The clone works without the base at its old path.** The base was renamed, the clone was attached,
  and its data was intact.
- **Build caches do not move between paths.** In both variants Swift rejected the module cache that
  was built at another absolute path (build exit code 1). After a module cache reset the build passed.
- **Seatbelt blocked every write outside the workspace:** outside write, outside delete, another
  agent's workspace, a symbolic link, path traversal, deleting the base, writes from a child process.
- **A strict Seatbelt profile broke the Swift build.** The changed-code build failed (exit code 1) with
  writes allowed only to the workspace and `/dev/null`. It passed only when the shared user temporary
  folder was also writable. That folder is shared by all agents, so it is not isolation between them.

### Seatbelt with Apple build tools (follow-up)

Test on 2026-10-03: a new SwiftPM executable package and a copy of the real project
`CallScribe.xcodeproj` (6 remote Swift packages) inside an ASIF mount, Swift 6.4, Xcode SDK 27.0.
Strict profile: `(allow default)`, `(deny file-write*)`, writes allowed only to the agent mount and
`/dev/null`. Environment: `TMPDIR`, `CLANG_MODULE_CACHE_PATH` and `SWIFTPM_MODULECACHE_OVERRIDE` inside
the mount. `swift build` also used `--disable-sandbox`, `--cache-path`, `--config-path` and
`--security-path` inside the mount.

| Build                                                                                      | Profile                                                | Result                                                                             |
| ------------------------------------------------------------------------------------------ | ------------------------------------------------------ | ---------------------------------------------------------------------------------- |
| `swift build` (default Swift Build system)                                                 | strict                                                 | fails: `error: permissionDenied` at `Ld`                                           |
| the same link command run alone                                                            | strict                                                 | passes, so the linker is not the cause                                             |
| `swift build --build-system native`                                                        | strict                                                 | **passes**                                                                         |
| `swift build`                                                                              | strict + `T/TemporaryItems` only                       | fails                                                                              |
| `swift build`                                                                              | strict + `T/TemporaryItems` + `T/TemporaryDirectory.*` | **passes (2 of 2)**                                                                |
| `swift build`                                                                              | strict + the whole user temporary folder `T`           | passes                                                                             |
| `xcodebuild` on the package, `-packageCachePath` and `-derivedDataPath` in the mount       | strict, narrow or whole `T`                            | fails: writes `~/Library/Caches/org.swift.swiftpm/manifests/ManifestLoading/*.dia` |
| the same, with that cache folder also writable                                             | narrow                                                 | fails: `sandbox-exec: sandbox_apply: Operation not permitted`                      |
| `CallScribe.xcodeproj`, packages resolved and built once outside the sandbox (27 s + 33 s) | narrow, narrow + cache, whole `T`                      | fails the same two ways                                                            |

`T` is the user temporary folder from `getconf DARWIN_USER_TEMP_DIR`.

Findings:

- **`TMPDIR` is not enough.** Swift Build writes to the user temporary folder through Foundation and
  TSCBasic: `T/TemporaryItems/NSIRD_swift-build_*` and `T/TemporaryDirectory.*/`. They ignore `TMPDIR`.
  A `HOME` inside the mount does not move user caches either.
- **Seatbelt matches resolved paths.** A rule for `/var/folders/...` has no effect. Write
  `/private/var/folders/...`.
- **A narrow rule is enough for `swift build`.** Allow `(subpath "<T>/TemporaryItems")` and
  `(regex #"^<T>/TemporaryDirectory\.[^/]+(/.*)?$")`. Files directly in `T` are not needed. These
  folders are shared by all agents. Their names are random, so an accident is unlikely, but one agent
  can still delete the temporary items of another.
- **The native build system needs no exception.**
- **`xcodebuild` with Swift packages does not run under Seatbelt.** SwiftPM writes manifest
  diagnostics to the global cache, which `-packageCachePath` does not move, and then applies its own
  sandbox to the manifest, which fails inside a sandbox. `xcodebuild` has no option to turn the
  manifest sandbox off. A warm resolve and build outside the sandbox does not avoid it.

Options for Xcode projects with packages (not tested):

1. A build broker: Studio runs `xcodebuild` outside the agent sandbox, with all paths inside the agent
   mount, when the agent asks for a build.
2. Turn off the SwiftPM manifest sandbox with a global Xcode user setting. This changes the user's
   Xcode for all projects.
3. EndpointSecurity instead of Seatbelt, as Apple DTS suggests. It needs an Apple entitlement.

### Linux overlayfs (OrbStack machine, blink and trading-bot)

| Operation                        | Native                    | overlayfs               |
| -------------------------------- | ------------------------- | ----------------------- |
| mount                            | none                      | 0.00 s                  |
| walk with `stat`, cold / warm    | 0.73 s / 0.38 s           | 1.19 s / 0.49 s         |
| `git status`, cold / warm        | 0.33 s / 0.14 s           | 0.47 s / 0.17 s         |
| `rg` over all files, cold / warm | 0.36 s / 0.25 s           | 0.40 s / 0.26 s         |
| write 2,000 small files          | 0.03 s                    | 0.03 s                  |
| unmount and delete upper         | none                      | 0.22 s                  |
| 10 mounts                        | none                      | 0.03 s                  |
| 10 parallel `git status`, cold   | 0.58 s (one shared tree)  | 3.43 s                  |
| 10 parallel `rg`, cold           | 0.80 s (one shared tree)  | 3.37 s                  |
| 1 cargo build (trading-bot)      | 36.6 s                    | 35.4 s, upper 3.5 GB    |
| 10 cargo builds in parallel      | 401.0 s, 10 of 10 built   | 380.0 s, 10 of 10 built |
| delete 10 agents after builds    | 2.6 s (10 reflink copies) | 14.6 s                  |

Other Linux facts:

- **Rootless mount works:** `unshare -Urm mount -t overlay ...` as a normal user, kernel 7.0.
- **A live lower leaks:** when the user edits the lower folder while the overlay is mounted, the
  agent sees the edit to a file it already read and sees a new file. The kernel documentation says
  changes to the lower layer while mounted give undefined behavior.
- **Frozen lower:** a read-only btrfs snapshot takes 0.49 s per base version. The first setup
  (subvolume plus reflink copy of 211k files) took 3.16 s. After a snapshot, later edits to the
  source are not visible to the agent. First `git status` on the frozen lower: 0.53 s.
- The Linux machine is much faster than macOS for metadata (walk 0.73 s against about 4.7 s on the
  same tree), so the macOS image overhead must be judged against macOS native, not against Linux.

## Design rules

1. **macOS: use ASIF only.** `diskutil` ignores `--shadow` for `.sparseimage`. Test isolation for any
   new image format before use.
2. **Give the base free space.** Create a large blank sparse ASIF and copy into it. Do not use
   `create from <folder>` for a base.
3. **Do not copy `.git/objects`.** Point `objects/info/alternates` of every repository in the base to
   the user's object store (read-only). For chromium this saves 61 GB.
4. **Set `core.checkStat=minimal` and `core.trustctime=false` in the base.** The index refresh drops
   from minutes to seconds, because copies keep mtime and size but change inode and ctime.
5. **Do not rely on untracked cache or fsmonitor for the cold start.** They do not help.
6. **Linux: never mount a live lower.** Use a read-only btrfs snapshot (or another frozen copy) as the
   lower.
7. **Deleted files return most of their space, not all.** After `cargo clean`, both the shadow file
   and the cloned image gave back about 2.6 GB of 3.2 GB, but about 0.5 GB stayed per build and clean
   cycle. Budget disk per agent and remove finished agents.
8. **The image does not stop writes outside the workspace.** A process sandbox (Seatbelt on macOS,
   namespaces or Landlock on Linux) must allow writes only to the agent mount and the agent's own
   temporary and cache folders. See "Limits and risks", item 2.
9. **Build the base in parallel.** 16 `tar` streams are 4 times faster than one on macOS.
10. **Measure a cloned image with `ATTR_CMNEXT_PRIVATESIZE`.** `ls` and `du` show the full size of a
    clone. `getattrlist` with `ATTR_CMNEXT_PRIVATESIZE` (option `FSOPT_ATTR_CMN_EXTENDED`) returns the
    bytes that only this clone owns: 2.00 GB after 2 GB of writes in the test.

## Limits and risks

1. **The first start is not instant.** The base build is O(files): about 16.5 min for chromium on
   macOS, seconds for small repositories. If requirement 3 is strict for a new repository, this design
   does not meet it. On Linux the base is a reflink copy (3.16 s for 211k files) plus a snapshot, and
   it needs btrfs or XFS. On ext4 the copy is a full copy.
2. **The Mac is not protected yet.** The image isolates the workspace, not the process. Today YOLO mode
   maps to `{"approvalPolicy": "never", "sandboxPolicy": {"type": "dangerFullAccess"}}`
   (`scripts/codex_runtime.py:3682-3683`), so an agent can still delete files outside its mount. Every
   path that runs commands or writes files needs a mandatory process sandbox (Seatbelt on macOS)
   that allows writes only to the agent mount and the agent's own temporary and cache folders. Shared
   writable caches let one agent damage the cache of another agent: give each agent its own writable
   cache, or make shared caches read-only. Apple build tools need exceptions: `swift build` needs two
   shared folders in the user temporary folder, and `xcodebuild` with Swift packages does not run under
   Seatbelt at all. See "Seatbelt with Apple build tools".
3. **FSEvents is not always cheap and is not a snapshot.** When events are lost, FSEvents sets
   `kFSEventStreamEventFlagMustScanSubDirs` and the client must scan those folders again
   ([Apple FSEvents guide](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/FSEvents_ProgGuide/UsingtheFSEventsFramework/UsingtheFSEventsFramework.html)).
   A list of events also does not give a consistent snapshot of files that the user keeps changing
   during the copy. The delta step needs a rescan fallback and a final check (for example, compare
   size and mtime after the copy, and repeat for changed paths).
4. **git alternates make the base depend on the user's repository.** If the user's repository prunes
   objects (`git gc` after a rebase or a branch delete), the base or the agent repository can lose
   objects it needs ([git clone, `--shared`](https://git-scm.com/docs/git-clone)). Studio must keep
   those objects alive, for example with a ref per base commit in the user's repository
   (`refs/studio/base/<id>`), or copy the needed packs into the base.
5. **Build output reuse is not shown, and absolute paths block it.** The bases in these tests had no
   build output. Ten Rust builds made 33 GB of agent layers. In the independent Swift check, the
   module cache from another absolute path was rejected in both variants. Every agent mount has its own
   path, so build caches stored in the base must be free of absolute paths, use path remapping, or be
   rebuilt per agent. A full Xcode project was not tested.
6. **Linux under parallel load.** Ten overlay mounts were 4 to 6 times slower than one shared native
   tree for parallel `git status` and `rg` (3.4 s against 0.6 to 0.8 s). The native runs shared one
   tree and one cache, so this is a lower bound for native, but the gap is large.
7. **Mistakes through the network** (`git push --force`, cloud command line tools, production APIs)
   are outside this design. Do not give the agent such credentials by default.

## Not measured

- Agents that live for days. Three build and clean cycles are the only proxy.
- Update of a base to a new version. Plan: `cp -c` of the base file (O(1)), attach read-write, apply
  the FSEvents delta, detach. Expected O(changes).
- Checkpoint and restore with the shadow file variant.
- Real cold start after a reboot (the clone method removes the page cache, not the SSD cache).
- A full Xcode build inside an agent mount under a sandbox (blocked, see above), and reuse of build
  output stored in the base.
- Windows (differencing VHDX, ProjFS).
- Linux on bare metal and on ext4 or XFS.
- The process sandbox together with the image.
- More than 10 agents.

## Appendix: commands

macOS base and agent:

```sh
# base: blank sparse ASIF with free space, then copy (in parallel for large trees)
diskutil image create blank --format ASIF --size 200g --volumeName base --fs APFS base.asif
diskutil image attach --nobrowse --mountPoint /path/to/build base.asif
# copy files except .git/objects, then for each .git directory:
#   echo /user/repo/<path>/.git/objects > <path>/.git/objects/info/alternates
#   git --git-dir=<path>/.git config core.checkStat minimal
#   git --git-dir=<path>/.git config core.trustctime false
git -C /path/to/build status --porcelain   # index refresh
diskutil eject <device of /path/to/build>

# agent (chosen): clone the detached base file, attach read-write; remove = eject + delete the file
cp -c base.asif agent.asif
diskutil image attach --nobrowse --mountPoint /path/to/agent agent.asif
# checkpoint: eject, cp -c agent.asif agent.ckpt.asif, attach again

# agent (fallback): attach the base with its own shadow; remove = eject + delete the shadow file
diskutil image attach --nobrowse --mountPoint /path/to/agent base.asif --shadow agent.shadow
```

Linux agent:

```sh
btrfs subvolume snapshot -r /data/base /data/base-v1           # freeze one base version
mkdir -p /ws/a/{u,w,m}
unshare -Urm mount -t overlay overlay \
  -o lowerdir=/data/base-v1,upperdir=/ws/a/u,workdir=/ws/a/w /ws/a/m
```

FSEvents delta tool (`swiftc -O fsdelta.swift -o fsdelta`):

```swift
// fsdelta now            -> print the current FSEvents event id
// fsdelta since ID PATH  -> print paths changed under PATH since event ID, one per line
import CoreServices
import Foundation

let args = CommandLine.arguments
if args.count >= 2 && args[1] == "now" { print(FSEventsGetCurrentEventId()); exit(0) }
guard args.count == 4, args[1] == "since", let since = UInt64(args[2]) else { exit(2) }
let root = (args[3] as NSString).standardizingPath
var changed = Set<String>()
let callback: FSEventStreamCallback = { _, _, count, paths, flags, _ in
    let list = unsafeBitCast(paths, to: NSArray.self) as! [String]
    for i in 0..<count {
        if flags[i] & UInt32(kFSEventStreamEventFlagHistoryDone) != 0 {
            for p in changed.sorted() { print(p) }
            exit(0)
        }
        changed.insert(list[i])
    }
}
let flags = UInt32(kFSEventStreamCreateFlagFileEvents | kFSEventStreamCreateFlagUseCFTypes | kFSEventStreamCreateFlagNoDefer)
let stream = FSEventStreamCreate(nil, callback, nil, [root] as CFArray, since, 0, FSEventStreamCreateFlags(flags))!
FSEventStreamSetDispatchQueue(stream, DispatchQueue.main)
FSEventStreamStart(stream)
dispatchMain()
```

A production version must also handle `kFSEventStreamEventFlagMustScanSubDirs` (rescan that folder)
and a history that no longer reaches back to the event id (rebuild the base).

## Sources

- [FSKit overlay feasibility](2026-09-30-fskit-overlay-feasibility.md) (this repository)
- [nfsserve](https://github.com/xetdata/nfsserve), commit d12c92dd, `examples/mirrorfs.rs`
- [AgentFS overlay](https://turso.tech/blog/agentfs-overlay), [AgentFS manual](https://github.com/tursodatabase/agentfs/blob/main/MANUAL.md)
- [GhostVM for AI agents](https://ghostvm.org/ghostvm-for-ai-agents)
- [Tart: two macOS VM limit](https://github.com/cirruslabs/tart/discussions/1054), [macOS VMs in the Virtualization framework](https://macops.ca/macos-monterey-apple-silicon-vms/)
- [Claude Code sandboxing](https://code.claude.com/docs/en/sandboxing), [Bazel sandboxing](https://bazel.build/docs/sandboxing), [Bazel sandboxfs](https://blog.bazel.build/2018/04/13/preliminary-sandboxfs-support.html)
- [Linux overlayfs documentation](https://docs.kernel.org/filesystems/overlayfs.html) (changes to underlying file systems)
- `man diskutil`, `man hdiutil`, `man 2 clonefile`, `man mount_nfs` on macOS 27
