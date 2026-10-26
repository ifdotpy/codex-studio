# layr 0.2.0 cache reuse in Studio agent lines

Date: 2026-10-25. Task: E (`work-1792917911219744000-4`).

Warm lines reuse Rust and pnpm output after an explicit `layr save --caches` in main.
In the large APFS run, Cargo reused 285 of 286 packages.
It rebuilt the HTTP package because the build script used paths from main.
A one-file edit also rebuilt one package.

## Scope and limits

- Studio source: `1c2e4c1e3f39bf38de9da1fd8c72706974878ec9`.
- layr source: `efa2fa142b44c278d264f59355d0d97d815fe5ba`, branch `release/v0.2.0`.
- Private binary: `/Users/igor/layr-cache-bench-e/layr-target/debug/layr`.
- Private store: `/Users/igor/layr-cache-bench-e/store`.
- The input was a Git archive of the Studio source, with two cache attributes added.
- Package: `studio-runtime-http`, with its transitive dependencies.
- Commands: `cargo build -v -j 4 --locked -p studio-runtime-http` and `pnpm install --frozen-lockfile`.
- Host: APFS, arm64, 18 logical CPUs, 128 GiB RAM.
- Tools: Cargo 1.99.1, rustc 1.99.1, pnpm 12.15.0, sccache 0.16.0.
- Other workers built at the same time, as authorized. These times do not measure isolated performance.
- No synthetic load ran. This worker ran one Rust build at a time.
- `Fresh` and `Compiling` count Cargo status lines. They are package counts, not exact compiler-unit counts.
- The report also counts actual `rustc` and build-script command lines.
- The baseline used no `RUSTC_WRAPPER` or external `CARGO_TARGET_DIR`.
- Its compiler commands contained no incremental option; its incremental folder was empty.
- The Linux VM was unavailable. No VM helper process, `control.sock`, or `guest.sock` existed.
- No Studio source, installed layr binary, or VM project changed.
- pnpm read and updated its existing user cache metadata during installs.

## Rust measurements

| Run                         | Seconds | Fresh | Compiling | rustc commands | Build scripts | Exit |
| --------------------------- | ------: | ----: | --------: | -------------: | ------------: | ---: |
| Main, empty target          | 386.815 |     0 |       286 |            349 |            37 |    0 |
| Main, no change             |   1.152 |   286 |         0 |              0 |             0 |    0 |
| w1, saved caches            | 180.391 |   285 |         1 |              1 |             1 |    0 |
| w1, no change               |   1.947 |   286 |         0 |              0 |             0 |    0 |
| w1, one source edit         | 136.980 |   285 |         1 |              1 |             0 |    0 |
| batch-2, --count 2 (failed) | 218.174 |   285 |         1 |              1 |             1 |  101 |

The main build compiled dependencies from an empty target folder.
The next main build reused every package.
The warm w1 build ran one build script and one compiler command.
Cargo reported this exact reason:

```text
Dirty studio-runtime-http: the rerun-if-changed instructions changed
```

The HTTP build script prints absolute paths from `CARGO_MANIFEST_DIR`.
It also writes absolute generated-module paths under `OUT_DIR`.
See `workspaces/runtime/packages/runtime-http/build.rs`.
The saved `.d` file contains main's absolute output and generated-source paths.

Most ordinary workspace source dependencies in that file are relative.
The `.fingerprint` JSON normalizes the HTTP input paths to relative paths.
Thus, absolute strings in `.d` files do not imply a full dependency rebuild.

The one-file test appended a comment to `runtime-http/src/lib.rs` in w1.
It ran one compiler command, no build script, and reused 285 packages.
This baseline does not measure compiler incremental reuse.

The `--count 2` command made `batch-1` and `batch-2` from the same main state.
Both received warm folders. The batch-2 build reused 285 packages.
Its one HTTP compile failed with `No space left on device` while it wrote the archive.
The failed duration is not a valid successful build time.

No large retry ran after that failure.

