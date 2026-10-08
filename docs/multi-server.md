# Multiple Studio servers: implementation contract

Status: in progress (2026-10-08). One user runs Studio servers on several computers in one
Tailscale network and works with all of them from one UI.

## User decisions (2026-10-08)

1. The UI and the server are separate components. An installation can contain only the UI, only
   the server, or both. Any UI can connect to any set of paired servers.
2. Access: only the owner, after a one-time pairing. Full control of the paired server (read,
   send, start and stop agents).
3. Cross-server teams are supported: a lead can start a worker on another server. By default a
   worker runs on the lead's server; another server is used only when the lead asks for it.
4. A worker on another server uses that computer's own clone of the repository. Studio does not
   copy source between computers. The orchestrator organizes the work (which server, which folder,
   which branch to push or fetch); Studio provides the tools.

## Components

| Component                               | Content                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| --------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Server pairing and access (server side) | Pair a UI client or a peer server through Tailscale Serve HTTPS; long-lived per-device credentials (signed requests, like federation: Ed25519, nonces, Tailscale `whois` check of the same user); revoke; audit list; all existing APIs and the sync stream usable by a paired client over the Serve origin                                                                                                                                                                                                       |
| Multi-server UI                         | Server list (add by pairing, remove, status); one sync store per server; unified sidebar and project list with a server label; every action routed to the owning server; works without a local server (UI-only install); desktop packaging option for UI-only                                                                                                                                                                                                                                                     |
| Cross-server orchestration              | `orchestration_spawn` gets `server` (default: the lead's server) and `cwd` on that server; the lead's server asks the remote server to create the worker under a remote-parent link; worker events (child results, messages, task board updates, stops) flow back to the lead; tools for the lead: list paired servers and their projects and folders, run a bounded read-only git command on a remote folder, fetch a worker branch from the remote server into a local repository (git over the paired channel) |

## Rules

- Server APIs stay on loopback; remote access goes only through Tailscale Serve HTTPS with paired
  credentials. Funnel is never used.
- Every cross-server call has a stable request id, a timeout and a durable receipt (no duplicated
  spawn or input after a retry).
- A server that is offline shows as offline in the UI; queued cross-server events are delivered
  when it returns.
- Existing peer federation (rooms between two users) stays as it is.

## Credential and request contract, version 1

This contract gives a paired device full owner access. Peer room federation keeps
its separate keys, permissions, and protocol. The server listener stays on
loopback. Remote requests require the configured Tailscale Serve HTTPS origin.
Tests use separate state directories and ports. Tests never change Serve or
enable Funnel.

### Pair a device

1. On the target server, call `POST /api/multi-server` with
   `{action:"create_invite",requestId,label?}` and the existing local session
   header. A paired owner can also create an invitation with a signed request.
2. Transfer the invitation through a private channel. The response contains
   `{invitation:{protocol:1,inviteId,token,serverId,label,origin,publicKey,tailscaleUser,expires},expires}`.
   The random secret expires after 15 minutes. The server stores its hash.
3. Generate an Ed25519 key pair on the client. Save the key, client ID, invitation,
   and stable request ID before the network request. A lost response must not
   cause a new client identity.
4. Call `POST /api/multi-server/v1/pair` with
   `{protocol:1,inviteId,token,clientId,label,kind,publicKey,requestId}`.
   `kind` is `ui` or `server`. A server client also supplies its HTTPS `origin`.
   Sign the exact request bytes with the submitted key and the headers below.
5. Check the response against the invitation's server ID, origin, and public key.
   The response is
   `{protocol:1,serverId,clientId,label,origin,publicKey,tailscaleUser,paired:true}`.

The public key uses SubjectPublicKeyInfo (SPKI) PEM. Native private keys use
PKCS8 PEM. A server client uses its local server ID as its client ID.
The server verifies the signature and the secret before it consumes the
invitation. It requires a verified Tailscale user equal to the server owner.
Missing or different identity fails. This flow has no federation warning override.
An exact pairing retry returns its saved receipt. A reused request ID with
different bytes fails. A different client cannot reuse a consumed invitation.

### Store credentials

The browser stores a nonextractable WebCrypto private `CryptoKey` in IndexedDB.
It stores the public key and the pinned server ID, origin, and public key beside
that key. The desktop client stores its private key with Electron `safeStorage`
through the main process. It does not send a private key to another frame.
Native servers store their key in the existing server state directory with
owner-only access. They store paired devices, invitations, nonces, audit records,
and receipts in SQLite. No pairing operation copies provider credentials or
the target's local session token.

### Authenticate every HTTP request

All remote API requests, including reads, require these headers:

| Header                | Value                                    |
| --------------------- | ---------------------------------------- |
| `X-Studio-Client`     | Registered client ID                     |
| `X-Studio-Server`     | Target server ID                         |
| `X-Studio-Timestamp`  | Integer Unix time in seconds             |
| `X-Studio-Nonce`      | New base64url value from 24 random bytes |
| `X-Studio-Request-Id` | Stable request ID for this operation     |
| `X-Studio-Signature`  | Base64 Ed25519 signature                 |

The signature covers UTF-8 bytes of these lines, separated by `\n`, without a
final newline:

```text
studio-multi-server-v1
METHOD
exact_path_and_query
target_server_id
client_id
timestamp
nonce
request_id
sha256_of_exact_body_bytes_as_lowercase_hex
```

Use the uppercase method. Preserve query order and percent encoding. An empty
body has the SHA256 digest of zero bytes. The server accepts a maximum clock
difference of 300 seconds. It rejects duplicate headers, duplicate nonces,
revoked clients, different users, and invalid signatures. Each retry keeps its
request ID and exact body. It uses a fresh timestamp, nonce, and signature.
Existing API schema and workspace identity headers keep their current rules.
If a mutation body contains `requestId` or `request_id`, that value must equal
the signed request ID. Do not send `X-Canvas-Token` to another server.

The HTTP response uses Tailscale HTTPS. The client pins the pairing identity
and rejects redirects. Cross-origin resource sharing (CORS) permits requests
without cookies. An unsigned preflight returns only CORS policy. It gives no
API access. A remote unsigned read cannot obtain a local session token.

### Authenticate the sync stream

Use `fetch` for `GET /api/sync/stream`. Sign its exact path and query with the
same headers. `EventSource` cannot set these headers. No key, token, signature,
or invitation secret belongs in a URL. Reconnect with the existing sync cursor,
a fresh nonce, and a new request ID. The server checks revocation during the
stream. It closes the stream when the client is revoked. The existing sync
cursor, heartbeat, schema checks, and reconnect rules stay in use.

### Management and server transport

`GET /api/multi-server` returns
`{protocol:1,identity,clients,servers,invites}`. Public records contain no secret
or private key. The identity contains the server ID, label, origin, public key,
and Tailscale user. The client list includes status and access audit times.

`POST /api/multi-server` also accepts:

- `{action:"revoke",clientId,requestId}`
- `{action:"accept_invite",invitation,requestId}` for native server pairing

`Runtime.paired_access()` owns credentials. Its service provides
`local_server_id`, `servers()`, `paired_server(id)`, and
`request(id,method,path,body,request_id,timeout=15)`.
The request method signs exact JSON bytes, pins the target identity, rejects
redirects, and bounds the response. The maximum orchestration request size is
256 KiB. The maximum response size is 1 MiB. A timeout leaves the operation
outcome unknown until its durable receipt is recovered.

`Runtime.multi_server()` owns cross-server orchestration. The signed
`POST /api/servers/orchestration` route requires a server client. It calls
`receive(principal.clientId,envelope)`. The envelope is
`{requestId,action,payload}`. The request ID must equal the signed header.
The verified `studio_principal` has `clientId`, `kind`, `publicKey`,
`tailscaleUser`, and `targetServerId`. For a server client, its `serverId` equals
its source client ID. It never equals the target ID merely because that server
receives the request.

### Receipts and errors

The server reserves a durable receipt before a remote mutation reaches its
existing API handler. The receipt binds the client, request ID, method, exact
target, and body hash. Concurrent duplicates cannot run the handler twice.
Completed duplicates return the saved status and body. A different payload
with the same ID returns a conflict. After a crash, an unfinished receipt
stays unknown. It must not authorize a second mutation. Existing operation
receipts remain the source of truth for recovery of the actual effect.

Authentication fails before API dispatch. Errors identify invalid credentials,
clock difference, replay, owner identity, revocation, request conflict, an
operation in progress, or an unknown outcome. Request bodies have a 10-second
read limit. Tailscale identity commands and crypto commands have bounded
timeouts. Transport calls have a 15-second default timeout. Tests must cover
pairing retry, signature tampering, unpaired reads and writes, replay,
revocation, restart receipts, and authenticated sync.
