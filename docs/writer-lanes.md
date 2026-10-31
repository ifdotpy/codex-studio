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
`analytics_projection_transaction` writes it without writer admission. Use it for
any analytics write that does not read Canvas. `analytics_transaction` still
occupies the Canvas writer, because its callback reads Canvas through the writer
connection (capture and history batches). Moving those reads to a snapshot reader
is open work.

## Raw provider frames

Provider stderr goes to the bounded `app-server.log` (`APP_SERVER_LOG`: 50 MiB,
4 backups). No table stores raw debug frames. Frames that carry state become typed
rows and items.

## Open

- Terminal output (`user_terminal_output`) is still written through the Canvas
  writer. It needs its own file or database and writer.
- Online migrations (`online_migration.rs`, other branch) should call
  `background_batch` once both branches are on main.
