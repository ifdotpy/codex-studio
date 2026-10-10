# External resource relay

External CLI writers call the authenticated `POST /api/sync/notify` endpoint
after their SQLite transaction commits. The canonical wire models are in
`models.py`; `router.py` validates and publishes the typed resource list through
the process-local resource hub.

Request receipts retain up to 256 exact request ID/body pairs per server
process. Repeating the same pair while retained returns the acknowledgement
without publishing again. Reusing an ID with different normalized resources
returns HTTP 409. Receipts are intentionally not durable: a server restart or
receipt eviction may publish an invalidation again, which cannot repeat the
source write. A new SSE subscription receives a full baseline to reconcile state.

CLI transport reuses the local `codex-control` HTTP URL and shared API client.
Each explicit CLI action reads the token from `/api/session` and the state
directory identity from `/api/desktop`. It compares that identity with the
source state directory before sending a notification. A mismatch is reported
as an unconfirmed invalidation and prevents publication to the wrong workspace.
The session and desktop reads and each POST have a five-second timeout. A 404
from either bootstrap endpoint means the connected backend may be older than
this client. After a committed write it
retries the same notification identity and payload once, after a 100 ms delay,
on transport errors, HTTP 429, or server errors. The notification path can take
up to about 20.1 seconds, including both five-second bootstrap reads and two
five-second POST attempts with the retry delay. A final failure is reported as a committed source
write with an unconfirmed invalidation; callers must not repeat the source
operation to recover it. The error directs the operator to reconnect the event
stream, which receives the server's full baseline. No durable command or
notification queue is created.

Focused route and subprocess checks live in `test_relay.py`.