The line created before the save still had no target or node_modules folder afterwards.
Disk pressure prevented a second full HTTP build there.
A smaller `studio-diagnostics` build compiled all six packages, reused none, and passed in 7.355 seconds.
This proves the old line stayed cold, but it is not a second full HTTP timing.

`cargo clean` removed 2,838 files and reported 3.4 GiB in batch-1.
Then `target` disappeared from `layr cache list`.
With attributes alone, discovery lists existing folders, so `layr cache repair` left target absent.
After a tool recreated target with one marker file, repair made it a cache again.

Only the marker remained. Repair did not restore saved compiler output.
This matches repair's role as folder conversion, rather than artifact recovery.

## pnpm measurements

| Run                                  | Seconds | Added packages | Downloads |
| ------------------------------------ | ------: | -------------: | --------: |
| Main, empty node_modules             |   5.691 |            667 |         0 |
| w1, saved node_modules               |   3.733 |             48 |         0 |
| batch-2, first pnpm exec             |   2.732 |             48 |         0 |
| batch-2, next install                |   0.187 |              0 |         0 |
| Before-save line, empty node_modules |   6.643 |            667 |         0 |

The first main install used the existing store at `/Users/igor/Library/pnpm/store/v11`.
It added 667 packages and downloaded none.
The w1 install added 48 packages and downloaded none.
Thus, the main install was not a cold package-store test.

The warm line still needed an install pass.
The first `pnpm exec tsc` in batch-2 also ran an automatic install that added 48 packages.
Its next explicit install added none.
Direct execution of the untouched batch-1 TypeScript shim passed in 0.320 seconds before any install in that line.

The root `.modules.yaml` retains the absolute shared `storeDir`.
Its `virtualStoreDir` is `.pnpm`, relative to `node_modules`.
The TypeScript shim in pnpm 12.15.0 calculates `basedir_abs` at execution.
It uses that value for `NODE_PATH` and the script path.

It does not retain main's absolute line path.
This differs from the generic pnpm warning in layr's design document.
The result applies to the installed pnpm version and this shim.

The TypeScript file had link count 1 in each line and different inode numbers.
Its APFS clone identity was the same in main and w1.
This run used APFS clones, not hard links between lines.
The pnpm log explicitly says it clones packages from the store.

## Cache state, disk use, and APFS sharing

The following six folders showed `cache` in main and w1 after the save:

```text
node_modules
target
workspaces/client/apps/desktop/node_modules
workspaces/client/apps/native/node_modules
workspaces/client/apps/web/node_modules
workspaces/providers/apps/claude-bridge/node_modules
```

Before the save, those folders showed `plain folder (run layr cache repair)` in main.
The command took 270.612 seconds to save caches.
The w1 line took 28.760 seconds to create.
The two `--count` lines took 69.988 seconds together.

`du -sk` reported the same sizes in main and w1 before the worker build:

| Folder            | KiB per line | GiB per line |
| ----------------- | -----------: | -----------: |
| target            |    3,572,476 |        3.407 |
| root node_modules |      924,692 |        0.882 |

These values count shared blocks in each folder.
They must not be added to claim physical space for multiple lines.
The separate nested `node_modules` folders are not in the root row.

The sample files had distinct inodes but equal valid APFS clone IDs before worker writes:

| File               | Main inode |   w1 inode | Shared clone ID |
| ------------------ | ---------: | ---------: | --------------: |
| serde rlib         | 1255737852 | 1256014070 |      1255737852 |
| HTTP rlib          | 1255803849 | 1256014635 |      1255803849 |
| TypeScript library | 1255687332 | 1255980950 |      1255687332 |

The HTTP rlib was 462,500,336 bytes.
Its rewrite needs new blocks even though the original cache shares blocks.
No reliable aggregate physical-space delta was available because other workers wrote to the same volume.
Free space fell from 24 GiB to 6.8 GiB during these runs.

The ENOSPC error occurred before the final free-space observation.

