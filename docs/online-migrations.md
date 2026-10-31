# Online migrations

Move a large table in small batches while the single writer keeps serving
commands. Owner: `runtime-store/src/online_migration.rs`. Design:
[target architecture, Storage](decisions/2026-10-28-target-client-server-architecture.md).

## Declare a migration

Implement `OnlineMigration` for a type:

- `NAME`, `SOURCE_TABLE`, `SOURCE_KEY`: the name (`[a-z0-9_]+`), the source table
  and its positive integer key column.
- `read_after(db, after, limit)`: rows with a key above `after`, ordered by key.
- `read_one(db, key)`: the current source row, or `None` if it is gone.
- `transform(row)`: pure conversion, no I/O.
- `write(db, key, out)`: upsert into the target. It must be idempotent.
- `remove(db, key)`: delete the target row of a deleted source row.

## Run it

`online_migration::run(&store, Arc::new(migration), stop)` calls `begin`, then
one `step` per write transaction until the migration is complete. It pauses
`BATCH_PAUSE_MS` between batches, so queued command writes run first. Start it
from the owner's background task at every boot; a complete migration returns at
once. To stop, return true from `stop` or drop the future. Run it again to
resume from the mark.

A batch ends before a row whose estimated cost (the slowest row so far) would
cross `BATCH_TIME_LIMIT_MS`. It reads at most `BATCH_MAX_ROWS` rows. The first
row always runs, so one row slower than the limit still advances.

## Rows written during the migration

`begin` adds triggers on the source table. A changed key at or below the mark
goes to `runtime_online_migration_dirty`. Higher keys need no mark: the copy
reads them later. Each batch re-copies dirty keys first, then copies new rows.
The batch that finds no dirty key and no row above the mark drops the triggers
and records completion in one transaction. Owner code needs no dual write, but
it must switch its reads and writes to the target after completion, not before.

## Progress

The mark is a row in `runtime_payload_migrations` named `online:<NAME>`. The
migration status endpoint lists every one under `online`: `cursor`, `complete`,
`status`, `updated`, `error` and `dirty` (keys waiting for a re-copy). A failed
batch rolls back and records `status = error`; fix the cause and run again.

## Limits

- Integer keys only. A source with a changed key column re-copies both keys.
- Triggers on the source table slow its writes a little until completion.
- A crash loses at most the batch in progress (one transaction).

## Enum check triggers

`runtime-schema` `persistence::enum_check_triggers::<T>(table, column)` returns
SQL for `BEFORE INSERT` and `BEFORE UPDATE` triggers that reject a value outside
the Rust enum `T`. A migration replaces them in milliseconds, unlike a table
`CHECK`. `check_enum_triggers::<T>(db, table, column)` is the parity check: it
fails when the stored triggers differ from the enum.
