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

| OS      | Mechanism                                                                   | Status                           |
| ------- | --------------------------------------------------------------------------- | -------------------------------- |
| macOS   | Read-only ASIF disk image as the base, one shadow file per agent            | Measured. Meets all requirements |
| Linux   | overlayfs: lower is a read-only btrfs snapshot, upper is a folder per agent | Measured. Meets all requirements |
| Windows | Differencing VHDX or ProjFS                                                 | Not measured                     |

Common model for all platforms:

- **Base.** A read-only copy (macOS) or snapshot (Linux) of the repository at one point in time. It
  is a cache. It can be deleted and built again. One base serves all agents of that repository.
- **Agent layer.** The agent writes only to its own layer: a shadow file (macOS) or an upper folder
  (Linux). One agent equals one file or one folder, so its disk cost is visible.
- **Fresh user edits.** At agent start, Studio copies the files that changed since the base was built
  into the agent layer. FSEvents gives this list in O(changes).
- **Result.** The agent commits to a git branch. Studio fetches the branch into the user's repository.
- **Removal.** Detach and delete one file (macOS), or unmount and delete one folder (Linux).

## Options and verdicts

| Option                                      | Start for chromium                         | Speed                    | Verdict                                                  |
| ------------------------------------------- | ------------------------------------------ | ------------------------ | -------------------------------------------------------- |
| git worktree (today)                        | full checkout, minutes                     | native                   | No isolation of `.git`, no warm build, disk spreads      |
| APFS clone of the tree (`cp -c -R`)         | 241 s                                      | native                   | Too slow to start. Delete takes 106 to 339 s             |
| APFS `clonefile()` of the directory         | 30 s                                       | native                   | Too slow to start for large repositories                 |
| Pool of prepared clones                     | 0 s                                        | native                   | Rejected: the agent must be universal                    |
| Clone in parallel with the first model turn | up to 30 s wait                            | native                   | Rejected: the workspace must be ready at once            |
| Local NFS overlay server                    | 0.06 s                                     | `git status` over 20 min | Rejected: too slow on large trees                        |
| FSKit overlay module                        | O(1)                                       | XPC call per cache miss  | Rejected: slow and buggy by Apple's own account          |
| Linux VM with overlayfs on macOS            | O(1)                                       | native Linux             | Rejected: no macOS builds                                |
| macOS VM per agent                          | APFS clone of VM disk                      | native                   | Not chosen: at most 2 macOS VMs at once, GBs of RAM each |
| OSTree                                      | O(1) hardlink checkout                     | native                   | Linux only. Opaque object store                          |
| **ASIF image + shadow (macOS)**             | **about 1 s**                              | **native**               | **Chosen**                                               |
| **overlayfs + btrfs snapshot (Linux)**      | **0.00 s mount, 0.49 s snapshot per base** | **native for 1 agent**   | **Chosen**                                               |

## Prior art

| Project                                                                                                         | Mechanism                                                                                                                                               | Difference from our design                                                                                                                                  |
| --------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [AgentFS](https://turso.tech/blog/agentfs-overlay) (Turso)                                                      | Copy-on-write overlay: host folder read-only, agent changes in SQLite. macOS: localhost NFS server, `mount_nfs`, Seatbelt profile, no root. Linux: FUSE | Closest idea. Every file call goes through a user-space server. No published speed numbers. Not measured here                                               |
| [GhostVM](https://ghostvm.org/ghostvm-for-ai-agents), [Tart](https://github.com/cirruslabs/tart)                | Full macOS VM per agent, VM disk is an APFS clone                                                                                                       | Strong isolation and native macOS builds. Apple allows at most 2 macOS VMs at once ([Tart discussion](https://github.com/cirruslabs/tart/discussions/1054)) |
| [Claude Code sandbox](https://code.claude.com/docs/en/sandboxing), [Bazel](https://bazel.build/docs/sandboxing) | Seatbelt (`sandbox-exec`) on macOS, bubblewrap or namespaces on Linux                                                                                   | Limits where a process can write. No private copy of the workspace. We need this in addition to the image                                                   |
| [Bazel sandboxfs](https://blog.bazel.build/2018/04/13/preliminary-sandboxfs-support.html)                       | FUSE file system with a custom view of files                                                                                                            | FUSE on macOS needs a kernel extension or the slow FSKit backend                                                                                            |
| Meta EdenFS                                                                                                     | Virtual file system for source control. NFSv3 on macOS, FUSE on Linux, ProjFS on Windows                                                                | Needs its own source control (Sapling) for fast status                                                                                                      |
| Apple DTS advice (forum thread 828533, see the FSKit report)                                                    | For build sandboxes: clone the tree, or use disk images plus EndpointSecurity, not a projection file system                                             | Points to disk images, but no product does it                                                                                                               |

No public project uses a disk image with a shadow file for agent workspaces, as far as the search on
2026-10-03 shows. `hdiutil -shadow` itself is old and well known for changing read-only images.

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

### Linux overlayfs (OrbStack machine, blink and trading-bot)

| Operation                        | Native                      | overlayfs               |
| -------------------------------- | --------------------------- | ----------------------- |
| mount                            | none                        | 0.00 s                  |
| walk with `stat`, cold / warm    | 0.73 s / 0.38 s             | 1.19 s / 0.49 s         |
| `git status`, cold / warm        | 0.33 s / 0.14 s             | 0.47 s / 0.17 s         |
| `rg` over all files, cold / warm | 0.36 s / 0.25 s             | 0.40 s / 0.26 s         |
| write 2,000 small files          | 0.03 s                      | 0.03 s                  |
| unmount and delete upper         | none                        | 0.22 s                  |
| 10 mounts                        | none                        | 0.03 s                  |
| 10 parallel `git status`, cold   | 0.58 s (one shared tree)    | 3.43 s                  |
| 10 parallel `rg`, cold           | 0.80 s (one shared tree)    | 3.37 s                  |
| 1 cargo build (trading-bot)      | 36.6 s                      | 35.4 s, upper 3.5 GB    |
| 10 cargo builds in parallel      | not finished at commit time | 380.0 s, 10 of 10 built |
| delete 10 agents after builds    | not finished at commit time | 14.6 s                  |

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
7. **The agent layer only grows.** A deleted `target/` stays in the shadow until the agent is removed.
   Budget disk per agent and remove finished agents.
8. **The image does not stop writes outside the workspace.** A process sandbox (Seatbelt on macOS,
   namespaces or Landlock on Linux) must allow writes only to the agent mount, shared caches and
   temporary folders.
9. **Build the base in parallel.** 16 `tar` streams are 4 times faster than one on macOS.

## What this does not solve

- Writes outside the workspace: needs the process sandbox (rule 8).
- Mistakes through the network: `git push --force`, cloud command line tools, production APIs. Do not
  give the agent such credentials by default.
- The first agent in a new large repository waits for the base: about 16.5 min for chromium on macOS,
  seconds for small repositories. On Linux with btrfs the base is a reflink copy plus a snapshot.

## Not measured

- Agents that live for days: shadow growth and fragmentation.
- Update of a base to a new version. Plan: `cp -c` of the base file (O(1)), attach read-write, apply
  the FSEvents delta, detach. Expected O(changes).
- Checkpoint and restore of an agent: `cp -c` of the shadow file while detached or frozen.
- Real cold start after a reboot (the clone method removes the page cache, not the SSD cache).
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

# agent: attach with its own shadow; remove = eject + delete the shadow file
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
