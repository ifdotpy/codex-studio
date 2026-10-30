# Backup of canvas.sqlite3 to a paired server

Target: [Storage in the target architecture](decisions/2026-10-28-target-client-server-architecture.md).
Tracking issue: ifdotpy/layr-studio#630.

The backup copies `canvas.sqlite3` to one paired server by WAL shipping (the Litestream pattern).
It is off until the owner turns it on. `analytics.sqlite3` is not part of it.

## How it works

- A **generation** is one base snapshot (a copy of the database file) and the WAL frames
  of the WAL file from its start. Its id is `<start time>-<random>`, so the ids sort by age.
- The writer actor runs each Canvas checkpoint (about every 15 s while commits arrive, and at
  shutdown). Around it, `wal_backup::before_checkpoint` copies every new committed WAL frame to
  a segment file in `<state>/backup/outbox/<generation>/`. This is local file I/O. It holds no
  SQL transaction and does no network I/O. The checkpoint then runs and the hook records
  whether it finished the WAL.
- No writer runs while the actor runs that job. So a checkpoint never restarts the WAL with
  frames that are not in the outbox. A WAL longer than 32 MiB is copied over several
  checkpoints, and the checkpoint waits until the copy has caught up.
- The sender (`peer_backup.rs`) sends the snapshot and then the segments to the target as
  `backup.put` requests of 128 KiB. A request lands at an offset. The receiver appends the
  chunk only when the offset is the end of what it holds, and always answers the length it
  holds. A lost answer or a repeated request therefore writes nothing twice.
- The receiver stores files in `<state>/backup-received/<source server>/<generation>/`:
  `snapshot.zstblocks` and `w<WAL index>-<start>-<end>-<checksum>-<pages>.seg` segments.
  The name is the source position: the WAL byte range, the WAL checksum chain at its end, and
  the database size in pages after the last commit.
- **Snapshot.** A separate read connection holds a read mark while the database file is
  copied, in 4 MiB blocks, each compressed with zstd (level 3) and sent as it is made. The
  normal 15 s checkpoints keep running. A passive checkpoint copies only the frames up to the
  read mark into the file and still syncs the WAL, so commits stay durable as before. The WAL
  cannot restart while the read mark is held, so it grows during the copy. The generation
  starts with one write (the user version rewritten with its own value), so the read mark is
  after the last checkpointed frame. Restore applies every WAL frame of the generation over the
  copy, so a page that a checkpoint wrote during the copy ends with its newest content.
- The snapshot file is `snapshot.zstblocks`: for each block a 4 byte big-endian length and one
  zstd frame.

### A new generation starts when

- the backup is turned on, or no generation exists;
- the server stops or restarts during the snapshot copy (the read mark is lost), or the
  upload of the snapshot fails after the retries of each request (5 tries, 5 s apart);
- the WAL restarted without the hook (a checkpoint outside the hook, or a restore);
- the outbox is larger than 4 GiB (`BACKUP_OUTBOX_MAX_BYTES`), for example when the target is
  away for a long time;
- the target no longer holds the snapshot of the generation.

The receiver keeps the newest `retainGenerations` generations of each source server. It
removes older ones when a new snapshot is complete.

## Settings

Settings are the file `<state>/backup/settings.json`. The backend reads it again within 30
seconds, so no restart is needed.

```sh
studio-runtime-server backup enable --state DIRECTORY --target SERVER_ID [--retain 2]
studio-runtime-server backup disable --state DIRECTORY
studio-runtime-server backup status --state DIRECTORY
```

Turning it off drops the outbox. `status` shows the settings and `backup/state.json`
(generation, snapshot state, `broken`, `last_error`). The target must be a paired server with
this build.

## Restore

Run these steps on the server that holds the copy (or copy the generation folder to the
server that you restore on). Stop the Studio backend that owns the target state directory
first.

1. Find the newest generation folder with a `snapshot.zstblocks`:
   `ls <state>/backup-received/<source server id>/`.
2. Rebuild and check it:

   ```sh
   studio-runtime-server backup restore --from <generation folder> --to /path/restored.sqlite3
   ```

   The command decompresses the snapshot and applies the segments in order. It refuses a gap or a
   segment whose WAL checksum differs from its name. It then runs `PRAGMA integrity_check`
   (must be `ok`) and compares `PRAGMA page_count` with the source position of the last
   segment. Run it into a scratch path as a **restore check**, for example after the first snapshot. It needs
   free disk space for one more copy of the database.

3. Compare a row count with the source if the source is still readable, for example
   `sqlite3 restored.sqlite3 'select count(*) from runtime_items'`.
4. To put the copy in service: stop the backend, move the old `canvas.sqlite3`,
   `canvas.sqlite3-wal`, and `canvas.sqlite3-shm` away, and move `restored.sqlite3` to
   `<state>/canvas.sqlite3`. Start the backend. The first backup after a restore starts a new
   generation.

The copy lags the source by the commits after the last checkpoint, normally less than 15 s
plus the time to send them.

## Limits

- One target server. Restore is by hand.
- The snapshot of the live database (about 29 GB) is sent serially in 128 KiB requests. The
  time is not measured yet (compression and the link speed decide it; the link measured
  8.7 MiB/s with 512 KiB requests, docs/peer-link.md). During the copy the WAL grows by the
  write rate times the copy time, and the outbox holds a second copy of those frames. At
  4 GiB of outbox the generation breaks and starts again.
- A server restart during the snapshot ends the generation.
- Segments with the snapshot of a failed upload are not resumed; the next pass starts a new
  generation.
- `backup.put` requests are limited by the 256 KiB paired-server request limit.

## Write transaction alert

A Canvas write transaction that holds the single writer for `SQLITE_WRITE_TRANSACTION_ALERT_MS`
(1000 ms, `runtime-schema/src/shared_values.rs`) writes this warning to the server log, when
the transaction ends:

```text
warning: Canvas write transaction held the writer for 1234 ms (alert at 1000 ms), caller path/file.rs:123
```

The caller is the `file:line` of the `transaction` call. The same transactions are in the
diagnostics under `sqliteContention.slowTransactions` (with the caller in `originFrames`). The
alert fires after the transaction ends, so a transaction that never ends gives no warning; the
active transaction list of the same diagnostics shows it.
