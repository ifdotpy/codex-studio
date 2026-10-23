# Canvas database maintenance

The runtime owns `canvas.sqlite3`. Studio keeps all user history: no background
job shortens or deletes transcripts, task output, tool results or analytics.
Reclaiming free file space is a manual, offline step.

## Background maintenance

`studio_runtime_services::storage_retention` runs once after startup and then
every `RETENTION_PASS_INTERVAL` (one hour). Each step is a short writer
transaction with a pause between batches, so interactive writes keep the
writer actor.

- **Entity tombstones.** When the non-transcript tombstone count exceeds
  `ENTITY_TOMBSTONE_LIMIT` (10,000) by the prune slack, the oldest tombstones are
  removed in batches of `ENTITY_TOMBSTONE_PRUNE_BATCH` (500) until the limit is
  reached, and `entity_tombstone_floor` is raised to the highest removed
  sequence. A client whose cursor is below the floor resets. Tombstones are sync
  bookkeeping for deleted entities, not user history.

A pass that finds nothing to do submits no writer transaction.

Releases from 2026-10-17 (`478b2e176`) until this was removed also trimmed
the output of finished tasks: a task created more than seven days ago and
outside its agent's 100 newest tasks kept only the last 512 bytes of `tail`
and was marked `outputTruncated` and `outputPreviewOnly`. Those tails cannot be
restored from the state. Their `payload-v1:tasks` row in
`runtime_payload_migrations` remains as a historical marker; no current
release writes it.

## Offline VACUUM

Freed pages go to the SQLite freelist. New rows reuse them, but the file does
not shrink. The only way to shrink it is `VACUUM`, which rewrites the whole
file. Do this only while the backend is stopped.

### Disk space

`VACUUM INTO` writes a new, compact copy next to the original:

- the new file needs about the live data size: file size minus freelist bytes
  (`PRAGMA page_count`, `PRAGMA freelist_count` and `PRAGMA page_size`);
- the original stays in place until the swap, so the volume needs that
  amount free in addition to the existing file, plus 10 % margin;
- point `SQLITE_TMPDIR` at a directory on the same volume. Do not use `/tmp`,
  which has a per-user quota on the owner's machine.

For example, the 2026-10-17 state had 9.4 GB with a 1.3 GB freelist. It
needs at least 8.9 GB free (8.1 GB of live data plus 10 %). In-place `VACUUM`
needs up to twice the file size and leaves no fallback copy, so prefer
`VACUUM INTO`.

### Procedure

Run these from the state directory (`~/.local/state/codex-agents`) with the
SQLite 3.35 or later command-line shell.

1. Quit Studio normally and confirm that no process holds the database:
   `fuser canvas.sqlite3 canvas.sqlite3-wal` prints nothing. Never run these
   steps while a backend uses the state directory.
2. Check the space: `sqlite3 canvas.sqlite3 'PRAGMA page_size; PRAGMA page_count; PRAGMA freelist_count;'`
   and `df -h .`.
3. Fold the WAL into the main file: `sqlite3 canvas.sqlite3 'PRAGMA wal_checkpoint(TRUNCATE);'`.
   The first result column must be `0`, and `canvas.sqlite3-wal` must then be
   empty or missing.
4. Write the compact copy:
   `SQLITE_TMPDIR="$PWD" sqlite3 canvas.sqlite3 "VACUUM INTO 'canvas.sqlite3.compact'"`.
   On NVMe this takes a few minutes.
5. Verify the copy and restore WAL mode:
   `sqlite3 canvas.sqlite3.compact 'PRAGMA integrity_check; PRAGMA journal_mode=WAL;'`
   must print `ok` and `wal`.
6. Swap the files and keep the original as a fallback:
   `mv canvas.sqlite3 canvas.sqlite3.pre-vacuum && mv canvas.sqlite3.compact canvas.sqlite3`.
   Remove a leftover empty `canvas.sqlite3-shm` if present.
7. Start Studio and check the chats, tasks and `/api/diagnostics`. Then
   delete `canvas.sqlite3.pre-vacuum`.

To roll back, quit Studio and move `canvas.sqlite3.pre-vacuum` back to
`canvas.sqlite3`.

## Compressed full text and analytics records

Writers store `runtime_item_fulltext.body` in stored format 1 (see
[runtime state formats](runtime-state-formats.md#compressed-text-columns)).
After the full-text pass, the same job handles `analytics_items.record` in
`analytics.sqlite3`: when the table has `DICTIONARY_MIN_ROWS` (2,048) rows and no
dictionary, it trains one from `DICTIONARY_SAMPLE_ROWS` (4,096) sampled plain
records on a read-only connection and stores it, then converts plain records in
one analytics transaction per batch, with the same checks and its own progress
row.
`studio_runtime_services::stored_text_compaction` converts the plain bodies
that older releases wrote. It starts `COMPACTION_START_DELAY` (one minute)
after startup and repeats every `COMPACTION_PASS_INTERVAL` (one hour) until the
column is complete:

- A reader scans `COMPACTION_WINDOW_ROWS` (512) rowids at a time and converts
  at most `COMPACTION_BATCH_ROWS` (64) values or `COMPACTION_BATCH_BYTES`
  (8 MiB) of text per batch, outside the writer. Each value must decode to its
  exact text first.
- One short writer transaction per batch replaces a value only if the row
  still holds the scanned text. It reads each replaced value back and commits
  only if it decodes to the original text. The SHA-256 of the batch texts goes
  to `runtime_stored_text_batches`.
- Progress is the `stored-text-v1:runtime_item_fulltext.body` row of
  `runtime_payload_migrations`: `running`, `complete`, `waitingForSpace` or
  `error`. The pass writes nothing while the state volume has less than
  `COMPACTION_MIN_FREE_BYTES` free (the 5 GiB critical warning), or when free
  space is unknown.

The pass never deletes data and never shrinks the file. Freed pages go to the
freelist and new rows reuse them. To return the space to the disk, run the
offline `VACUUM INTO` procedure above after the progress row shows
`complete=1`, at a restart that the owner approves.

To install a release older than the first release that reads compressed bodies
(`Read compressed full-text bodies`), stop the backend first and run
`studio-runtime-server stored-text inflate --state ~/.local/state/codex-agents`.
It rewrites every compressed full-text body and analytics item record as plain
TEXT with the same checks, records `:inflate` batches, and forgets the
compaction progress. The stored dictionaries stay. The command refuses a
state directory that a running backend owns.
