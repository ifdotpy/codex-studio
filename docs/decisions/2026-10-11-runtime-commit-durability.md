# Runtime database commits stay fully durable

- **Status:** Accepted, 2026-10-11, by the maintainer.
- **Audience:** anyone who changes how the server writes its SQLite database or
  who investigates the time spent under the runtime lock.

## In short

Every change the server commits to its database is forced onto the disk before
the server continues. That costs about one millisecond per commit. We keep it,
because it is the simple and safe choice. We will look at faster options only
when measurements show that this cost has become a bottleneck.

## Context

The runtime database is SQLite in write-ahead-log mode (WAL: changes are first
appended to a log file). `Runtime` selects WAL explicitly and sets no
`synchronous` level, so SQLite's default applies: `FULL`. With `FULL`, each
commit waits until the log has been synced to the disk.

Measured on 2026-10-11 on a disposable database, single thread, 300 timed
transactions, on the same ext4 NVMe device that holds the real state:

| Step                                       | Median time |
| ------------------------------------------ | ----------- |
| Small indexed update                       | 0.02 ms     |
| Commit with `synchronous=FULL` (current)   | 1.08 ms     |
| Commit with `synchronous=NORMAL`           | 0.015 ms    |
| Commit that also writes the analytics file | 2.1 ms      |

The runtime lock is held across the commit. Handling of native notifications
(messages from the provider process about a running turn) commits once per
notification: a read-only snapshot of the running server showed 4,424 such lock
holds in 27 minutes, 26.5 s in total, out of 83 s for all call sites.

Not measured: tails on a large database, commits that cross the checkpoint
threshold, and the settings of the running server (they follow from the source;
they were not read from the live process).

The project rule that this decision protects: a retry must not repeat a command,
a user message, or a reset-credit charge. That rule depends on an acknowledged
record still being there after a crash.

## Alternatives considered

1. **`synchronous=NORMAL` for the whole runtime database.** Removes about 1 ms
   per commit. Committed data survives a crash of the server process, and the
   database stays consistent. After a power loss or kernel crash the most recent
   commits can be rolled back, so an acknowledged request identity could be
   missing and a retry could repeat it. Rejected: it weakens the rule above.
2. **`NORMAL` only for notification handling, `FULL` elsewhere.** A partial
   read of the source (stopped when this decision was taken) found that
   notification transactions are mixed. Text and command output can usually be
   rebuilt from the provider's own history. But the transaction for an item's
   start or completion can also record that a user input was delivered, bind an
   attempt to its turn, and store a question or approval request; a turn start
   records the turn identity that resend decisions depend on. So one level per
   notification kind would not separate safe data from critical data. Not
   traced: turn completion, tool-request receipts, and replay by the supervisor
   after an acknowledged event is lost. Not pursued for now.
3. **One transaction for several notifications.** Keeps `FULL` after the commit;
   a process crash loses the uncommitted batch, and state becomes visible later.
   Not pursued for now.
4. **Commit outside the runtime lock where independence is proven.** Keeps
   durability and shortens lock holds, but needs a correctness proof per path.
   Not pursued for now.
5. **`synchronous=OFF`.** Not considered acceptable: SQLite can corrupt the
   database after a power loss.

## Decision

Keep WAL with `synchronous=FULL` for the runtime database, for every writer,
including notification handling. Do not add per-path durability levels, batching
of notifications, or commits outside the lock for the purpose of speed.

Revisit only when a measurement on the running server shows that commit time is
a bottleneck for users or agents: for example a profile in which commit sync is
the dominant part of runtime-lock time after structural fixes, with visible
waiting as a result. A revisit starts from alternative 2 or 4 and needs its own
decision record.

## Consequences

- Each committing lock hold carries about 1 ms of sync on this hardware. Work
  that reduces the number of commits or the work done under the lock remains
  worthwhile and needs no new decision (for example skipping a write whose
  record did not change).
- A commit that writes two database files pays two syncs. Avoid writing the
  runtime and analytics files in one transaction on hot paths.
- Tests do not pay this cost when their temporary state is on a memory-backed
  directory, which the server test runner selects for both full and focused
  runs. A test timing taken on disk mostly measures the sync.
- A later Rust storage owner must keep the same durability unless a new decision
  record says otherwise.

## Related sources

- Issue #37, comments of 2026-10-11: the measurement and the options
  (`issuecomment-6108707463`) and the decision
  (`issuecomment-6109473646` and the follow-up).
- `workspaces/runtime/apps/server/src/codex_runtime.py`: `Runtime.db`,
  `Runtime.notification_db`, and the `PRAGMA journal_mode=WAL` statements.
- `workspaces/runtime/apps/server/src/codex_sqlite.py`: connection setup without
  a `synchronous` override.
- [Workspace and Rust migration plan](../plans/rust-workspaces.md), R-16.
- SQLite documentation: [`PRAGMA synchronous`](https://www.sqlite.org/pragma.html#pragma_synchronous)
  and [write-ahead logging](https://www.sqlite.org/wal.html).
