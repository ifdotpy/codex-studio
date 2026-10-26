## Symptom

Two tests failed in local gate runs under load and passed when run alone.

- `idle_compaction::idle_claude_dispatches_once_and_restart_does_not_repeat` (workspace nextest in `just test-comprehensive`): the reopen after the timer stopped panicked with `Err(AlreadyOwned)`.
- `test_service.GuestTests.test_disconnect_does_not_stop_exec` (VM guest suite in `just test-fast`): `AssertionError: '' != 'one'`.

## Cause

Both defects are in the tests. The product behavior is correct.

- Rust: the scripted host signals the test from inside `compact`, before `idle::fire` writes its last record. The test then stops the timer, and `jobs.shutdown()` cancels the fire job after it submitted that writer job. By design, a submitted writer job keeps the state lease until the writer thread has run it (`SqliteActor::call_admitted_with_completion`). The test dropped the store and reopened at once. When the writer thread was slow, the reopen saw `AlreadyOwned`. A real restart is a new process, and it can start only after the old process released the lease.
- Python: the command `open(marker,'w').write('one')` creates the empty file before it writes the content. The test read the marker as soon as the file existed. The test also used fixed sleeps for ordering (0.2 s in the command, 0.2 s before the retry) and a 5 s deadline.

## Fix

- Rust: the test reopens the state through `reopen`, which retries only on `AlreadyOwned` until the lease is free. A 60 s bound limits only a hang. The notification wait uses the same bound.
- Python: the command waits for a release file. The test touches it only after the guest has the request and has seen the disconnect (`service.active`, `service.clients`), so the disconnect always happens during the exec. The test reads the marker after the operation ended, when the command has exited and the receipt exists. A new `until` helper polls the exact condition, and a 60 s bound limits only a hang. No fixed sleep is left in the test.

The assertions are unchanged.

## Evidence

- Deterministic reproduction of each failure with scratch experiments (not committed):
  - Rust: another connection holds the canvas write lock while the fire job writes its last record. The original immediate reopen fails with `AlreadyOwned`, the same panic as in the gate. `reopen` passes.
  - Python: the command pauses between `open()` and `write()`. The original wait fails with `'' != 'one'`, the same failure as in the gate. The new wait passes.
- Repeated runs, each in its own process, during a concurrent `cargo build -j 4 -p studio-runtime-services` (no synthetic load):
  - Rust test after the fix: 300 of 300 passed.
  - Guest test after the fix: 300 of 300 passed.
  - Before the fix, the same load did not reproduce the failures (Rust 200 of 200, guest 150 of 150 passed). This load is lower than the contention of a gate run, so the deterministic experiments above are the proof of each cause.
- `cargo clippy -p studio-runtime-services --test integration -- -D warnings`: no warnings.
- `just test-fast` on the head commit 2c6a8c7ef: passed (1026 s), including the VM guest suite (150 passed, 0 failed, 9 skipped).