## Confirmed layr defects

These findings were sent to the lead immediately, before this report.
No fix was made.

### E1: cache conversion loses timestamp precision and hard links

Source: layr `src/repo.rs:1927`, `copy_tree`, at `efa2fa1`.
Each file is created independently.
The copy sets its modification time with `set_mtime(&dst, m.mtime, 0)`.

Reproduce in an empty throwaway project with `target/` ignored:

```sh
printf 'target regenerable\n' > .layrattributes
mkdir target
printf x > target/a
ln target/a target/b
python3 - <<'PYTIME'
import os
os.utime('target/a', ns=(1700000000123456789, 1700000000123456789))
for p in ('target/a', 'target/b'):
    s = os.stat(p)
    print(p, s.st_ino, s.st_nlink, s.st_mtime_ns)
PYTIME
layr cache repair
python3 - <<'PYSTAT'
import os
for p in ('target/a', 'target/b'):
    s = os.stat(p)
    print(p, s.st_ino, s.st_nlink, s.st_mtime_ns)
PYSTAT
```

Measured before: same inode, link count 2, modification time `1700000000123456789` ns.
Measured after: separate inodes, link count 1, modification time `1700000000000000000` ns.
The next line retains those changed values.
The first `save --caches` uses this same conversion path.

Impact: conversion changes active main's cache metadata and hard-link behavior.
This can affect tools that depend on subsecond timestamps or linked-file identity.
No stale Studio binary was demonstrated from this defect.

### E2: first APFS cache conversion copies bytes

Source: layr `src/apfs.rs:541` and `src/repo.rs:1952`.
APFS `clone_file` always returns `ENOTSUP` for an existing destination inode.
The cache converter then calls `std::io::copy`.
The comment that this conversion uses reflinks is incorrect on APFS.

The E1 reproduction also changed the valid clone IDs during conversion.

Impact: the first save copies populated cache folders and can require temporary disk space.
The initial 270.612-second save included conversion of about 4.29 GiB in the two largest folders.
This observation does not isolate time spent in conversion from other save work.
Subsequent snapshots and lines do share APFS blocks, as the table above shows.

Workaround for E1 and E2: create cache folders before the first build.
For example, run `layr cache add target` before Cargo writes output.
For attribute-based folders, create the empty folders and run `layr cache repair` before the install or build.

## Focused path, incremental, and sccache probes

A separate one-package Rust fixture had a build script with an absolute input path.
It also embedded `CARGO_MANIFEST_DIR` in its output binary.
The initial seed made an automatic state because Cargo created an untracked Cargo.lock.
A line from main therefore had no warm target.

That initial probe is excluded from warm-reuse claims.
After the lockfile was saved in main, the next cache save attached to main's head.
This is a setup condition, not a layr defect.

With `CARGO_INCREMENTAL=1`, the corrected w2 probe received three saved `dep-graph.bin` files.
Each had the same APFS clone ID as main, with a different inode.
For example, the 477,166-byte build-script graph had clone ID `1256405326` in both lines.
The warm build reran the build script and two compiler commands in 2.741 seconds.

It printed the worker's manifest path, not main's path.
After the worker input changed, the binary printed `WARM-WORKER` and the worker path.
The rebuild took 0.874 seconds.
These checks prove that incremental files survive and the output uses the worker input.

They do not measure internal compiler query reuse or an incremental speed advantage.

A separate source touch, without a content change, caused Cargo to rebuild the small fixture.
Cargo reported that `src/lib.rs` had changed.
Preserved source timestamps therefore matter to cache reuse.

`cargo metadata` with an external `CARGO_TARGET_DIR` selected that external directory.
It did not select the line's copied target folder.
No external-target large build ran.

The sccache fixture was a library with no dependencies.
Each row below used a private server socket and private disk cache.
The servers were stopped after their runs.
Cargo rebuilt after each explicit clean, so compiler-cache hits came from sccache.

