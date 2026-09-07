# FieldView request admission latency

Status: OPEN. Complaint `142b6ced-a6be-57e9-9c5e-3c3d3fb8295d`.
The blocking callback or lock has not been identified. More live timing data is
required before this incident can be called fixed.

The persisted request registry confirms both operations applied successfully:

| Call ID | Before registration | Registration to start | Total queue delay |
| --- | ---: | ---: | ---: |
| `exec-82b8cb7f-2fbf-49fb-9503-139bb600e6f4` | 144670.13 ms | 648.51 ms | 145318.64 ms |
| `exec-20c46325-5e2a-423e-81c7-942159f756a4` | 117720.55 ms | 621.76 ms | 118342.31 ms |

The first operation finished 712.78 ms after it started. The second finished
1049.67 ms after it started. These receipts do not indicate failed mutations.
They do not authorize retries with new request IDs.

`AppServer.read` timestamps wire receipt. Its ordered callback queue invokes
`Runtime.request`, which reserves the request before submitting it to a tool
executor. The old registry cannot split callback queue time from reservation
lock time. Increasing executor capacity does not address the measured interval.

## Changes and limits

- New receipts split `callbackQueueDelayMs`, `reservationDelayMs`, and
  `executionQueueDelayMs`. `admissionDelayMs` and the existing `queueDelayMs`
  retain the complete intervals.
- `callbackLatency` records in each connection's `app-server.log` identify slow
  callbacks by method, thread, turn, and RPC ID. Records include duration and queue
  depth. They exclude message text and tool arguments.
- Adjacent text deltas for the same item are processed in bounded batches.
  Requests, receipts, and lifecycle events remain ordering boundaries. Tests
  preserve every character, analytics count, and analytics payload-byte count.
  This reduces callback transactions under text backlog. It does not establish
  the cause of this incident.
- `wait_transcript` releases the UI condition before acquiring the runtime lock.
  The previous opposite lock order could deadlock a reader and a writer. A
  regression verifies that a writer can acquire the condition during transcript
  reads. No evidence connects that separate defect to these two requests.

## Required live evidence

Activate the instrumented server only at a safe restart boundary. Preserve
running agents and commands. For the next delayed request, retain its exact ID,
the new timing fields, and preceding callback latency records for its connection.
Use these to identify the callback or lock that blocks admission. Verify the
specific fix under concurrent agent activity before resolving this complaint.
