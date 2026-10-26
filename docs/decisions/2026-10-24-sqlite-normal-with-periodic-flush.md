# SQLite commits use synchronous=NORMAL with a periodic flush

- **Status:** Accepted, 2026-10-24, by the maintainer. Supersedes
  [Runtime database commits stay fully durable](2026-10-11-runtime-commit-durability.md).
- **Audience:** anyone who changes how Studio opens, writes, or checkpoints a
  SQLite database.

## In short

Every Studio SQLite database in the Rust code uses WAL with
`synchronous=NORMAL`, for every transaction. A commit is no longer forced to
disk before the writer continues. A checkpoint flushes the committed changes to
disk at most once per flush interval (15 seconds), only when there is something
to flush, and once more when the server stops gracefully. After an OS crash or
a power loss, the commits of the last interval can be lost. A crash of the
server process loses nothing.

## Context

The revisit condition of the previous record was met. Live diagnostics showed
the single Canvas writer busy about 78% of the time, with commit medians of
80 to 470 ms, because every commit waited for an fsync on a saturated disk.

## Decision

- One owner holds the values: `SQLITE_WAL_FLUSH_INTERVAL_SECONDS` and
  `SQLITE_WAL_JOURNAL_SIZE_LIMIT_BYTES` in
  `workspaces/runtime/packages/runtime-schema/src/shared_values.rs`.
  `studio_runtime_store::sqlite_durability` applies them: `configure_writer`
  sets WAL, `synchronous=NORMAL`, and `journal_size_limit` on every writer
  connection, and `WalFlushSchedule` decides when a flush is due.
- Canvas and analytics: a commit requests a flush. The account registry runs
  `PRAGMA wal_checkpoint(PASSIVE)` on both databases when a request is
  pending and the last flush is at least one interval old, on its one retry
  timer. The first request after a flush wakes the registry, so a commit that
  wakes no scheduler is flushed too. The former 4 MiB WAL size trigger is
  removed: a WAL file never shrinks after a passive checkpoint, so the trigger
  fired on almost every wake.
- Graceful shutdown: after every writer has stopped, the server runs
  `PRAGMA wal_checkpoint(TRUNCATE)` on both databases inside the shutdown grace.
  A failure is reported and does not fail the shutdown.
- The supervisor journal and the Mac `host_exec` service database each have one
  long-lived writer in their own process. A thread flushes that writer once per
  interval when it changed rows; closing the last connection checkpoints it.
- Analytics projection: Canvas deletes projected log rows only after a complete
  analytics flush. When an OS crash rolled Canvas back below the analytics
  projection cursor, `RuntimeStore::open` raises the log sequence above the
  cursor, so new log rows are not skipped.

Not yet converted, each pending its own change: the layr database in the Linux
VM (`workspaces/runtime/apps/layr`, vendored from a fixed upstream revision and
checked by its manifest), the VM guest receipt databases
(`workspaces/runtime/apps/vm-guest/common.py`, Python, no flush owner yet), and
the client store (`workspaces/client/packages/client-core/src/store.rs`, which
cannot use the runtime owner and has no flush owner yet). They still use
`synchronous=FULL`.

## Consequences

- The project rule that a retry must not repeat a command, a user message, or a
  reset-credit charge now holds against process crashes. After an OS crash or a
  power loss, an acknowledged request whose record was in the last interval can
  be missing, and a retry can repeat it. The maintainer accepted this risk.
- After such a crash, Canvas and analytics are each a consistent prefix of their
  commits, but not necessarily of the same moment. Analytics can keep the
  projection of Canvas commits that were lost.
- WAL files are truncated to the size limit when SQLite restarts them, and to
  zero bytes at a graceful shutdown.

## Related sources

- `workspaces/runtime/packages/runtime-store/src/sqlite_durability.rs`
- `workspaces/runtime/packages/runtime-store/src/lib.rs`: `checkpoint_wal_if_due`,
  `flush_wal_for_shutdown`, `flush_analytics_wal`
- `workspaces/runtime/apps/runtime-server/src/account_registry.rs`:
  `checkpoint_maintenance` and the `FlushRequested` trigger
- `workspaces/runtime/packages/runtime-services/src/analytics_projection.rs`: `prune`
- SQLite documentation: [`PRAGMA synchronous`](https://www.sqlite.org/pragma.html#pragma_synchronous),
  [`PRAGMA wal_checkpoint`](https://www.sqlite.org/pragma.html#pragma_wal_checkpoint)
  and [write-ahead logging](https://www.sqlite.org/wal.html).