| Case                                                  | Rust misses | Rust hits | Result            |
| ----------------------------------------------------- | ----------: | --------: | ----------------- |
| Main, then w1, no basedirs                            |           2 |         0 | No cross-line hit |
| Main, then w1, line roots in SCCACHE_BASEDIRS         |           2 |         0 | No cross-line hit |
| Two builds in main, same server, clean between builds |           1 |         1 | Same-line hit     |

The same-server control took 3.132 seconds for the first build and 1.436 for the cache-hit build.
A separate same-path control after a server restart missed once.
The cause of that additional miss was not isolated.
The Rust source hashes the working folder and Cargo environment values without the basedirs normalization.

Do not treat the general README's basedirs advice as proof of Rust cross-line hits.

With `RUSTC_WRAPPER=sccache` and `CARGO_INCREMENTAL=1`, the installed tool refused the compiler version query.
Its exact message was `incremental compilation is prohibited: Unset CARGO_INCREMENTAL to continue.`
This failure is independent of layr.

## Other cache scope

The first two rows are measured. The remaining rows follow the inspected paths and cache contract.

| Data                             | Location                          | Reuse across lines                                                                   |
| -------------------------------- | --------------------------------- | ------------------------------------------------------------------------------------ |
| Rust output                      | line target                       | Saved clones; rebuild only what Cargo rejects                                        |
| pnpm virtual store               | line node_modules/.pnpm           | Saved clones; store validation can still need an install                             |
| Cargo registry and Git downloads | CARGO_HOME, normally ~/.cargo     | Outside line caches; same-user home reuse only unless explicitly shared              |
| pnpm content store               | pnpm store path                   | Outside line caches; the Mac run used an existing home store                         |
| rustc incremental graphs         | target/debug/incremental          | Included with target; confirmed by the focused probe                                 |
| sccache objects                  | SCCACHE_DIR or user default       | Outside line caches; separate sharing and trust policy                               |
| TypeScript helper state          | STUDIO_TSC_CACHE, TMPDIR, or /tmp | Root package.json selects an external tsbuildinfo path; not copied with node_modules |
| Ignored dist folders             | line-local output                 | Not caches under these two attributes; no saved output contract                      |

The actual Linux users have separate homes.
A warm target alone does not provide missing Cargo registry sources or Git dependencies to a new home.
That cross-user download case was not measured because the VM was unavailable.

## Recommended Studio setup

Track these attributes in main, with matching ignore rules:

```gitattributes
target regenerable
node_modules regenerable
```

The `node_modules` pattern covered the nested workspace folders in this run.
Only ignored, unversioned folders can become caches.
Add other regenerable outputs only when their tools support a new line path.
Do not assume a Python virtual environment or Xcode build folder can move safely.

Use this sequence in main:

1. Create the empty cache folders.
2. Run `layr cache repair`, or register each exact path with `layr cache add`.
3. Run the required build with `-j 4`.
4. Run `pnpm install --frozen-lockfile`.
5. Save any new source or lockfile changes into main through the normal review process.
6. Run `layr save --caches` after the build succeeds.
7. Create worker lines from that main state.

Keep `CARGO_TARGET_DIR` unset for normal workspace commands.
This puts the target folder inside the line and lets layr copy it.
If a tool needs an explicit value, use that worker line's target path.
Do not point all workers at main's target or one external mutable target.

An external target is outside the cache snapshot and weakens line isolation.

Keep the toolchain, build profile, features, and compiler flags consistent between main and workers.
A change to those inputs can invalidate the saved output.
Use relative `rerun-if-changed` paths where possible in build scripts.
Avoid unnecessary absolute generated-module paths.

A debug-path remap does not repair Cargo build-script input rules.

Run `pnpm install --frozen-lockfile` in each worker to validate and repair its links.
Keep its virtual store inside that line's `node_modules`.
Use a stable pnpm `store-dir` on the same filesystem when users can safely share it.
For one trusted Unix user, a private home store is sufficient.

