# Lumina terminal output queue saturation

Observed on 2026-09-08. The account log contains two failures:

```
Canvas protocol error: Codex callback queue saturated; rejected {"method": "item/commandExecution/outputDelta"}; connection closed; outcome unknown
```

Affected lead: `2eda0742-0ace-4fe0-b9dc-873ff3ddd59b`.
The state API reported `interrupted`, `inFlight=false`, and a disconnect error.
The Studio HTTP server still answered its state request in 85 ms.
This incident differs from the earlier null transcript failure.

## Cause and change

`AppServer.enqueue` admitted each terminal fragment as a separate queue entry.
The queue held at most 4096 callbacks. On overflow, `fail_transport` terminated
that account's native process. Existing batching covered assistant text on the
consumer side only. It did not protect terminal output admission.

Admission now joins adjacent terminal fragments for the same callback, method,
and complete parameter identity. A batch contains at most 128 fragments and
65536 characters. Requests, item changes, and lifecycle events remain barriers.
The queue retains its memory bound and explicit failure for other overflow.
This fix does not guarantee admission for every possible sustained workload.

Original samples remain available to notification analytics. Event counts and
encoded byte counts therefore retain their original values.

## Checks

- The 14 protocol reader tests pass.
- A burst of 5000 terminal fragments fits a 64-entry test queue while its
  consumer is blocked. A separate response still completes.
- Output text, Unicode, request boundaries, and item boundaries are preserved.
- The text and terminal analytics tests both pass.
- The same checks pass against the merged application source.

## Recovery and deployment boundary

A single recovery message was accepted with receipt
`studio-lumina-recovery-20260908-output-overflow`.
The lead then reported `running`, no error, and native turn
`01a080f5-7906-7181-855f-105bd86ff43d`.
The recovery message requires checking existing processes and artifacts before
repeating uncertain commands. It requests bounded output until deployment.

At this record's creation, live code activation awaits the macOS administrator
dialog. Recovery is observed; permanent live deployment is not yet verified.
