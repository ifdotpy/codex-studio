# Sync protocols

This document defines Studio's change notifications and durable pull recovery.
The renderer uses protocol 3 for live notifications. Protocol 1 remains the
legacy scoped payload/cursor contract described below; protocol 2 is the older
generation stream. Pull projection schemas and transport versions are separate.

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
`npm run api:generate` emits both TypeScript declarations and standalone runtime
validators. `npm run api:check` rejects stale output. Renderer handlers validate
incoming JSON as unknown before using it. Do not maintain a second handwritten
wire schema or cast incoming JSON to a generated type.

The existing native EventSource transport and RxDB projection cache remain in
use. Adding a separate query cache is unnecessary for this contract. FastAPI
owns SSE framing; Ajv compiles the Python/OpenAPI schemas at build time so the
browser needs no schema compiler or dynamic code evaluation.

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
subscriptions from tabs and distributes validated events. Without that
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

## Legacy protocol 1

The remaining sections describe the legacy scoped payload transport. Their
fallback rules apply to legacy clients only; the protocol-3 renderer never
uses a periodic pull fallback.

### Negotiation and compatibility

`GET /api/sync/protocol` returns `protocolVersion: 1`, supported versions,
capabilities (`pull`, `stream`, `streamChanges`, `entityReset`, `unixSocket`),
scope names, maximum page sizes, and the stream endpoint. A client must check
that version 1 and `streamChanges` are advertised before subscribing to
changes. A 404, invalid JSON, unsupported version, or missing capability means
use the existing pull-and-invalidation behavior. A client sends
`X-Codex-Sync-Protocol: 1` on native stream requests. Browser `EventSource`
clients, which cannot set request headers, use `protocol=1` in the stream query.
Unsupported requested
versions receive HTTP 426 with supported versions. Version 1 servers retain
the old stream event format when that header is absent. The server also keeps
the protocol-2 shared generation/invalidation stream for clients that already
use it; `streamChanges` is an independently negotiated payload stream.

The schema version in `state:entities:v1` names the entity projection format;
it is independent of this transport protocol version. Unknown response fields
must be ignored. Clients must reject an unknown protocol version without
changing local state. During rollout, new clients use only capabilities
explicitly advertised by the server; old servers and renderers continue using
pull.

### Identity and scopes

`GET /api/sync/identity` returns the stable `workspaceId`. Every pull result
and stream event includes it. Clients must not apply data from another
workspace. Supported pull scopes are:

- `state:entities:v1`: the shared entity sequence space, excluding transcript
  entities. Documents use `entity:<collection>:<id>` IDs.
- `transcript:<agent-id>`: one transcript projection. Transcript payloads can
  be full snapshots or deltas; deltas contain changed `items`, `removed`, and
  optionally `order` and `itemRevisions`.
- `drafts`: local drafts and their tombstones.
- `state` and `state:chat`: legacy whole-projection snapshots. These remain
  pull-only and may use the legacy invalidation stream.

An entity or draft document is `{id, payload, seq, _deleted?}`. `seq` is a
non-negative safe integer. Sequence order is durable and increasing within
each scope's sequence space. A scope cursor advances only after its documents
are durably applied. A client may coalesce multiple changes to one document
by retaining the greatest `seq`.

### Pull pages and cursors

`GET /api/sync/pull?scope=<scope>&after=<seq>&limit=<n>` returns
`workspaceId`, `documents`, and `checkpoint: {seq}`. Limits are clamped to
500 for entities and 100 for other scopes. Pages are ordered by sequence. An
empty delta keeps the requested cursor. Entity pages also include `maxSeq`.
The page's checkpoint is the greatest included sequence, or `maxSeq` when the
page is short. Clients persist each page before advancing the checkpoint.
Pull remains available to every protocol version and is the recovery path for
all stream resynchronization events.

Entity pulls use one SQLite read snapshot for the documents, floor, and checkpoint.
They do not wait for transcript scope locks or for another entity pull's writer wait.
Projection maintenance uses SQLite's writer lock and reads the source again after
it obtains that lock. A maintenance failure returns an error without a checkpoint.

The server records global API reads that exceed one second in
`<state-directory>/diagnostics/http-requests.json`. The existing update tick saves
active requests and their stack locations without the runtime lock. The journal
retains at most 128 active requests and 32 completed slow requests. It stores
constant endpoint names and scope types. It excludes query values, chat IDs,
request bodies, and tokens. Streams and writes do not enter this journal.
The first journal write preserves a nonempty previous session in
`http-requests.previous.json`.

### Entity floor, reset, and fresh baselines

The entity server retains at most 10,000 non-transcript tombstones. Pruning is
asynchronous, in batches of at most 500 rows per committed transaction, with
150 ms between batches. Live entities are never pruned. The greatest pruned
sequence is `floor`; `maxSeq` is at least `floor`.

Clients implementing reset send `reset=1` on entity pulls. If
`0 < after < floor` outside a fresh baseline, the server returns
`{workspaceId, reset:true, floor, maxSeq}` without documents or an ordinary
checkpoint. A client that did not opt into reset retains its legacy response
shape. On reset the client persists a hidden/resetting marker, clears only
entity projection rows, starts again at zero, and keeps drafts and outbox rows.
It publishes the projection only after the replacement reaches the page
sequence's `maxSeq`. An interrupted reset must resume before the projection is
shown.

