# Sync protocols

This document defines Studio's change notifications and durable pull recovery.
The renderer and server use protocol 4 for live notifications. Unsupported and
unspecified stream protocol selectors receive HTTP 426. Pull projection schemas
remain separate from the stream transport version.

## Rule: no polling, only SSE events

This section is the single owner of this rule. Other documents link here and do
not restate it. The [screen subscription rule](#subscriptions-follow-the-screen)
defines which resources belong on that stream.

**The client never polls the server.** Every change the server makes reaches the
client as an event on the one `/api/sync/stream` SSE connection: an entity
upsert or delete when the data is bounded and safe to persist, otherwise a
transient typed frame. The client applies the event; it does not answer an
event with an HTTP read.

**The data travels inside the stream.** An event carries the changed records
themselves, not a notice that something changed. When a subscription is added,
the stream sends that resource's current records right after the
acknowledgement. A "something changed, fetch it" signal followed by a separate
GET or pull is not an allowed design, neither for live changes nor for the
initial data of a new subscription. Only an event that exceeds the stream size
limit may carry revisions alone; the client then reads that one interval.

The client sends an HTTP request in exactly these cases:

1. **First load** of a view, before the stream has delivered its baseline.
2. **Recovery** after a stream reconnect, a reported gap, an expired cursor or
   an event larger than the stream limit. One bounded read per resource,
   inside the shared read-recovery budget.
3. **An explicit user action**: a click, a submit, opening unbounded detail such
   as terminal output, a diff or history, or pressing retry.

Anything else is a defect, in particular:

- a timer, interval or backoff loop that reads server state;
- a refetch on window focus, tab visibility or route change;
- a GET or pull issued because an event said that something changed;
- a business write (`POST`, `PUT`) the user did not trigger.

Subscription interest updates are transport control messages. They carry typed
references and never read data or repeat a business action.

A quiet interface on a live stream sends no HTTP requests. A new feature that
needs server data adds a stream resource and an event; it does not add a read.

## Subscriptions follow the screen

This section owns the rule: a mounted view subscribes to the resources it displays.
Identical typed references share one subscription across components and cooperating
tabs. The [subscription module](../workspaces/client/apps/web/src/sync/subscriptions/index.ts)
exposes `useSubscription(ref, { graceMs })` and domain data with loading, live, stale
or error status. It owns the [grace interval in milliseconds](../workspaces/client/apps/web/src/sync/subscriptions/contracts.ts),
reference counting and transport integration. Returning to a recently viewed chat
within grace reuses its interest without a subscription change. A resource with
no mounted reader is stale, including while its grace interval runs. Offscreen
chats retain cached history without opening a subscription to keep it warm.

The [OpenAPI document](../workspaces/runtime/packages/runtime-http/openapi.json)
owns `ResourceRef` and `SubscriptionDataMap`; Rust and TypeScript consume generated
types. Diagnostic keys are derived from references and are never parsed back.
The [server registry](../workspaces/runtime/packages/runtime-http/src/routes/subscriptions.rs)
owns each resource kind's initial snapshot, change filter, encoded byte bound and
diagnostic key. A parity witness requires exactly one handler and data contract
for every generated resource kind. Business services publish domain changes
without tracking sessions or subscribers.

Transcript history uses a typed `transcript` reference with an optional `tail`
window. Its [default item count](../workspaces/runtime/packages/runtime-schema/src/shared_values.rs)
is generated for the renderer and checked against OpenAPI. The subscription
baseline contains the newest window and its older-history cursor; the server
shrinks that window further to fit the complete SSE byte limit. Only a newest
item that cannot fit by itself requires oversized-item recovery.

Scrolling to older history uses the [explicit user action exception](#rule-no-polling-only-sse-events):
one bounded `/api/transcript/page` read starts before the retained boundary.
The renderer merges pages by item identity into the current conversation range,
retaining the scroll anchor and both paging cursors. Live deltas preserve that
boundary even when the shared transcript projection covers a larger range.

## Protocol 4: resource changes

User actions, media transport, and recovery of an already accepted write retain
their own request identities and lifecycles.

### Authoritative types

The committed [OpenAPI document](../workspaces/runtime/packages/runtime-http/openapi.json)
owns the closed resource union and named event envelopes.
`pnpm run contracts:generate` emits TypeScript declarations and the SHA-256 identity of
the canonical OpenAPI JSON. `pnpm run contracts:check` rejects stale output. API
responses include the server identity. Hash-bearing protocol-4 connections begin
with an `api-schema` event; mismatches get only that handshake before close.
Hashless non-renderer clients retain the existing event sequence. Mutating
requests with a present mismatching hash receive a marked 426. A mismatch stops
resource and draft synchronization and message delivery until the renderer
updates. Stream payloads are not walked against generated
runtime validators; handlers retain structural preconditions and workspace,
epoch, revision, and ordering checks.

### Entity publication

The renderer reads chats and cards only from their `sync_entities` rows, and a
pull does not rebuild a complete projection. Every writer transaction therefore
publishes the `runtime_agents` and `runtime_requests` rows it changed before it
commits. The [store journal](../workspaces/runtime/packages/runtime-store/src/entity_publication.rs)
records each changed row, and the services publisher projects it through
`sync_policy::publish_entity_changes`. A write path needs no publish call of its
own. A path that publishes after its last write is not published twice.
Volatile agent fields are outside this rule: their writers leave
`runtime_agents` unchanged on purpose.

Retained cleanup folders have their own `retainedWorktree` entities, including
folders of deleted workers. Question answers remain in `questionHistory` entities
after their pending request disappears; secret answers are omitted from this
persistent projection. Both collections follow their source transaction and
hard-delete tombstones. A bounded transcript baseline uses `transcriptSnapshot`
in the subscription's initial resource frame, after its acknowledgement; the
renderer persists it before notifying transcript readers. Oversized frames keep
the scoped recovery hint. Saved Claude repair evidence is read on opening the
view or explicit refresh, never because an agent's status changed. Viewing a
focused completed result records its read receipt; its confirmed mutation
response does not cause another session or projection read. An empty local
sidebar order never creates a migration write; existing pending requests and
nonempty legacy user preferences retain their original migration lifecycle.

Vault setup, AWS sign-in, and pull receipts use the transient
`vault-job-progress` resource, scoped by job kind and request identity. The
subscription baseline and later changes carry the in-memory receipt itself;
sign-in codes and URLs never enter the entity stores. The start action retains
its request identity for one bounded lost-response recovery. Terminal receipts
release the subscription.

Rename completion uses the agent entity for its title and the transient
`rename-progress` resource for the exact request's receipt, including failure.
The selector includes both the agent and request identity; subscription baselines
replay the durable receipt after reconnect. Terminal receipts release the selector.
Saved server names consume each owner's discovery snapshot through its existing
stream. Startup credentials and workspace initialization share the finite
[read-recovery budget](../workspaces/client/apps/web/src/sync/readRetry.ts);
exhaustion stays visible until reconnect or an explicit retry. The visible transcript
uses inline stream snapshots and intervals; a revision-only oversized interval
retains its scoped recovery read.

Open process and work-review panels and complaint threads receive their bounded
records in `viewSnapshot` through typed screen subscriptions. Room baselines
retain a newest-message window and an older-history cursor. Open terminal output
uses Unicode-scalar offsets; the renderer applies overlapping tails as appends
and stores the retained output with its resume cursor under the projection write
lock. A reconnect reuses that applied base or receives a replacement snapshot.
Saved terminal history and explicit retries remain user actions.

The existing native EventSource transport and RxDB projection cache remain in
use. Adding a separate query cache is unnecessary for this contract. The Rust HTTP adapter
owns SSE framing; the renderer and server compare the same generated schema
identity before continuing with live updates.

### Subscription and notification

A same-origin EventSource requests `/api/sync/stream?protocol=4&resources=...`.
`resources` is a URL-encoded JSON array of generated typed references. After the
session handshake, `/api/sync/subscriptions` accepts typed additions and removals;
its generation acknowledgement precedes the added resources' initial data. The
connection stays open when interests change. Only the subscription module knows
about sessions, generations, acknowledgements, cursors or grace timers.

Named `resources` events carry the workspace, process epoch, monotonic stream
revision and bounded domain records. Entity changes use typed upserts and deletes.
Transcript subscriptions receive a bounded visible tail; older history remains an
explicit user action. Durable per-reference cursors describe fully applied local
records. An initial subscription can resume with a delta when its cursor remains
valid; a missing or expired cursor receives a fresh bounded snapshot. Transient
stream revisions never replace durable projection cursors. Snapshot records and
their cursor are read at the same database boundary.

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

### Durable per-ref resume cursors

The [OpenAPI subscription contract](../workspaces/runtime/packages/runtime-http/openapi.json)
defines `SubscriptionAdd`, `SubscriptionCursor`, and `SubscriptionReplyKind`.
Each added ref carries an optional `since: { workspaceId, seq }` cursor. The
stream's initial request carries the same entries in its `since` query parameter.
The server returns `replyKind: delta` and only records newer than the applied
cursor when retained history covers it. A current cursor returns an empty delta.
Missing, foreign, future, or expired cursors return `replyKind: snapshot` and
replace that ref's projection. The durable store sequence and records come from
the same SQLite boundary; stream epochs are independent of these cursors.

The client stores each cursor in its IndexedDB projection after its records are
applied, under the projection write lock. A failed, cancelled, incomplete, or
oversized delivery cannot advance the cursor. Reconnect resends active refs with
these applied cursors through the same subscription path. A covered reconnect
requires no sync pull. Transient resources retain their existing in-memory
snapshot owners and do not create durable cursor rows.

### Connection loss and recovery

Named `heartbeat` events prove liveness and never trigger resource reads.
Comment-only SSE heartbeats are insufficient for a browser silence timeout
because EventSource does not expose them to JavaScript. A silent, failed or
closed stream changes the visible connection state; reconnect attempts use
bounded increasing delay with jitter. Voice capture closes when the connection
becomes unavailable.

There is no periodic `/api/sync/generations` request and no polling fallback.
While the stream is unavailable, automatic state updates are unavailable.
A successful reconnection resumes each current subscription from its last fully
applied durable cursor or receives a bounded snapshot. It does not retry user
commands, messages, credit charges or other writes.

A transient failure while reconciling a notified resource may retry that same
outstanding read within the shared [read-recovery attempt budget](../workspaces/client/apps/web/src/sync/readRetry.ts),
with increasing backoff. The budget counts failed attempts, including the initial
attempt, for both sync pulls and resource reads. Exhaustion reports a visible
read-recovery error and leaves no retry timer. Only a newer resource version or
unversioned notification, a transition back to a live stream, or an explicit user
retry starts a fresh budget. Duplicate or older versions, repeated live status,
and page resume do not replenish it. Retries pause while hidden or offline and
stop when the consumer unsubscribes. Successful reads do not schedule retries.
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
numbers; they are not durable projection cursors. Reconnect/reset uses scoped durable deltas or bounded snapshots, so missed
notifications do not require an unbounded stream event log.

Named `token-rates` events preserve live token-rate telemetry on the same stream.
They use the generated OpenAPI contract and update the existing renderer cache;
they do not cause a periodic limits or costs request.

### Notifications from other processes

Supported CLI writers publish their committed resource changes through the
[authenticated notification relay](../workspaces/runtime/apps/server/src/studio_api/sync/resources/relay/README.md).
`POST /api/sync/notify` accepts the OpenAPI-defined request identity and resource
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
are not recursively watched. The response's measurement
time identifies the age of the returned result.

### Read-only request audit

[`audit-live-requests.mjs`](../workspaces/client/apps/web/scripts/audit-live-requests.mjs)
measures idle and chat-switching traffic in headless Chromium, retaining request
counts, fetch stacks, CDP initiators and stream metadata outside the checkout.
It blocks every write except the audited tab's stream subscription updates:

```sh
node workspaces/client/apps/web/scripts/audit-live-requests.mjs \
  --url http://127.0.0.1:4620 --chat FIRST_ID --chat SECOND_ID \
  --output "$HOME/.cache/codex-studio/request-audit.json"
```

Blocked writes remain attempted requests in the report. A blocked read receipt
can cause uncertainty-recovery pulls, which must be classified separately from
reads triggered by a live change.
