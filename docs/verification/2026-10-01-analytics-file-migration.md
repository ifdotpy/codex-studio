# Analytics database migration measurements

Measurements ran on 2026-10-01 against temporary SQLite files under `/tmp`.
The live state directory and its database were not opened. Sample sizes are
small fixtures; production time estimates are linear extrapolations, not a
full-size rehearsal.

## Copy fixture

The fixture contained 3,000 analytics items with 1 KiB payloads (4,131,783
encoded source bytes). The resumable copy and batched source retirement took
0.557 seconds, or 7.08 MiB/s. At that rate, copying 16.5 GB takes about 2,224
seconds (37 minutes). SQLite and storage load can change this substantially.

Copy batch transaction time was 4.29 ms p50, 10.00 ms p95, and 12.99 ms max
across 24 batches. Source deletion was 1.24 ms p50, 4.28 ms p95, and 5.96 ms
max across 24 batches. These remain below the 50 ms target in this fixture.

The real `/api/analytics` endpoint was measured 12 times before and after
copying the fixture. Before: 138.8 ms p50, 169.6 ms p95, 204.3 ms max. After:
82.8 ms p50, 88.8 ms p95, 122.8 ms max. The before state exercised the
read-only union of copied and uncopied rows; the after state read the completed
analytics file.

## Runtime write contention

A synthetic analytics writer held an immediate SQLite write transaction while
a runtime writer attempted its own transaction. Across 25 iterations, shared
file runtime wait was 209.0 ms p50, 269.5 ms p95, and 273.0 ms max. With the
analytics writer on the separate file, runtime wait was 1.85 ms p50, 6.47 ms
p95, and 8.43 ms max. Each analytics fixture transaction deliberately slept
for 20 ms; these figures measure SQLite contention and scheduling in this
process, not live workload latency.

## Rollout history rebuild comparison

The existing rollout importer processed a synthetic 505,946-byte file into
3,001 turns in 24 bounded steps and 0.497 seconds (0.97 MiB/s). On these
fixtures, row copy was faster (7.08 MiB/s). The rebuild avoids reading/copying
old analytics tables, but this small fixture does not establish a production
space advantage: rollout files and analytics projections may have different
sizes and contents. Keep rebuild as an operator-selected alternative only
after measuring the actual rollout corpus and required output coverage.

## Rollout order

1. Before starting, check free space for the 16.5 GB analytics copy plus the
   temporary growth required by Opt3. The migration refuses to copy unless at
   least 16,500,000,000 bytes are free.
2. Start the reviewed server build and observe
   `/api/diagnostics.analyticsFileMigration`. Analytics reads remain complete
   from both files during copy. Copy progress and each transaction cursor are
   durable, so a restart resumes. A space refusal is visible as
   `insufficientSpace`; do not continue expecting a partial copy to finish.
3. Wait for analytics status `complete`, then run the Opt3 migration while the
   old analytics tables are being retired or after retirement has completed,
   according to Opt3's own disk estimate. Recheck free space before each
   operation; never assume deleting rows from SQLite has released file space.
4. After both migrations and integrity checks complete, stop new maintenance
   writes using the approved rollout window and run the final `VACUUM` on
   `canvas.sqlite3`. It needs additional temporary free space; verify the
   available-space requirement for the installed SQLite build before starting.
   Do not run VACUUM while the server is using that file.

## Code and contract checks

The implementation is rebased onto Opt2 commit `f2d1545` and uses its
`codex_sqlite.connect()` and `scope()` for runtime analytics connections. The
analytics migration and session-cost read connections also use `connect()`.
Commits: `0b68fed` (analytics split) and `516f25d` (Opt2 connection API).

Passed: analytics contract (32), analytics storage contract (2), analytics
history worker (10), session-cost contract (16), workspace contract (45), and
sync-entities contract. Earlier task checks also passed analytics history (18),
analytics transaction (4), reasoning history (8), budget (22), and the 12
streaming/memory checks. Python compilation and `git diff --check` passed.
Browser tests remain unverified because `playwright-core` is absent in this
worktree.