An initial full baseline uses `fresh=1`. The server returns `initialHigh`;
the client repeats `fresh=1&initialHigh=<returned value>` on every page. A
fresh baseline includes all live rows and all tombstones newer than
`initialHigh`, even if a live row has an older sequence. Once it reaches
`maxSeq`, the client resumes ordinary deltas. This rule also applies when
continuing a reset baseline and prevents repeated reset responses while the
checkpoint crosses old live rows.

### Drafts and transcripts

Draft writes use the existing authenticated `/api/sync/drafts` endpoint and
workspace header. A draft pull returns the latest retained row per draft ID;
clients apply by sequence and preserve tombstones. A stream cursor that is no
longer replayable emits `reset` and requires a draft pull from zero.

Transcript deltas are produced by `/api/sync/pull` and are scoped to one agent.
The stream carries the same projection document shape and cursor semantics.
A transcript delta requires its existing item base. If the base is absent,
the client pulls from zero and applies the resulting full snapshot before
accepting deltas. A legacy cached transcript stored as one document with an
`items` array is not a valid base for applying a delta to per-item storage:
clients must first migrate every cached item into item rows or pull and apply a
full snapshot. A transcript retention floor or deleted/recreated projection
also requires a full pull. Clients must not infer a full transcript from a
delta.

### Resumable change stream

Subscribe with:

```text
GET /api/sync/stream?scope=state%3Aentities%3Av1&after=120
Accept: text/event-stream
X-Codex-Sync-Protocol: 1
```

`after` is the initial cursor when opening a new EventSource. On automatic SSE
reconnect, the server uses `Last-Event-ID` in preference to `after`. Each
`changes` event has `id: <seq>` where the ID is the event's ending scope
sequence, and JSON data:

```json
{
  "protocolVersion": 1,
  "workspaceId": "…",
  "scope": "drafts",
  "documents": [{ "id": "…", "payload": "…", "seq": 121 }],
  "cursor": 121
}
```

An event contains a bounded batch (at most 100 documents and at most 1 MiB of
encoded data). The event ID/cursor advances only through changes represented
by that event. The current server stores latest entity/draft values, so a
replayed range may collapse intermediate writes to the latest document per
ID; this is state replication, not an audit log. A stream for an unsupported
scope returns 400. The server emits a comment heartbeat at least every 15
seconds while idle.

The server retains no per-client event queue. It reads bounded pages from the
durable store and writes directly to the socket. Socket backpressure blocks
only that handler; a write timeout closes a slow connection. On reconnect, if
the cursor is ahead of the scope high-water mark the server sends `cursor-ahead`
and closes; the client discards that cursor and obtains the current high-water
mark with a pull. If a cursor is below the retained floor, the server sends a
`reset` event containing `reason`, `floor`, and `maxSeq`, then closes. The
client performs a full pull/reset before reconnecting. Cursor equality with
the high-water mark is valid. Gaps caused by writes in other scopes are valid;
the stream cursor is scope-local.

Event `error` messages are JSON. `version-mismatch` closes with HTTP 426 before
SSE headers; `cursor-ahead` and `reset` are normal terminal SSE events; abrupt
disconnects are retried by EventSource. HTTP 400 means invalid scope/cursor,
403 means origin/authentication rejected, 409 means workspace mismatch, and
426 means protocol mismatch. Clients must not advance state on an error.
Pull is the authoritative way to recover after any terminal stream event.

The stream is usable by another server: scope, workspace, durable cursor,
documents, reset behavior, and version are explicit; authentication and
authorization are handled by the same local-origin policy as other read APIs.
Peer clients must use their own workspace identity and must not treat a stream
connection as write authorization.

### Authentication and transports

The HTTP API binds only to `127.0.0.1`. Tailscale Serve may proxy this listener
over HTTPS using the existing exact-origin and Tailscale-source validation;
the client still uses the same protocol and session/write token. Forwarded
headers alone never grant trust. Native local clients may use the same HTTP
paths over the state-directory Unix socket, mode `0600`; the socket is an
additional local transport, not a non-loopback listener. Both transports
enforce protocol/workspace checks and normal write tokens.

### Native client requirements

A native client porting this contract must: negotiate version/capabilities;
validate the workspace identity; persist scope cursors only after applying
documents; obey page and payload limits; implement entity `reset=1`, hidden
baseline replacement, floor and `fresh/initialHigh` rules; preserve drafts and
outbox during entity reset; apply transcript full snapshots and deltas only
with a valid item base in the native storage format. A legacy cached full
transcript is not sufficient if the client persists only changed delta items;
it must migrate all cached items to item rows or pull a full snapshot first.
Reconnect with `Last-Event-ID` or explicit `after`; handle
cursor-ahead/reset by pulling; cap its own buffers; treat heartbeats as liveness
only; and fall back to pull on unsupported capability, version, or stream
failure. It must never interpret a stream cursor as authorization or repeat a
write because a response was lost. No changes to the separate Attar Rust
branch are part of this task.

### Limits

The server clamps pull and stream batches, and every stream handler owns only
one bounded result batch. SSE is a delivery hint with replay from current
durable state, not a transaction log. State may be coalesced. Clients that
need every intermediate mutation require a separately versioned append-only
log. Browser/EventSource reconnect behavior is subject to browser suspension;
visibility resume must reconnect from the last durable client cursor.
