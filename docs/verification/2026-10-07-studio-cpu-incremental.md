# Studio CPU update, 2026-10-07

This update removes repeated database and path work.
It retains the existing polling periods and background CPU budgets.
The baseline is commit `1957c559`.

## Changes

- The supervisor Journal retains up to four idle SQLite connections.
  Each lease owns one connection.
  A busy pool opens another connection without waiting for a lease.
  Each transaction retains FULL durability and its exact receipt identity.
  Errors discard the connection. Close rejects new leases and lets active leases finish.
- The scheduler stores the latest change generation for each agent ID.
  The existing generation triggers update this metadata in the same transaction.
  Reads check only changed IDs when work owners, transfer roots, and the schema remain unchanged.
  Unknown coverage, an older snapshot, or changed dependencies use the full predicate.
  Uncommitted rows never enter the committed roster cache.
  The decode cache limit no longer causes a KeyError above 4,096 selected rows.
- The history importer projects ID and threadId once per round.
  It retains JSON types, record order, and a fresh actor read for each step.
  Older SQLite versions retain the previous full-record path.
  Budget receipts and checkpoints remain unchanged.
- The disk scanner groups aliases and lexical exclusions once per pass.
  Dynamic priorities retain the previous agent order.
  Aliases share the first sample and its actual timestamp.
  A changed nested-worker set invalidates the parent cache.
  APFS byte measurements, hardlink handling, and identity checks remain unchanged.

## Controlled measurements

Each pair uses the same inputs and measures CPU time.

| Operation                 |      Before |      After | Input                                                |
| ------------------------- | ----------: | ---------: | ---------------------------------------------------- |
| Supervisor Journal        |  870.713 ms |  85.930 ms | 128 events, median of five runs                      |
| Journal connections       |         390 |          1 | Same append, poll, ACK, and duplicate-input receipts |
| Scheduler                 | 1404.899 ms |  37.530 ms | 2,192 agents, 24 update, commit, and read cycles     |
| Full scheduler reads      |          24 |          0 | Same 17,237,067 bytes of agent records               |
| History roster            |  602.597 ms | 157.094 ms | Three refreshes, 2,186 agents, median of three runs  |
| History JSON decode bytes |  36,656,102 |     64,516 | One roster refresh                                   |
| Disk metadata             | 6541.472 ms |  22.413 ms | 1,024 IDs, 512 roots, median of five runs            |
| Disk root signatures      |       1,024 |        512 | Same order and size result                           |

The disk comparison uses mock filesystem measurements.
It measures path selection and grouping, not the time to read files.
The 512 physical measurements remain unchanged.

The scheduler comparison includes writes and commits.
Writer CPU p95 changes from 0.504 to 0.703 ms.
Writer wall p95 changes from 0.639 to 0.922 ms.
The extra metadata writes reduce the total cost of the paired workload.

The history ledger and checkpoint hash matches in both variants:
`1662e38b2c8ba6bbd3be45153b29e96e227f77b223a9e92f5746d9a3c9cfa1fa`.
The supervisor receipts and disk result hashes also match.
The old implementations fail the corresponding performance checks.

## Correctness checks

Source checks pass: supervisor 25, history 64, disk 40, and scheduler 56.
The scheduler checks include startup, work ownership, transfers, rollback, older WAL snapshots, and the decode cache limit.
Independent reviews find no remaining source defect in the final changes.

The review finds and fixes an additional cache defect.
After a generation trigger disappears, the second untracked write could reuse the first full-query result.
The roster key now verifies all three generation triggers in the same snapshot as the generation.
Unknown triggers disable both the roster cache and the dispatch closure cache.
Schema changes and rollback cannot reuse an unproved cache era.

The history tests also check the older SQLite fallback.
Projection-only checks skip there; membership and round checks still run.
The disk tests retain actual APFS, clone, sparse-file, hardlink, symlink, and exclusion checks.

## Live boundary

The separate supervisor owns the native stdin and stdout pipes.
It has no code reload or pipe handoff action.
Its restart recovery sends TERM, and can send KILL, to surviving native process groups.
Replacing backend functions cannot change that daemon's Journal objects.
The new supervisor source therefore applies at its next safe start.
It does not reduce the current daemon's CPU use.

The live backend uses the legacy protocol 2 constructor.
The applied backend release retains that constructor, process identity, native work, and state-directory identity.
The scheduler metadata installer uses a separate bounded transaction outside Runtime.lock.
The function updater performs no SQL under Runtime.lock.

## Applied backend evidence

The backend applies the three optimizations without a restart.
The receipt reports `applied`, attempt 1, PID 3058, and the existing manager `9fe9bda2888940d1ac5bb6a15c138c7b`.
The manifest SHA-256 is `9389a5a9f8c3f9b8fdf4f1f3648c21f7fefc87b0b196282d656aa8beff1d9864`.

The scheduler metadata transaction retains generation 1,111,569 and installs an empty journal at that coverage floor.
It changes no agent record.
The installer uses the existing database inode and a 50 ms busy timeout.
The private installed-loader and metadata CLI checks pass: 91 checks.

The before and after snapshots retain all seven native process identities and handle generations.
All three agents with an active turn retain their epoch and thread identity.
Four native handles receive new events after publication.
The backend, supervisor PID 92109, manager, and state-directory identities remain unchanged.

The eight-second sample completes with 322 rounds.
Its file has mode 0600 and SHA-256 `0b493e99003177e835df5cc91b811e56ea9221b205e118d0f19b633dbc6056e1`.
The exact manifest and two temporary helpers are then removed.
The four permanent source files retain their reviewed hashes.

Two separate 30-second measurements report backend CPU use of 104.49% before and 83.20% after.
These measurements include changing user work and are observations, not a controlled comparison.
The scheduler thread reports 21.78% before and 16.65% after.
History and disk threads still report 15.05% and 13.64% after.
Their remaining work includes history steps and physical APFS file measurements.
The compact roster and grouped paths reduce only part of that work.
The supervisor connection pool remains staged for a future safe start.
It is not active in the current supervisor daemon.

Private source evidence:

- `/private/tmp/studio-supervisor-journal-cpu-20261007`
- `/private/tmp/studio-history-agent-projection-benchmark-20261007.json`
- `/private/tmp/studio-worktree-groups-20261007`
- `/private/tmp/studio-scheduler-incremental-final-exact-benchmark-20261007.log`
- `/private/tmp/studio-cpu-incremental-20261007`
- `/private/tmp/studio-cpu-incremental-live-20261007`

Controlled tests do not establish a constant live CPU limit or model reliability.