For Studio's separate VM users, prefer per-user stores until shared-store permissions and trust are designed.
Saved protected-main `node_modules` folders can still warm workers without a shared writable package store.
A changed store path can require another install pass.
The real per-user VM case was not measured here.

A same-user simulation selected a new empty private store and used `pnpm install --offline --frozen-lockfile`.
It failed with `ERR_PNPM_NO_OFFLINE_TARBALL` for d3 in 21.001 seconds.
Warm node_modules alone therefore did not satisfy an install against a new store.
Keep the default store for the same trusted user, or use one explicit stable path for that user.

For example: `pnpm install --frozen-lockfile --store-dir "$HOME/.cache/pnpm/store"`.
Apply the choice consistently to main and all lines for that user.

Use these default per-line settings:

```sh
unset CARGO_TARGET_DIR
export CARGO_BUILD_JOBS=4
pnpm install --frozen-lockfile
```

Use sccache only as an additional compiler cache.
Keep its cache directory outside source snapshots and limit sharing to a suitable trust boundary.
For an optional same-user setup, use these values before the main build:

```sh
export RUSTC_WRAPPER=sccache
export CARGO_INCREMENTAL=0
export SCCACHE_DIR="$HOME/.cache/sccache"
```

Disable Cargo incremental compilation for crates whose objects sccache should cache.
Do not rely on `SCCACHE_BASEDIRS` for Rust cross-line hits in sccache 0.16.0.
The measured probe had no hits with that setting.
The Rust implementation hashes the working folder and Cargo environment values.

Use a stable compiler path and working folder if a future isolated namespace design needs cross-line hits.
That design was not tested here.
Do not change these variables after main is built and expect identical Cargo fingerprints.

Studio does not build and publish main caches automatically at the inspected revision.
`docs/vm-layr.md`, under “Warm caches”, assigns that decision to the operator or lead.
The VM gives each agent its own Unix user and home (`layr_agents.py:196`).
Saved caches cross user boundaries only when their source line is protected.

They also require a matching operating system and architecture (`layr/src/repo.rs`, `warm_cache`).
Mac output does not warm a Linux target.
It does not set Cargo, pnpm-store, or sccache cache variables in `agent_env`.
A shared tool or download cache therefore needs a separate explicit policy.

The inspected VM helper still calls `layr rev-parse HEAD`, which layr 0.2.0 rejects.
That CLI migration is separate work; no live Studio session against layr 0.2.0 was verified.

## Evidence and reproduction

Raw logs and probe scripts remain under `/Users/igor/layr-cache-bench-e`.
The [JSON companion](2026-10-25-layr-cache-reuse.json) contains command arguments, timestamps, counts, and selected filesystem samples.
The private layr compiler intermediates were removed after ENOSPC; the binary and evidence remain.

The core sequence was:

```sh
export LAYR_ROOT=/Users/igor/layr-cache-bench-e/store
layr init cabench --from /Users/igor/layr-cache-bench-e/import
cd "$LAYR_ROOT/projects/cabench/lines/main"
layr line new before main
cargo build -v -j 4 --locked -p studio-runtime-http
pnpm install --frozen-lockfile
layr save --caches
layr line new w1 main
cd ../w1
cargo build -v -j 4 --locked -p studio-runtime-http
pnpm install --frozen-lockfile
```

Read sources: layr `docs/commands.md`, `docs/apfs.md`, `docs/apfs-vs-btrfs.md`, and `docs/design.md` section 5.
Studio source: `docs/vm-layr.md`, `layr_agents.py`, `project_env.py`, and the HTTP build script.
The installed sccache 0.16.0 README documents path normalization and the incremental-cache limit.
The [Rust cache rules](https://github.com/mozilla/sccache/blob/v0.16.0/docs/Rust.md) require incremental compilation to be disabled.

The [Rust cache implementation](https://github.com/mozilla/sccache/blob/v0.16.0/src/compiler/rust.rs#L1450-L1475) hashes Cargo environment values and the working folder.
