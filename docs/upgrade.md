# Upgrade an existing Studio installation

Use this runbook on the Mac that owns the existing state directory. Run each
step in order. Stop at the first failed or unknown check. Do not start a second
backend, move the state directory, or kill active agents, monitors, terminals,
or background work to make an upgrade proceed.

## 1. Record the current state

Open a terminal in the checkout used by the running Studio installation. Set
`REPO` to that checkout and `STATE` to its existing state directory. Keep the
current identity; by default it is `~/.local/state/codex-agents`.

```sh
set -eu
REPO="$PWD"
STATE="${CODEX_AGENTS_STATE_DIR:-$HOME/.local/state/codex-agents}"
git -C "$REPO" status --short
git -C "$REPO" fetch origin
CHECK_DIR="$(mktemp -d)"
trap 'rm -rf "$CHECK_DIR"' EXIT
git -C "$REPO" archive origin/main scripts/codex-upgrade-check scripts/codex_state.py | tar -x -C "$CHECK_DIR"
python3 "$CHECK_DIR/scripts/codex-upgrade-check" --repo "$REPO" --state-dir "$STATE" > /tmp/studio-upgrade-before.json
cat /tmp/studio-upgrade-before.json
```

Stop if the first `git status --short` prints any change, or if
`prechecks.git.ok` is false. Save or commit intentional local changes
using the site's normal process, then rerun the check. Do not discard changes.
Confirm `repo`, `stateDir`, branch, and `head` identify the intended install.

Check `prechecks.python.ok` (Python 3.11 or later) and `prechecks.sqlite.ok`
(SQLite 3.43 or later). The latter is required by the contentless FTS5 search
index. Stop if either check fails; update the Python runtime used by Studio
before updating its source.

Review `runningAgentCount`. Also inspect Studio's team view and diagnostics for
active turns, monitors, background tasks, and terminals. Let them reach an idle
boundary before restarting the backend. An approved live patch can be applied
while work runs only when its exact patch contract allows it; see
[`live-updates.md`](live-updates.md). Do not use a live patch to apply arbitrary
source or schema changes.

Review disk capacity before changing source. The JSON reports free bytes,
`canvas.sqlite3` bytes, and their ratio. This release's analytics copy requires
16,500,000,000 free bytes to start; it pauses below 4 GiB during copying. Search
needs at least the larger of 16 GiB or half the database size to create its new
index, then at least 8 GiB including reusable SQLite pages while building.
Payload migration keeps a 64 MiB free-space reserve and also needs room for the
externalized blobs. The ratio is context, not a substitute for those absolute
limits. Stop if capacity is below a migration's requirement. Add storage or
free unrelated files safely, then rerun the check. Never delete Studio state to
make room.

If `prechecks.diagnostics.reachable` is true, the command has read the running
backend's `/api/diagnostics`; otherwise it reads migration state from SQLite in
read-only mode. A diagnostics connection failure alone is not a failed
precheck, but `prechecks.stateDatabase.readable` must be true and
`prechecks.migrationStateReadErrors` must be empty. Stop on a failed local
database read. Keep this before file for comparison.

Before the source update, confirm there is a restorable backup of the source
revision and the complete state directory. The state backup must capture
`canvas.sqlite3`, `analytics.sqlite3` if present, `blobs/`, and supervisor
state, if enabled, at one idle point. Verify that the backup can be read. Use
the site's established backup or filesystem snapshot mechanism, preferably on
another volume. Stop if no complete, restorable backup is available; copying
these files to the same nearly-full volume does not provide a safe rollback.

## 2. Apply the source update

Use the established source installation procedure. Fetch and fast-forward the
intended branch only after the checkout is clean; do not reset or force-update
it. For example:

```sh
git -C "$REPO" pull --ff-only origin main
git -C "$REPO" status --short
```

Stop if the fast-forward fails or leaves unexpected changes. Review the new
revision before continuing. A source update that changes constructors, loaded
modules, or database schema needs a backend restart. Apply a reviewed live
patch only through the manifest and locking steps in
[`live-updates.md`](live-updates.md); the live-update mechanism does not reload
arbitrary Python files and does not undo database migrations.

For a restart, stop admitting new work and wait until agents, monitors,
background tasks, and terminals are idle. Use the installation's existing
normal restart/recovery mechanism. Do not terminate the backend directly while
work is active. Keep the same state directory. Start Studio once, then confirm
that the existing backend is serving `/api/session`, `/api/desktop`, and
`/api/sync/identity`:

```sh
curl --fail --silent http://127.0.0.1:4620/api/session >/dev/null
curl --fail --silent http://127.0.0.1:4620/api/desktop >/dev/null
curl --fail --silent http://127.0.0.1:4620/api/sync/identity
```

