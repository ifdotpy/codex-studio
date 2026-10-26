Restart witness: where it stopped (2026-10-24)

- Branch test/restart-fixes-witness, local only, commit 55d90ff90 "WIP: backend restart witness (not run yet)".
  It is based on the batch commit 9413f8f2f (gate/batch-20261023e2).
- The commit adds:
  - wave7_p01_registry::backend_restart_keeps_live_work_and_ends_the_stopped_turn_once (cfg linux or macos). The test re-runs itself with CODEX_AGENTS_SUPERVISOR_MODE=1.
  - tests/fixtures/restart-provider.mjs, a scripted app-server. It logs requests to requests.jsonl, holds command/exec until release-exec exists, completes a turn when complete-<threadId> exists, and answers turn/interrupt with an interrupted turn/completed.
- The test has not been built or run yet. To run it:
  1. cargo build -p studio-process-supervisor
  2. Run with STUDIO_SUPERVISOR_BINARY=<target>/debug/codex-process-supervisor:
     cargo nextest run -p studio-runtime-server --test integration -E 'test(backend_restart_keeps_live_work)'
- Expected gap: with #458 a stopped agent keeps no disconnect receipt. So the #459 step (stopped_turns.rs, which needs disconnectRecovery) may not interrupt B. B's stop interrupt is staged in startAttempt.interrupt with outcome "unsent" while no worker runs.
  Likely fix: stopped_turns also selects a stopped agent whose startAttempt.interrupt is "unsent" for a turn of the current generation. Resending a proven unsent interrupt is safe.
