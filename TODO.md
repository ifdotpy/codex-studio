# TODO

This file lists the unfinished work of the 512-subagent scale effort and of the backend Python removal. Each entry says enough to resume without the original session.

- Orchestration defects and their status are tracked in GitHub issue #373 ("Audit bugs").
- Python-removal scope as of 2026-10-20: clean the backend only. Keep Electron and all VM-host Python (host provisioning, vm-guest, Mac host-exec) and the server modules it imports.

## Blocking

### Red browser smoke on main (#373 B22)

- **Symptom:** since 370adfdc, `message-receipt-pagination-browser.spec.mjs` and `execution-settings-ui.spec.mjs` time out in CI because the Rust fixture server becomes unreachable (ECONNREFUSED).
- **History:**
  - The job was green at 03b57db8.
  - At 370adfdc it passed once and failed on rerun.
  - It fails at e7d8004d too.
  - Locally, the specs pass with one worker on four CPUs.
- **Suspect:** f19222e8 "Admit every SQLite read through read_async before taking a blocking thread". It could cause a panic, a blocking-pool deadlock, or read-permit starvation while the fixture seeds agents concurrently.
- **Next step:** read the server stderr in the CI smoke artifacts, then reproduce under CPU restriction (`taskset -c 0-1`, several Playwright workers).

## Live scale runs

