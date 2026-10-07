# Studio CPU algorithms, 2026-10-07

This update removes repeated work found after the first CPU update.
It does not reduce the existing background CPU budgets further.

## Profile and changes

The same live backend has process ID 3058.
An eight-second Python frame sample identifies the scheduler, disk scanner, and history importer.
The sample records code locations and thread identities, without frame values or message text.

- The history importer opens a budget database only when a migration page contains usage rows.
  Empty and completed pages retain their cursor checks.
  Budget commits still precede the analytics checkpoint.
  A lost checkpoint uses the same budget receipt identities on retry.
- The scheduler caches its roster against agent generation, work owners, and pending transfer roots.
  Unrelated task writes no longer cause a full agent query.
  It does not cache uncommitted rosters.
  A missing generation counter retains the full query.
- The disk scanner reads APFS attributes in directory batches.
  It retains exact PRIVATE bytes, hardlink deduplication, exclusions, and the existing CPU budget.
  Incomplete attributes, directory identity changes, mounts, and firmlinks use the existing path scan.
  The scan does not use file timestamps as a substitute for PRIVATE bytes.
- Each rule phase decodes an unchanged owner once.
  The read and write phases use separate caches.
  A SQL write or agent revision change prevents the next read from using the old cache.
  The existing permission, epoch, recovery, and file checks remain intact.

The live database contains 95 active rules without an active command and 14 unique owners at the measured point.
Repeated owner records contain 14,908,652 bytes per phase, versus 785,567 bytes for unique owners.

## Controlled checks

These results use the same inputs for each pair.
They measure CPU time, rather than the effect of longer pauses.

| Operation              |     Before |     After | Input                                                     |
| ---------------------- | ---------: | --------: | --------------------------------------------------------- |
| History CPU            | 135.328 ms | 83.803 ms | 24 idle steps, median of three runs                       |
| History SQL commands   |        336 |       120 | 12 idle steps                                             |
| Scheduler CPU          | 266.061 ms |  1.924 ms | 2,048 agents, 64 selected, eight unrelated writes         |
| Full scheduler queries |          8 |         0 | Same warm roster                                          |
| Disk CPU               |  90.786 ms | 41.816 ms | One native APFS pair                                      |
| Disk native calls      |      2,062 |        20 | Same tree, 8,392,704 bytes                                |
| Rule CPU               |  65.140 ms |  2.810 ms | 128 quiet rules, 131,072-byte owner, median of seven runs |
| Rule owner reads       |        256 |         2 | Same rules and owner                                      |

The history budget ledger and checkpoint have the same hash in both variants.
The native disk test includes clones, sparse files, hardlinks, symlinks, exclusions, and multiple buffers.
The roster tests cover rollback followed by another writer that reuses the generation value.
The rule tests cover owner changes between phases and writes within the same tick.

New and related checks pass: history 57, scheduler 24, disk 34, and rules 28.
The old implementations fail their corresponding regression checks.
Independent source reviews find no concrete defect in the four final changes.

## Live evidence

The guarded update applies on attempt 1 under the existing update manager.
The backend keeps its process ID, start time, constructor, protocol, connections, and disk loop.
Only six existing functions and five supporting helpers change.
The installed compatibility package passes 52 private checks with the actual compiler and loader.

Each total below comes from cumulative process CPU time over a separate 30-second window.
Thread CPU time comes from Darwin `proc_pidinfo`.
100% means one processor core.

| Window                          | Backend | Scheduler | History |   Disk |
| ------------------------------- | ------: | --------: | ------: | -----: |
| Earlier baseline, same old code |  47.84% |    14.28% |   9.84% | 13.62% |
| Final baseline                  |  72.41% |    22.65% |  14.15% | 14.05% |
| After algorithms                |  55.25% |    11.03% |  13.04% | 13.69% |
| After the old disk pass ends    |  63.08% |    13.74% |  13.41% | 13.16% |

The workload changes between windows.
The controlled pairs above establish less work for the same input.
The live totals do not establish a constant CPU limit.
The first eight-second after sample still contains an old disk path scan that started before the update.
The scanner finishes that pass without interruption.
The later sample contains 90 samples inside `_apfs_bulk_entries` and no old `_tree_bytes` frame in the disk thread.
That eight-second sample uses 107.86% CPU, which confirms that short peaks remain.
The scheduler still runs full queries when agent records or roster dependencies change.
The disk scanner and history importer still use their budgets while their work continues.

- Algorithm manifest: `75c4b691f7ffd4b3194badb84e1236a0b49a8dfb756b2d6977e159888304a815`.
- Installed Runtime source: `adde0de68fd15150f1f5b4f02b50c0bee049d27746378c4bc139a10a3f35ad14`.
- All source syntax trees match the installed originals after the exact changed functions are removed.
- The three previously active agents retain their epochs.
- Sixteen new events have delivery status and native turn IDs after application.
- The algorithm manifest and temporary helpers are removed after the exact receipt and sample are saved.
- The later sample manifest is `cf07a7fac60c4744e3286f2f31637b32252a5af6e79ec460c34e2736a7b6af4a`.
  Its exact receipt and output are saved before its temporary files are removed.

The first read-only sample package fails the installed patch-name check before import.
The rejection does not alter application functions.
The corrected package uses the required name and the actual installed loader check.

Private evidence: `/private/tmp/studio-cpu-phase2-baseline-20261007`,
`/private/tmp/studio-cpu-phase2-retry-20261007`,
`/private/tmp/studio-cpu-algorithms-live-20261007`, and
`/private/tmp/studio-cpu-bulk-sample-20261007`.

## Remaining work

The separate supervisor also uses CPU.
Its native sample shows repeated SQLite open, prepare, and commit calls in the journal thread.
The supervisor has no compatible function update mechanism.
This update does not replace or restart that process.
Changing its journal connections requires separate tests of child streams, acknowledgements, and durable commits.

The controlled tests do not establish a constant live CPU limit or live model reliability.
