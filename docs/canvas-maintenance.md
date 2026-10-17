# Canvas database maintenance

The runtime owns `canvas.sqlite3`. Two background jobs keep it from growing
without bound; reclaiming the file space they free is a manual, offline step.

## Background retention

`studio_runtime_services::storage_retention` runs once after startup and then
every `RETENTION_PASS_INTERVAL` (one hour). Each step is a short writer
transaction with a pause between batches, so interactive writes keep the
writer actor.

- **Task output.** A finished task (status in `TRIMMABLE_TASK_STATUSES`:
  `completed`, `failed` or `interrupted`; any other status keeps its output)
  created more than `TASK_RETENTION_SECONDS` (seven days) ago, and outside its
  agent's `TASK_NEWEST_PER_AGENT` (100) newest tasks (read through the
  `runtime_task_agent_created_id` index, which Rust startup creates if missing),
  keeps only the last `TASK_PREVIEW_BYTES` (512) bytes of `tail` and is marked
  `outputTruncated` and `outputPreviewOnly`. Progress is stored in the
  `payload-v1:tasks` row of `runtime_payload_migrations`. A restart resumes from
  that cursor; repeating a batch changes nothing. A task with a missing or null
  `created` is never trimmed. The retired Python job failed on such rows, which
  left the marker at cursor 0 with status `error`; the first Rust batch replaces
  that state.
- **Entity tombstones.** When the non-transcript tombstone count exceeds
  `ENTITY_TOMBSTONE_LIMIT` (10,000) by the prune slack, the oldest tombstones are
  removed in batches of `ENTITY_TOMBSTONE_PRUNE_BATCH` (500) until the limit is
  reached, and `entity_tombstone_floor` is raised to the highest removed
  sequence. A client whose cursor is below the floor resets.

A pass that finds nothing to do submits no writer transaction.

## Offline VACUUM

Freed pages go to the SQLite freelist. New rows reuse them, but the file does
not shrink. The only way to shrink it is `VACUUM`, which rewrites the whole
file. Do this only while the backend is stopped, and only after one retention
pass has finished (the `payload-v1:tasks` marker has `complete=1`), so the
trimmed space is reclaimed as well.

### Disk space

`VACUUM INTO` writes a new, compact copy next to the original:

- the new file needs about the live data size: file size minus freelist bytes
  (`PRAGMA page_count`, `PRAGMA freelist_count` and `PRAGMA page_size`);
- the original stays in place until the swap, so the volume needs that
  amount free in addition to the existing file, plus 10 % margin;
- point `SQLITE_TMPDIR` at a directory on the same volume. Do not use `/tmp`,
  which has a per-user quota on the owner's machine.

For example, the 2026-10-17 state had 9.4 GB with a 1.3 GB freelist before
task retention ran. It needs at least 8.9 GB free (8.1 GB of live data plus
10 %). After retention the copy is smaller. In-place `VACUUM` needs up to twice
the file size and leaves no fallback copy, so prefer `VACUUM INTO`.

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
