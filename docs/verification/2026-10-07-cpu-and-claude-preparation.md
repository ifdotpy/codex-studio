# CPU and Claude preparation repair, 2026-10-07

## Changes

Agent folder measurements are removed from the source application.
The scanner, disk resource, HTTP route, UI hook, and size labels are removed.
Archive and removal operations no longer run `du` or walk folders to count bytes.
An unknown removal size stays `null`. Existing cleanup receipts remain valid.

History discovery reuses unchanged directory signatures and checks path containment on each lookup.
The background importer checks up to 32 idle actors through one connection.
It stops at progress and closes the connection before its CPU budget sleep.
Foreground reads still process one actor.

Rule ticks read eligible rules through a partial index.
The writer rechecks each sampled rule and its owner before it applies changes.
Transfer ticks decode pending transfers only.

Claude metadata refresh starts after four minutes while a turn or native task remains active.
The existing five-minute trust limit remains unchanged.
Concurrent requests share one probe. Expired data never supplies a fallback.
A proven account rejection clears the cached proof.
Idle queries cancel the timer. A failed background probe does not enter a retry loop.
Constructor flags, fresh account checks, permissions, and timeout limits remain unchanged.

Preparation errors now record the blocked phase and elapsed milliseconds.
The receipt retains `turnStartOutcome: not_applied` when no input was submitted.
Bridge version 21 preserves the existing checks before retirement of an idle process.

## Controlled measurements

Each measurement uses five trials. Values below are median CPU milliseconds.
Each comparison retains identical input and output state.

| Work                                                               |    Before |   After |
| ------------------------------------------------------------------ | --------: | ------: |
| Twelve unchanged directory checks, 8,192 files                     |   950.285 |  13.277 |
| Twelve rule ticks, 1,024 historical records and eight active rules |   650.046 |  12.915 |
| Twelve transfer ticks, 1,024 historical records                    |   670.526 |   4.907 |
| History idle sweep, 512 actors                                     | 2,402.715 | 416.298 |

The idle sweep opens 16 connections instead of 512.
Its retained-state SHA-256 is identical in every trial.
The benchmark disables the existing throttle sleep on both sides to measure CPU work.
The application retains its CPU budget.

## Verification

The disk removal passes API, resource, canvas, management, image, and UI checks.
The UI build, TypeScript check, selected lint, formatting, and staged-content hook checks pass.
The new history and scheduler contracts pass with the existing related suites.
The Claude change passes 110 checks across bridge, account, admission, phase, cache, and idle-retirement contracts.
The old bridge fails the new refresh and phase assertions.

The installed backend still uses protocol 2. Its update uses scoped changes against its exact installed source.
The UI patch retains one module graph and updates the offline manifest.
A hidden desktop check finds no folder-size labels or requests with 37 workers.
The application signature passes strict verification.

## Live restart and restoration

The saved supervisor plist specifies `Interactive`, but the registered job still reports `daemon (3)`.
A `kickstart` did not replace that registration.
The authorized restart closes seven exact native generations with durable close receipts.
It uses `bootout` and `bootstrap` with the saved plist.
The new supervisor reports `interactive (4)`. Its process priority changes from 20 to 31.
Its new native children also report priority 31.

Backend PID 86660 starts in supervisor mode with no fallback.
The state directory identity and original recovery configuration remain unchanged.
All 41 saved actors retain their accounts and thread identities.
Three workers receive newer deletions after restoration. Those changes remain in place.
All original event IDs remain present.

Fourteen continuation inputs reach `delivered`, including all 12 lead chats.
All 12 leads have later nonempty assistant output.
Eleven leads have output in the exact accepted continuation turn.
The remaining lead has output in a later rule turn.
The 25 actors in the resume plan have no current error at the check.
They run, wait for work, or have completed their turns.
Eleven command monitors remain active at that check.
Restoration uses fixed input IDs and does not repeat commands with unknown outcomes.

## Limits and evidence

The first cold Claude start still requires a catalog probe and the actual session process.
The old timeout receipts do not identify their blocked phase.
The new phase fields support diagnosis if another timeout occurs.
This repair does not prove that all future provider timeouts are eliminated.

Backend initialization takes more than the restart script's 120-second readiness window.
The recovery launcher stays enabled. The same backend then becomes ready and resumes the chats.
The native startup sample shows JSON and SQLite work, but does not identify its exact Python caller.

A later 30-second sample measures 81.81% backend CPU under a different workload.
It identifies session-cost refresh at 22.55%, history at 6.57%, and the scheduler at 4.92%.
These live samples are not a controlled before-and-after comparison.

Private measurements and apply receipts are under `/private/tmp/studio-cpu-remove-disk-20261007`.
Private restart evidence is under the existing state directory at
`diagnostics/interactive-supervisor-20261007-1791383659339547000`.
The evidence includes exact source hashes, close receipts, registration readback, continuation IDs, and output checks.
