# Authorized supervisor restart, 2026-10-07

The user requests a restart and restoration of all work.
The restart activates the Journal connection pool from commit `24f45a86`.
It retains the existing state directory and all saved operation identities.

## Cutover

- The private snapshot records 33 actors with activity today, 16 active actors, seven native handles, and 16 active monitors.
- Two workers already have explicit stops from their lead. They remain under that lead's control.
- The recovery plan checks 31 actors, including 11 lead chats.
- The existing recovery launcher pauses through its saved configuration.
- Backend PID 3058 exits gracefully and saves restart receipts.
- The operator closes each native handle after checking its PID, process start, signature, and generation.
- Every close has a durable receipt and a dead-process readback before supervisor replacement.
- The independent supervisor LaunchAgent starts PID 14714 with the installed pool source.
- The recovery configuration returns byte for byte to its original contents.
- The recovery LaunchAgent starts backend PID 14728 in supervisor mode. Fallback remains disabled.

The old native commands can end when their process groups close.
Their owners check receipts and existing processes before they start another command.
The recovery does not automatically repeat an unknown command or remote build.

## Restoration

All 11 lead continuation messages reach `delivered` and have later nonempty assistant output.
Ten have output in the exact accepted turn.
The FieldView optimizer reports its saved stop on heavy computation and restores its observation timer.
It does not start another build without the user's requested go-ahead.

Seventeen continuation messages reach `delivered`, including six worker messages.
The remaining workers already run or have finished their tasks.
The plan uses fixed message IDs. A lost response does not create another input.

The `product-decompiler` worker initially cannot start because its lead uses Single agent mode.
The user requests restoration of all workers.
The recovery changes that chat to Multi agent through the revision-checked conversation API.
Its limit returns to 32. Its account, model, thread, epoch, and worker defaults remain unchanged.
The worker's original continuation ID reaches `delivered` and has new assistant output.

At the final check, all 16 actors active before the restart run, wait for work, or have completed.
None has a failed, interrupted, queued, or paused status.
The checked actors have no current error.
Fifteen monitors are active. The original 16 monitor records have two completed results and 14 lost outcomes.
Owners restore observation with new monitor records rather than repeat the original commands.

All 29 original pending-event identities remain present.
Three pending inputs become delivered.
The 26 previously uncertain inputs remain uncertain.
One previously completed worker acquires a newer stop during recovery. The recovery preserves it.
All checked actors retain their account identity.

## Live checks

The desktop endpoint confirms backend PID 14728, supervisor mode, no fallback, and no degraded or blocked supervisor ownership.
The diagnostics endpoint returns HTTP 200.
The state directory retains its device and inode.
All seven saved account handle names have new native children.

The new supervisor starts from installed source SHA-256 `1902bda7c53ba122edd3386c33c9b86c5f71ea10d10064559af32544b8a5aba8`.
Three health probes retain the same SQLite file descriptors between calls.
This confirms retained database handles in the new process; the source tests establish the exclusive connection-lease behavior.

Private evidence, mode 0600, is in the existing state directory:
`diagnostics/planned-restart-20261007-1791373753069199000`.
It contains the snapshots, close receipts, exact continuation IDs, mode-change receipt, native output checks, and final recovery proof.
