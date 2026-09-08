# Missing worker thread, 2026-09-08

## Observed incident

Worker `9f62e3de-d5ee-5c1c-9d3e-e79a207be582` could not continue.
Native thread: `01a07782-9f40-7be0-9cec-53a6049b7aa7`.
The owner reported JSON-RPC error `-32600`, `thread not found`, after send
`7cfd3292-881b-4e19-b588-0ef4fa2f9b08`.

Read-only inspection found the rollout in the worker's recorded account home.
The existing app-server returned the same thread ID with status `notLoaded`.
Studio's current loaded cache did not contain the worker. Its saved preparation
belongs to an earlier native connection. The worker was failed and not in flight.
Its 77 recorded monitors were terminal: 40 failed, 36 completed, 1 cancelled.
This does not establish the state of other workers or external processes.

The native connection changed before inspection. The original unload trigger
and original cache contents are not established. Keep this incident open.

## Confirmed source defects and correction

Studio ignored `thread/closed` and `thread/status/changed` with `notLoaded`.
It also retained its loaded cache after an explicit missing-thread rejection.
Subsequent preparation could therefore skip `thread/resume` and repeat the error.
A pending resume callback could also restore the cache after an unload event.

The correction invalidates the cache only for the matching account, connection,
and thread. An unload invalidates a matching pending preparation. A late response
cannot restore that preparation. An exact typed native missing-thread rejection
also invalidates the cache. The next authorized dispatch resumes the same thread.
No failed or uncertain input is automatically replayed. An unload alone does not
change turn state, delivery receipts, or monitor state.

Native source inspected: OpenAI Codex tag `rust-v0.153.4`, commit
`3d2ee51ca2d5db578f328aa75e20aa22c0197c9a`.
In `codex-rs/app-server/src/request_processors/turn_processor.rs`, `load_thread`
returns the missing-thread rejection when the in-memory lookup fails.
`turn_start_inner` performs this lookup before it submits the input.
This error does not by itself establish that the stored rollout is missing.

## Verification and deployment boundary

Isolated regression checks cover both unload notifications, account and connection
isolation, exact-thread reload, late preparation, active reservation preservation,
explicit rejection, transport uncertainty, and unrelated errors.

All 108 checks passed:

| Contract | Checks |
|---|---:|
| `missing-thread-contract.py` | 8 |
| `turn-start-contract.py` | 16 |
| `prepare-steer-contract.py` | 14 |
| `native-error-contract.py` | 12 |
| `critical-runtime-contract.py` | 10 |
| `runtime-contract.py` | 48 |

The source correction does not certify recovery of the reported build.
No live message, build command, worker replacement, or monitor cancellation was
submitted. Live notification code differs from the reviewed replacement versions.
No live method was replaced without review, and the backend was not restarted.
At 2026-09-08 07:07 UTC, the existing runtime's `prepare` method successfully
resumed this worker's exact stored thread. A subsequent native `thread/read`
returned `idle`. The loaded cache contained the worker. Its recorded status
remained `failed`, `inFlight` remained false, and `turnId` remained null.
The old failed message was not retried. This verifies history restoration,
not continuation of the build or deployment of the source correction.
Evidence for the original unload remains required.

A separate baseline run at `f215fd0` found one existing failure among 17
`turn-recovery-contract.py` checks: its live-installer test expects an older
`BASE_PREPARE` fingerprint. This patch changes neither `prepare_locked` nor
that installer. The other 16 baseline checks passed. The version guard was
not relaxed to apply unreviewed live code.
