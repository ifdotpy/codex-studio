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

CLI transport reuses the local `codex-control` HTTP URL and token bootstrap.
Each explicit CLI action reads `/api/state` once to get the session token. The
token read and each POST have a five-second timeout. After a committed write it
retries the same notification identity and payload once, after a 100 ms delay,
on transport errors, HTTP 429, or server errors. The notification path can take
up to about 15.1 seconds. A final failure is reported as a committed source
write with an unconfirmed invalidation; callers must not repeat the source
operation to recover it. Refreshing the stream or reconnecting receives the
server's full baseline. No durable command or notification queue is created.

Focused route and subprocess checks live in `test_relay.py`.
