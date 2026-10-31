# Writer lanes

Commands go first. Background batches write in short parts. Owner:
`runtime-store/src/writer_lanes.rs`. Design: [target architecture](decisions/2026-10-28-target-client-server-architecture.md),
Latency rule 3 and Command execution rule 5.

## Rules

- Every `RuntimeStore::transaction*` call is a command. It registers with the
  command gate before it waits for writer admission.
- `RuntimeStore::background_batch(rows, process)` writes `rows` in parts. A part is
  one write transaction. It ends before a row whose estimated cost would cross
  `BACKGROUND_PART_TIME_LIMIT_MS` (50 ms). The first row of a part always runs.
- A batch submits the next part only when no command waits. Queued commands run
  before the rest of the batch. A command waits for at most one part.
- `process` must not do I/O. Read input before the batch, run effects after it.
- Heavy commands can starve a batch. This is intended.

## Analytics

`analytics.sqlite3` is a separate database with its own connection.
`analytics_projection_transaction` writes it without writer admission. Capture and
history batches check agent identity first on a Canvas read snapshot
(`check_agent_identity`), then write analytics only. An identity that changes between
the check and the commit lets one capture of the old identity land, as if it had been
captured just before the change. `analytics_transaction` (Canvas writer) remains for
startup migrations and tests.

## Terminal output

`terminal-output.sqlite3` holds `user_terminal_output` (own connection, own WAL, flushed
with the other databases). The terminal row (tail, offset, event cursor) commits in Canvas
first; then the driver appends the same text with `terminal_output_append`. A crash
between the two commits loses that one chunk of older history (the tail is safe). Deleting
a terminal removes its archive rows after the Canvas commit. On open, rows left in the
Canvas table move to the archive (copy, flush, delete); the empty Canvas table stays for
schema checks.

## Raw provider frames

Provider stderr goes to the bounded `app-server.log` (`APP_SERVER_LOG`: 50 MiB,
4 backups). No table stores raw debug frames. Frames that carry state become typed
rows and items.

## Open

- Online migrations (`online_migration.rs`, other branch) should call
  `background_batch` once both branches are on main.
- `analytics_items` holds 11.4 GB of free space inside pages. Rebuilding it needs about
  16 GB of free disk and is deferred until the disk has room.
