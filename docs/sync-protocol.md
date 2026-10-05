# Sync protocols

This document defines Studio's change notifications and durable pull recovery.
The renderer and server use protocol 3 for live notifications. Unsupported and
unspecified stream protocol selectors receive HTTP 426. Pull projection schemas
remain separate from the stream transport version.

## Protocol 3: resource changes without polling

The server sends small typed messages identifying changed resources. A subscribed
consumer reads that resource once after an initial notification, a real change,
or reconnection. A quiet interface sends no background state requests. This
covers panels, queue and receipts, terminals, accounts and login status, models,
limits and costs, tasks, workspace, desktop status, worktree sizes, transcripts,
rooms, drafts and voice records. User actions, media transport, and recovery of
an already accepted write retain their own request identities and lifecycles.

### Authoritative types

[Python resource models](../scripts/studio_api/sync/resources/models.py) own the
closed resource union and named event envelopes. OpenAPI publishes those models;
`npm run api:generate` emits TypeScript declarations and the SHA-256 identity of
the canonical OpenAPI JSON. `npm run api:check` rejects stale output. API
responses include the server identity. Hash-bearing protocol-3 connections begin
with an `api-schema` event; mismatches get only that handshake before close.
Hashless non-renderer clients retain the existing event sequence. Mutating
requests with a present mismatching hash receive a marked 426. A mismatch stops
resource and draft synchronization and message delivery until the renderer
updates. Stream payloads are not walked against generated
runtime validators; handlers retain structural preconditions and workspace,
epoch, revision, and ordering checks.

The existing native EventSource transport and RxDB projection cache remain in
use. Adding a separate query cache is unnecessary for this contract. FastAPI
owns SSE framing; the renderer and server compare the same generated schema
identity before continuing with live updates.

### Subscription and notification

A same-origin EventSource requests `/api/sync/stream?protocol=3&resources=...`.
`resources` is a URL-encoded JSON array of the generated resource union. References
identify a resource kind and, where required, its nonempty agent, task, terminal,
room or account identifier. Per-agent transcript references prevent a change in
one conversation from reloading unrelated conversations.

Named `resources` events carry protocol, workspace identity, process epoch,
monotonic revision, reason and affected resource references. The authoritative
field definitions and accepted reasons are in the Python models. The initial
notification reconciles all subscribed resources. Ordinary change notifications
contain only affected references. Subscribers coalesce duplicate references and
serialize reads; a notification arriving during a read schedules one further
read so the update cannot be lost.

The server installs a subscription and captures its initial state without a gap
that could lose a committed change. Producers notify only after a successful
mutation is visible. Rollbacks, no-op writes and projection-cache maintenance
must not produce a resource change. Completing an asynchronous refresh may
notify waiting consumers even when its data is unchanged, because its loading
or error state has settled. Fetching an unchanged projection must not recursively
invalidate that projection. Queues and pending references are
bounded; overflow requires explicit reconciliation rather than silent loss.

Where browser coordination is available, one stream owner combines the active
subscriptions from tabs and distributes stream events. Without that
coordination, a tab may open its own stream. Neither case enables HTTP polling.
Unused subscriptions and their source watchers are released. A new owner requests
peer subscriptions again. Peer heartbeats do not replay unchanged resource
baselines. The owner retains versions for its active aggregate and at most 128
inactive references; an evicted reference requires a fresh server baseline.
A late follower receives a targeted baseline without waiting for a source change.

### Connection loss and recovery

Named `heartbeat` events prove liveness and never trigger resource reads.
Comment-only SSE heartbeats are insufficient for a browser silence timeout
because EventSource does not expose them to JavaScript. A silent, failed or
closed stream changes the visible connection state; reconnect attempts use
bounded increasing delay with jitter. Voice capture closes when the connection
becomes unavailable.

There is no periodic `/api/sync/generations` request and no polling fallback.
While the stream is unavailable, automatic state updates are unavailable.
A successful reconnection reconciles the current subscriptions once. It does
not retry user commands, messages, credit charges or other writes.

A transient failure while reconciling a notified resource may retry that same
outstanding read a finite number of times, with backoff and jitter. These retries
stop when the connection is unavailable or the consumer unsubscribes. Exhaustion
leaves a visible error until an explicit retry or a new change/reconnection; it
must not start a periodic refresh loop. Successful reads do not schedule retries.
Draft storage bootstrap permits three delayed retries (1, 3 and 10 seconds) after
its initial attempt, then pauses until explicit activity or resume. Pending-write
recovery keeps its existing journal and operation identities.

An unchanged reconnect baseline still reconciles a subscribed consumer once.
For transcript prefetch, repeated notifications for the same version during an
active history read are coalesced; a newer version schedules a follow-up read.
If that read fails, a coalesced recovery signal permits one further attempt.
Paginated catch-up drains all available pages before resolving the notification,
with cancellation and a cursor-progress check.

An epoch distinguishes a server process restart from an old event revision.
The client rejects malformed messages, incompatible protocol versions and
another workspace's events. Duplicate notifications within an epoch do not
cause repeated reads. Stream revisions are transient notification sequence
numbers; they are not durable projection cursors. Reconnect/reset uses current
resource reads, so missed notifications do not require an unbounded event log.

Named `token-rates` events preserve live token-rate telemetry on the same stream.
They use the generated Python contract and update the existing renderer cache;
they do not cause a periodic limits or costs request.

### Notifications from other processes

Supported CLI writers publish their committed resource changes through the
[authenticated notification relay](../scripts/studio_api/sync/resources/relay/README.md).
`POST /api/sync/notify` accepts the Python-defined request identity and resource
references; the ordinary API token and request-boundary checks still apply.
The CLI verifies that the API state directory matches the source write's root
before sending a notification.

A bounded process-local receipt cache acknowledges an identical retry without
republishing it; reusing a retained identity with a different body returns 409.
After receipt eviction or server restart, repeating an invalidation is safe.
This is not durable exactly-once delivery. A lost or truncated response retries
only the same notification, never the original command or database write.
If notification delivery remains unconfirmed, the CLI reports that the source
change has committed. Reconnecting the stream obtains a full subscription
baseline. Arbitrary external SQL writes are not supported event sources.

### Worktree measurements

Worktree size notifications report completed measurements. The scanner wakes
on a request, rather than scanning on a timer. A cached size is not evidence
that arbitrary external filesystem writes have been detected: repository trees
are not recursively watched by the progress observer. The response's measurement
time identifies the age of the returned result.

### Progress files and layout measurements

An explicit native filesystem observer watches only the progress file belonging
to an actively subscribed panel. Watching its parent catches atomic replacement,
creation and deletion, but events for other names are ignored. Reads retain the
existing path, symlink and size checks. Content/revision changes invalidate the
panel. The last unsubscribe removes the watch. The observer does not watch
repositories, SQLite files, logs or `PROGRESS.layout.json`, and does not silently
switch to filesystem polling.

Layout measurements are sent when the actual measurement changes. They do not
renew themselves on a timer; an old measurement may become unmeasured under the
existing freshness policy. The layout response schema accepts the stored
per-client report shape, whose revision may be absent, while the top-level
response retains its current file revision.
