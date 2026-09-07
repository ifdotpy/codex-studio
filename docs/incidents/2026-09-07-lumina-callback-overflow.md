# Lumina callback queue overflow

Status: OPEN. Live connection recovery is not verified.

## Evidence

- Runtime PID: 35212, started from the project source on port 4620.
- Account: `profile-1a70871ee4be5b7a22dc`.
- Lead: `2eda0742-0ace-4fe0-b9dc-873ff3ddd59b`.
- The account log records `Codex callback queue saturated` for
  `account/rateLimits/updated`, followed by connection closure.
- One child process of the runtime, PID 6035, is defunct. Its account identity
  was not verified from inside the process. The other two child processes remain.
- The lead and `lhs-emulator` report `Codex app-server is offline`. Six other
  workers retain running state. Those database labels do not prove live execution.
- The limits endpoint returns values with recent timestamps. This is insufficient
  evidence of recovery: queued notifications used processing time as their age.

The log proves the immediate cause of closure. It does not identify the callback
or lock that caused the queue to fill. Do not close the earlier admission-latency
incident based on this evidence.

## Source fixes and checks

The notification batching and callback latency instrumentation from `389fcd1`
are present in the source. They were not activated in this running server.

The limits changes from `6e2a155` isolate reads by account. The source now also
uses the pipe receive timestamp for delayed quota notifications. Older queued
notifications cannot overwrite newer reads. Rejected stale samples remain in
analytics with their receive and processing timestamps.

`tests/limits-refresh-contract.py` covers stale notification age, read ordering,
retained analytics, account isolation, and timeout recovery.

## Recovery boundary

The existing connection code waits for accepted callbacks to drain before it
marks a connection offline. It cannot replace that connection while its callback
dispatcher still drains. Restarting the whole runtime interrupts other accounts.
No running worker command or monitor was stopped. No uncertain input was resent.

Read-only inspection through CPython 3.14 `sys.remote_exec` was rejected by macOS:
`Cannot get task port for PID 35212`. A noninteractive administrator attempt also
failed because sudo requires a password. No inspection script executed.

Resume with authorized administrator access for process inspection, or restart
Studio after the other active work ends. First inspect the callback queue and
preserve pending receipts. After recovery, verify a fresh native read and the
lead's turn state. A cached quota response alone does not establish recovery.
