# Spawn recovery, 2026-09-07

Related incident: `f53790e3-1fd4-5ab2-a947-75fd43ea4b52`.
The broader cancellation incident is `34b8c45c-b6c3-530b-a05e-738db97547d0`.
These findings do not claim lost completion notifications.

## Recorded evidence

The owner reported two failed batches, three workers in exec773 and four in
exec789. Read-only SQLite and native rollout inspection found both saved tool
results. Each contains `model/list response timed out; outcome unknown`.
Catalog access precedes creation in the inspected runtime. All seven planned
UUID5 worker identities and names are absent from the registry.

| Request | Native start UTC | Native cancellation UTC | Studio task creation UTC | Stored result UTC |
|---|---|---|---|---|
| exec773 | 11:57:54.557 | 11:59:40.224 | 12:00:21.095 | 12:01:22.512 |
| exec789 | 13:11:33.218 | 13:12:33.941 | 13:13:57.475 | 13:14:57.821 |

The exact native call IDs are `exec-7ecb02a4-a933-49d4-b110-ddc6d2c1ff9d`
and `exec-45a0d3ee-1e12-40f9-be9b-bd5a91258292`.
Task creation after receiver cancellation proves delayed host processing or
recording. It does not identify the exact historical queue or lock responsible.

The new CLI recovered exec773 from the still-running legacy server. It returned
the original model/list error through a read-only database connection. This path
does not need a backend restart or another spawn. A failed legacy result remains
`unknown`; the separate registry and source evidence support the pre-create
finding for these two batches.

Cancelled peers775 and status779 have successful saved results. The status779
native call started at 12:14:18.463. Studio recorded its task at 12:22:24.423 and
saved its successful result at 12:22:40.211.

A fresh batch succeeded at 13:17:54.272. The cached registry has 20 agents before
that batch and 23 after it. At 13:19, six workers were running. Each new worker
later produced a delivered child_result event. Repeated worker text alone does
not prove duplicate transport delivery.

## Changes and verification

The runtime now stores request state before executor submission. Spawn uses
stable caller IDs and one transaction for workers, initial events, and receipts.
Recovery reads expose saved results without another mutation. Queued cancellation
prevents execution. Cancellation during spawn preflight prevents its transaction.

Native response reads are separate from ordered callbacks. Recovery and
coordination have separate pools. Account-scoped model metadata uses one pending
native read and a bounded wait. See [the recovery contract](../../ORCHESTRATION.md).

Regression checks cover blocked callbacks, blocked pools, delayed catalog
responses, transaction rollback, lost replies, stable-ID replay, and parent wake.
The explicit cloud canary is separate from these local fixtures. It ran against
Codex CLI 0.153.4 with two real Sol turns in an isolated Studio database. Worker
creation, a running registry state, completion, and child_result delivery all
passed. Recovery read the spawn receipt after an injected reply failure without
replaying the mutation. The no-op monitor returned exit 0 and 18 output bytes.
This check passed in 11.421 seconds against the final runtime source. A separate
regression changes the connection after metadata returns and proves zero created
workers. Another blocks the tool and coordination pools while an older thread
recovers its receipt through the workspace fallback.
The [native evidence](2026-09-07-spawn-native-evidence.json) records exact source
hashes, worker and event identities, and monitor output size.

## Deployment limit

At investigation time, backend PID 78700 predates these source changes. Updating
source files does not replace loaded Python code. Active worker commands must
remain intact. Production activation requires the existing idle upgrade path;
isolated native verification cannot prove the old process has been upgraded.