- **Rerun Codex 512 with the strict verifier** (`verify-ui-team-scale.mjs`) after B12 (e7d8004d) is deployed. The previous run (UITEAM5C70EFF2, 03b57db8) reached 508 of 512 replies, then hung: four workers and the lead were stuck in an unsent capacity retry.
- **The deploy needs a restart** so that startup recovery releases the five parked capacity claims left in the live state.
- **Then run Claude** at 30, 128 and 512: Sonnet lead, Haiku workers.
- **Verifier defect (#373 B21):** on SIGINT the verifier can exit before its cleanup, so the team is left behind. The run's teardown must run on every exit.

## Orchestration follow-ups (recorded in #373; not scheduled)

The owner deferred these on 2026-10-21. Do not start them without approval.

- **B13:** the radio tick refuses with "Project state changed during preparation" when any agent row changes between `PreparedHost::load` and the tick transaction.
- **B14:** a user message that arrives while the lead processes worker results waits for the roughly 30 s idle wake tick.
- **B16:** the server binary resolves the Claude bridge directory from the compile-time `CARGO_MANIFEST_DIR`. The live binary depends on the deploy worktree staying in place.
- **B19:** each batch of up to 32 child results costs a full lead model turn, about 16 turns for 512 workers. The proposal is one coalesced "all children finished" event with a per-child summary.
- **B20:** `/api/accounts` returns Codex accounts without `provider`. Unpushed client-side workaround: a3d67ec4 in `/work/ssd2/worktrees/ui1b`.
- **B23:** a capacity retry whose `turn/start` outcome became unknown across a restart stays `starting` and keeps suppressing the original failed turn's `child_result`.
- **B24:** dead store SQL `account_wake_capacity_candidate` and the column `start_attempt_capacity_retry_id` (removal needs an index rename and a migration). `docs/cli.md` still says a capacity retry uses empty input.
- **B17:** file and process work inside SQLite read closures:
  - `message_admission` `PreparedAdmission::load` and `MessageIngressAdmission::load`;
  - `sync_documents` `ProjectVaultSnapshot::capture_for_sync`.

## Python removal from the backend

### Step 1: replace Python with Rust, lane by lane

- **A2 — store, http and server tests:**
  - Strip the Python oracles and `include_str!` fixtures from `runtime-store/tests`, `runtime-http/tests` and `runtime-server/tests`.
  - Write a Rust-only fresh-state snapshot.
  - Make the reattach witness work without Python.
- **A2 + A3 joint — multi-server fixture:** `runtime-services/tests/multi_server/common.rs` builds its fixture by running `multi_server/oracle.py`. That fixture is used by about 20 tests in `multi_server_corpus.rs` and by `runtime-http/tests/{multi_server_incoming,voice_provider}.rs`. Replace it with a Rust fixture writer, or delete the dependent tests.
- **A4 — services tests n–z:** same treatment as A3 (done in bc8c607b).
  - These still use `agents_create::prepared_python`: `terminal_driver`, `terminal_pty` and `wave7_p03_gate_tests/voice.rs`.
  - `wave7_p10_lifecycle/oracle.py` imports `agent_management/oracle.py`, kept by 904ec845.
- **A5 — process tests:**
  - Replace Python children with `/bin/sh` stand-ins.
  - Use only the Rust supervisor.
  - Move the JSON fixtures next to their crates.
- **A6 — clients:**
  - Tauri and Electron start `studio-runtime-server` and the Rust supervisor by default.
  - Remove `CODEX_DESKTOP_BACKEND`, `CODEX_AGENTS_PYTHON`, `api_python` and the PyO3 packaging.
  - Electron and its Python helpers stay.
  - **Landing A6 requires stopping development and asking the owner to restart.**
- **B1 — operations CLI:**
  - Rust `service install|status|uninstall` replaces `codex_supervisor_service.py`.
  - `just install` replaces `install-cli.py`.
  - B1 must merge before the delete, because the `~/.local/bin` links point into `apps/server/src`.

### Step 2: one bulk delete of the unreachable Python and its tests

- **Delete:**
  - `apps/server/**`;
  - the `.py` oracles;
  - `support/prepared_python.rs`;
  - `operations-python/`;
  - the PyO3 dependencies;
  - `mypy.ini`;
  - the requirements files.
- **Keep:**
  - Electron;
  - the import closure of the VM-host Python (map it first; move shared modules beside the VM code).
- **Remove the Python gates:**
  - in `facade.mjs`: the `server` pseudo-package, `test-server-*` and the FAST_SUITES step;
  - in the justfile: the server recipes;
  - in CI: the `python` job and `fast-server`;
  - setup-python and `install-cli` from the CI setup actions.
- **Drop the Python cost triggers and tables** (`analytics_cost_projection_v1` and its about six JSON triggers per usage write), the store-bootstrap analytics-usage-schema dependency, and the Python-parity skips of `SCHEMA_OBJECTS`.
- **Then the Rust simplification:**
  - delete the Python-state compatibility code;
  - delete supervisor protocol v1;
  - delete `reattach_commands`, but only after every live process uses v2 and the new launch command.
- **Coverage lost by deleting Python parity:** in A3 this included transfer selection, agent-management rows, analytics capture and import, budget steps, and the browser, account-policy, catalog and diagnostics projections. Also gone are the claude login/controls tables, the context-repair and continuation decision tables, monitor decisions, managed-tool rows, pricing and the default registry. Behavioural Rust assertions remain.

## Test reliability

A test that needs repeated runs is defective: make it deterministic or move it to the heavy tier.

- **`tests/servers/accounts-settings-browser.spec.mjs`:**
  - fails in 5 of 8 local runs under load, showing "Local · offline [disabled]" instead of "✓ Local";
  - uses the Node multi-server mock;
  - the area was changed by 1bcb2da6 and 84d0ce62.
- **Server shard runtime-contract test** `test_follow_up_completed_before_its_start_reply_reports_to_the_parent`: failed under load, passes alone.
- **Three `runtime-services` `provider_session_task` tests fail locally for every lane:**
  - `review_numeric_request_recovery_needs_hidden_receipt_mapping`;
  - `disposable_supervisor_session_preserves_replay_and_settlement_across_reattach`;
  - `closure_crash_before_written_persistence_keeps_unknown_without_resend`.

  Find out why, and whether CI runs them.

- **`claude-bridge-contract.py`** `test_resume_preserves_background_task_and_accepts_the_next_turn_once`: flaky under load.
- **`idle_unload`** `idle_thread_unloads_then_the_session_stops_and_a_new_turn_resumes_it`: the 5 s idle protocol-progress deadline flaked in CI.
- **Seen failing 2–4 times:**
  - `image_workspace` `fake_backend_lifecycle_recovers_failpoints_and_syncs_changed_content`;
  - `worker-lifecycle-restart-contract` `test_automatic_continuations_stop_when_the_assigned_task_changes`;
  - `provider_versions` `provider_update_notice_durable_failure_restart_and_replay_matrix` and `preference_read_error_fails_closed_without_an_upstream_request`;
  - `notification_dispatch_corpus`: `wave7_p06_runtime_notification_corpus`, `python_written_notification_corpus` and `publication::wave7_p06_commit_publication_restart_preserves_order`.
- **Stack overflow** seen in Rust partitions (f7205148, 9d027dc2): check the stack headroom after recent future growth.
- **Heavy corpus:** `STUDIO_RUNTIME_CORPUS_FULL` has 6 red cases, the malformed `runtime_agents` refusal text. Pre-existing.

## Performance follow-ups

- **Pending preparations in `account_worker.rs`** subscribe to any agent change. Address wakes by agent, account or dependency, using the changed-row hints.
- **Token-rate state** (`token_rate.rs`): one task should own the mutable state, and readers should get an immutable snapshot built once per frame interval.
- **Diagnostics sub-documents** are built with `json!`: give them `Serialize` structs. `routes/mod.rs` `generated_json` goes Value → DTO → JSON at about 120 call sites; split it per route family.
- **Remaining `Value` getters** in pricing catalog parsing and in the diagnostics response assembly.
- **Provider frames:** a method-tagged `Frame` enum with typed per-method params; `bridge.mjs` should emit explicit change operations instead of a whole-session diff.
- **Analytics** as a recoverable projection from Canvas usage receipts (approved 2026-10-20). Then the analytics write can leave the journal commit.
- **Session-cost follow-ups:** the Claude local log scan takes 85–250 ms on large Claude roots; pool the analytics read connection (about 1 ms per request).
