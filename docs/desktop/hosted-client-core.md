# Hosted client core

Status: step 8a (server routes, hosted core, browser transport) and step
8b-A (one core per server, also for paired servers) are built. The removal
of the browser stores (step 8b-B) is not done yet.

## Goal

Every Studio page works the same way: it renders, and it sends commands to a
client core (`studio-client-core`). The page calls no browser storage or
coordination API: no IndexedDB, RxDB, Dexie, localStorage, sessionStorage,
Web Locks, BroadcastChannel, or `crypto.randomUUID` for identities.

A core serves one server. It has its own state folder and its own
authentication to that server.

| Page and server                  | Core                                                              | Transport                                                                                 |
| -------------------------------- | ----------------------------------------------------------------- | ----------------------------------------------------------------------------------------- |
| Desktop app, local server        | Desktop process, `client/local`, loopback session token           | Tauri `client_request` and `studio-client-event`                                          |
| Desktop app, paired server       | Desktop process, `client/<server id>`, signed with the device key | Tauri `client_request` with `server`; its events carry `server`                           |
| Desktop app, server view frame   | The core of the frame's server                                    | The shell bridge (`native/frame.ts`) selects the core from the frame, never from the page |
| Plain browser, the page's server | That server's process (hosted core of the device)                 | `POST /api/client/request` and `GET /api/client/events`                                   |
| Plain browser, paired server     | The page's server, sub-core of the device for that server         | The same routes with `?server=<server id>`                                                |

The page uses the network (fetch and one SSE stream per server core) for the
hosted transport. That is transport, not storage. The page names a server in
each request; it stores nothing itself.

## Hosted core

`runtime-http/src/routes/client_core.rs` owns the hosted cores and their
constants.

- Device identity: on the first `/api/client/*` request without a known
  device, the server makes a random 128-bit identity, creates
  `<state>/web-clients/<device>/` (mode 0700), and sets the cookie
  `studio_client` with `Path=/api/client; HttpOnly; SameSite=Strict`, a
  400-day `Max-Age`, and `Secure` when the request came through the HTTPS
  remote origin. The page never reads the cookie. The server accepts a cookie
  only when its state folder exists, so the page can not choose a device.
- One core per device (`Client::open` with the device folder as its state
  directory). All tabs of one browser share it. At most
  `CLIENT_CORE_MAX_OPEN` (16) cores are open; more devices get 503.
- A core closes when no request or event stream used it for
  `CLIENT_CORE_IDLE_SECONDS` (600 s). An open event stream keeps it open. The
  next request opens it again on the same state. Before it opens a core, the
  server waits up to `CLIENT_CORE_CLOSE_WAIT_MILLISECONDS` (5000 ms) until the
  previous core of that device released its state lock
  (`studio_client_core::native::STATE_LOCK_FILE`). Only that device waits.
- The lock file time records the last use of a device. At server start and
  when a core opens, the server deletes the folders of closed devices unused
  for `CLIENT_STATE_RETENTION_SECONDS` (90 days).
- `POST /api/client/request` takes one core `Request` (`id`, `method`,
  `params`) and returns the core `Response` with the same `id`. Its body limit
  is the core message limit (`protocol::MAX_MESSAGE`). If the core connection
  fails, the reply is the error `client_core` and the server opens a new core
  on the next request.
- `GET /api/client/events` streams the core events of the device as SSE
  `data:` lines (default `message` event, keep-alive every 15 s). The first
  event is `{"connection": "<id>"}`. A hosted `view.subscribe` must name an
  open connection of the device in `params.connection`; the server removes
  it before the core sees the command. When the stream ends, the server
  unsubscribes every view of that connection, so views never outlive their
  stream. Only the
  core event callback holds the event sender, so the stream ends when the core
  closes. A stream that falls more than 1024 events behind also ends.
- Request identities keep their meaning: the core replays a stored result for
  a durable command with the same ID and refuses a second pending request with
  the same ID.

## Paired servers

`client-core/src/server_keys.rs` owns the device keys and `VaultSigner`.
A core of a paired server gets a `ConnectionConfig.signer`. Its `Backend`
then signs every request (method, path and query, server and client
identities, time, nonce, request ID and the SHA-256 of the exact body) and
sends no session token. The core accepts the signer only when its origin is
the core's server origin, so a request for one server never goes to another
one. Each request reads the key store again, so a removed pairing stops at
once. Server management (`management_request`: the server-to-server
protocol and writes to `/api/multi-server`) stays with the workspace shell
of the local server; a signed core refuses it.

A write keeps its identity: the page sends the request ID of the write
(`X-Studio-Request-Id` or the identity in its body), and the core signs it
with that ID. The core never sends a write again by itself.

Desktop app (`src-tauri/src/lib.rs`):

- One `Client` per paired server, opened on first use with the key in
  `server-keys.json` of the profile. `servers.forget` closes it.
- A paired server core refuses device commands (`native::device_only`),
  `servers.*` and `profile.*`. Draft commands need the workspace that its own
  `sync.open` or `identity.get` verified, like the local core.
- The frame bridge keeps its method list. Server data commands (`outbox.*`,
  `drafts.*`, `uploads.*`, `files.*`, `dictation.*` except `dictation.wav`,
  `projection.*`, `usage.*`, `terminal.*`, `api.request`, `sync.open`,
  `identity.get`, and `view.*` with the topics `projection`, `outbox`,
  `drafts`, `sse` and `terminal`) go to the core of the frame's own server.
  The bridge forwards that core's events only to that frame. Other commands
  keep their earlier rules; unknown commands are refused.