If either command fails, stop here and use the installation's normal recovery
path. Do not start another backend against this state.

## 3. Observe automatic migrations

The backend starts schema setup, bounded entity-sync initialization, tombstone
pruning, full-text search migration, and analytics-file migration during
startup. These run in bounded batches while the backend serves requests.
Search and analytics can take minutes on a small database and tens of minutes
or longer on a large one. The measured analytics sample copied at about
7 MiB/s; 16.5 GB extrapolated to roughly 37 minutes on that host. Actual time
depends on storage, load, and row sizes. Search time also depends on index
size. The migrations resume from durable cursors after a restart.

Fetch a fresh check and diagnostics snapshot periodically; compare `cursor`,
`phase`, and `updated` rather than relying on one observation:

```sh
python3 "$REPO/scripts/codex-upgrade-check" --repo "$REPO" --state-dir "$STATE" > /tmp/studio-upgrade-after.json
curl --fail --silent http://127.0.0.1:4620/api/diagnostics > /tmp/studio-upgrade-diagnostics.json
```

Use `/api/diagnostics.migrations` to verify:

- `search.phase` reaches `complete`. While building, its cursor must advance.
  `waiting_for_space` means it is preserving the old index and waiting for
  capacity. A non-empty `search.error` is visible failure; stop and preserve
  both indexes and the database for investigation.
- `analyticsFile.status` reaches `complete`. `insufficientSpace` records the
  required and measured free bytes and waits for operator capacity. During
  copying, `copy` and then `retire` progress through durable per-table cursors.
  Recheck free space before taking action. Stop on `unsupportedTables`,
  `missingTargetTable`, or `error`.
- `entityTombstones.count` is at or below the configured limit after pruning;
  `floor` is a monotonic reset boundary for older clients. The backend may
  prune in batches, so allow it to settle. `pruning.status` identifies
  `running`, `waitingForLock`, `complete`, or a visible `error`; while running,
  `pruning.deleted` must increase until the count is within the limit. A
  client that requests reset support with a cursor below `floor` receives a
  reset response and must perform a fresh entity pull from sequence zero.
- `payloads` shows one cursor/completion status per table. This migration is
  manual; run it in the next section. Stop if any entry reports `error`.

The bounded task, event, and monitor windows are query/sync behavior, not a
one-time data-copy migration. They do not delete the source runtime records.
The contentless search migration retains truncated transcript bodies in
`runtime_item_fulltext`. The analytics migration keeps reads available across
both database files during copy. If any phase and its cursor/updated timestamp
do not change across repeated observations, or diagnostics reports an error,
stop. Preserve the JSON snapshots and logs; do not rerun a schema command by
hand or remove migration tables.

For a low-space wait, the backend must remain available and source data must
remain intact. Add capacity, then verify that the phase resumes and the cursor
advances. Do not delete either analytics copy or the old search index manually.

## 4. Run the payload migration

After confirming adequate capacity, externalize large checkpoints, tool
requests, tool results, and eligible old task tails while Studio continues to
serve requests:

```sh
python3 "$REPO/scripts/codex_payload_migrate.py" --state-dir "$STATE" --table all
```

The migration checks free space before each batch, uses a 64 MiB reserve, and
commits each cursor with its row update. It may take minutes or hours depending
on the payload volume and disk speed. Re-running the command resumes from its
saved per-table cursor. Do not use `--restart-tasks` during an upgrade; it
rescans retention eligibility. To collect old unreferenced blobs, use the
separate `--gc` option only after the migration has completed and its seven-day
orphan grace period has elapsed.

Run the upgrade check again. Require each `payloads` entry to report
`complete: true`. Confirm `canvas.sqlite3` still opens and `/api/session`,
`/api/desktop`, and `/api/sync/identity` still work. Spot-check long transcript bodies and search
results through the normal UI before considering the upgrade complete.

## 5. VACUUM only when needed

Payload externalization and analytics source-row deletion make SQLite pages
reusable, but they do not shrink `canvas.sqlite3`. Run `VACUUM` only when
reclaiming physical database-file space is worth the I/O and temporary-space
cost. It requires a planned maintenance window, an idle backend, a verified
backup, and enough free disk for SQLite's temporary copy (budget at least the
database file size in addition to ongoing growth). Check the exact SQLite
build's requirements. Never run `VACUUM` while Studio is connected to the
database. Skip it if capacity is marginal, the file-size reduction is not
needed, or an idle window is unavailable.

After a successful backup and shutdown using the normal mechanism, run:

