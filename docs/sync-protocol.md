# Sync protocol v1

This document is the normative contract for web, mobile web, native, and peer
sync clients. It replaces the entity reset notes in `sync.md`; that file links
here. The wire format is HTTP/1.1 JSON and Server-Sent Events (SSE). Protocol
version 1 is additive: `/api/sync/pull` remains supported and old clients may
ignore every new endpoint and field.

## Negotiation and compatibility

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

## Identity and scopes

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

## Pull pages and cursors

`GET /api/sync/pull?scope=<scope>&after=<seq>&limit=<n>` returns
`workspaceId`, `documents`, and `checkpoint: {seq}`. Limits are clamped to
500 for entities and 100 for other scopes. Pages are ordered by sequence. An
empty delta keeps the requested cursor. Entity pages also include `maxSeq`.
The page's checkpoint is the greatest included sequence, or `maxSeq` when the
page is short. Clients persist each page before advancing the checkpoint.
Pull remains available to every protocol version and is the recovery path for
all stream resynchronization events.

## Entity floor, reset, and fresh baselines

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

## Drafts and transcripts

Draft writes use the existing authenticated `/api/sync/drafts` endpoint and
workspace header. A draft pull returns the latest retained row per draft ID;
clients apply by sequence and preserve tombstones. A stream cursor that is no
longer replayable emits `reset` and requires a draft pull from zero.

Transcript deltas are produced by `/api/sync/pull` and are scoped to one agent.
The stream carries the same projection document shape and cursor semantics.
A transcript delta requires its existing item base. If the base is absent,
the client pulls from zero and applies the resulting full snapshot before
accepting deltas. A transcript retention floor or deleted/recreated projection
requires a full pull. Clients must not infer a full transcript from a delta.

## Resumable change stream

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
{"protocolVersion":1,"workspaceId":"…","scope":"drafts",
 "documents":[{"id":"…","payload":"…","seq":121}],"cursor":121}
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

## Authentication and transports

The HTTP API binds only to `127.0.0.1`. Tailscale Serve may proxy this listener
over HTTPS using the existing exact-origin and Tailscale-source validation;
the client still uses the same protocol and session/write token. Forwarded
headers alone never grant trust. Native local clients may use the same HTTP
paths over the state-directory Unix socket, mode `0600`; the socket is an
additional local transport, not a non-loopback listener. Both transports
enforce protocol/workspace checks and normal write tokens.

## Native client requirements

A native client porting this contract must: negotiate version/capabilities;
validate the workspace identity; persist scope cursors only after applying
documents; obey page and payload limits; implement entity `reset=1`, hidden
baseline replacement, floor and `fresh/initialHigh` rules; preserve drafts and
outbox during entity reset; apply transcript full snapshots and deltas only
with a valid base; reconnect with `Last-Event-ID` or explicit `after`; handle
cursor-ahead/reset by pulling; cap its own buffers; treat heartbeats as liveness
only; and fall back to pull on unsupported capability, version, or stream
failure. It must never interpret a stream cursor as authorization or repeat a
write because a response was lost. No changes to the separate Attar Rust
branch are part of this task.

## Limits

The server clamps pull and stream batches, and every stream handler owns only
one bounded result batch. SSE is a delivery hint with replay from current
durable state, not a transaction log. State may be coalesced. Clients that
need every intermediate mutation require a separately versioned append-only
log. Browser/EventSource reconnect behavior is subject to browser suspension;
visibility resume must reconnect from the last durable client cursor.