Plain browser (`runtime-http/src/routes/client_core.rs`):

- The device folder holds the keys of this device for other servers
  (`server-keys.json`). `servers.pair`, `servers.pairAttempt` and
  `servers.forget` on the device core change them. A reply carries only the
  server row, never a key. The hosted core refuses `servers.request`,
  `servers.read`, `servers.cancel`, and every `servers.*` command of a
  sub-core. It does not check an automatic pairing approval; the page checks
  it against this server, like the earlier browser path did.
- `?server=<id>` opens the sub-core of the device for that server in
  `<device>/servers/<id>/`. Without a key for it the reply is 404. Another
  device never uses these keys. Sub-cores count against
  `CLIENT_CORE_MAX_OPEN` and close when idle like device cores. Key changes
  of all devices run one at a time.

Risk accepted by the owner (2026-10-28): the hosting server holds the
private keys of this device for the other servers. A person or program that
controls the hosting server can use them, so a compromise of one server
exposes the paired servers of its browser devices. The desktop app keeps its
keys on the device and has no such exposure.

## Authentication

The two routes use the same boundary as every other `/api/*` route: loopback
peer or the trusted HTTPS remote origin, same-origin `Origin`, and for
`POST` the session token in `X-Canvas-Token`. A request that fails these checks
never reaches a core and gets no cookie.

The hosted core calls its own server at `http://127.0.0.1:<port>/`, like the
desktop core calls its local server. It reads the session token from
`GET /api/session` on loopback and sends it on writes. This is the same token
that the page already has; the server adds no new credential and no bypass.
The loopback address is fixed by the server, never by the page.

## Device-only capabilities

`studio_client_core::native::device_only` owns the list. The hosted core
refuses, before it opens a core: `media.*`, `voice.*`, `speech.*`,
`desktop.*` (file dialogs, quick look and other file actions, file save,
external links, sign-in windows, microphone), `preview.*` (native file
preview), `notifications.show`, `dictation.wav`, and `view.subscribe` with
the topics `microphone`, `voice`, `transcription` and `navigation`. The reply
is the error code `unsupported_here` with a platform-neutral message.

The page asks `hasDeviceClient()` (desktop window or its server view frame)
before it shows these controls. `hasNativeClient()` is also true for a plain
browser page.

## Web transport

`web/src/native/hosted.ts` is used when the page has neither the desktop host
nor a frame, and the page origin is HTTP or HTTPS.

- `nativeRequest` sends one `POST /api/client/request` with the caller's exact
  request ID, the session token from `GET /api/session`, and the API schema
  hash. It never sends a command again by itself. A lost response or an
  unreadable reply throws `OutcomeUnknownError`.
- The first contact of the page gets the device cookie. Other requests and the
  event stream wait for it, so concurrent first requests do not make two
  devices.
- `listenNativeEvents` shares one `EventSource` on `/api/client/events`
  between all listeners and closes it after the last listener.
- `view.subscribe` adds the current connection. On each new connection
  (the browser reopened the stream, maybe to a new core) the page sends every
  live `view.subscribe` again with the same subscription identity and a new
  request identity. If that fails, the view listeners get an error event.

## Removal

`hasNativeClient()` is true for the desktop app and for every HTTP or
HTTPS page. On those paths the browser stores below are unreachable. Step
8b-B deletes them:

- `servers/browserCredentials.ts` (IndexedDB keys, `crypto.subtle` signing)
  and its helpers in `servers/pairing.ts` (`signedRequest`, `publicKeyPem`,
  `pairedServer`).
- The fallbacks in `servers/serverServices.ts` and
  `servers/ServerServicesContext.tsx` without a core: browser `createOutbox`,
  `createUploads`, `createRecordingStorage`, and the RxDB projection path of
  `sync/client.ts` (`projectionStore` undefined).
- `SignedEventSource` and the plain `EventSource` in `servers/eventSource.ts`
  and `sync/subscriptions/server.ts`; Web Locks owner election and
  BroadcastChannel in `sync/resourceEvents.ts` (every core runs
  `independent`).
- The frame transport bridge for raw server requests
  (`studio-server-transport` in `servers/shellTransport.ts`,
  `DesktopServerCredentials` over `shellCredentialBridge`) and the local
  frame account branch in `servers/transport.ts`.
- `servers.request`, `servers.read` and `servers.cancel` of
  `server_transport.rs`: the desktop still accepts them, but no page uses them.
- The legacy importers stay until their one-time import is no longer needed.

Then:

- Delete the RxDB and Dexie sync path (`sync/client.ts` browser database,
  `createBrowserDrafts`, browser `createOutbox`, browser uploads and dictation
  storage, the browser profile import, Web Locks owner election, and
  BroadcastChannel).
- Delete the `rxdb` and `dexie` dependencies.

## Known limits

- Events sent while the stream reconnects are lost; a view starts again
  from its current state after it subscribes again.
- The client core uses Unix sockets, so only Unix servers link it. On other
  targets the two routes answer the shared 501 response.
- In the plain browser the queue of unsent messages lives on the page's
  server. With no network the page can not queue a new message.
- A plain browser page that the server shows on a separate frame origin
  (local development on `localhost`) gets its own device cookie there.
- The older desktop bridge (`window.codexDesktop`) keeps its own credential
  path.
- Push notifications in the plain browser need a service worker. They are not
  part of this step.