```sh
sqlite3 "$STATE/canvas.sqlite3" 'PRAGMA integrity_check; VACUUM; PRAGMA integrity_check;'
```

Require both integrity checks to print `ok`, then restart Studio once and
verify `/api/session`, `/api/desktop`, `/api/sync/identity`, and `/api/diagnostics`.

## 6. Optional process supervisor

The process supervisor is off by default and is independent of the migrations.
Enable it only as a separate planned change after reading the
[restart recovery runbook](restart-recovery.md#opt-in-process-supervisor-v1).
Its first cutover needs one planned interruption at an idle boundary because
existing in-process pipes cannot transfer to it. Install the matching desktop
package, set `CODEX_AGENTS_SUPERVISOR_MODE=1` through the desktop recovery
configuration, and use the documented `scripts/restart-backend-v2.sh
--initial-cutover` procedure. Desktop startup installs a separate
`local.codex.agents.supervisor.<state-hash>` LaunchAgent before it starts or
attaches a backend. The recovery job has its own label and only probes the
supervisor. It does not start or stop an independent owner when a probe fails.
Its LaunchAgent instance waits quietly for a legacy recovery-started owner to
release the state lease, logs the owner PID once, then takes over and performs
identity-checked crash recovery.

For an install that already has a supervisor started by the recovery job,
install the new build and leave the old owner and recovery job running. The new
supervisor LaunchAgent waits for that owner to exit; the next reboot/login or a
planned stop hands the lease to the waiting instance. The old owner can still be
stopped by rewriting or booting out the recovery job that started it. Do not
disable, rewrite, or boot out that recovery job while its supervisor owns live
handles. `desktop/recovery.cjs` checks the recovery-job PID and supervisor
status, and refuses a recovery-job restart or bootout while those legacy handles
are live. Wait for the legacy owner to exit or its handles to close before
retrying. Rewriting, disabling, or kickstarting recovery after handoff does not
unload the independent supervisor LaunchAgent. Do not unload or kickstart the
supervisor service while it owns handles. Verify supervisor protocol and empty
handles in `/api/desktop` and `/api/diagnostics` at the initial idle cutover.
Do not enable it by changing an ad hoc shell environment while the desktop
continues to launch the old backend.

## Rollback and recovery

- **Round 3 sync-entity rollback:** The pull request that completed the move to
  sync entities (round 3), `#TBD-ROUND3`, raises the
  `agent_organization_fields` marker in `sync_entity_meta` to version 3 and
  adds fields to stored entity rows. To go back to a build from before that
  pull request: stop the server; delete only the rows `seeded` and
  `agent_organization_fields` from `sync_entity_meta`; start the old build. It
  reseeds entities in its own shape and writes its own marker. If the markers
  are left in place the old build also starts, but its strict pull validator
  rejects entity rows that carry the promoted fields. A later start of the new
  build runs the upgrade to version 3 again. Room entities that the upgrade
  retired stay retired under the old build; their messages are not touched.
  This procedure was verified by two reviews on a copy of a real database; a
  full start of the old server on real data was not performed, so keep a copy
  of the database before rolling back.
- **Before source update:** no changes were made; stop safely.
- **After source update, before migration:** at an idle boundary, restore the
  prior reviewed source revision using the installation's normal source
  procedure, then restart once. Keep the same state directory.
- **During search or analytics migration:** the durable cursors allow forward
  resume. Prefer fixing the cause and continuing. A code rollback is safe only
  if that exact earlier version supports the migration's current schema and
  phase. Do not drop the new index, `analytics.sqlite3`, or migration metadata.
  Restore a coordinated pre-upgrade backup if the older code cannot read this
  state.
- **After analytics reaches `complete`:** older source may expect analytics
  tables in `canvas.sqlite3`; do not downgrade against the migrated state.
  Restore the full, coordinated pre-upgrade state backup and matching source,
  or apply a forward fix.
- **During payload migration:** rerun the same command after resolving the
  visible space or I/O problem. Do not delete blobs or rewrite references by
  hand. If rollback is required, first establish that the older source can
  resolve blob references; otherwise restore the full pre-upgrade state and
  blob directory together.
- **After `VACUUM`:** restore the pre-VACUUM backup if integrity checks fail.
  Do not copy only `canvas.sqlite3` from one moment and analytics/blobs from
  another; restore a consistent state-directory snapshot.
- **Supervisor cutover:** use the fallback and identity checks in
  [`restart-recovery.md`](restart-recovery.md). Never kill a supervisor or
  child process by PID alone.

Keep the original source and state backup until the upgraded backend, sync, and
all migration phases have been verified.
