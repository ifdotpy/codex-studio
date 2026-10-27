# Native live stream

In the desktop app, the native core owns the `/api/sync/stream` connection of
the local server. The page does not retry it. This removes the second retry
layer that made "Live updates are reconnecting" loops: the page watchdog
counted the time in which the core waited for the page's own credit.

The [sync protocol](../sync-protocol.md) is unchanged. The stream is still the
only source of live data; the client does not poll.

## Where the state lives

| State                                                                        | Owner                                                                                    |
| ---------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| HTTP connection, `Last-Event-ID`, SSE parsing                                | Core, [`events.rs`](../../workspaces/client/packages/client-core/src/events.rs)          |
| Event credits (one event per credit)                                         | Core `EventFlow`; the page grants                                                        |
| Reconnect delay: 500 ms, doubles to 15 s, 50% to 150% jitter                 | Core (`RETRY_BASE_MS`, `RETRY_MAX_MS`)                                                   |
| Silence watchdog: 60 s while the core reads, not while it waits for a credit | Core (`EVENT_SILENCE_TIMEOUT`)                                                           |
| Close reasons                                                                | Core stream log (`sync-streams.log`)                                                     |
| Subscribed resources, per-resource cursors, session, schema handshake        | Page, [`resourceEvents.ts`](../../workspaces/client/apps/web/src/sync/resourceEvents.ts) |

The cursors stay in the page until projections move to the core (step 2).
The core therefore asks the page for the path of each new connection.

## What the page receives

[`NativeEventSource`](../../workspaces/client/apps/web/src/native/stream.ts)
subscribes with `view.subscribe` (topic `sse`, `flowControl: true`) and gets:

| Core message                                    | Meaning                                   | Page action                                         |
| ----------------------------------------------- | ----------------------------------------- | --------------------------------------------------- |
| `open` + credit                                 | A connection is open                      | New per-connection state; grant the credit          |
| event + `consumedCredit` + credit               | One SSE event                             | Apply it; grant the next credit after the page hold |
| `error`, `closed: false`, `opened`, `retryInMs` | The connection ended; the core reconnects | Status "degraded", show the retry time              |
| `resume`                                        | The core is ready to reconnect            | Read cursors; send `view.resume` with the path      |
| `error`, `closed: true`                         | The core stopped (HTTP 204, no backend)   | `onerror`                                           |

The page sends `view.restart` with a reason when it cannot use a connection:
an invalid frame, a failed schema handshake, or a subscription change without
an answer. The core ends that connection and applies the same delay.

## Failure cases

- Network loss or server restart: the core reports `error` and waits.
  Then it asks for `resume` and reconnects with fresh cursors and `reconnect=true`.
- Server silence: the watchdog ends the connection after 60 s of reading
  without a byte. A slow page holds credits; that does not count as silence.
- Lost subscription response: the page never repeats the diff. It asks for a
  restart, and the next connection carries the current resources.
- Lost `view.next` response: the page never repeats the grant. It closes the
  stream, and the page opens a new one after its own delay (terminal path).
- Events that arrive after a page restart and before the next `open` are
  dropped. Their credits still count.
- Repeated or early `view.resume`: the core accepts one path per request.

## Browser and remote servers

A browser page and a remote server in the desktop app keep the web
`EventSource` and its reconnect, backoff, heartbeat and visibility logic in
`resourceEvents.ts`. The page opens a new native source only after a terminal
close from the core.
