# Hosted client core

Status: step 8a is built (server routes, hosted core, browser transport).
Remote server views in the desktop app still use the frame path. The
removal below is not done yet.

## Goal

Every Studio page works the same way: it renders, and it sends commands to a
client core (`studio-client-core`). The page calls no browser storage or
coordination API: no IndexedDB, RxDB, Dexie, localStorage, sessionStorage,
Web Locks, BroadcastChannel, or `crypto.randomUUID` for identities.

The core runs in one of two places:

| Page                            | Core                                          | Transport                                                              |
| ------------------------------- | --------------------------------------------- | ---------------------------------------------------------------------- |
| Desktop app, local server       | Desktop process (`Client::open`, Unix socket) | Tauri `client_request` command and `studio-client-event` events        |
| Plain browser, any server       | That server's process (hosted core)           | `POST /api/client/request` and the SSE stream `GET /api/client/events` |
| Desktop app, remote server view | That remote server's hosted core              | Same HTTP transport as the plain browser                               |

The page uses the network (fetch and one SSE stream) for the hosted
transport. That is transport, not storage.

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
  (`studio_client_core::native::STATE_LOCK_FILE`).
- `POST /api/client/request` takes one core `Request` (`id`, `method`,
  `params`) and returns the core `Response` with the same `id`. Its body limit
  is the core message limit (`protocol::MAX_MESSAGE`). If the core connection
  fails, the reply is the error `client_core` and the server opens a new core
  on the next request.
- `GET /api/client/events` streams the core events of the device as SSE
  `data:` lines (default `message` event, keep-alive every 15 s). Only the
  core event callback holds the event sender, so the stream ends when the core
  closes. A stream that falls more than 1024 events behind also ends.
- Request identities keep their meaning: the core replays a stored result for
  a durable command with the same ID and refuses a second pending request with
  the same ID.

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

## Removal

After the hosted transport works, `hasNativeClient()` is always true. Then:

- Delete the RxDB and Dexie sync path (`sync/client.ts` browser database,
  `createBrowserDrafts`, browser `createOutbox`, browser uploads and dictation
  storage, the browser profile import, Web Locks owner election, and
  BroadcastChannel).
- Delete the `rxdb` and `dexie` dependencies.

## Known limits

- After the event stream reconnects (server restart, lagged stream), the core
  can be new and the view subscriptions of the page are gone. The page does
  not subscribe again by itself yet.
- A tab that closes without `view.unsubscribe` leaves its views in the shared
  device core until the core closes. The core has a limit of 64 views.
- State folders under `web-clients/` are not deleted.
- Waiting for a closing core holds the open-core lock of all devices, for at
  most 5 s.
- The client core uses Unix sockets, so only Unix servers link it. On other
  targets the two routes answer the shared 501 response.
- In the plain browser and in remote server views the queue of unsent
  messages lives on the server. With no network the page can not queue a new
  message.
- Push notifications in the plain browser need a service worker. They are not
  part of this step.
